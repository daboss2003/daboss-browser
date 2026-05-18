//! `localStorage` / `sessionStorage` bindings.
//!
//! `localStorage` is disk-backed and origin-scoped: each
//! (key, value) lives at `<data_dir>/daboss-localstorage/<origin>/
//! <key-hex>`. setItem writes atomically (tempfile + rename),
//! removeItem unlinks, clear wipes the directory. The data survives
//! page reloads and process restarts, matching the spec.
//!
//! `sessionStorage` stays in memory — it's a per-tab ephemeral
//! store. We cap total bytes at 5 MiB (the typical real-browser
//! quota) so a runaway page can't OOM the runtime through it.
//!
//! Spec-style direct property access (`localStorage.foo = 1`) still
//! isn't wired (that needs a Proxy); use the method API.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use boa_engine::{
    js_string, object::ObjectInitializer, property::Attribute, Context, JsResult, JsValue,
    NativeFunction,
};

/// Cap on sessionStorage total bytes. Matches the de-facto real
/// browser quota of ~5 MiB per origin.
pub const SESSION_STORAGE_BYTE_CAP: usize = 5 * 1024 * 1024;

pub type StorageArea = Rc<RefCell<HashMap<String, String>>>;

thread_local! {
    /// Legacy slot — kept so engine.rs's install/uninstall plumbing
    /// compiles without churn. The disk-backed localStorage doesn't
    /// consult it; reads/writes go straight to disk.
    pub(crate) static JS_LOCAL_STORAGE: RefCell<Option<StorageArea>> =
        const { RefCell::new(None) };
    /// sessionStorage stays in memory (per-tab ephemeral). Bounded
    /// at SESSION_STORAGE_BYTE_CAP — setItem returns silently
    /// without writing once the cap is hit.
    pub(crate) static JS_SESSION_STORAGE: RefCell<Option<StorageArea>> =
        const { RefCell::new(None) };
}

#[derive(Copy, Clone)]
enum Which {
    Local,
    Session,
}

const WHICH_KEY: &str = "__storage_kind";

/// Install `localStorage` and `sessionStorage` on the global object.
pub fn install(ctx: &mut Context) {
    let local = build_storage(ctx, Which::Local);
    let session = build_storage(ctx, Which::Session);
    let _ = ctx.register_global_property(js_string!("localStorage"), local, Attribute::all());
    let _ = ctx.register_global_property(js_string!("sessionStorage"), session, Attribute::all());
}

fn build_storage(ctx: &mut Context, which: Which) -> boa_engine::JsObject {
    let realm = ctx.realm().clone();
    let length_get = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(length_getter),
    )
    .build();

    let mut init = ObjectInitializer::new(ctx);
    init.property(
        js_string!(WHICH_KEY),
        JsValue::from(which as u32),
        Attribute::READONLY,
    );
    init.accessor(
        js_string!("length"),
        Some(length_get),
        None,
        Attribute::ENUMERABLE,
    );
    init.function(
        NativeFunction::from_fn_ptr(get_item),
        js_string!("getItem"),
        1,
    );
    init.function(
        NativeFunction::from_fn_ptr(set_item),
        js_string!("setItem"),
        2,
    );
    init.function(
        NativeFunction::from_fn_ptr(remove_item),
        js_string!("removeItem"),
        1,
    );
    init.function(NativeFunction::from_fn_ptr(clear), js_string!("clear"), 0);
    init.function(NativeFunction::from_fn_ptr(key), js_string!("key"), 1);
    init.build()
}

fn which_for_this(this: &JsValue, ctx: &mut Context) -> Which {
    let kind = this
        .as_object()
        .and_then(|o| o.get(js_string!(WHICH_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(Which::Local as u32);
    if kind == Which::Local as u32 {
        Which::Local
    } else {
        Which::Session
    }
}

// ============ localStorage (disk-backed) ============

fn local_storage_dir() -> PathBuf {
    let mut p = super::opfs::data_dir_path();
    p.push("daboss-localstorage");
    p.push(super::opfs::current_origin_host());
    let _ = fs::create_dir_all(&p);
    p
}

fn key_to_filename(key: &str) -> String {
    let mut out = String::with_capacity(key.len() * 2);
    for b in key.as_bytes() {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn filename_to_key(name: &str) -> Option<String> {
    if name.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(name.len() / 2);
    for chunk in name.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(chunk).ok()?;
        bytes.push(u8::from_str_radix(pair, 16).ok()?);
    }
    String::from_utf8(bytes).ok()
}

fn local_get(key: &str) -> Option<String> {
    let mut path = local_storage_dir();
    path.push(key_to_filename(key));
    fs::read_to_string(&path).ok()
}

fn local_set(key: &str, value: &str) {
    let mut path = local_storage_dir();
    path.push(key_to_filename(key));
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, value.as_bytes()).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

fn local_remove(key: &str) {
    let mut path = local_storage_dir();
    path.push(key_to_filename(key));
    let _ = fs::remove_file(&path);
}

fn local_clear() {
    let dir = local_storage_dir();
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::create_dir_all(&dir);
}

fn local_keys_sorted() -> Vec<String> {
    let dir = local_storage_dir();
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let file = entry.file_name();
            let Some(name) = file.to_str() else { continue };
            if name.ends_with(".tmp") {
                continue;
            }
            if let Some(k) = filename_to_key(name) {
                out.push(k);
            }
        }
    }
    out.sort();
    out
}

fn local_length() -> usize {
    let dir = local_storage_dir();
    let Ok(rd) = fs::read_dir(&dir) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| !n.ends_with(".tmp"))
                .unwrap_or(false)
        })
        .count()
}

// ============ sessionStorage (in-memory + bounded) ============

fn session_area() -> Option<StorageArea> {
    JS_SESSION_STORAGE.with(|s| s.borrow().as_ref().cloned())
}

fn session_total_bytes(area: &StorageArea) -> usize {
    area.borrow().iter().map(|(k, v)| k.len() + v.len()).sum()
}

// ============ shared JS handlers ============

fn length_getter(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let len = match which_for_this(this, ctx) {
        Which::Local => local_length() as u32,
        Which::Session => session_area().map(|a| a.borrow().len() as u32).unwrap_or(0),
    };
    Ok(JsValue::from(len))
}

fn get_item(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(k) = args.first() else {
        return Ok(JsValue::null());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    let value = match which_for_this(this, ctx) {
        Which::Local => local_get(&key),
        Which::Session => session_area().and_then(|a| a.borrow().get(&key).cloned()),
    };
    Ok(match value {
        Some(v) => JsValue::from(js_string!(v)),
        None => JsValue::null(),
    })
}

fn set_item(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (Some(k), Some(v)) = (args.first(), args.get(1)) else {
        return Ok(JsValue::undefined());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    let value = v.to_string(ctx)?.to_std_string_escaped();
    match which_for_this(this, ctx) {
        Which::Local => local_set(&key, &value),
        Which::Session => {
            if let Some(area) = session_area() {
                let mut a = area.borrow_mut();
                let new_size = key.len() + value.len();
                let old_size = a.get(&key).map(|v| key.len() + v.len()).unwrap_or(0);
                let total = a.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>();
                let projected = total.saturating_sub(old_size) + new_size;
                if projected > SESSION_STORAGE_BYTE_CAP {
                    return Err(boa_engine::JsNativeError::error()
                        .with_message("QuotaExceededError: sessionStorage full")
                        .into());
                }
                a.insert(key, value);
            }
        }
    }
    Ok(JsValue::undefined())
}

fn remove_item(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(k) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let key = k.to_string(ctx)?.to_std_string_escaped();
    match which_for_this(this, ctx) {
        Which::Local => local_remove(&key),
        Which::Session => {
            if let Some(area) = session_area() {
                area.borrow_mut().remove(&key);
            }
        }
    }
    Ok(JsValue::undefined())
}

fn clear(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    match which_for_this(this, ctx) {
        Which::Local => local_clear(),
        Which::Session => {
            if let Some(area) = session_area() {
                area.borrow_mut().clear();
            }
        }
    }
    Ok(JsValue::undefined())
}

fn key(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(idx_val) = args.first() else {
        return Ok(JsValue::null());
    };
    let Ok(idx) = idx_val.to_u32(ctx) else {
        return Ok(JsValue::null());
    };
    let entry = match which_for_this(this, ctx) {
        Which::Local => local_keys_sorted().into_iter().nth(idx as usize),
        Which::Session => session_area().and_then(|a| {
            let area = a.borrow();
            let mut keys: Vec<String> = area.keys().cloned().collect();
            keys.sort();
            keys.into_iter().nth(idx as usize)
        }),
    };
    Ok(match entry {
        Some(k) => JsValue::from(js_string!(k)),
        None => JsValue::null(),
    })
}

#[allow(dead_code)] // Silences a warning from the keep-the-area-type-alive plumbing.
fn _keep_session_total_bytes_alive(area: &StorageArea) -> usize {
    session_total_bytes(area)
}
