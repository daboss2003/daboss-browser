//! Service Workers + Cache API (toy).
//!
//! `navigator.serviceWorker.register(url)` fetches the URL, builds a
//! separate `boa::Context` for the worker scope, evaluates the
//! source, and returns a Promise that resolves to a
//! `ServiceWorkerRegistration`.
//!
//! Cache API: `caches.open(name)` returns a Cache wrapping an
//! origin-scoped key/value store of (request URL → response body).
//!
//! Scope cut for the toy:
//!  * fetch interception — registered service workers parse but
//!    don't actually intercept `fetch()` calls yet. That requires
//!    threading a synchronous worker round-trip into every
//!    page-side fetch which is its own integration commit.
//!  * No install / activate lifecycle events.
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
}

pub fn install(ctx: &mut Context) {
    install_caches(ctx);
    install_service_worker(ctx);
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

    // Spin up an isolated Context, run the worker source. Errors get
    // logged but don't prevent registration.
    let mut worker_ctx = Context::default();
    super::install_console(&mut worker_ctx);
    // Worker globals: `self.addEventListener`, no DOM. We provide a
    // very thin stub so installs don't throw.
    register_sw_self(&mut worker_ctx);
    if let Err(e) = worker_ctx.eval(Source::from_bytes(source.as_bytes())) {
        eprintln!("[sw] register({url}) threw: {e}");
    }
    // We don't keep the worker Context alive — the toy registration
    // is parse-and-discard. A real implementation would persist it
    // and route page fetches through its `fetch` event handlers.
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

fn register_sw_self(ctx: &mut Context) {
    // Provide a stub `self.addEventListener` so SW scripts can call it
    // without errors.
    let realm = ctx.realm().clone();
    let listener = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(sw_add_event_listener),
    )
    .build();
    let skip_waiting = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(sw_skip_waiting),
    )
    .build();
    let self_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("addEventListener"),
            JsValue::from(listener),
            Attribute::READONLY,
        )
        .property(
            js_string!("skipWaiting"),
            JsValue::from(skip_waiting),
            Attribute::READONLY,
        )
        .build();
    let global = ctx.global_object();
    let _ = global.set(
        js_string!("self"),
        JsValue::from(self_obj),
        false,
        ctx,
    );
}

fn sw_add_event_listener(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    // Real implementation would record the (type, handler) pair so
    // page fetches could route through. Toy keeps it parse-clean.
    let _ = args;
    Ok(JsValue::undefined())
}

fn sw_skip_waiting(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

/// Make a JsFunction wrapper available for the future fetch
/// interception path. Currently unused.
#[allow(dead_code)]
pub fn dispatch_fetch_intercept(_ctx: &mut Context) -> Option<JsFunction> {
    None
}
