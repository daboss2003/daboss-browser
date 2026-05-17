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

/// Pending `requestAnimationFrame` callbacks. Each frame we drain the
/// queue (matching browser semantics — callbacks scheduled *during* a
/// frame run on the next frame, not this one).
pub(crate) struct AnimationFrameEntry {
    pub(crate) id: u32,
    pub(crate) callback: JsFunction,
}

#[derive(Default)]
pub(crate) struct AnimationFrameQueue {
    pub(crate) pending: Vec<AnimationFrameEntry>,
    pub(crate) next_id: u32,
}

/// One entry on the history stack. `state` is currently always `null`
/// (we don't serialise JS values into Rust yet) but the URL part is
/// honoured. Real browsers store an arbitrary structured-cloneable
/// payload here.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub url: url::Url,
}

#[derive(Default, Debug)]
pub struct JsHistory {
    pub entries: Vec<HistoryEntry>,
    pub cursor: usize,
}

/// Navigation requests scripts emit (via `location.*` / `history.*`)
/// that the browser shell processes after the current dispatch tick.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `.0` is read by `main.rs::process_js_nav_requests`
pub enum NavRequest {
    /// `location.assign(url)` / `location.href = url` — push and load.
    Assign(String),
    /// `location.replace(url)` — replace current entry then load.
    Replace(String),
    /// `location.reload()`
    Reload,
    /// `history.back()` / `history.forward()` / `history.go(n)`.
    /// Positive `n` moves forward, negative back.
    Go(i32),
}

thread_local! {
    pub(crate) static JS_LISTENERS: RefCell<Option<Rc<RefCell<ListenerMap>>>> =
        const { RefCell::new(None) };

    pub(crate) static JS_TIMERS: RefCell<Option<Rc<RefCell<TimerState>>>> =
        const { RefCell::new(None) };

    pub(crate) static JS_RAF: RefCell<Option<Rc<RefCell<AnimationFrameQueue>>>> =
        const { RefCell::new(None) };

    pub(crate) static JS_FETCH_CLIENT: RefCell<Option<Rc<net::Client>>> =
        const { RefCell::new(None) };

    pub(crate) static JS_BASE_URL: RefCell<Option<url::Url>> = const { RefCell::new(None) };

    /// Mutable current URL exposed to JS via `location.*` and mutated by
    /// `history.pushState` / `history.replaceState`. The browser shell
    /// refreshes this slot on every navigation.
    pub(crate) static JS_LOCATION: RefCell<Option<Rc<RefCell<url::Url>>>> =
        const { RefCell::new(None) };

    /// Queue of navigation requests issued by scripts (location.assign,
    /// history.back, etc.). The browser drains it after each event /
    /// timer / rAF dispatch.
    pub(crate) static JS_NAV_REQUESTS: RefCell<Option<Rc<RefCell<Vec<NavRequest>>>>> =
        const { RefCell::new(None) };

    /// History state stack (URLs only — `pushState` is supported, real
    /// state-object storage isn't yet).
    pub(crate) static JS_HISTORY: RefCell<Option<Rc<RefCell<JsHistory>>>> =
        const { RefCell::new(None) };

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
    /// `requestAnimationFrame` queue. Drained at the top of every paint
    /// cycle.
    raf: Rc<RefCell<AnimationFrameQueue>>,
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
    /// Wall-clock origin used by `performance.now()`. Set when the
    /// engine is constructed (page load time).
    perf_origin: Instant,
    /// Mutable current URL — `location.*` reads from here, History API
    /// pushes mutate it.
    location_url: Rc<RefCell<url::Url>>,
    /// Pending navigation requests emitted by JS. Browser drains.
    nav_requests: Rc<RefCell<Vec<NavRequest>>>,
    /// In-engine history stack for the History API.
    history: Rc<RefCell<JsHistory>>,
}

/// Outcome of an event dispatch — informs the caller whether to skip the
/// built-in action (preventDefault) and whether to re-cascade and
/// re-layout (any mutation happened).
#[derive(Default, Clone, Copy)]
pub struct DispatchResult {
    pub default_prevented: bool,
    pub mutated: bool,
}

/// Optional per-event-type properties to attach to the JS event object.
/// Fields are `None` when not applicable.
#[derive(Default, Clone)]
pub struct EventInit {
    /// `bubbles` defaults to true for most user-fired events; some
    /// lifecycle events (`load`, `focus`/`blur` to a degree) don't bubble.
    pub bubbles: bool,
    /// MouseEvent.clientX / clientY
    pub client_x: Option<f32>,
    pub client_y: Option<f32>,
    /// KeyboardEvent.key / code / modifier flags
    pub key: Option<String>,
    pub code: Option<String>,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
    /// InputEvent.data (the inserted text) and input.value at time of fire.
    pub input_data: Option<String>,
}

impl EventInit {
    pub const fn bubbling() -> Self {
        Self {
            bubbles: true,
            client_x: None,
            client_y: None,
            key: None,
            code: None,
            ctrl: false,
            shift: false,
            alt: false,
            meta: false,
            input_data: None,
        }
    }
    #[allow(dead_code)] // used by load/focus/blur dispatch when wired
    pub const fn non_bubbling() -> Self {
        Self {
            bubbles: false,
            client_x: None,
            client_y: None,
            key: None,
            code: None,
            ctrl: false,
            shift: false,
            alt: false,
            meta: false,
            input_data: None,
        }
    }
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
        let perf_origin = Instant::now();
        let mut ctx = Context::default();
        install_console(&mut ctx);
        js_dom::install(&mut ctx);
        install_timer_globals(&mut ctx);
        install_animation_frame_globals(&mut ctx);
        install_fetch_global(&mut ctx);
        storage::install(&mut ctx);
        install_window_alias(&mut ctx);
        install_navigator_screen_performance(&mut ctx);
        install_location_and_history_globals(&mut ctx);

        let listeners: Rc<RefCell<ListenerMap>> = Rc::new(RefCell::new(HashMap::new()));
        let timers: Rc<RefCell<TimerState>> = Rc::new(RefCell::new(TimerState::default()));
        let raf: Rc<RefCell<AnimationFrameQueue>> =
            Rc::new(RefCell::new(AnimationFrameQueue::default()));
        let local_storage = local_storage
            .unwrap_or_else(|| Rc::new(RefCell::new(HashMap::new())));
        let session_storage: StorageArea = Rc::new(RefCell::new(HashMap::new()));
        let initial_url = base_url
            .clone()
            .unwrap_or_else(|| url::Url::parse("about:blank").unwrap());
        let location_url = Rc::new(RefCell::new(initial_url.clone()));
        let nav_requests: Rc<RefCell<Vec<NavRequest>>> = Rc::new(RefCell::new(Vec::new()));
        let history = Rc::new(RefCell::new(JsHistory {
            entries: vec![HistoryEntry {
                url: initial_url,
            }],
            cursor: 0,
        }));
        let mut engine = JsEngine {
            ctx,
            listeners,
            timers,
            raf,
            fetch_client: client,
            base_url,
            local_storage,
            session_storage,
            perf_origin,
            location_url,
            nav_requests,
            history,
        };
        engine.run_initial_scripts(dom);
        engine
    }

    /// Drain every pending navigation request scripts have queued. The
    /// browser shell calls this after each event / timer / rAF tick.
    pub fn drain_nav_requests(&self) -> Vec<NavRequest> {
        let mut q = self.nav_requests.borrow_mut();
        std::mem::take(&mut *q)
    }

    /// Update the engine's view of the current URL when the browser
    /// navigates (e.g. user clicked a link). Keeps `location.*` in sync
    /// without producing a navigation request.
    #[allow(dead_code)] // called by Browser once per-history-step popstate is wired
    pub fn set_current_url(&self, url: url::Url) {
        *self.location_url.borrow_mut() = url.clone();
        let mut h = self.history.borrow_mut();
        let new_len = h.cursor + 1;
        h.entries.truncate(new_len);
        h.entries.push(HistoryEntry { url });
        h.cursor = h.entries.len() - 1;
    }

    /// Run every `requestAnimationFrame` callback queued so far. New
    /// callbacks scheduled by them go into the next frame (matching
    /// browser behaviour). Returns whether the DOM grew (the caller
    /// uses this as a re-layout signal).
    pub fn pump_animation_frames(&mut self, dom: &mut Dom) -> DispatchResult {
        let due: Vec<AnimationFrameEntry> = {
            let mut q = self.raf.borrow_mut();
            std::mem::take(&mut q.pending)
        };
        if due.is_empty() {
            return DispatchResult::default();
        }
        let (dom_rc, listeners_rc) = self.install_thread_locals(dom);
        let pre_count = dom_rc.borrow().node_count();

        let elapsed_ms = self.perf_origin.elapsed().as_secs_f64() * 1000.0;
        let ts = JsValue::from(elapsed_ms);
        for entry in due {
            if let Err(e) = entry.callback.call(&JsValue::undefined(), &[ts.clone()], &mut self.ctx)
            {
                eprintln!("[js] rAF #{} threw: {e}", entry.id);
            }
        }
        self.ctx.run_jobs();

        let mutated = dom_rc.borrow().node_count() != pre_count;
        self.uninstall_thread_locals(dom, dom_rc, listeners_rc);
        DispatchResult {
            default_prevented: false,
            mutated,
        }
    }

    pub fn has_pending_animation_frames(&self) -> bool {
        !self.raf.borrow().pending.is_empty()
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

    /// Dispatch `event_type` to `target` with bubbling and default init
    /// (no mouse coords, no key info). Equivalent to a bare DOM event.
    #[allow(dead_code)] // kept as a convenience wrapper for tests and integrations
    pub fn dispatch_event(
        &mut self,
        dom: &mut Dom,
        event_type: &str,
        target: NodeId,
    ) -> DispatchResult {
        self.dispatch_event_with(dom, event_type, target, EventInit::bubbling())
    }

    /// Dispatch with explicit per-event properties (MouseEvent coords,
    /// KeyboardEvent.key, etc.). Bubbling is controlled by `init.bubbles`.
    pub fn dispatch_event_with(
        &mut self,
        dom: &mut Dom,
        event_type: &str,
        target: NodeId,
        init: EventInit,
    ) -> DispatchResult {
        // Build the bubble chain (or singleton chain when non-bubbling)
        // from a snapshot of the live tree.
        let chain = if init.bubbles {
            bubble_chain(dom, target)
        } else if matches!(dom.node(target).kind, NodeKind::Element { .. }) {
            vec![target]
        } else {
            Vec::new()
        };

        // Empty chain means the target isn't an element we can dispatch
        // to (probably text node / document); nothing to do.
        if chain.is_empty() {
            return DispatchResult::default();
        }

        let (dom_rc, listeners_rc) = self.install_thread_locals(dom);
        EVENT_FLAGS.with(|f| *f.borrow_mut() = EventFlags::EMPTY);

        let pre_mutation_marker = dom_rc.borrow().node_count();

        let event_obj = build_event_object_with(&mut self.ctx, event_type, target, &init);

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
        JS_RAF.with(|slot| {
            *slot.borrow_mut() = Some(self.raf.clone());
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
        JS_LOCATION.with(|slot| {
            *slot.borrow_mut() = Some(self.location_url.clone());
        });
        JS_NAV_REQUESTS.with(|slot| {
            *slot.borrow_mut() = Some(self.nav_requests.clone());
        });
        JS_HISTORY.with(|slot| {
            *slot.borrow_mut() = Some(self.history.clone());
        });
        (dom_rc, listeners_rc)
    }

    fn uninstall_thread_locals(
        &mut self,
        dom: &mut Dom,
        dom_rc: Rc<RefCell<Dom>>,
        listeners_rc: Rc<RefCell<ListenerMap>>,
    ) {
        JS_HISTORY.with(|slot| {
            slot.borrow_mut().take();
        });
        JS_NAV_REQUESTS.with(|slot| {
            slot.borrow_mut().take();
        });
        JS_LOCATION.with(|slot| {
            slot.borrow_mut().take();
        });
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
        JS_RAF.with(|slot| {
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
    ctx.register_global_callable(
        js_string!("queueMicrotask"),
        1,
        NativeFunction::from_fn_ptr(queue_microtask),
    )
    .ok();
}

fn install_animation_frame_globals(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("requestAnimationFrame"),
        1,
        NativeFunction::from_fn_ptr(request_animation_frame),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("cancelAnimationFrame"),
        1,
        NativeFunction::from_fn_ptr(cancel_animation_frame),
    )
    .ok();
}

fn request_animation_frame(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let Some(callback) = extract_callback(args.first()) else {
        return Ok(JsValue::from(0));
    };
    let id = JS_RAF.with(|slot| {
        let Some(q_rc) = slot.borrow().as_ref().cloned() else {
            return 0;
        };
        let mut q = q_rc.borrow_mut();
        q.next_id = q.next_id.wrapping_add(1);
        let id = q.next_id;
        q.pending.push(AnimationFrameEntry { id, callback });
        id
    });
    Ok(JsValue::from(id))
}

fn cancel_animation_frame(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Ok(id) = id_val.to_u32(ctx) else {
        return Ok(JsValue::undefined());
    };
    JS_RAF.with(|slot| {
        if let Some(q_rc) = slot.borrow().as_ref() {
            q_rc.borrow_mut().pending.retain(|e| e.id != id);
        }
    });
    Ok(JsValue::undefined())
}

fn queue_microtask(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Stand-in: just call the function before returning. The microtask
    // queue inside boa will drain anything it queues. Not spec-correct
    // (real microtasks run after the current script step completes) but
    // covers the common "schedule shortly" use case.
    let Some(cb) = extract_callback(args.first()) else {
        return Ok(JsValue::undefined());
    };
    let _ = cb.call(&JsValue::undefined(), &[], ctx);
    Ok(JsValue::undefined())
}

/// Install `navigator`, `screen`, and `performance` on the global. All
/// are read-only static snapshots — no permission prompts, no live
/// device data. Enough for code that probes `navigator.userAgent` or
/// uses `performance.now()` to gate timings.
fn install_navigator_screen_performance(ctx: &mut Context) {
    let realm = ctx.realm().clone();

    // navigator
    let navigator = ObjectInitializer::new(ctx)
        .property(
            js_string!("userAgent"),
            JsValue::from(js_string!("daboss/0.1")),
            Attribute::READONLY,
        )
        .property(
            js_string!("appName"),
            JsValue::from(js_string!("DaBoss")),
            Attribute::READONLY,
        )
        .property(
            js_string!("appVersion"),
            JsValue::from(js_string!("0.1")),
            Attribute::READONLY,
        )
        .property(
            js_string!("platform"),
            JsValue::from(js_string!(std::env::consts::OS)),
            Attribute::READONLY,
        )
        .property(
            js_string!("language"),
            JsValue::from(js_string!("en-US")),
            Attribute::READONLY,
        )
        .property(
            js_string!("onLine"),
            JsValue::from(true),
            Attribute::READONLY,
        )
        .property(
            js_string!("cookieEnabled"),
            JsValue::from(true),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("navigator"),
        navigator,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );

    // screen — toy renders into one synthetic 1024x768 viewport unless
    // resized. Real width/height come from JS_VIEWPORT (set per-page).
    let screen = ObjectInitializer::new(ctx)
        .property(
            js_string!("width"),
            JsValue::from(1024_u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("height"),
            JsValue::from(768_u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("availWidth"),
            JsValue::from(1024_u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("availHeight"),
            JsValue::from(768_u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("colorDepth"),
            JsValue::from(24_u32),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("screen"),
        screen,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );

    // performance.now()
    let now_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(performance_now),
    )
    .build();
    let performance = ObjectInitializer::new(ctx)
        .property(
            js_string!("now"),
            JsValue::from(now_fn),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("performance"),
        performance,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn performance_now(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    // Use process-relative high-resolution time. Real browsers expose a
    // page-load-relative origin; close enough for the toy.
    let ms = PERF_ORIGIN.with(|t| t.elapsed().as_secs_f64() * 1000.0);
    Ok(JsValue::from(ms))
}

thread_local! {
    static PERF_ORIGIN: Instant = Instant::now();
}

fn install_location_and_history_globals(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f))
            .build()
    };

    // ---- location ----
    let location = ObjectInitializer::new(ctx);
    let location = {
        let mut b = location;
        b.accessor(
            js_string!("href"),
            Some(getter(location_get_href)),
            Some(getter(location_set_href)),
            Attribute::ENUMERABLE,
        );
        b.accessor(
            js_string!("origin"),
            Some(getter(location_get_origin)),
            None,
            Attribute::ENUMERABLE,
        );
        b.accessor(
            js_string!("protocol"),
            Some(getter(location_get_protocol)),
            None,
            Attribute::ENUMERABLE,
        );
        b.accessor(
            js_string!("host"),
            Some(getter(location_get_host)),
            None,
            Attribute::ENUMERABLE,
        );
        b.accessor(
            js_string!("hostname"),
            Some(getter(location_get_hostname)),
            None,
            Attribute::ENUMERABLE,
        );
        b.accessor(
            js_string!("port"),
            Some(getter(location_get_port)),
            None,
            Attribute::ENUMERABLE,
        );
        b.accessor(
            js_string!("pathname"),
            Some(getter(location_get_pathname)),
            None,
            Attribute::ENUMERABLE,
        );
        b.accessor(
            js_string!("search"),
            Some(getter(location_get_search)),
            None,
            Attribute::ENUMERABLE,
        );
        b.accessor(
            js_string!("hash"),
            Some(getter(location_get_hash)),
            None,
            Attribute::ENUMERABLE,
        );
        b.function(
            NativeFunction::from_fn_ptr(location_assign),
            js_string!("assign"),
            1,
        );
        b.function(
            NativeFunction::from_fn_ptr(location_replace),
            js_string!("replace"),
            1,
        );
        b.function(
            NativeFunction::from_fn_ptr(location_reload),
            js_string!("reload"),
            0,
        );
        b.function(
            NativeFunction::from_fn_ptr(location_to_string),
            js_string!("toString"),
            0,
        );
        b.build()
    };
    let _ = ctx.register_global_property(
        js_string!("location"),
        location,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );

    // ---- history ----
    let history = ObjectInitializer::new(ctx)
        .accessor(
            js_string!("length"),
            Some(getter(history_get_length)),
            None,
            Attribute::ENUMERABLE,
        )
        .accessor(
            js_string!("state"),
            Some(getter(history_get_state)),
            None,
            Attribute::ENUMERABLE,
        )
        .function(
            NativeFunction::from_fn_ptr(history_push_state),
            js_string!("pushState"),
            3,
        )
        .function(
            NativeFunction::from_fn_ptr(history_replace_state),
            js_string!("replaceState"),
            3,
        )
        .function(
            NativeFunction::from_fn_ptr(history_back),
            js_string!("back"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(history_forward),
            js_string!("forward"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(history_go),
            js_string!("go"),
            1,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("history"),
        history,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn current_url() -> Option<url::Url> {
    JS_LOCATION.with(|slot| slot.borrow().as_ref().map(|rc| rc.borrow().clone()))
}

fn enqueue_nav(req: NavRequest) {
    JS_NAV_REQUESTS.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            rc.borrow_mut().push(req);
        }
    });
}

fn location_get_href(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = current_url().map(|u| u.to_string()).unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn location_set_href(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(v) = args.first() {
        let url = v.to_string(ctx)?.to_std_string_escaped();
        enqueue_nav(NavRequest::Assign(url));
    }
    Ok(JsValue::undefined())
}

fn location_get_origin(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = current_url().map(|u| u.origin().ascii_serialization()).unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn location_get_protocol(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = current_url()
        .map(|u| format!("{}:", u.scheme()))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn location_get_host(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = current_url()
        .and_then(|u| u.host_str().map(|h| {
            match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            }
        }))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn location_get_hostname(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = current_url()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn location_get_port(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = current_url()
        .and_then(|u| u.port().map(|p| p.to_string()))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn location_get_pathname(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = current_url().map(|u| u.path().to_string()).unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn location_get_search(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = current_url()
        .and_then(|u| u.query().map(|q| format!("?{q}")))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn location_get_hash(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let s = current_url()
        .and_then(|u| u.fragment().map(|f| format!("#{f}")))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn location_assign(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(v) = args.first() {
        let url = v.to_string(ctx)?.to_std_string_escaped();
        enqueue_nav(NavRequest::Assign(url));
    }
    Ok(JsValue::undefined())
}

fn location_replace(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(v) = args.first() {
        let url = v.to_string(ctx)?.to_std_string_escaped();
        enqueue_nav(NavRequest::Replace(url));
    }
    Ok(JsValue::undefined())
}

fn location_reload(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    enqueue_nav(NavRequest::Reload);
    Ok(JsValue::undefined())
}

fn location_to_string(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    location_get_href(&JsValue::undefined(), &[], &mut Context::default())
}

fn history_get_length(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let n = JS_HISTORY.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|rc| rc.borrow().entries.len() as u32)
            .unwrap_or(0)
    });
    Ok(JsValue::from(n))
}

fn history_get_state(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    // State payloads aren't preserved through Rust yet — return null.
    Ok(JsValue::null())
}

fn history_push_state(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    history_push_or_replace(args, ctx, /*replace=*/ false)
}

fn history_replace_state(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    history_push_or_replace(args, ctx, /*replace=*/ true)
}

fn history_push_or_replace(args: &[JsValue], ctx: &mut Context, replace: bool) -> JsResult<JsValue> {
    // args: (state, title, url)
    let url_arg = args.get(2);
    let Some(url_val) = url_arg else {
        // pushState({}, "") with no URL → no-op for the URL.
        return Ok(JsValue::undefined());
    };
    if url_val.is_null() || url_val.is_undefined() {
        return Ok(JsValue::undefined());
    }
    let url_str = url_val.to_string(ctx)?.to_std_string_escaped();
    let Some(base) = current_url() else {
        return Ok(JsValue::undefined());
    };
    let Ok(new_url) = base.join(&url_str) else {
        return Ok(JsValue::undefined());
    };
    JS_HISTORY.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            let mut h = rc.borrow_mut();
            if replace {
                let idx = h.cursor;
                if let Some(entry) = h.entries.get_mut(idx) {
                    entry.url = new_url.clone();
                }
            } else {
                let new_len = h.cursor + 1;
                h.entries.truncate(new_len);
                h.entries.push(HistoryEntry { url: new_url.clone() });
                h.cursor = h.entries.len() - 1;
            }
        }
    });
    JS_LOCATION.with(|slot| {
        if let Some(rc) = slot.borrow().as_ref() {
            *rc.borrow_mut() = new_url;
        }
    });
    Ok(JsValue::undefined())
}

fn history_back(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    enqueue_nav(NavRequest::Go(-1));
    Ok(JsValue::undefined())
}

fn history_forward(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    enqueue_nav(NavRequest::Go(1));
    Ok(JsValue::undefined())
}

fn history_go(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let n = args
        .first()
        .and_then(|v| v.to_i32(ctx).ok())
        .unwrap_or(0);
    enqueue_nav(NavRequest::Go(n));
    Ok(JsValue::undefined())
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

fn build_event_object_with(
    ctx: &mut Context,
    event_type: &str,
    target: NodeId,
    init: &EventInit,
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

    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("type"),
        JsValue::from(js_string!(event_type)),
        Attribute::READONLY,
    );
    b.property(
        js_string!("target"),
        JsValue::from(target_handle.clone()),
        Attribute::READONLY,
    );
    b.property(
        js_string!("currentTarget"),
        JsValue::from(target_handle),
        Attribute::WRITABLE,
    );
    b.property(
        js_string!("bubbles"),
        JsValue::from(init.bubbles),
        Attribute::READONLY,
    );
    b.property(
        js_string!("preventDefault"),
        JsValue::from(prevent_default),
        Attribute::READONLY,
    );
    b.property(
        js_string!("stopPropagation"),
        JsValue::from(stop_propagation),
        Attribute::READONLY,
    );

    // MouseEvent fields
    if let (Some(x), Some(y)) = (init.client_x, init.client_y) {
        b.property(js_string!("clientX"), JsValue::from(x), Attribute::READONLY);
        b.property(js_string!("clientY"), JsValue::from(y), Attribute::READONLY);
        // pageX / pageY ≈ clientX/Y in our toy (no scrollable iframes
        // beyond top-level). Real browsers add scroll position.
        b.property(js_string!("pageX"), JsValue::from(x), Attribute::READONLY);
        b.property(js_string!("pageY"), JsValue::from(y), Attribute::READONLY);
    }
    // KeyboardEvent fields
    if let Some(k) = &init.key {
        b.property(
            js_string!("key"),
            JsValue::from(js_string!(k.clone())),
            Attribute::READONLY,
        );
    }
    if let Some(c) = &init.code {
        b.property(
            js_string!("code"),
            JsValue::from(js_string!(c.clone())),
            Attribute::READONLY,
        );
    }
    if init.key.is_some() || init.code.is_some() {
        b.property(js_string!("ctrlKey"), JsValue::from(init.ctrl), Attribute::READONLY);
        b.property(js_string!("shiftKey"), JsValue::from(init.shift), Attribute::READONLY);
        b.property(js_string!("altKey"), JsValue::from(init.alt), Attribute::READONLY);
        b.property(js_string!("metaKey"), JsValue::from(init.meta), Attribute::READONLY);
    }
    // InputEvent fields
    if let Some(d) = &init.input_data {
        b.property(
            js_string!("data"),
            JsValue::from(js_string!(d.clone())),
            Attribute::READONLY,
        );
    }

    b.build()
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
    fn keyboard_event_init_props_are_visible_to_handler() {
        let src = r#"
            document.getElementById('hi').addEventListener('keydown', function(ev) {
                document.getElementById('hi').setAttribute('data-key', ev.key);
                document.getElementById('hi').setAttribute('data-shift', String(ev.shiftKey));
            });
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut engine = JsEngine::new(&mut dom);
        let target = find_for_test_by_id(&dom, "hi").unwrap();
        let mut init = EventInit::bubbling();
        init.key = Some("a".into());
        init.shift = true;
        engine.dispatch_event_with(&mut dom, "keydown", target, init);

        if let NodeKind::Element { attrs, .. } = &dom.node(target).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-key").map(|(_, v)| v.as_str()),
                Some("a")
            );
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-shift").map(|(_, v)| v.as_str()),
                Some("true")
            );
        }
    }

    #[test]
    fn request_animation_frame_runs_callback_on_pump() {
        let src = r#"
            requestAnimationFrame(function(ts) {
                document.getElementById('hi').setAttribute('data-raf', 'fired');
            });
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut engine = JsEngine::new(&mut dom);
        engine.pump_animation_frames(&mut dom);
        let id = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-raf").map(|(_, v)| v.as_str()),
                Some("fired")
            );
        }
    }

    #[test]
    fn cancel_animation_frame_removes_pending() {
        let src = r#"
            var id = requestAnimationFrame(function() {
                document.getElementById('hi').setAttribute('data-bad', '1');
            });
            cancelAnimationFrame(id);
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut engine = JsEngine::new(&mut dom);
        engine.pump_animation_frames(&mut dom);
        let id = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            assert!(attrs.iter().all(|(k, _)| k != "data-bad"));
        }
    }

    #[test]
    fn navigator_and_performance_globals_exposed() {
        let src = r#"
            var el = document.getElementById('hi');
            el.setAttribute('data-ua', navigator.userAgent);
            el.setAttribute('data-now-type', typeof performance.now());
            el.setAttribute('data-sw', String(screen.width));
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let mut _engine = JsEngine::new(&mut dom);
        let id = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            assert!(attrs.iter().any(|(k, v)| k == "data-ua" && v.starts_with("daboss")));
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-now-type").map(|(_, v)| v.as_str()),
                Some("number")
            );
            assert_eq!(
                attrs.iter().find(|(k, _)| k == "data-sw").map(|(_, v)| v.as_str()),
                Some("1024")
            );
        }
    }

    #[test]
    fn location_and_history_reflect_url_state() {
        // location reads from JS_LOCATION, which is seeded from the
        // engine's base_url. History.pushState mutates it without
        // emitting a real navigation.
        let src = r#"
            var el = document.getElementById('hi');
            el.setAttribute('data-href-before', location.href);
            el.setAttribute('data-pathname', location.pathname);
            history.pushState({}, '', '/two');
            el.setAttribute('data-href-after', location.href);
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><div id='hi'>x</div><script>{src}</script></body></html>"
        ));
        let base = url::Url::parse("https://example.com/one").unwrap();
        let mut _engine = JsEngine::with_fetch(
            &mut dom,
            None,
            Some(base),
            None,
        );
        let id = find_for_test_by_id(&dom, "hi").unwrap();
        if let NodeKind::Element { attrs, .. } = &dom.node(id).kind {
            let get = |k: &str| {
                attrs
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.as_str())
            };
            assert_eq!(get("data-href-before"), Some("https://example.com/one"));
            assert_eq!(get("data-pathname"), Some("/one"));
            assert_eq!(get("data-href-after"), Some("https://example.com/two"));
        }
    }

    #[test]
    fn location_assign_enqueues_nav_request() {
        let src = r#"
            location.assign('https://example.com/next');
        "#;
        let mut dom = html::parse(&format!(
            "<html><body><script>{src}</script></body></html>"
        ));
        let base = url::Url::parse("https://example.com/").unwrap();
        let engine = JsEngine::with_fetch(&mut dom, None, Some(base), None);
        let reqs = engine.drain_nav_requests();
        assert_eq!(reqs.len(), 1);
        assert!(matches!(reqs[0], NavRequest::Assign(ref u) if u == "https://example.com/next"));
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
