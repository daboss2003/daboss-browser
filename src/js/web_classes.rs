//! Standard web-platform classes that ride on simple Rust state:
//! `URL`, `URLSearchParams`, `FormData`, `TextEncoder`, `TextDecoder`.
//!
//! Implementation strategy mirrors XHR: each instance carries a
//! pointer-keyed `__state` property identifying a `Rc<RefCell<…>>` in
//! a per-class thread-local registry. Methods look up the state by
//! that key.
//!
//! Scope notes:
//!  * `URL` is backed by the `url` crate so parsing/serialisation
//!    matches what the network layer already does.
//!  * `TextEncoder` returns a plain JS array of byte numbers rather
//!    than a real `Uint8Array` — boa exposes typed arrays but the
//!    construction API is awkward, and array-of-numbers is enough for
//!    the common "encode and feed to fetch body" flow.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, FunctionObjectBuilder, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("URL"),
        2,
        NativeFunction::from_fn_ptr(url_constructor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("URLSearchParams"),
        1,
        NativeFunction::from_fn_ptr(usp_constructor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("FormData"),
        0,
        NativeFunction::from_fn_ptr(form_data_constructor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("TextEncoder"),
        0,
        NativeFunction::from_fn_ptr(text_encoder_constructor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("TextDecoder"),
        0,
        NativeFunction::from_fn_ptr(text_decoder_constructor),
    )
    .ok();
}

// ============================ URL ============================

const URL_STATE_KEY: &str = "__url_state";

thread_local! {
    static URL_REGISTRY: RefCell<HashMap<u64, Rc<RefCell<url::Url>>>> =
        RefCell::new(HashMap::new());
}

fn url_constructor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let href = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let base = args
        .get(1)
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?;

    let parsed = match base {
        Some(b) => url::Url::parse(&b).and_then(|base| base.join(&href)),
        None => url::Url::parse(&href),
    };
    let url = match parsed {
        Ok(u) => u,
        Err(e) => {
            return Err(boa_engine::JsNativeError::typ()
                .with_message(format!("Invalid URL: {e}"))
                .into());
        }
    };

    let state = Rc::new(RefCell::new(url));
    let key = Rc::as_ptr(&state) as u64;
    URL_REGISTRY.with(|reg| reg.borrow_mut().insert(key, state.clone()));

    let realm = ctx.realm().clone();
    let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f)).build()
    };
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(URL_STATE_KEY),
        JsValue::from(key as f64),
        Attribute::READONLY,
    );
    for (name, fr) in [
        ("href", url_get_href as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>),
        ("origin", url_get_origin),
        ("protocol", url_get_protocol),
        ("host", url_get_host),
        ("hostname", url_get_hostname),
        ("port", url_get_port),
        ("pathname", url_get_pathname),
        ("search", url_get_search),
        ("hash", url_get_hash),
    ] {
        b.accessor(
            js_string!(name),
            Some(getter(fr)),
            None,
            Attribute::ENUMERABLE,
        );
    }
    b.function(
        NativeFunction::from_fn_ptr(url_to_string),
        js_string!("toString"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn url_of(this: &JsValue, ctx: &mut Context) -> Option<Rc<RefCell<url::Url>>> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(URL_STATE_KEY), ctx).ok()?;
    let key = v.to_number(ctx).ok()? as u64;
    URL_REGISTRY.with(|r| r.borrow().get(&key).cloned())
}

fn url_get_href(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = url_of(this, ctx).map(|u| u.borrow().to_string()).unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}
fn url_to_string(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    url_get_href(this, &[], ctx)
}
fn url_get_origin(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = url_of(this, ctx)
        .map(|u| u.borrow().origin().ascii_serialization())
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}
fn url_get_protocol(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = url_of(this, ctx)
        .map(|u| format!("{}:", u.borrow().scheme()))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}
fn url_get_host(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = url_of(this, ctx)
        .and_then(|u| {
            let u = u.borrow();
            u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}
fn url_get_hostname(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = url_of(this, ctx)
        .and_then(|u| u.borrow().host_str().map(str::to_string))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}
fn url_get_port(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = url_of(this, ctx)
        .and_then(|u| u.borrow().port().map(|p| p.to_string()))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}
fn url_get_pathname(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = url_of(this, ctx).map(|u| u.borrow().path().to_string()).unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}
fn url_get_search(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = url_of(this, ctx)
        .and_then(|u| u.borrow().query().map(|q| format!("?{q}")))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}
fn url_get_hash(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = url_of(this, ctx)
        .and_then(|u| u.borrow().fragment().map(|f| format!("#{f}")))
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(s)))
}

// =================== URLSearchParams ===================

const USP_STATE_KEY: &str = "__usp_state";

thread_local! {
    static USP_REGISTRY: RefCell<HashMap<u64, Rc<RefCell<Vec<(String, String)>>>>> =
        RefCell::new(HashMap::new());
}

fn usp_constructor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let pairs: Vec<(String, String)> = match args.first() {
        Some(v) if v.is_string() => {
            let s = v.to_string(ctx)?.to_std_string_escaped();
            parse_query_pairs(s.strip_prefix('?').unwrap_or(&s))
        }
        _ => Vec::new(),
    };
    let state = Rc::new(RefCell::new(pairs));
    let key = Rc::as_ptr(&state) as u64;
    USP_REGISTRY.with(|r| r.borrow_mut().insert(key, state.clone()));

    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(USP_STATE_KEY),
        JsValue::from(key as f64),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(usp_get),
        js_string!("get"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(usp_get_all),
        js_string!("getAll"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(usp_has),
        js_string!("has"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(usp_set),
        js_string!("set"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(usp_append),
        js_string!("append"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(usp_delete),
        js_string!("delete"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(usp_to_string),
        js_string!("toString"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn usp_of(this: &JsValue, ctx: &mut Context) -> Option<Rc<RefCell<Vec<(String, String)>>>> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(USP_STATE_KEY), ctx).ok()?;
    let key = v.to_number(ctx).ok()? as u64;
    USP_REGISTRY.with(|r| r.borrow().get(&key).cloned())
}

fn parse_query_pairs(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut it = p.splitn(2, '=');
            let k = decode_form(it.next().unwrap_or(""));
            let v = decode_form(it.next().unwrap_or(""));
            (k, v)
        })
        .collect()
}

fn decode_form(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(' ');
            i += 1;
        } else if b == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8 as char);
                    i += 3;
                }
                _ => {
                    out.push(b as char);
                    i += 1;
                }
            }
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

fn encode_form(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn usp_get(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = usp_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    let val = state.borrow().iter().find(|(k, _)| k == &name).map(|(_, v)| v.clone());
    Ok(match val {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::null(),
    })
}

fn usp_get_all(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let arr = JsArray::new(ctx);
    let Some(state) = usp_of(this, ctx) else {
        return Ok(arr.into());
    };
    let Some(name_val) = args.first() else {
        return Ok(arr.into());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    for (k, v) in state.borrow().iter() {
        if k == &name {
            arr.push(JsValue::from(js_string!(v.clone())), ctx)?;
        }
    }
    Ok(arr.into())
}

fn usp_has(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = usp_of(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::from(false));
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    let has = state.borrow().iter().any(|(k, _)| k == &name);
    Ok(JsValue::from(has))
}

fn usp_set(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = usp_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let (Some(k), Some(v)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::undefined());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    let value = v.to_string(ctx)?.to_std_string_escaped();
    let mut s = state.borrow_mut();
    s.retain(|(k, _)| k != &key);
    s.push((key, value));
    Ok(JsValue::undefined())
}

fn usp_append(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = usp_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let (Some(k), Some(v)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::undefined());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    let value = v.to_string(ctx)?.to_std_string_escaped();
    state.borrow_mut().push((key, value));
    Ok(JsValue::undefined())
}

fn usp_delete(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = usp_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(name_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let name = name_val.to_string(ctx)?.to_std_string_escaped();
    state.borrow_mut().retain(|(k, _)| k != &name);
    Ok(JsValue::undefined())
}

fn usp_to_string(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = usp_of(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let s = state
        .borrow()
        .iter()
        .map(|(k, v)| format!("{}={}", encode_form(k), encode_form(v)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(JsValue::from(js_string!(s)))
}

// ===================== FormData =====================
// Same shape as URLSearchParams; semantically a separate type but
// behaviourally identical for our toy.

const FD_STATE_KEY: &str = "__fd_state";

thread_local! {
    static FD_REGISTRY: RefCell<HashMap<u64, Rc<RefCell<Vec<(String, String)>>>>> =
        RefCell::new(HashMap::new());
}

fn form_data_constructor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let state: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let key = Rc::as_ptr(&state) as u64;
    FD_REGISTRY.with(|r| r.borrow_mut().insert(key, state.clone()));
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(FD_STATE_KEY),
        JsValue::from(key as f64),
        Attribute::READONLY,
    );
    b.function(NativeFunction::from_fn_ptr(fd_append), js_string!("append"), 2);
    b.function(NativeFunction::from_fn_ptr(fd_set), js_string!("set"), 2);
    b.function(NativeFunction::from_fn_ptr(fd_get), js_string!("get"), 1);
    b.function(NativeFunction::from_fn_ptr(fd_get_all), js_string!("getAll"), 1);
    b.function(NativeFunction::from_fn_ptr(fd_has), js_string!("has"), 1);
    b.function(NativeFunction::from_fn_ptr(fd_delete), js_string!("delete"), 1);
    Ok(JsValue::from(b.build()))
}

fn fd_of(this: &JsValue, ctx: &mut Context) -> Option<Rc<RefCell<Vec<(String, String)>>>> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(FD_STATE_KEY), ctx).ok()?;
    let key = v.to_number(ctx).ok()? as u64;
    FD_REGISTRY.with(|r| r.borrow().get(&key).cloned())
}

fn fd_append(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = fd_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let (Some(k), Some(v)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::undefined());
    };
    state
        .borrow_mut()
        .push((k.to_string(ctx)?.to_std_string_escaped(), v.to_string(ctx)?.to_std_string_escaped()));
    Ok(JsValue::undefined())
}

fn fd_set(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = fd_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let (Some(k), Some(v)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::undefined());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    let value = v.to_string(ctx)?.to_std_string_escaped();
    let mut s = state.borrow_mut();
    s.retain(|(k, _)| k != &key);
    s.push((key, value));
    Ok(JsValue::undefined())
}

fn fd_get(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = fd_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(k) = args.first() else {
        return Ok(JsValue::null());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    let val = state.borrow().iter().find(|(k, _)| k == &key).map(|(_, v)| v.clone());
    Ok(match val {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::null(),
    })
}

fn fd_get_all(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let arr = JsArray::new(ctx);
    let Some(state) = fd_of(this, ctx) else {
        return Ok(arr.into());
    };
    let Some(k) = args.first() else {
        return Ok(arr.into());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    for (kk, vv) in state.borrow().iter() {
        if kk == &key {
            arr.push(JsValue::from(js_string!(vv.clone())), ctx)?;
        }
    }
    Ok(arr.into())
}

fn fd_has(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = fd_of(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let Some(k) = args.first() else {
        return Ok(JsValue::from(false));
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    let has = state.borrow().iter().any(|(kk, _)| kk == &key);
    Ok(JsValue::from(has))
}

fn fd_delete(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = fd_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(k) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    state.borrow_mut().retain(|(kk, _)| kk != &key);
    Ok(JsValue::undefined())
}

// =================== TextEncoder / TextDecoder ===================

fn text_encoder_constructor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("encoding"),
        JsValue::from(js_string!("utf-8")),
        Attribute::READONLY,
    );
    b.function(NativeFunction::from_fn_ptr(text_encoder_encode), js_string!("encode"), 1);
    Ok(JsValue::from(b.build()))
}

fn text_encoder_encode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let s = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for b in s.as_bytes() {
        arr.push(JsValue::from(*b as u32), ctx)?;
    }
    Ok(arr.into())
}

fn text_decoder_constructor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("encoding"),
        JsValue::from(js_string!("utf-8")),
        Attribute::READONLY,
    );
    b.function(NativeFunction::from_fn_ptr(text_decoder_decode), js_string!("decode"), 1);
    Ok(JsValue::from(b.build()))
}

fn text_decoder_decode(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsArray;
    let bytes: Vec<u8> = match args.first().and_then(|v| v.as_object().cloned()) {
        Some(obj) => {
            // Best-effort: accept arrays of numbers (TextEncoder output)
            // and typed arrays of u8. If it has a `.length`, walk it.
            let arr = JsArray::from_object(obj);
            match arr {
                Ok(arr) => {
                    let len = arr.length(ctx)? as u32;
                    let mut out = Vec::with_capacity(len as usize);
                    for i in 0..len {
                        let n = arr.get(i, ctx)?.to_u32(ctx).unwrap_or(0);
                        out.push(n as u8);
                    }
                    out
                }
                Err(_) => Vec::new(),
            }
        }
        None => Vec::new(),
    };
    let s = String::from_utf8_lossy(&bytes).into_owned();
    Ok(JsValue::from(js_string!(s)))
}
