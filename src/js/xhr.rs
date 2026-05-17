//! Toy `XMLHttpRequest` implementation. Wraps the same SSRF-guarded
//! [`crate::net::Client`] that `fetch` uses, with two practical
//! shortcuts versus the W3C spec:
//!
//! * **Async sends are pseudo-async.** `send()` blocks until the
//!   response arrives, then immediately fires `readystatechange` (with
//!   `readyState = 4`) and `load`. From JS this looks fine because boa
//!   drains the microtask queue before returning to the caller. Real
//!   XHR runs the network on a thread and posts state changes back as
//!   tasks.
//! * **Sync XHR is also supported** but indistinguishable from async
//!   given the above.
//!
//! Supported surface: `open(method, url, async?)`, `send(body?)`,
//! `setRequestHeader(k, v)` (ignored — we don't yet wire arbitrary
//! request headers through the client), `getResponseHeader(name)`,
//! `getAllResponseHeaders()`, `abort()`, plus the standard properties:
//! `readyState`, `status`, `statusText`, `responseText`, `response`,
//! `responseType`, `responseURL`. Event listener properties
//! (`onreadystatechange`, `onload`, `onerror`, `onabort`) work the same
//! way they do in browsers — assigning a function to them is equivalent
//! to a single `addEventListener` of the same type.

use std::cell::RefCell;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsFunction, FunctionObjectBuilder, ObjectInitializer},
    property::Attribute,
    Context, JsObject, JsResult, JsValue, NativeFunction,
};

use crate::net;

/// readyState constants per the spec.
const STATE_UNSENT: u8 = 0;
const STATE_OPENED: u8 = 1;
#[allow(dead_code)]
const STATE_HEADERS_RECEIVED: u8 = 2;
#[allow(dead_code)]
const STATE_LOADING: u8 = 3;
const STATE_DONE: u8 = 4;

struct XhrState {
    method: String,
    url: String,
    ready_state: u8,
    status: u16,
    status_text: String,
    response_text: String,
    response_headers: Vec<(String, String)>,
    response_url: String,
    aborted: bool,
    on_readystatechange: Option<JsFunction>,
    on_load: Option<JsFunction>,
    on_error: Option<JsFunction>,
    on_abort: Option<JsFunction>,
}

impl XhrState {
    fn new() -> Self {
        Self {
            method: "GET".into(),
            url: String::new(),
            ready_state: STATE_UNSENT,
            status: 0,
            status_text: String::new(),
            response_text: String::new(),
            response_headers: Vec::new(),
            response_url: String::new(),
            aborted: false,
            on_readystatechange: None,
            on_load: None,
            on_error: None,
            on_abort: None,
        }
    }
}

const STATE_KEY: &str = "__xhr_state";

pub fn install(ctx: &mut Context) {
    // Register the XMLHttpRequest constructor.
    ctx.register_global_callable(
        js_string!("XMLHttpRequest"),
        0,
        NativeFunction::from_fn_ptr(xhr_constructor),
    )
    .ok();
}

fn xhr_constructor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let realm = ctx.realm().clone();
    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f)).build()
    };

    let state = Rc::new(RefCell::new(XhrState::new()));
    let state_handle: u64 = Rc::as_ptr(&state) as u64;
    XHR_REGISTRY.with(|reg| {
        reg.borrow_mut().insert(state_handle, state.clone());
    });

    let mut init = ObjectInitializer::new(ctx);
    init.property(
        js_string!(STATE_KEY),
        // f64 can carry up to 2^53 losslessly; pointer addresses fit.
        JsValue::from(state_handle as f64),
        Attribute::READONLY,
    );
    init.accessor(
        js_string!("readyState"),
        Some(getter(xhr_get_ready_state)),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("status"),
        Some(getter(xhr_get_status)),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("statusText"),
        Some(getter(xhr_get_status_text)),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("responseText"),
        Some(getter(xhr_get_response_text)),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("response"),
        Some(getter(xhr_get_response_text)),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("responseURL"),
        Some(getter(xhr_get_response_url)),
        None,
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("onreadystatechange"),
        Some(getter(xhr_get_onrsc)),
        Some(getter(xhr_set_onrsc)),
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("onload"),
        Some(getter(xhr_get_onload)),
        Some(getter(xhr_set_onload)),
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("onerror"),
        Some(getter(xhr_get_onerror)),
        Some(getter(xhr_set_onerror)),
        Attribute::ENUMERABLE,
    );
    init.accessor(
        js_string!("onabort"),
        Some(getter(xhr_get_onabort)),
        Some(getter(xhr_set_onabort)),
        Attribute::ENUMERABLE,
    );
    init.function(
        NativeFunction::from_fn_ptr(xhr_open),
        js_string!("open"),
        3,
    );
    init.function(
        NativeFunction::from_fn_ptr(xhr_send),
        js_string!("send"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(xhr_set_request_header),
        js_string!("setRequestHeader"),
        2,
    );
    init.function(
        NativeFunction::from_fn_ptr(xhr_get_response_header),
        js_string!("getResponseHeader"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(xhr_get_all_response_headers),
        js_string!("getAllResponseHeaders"),
        0,
    );
    init.function(
        NativeFunction::from_fn_ptr(xhr_abort),
        js_string!("abort"),
        0,
    );
    Ok(JsValue::from(init.build()))
}

thread_local! {
    /// Pointer-keyed map of live XHR state objects. Each constructor
    /// puts an entry here; methods look up the state by reading the
    /// `__xhr_state` property off `this`.
    static XHR_REGISTRY: RefCell<std::collections::HashMap<u64, Rc<RefCell<XhrState>>>> =
        RefCell::new(std::collections::HashMap::new());
}

fn state_of(this: &JsValue, ctx: &mut Context) -> Option<Rc<RefCell<XhrState>>> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(STATE_KEY), ctx).ok()?;
    let key = v.to_number(ctx).ok()? as u64;
    XHR_REGISTRY.with(|reg| reg.borrow().get(&key).cloned())
}

fn xhr_open(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let method = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped().to_uppercase()))
        .transpose()?
        .unwrap_or_else(|| "GET".to_string());
    let url = args
        .get(1)
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    {
        let mut s = state.borrow_mut();
        s.method = method;
        s.url = url;
        s.ready_state = STATE_OPENED;
        s.aborted = false;
    }
    fire_readystatechange(&state, ctx);
    Ok(JsValue::undefined())
}

fn xhr_set_request_header(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    // Headers are intentionally dropped for now — wiring per-request
    // headers requires extending net::Client::Request, deferred.
    Ok(JsValue::undefined())
}

fn xhr_send(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };

    let (method, url) = {
        let s = state.borrow();
        (s.method.clone(), s.url.clone())
    };
    let body_bytes = match args.first() {
        Some(v) if !v.is_null() && !v.is_undefined() => {
            v.to_string(ctx)?.to_std_string_escaped().into_bytes()
        }
        _ => Vec::new(),
    };

    let resolved = super::engine::JS_BASE_URL.with(|base_slot| {
        if let Some(base) = base_slot.borrow().as_ref() {
            base.join(&url).ok()
        } else {
            url::Url::parse(&url).ok()
        }
    });
    let Some(target_url) = resolved else {
        finish_with_error(&state, ctx, "invalid-url");
        return Ok(JsValue::undefined());
    };

    let result = super::engine::JS_FETCH_CLIENT.with(|c| -> Option<net::Result<net::Response>> {
        let client = c.borrow().as_ref()?.clone();
        let initiator = super::engine::JS_BASE_URL.with(|u| u.borrow().clone());
        let mut ctx = net::RequestContext::new().with_cors(true);
        if let Some(init) = initiator {
            ctx = ctx.with_initiator(init);
        }
        let url_str = target_url.to_string();
        Some(match method.as_str() {
            "POST" => client.post_with(&url_str, body_bytes, "application/x-www-form-urlencoded", ctx),
            _ => client.get_with(&url_str, ctx),
        })
    });

    match result {
        Some(Ok(resp)) => {
            {
                let mut s = state.borrow_mut();
                s.status = resp.status;
                s.status_text = resp.reason.clone();
                s.response_text = String::from_utf8_lossy(&resp.body).into_owned();
                s.response_headers = resp.headers.clone();
                s.response_url = target_url.to_string();
                s.ready_state = STATE_DONE;
            }
            fire_readystatechange(&state, ctx);
            fire_load(&state, ctx);
        }
        Some(Err(e)) => {
            finish_with_error(&state, ctx, &e.to_string());
        }
        None => {
            finish_with_error(&state, ctx, "no-fetch-client");
        }
    }
    Ok(JsValue::undefined())
}

fn finish_with_error(state: &Rc<RefCell<XhrState>>, ctx: &mut Context, reason: &str) {
    {
        let mut s = state.borrow_mut();
        s.status = 0;
        s.status_text = reason.to_string();
        s.ready_state = STATE_DONE;
    }
    fire_readystatechange(state, ctx);
    fire_error(state, ctx);
}

fn xhr_abort(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    {
        let mut s = state.borrow_mut();
        s.aborted = true;
        s.ready_state = STATE_DONE;
        s.status = 0;
    }
    fire_readystatechange(&state, ctx);
    let cb = state.borrow().on_abort.clone();
    if let Some(cb) = cb {
        let _ = cb.call(&JsValue::undefined(), &[], ctx);
    }
    Ok(JsValue::undefined())
}

fn xhr_get_response_header(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    let s = state.borrow();
    let hit = s
        .response_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&name))
        .map(|(_, v)| v.clone());
    Ok(match hit {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::null(),
    })
}

fn xhr_get_all_response_headers(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_of(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let s = state.borrow();
    let mut out = String::new();
    for (k, v) in &s.response_headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    Ok(JsValue::from(js_string!(out)))
}

fn fire_readystatechange(state: &Rc<RefCell<XhrState>>, ctx: &mut Context) {
    let cb = state.borrow().on_readystatechange.clone();
    if let Some(cb) = cb {
        let _ = cb.call(&JsValue::undefined(), &[], ctx);
    }
}

fn fire_load(state: &Rc<RefCell<XhrState>>, ctx: &mut Context) {
    let cb = state.borrow().on_load.clone();
    if let Some(cb) = cb {
        let _ = cb.call(&JsValue::undefined(), &[], ctx);
    }
}

fn fire_error(state: &Rc<RefCell<XhrState>>, ctx: &mut Context) {
    let cb = state.borrow().on_error.clone();
    if let Some(cb) = cb {
        let _ = cb.call(&JsValue::undefined(), &[], ctx);
    }
}

// ---- accessors ----

fn xhr_get_ready_state(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(state_of(this, ctx)
        .map(|s| JsValue::from(s.borrow().ready_state as u32))
        .unwrap_or(JsValue::from(0_u32)))
}

fn xhr_get_status(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(state_of(this, ctx)
        .map(|s| JsValue::from(s.borrow().status as u32))
        .unwrap_or(JsValue::from(0_u32)))
}

fn xhr_get_status_text(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = state_of(this, ctx)
        .map(|s| s.borrow().status_text.clone())
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn xhr_get_response_text(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = state_of(this, ctx)
        .map(|s| s.borrow().response_text.clone())
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn xhr_get_response_url(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = state_of(this, ctx)
        .map(|s| s.borrow().response_url.clone())
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

fn xhr_get_onrsc(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(state_of(this, ctx)
        .and_then(|s| s.borrow().on_readystatechange.clone().map(JsValue::from))
        .unwrap_or(JsValue::null()))
}
fn xhr_set_onrsc(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(state) = state_of(this, ctx) {
        state.borrow_mut().on_readystatechange = args.first().and_then(extract_fn);
    }
    Ok(JsValue::undefined())
}
fn xhr_get_onload(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(state_of(this, ctx)
        .and_then(|s| s.borrow().on_load.clone().map(JsValue::from))
        .unwrap_or(JsValue::null()))
}
fn xhr_set_onload(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(state) = state_of(this, ctx) {
        state.borrow_mut().on_load = args.first().and_then(extract_fn);
    }
    Ok(JsValue::undefined())
}
fn xhr_get_onerror(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(state_of(this, ctx)
        .and_then(|s| s.borrow().on_error.clone().map(JsValue::from))
        .unwrap_or(JsValue::null()))
}
fn xhr_set_onerror(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(state) = state_of(this, ctx) {
        state.borrow_mut().on_error = args.first().and_then(extract_fn);
    }
    Ok(JsValue::undefined())
}
fn xhr_get_onabort(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(state_of(this, ctx)
        .and_then(|s| s.borrow().on_abort.clone().map(JsValue::from))
        .unwrap_or(JsValue::null()))
}
fn xhr_set_onabort(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(state) = state_of(this, ctx) {
        state.borrow_mut().on_abort = args.first().and_then(extract_fn);
    }
    Ok(JsValue::undefined())
}

fn extract_fn(v: &JsValue) -> Option<JsFunction> {
    let obj = v.as_object()?;
    JsFunction::from_object(obj.clone())
}

/// Drop XHR state from the registry. The engine doesn't run this yet —
/// XHR instances live until the page exits. A real implementation would
/// hook into JS GC finalisers.
#[cfg(test)]
pub(crate) fn clear_registry() {
    XHR_REGISTRY.with(|r| r.borrow_mut().clear());
}

// `JsObject` import isn't used directly, but ObjectInitializer::build
// returns one — referenced via the From impl.
#[allow(dead_code)]
fn _coerce(v: JsObject) -> JsValue {
    v.into()
}
