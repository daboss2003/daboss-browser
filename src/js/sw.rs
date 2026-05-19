//! Service Workers + Cache API.
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
//! origin-scoped key/value store of `(request URL → CacheEntry)`.
//! Each `CacheEntry` carries the response status, reason, headers
//! and body bytes — pages get a real Response back from
//! `cache.match`.
//!
//! Persistence:
//!   * Cache entries land on disk under
//!     `<data_dir>/daboss-sw-caches/<origin>/<cache-name>/<url-hex>.bin`
//!     so PWAs survive restarts and stay offline-ready.
//!   * Service-worker registrations (scope, script URL, last-seen
//!     source) are stored at `<data_dir>/daboss-sw/<origin>/registrations.bin`
//!     and re-evaluated on engine boot so the previous SW
//!     immediately controls the new navigation.
//!
//! Lifecycle: on `register`, we evaluate the source then synthesise
//! `install` → `activate` events back-to-back. `event.waitUntil()`
//! is accepted but the toy doesn't block on the supplied promise —
//! activation completes synchronously. `navigator.serviceWorker.controller`
//! is then populated so pages that gate behaviour on it ("if SW is
//! in control") run their SW-controlled branches.
//!
//! Scope cut for the toy:
//!   * SW shares the page Context, so SW code can see DOM globals.
//!   * Cache.match is exact-URL only (no Vary handling).
//!   * No message channels between SW and page.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, builtins::JsFunction, builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction, Source,
};

const CACHE_FILE_MAGIC: &[u8; 4] = b"DBCE";
const CACHE_FILE_VERSION: u8 = 1;
const SW_REG_MAGIC: &[u8; 4] = b"DBSW";
const SW_REG_VERSION: u8 = 1;

#[derive(Debug, Clone, Default)]
pub struct CacheEntry {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Per-name cache backing store (request URL → entry).
pub type CacheStore = HashMap<String, CacheEntry>;
pub type Caches = Rc<RefCell<HashMap<String, CacheStore>>>;

thread_local! {
    pub(crate) static JS_CACHES: RefCell<Option<Caches>> = const { RefCell::new(None) };
    pub(crate) static JS_SW_SOURCES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Per-event-type handlers registered through `self.addEventListener`.
    /// Keyed by event type ("fetch" / "install" / "activate" / "message").
    pub(crate) static SW_HANDLERS: RefCell<HashMap<String, Vec<JsFunction>>> =
        RefCell::new(HashMap::new());
    /// Captured `event.respondWith(body)` value, drained per fetch.
    pub(crate) static SW_RESPONSE_SLOT: RefCell<Option<CacheEntry>> = RefCell::new(None);
    /// True once we've loaded persisted caches into memory for the
    /// current origin. Gates the lazy on-open fault-in.
    static CACHES_LOADED_ORIGIN: RefCell<Option<String>> = const { RefCell::new(None) };
    /// True once we've replayed disk-backed SW registrations.
    static REGS_LOADED_ORIGIN: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn install(ctx: &mut Context) {
    install_caches(ctx);
    install_service_worker(ctx);
    install_sw_register_handler(ctx);
    // Replay any persisted SW registrations for the current origin so
    // a previously-installed SW takes control of the new page.
    replay_persisted_registrations(ctx);
}

/// Expose a global `__sw_register_handler__(type, handler)` that the
/// SW IIFE wrapper calls in place of `self.addEventListener`.
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
    let Some(handler_val) = args.get(1) else {
        return Ok(JsValue::undefined());
    };
    let Some(handler_obj) = handler_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
        return Ok(JsValue::undefined());
    };
    SW_HANDLERS.with(|slot| {
        slot.borrow_mut().entry(ty).or_default().push(handler);
    });
    Ok(JsValue::undefined())
}

/// Called from `js_fetch` before any network I/O. Runs every
/// registered `fetch` handler with a synthetic FetchEvent and
/// returns whatever it passed to `event.respondWith(...)`.
pub fn try_intercept_fetch(ctx: &mut Context, url: &str, method: &str) -> Option<CacheEntry> {
    let handlers: Vec<JsFunction> = SW_HANDLERS.with(|slot| {
        slot.borrow()
            .get("fetch")
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default()
    });
    if handlers.is_empty() {
        return None;
    }
    SW_RESPONSE_SLOT.with(|slot| *slot.borrow_mut() = None);
    let event = build_fetch_event(ctx, url, method);
    let undef = JsValue::undefined();
    for handler in handlers {
        let _ = handler.call(&undef, &[JsValue::from(event.clone())], ctx);
        let captured = SW_RESPONSE_SLOT.with(|slot| slot.borrow_mut().take());
        if let Some(entry) = captured {
            return Some(entry);
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
        .function(
            NativeFunction::from_fn_ptr(lifecycle_event_wait_until),
            js_string!("waitUntil"),
            1,
        )
        .build()
}

fn fetch_event_respond_with(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    if let Some(entry) = resolve_to_cache_entry(val, ctx) {
        SW_RESPONSE_SLOT.with(|slot| *slot.borrow_mut() = Some(entry));
    }
    Ok(JsValue::undefined())
}

fn lifecycle_event_wait_until(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Drive the supplied promise (if any) so any `.then` work
    // queued by the SW author runs before we move to the next
    // lifecycle phase. We don't actually block — the spec says
    // activation waits on the install promise, but the toy can
    // assume install work is synchronous.
    if let Some(val) = args.first() {
        if let Some(obj) = val.as_object() {
            if JsPromise::from_object(obj.clone()).is_ok() {
                ctx.run_jobs();
            }
        }
    }
    Ok(JsValue::undefined())
}

/// Flatten whatever was passed to respondWith into a CacheEntry.
/// Accepts:
///   * a raw string → 200 OK with that body
///   * a Response-shaped object → reads status / __body / headers
///   * a Promise → resolve via `then`, recurse
fn resolve_to_cache_entry(val: &JsValue, ctx: &mut Context) -> Option<CacheEntry> {
    if val.is_string() {
        let body = val.to_string(ctx).ok()?.to_std_string_escaped();
        return Some(CacheEntry {
            status: 200,
            reason: "OK".into(),
            headers: Vec::new(),
            body: body.into_bytes(),
        });
    }
    let obj = val.as_object()?;
    // Response shape: status, statusText, body / __body, headers.
    if let Ok(status_val) = obj.get(js_string!("status"), ctx) {
        if !status_val.is_undefined() {
            let status = status_val.to_u32(ctx).unwrap_or(200) as u16;
            let reason = obj
                .get(js_string!("statusText"), ctx)
                .ok()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_else(|| "OK".to_string());
            let body = read_response_body(obj, ctx);
            let headers = read_response_headers(obj, ctx);
            return Some(CacheEntry {
                status,
                reason,
                headers,
                body,
            });
        }
    }
    // Could be a Promise — drive once with then.
    if let Ok(then_val) = obj.get(js_string!("then"), ctx) {
        if then_val.is_callable() {
            let promise = JsPromise::from_object(obj.clone()).ok()?;
            let realm = ctx.realm().clone();
            let cb = boa_engine::object::FunctionObjectBuilder::new(
                &realm,
                NativeFunction::from_fn_ptr(promise_then_capture),
            )
            .build();
            let _ = promise.then(Some(cb), None, ctx);
            ctx.run_jobs();
            return SW_RESPONSE_SLOT.with(|slot| slot.borrow_mut().take());
        }
    }
    // Plain object with __body — used by our own Response wrapper.
    let body = read_response_body(obj, ctx);
    if !body.is_empty() {
        return Some(CacheEntry {
            status: 200,
            reason: "OK".into(),
            headers: read_response_headers(obj, ctx),
            body,
        });
    }
    None
}

fn read_response_body(obj: &boa_engine::JsObject, ctx: &mut Context) -> Vec<u8> {
    // We carry the response body under `__body` (string) for synth
    // Response objects, or pages may have set `body`.
    for key in ["__body", "body"] {
        if let Ok(v) = obj.get(js_string!(key.to_string()), ctx) {
            if !v.is_undefined() && !v.is_null() {
                if let Ok(s) = v.to_string(ctx) {
                    return s.to_std_string_escaped().into_bytes();
                }
            }
        }
    }
    Vec::new()
}

fn read_response_headers(
    obj: &boa_engine::JsObject,
    ctx: &mut Context,
) -> Vec<(String, String)> {
    let Ok(headers_val) = obj.get(js_string!("headers"), ctx) else {
        return Vec::new();
    };
    let Some(headers_obj) = headers_val.as_object() else {
        return Vec::new();
    };
    // Headers may be a Map-like object exposing `forEach` or a plain
    // dict. We try the plain-dict shape and fall back to nothing.
    let mut out = Vec::new();
    let proto = headers_obj.clone();
    // Attempt: iterate string keys of the headers object.
    if let Ok(keys_fn) = proto.get(js_string!("entries"), ctx) {
        if keys_fn.is_callable() {
            // Not bothering to drive the iterator — fall through.
        }
    }
    // Plain dict iteration over own properties via __get_own_keys_for_tests__?
    // Easier: try a list of well-known headers pages typically set.
    for k in [
        "content-type",
        "content-length",
        "cache-control",
        "etag",
        "last-modified",
        "location",
        "x-frame-options",
    ] {
        if let Ok(v) = proto.get(js_string!(k.to_string()), ctx) {
            if let Ok(s) = v.to_string(ctx) {
                let val = s.to_std_string_escaped();
                if !val.is_empty() && val != "undefined" {
                    out.push((k.to_string(), val));
                }
            }
        }
    }
    out
}

fn promise_then_capture(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    if let Some(entry) = resolve_to_cache_entry(val, ctx) {
        SW_RESPONSE_SLOT.with(|slot| *slot.borrow_mut() = Some(entry));
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
    let match_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(caches_match_any),
    )
    .build();
    let caches = ObjectInitializer::new(ctx)
        .property(js_string!("open"), JsValue::from(open), Attribute::READONLY)
        .property(js_string!("has"), JsValue::from(has), Attribute::READONLY)
        .property(js_string!("delete"), JsValue::from(delete), Attribute::READONLY)
        .property(js_string!("keys"), JsValue::from(keys), Attribute::READONLY)
        .property(js_string!("match"), JsValue::from(match_fn), Attribute::READONLY)
        .build();
    let _ = ctx.register_global_property(
        js_string!("caches"),
        caches,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

fn caches_open(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    ensure_caches_loaded();
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
    ensure_caches_loaded();
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
    ensure_caches_loaded();
    if let Some(v) = args.first() {
        let name = v.to_string(ctx)?.to_std_string_escaped();
        JS_CACHES.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                state.borrow_mut().remove(&name);
            }
        });
        // Wipe the on-disk directory too.
        let dir = cache_dir_for(&name);
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(JsPromise::resolve(JsValue::from(true), ctx).into())
}

fn caches_keys(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    ensure_caches_loaded();
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

/// `caches.match(request)` — looks across every cache for a match.
fn caches_match_any(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    ensure_caches_loaded();
    let Some(req_val) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let url = request_to_url(req_val, ctx);
    let entry = JS_CACHES.with(|slot| -> Option<CacheEntry> {
        let state = slot.borrow();
        let state = state.as_ref()?;
        let s = state.borrow();
        for cache in s.values() {
            if let Some(e) = cache.get(&url) {
                return Some(e.clone());
            }
        }
        None
    });
    let result = match entry {
        Some(e) => JsValue::from(cache_entry_to_response_object(ctx, &e, &url)),
        None => JsValue::undefined(),
    };
    Ok(JsPromise::resolve(result, ctx).into())
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
    b.function(NativeFunction::from_fn_ptr(cache_add), js_string!("add"), 1);
    b.function(NativeFunction::from_fn_ptr(cache_add_all), js_string!("addAll"), 1);
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

fn request_to_url(val: &JsValue, ctx: &mut Context) -> String {
    if val.is_string() {
        return val
            .to_string(ctx)
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
    }
    if let Some(obj) = val.as_object() {
        if let Ok(u) = obj.get(js_string!("url"), ctx) {
            return u
                .to_string(ctx)
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
        }
    }
    String::new()
}

fn cache_put(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = cache_name(this, ctx);
    let (Some(key_val), Some(val_val)) = (args.first(), args.get(1)) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let key = request_to_url(key_val, ctx);
    let entry = resolve_to_cache_entry(val_val, ctx).unwrap_or_default();
    JS_CACHES.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state
                .borrow_mut()
                .entry(name.clone())
                .or_default()
                .insert(key.clone(), entry.clone());
        }
    });
    let _ = write_cache_entry(&name, &key, &entry);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn cache_match(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = cache_name(this, ctx);
    let Some(key_val) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let key = request_to_url(key_val, ctx);
    let entry = JS_CACHES.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|state| state.borrow().get(&name).and_then(|c| c.get(&key).cloned()))
    });
    let result = match entry {
        Some(e) => JsValue::from(cache_entry_to_response_object(ctx, &e, &key)),
        None => JsValue::undefined(),
    };
    Ok(JsPromise::resolve(result, ctx).into())
}

/// `cache.add(request)` — fetch + insert. Pages call this in install
/// to precache assets.
fn cache_add(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = cache_name(this, ctx);
    let Some(req_val) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let url = request_to_url(req_val, ctx);
    let entry = match fetch_for_cache(&url) {
        Some(e) => e,
        None => return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into()),
    };
    JS_CACHES.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state
                .borrow_mut()
                .entry(name.clone())
                .or_default()
                .insert(url.clone(), entry.clone());
        }
    });
    let _ = write_cache_entry(&name, &url, &entry);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn cache_add_all(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = cache_name(this, ctx);
    let Some(arr_val) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let Some(arr_obj) = arr_val.as_object() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let len = arr_obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    for i in 0..len {
        if let Ok(item) = arr_obj.get(i as u64, ctx) {
            let url = request_to_url(&item, ctx);
            if let Some(entry) = fetch_for_cache(&url) {
                JS_CACHES.with(|slot| {
                    if let Some(state) = slot.borrow().as_ref() {
                        state
                            .borrow_mut()
                            .entry(name.clone())
                            .or_default()
                            .insert(url.clone(), entry.clone());
                    }
                });
                let _ = write_cache_entry(&name, &url, &entry);
            }
        }
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn fetch_for_cache(url: &str) -> Option<CacheEntry> {
    let client = super::engine::JS_FETCH_CLIENT.with(|c| c.borrow().clone())?;
    let resolved = super::engine::JS_BASE_URL.with(|u| u.borrow().clone());
    let abs = match resolved {
        Some(b) => b.join(url).ok()?.to_string(),
        None => url::Url::parse(url).ok()?.to_string(),
    };
    let resp = client.get(&abs).ok()?;
    Some(CacheEntry {
        status: resp.status,
        reason: resp.reason.clone(),
        headers: resp.headers.clone(),
        body: resp.body_bytes(),
    })
}

fn cache_delete(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = cache_name(this, ctx);
    if let Some(key_val) = args.first() {
        let key = request_to_url(key_val, ctx);
        JS_CACHES.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                if let Some(cache) = state.borrow_mut().get_mut(&name) {
                    cache.remove(&key);
                }
            }
        });
        let _ = fs::remove_file(cache_entry_path(&name, &key));
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
        // Spec returns Request objects; the toy returns the URL
        // strings, which is what most pages key on.
        let _ = arr.push(JsValue::from(js_string!(k)), ctx);
    }
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

/// Build a JS Response object from a CacheEntry, mirroring the
/// shape `make_response_object` uses elsewhere.
fn cache_entry_to_response_object(
    ctx: &mut Context,
    entry: &CacheEntry,
    url: &str,
) -> boa_engine::JsObject {
    let body_str = String::from_utf8_lossy(&entry.body).into_owned();
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("status"),
        JsValue::from(entry.status as u32),
        Attribute::READONLY,
    );
    b.property(
        js_string!("statusText"),
        JsValue::from(js_string!(entry.reason.clone())),
        Attribute::READONLY,
    );
    b.property(
        js_string!("ok"),
        JsValue::from((200..300).contains(&entry.status)),
        Attribute::READONLY,
    );
    b.property(
        js_string!("url"),
        JsValue::from(js_string!(url.to_string())),
        Attribute::READONLY,
    );
    b.property(
        js_string!("__body"),
        JsValue::from(js_string!(body_str.clone())),
        Attribute::READONLY,
    );
    // text() / json() helpers so the page doesn't have to know about
    // __body. We attach the string directly here too for legacy
    // consumers.
    b.property(
        js_string!("body"),
        JsValue::from(js_string!(body_str)),
        Attribute::READONLY,
    );
    b.build()
}

// ============ disk persistence ============

fn ensure_caches_loaded() {
    let origin = crate::js::opfs::current_origin_host();
    let already = CACHES_LOADED_ORIGIN.with(|s| s.borrow().as_deref() == Some(&origin));
    if already {
        return;
    }
    CACHES_LOADED_ORIGIN.with(|s| *s.borrow_mut() = Some(origin.clone()));
    let dir = caches_origin_root();
    let Ok(rd) = fs::read_dir(&dir) else { return };
    for cache_dir in rd.flatten() {
        if !cache_dir.path().is_dir() {
            continue;
        }
        let cache_name = cache_dir.file_name().to_string_lossy().into_owned();
        // Filenames are hex-encoded URLs.
        let Ok(entries) = fs::read_dir(cache_dir.path()) else {
            continue;
        };
        let mut store: CacheStore = HashMap::new();
        for f in entries.flatten() {
            let path = f.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(url_bytes) = hex_decode(stem) else { continue };
            let Ok(url) = String::from_utf8(url_bytes) else { continue };
            let Ok(buf) = fs::read(&path) else { continue };
            if let Some(entry) = decode_cache_entry(&buf) {
                store.insert(url, entry);
            }
        }
        JS_CACHES.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                state.borrow_mut().insert(cache_name, store);
            }
        });
    }
}

fn caches_origin_root() -> PathBuf {
    let mut p = crate::js::opfs::data_dir_path();
    p.push("daboss-sw-caches");
    p.push(crate::js::opfs::current_origin_host());
    let _ = fs::create_dir_all(&p);
    p
}

fn cache_dir_for(name: &str) -> PathBuf {
    let mut p = caches_origin_root();
    p.push(crate::js::opfs::sanitise_path_component(name));
    let _ = fs::create_dir_all(&p);
    p
}

fn cache_entry_path(name: &str, url: &str) -> PathBuf {
    let mut p = cache_dir_for(name);
    p.push(format!("{}.bin", hex_encode(url.as_bytes())));
    p
}

fn write_cache_entry(name: &str, url: &str, entry: &CacheEntry) -> std::io::Result<()> {
    let bytes = encode_cache_entry(entry);
    let target = cache_entry_path(name, url);
    let tmp = target.with_extension("bin.tmp");
    if let Some(parent) = target.parent() {
        let _ = fs::create_dir_all(parent);
    }
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
    }
    fs::rename(&tmp, &target)
}

fn encode_cache_entry(entry: &CacheEntry) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(64 + entry.reason.len() + entry.body.len() + entry.headers.len() * 32);
    out.extend_from_slice(CACHE_FILE_MAGIC);
    out.push(CACHE_FILE_VERSION);
    out.extend_from_slice(&entry.status.to_le_bytes());
    write_lp(&mut out, entry.reason.as_bytes());
    out.extend_from_slice(&(entry.headers.len() as u32).to_le_bytes());
    for (k, v) in &entry.headers {
        write_lp(&mut out, k.as_bytes());
        write_lp(&mut out, v.as_bytes());
    }
    out.extend_from_slice(&(entry.body.len() as u64).to_le_bytes());
    out.extend_from_slice(&entry.body);
    out
}

fn decode_cache_entry(buf: &[u8]) -> Option<CacheEntry> {
    if buf.len() < 9 || &buf[..4] != CACHE_FILE_MAGIC {
        return None;
    }
    let mut p = 4usize;
    if buf[p] != CACHE_FILE_VERSION {
        return None;
    }
    p += 1;
    let status = read_u16(buf, &mut p)?;
    let reason = String::from_utf8(read_lp(buf, &mut p)?).ok()?;
    let n = read_u32(buf, &mut p)? as usize;
    let mut headers = Vec::with_capacity(n);
    for _ in 0..n {
        let k = String::from_utf8(read_lp(buf, &mut p)?).ok()?;
        let v = String::from_utf8(read_lp(buf, &mut p)?).ok()?;
        headers.push((k, v));
    }
    let body_size = read_u64(buf, &mut p)? as usize;
    if p + body_size > buf.len() {
        return None;
    }
    let body = buf[p..p + body_size].to_vec();
    Some(CacheEntry {
        status,
        reason,
        headers,
        body,
    })
}

fn write_lp(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn read_u16(buf: &[u8], p: &mut usize) -> Option<u16> {
    if *p + 2 > buf.len() {
        return None;
    }
    let v = u16::from_le_bytes([buf[*p], buf[*p + 1]]);
    *p += 2;
    Some(v)
}

fn read_u32(buf: &[u8], p: &mut usize) -> Option<u32> {
    if *p + 4 > buf.len() {
        return None;
    }
    let v = u32::from_le_bytes([buf[*p], buf[*p + 1], buf[*p + 2], buf[*p + 3]]);
    *p += 4;
    Some(v)
}

fn read_u64(buf: &[u8], p: &mut usize) -> Option<u64> {
    if *p + 8 > buf.len() {
        return None;
    }
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&buf[*p..*p + 8]);
    *p += 8;
    Some(u64::from_le_bytes(arr))
}

fn read_lp(buf: &[u8], p: &mut usize) -> Option<Vec<u8>> {
    let n = read_u32(buf, p)? as usize;
    if *p + n > buf.len() {
        return None;
    }
    let out = buf[*p..*p + n].to_vec();
    *p += n;
    Some(out)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(chunk).ok()?;
        out.push(u8::from_str_radix(pair, 16).ok()?);
    }
    Some(out)
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
        .property(js_string!("ready"), JsValue::null(), Attribute::all())
        .build();
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

    let scope = resolved.origin().ascii_serialization();
    install_sw_source(ctx, &source, &resolved.to_string());
    let _ = persist_registration(&scope, &resolved.to_string(), &source);

    let registration = build_registration_object(ctx, &scope);
    activate_controller(ctx);
    Ok(JsPromise::resolve(JsValue::from(registration), ctx).into())
}

fn install_sw_source(ctx: &mut Context, source: &str, script_url: &str) {
    let wrapped = format!(
        "(function() {{ \
            function addEventListener(t, cb) {{ return __sw_register_handler__(t, cb); }} \
            var self = {{ \
                addEventListener: addEventListener, \
                skipWaiting: function() {{}}, \
                clients: {{ claim: function() {{}}, matchAll: function() {{ return Promise.resolve([]); }} }} \
            }}; \
            try {{ {source} }} catch (e) {{ \
                console && console.error && console.error('[sw] threw', e); \
            }} \
        }})();",
        source = source
    );
    if let Err(e) = ctx.eval(Source::from_bytes(wrapped.as_bytes())) {
        eprintln!("[sw] register({script_url}) threw: {e}");
    }
    JS_SW_SOURCES.with(|slot| slot.borrow_mut().push(source.to_string()));
    // Fire install + activate immediately so install handlers can
    // populate caches and activate handlers can claim clients.
    dispatch_lifecycle(ctx, "install");
    dispatch_lifecycle(ctx, "activate");
}

fn dispatch_lifecycle(ctx: &mut Context, ty: &str) {
    let handlers: Vec<JsFunction> = SW_HANDLERS.with(|slot| {
        slot.borrow()
            .get(ty)
            .map(|v| v.iter().cloned().collect())
            .unwrap_or_default()
    });
    if handlers.is_empty() {
        return;
    }
    let event = ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(js_string!(ty.to_string())),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(lifecycle_event_wait_until),
            js_string!("waitUntil"),
            1,
        )
        .build();
    let event_val = JsValue::from(event);
    for h in handlers {
        let _ = h.call(&JsValue::undefined(), &[event_val.clone()], ctx);
    }
    ctx.run_jobs();
}

fn activate_controller(ctx: &mut Context) {
    let controller = ObjectInitializer::new(ctx)
        .property(
            js_string!("state"),
            JsValue::from(js_string!("activated")),
            Attribute::READONLY,
        )
        .property(
            js_string!("scriptURL"),
            JsValue::from(js_string!("")),
            Attribute::READONLY,
        )
        .build();
    let global = ctx.global_object();
    if let Ok(nav) = global.get(js_string!("navigator"), ctx) {
        if let Some(nav_obj) = nav.as_object() {
            if let Ok(sw_val) = nav_obj.get(js_string!("serviceWorker"), ctx) {
                if let Some(sw) = sw_val.as_object() {
                    let _ = sw.set(
                        js_string!("controller"),
                        JsValue::from(controller),
                        false,
                        ctx,
                    );
                }
            }
        }
    }
}

fn build_registration_object(ctx: &mut Context, scope: &str) -> boa_engine::JsObject {
    let active = ObjectInitializer::new(ctx)
        .property(
            js_string!("state"),
            JsValue::from(js_string!("activated")),
            Attribute::READONLY,
        )
        .build();
    ObjectInitializer::new(ctx)
        .property(
            js_string!("scope"),
            JsValue::from(js_string!(scope.to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!("active"),
            JsValue::from(active),
            Attribute::READONLY,
        )
        .property(js_string!("installing"), JsValue::null(), Attribute::READONLY)
        .property(js_string!("waiting"), JsValue::null(), Attribute::READONLY)
        .property(
            js_string!("updateViaCache"),
            JsValue::from(js_string!("imports")),
            Attribute::READONLY,
        )
        .build()
}

fn sw_get_registrations(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::from(JsArray::new(ctx)), ctx).into())
}

// ============ registration persistence ============

fn reg_origin_path() -> PathBuf {
    let mut p = crate::js::opfs::data_dir_path();
    p.push("daboss-sw");
    p.push(crate::js::opfs::current_origin_host());
    let _ = fs::create_dir_all(&p);
    p.push("registrations.bin");
    p
}

fn persist_registration(scope: &str, script_url: &str, source: &str) -> std::io::Result<()> {
    let path = reg_origin_path();
    // Append a new record by reading existing + writing the union.
    let mut existing = read_registrations(&path).unwrap_or_default();
    // Replace any existing entry with the same scope+script_url so we
    // don't accumulate stale duplicates.
    existing.retain(|(s, u, _)| !(s == scope && u == script_url));
    existing.push((scope.to_string(), script_url.to_string(), source.to_string()));
    let mut bytes = Vec::new();
    bytes.extend_from_slice(SW_REG_MAGIC);
    bytes.push(SW_REG_VERSION);
    bytes.extend_from_slice(&(existing.len() as u32).to_le_bytes());
    for (s, u, body) in &existing {
        write_lp(&mut bytes, s.as_bytes());
        write_lp(&mut bytes, u.as_bytes());
        write_lp(&mut bytes, body.as_bytes());
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("bin.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
    }
    fs::rename(&tmp, &path)
}

fn read_registrations(path: &std::path::Path) -> Option<Vec<(String, String, String)>> {
    let buf = fs::read(path).ok()?;
    if buf.len() < 9 || &buf[..4] != SW_REG_MAGIC {
        return None;
    }
    let mut p = 4usize;
    if buf[p] != SW_REG_VERSION {
        return None;
    }
    p += 1;
    let n = read_u32(&buf, &mut p)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let s = String::from_utf8(read_lp(&buf, &mut p)?).ok()?;
        let u = String::from_utf8(read_lp(&buf, &mut p)?).ok()?;
        let body = String::from_utf8(read_lp(&buf, &mut p)?).ok()?;
        out.push((s, u, body));
    }
    Some(out)
}

fn replay_persisted_registrations(ctx: &mut Context) {
    let origin = crate::js::opfs::current_origin_host();
    let already = REGS_LOADED_ORIGIN.with(|s| s.borrow().as_deref() == Some(&origin));
    if already {
        return;
    }
    REGS_LOADED_ORIGIN.with(|s| *s.borrow_mut() = Some(origin.clone()));
    let path = reg_origin_path();
    let Some(regs) = read_registrations(&path) else {
        return;
    };
    if regs.is_empty() {
        return;
    }
    for (_, script_url, source) in regs {
        install_sw_source(ctx, &source, &script_url);
    }
}
