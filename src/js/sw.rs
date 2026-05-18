//! Service Workers + Cache API (toy).
//!
//! `navigator.serviceWorker.register(url)` fetches the URL and
//! evaluates the worker source *in the page's JS context* with a
//! shadowed `self` / `addEventListener` so handlers register into a
//! per-page handler table. A real browser runs SW in an isolated
//! globalScope on its own thread; we trade isolation for the ability
//! to call the registered fetch handler synchronously from
//! `js_fetch()` and capture whatever the handler passes to
//! `event.respondWith(...)`.
//!
//! Cache API: `caches.open(name)` returns a Cache wrapping an
//! origin-scoped key/value store of (request URL → response body).
//!
//! Scope cut for the toy:
//!  * SW shares the page Context, so SW code can see DOM globals.
//!  * No install / activate lifecycle events (handlers fire only on
//!    `fetch`).
//!  * No update versioning.
//!  * Cache match is exact-URL only (no Vary handling).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, builtins::JsFunction, builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction, Source,
};

/// Per-name cache backing store (URL → body).
pub type CacheStore = HashMap<String, String>;
pub type Caches = Rc<RefCell<HashMap<String, CacheStore>>>;

thread_local! {
    pub(crate) static JS_CACHES: RefCell<Option<Caches>> = const { RefCell::new(None) };
    pub(crate) static JS_SW_SOURCES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Registered `fetch` event handlers. One Vec entry per
    /// `self.addEventListener('fetch', cb)` call.
    pub(crate) static SW_FETCH_HANDLERS: RefCell<Vec<JsFunction>> =
        const { RefCell::new(Vec::new()) };
    /// Captured `event.respondWith(body)` value, drained per fetch.
    pub(crate) static SW_RESPONSE_SLOT: RefCell<Option<String>> =
        const { RefCell::new(None) };
}

pub fn install(ctx: &mut Context) {
    install_caches(ctx);
    install_service_worker(ctx);
    install_sw_register_handler(ctx);
}

/// Expose a global `__sw_register_handler__(type, handler)` that the
/// SW IIFE wrapper calls in place of `self.addEventListener`. Pushes
/// the handler into [`SW_FETCH_HANDLERS`] for later invocation by
/// `try_intercept_fetch`.
fn install_sw_register_handler(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("__sw_register_handler__"),
        2,
        NativeFunction::from_fn_ptr(sw_register_handler),
    )
    .ok();
}

fn sw_register_handler(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(ty_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let ty = ty_val.to_string(ctx)?.to_std_string_escaped();
    if ty != "fetch" {
        // Other event types (install/activate/message) aren't wired
        // in the toy; drop them silently.
        return Ok(JsValue::undefined());
    }
    let Some(handler_val) = args.get(1) else {
        return Ok(JsValue::undefined());
    };
    let Some(handler_obj) = handler_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
        return Ok(JsValue::undefined());
    };
    SW_FETCH_HANDLERS.with(|slot| slot.borrow_mut().push(handler));
    Ok(JsValue::undefined())
}

/// Called from `js_fetch` before any network I/O. If a service worker
/// has registered a `fetch` handler, run it with a synthetic FetchEvent
/// and return whatever it passed to `event.respondWith(...)`. Returns
/// `None` if no handler intercepted, signalling fetch should fall
/// through to the network.
pub fn try_intercept_fetch(ctx: &mut Context, url: &str, method: &str) -> Option<String> {
    let handlers: Vec<JsFunction> =
        SW_FETCH_HANDLERS.with(|slot| slot.borrow().iter().cloned().collect());
    if handlers.is_empty() {
        return None;
    }
    // Always clear the slot before invoking — a previous failed
    // intercept must not leak its body.
    SW_RESPONSE_SLOT.with(|slot| *slot.borrow_mut() = None);
    let event = build_fetch_event(ctx, url, method);
    let undef = JsValue::undefined();
    for handler in handlers {
        let _ = handler.call(&undef, &[JsValue::from(event.clone())], ctx);
        let captured = SW_RESPONSE_SLOT.with(|slot| slot.borrow_mut().take());
        if let Some(body) = captured {
            return Some(body);
        }
    }
    None
}

fn build_fetch_event(ctx: &mut Context, url: &str, method: &str) -> boa_engine::JsObject {
    let request = ObjectInitializer::new(ctx)
        .property(
            js_string!("url"),
            JsValue::from(js_string!(url.to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!("method"),
            JsValue::from(js_string!(method.to_string())),
            Attribute::READONLY,
        )
        .build();
    ObjectInitializer::new(ctx)
        .property(
            js_string!("request"),
            JsValue::from(request),
            Attribute::READONLY,
        )
        .property(
            js_string!("type"),
            JsValue::from(js_string!("fetch")),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(fetch_event_respond_with),
            js_string!("respondWith"),
            1,
        )
        .build()
}

/// `event.respondWith(value)` — value may be a string, a Response
/// object (we read `__body`), or a Promise resolving to either. For
/// the toy we extract a body string and stash it for `js_fetch` to
/// surface as the network response.
fn fetch_event_respond_with(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let resolved = resolve_response_to_body(val, ctx);
    if let Some(body) = resolved {
        SW_RESPONSE_SLOT.with(|slot| *slot.borrow_mut() = Some(body));
    }
    Ok(JsValue::undefined())
}

/// Best-effort flattening of whatever `respondWith` received. Handles:
///   * a raw string → use directly
///   * a Response-shaped object → read `__body`
///   * a Promise → drive it once with `then` to capture the resolved
///     value (synchronous because boa's promise resolution is)
fn resolve_response_to_body(val: &JsValue, ctx: &mut Context) -> Option<String> {
    if val.is_string() {
        return val.to_string(ctx).ok().map(|s| s.to_std_string_escaped());
    }
    let obj = val.as_object()?;
    if let Ok(body_val) = obj.get(js_string!("__body"), ctx) {
        if !body_val.is_undefined() && !body_val.is_null() {
            return body_val.to_string(ctx).ok().map(|s| s.to_std_string_escaped());
        }
    }
    // Treat as Promise: install a `then` continuation that stashes the
    // resolved body into a local cell, then run the microtask queue.
    if let Ok(then_val) = obj.get(js_string!("then"), ctx) {
        if then_val.is_callable() {
            let promise: JsPromise = match JsPromise::from_object(obj.clone()) {
                Ok(p) => p,
                Err(_) => return None,
            };
            let cb = NativeFunction::from_fn_ptr(promise_then_capture);
            let realm = ctx.realm().clone();
            let then_cb =
                boa_engine::object::FunctionObjectBuilder::new(&realm, cb).build();
            let _ = promise.then(Some(then_cb), None, ctx);
            ctx.run_jobs();
            return SW_RESPONSE_SLOT.with(|slot| slot.borrow_mut().take());
        }
    }
    None
}

fn promise_then_capture(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    if let Some(body) = resolve_response_to_body(val, ctx) {
        SW_RESPONSE_SLOT.with(|slot| *slot.borrow_mut() = Some(body));
    }
    Ok(JsValue::undefined())
}

// ============ Cache API ============

fn install_caches(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let open = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(caches_open),
    )
    .build();
    let has = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(caches_has),
    )
    .build();
    let delete = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(caches_delete),
    )
    .build();
    let keys = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(caches_keys),
    )
    .build();
    let caches = ObjectInitializer::new(ctx)
        .property(js_string!("open"), JsValue::from(open), Attribute::READONLY)
        .property(js_string!("has"), JsValue::from(has), Attribute::READONLY)
        .property(js_string!("delete"), JsValue::from(delete), Attribute::READONLY)
        .property(js_string!("keys"), JsValue::from(keys), Attribute::READONLY)
        .build();
    let _ = ctx.register_global_property(
        js_string!("caches"),
        caches,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn caches_open(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(name_val) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    JS_CACHES.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state.borrow_mut().entry(name.clone()).or_default();
        }
    });
    let cache = make_cache_object(ctx, &name);
    Ok(JsPromise::resolve(cache, ctx).into())
}

fn caches_has(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let exists = match args.first() {
        Some(v) => {
            let name = v.to_string(ctx)?.to_std_string_escaped();
            JS_CACHES.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .map(|state| state.borrow().contains_key(&name))
                    .unwrap_or(false)
            })
        }
        None => false,
    };
    Ok(JsPromise::resolve(JsValue::from(exists), ctx).into())
}

fn caches_delete(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(v) = args.first() {
        let name = v.to_string(ctx)?.to_std_string_escaped();
        JS_CACHES.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                state.borrow_mut().remove(&name);
            }
        });
    }
    Ok(JsPromise::resolve(JsValue::from(true), ctx).into())
}

fn caches_keys(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let keys: Vec<String> = JS_CACHES.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|state| state.borrow().keys().cloned().collect())
            .unwrap_or_default()
    });
    let arr = JsArray::new(ctx);
    for k in keys {
        let _ = arr.push(JsValue::from(js_string!(k)), ctx);
    }
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

fn make_cache_object(ctx: &mut Context, name: &str) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("__cache_name"),
        JsValue::from(js_string!(name.to_string())),
        Attribute::READONLY,
    );
    b.function(NativeFunction::from_fn_ptr(cache_put), js_string!("put"), 2);
    b.function(NativeFunction::from_fn_ptr(cache_match), js_string!("match"), 1);
    b.function(NativeFunction::from_fn_ptr(cache_delete), js_string!("delete"), 1);
    b.function(NativeFunction::from_fn_ptr(cache_keys), js_string!("keys"), 0);
    JsValue::from(b.build())
}

fn cache_name(this: &JsValue, ctx: &mut Context) -> String {
    this.as_object()
        .and_then(|o| o.get(js_string!("__cache_name"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default()
}

fn cache_put(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = cache_name(this, ctx);
    let (Some(key_val), Some(val_val)) = (args.first(), args.get(1)) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let key = key_val.to_string(ctx)?.to_std_string_escaped();
    let value = val_val.to_string(ctx)?.to_std_string_escaped();
    JS_CACHES.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state
                .borrow_mut()
                .entry(name)
                .or_default()
                .insert(key, value);
        }
    });
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn cache_match(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = cache_name(this, ctx);
    let Some(key_val) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let key = key_val.to_string(ctx)?.to_std_string_escaped();
    let value = JS_CACHES.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|state| state.borrow().get(&name).and_then(|c| c.get(&key).cloned()))
    });
    let result = match value {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::undefined(),
    };
    Ok(JsPromise::resolve(result, ctx).into())
}

fn cache_delete(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = cache_name(this, ctx);
    if let Some(key_val) = args.first() {
        let key = key_val.to_string(ctx)?.to_std_string_escaped();
        JS_CACHES.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                if let Some(cache) = state.borrow_mut().get_mut(&name) {
                    cache.remove(&key);
                }
            }
        });
    }
    Ok(JsPromise::resolve(JsValue::from(true), ctx).into())
}

fn cache_keys(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = cache_name(this, ctx);
    let keys: Vec<String> = JS_CACHES.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|state| state.borrow().get(&name).map(|c| c.keys().cloned().collect()))
            .unwrap_or_default()
    });
    let arr = JsArray::new(ctx);
    for k in keys {
        let _ = arr.push(JsValue::from(js_string!(k)), ctx);
    }
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

// ============ Service Worker ============

fn install_service_worker(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let register = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(sw_register),
    )
    .build();
    let get_registrations = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(sw_get_registrations),
    )
    .build();
    let service_worker = ObjectInitializer::new(ctx)
        .property(
            js_string!("register"),
            JsValue::from(register),
            Attribute::READONLY,
        )
        .property(
            js_string!("getRegistrations"),
            JsValue::from(get_registrations),
            Attribute::READONLY,
        )
        .property(
            js_string!("controller"),
            JsValue::null(),
            Attribute::all(),
        )
        .build();
    // Hang `serviceWorker` off `navigator`.
    let global = ctx.global_object();
    if let Ok(nav) = global.get(js_string!("navigator"), ctx) {
        if let Some(nav_obj) = nav.as_object() {
            let _ = nav_obj.set(
                js_string!("serviceWorker"),
                JsValue::from(service_worker),
                false,
                ctx,
            );
        }
    }
}

fn sw_register(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(url_val) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let url = url_val.to_string(ctx)?.to_std_string_escaped();

    // Fetch the source through the SSRF-guarded client.
    let client = super::engine::JS_FETCH_CLIENT.with(|c| c.borrow().clone());
    let base = super::engine::JS_BASE_URL.with(|u| u.borrow().clone());
    let resolved = match base {
        Some(b) => b.join(&url).ok(),
        None => url::Url::parse(&url).ok(),
    };
    let Some(resolved) = resolved else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let source = match client {
        Some(c) => match c.get(&resolved.to_string()) {
            Ok(resp) if (200..300).contains(&resp.status) => {
                String::from_utf8_lossy(&resp.body).into_owned()
            }
            _ => return Ok(JsPromise::resolve(JsValue::null(), ctx).into()),
        },
        None => return Ok(JsPromise::resolve(JsValue::null(), ctx).into()),
    };

    // Run the worker source in the *page* Context so the handlers it
    // registers live in our SW_FETCH_HANDLERS table and can be called
    // back synchronously from `js_fetch`. We shadow `addEventListener`
    // and `self.addEventListener` with a forwarder to
    // `__sw_register_handler__`, and shadow `self` with a local stub.
    // Without this, an SW source that does
    // `addEventListener('fetch', cb)` would either land on the page's
    // global (wrong target) or throw.
    let wrapped = format!(
        "(function() {{ \
            function addEventListener(t, cb) {{ \
                return __sw_register_handler__(t, cb); \
            }} \
            var self = {{ \
                addEventListener: addEventListener, \
                skipWaiting: function() {{}}, \
                clients: {{ claim: function() {{}} }} \
            }}; \
            try {{ {source} }} catch (e) {{ \
                console && console.error && console.error('[sw] threw', e); \
            }} \
        }})();",
        source = source
    );
    if let Err(e) = ctx.eval(Source::from_bytes(wrapped.as_bytes())) {
        eprintln!("[sw] register({url}) threw: {e}");
    }
    JS_SW_SOURCES.with(|slot| slot.borrow_mut().push(source));

    let active = ObjectInitializer::new(ctx).build();
    let registration = ObjectInitializer::new(ctx)
        .property(
            js_string!("scope"),
            JsValue::from(js_string!(resolved.origin().ascii_serialization())),
            Attribute::READONLY,
        )
        .property(
            js_string!("active"),
            JsValue::from(active),
            Attribute::READONLY,
        )
        .build();
    Ok(JsPromise::resolve(JsValue::from(registration), ctx).into())
}

fn sw_get_registrations(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::from(JsArray::new(ctx)), ctx).into())
}

