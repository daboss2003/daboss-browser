//! Long-lived JS execution context for a page.
//!
//! Unlike the original `run_inline_scripts` function this engine survives
//! beyond the initial parse: it keeps the `boa::Context` and the listener
//! registry alive so that DOM events fired later (clicks, eventually
//! timer fires and fetch callbacks) can invoke JS handlers registered by
//! the original page scripts.
//!
//! Threading model: everything is single-threaded. The active `Dom` and
//! the listener map are installed into thread-locals only while the
//! engine is actually executing JS (initial scripts or an event
//! dispatch); outside of those windows the engine just holds owned data.
//!
//! Event model (cut down): listeners are keyed by `(NodeId, event_type)`
//! and fire in registration order. Dispatch walks from the target up to
//! the document root (bubbling). Each handler is called with `this` set
//! to the current target and an Event-ish object passed as the first
//! argument. `event.preventDefault()` and `event.stopPropagation()` are
//! supported via per-dispatch thread-local flags.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use boa_engine::{
    js_string, object::builtins::JsFunction, object::ObjectInitializer, property::Attribute,
    Context, JsResult, JsValue, NativeFunction, Source,
};

use crate::dom::{Dom, NodeId, NodeKind};
use crate::net;

use super::dom as js_dom;
use super::storage::{self, StorageArea, JS_LOCAL_STORAGE, JS_SESSION_STORAGE};
use super::{collect_inline_scripts, install_console, JS_DOM};

type ListenerMap = HashMap<(NodeId, String), Vec<JsFunction>>;

pub(crate) struct TimerEntry {
    pub(crate) id: u32,
    pub(crate) fire_at: Instant,
    /// `Some(d)` for setInterval (re-arms with period `d`).
    pub(crate) interval: Option<Duration>,
    pub(crate) callback: JsFunction,
}

#[derive(Default)]
pub(crate) struct TimerState {
    pub(crate) timers: Vec<TimerEntry>,
    pub(crate) next_id: u32,
}

thread_local! {
    pub(crate) static JS_LISTENERS: RefCell<Option<Rc<RefCell<ListenerMap>>>> =
        const { RefCell::new(None) };

    pub(crate) static JS_TIMERS: RefCell<Option<Rc<RefCell<TimerState>>>> =
        const { RefCell::new(None) };

    pub(crate) static JS_FETCH_CLIENT: RefCell<Option<Rc<net::Client>>> =
        const { RefCell::new(None) };

    pub(crate) static JS_BASE_URL: RefCell<Option<url::Url>> = const { RefCell::new(None) };

    /// Per-dispatch flags toggled by `event.preventDefault()` /
    /// `event.stopPropagation()`. Reset at the start of each dispatch.
    pub(crate) static EVENT_FLAGS: RefCell<EventFlags> = const { RefCell::new(EventFlags::EMPTY) };
}

#[derive(Default, Clone, Copy)]
pub(crate) struct EventFlags {
    pub default_prevented: bool,
    pub propagation_stopped: bool,
}

impl EventFlags {
    pub(crate) const EMPTY: EventFlags = EventFlags {
        default_prevented: false,
        propagation_stopped: false,
    };
}

pub struct JsEngine {
    ctx: Context,
    listeners: Rc<RefCell<ListenerMap>>,
    timers: Rc<RefCell<TimerState>>,
    /// Network client shared with the rest of the browser. `fetch` uses
    /// it; left `None` for the headless PNG mode and unit tests where we
    /// don't want unsolicited network I/O.
    fetch_client: Option<Rc<net::Client>>,
    base_url: Option<url::Url>,
    /// `localStorage` map. Shared across pages within one browser run
    /// (the [`crate::Browser`] hands a clone to each navigated engine).
    local_storage: StorageArea,
    /// `sessionStorage` map. Created per engine, so it resets on
    /// navigation but survives across event handler ticks and timers.
    session_storage: StorageArea,
}

/// Outcome of an event dispatch — informs the caller whether to skip the
/// built-in action (preventDefault) and whether to re-cascade and
/// re-layout (any mutation happened).
#[derive(Default, Clone, Copy)]
pub struct DispatchResult {
    pub default_prevented: bool,
    pub mutated: bool,
}

impl JsEngine {
    /// Build a fresh engine, install globals, and run the page's inline
    /// scripts against `dom`. Mutations made by those scripts are visible
    /// on `dom` when this returns.
    pub fn new(dom: &mut Dom) -> Self {
        Self::with_fetch(dom, None, None, None)
    }

    /// Like [`JsEngine::new`] but with a network client and page base
    /// URL plumbed through for `fetch`, plus an optional `localStorage`
    /// area shared across navigations within one browser run. The
    /// browser shell passes all four; tests use [`JsEngine::new`] to
    /// stay offline and per-page-isolated.
    pub fn with_fetch(
        dom: &mut Dom,
        client: Option<Rc<net::Client>>,
        base_url: Option<url::Url>,
        local_storage: Option<StorageArea>,
    ) -> Self {
        let mut ctx = Context::default();
        install_console(&mut ctx);
        js_dom::install(&mut ctx);
        install_timer_globals(&mut ctx);
        install_fetch_global(&mut ctx);
        storage::install(&mut ctx);
        install_window_alias(&mut ctx);

        let listeners: Rc<RefCell<ListenerMap>> = Rc::new(RefCell::new(HashMap::new()));
        let timers: Rc<RefCell<TimerState>> = Rc::new(RefCell::new(TimerState::default()));
        let local_storage = local_storage
            .unwrap_or_else(|| Rc::new(RefCell::new(HashMap::new())));
        let session_storage: StorageArea = Rc::new(RefCell::new(HashMap::new()));
        let mut engine = JsEngine {
            ctx,
            listeners,
            timers,
            fetch_client: client,
            base_url,
            local_storage,
            session_storage,
        };
        engine.run_initial_scripts(dom);
        engine
    }

    /// Soonest fire time of any pending timer, or `None` if none are queued.
    /// The browser loop uses this to set winit's `ControlFlow::WaitUntil`.
    pub fn next_timer_at(&self) -> Option<Instant> {
        self.timers
            .borrow()
            .timers
            .iter()
            .map(|t| t.fire_at)
            .min()
    }

    /// Fire every timer whose `fire_at <= now`, re-arming intervals.
    /// Returns whether any DOM node count grew during the firings (a
    /// rough mutation signal — see [`dispatch_event`]).
    pub fn pump_timers(&mut self, dom: &mut Dom) -> DispatchResult {
        let now = Instant::now();
        let due = self.drain_due_timers(now);
        if due.is_empty() {
            return DispatchResult::default();
        }

        let (dom_rc, listeners_rc) = self.install_thread_locals(dom);
        let pre_count = dom_rc.borrow().node_count();

        for timer in due {
            let cb = timer.callback.clone();
            if let Err(e) = cb.call(&JsValue::undefined(), &[], &mut self.ctx) {
                eprintln!("[js] timer #{} threw: {e}", timer.id);
            }
            if let Some(period) = timer.interval {
                // setInterval — re-add for the next firing. Use `now`
                // (not Instant::now() afresh) so a slow callback doesn't
                // skew the cadence forward unboundedly.
                self.timers.borrow_mut().timers.push(TimerEntry {
                    id: timer.id,
                    fire_at: now + period,
                    interval: Some(period),
                    callback: timer.callback,
                });
            }
        }
        // Drain microtasks queued by the timer bodies.
        self.ctx.run_jobs();

        let post_count = dom_rc.borrow().node_count();
        let mutated = post_count != pre_count;
        self.uninstall_thread_locals(dom, dom_rc, listeners_rc);

        DispatchResult {
            default_prevented: false,
            mutated,
        }
    }

    fn drain_due_timers(&self, now: Instant) -> Vec<TimerEntry> {
        let mut state = self.timers.borrow_mut();
        let mut due = Vec::new();
        let mut i = 0;
        while i < state.timers.len() {
            if state.timers[i].fire_at <= now {
                due.push(state.timers.swap_remove(i));
            } else {
                i += 1;
            }
        }
        due
    }

    fn run_initial_scripts(&mut self, dom: &mut Dom) {
        let scripts = collect_inline_scripts(dom);
        if scripts.is_empty() {
            return;
        }
        let (rc, listeners_rc) = self.install_thread_locals(dom);
        for (i, src) in scripts.iter().enumerate() {
            if let Err(e) = self.ctx.eval(Source::from_bytes(src.as_bytes())) {
                eprintln!("[js] script #{i} threw: {e}");
            }
        }
        // Drain the promise / microtask queue so `.then` callbacks, the
        // bodies after `await`, etc. all run before we hand control back.
        self.ctx.run_jobs();
        self.uninstall_thread_locals(dom, rc, listeners_rc);
    }

    /// Dispatch `event_type` to `target` with bubbling. Walks from the
    /// target up to the document root, firing all listeners at each
    /// level in registration order. Returns whether the default was
    /// prevented and whether any DOM mutation occurred.
    pub fn dispatch_event(
        &mut self,
        dom: &mut Dom,
        event_type: &str,
        target: NodeId,
    ) -> DispatchResult {
        // Build the bubble chain from a snapshot of the live tree.
        let chain = bubble_chain(dom, target);

        // Empty chain means the target isn't an element we can dispatch
        // to (probably text node / document); nothing to do.
        if chain.is_empty() {
            return DispatchResult::default();
        }

        let (dom_rc, listeners_rc) = self.install_thread_locals(dom);
        EVENT_FLAGS.with(|f| *f.borrow_mut() = EventFlags::EMPTY);

        let pre_mutation_marker = dom_rc.borrow().node_count();

        let event_obj = build_event_object(&mut self.ctx, event_type, target);

        'bubble: for &node in &chain {
            // Update event.currentTarget for this bubble step.
            let cur_handle = js_dom::make_element_handle(&mut self.ctx, node);
            let _ = event_obj.set(
                js_string!("currentTarget"),
                JsValue::from(cur_handle.clone()),
                false,
                &mut self.ctx,
            );

            // Snapshot the handlers so a handler that calls
            // addEventListener mid-dispatch doesn't grow the list we're
            // iterating, and so a handler that removes itself doesn't
            // skip a sibling. Web platform behaviour is more subtle than
            // this; we'll refine in a later sub-phase.
            let handlers: Vec<JsFunction> = {
                let map = listeners_rc.borrow();
                map.get(&(node, event_type.to_string()))
                    .cloned()
                    .unwrap_or_default()
            };

            for h in handlers {
                let this_val: JsValue = cur_handle.clone().into();
                let args = [JsValue::from(event_obj.clone())];
                if let Err(e) = h.call(&this_val, &args, &mut self.ctx) {
                    eprintln!("[js] {event_type} handler threw: {e}");
                }
                let stopped =
                    EVENT_FLAGS.with(|f| f.borrow().propagation_stopped);
                if stopped {
                    break 'bubble;
                }
            }
        }

        // Drain the microtask queue — handlers that called async fns or
        // chained `.then(...)` need their continuations to run before
        // we return to the browser.
        self.ctx.run_jobs();

        let flags = EVENT_FLAGS.with(|f| *f.borrow());
        let post_mutation_marker = dom_rc.borrow().node_count();
        let mutated = post_mutation_marker != pre_mutation_marker;
        // Note: attribute mutations don't change node_count. For a more
        // accurate signal we'd track a per-frame mutation counter on the
        // Dom; for now we conservatively report `false` for pure attr
        // changes and let the caller observe via "did your handler look
        // like it mutated something." Good enough for Phase 7c.

        self.uninstall_thread_locals(dom, dom_rc, listeners_rc);

        DispatchResult {
            default_prevented: flags.default_prevented,
            mutated,
        }
    }

    fn install_thread_locals(
        &mut self,
        dom: &mut Dom,
    ) -> (Rc<RefCell<Dom>>, Rc<RefCell<ListenerMap>>) {
        let owned = std::mem::take(dom);
        let dom_rc = Rc::new(RefCell::new(owned));
        JS_DOM.with(|slot| {
            *slot.borrow_mut() = Some(dom_rc.clone());
        });
        let listeners_rc = self.listeners.clone();
        JS_LISTENERS.with(|slot| {
            *slot.borrow_mut() = Some(listeners_rc.clone());
        });
        JS_TIMERS.with(|slot| {
            *slot.borrow_mut() = Some(self.timers.clone());
        });
        JS_FETCH_CLIENT.with(|slot| {
            *slot.borrow_mut() = self.fetch_client.clone();
        });
        JS_BASE_URL.with(|slot| {
            *slot.borrow_mut() = self.base_url.clone();
        });
        JS_LOCAL_STORAGE.with(|slot| {
            *slot.borrow_mut() = Some(self.local_storage.clone());
        });
        JS_SESSION_STORAGE.with(|slot| {
            *slot.borrow_mut() = Some(self.session_storage.clone());
        });
        (dom_rc, listeners_rc)
    }

    fn uninstall_thread_locals(
        &mut self,
        dom: &mut Dom,
        dom_rc: Rc<RefCell<Dom>>,
        listeners_rc: Rc<RefCell<ListenerMap>>,
    ) {
        JS_SESSION_STORAGE.with(|slot| {
            slot.borrow_mut().take();
        });
        JS_LOCAL_STORAGE.with(|slot| {
            slot.borrow_mut().take();
        });
        JS_BASE_URL.with(|slot| {
            slot.borrow_mut().take();
        });
        JS_FETCH_CLIENT.with(|slot| {
            slot.borrow_mut().take();
        });
        JS_TIMERS.with(|slot| {
            slot.borrow_mut().take();
        });
        JS_LISTENERS.with(|slot| {
            slot.borrow_mut().take();
        });
        JS_DOM.with(|slot| {
            slot.borrow_mut().take();
        });
        // Reclaim the dom. Boa's GC may still hold object clones of the
        // Rc, in which case `try_unwrap` fails and we swap.
        drop(listeners_rc); // explicit
        match Rc::try_unwrap(dom_rc) {
            Ok(cell) => *dom = cell.into_inner(),
            Err(rc) => *dom = std::mem::take(&mut *rc.borrow_mut()),
        }
    }
}

/// Aliases `window` (and `self`) to the global object so scripts that
/// reach into `window.something` or expect `self === globalThis` work.
/// Cheap because both are just `Attribute::WRITABLE` properties — direct
/// assignments to `window.foo = ...` mutate the global, matching browser
/// behaviour.
fn install_window_alias(ctx: &mut Context) {
    let global = ctx.global_object();
    let global_val = JsValue::from(global);
    let _ = ctx.register_global_property(
        js_string!("window"),
        global_val.clone(),
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
    let _ = ctx.register_global_property(
        js_string!("self"),
        global_val,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn install_timer_globals(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("setTimeout"),
        2,
        NativeFunction::from_fn_ptr(set_timeout),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("setInterval"),
        2,
        NativeFunction::from_fn_ptr(set_interval),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("clearTimeout"),
        1,
        NativeFunction::from_fn_ptr(clear_timer),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("clearInterval"),
        1,
        NativeFunction::from_fn_ptr(clear_timer),
    )
    .ok();
}

fn schedule_timer(
    callback: JsFunction,
    delay_ms: u32,
    interval: Option<Duration>,
) -> u32 {
    JS_TIMERS.with(|slot| {
        let Some(state_rc) = slot.borrow().as_ref().cloned() else {
            return 0;
        };
        let mut state = state_rc.borrow_mut();
        state.next_id = state.next_id.wrapping_add(1);
        let id = state.next_id;
        state.timers.push(TimerEntry {
            id,
            fire_at: Instant::now() + Duration::from_millis(delay_ms as u64),
            interval,
            callback,
        });
        id
    })
}

fn set_timeout(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(callback) = extract_callback(args.first()) else {
        return Ok(JsValue::from(0));
    };
    let ms = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let id = schedule_timer(callback, ms, None);
    Ok(JsValue::from(id))
}

fn set_interval(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(callback) = extract_callback(args.first()) else {
        return Ok(JsValue::from(0));
    };
    let ms = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let period = Duration::from_millis(ms.max(1) as u64);
    let id = schedule_timer(callback, ms, Some(period));
    Ok(JsValue::from(id))
}

fn clear_timer(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Ok(id) = id_val.to_u32(ctx) else {
        return Ok(JsValue::undefined());
    };
    JS_TIMERS.with(|slot| {
        if let Some(state_rc) = slot.borrow().as_ref() {
            state_rc.borrow_mut().timers.retain(|t| t.id != id);
        }
    });
    Ok(JsValue::undefined())
}

fn extract_callback(val: Option<&JsValue>) -> Option<JsFunction> {
    let v = val?;
    let obj = v.as_object()?;
    JsFunction::from_object(obj.clone())
}

// ---------- fetch ----------

fn install_fetch_global(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("fetch"),
        2,
        NativeFunction::from_fn_ptr(js_fetch),
    )
    .ok();
}

/// `fetch(url, [init])` — performs the HTTP request synchronously (no
/// real I/O concurrency yet) but wraps the result in a real
/// [`JsPromise`] so callers can `await fetch(...)` or chain `.then()`
/// using boa's native Promise machinery. The promise resolves with a
/// `Response`-shaped JS object on success or with a stubbed response
/// (`ok: false`, `status: 0`) on transport / blocklist failures.
///
/// Supported `init` keys: `method` (`GET` / `POST`), `body` (string).
/// Headers are ignored for now.
fn js_fetch(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;

    let Some(url_arg) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let url_str = url_arg.to_string(ctx)?.to_std_string_escaped();

    let resolved_url = JS_BASE_URL.with(|slot| -> Option<url::Url> {
        if let Some(base) = slot.borrow().as_ref() {
            base.join(&url_str).ok()
        } else {
            url::Url::parse(&url_str).ok()
        }
    });
    let Some(target_url) = resolved_url else {
        let v = JsValue::from(make_failed_response(ctx, &url_str, "invalid-url"));
        return Ok(JsPromise::resolve(v, ctx).into());
    };

    let mut method = "GET".to_string();
    let mut body: Option<Vec<u8>> = None;
    if let Some(init_val) = args.get(1) {
        if let Some(obj) = init_val.as_object() {
            if let Ok(m) = obj.get(js_string!("method"), ctx) {
                if !m.is_undefined() && !m.is_null() {
                    method = m.to_string(ctx)?.to_std_string_escaped().to_uppercase();
                }
            }
            if let Ok(b) = obj.get(js_string!("body"), ctx) {
                if !b.is_undefined() && !b.is_null() {
                    body = Some(b.to_string(ctx)?.to_std_string_escaped().into_bytes());
                }
            }
        }
    }

    let response = JS_FETCH_CLIENT.with(|slot| -> Option<net::Result<net::Response>> {
        let client = slot.borrow().as_ref()?.clone();
        let url = target_url.to_string();
        Some(match method.as_str() {
            "POST" => {
                let b = body.unwrap_or_default();
                client.post(&url, b, "application/x-www-form-urlencoded")
            }
            _ => client.get(&url),
        })
    });

    let value = match response {
        Some(Ok(resp)) => JsValue::from(make_response_object(ctx, target_url.as_str(), resp)),
        Some(Err(e)) => {
            eprintln!("[js] fetch({target_url}) failed: {e}");
            JsValue::from(make_failed_response(
                ctx,
                target_url.as_str(),
                &e.to_string(),
            ))
        }
        None => JsValue::from(make_failed_response(
            ctx,
            target_url.as_str(),
            "no-fetch-client",
        )),
    };
    Ok(JsPromise::resolve(value, ctx).into())
}

fn make_response_object(
    ctx: &mut Context,
    url_str: &str,
    resp: net::Response,
) -> boa_engine::JsObject {
    let ok = (200..300).contains(&resp.status);
    let body_str = String::from_utf8_lossy(&resp.body).into_owned();

    ObjectInitializer::new(ctx)
        .property(js_string!("ok"), JsValue::from(ok), Attribute::READONLY)
        .property(
            js_string!("status"),
            JsValue::from(resp.status as u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("statusText"),
            JsValue::from(js_string!(resp.reason)),
            Attribute::READONLY,
        )
        .property(
            js_string!("url"),
            JsValue::from(js_string!(url_str)),
            Attribute::READONLY,
        )
        .property(
            js_string!("__body"),
            JsValue::from(js_string!(body_str)),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(response_text),
            js_string!("text"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(response_json),
            js_string!("json"),
            0,
        )
        .build()
}

fn make_failed_response(ctx: &mut Context, url_str: &str, reason: &str) -> boa_engine::JsObject {
    ObjectInitializer::new(ctx)
        .property(js_string!("ok"), JsValue::from(false), Attribute::READONLY)
        .property(
            js_string!("status"),
            JsValue::from(0_u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("statusText"),
            JsValue::from(js_string!(reason)),
            Attribute::READONLY,
        )
        .property(
            js_string!("url"),
            JsValue::from(js_string!(url_str)),
            Attribute::READONLY,
        )
        .property(
            js_string!("__body"),
            JsValue::from(js_string!("")),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(response_text),
            js_string!("text"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(response_json),
            js_string!("json"),
            0,
        )
        .build()
}

fn response_text(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(obj) = this.as_object() else {
        return Ok(JsValue::from(js_string!("")));
    };
    obj.get(js_string!("__body"), ctx)
}

fn response_json(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(obj) = this.as_object() else {
        return Ok(JsValue::null());
    };
    let body = obj.get(js_string!("__body"), ctx)?;
    let body_str = body.to_string(ctx)?;
    // Use boa's JSON.parse via the global JSON object.
    let global = ctx.global_object();
    let json = global.get(js_string!("JSON"), ctx)?;
    let json_obj = json
        .as_object()
        .ok_or_else(|| boa_engine::JsNativeError::typ().with_message("JSON unavailable"))?;
    let parse = json_obj.get(js_string!("parse"), ctx)?;
    let parse_fn = parse
        .as_object()
        .and_then(|o| JsFunction::from_object(o.clone()))
        .ok_or_else(|| boa_engine::JsNativeError::typ().with_message("JSON.parse unavailable"))?;
    parse_fn.call(&json.clone(), &[JsValue::from(body_str)], ctx)
}


fn bubble_chain(dom: &Dom, target: NodeId) -> Vec<NodeId> {
    // Ignore non-element targets — events shouldn't fire on Text /
    // Document directly in our toy model.
    if !matches!(dom.node(target).kind, NodeKind::Element { .. }) {
        return Vec::new();
    }
    let mut chain = Vec::new();
    let mut cur = Some(target);
    while let Some(n) = cur {
        if matches!(dom.node(n).kind, NodeKind::Element { .. }) {
            chain.push(n);
        }
        cur = dom.node(n).parent;
    }
    chain
}

fn build_event_object(
    ctx: &mut Context,
    event_type: &str,
    target: NodeId,
) -> boa_engine::JsObject {
    let target_handle = js_dom::make_element_handle(ctx, target);
    let realm = ctx.realm().clone();
    let prevent_default = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(event_prevent_default),
    )
    .build();
    let stop_propagation = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(event_stop_propagation),
    )
    .build();

    ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(js_string!(event_type)),
            Attribute::READONLY,
        )
        .property(
            js_string!("target"),
            JsValue::from(target_handle.clone()),
            Attribute::READONLY,
        )
        .property(
            js_string!("currentTarget"),
            JsValue::from(target_handle),
            Attribute::WRITABLE,
        )
        .property(
            js_string!("preventDefault"),
            JsValue::from(prevent_default),
            Attribute::READONLY,
        )
        .property(
            js_string!("stopPropagation"),
            JsValue::from(stop_propagation),
            Attribute::READONLY,
        )
        .build()
}

fn event_prevent_default(
    _: &JsValue,
    _: &[JsValue],
    _: &mut Context,
) -> JsResult<JsValue> {
    EVENT_FLAGS.with(|f| f.borrow_mut().default_prevented = true);
    Ok(JsValue::undefined())
}

fn event_stop_propagation(
    _: &JsValue,
    _: &[JsValue],
    _: &mut Context,
) -> JsResult<JsValue> {
    EVENT_FLAGS.with(|f| f.borrow_mut().propagation_stopped = true);
    Ok(JsValue::undefined())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use crate::js::dom::find_for_test_by_id;

    #[test]
    fn dispatch_runs_registered_listener_on_target() {
        // Register a listener that mutates the target's id. After
        // dispatching click on it, the id should change.
        let src = r#"
            var el = document.getElementById('hi');
            el.addEventListener('click', function(ev) {
                ev.currentTarget.id = 'clicked';
            });
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut engine = JsEngine::new(&mut dom);
        let target = find_for_test_by_id(&dom, "hi").unwrap();
        engine.dispatch_event(&mut dom, "click", target);
        // The setter ran, so the id was rewritten.
        assert!(find_for_test_by_id(&dom, "clicked").is_some());
    }

    #[test]
    fn dispatch_bubbles_and_can_stop_propagation() {
        let src = r#"
            var outer = document.getElementById('outer');
            var inner = document.getElementById('inner');
            outer.addEventListener('click', function() {
                outer.setAttribute('data-outer', 'fired');
            });
            inner.addEventListener('click', function(ev) {
                inner.setAttribute('data-inner', 'fired');
                ev.stopPropagation();
            });
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='outer'><div id='inner'>x</div></div><script>{src}</script></body></html>"
        ));
        let mut engine = JsEngine::new(&mut dom);
        let inner = find_for_test_by_id(&dom, "inner").unwrap();
        engine.dispatch_event(&mut dom, "click", inner);

        let outer = find_for_test_by_id(&dom, "outer").unwrap();
        let inner = find_for_test_by_id(&dom, "inner").unwrap();
        match &dom.node(inner).kind {
            NodeKind::Element { attrs, .. } => {
                assert_eq!(
                    attrs.iter().find(|(k, _)| k == "data-inner").map(|(_, v)| v.as_str()),
                    Some("fired")
                );
            }
            _ => panic!(),
        }
        match &dom.node(outer).kind {
            NodeKind::Element { attrs, .. } => {
                // Propagation stopped — outer never fired.
                assert!(attrs.iter().all(|(k, _)| k != "data-outer"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn set_timeout_zero_ms_fires_on_pump() {
        let src = r#"
            setTimeout(function() {
                document.getElementById('hi').setAttribute('data-fired', 'yes');
            }, 0);
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut engine = JsEngine::new(&mut dom);
        // Pumping immediately fires zero-ms timers.
        engine.pump_timers(&mut dom);

        let div = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(div).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-fired").map(|(_, v)| v.as_str()),
                Some("yes")
            );
        }
    }

    #[test]
    fn clear_timeout_cancels_pending_timer() {
        let src = r#"
            var id = setTimeout(function() {
                document.getElementById('hi').setAttribute('data-bad', '1');
            }, 0);
            clearTimeout(id);
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut engine = JsEngine::new(&mut dom);
        engine.pump_timers(&mut dom);
        let div = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(div).kind {
            assert!(attrs.iter().all(|(k, _)| k != "data-bad"));
        }
    }

    #[test]
    fn set_interval_re_arms_until_cleared() {
        let src = r#"
            var n = 0;
            var id = setInterval(function() {
                n++;
                if (n >= 3) {
                    clearInterval(id);
                }
                document.getElementById('hi').setAttribute('data-n', String(n));
            }, 0);
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut engine = JsEngine::new(&mut dom);
        // Each pump fires whatever's due. With 0 ms intervals, the first
        // pump fires the timer once; after firing, it re-arms with `now`
        // as the base, so the next pump (later in wall-clock) fires it
        // again. Sleep a hair to make sure the re-armed timer is due.
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            engine.pump_timers(&mut dom);
        }
        let div = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(div).kind {
            let n = attrs
                .iter()
                .find(|(k, _)| k == "data-n")
                .map(|(_, v)| v.as_str())
                .unwrap_or("0");
            assert_eq!(n, "3");
        }
    }

    #[test]
    fn fetch_without_client_returns_stub_promise() {
        // No client plumbed in → promise resolves to ok=false stub. Use
        // a real .then() — the engine drains the microtask queue after
        // each script, so the handler fires before run_initial_scripts
        // returns.
        let src = r#"
            fetch('https://example.com/').then(function(resp) {
                var el = document.getElementById('hi');
                el.setAttribute('data-ok', String(resp.ok));
                el.setAttribute('data-status', String(resp.status));
                el.setAttribute('data-body', resp.text());
            });
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut _engine = JsEngine::new(&mut dom);
        let div = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(div).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-ok").map(|(_, v)| v.as_str()),
                Some("false")
            );
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-status").map(|(_, v)| v.as_str()),
                Some("0")
            );
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-body").map(|(_, v)| v.as_str()),
                Some("")
            );
        }
    }

    #[test]
    fn await_fetch_works_inside_async_function() {
        // `await` on the returned Promise should resolve to the same
        // stub response, end-to-end.
        let src = r#"
            (async function() {
                var resp = await fetch('https://example.com/');
                document.getElementById('hi').setAttribute('data-ok', String(resp.ok));
            })();
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut _engine = JsEngine::new(&mut dom);
        let div = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(div).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-ok").map(|(_, v)| v.as_str()),
                Some("false")
            );
        }
    }

    #[test]
    fn local_and_session_storage_round_trip() {
        let src = r#"
            localStorage.setItem('hello', 'world');
            sessionStorage.setItem('a', '1');
            sessionStorage.setItem('b', '2');
            var el = document.getElementById('hi');
            el.setAttribute('data-local', localStorage.getItem('hello'));
            el.setAttribute('data-len', String(sessionStorage.length));
            el.setAttribute('data-key0', sessionStorage.key(0));
            sessionStorage.removeItem('a');
            el.setAttribute('data-after-remove', String(sessionStorage.length));
            sessionStorage.clear();
            el.setAttribute('data-after-clear', String(sessionStorage.length));
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut _engine = JsEngine::new(&mut dom);
        let div = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(div).kind {
            let get = |k: &str| {
                attrs
                    .iter()
                    .find(|(name, _)| name == k)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(get("data-local"), Some("world"));
            assert_eq!(get("data-len"), Some("2"));
            assert_eq!(get("data-key0"), Some("a"));
            assert_eq!(get("data-after-remove"), Some("1"));
            assert_eq!(get("data-after-clear"), Some("0"));
        }
    }

    #[test]
    fn window_is_alias_for_global() {
        let src = r#"
            // `window` should be the global object: window === globalThis,
            // and `window.foo = ...` should be visible as a bare global.
            window.greeting = 'hi';
            var el = document.getElementById('hi');
            el.setAttribute('data-eq', String(window === globalThis));
            el.setAttribute('data-greeting', greeting);
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut _engine = JsEngine::new(&mut dom);
        let div = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(div).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-eq").map(|(_, v)| v.as_str()),
                Some("true")
            );
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-greeting").map(|(_, v)| v.as_str()),
                Some("hi")
            );
        }
    }

    #[test]
    fn prevent_default_flag_propagates_in_result() {
        let src = r#"
            document.getElementById('hi').addEventListener('click', function(ev) {
                ev.preventDefault();
            });
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><a id='hi' href='#x'>z</a><script>{src}</script></body></html>"
        ));
        let mut engine = JsEngine::new(&mut dom);
        let target = find_for_test_by_id(&dom, "hi").unwrap();
        let r = engine.dispatch_event(&mut dom, "click", target);
        assert!(r.default_prevented);
    }
}
