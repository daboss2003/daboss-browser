//! WebExtensions runtime: enough surface that an MV3 hello-world
//! can load + execute.
//!
//! Today the extension surface (in `web_apis::install_webextensions_stub`)
//! is a feature-detection stub — `chrome.runtime.id` is `null` and
//! `sendMessage` always rejects. This module makes those calls
//! actually work for a loaded extension:
//!
//! * `Extension` — parsed `manifest.json` + content files keyed by
//!   relative path. Stored in a thread-local `ACTIVE_EXTENSION`.
//! * `load(manifest_json)` — installs an extension into the current
//!   JS context. Generates a stable synthetic ID from the name +
//!   version (no real signing or update server).
//! * `chrome.runtime.{id, getManifest, getURL, sendMessage,
//!   onMessage.addListener}` — wired to the loaded extension.
//! * `chrome.storage.local.{get, set, remove, clear}` — disk-backed
//!   under `<data_dir>/daboss-extension/<id>/storage.json`.
//! * `chrome.scripting.executeScript({func})` — eval the function
//!   body in the current page Context.
//!
//! Not implemented yet: separate contexts for background vs. content
//! scripts (we collapse to one), tabs / windows beyond a stub
//! `query()`, declarativeNetRequest, alarms.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use boa_engine::{
    js_string, object::ObjectInitializer, property::Attribute, Context, JsError, JsObject,
    JsResult, JsString, JsValue, NativeFunction, Source,
};

/// One loaded extension. The shell can register multiple by calling
/// [`load`] repeatedly, but at any instant only one is `active` —
/// the chrome.* APIs only ever see that one.
#[derive(Debug, Clone)]
pub struct Extension {
    pub id: String,
    pub manifest_raw: String,
    pub name: String,
    pub version: String,
}

thread_local! {
    /// Currently-loaded extension. `None` outside an extension
    /// context (the chrome.* surface then behaves like the
    /// pre-extension stub).
    static ACTIVE_EXTENSION: RefCell<Option<Extension>> = const { RefCell::new(None) };

    /// `onMessage` listeners registered by extension scripts.
    /// `sendMessage` walks the list and calls each in turn.
    static MESSAGE_LISTENERS: RefCell<Vec<JsObject>> = RefCell::new(Vec::new());

    /// In-memory mirror of `chrome.storage.local` data, populated
    /// on first read and kept in sync with the disk file on writes.
    /// Storing JSON-style strings keeps cross-Context handoff
    /// simple — values become opaque to Rust until the JS layer
    /// re-parses them.
    static STORAGE_CACHE: RefCell<Option<HashMap<String, String>>> =
        const { RefCell::new(None) };
}

/// Load an extension into the current context. Reads `manifest.name`
/// and `manifest.version` straight off the JSON. Returns the
/// extension ID on success.
pub fn load(ctx: &mut Context, manifest_json: &str) -> Option<String> {
    // Use boa's own JSON.parse so we don't duplicate the parser.
    let parsed = ctx
        .eval(Source::from_bytes(
            format!("JSON.parse({})", js_string_literal(manifest_json)).as_bytes(),
        ))
        .ok()?;
    let obj = parsed.as_object()?.clone();
    let name = obj
        .get(js_string!("name"), ctx)
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .unwrap_or_else(|| "unnamed".to_string());
    let version = obj
        .get(js_string!("version"), ctx)
        .ok()
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .unwrap_or_else(|| "0".to_string());
    let id = synthesize_id(&name, &version);
    let ext = Extension {
        id: id.clone(),
        manifest_raw: manifest_json.to_string(),
        name,
        version,
    };
    ACTIVE_EXTENSION.with(|s| *s.borrow_mut() = Some(ext));
    // Drop any prior extension's cached storage so a re-load
    // doesn't show the previous one's keys.
    STORAGE_CACHE.with(|s| s.borrow_mut().take());
    MESSAGE_LISTENERS.with(|s| s.borrow_mut().clear());
    Some(id)
}

pub fn unload() {
    ACTIVE_EXTENSION.with(|s| s.borrow_mut().take());
    STORAGE_CACHE.with(|s| s.borrow_mut().take());
    MESSAGE_LISTENERS.with(|s| s.borrow_mut().clear());
}

pub fn active_id() -> Option<String> {
    ACTIVE_EXTENSION.with(|s| s.borrow().as_ref().map(|e| e.id.clone()))
}

/// Stable-but-toy extension ID. Real Chrome derives a hash of the
/// signing public key; we hash name+version with the default
/// hasher and emit the first 32 hex chars so the id "looks like" an
/// extension id (Chrome's are 32 ASCII chars).
fn synthesize_id(name: &str, version: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    version.hash(&mut h);
    let v = h.finish();
    // Repeat the 16-hex digit hash twice to land on 32 chars and
    // remap a..f → q..v (a..p like Chrome's a-p alphabet) — purely
    // cosmetic.
    let hex = format!("{v:016x}{v:016x}");
    hex.chars()
        .map(|c| match c {
            'a'..='f' => (c as u8 + 16) as char, // 'a'(0x61)+16='q'
            other => other,
        })
        .collect()
}

/// Escape a string so it can be embedded inside a JS string literal
/// inside the host-eval call above. Cheap because manifests are
/// JSON-encoded text — single-line, escape `\\`, `"`, and any
/// control character.
fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

// ============ chrome.runtime ============

pub fn runtime_get_manifest(
    _: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(raw) = ACTIVE_EXTENSION.with(|s| s.borrow().as_ref().map(|e| e.manifest_raw.clone()))
    else {
        return Ok(JsValue::from(ObjectInitializer::new(ctx).build()));
    };
    ctx.eval(Source::from_bytes(
        format!("JSON.parse({})", js_string_literal(&raw)).as_bytes(),
    ))
}

pub fn runtime_get_url(
    _: &JsValue,
    args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let path = args
        .first()
        .and_then(|v| v.as_string().map(|s| s.to_std_string_escaped()))
        .unwrap_or_default();
    let id = active_id().unwrap_or_default();
    let url = format!("chrome-extension://{id}/{}", path.trim_start_matches('/'));
    Ok(JsValue::from(JsString::from(url)))
}

pub fn runtime_send_message(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    // The spec signature is variadic; we honour the simple
    // (message, callback?) shape. Listeners receive (message,
    // sender, sendResponse) — sender is `{ id }`, sendResponse is
    // a no-op for the toy. The promise resolves to the value
    // returned by the LAST listener.
    let msg = args.first().cloned().unwrap_or(JsValue::undefined());
    let id = active_id().unwrap_or_default();
    let sender = ObjectInitializer::new(ctx)
        .property(
            js_string!("id"),
            JsValue::from(JsString::from(id)),
            Attribute::READONLY,
        )
        .build();
    let send_response = boa_engine::object::FunctionObjectBuilder::new(
        &ctx.realm().clone(),
        NativeFunction::from_fn_ptr(|_, _, _| Ok(JsValue::undefined())),
    )
    .build();
    let listeners = MESSAGE_LISTENERS.with(|s| s.borrow().clone());
    let mut last = JsValue::undefined();
    for listener in listeners {
        if let Some(callable) = JsValue::from(listener).as_callable() {
            let result = callable.call(
                &JsValue::undefined(),
                &[
                    msg.clone(),
                    JsValue::from(sender.clone()),
                    JsValue::from(send_response.clone()),
                ],
                ctx,
            );
            if let Ok(v) = result {
                last = v;
            }
        }
    }
    Ok(boa_engine::object::builtins::JsPromise::resolve(last, ctx).into())
}

pub fn runtime_on_message_add_listener(
    _: &JsValue,
    args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    if let Some(obj) = args.first().and_then(|v| v.as_object().cloned()) {
        MESSAGE_LISTENERS.with(|s| s.borrow_mut().push(obj));
    }
    Ok(JsValue::undefined())
}

pub fn runtime_on_message_remove_listener(
    _: &JsValue,
    args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    if let Some(target) = args.first().and_then(|v| v.as_object().cloned()) {
        MESSAGE_LISTENERS.with(|s| {
            let mut v = s.borrow_mut();
            v.retain(|o| !std::ptr::eq(o.as_ref(), target.as_ref()));
        });
    }
    Ok(JsValue::undefined())
}

// ============ chrome.storage.local ============

fn storage_path() -> Option<PathBuf> {
    let id = active_id()?;
    let mut p = super::opfs::data_dir_path();
    p.push("daboss-extension");
    p.push(super::opfs::sanitise_path_component(&id));
    let _ = fs::create_dir_all(&p);
    p.push("storage.json");
    Some(p)
}

fn load_storage() -> HashMap<String, String> {
    // Cached?
    if let Some(cache) = STORAGE_CACHE.with(|s| s.borrow().clone()) {
        return cache;
    }
    let Some(path) = storage_path() else {
        return HashMap::new();
    };
    let raw = fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
    // Hand-parse the trivial JSON object we wrote ourselves
    // (string -> string). Tolerant of garbage by returning empty.
    let map = parse_flat_string_map(&raw).unwrap_or_default();
    STORAGE_CACHE.with(|s| *s.borrow_mut() = Some(map.clone()));
    map
}

fn persist_storage(map: &HashMap<String, String>) {
    let Some(path) = storage_path() else { return };
    let serialised = serialise_flat_string_map(map);
    let _ = fs::write(&path, serialised);
    STORAGE_CACHE.with(|s| *s.borrow_mut() = Some(map.clone()));
}

/// Encode `map` as a JSON object whose values are pre-stringified
/// JSON snippets — chrome.storage.local stores arbitrary JSON
/// values, so we preserve that by treating each value as opaque
/// JSON text.
fn serialise_flat_string_map(map: &HashMap<String, String>) -> String {
    let mut out = String::from("{");
    let mut first = true;
    for (k, v) in map {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&js_string_literal(k));
        out.push(':');
        // `v` is already JSON text — paste it in directly.
        out.push_str(v);
    }
    out.push('}');
    out
}

/// Parse the file we wrote with `serialise_flat_string_map`. We do
/// not need full JSON support — the writer guarantees the keys are
/// valid JSON strings and values are valid JSON text.
fn parse_flat_string_map(s: &str) -> Option<HashMap<String, String>> {
    let bytes = s.as_bytes();
    let mut i = 0;
    skip_ws(bytes, &mut i);
    if i >= bytes.len() || bytes[i] != b'{' {
        return None;
    }
    i += 1;
    let mut out = HashMap::new();
    skip_ws(bytes, &mut i);
    if i < bytes.len() && bytes[i] == b'}' {
        return Some(out);
    }
    loop {
        skip_ws(bytes, &mut i);
        let key = read_json_string(bytes, &mut i)?;
        skip_ws(bytes, &mut i);
        if i >= bytes.len() || bytes[i] != b':' {
            return None;
        }
        i += 1;
        skip_ws(bytes, &mut i);
        let start = i;
        skip_json_value(bytes, &mut i)?;
        let value = std::str::from_utf8(&bytes[start..i]).ok()?.to_string();
        out.insert(key, value);
        skip_ws(bytes, &mut i);
        if i >= bytes.len() {
            return None;
        }
        match bytes[i] {
            b',' => {
                i += 1;
            }
            b'}' => return Some(out),
            _ => return None,
        }
    }
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
        *i += 1;
    }
}

fn read_json_string(b: &[u8], i: &mut usize) -> Option<String> {
    if *i >= b.len() || b[*i] != b'"' {
        return None;
    }
    *i += 1;
    let mut out = String::new();
    while *i < b.len() {
        let c = b[*i];
        *i += 1;
        match c {
            b'"' => return Some(out),
            b'\\' => {
                if *i >= b.len() {
                    return None;
                }
                let esc = b[*i];
                *i += 1;
                match esc {
                    b'"' | b'\\' | b'/' => out.push(esc as char),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    _ => return None,
                }
            }
            _ => out.push(c as char),
        }
    }
    None
}

fn skip_json_value(b: &[u8], i: &mut usize) -> Option<()> {
    skip_ws(b, i);
    let c = *b.get(*i)?;
    match c {
        b'{' | b'[' => {
            let open = c;
            let close = if c == b'{' { b'}' } else { b']' };
            let mut depth = 0i32;
            while *i < b.len() {
                let bb = b[*i];
                if bb == b'"' {
                    let _ = read_json_string(b, i)?;
                    continue;
                }
                if bb == open {
                    depth += 1;
                } else if bb == close {
                    depth -= 1;
                    *i += 1;
                    if depth == 0 {
                        return Some(());
                    }
                    continue;
                }
                *i += 1;
            }
            None
        }
        b'"' => {
            let _ = read_json_string(b, i)?;
            Some(())
        }
        b't' if b.get(*i..*i + 4) == Some(b"true") => {
            *i += 4;
            Some(())
        }
        b'f' if b.get(*i..*i + 5) == Some(b"false") => {
            *i += 5;
            Some(())
        }
        b'n' if b.get(*i..*i + 4) == Some(b"null") => {
            *i += 4;
            Some(())
        }
        _ => {
            // Number — span digits / dot / e / sign.
            while *i < b.len() {
                let c = b[*i];
                if c.is_ascii_digit() || c == b'.' || c == b'-' || c == b'+' || c == b'e' || c == b'E' {
                    *i += 1;
                } else {
                    break;
                }
            }
            Some(())
        }
    }
}

pub fn storage_local_get(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let map = load_storage();
    let result_obj = ObjectInitializer::new(ctx).build();
    let want_keys: Option<Vec<String>> = match args.first() {
        None | Some(JsValue::Null) | Some(JsValue::Undefined) => None,
        Some(JsValue::String(s)) => Some(vec![s.to_std_string_escaped()]),
        Some(other) => other.as_object().and_then(|o| {
            if o.is_array() {
                let len = o
                    .get(js_string!("length"), ctx)
                    .ok()?
                    .to_u32(ctx)
                    .ok()? as i64;
                let mut keys = Vec::new();
                for idx in 0..len {
                    if let Ok(v) = o.get(idx, ctx) {
                        if let Some(s) = v.as_string() {
                            keys.push(s.to_std_string_escaped());
                        }
                    }
                }
                Some(keys)
            } else {
                None
            }
        }),
    };
    let keys: Vec<String> = match want_keys {
        Some(k) => k,
        None => map.keys().cloned().collect(),
    };
    for k in &keys {
        if let Some(v) = map.get(k) {
            // value is JSON text — re-parse via boa.
            if let Ok(jv) = ctx.eval(Source::from_bytes(
                format!("JSON.parse({})", js_string_literal(v)).as_bytes(),
            )) {
                let _ = result_obj.set(JsString::from(k.as_str()), jv, false, ctx);
            }
        }
    }
    Ok(boa_engine::object::builtins::JsPromise::resolve(
        JsValue::from(result_obj),
        ctx,
    )
    .into())
}

pub fn storage_local_set(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let mut map = load_storage();
    if let Some(obj) = args.first().and_then(|v| v.as_object().cloned()) {
        // Iterate object's own enumerable keys via Object.keys().
        let keys_js = ctx
            .eval(Source::from_bytes(b"Object.keys"))
            .ok()
            .and_then(|f| f.as_callable().map(|c| c.clone()));
        if let Some(get_keys) = keys_js {
            if let Ok(arr) = get_keys.call(&JsValue::undefined(), &[JsValue::from(obj.clone())], ctx)
            {
                if let Some(arr_obj) = arr.as_object() {
                    let len = arr_obj
                        .get(js_string!("length"), ctx)
                        .ok()
                        .and_then(|v| v.to_u32(ctx).ok())
                        .unwrap_or(0) as i64;
                    for idx in 0..len {
                        let Ok(key_val) = arr_obj.get(idx, ctx) else { continue };
                        let key = match key_val.as_string() {
                            Some(s) => s.to_std_string_escaped(),
                            None => continue,
                        };
                        let Ok(val) = obj.get(JsString::from(key.as_str()), ctx) else { continue };
                        let stringified = ctx
                            .eval(Source::from_bytes(
                                format!("JSON.stringify(_)").as_bytes(),
                            ))
                            .ok();
                        // The above isn't quite right — we need to
                        // stringify `val` specifically. boa doesn't
                        // expose a convenient JSON.stringify helper
                        // off the global, so we re-eval against a
                        // temporary global:
                        let _ = stringified;
                        let stringified =
                            stringify_via_eval(ctx, &val).unwrap_or_else(|| "null".to_string());
                        map.insert(key, stringified);
                    }
                }
            }
        }
    }
    persist_storage(&map);
    Ok(boa_engine::object::builtins::JsPromise::resolve(JsValue::undefined(), ctx).into())
}

pub fn storage_local_remove(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let mut map = load_storage();
    if let Some(s) = args.first().and_then(|v| v.as_string()) {
        map.remove(&s.to_std_string_escaped());
    } else if let Some(obj) = args.first().and_then(|v| v.as_object().cloned()) {
        if obj.is_array() {
            let len = obj
                .get(js_string!("length"), ctx)
                .ok()
                .and_then(|v| v.to_u32(ctx).ok())
                .unwrap_or(0) as i64;
            for idx in 0..len {
                if let Ok(JsValue::String(s)) = obj.get(idx, ctx) {
                    map.remove(&s.to_std_string_escaped());
                }
            }
        }
    }
    persist_storage(&map);
    Ok(boa_engine::object::builtins::JsPromise::resolve(JsValue::undefined(), ctx).into())
}

pub fn storage_local_clear(
    _: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let map: HashMap<String, String> = HashMap::new();
    persist_storage(&map);
    Ok(boa_engine::object::builtins::JsPromise::resolve(JsValue::undefined(), ctx).into())
}

/// Stringify a `JsValue` via a temporary global. Returns the JSON
/// text or `None` if the value isn't JSON-representable (functions,
/// circular references, etc.). Used so `chrome.storage.local.set`
/// can persist arbitrary JS values on disk without needing a
/// hand-rolled serialiser.
fn stringify_via_eval(ctx: &mut Context, value: &JsValue) -> Option<String> {
    let temp_name = js_string!("__daboss_storage_tmp__");
    ctx.register_global_property(
        temp_name.clone(),
        value.clone(),
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    )
    .ok()?;
    let result = ctx.eval(Source::from_bytes(b"JSON.stringify(__daboss_storage_tmp__)"));
    // Best-effort clean-up: overwrite with `undefined` so future
    // sets don't see this stale entry.
    let _ = ctx.register_global_property(
        temp_name,
        JsValue::undefined(),
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
    match result {
        Ok(JsValue::String(s)) => Some(s.to_std_string_escaped()),
        Ok(JsValue::Undefined) => None,
        _ => None,
    }
}

// ============ chrome.scripting ============

pub fn scripting_execute_script(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    // Spec signature: `executeScript({target, func, args, files})`.
    // We honour the `func` form (eval its body string against the
    // current context). Pages that pass `files` to a content-script
    // injection won't work — we don't fetch + inject yet.
    let Some(opts) = args.first().and_then(|v| v.as_object().cloned()) else {
        return Ok(JsPromise_resolve_empty(ctx));
    };
    let Some(func) = opts.get(js_string!("func"), ctx).ok().and_then(|v| v.as_callable().cloned())
    else {
        return Ok(JsPromise_resolve_empty(ctx));
    };
    let result =
        func.call(&JsValue::undefined(), &[], ctx).unwrap_or(JsValue::undefined());
    let frame = ObjectInitializer::new(ctx)
        .property(js_string!("result"), result, Attribute::READONLY)
        .build();
    let arr = boa_engine::object::builtins::JsArray::from_iter(vec![JsValue::from(frame)], ctx);
    Ok(boa_engine::object::builtins::JsPromise::resolve(JsValue::from(arr), ctx).into())
}

#[allow(non_snake_case)]
fn JsPromise_resolve_empty(ctx: &mut Context) -> JsValue {
    boa_engine::object::builtins::JsPromise::resolve(JsValue::undefined(), ctx).into()
}

// ============ chrome.tabs (stub) ============

pub fn tabs_query(
    _: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    // Return an empty array — extension code that walks tabs
    // simply sees "no other tabs", which is correct for the toy
    // (no cross-tab queries land here).
    let arr = boa_engine::object::builtins::JsArray::new(ctx);
    Ok(boa_engine::object::builtins::JsPromise::resolve(JsValue::from(arr), ctx).into())
}

// ============ install ============

/// Replace the `chrome` global's previous stub with a fully wired
/// surface. Safe to call after the stub has been installed; the
/// last write wins.
pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let mk = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
        boa_engine::object::FunctionObjectBuilder::new(
            &realm,
            NativeFunction::from_fn_ptr(f),
        )
        .build()
    };

    // chrome.runtime
    let on_message = ObjectInitializer::new(ctx)
        .property(
            js_string!("addListener"),
            JsValue::from(mk(runtime_on_message_add_listener)),
            Attribute::READONLY,
        )
        .property(
            js_string!("removeListener"),
            JsValue::from(mk(runtime_on_message_remove_listener)),
            Attribute::READONLY,
        )
        .build();
    let runtime = ObjectInitializer::new(ctx)
        .property(
            js_string!("id"),
            active_id()
                .map(|s| JsValue::from(JsString::from(s)))
                .unwrap_or(JsValue::null()),
            Attribute::READONLY,
        )
        .property(
            js_string!("getManifest"),
            JsValue::from(mk(runtime_get_manifest)),
            Attribute::READONLY,
        )
        .property(
            js_string!("getURL"),
            JsValue::from(mk(runtime_get_url)),
            Attribute::READONLY,
        )
        .property(
            js_string!("sendMessage"),
            JsValue::from(mk(runtime_send_message)),
            Attribute::READONLY,
        )
        .property(
            js_string!("onMessage"),
            JsValue::from(on_message),
            Attribute::READONLY,
        )
        .property(
            js_string!("lastError"),
            JsValue::null(),
            Attribute::WRITABLE,
        )
        .build();

    // chrome.storage.local
    let storage_local = ObjectInitializer::new(ctx)
        .property(
            js_string!("get"),
            JsValue::from(mk(storage_local_get)),
            Attribute::READONLY,
        )
        .property(
            js_string!("set"),
            JsValue::from(mk(storage_local_set)),
            Attribute::READONLY,
        )
        .property(
            js_string!("remove"),
            JsValue::from(mk(storage_local_remove)),
            Attribute::READONLY,
        )
        .property(
            js_string!("clear"),
            JsValue::from(mk(storage_local_clear)),
            Attribute::READONLY,
        )
        .build();
    let storage = ObjectInitializer::new(ctx)
        .property(
            js_string!("local"),
            JsValue::from(storage_local),
            Attribute::READONLY,
        )
        .build();

    // chrome.scripting
    let scripting = ObjectInitializer::new(ctx)
        .property(
            js_string!("executeScript"),
            JsValue::from(mk(scripting_execute_script)),
            Attribute::READONLY,
        )
        .build();

    // chrome.tabs (stub)
    let tabs = ObjectInitializer::new(ctx)
        .property(
            js_string!("query"),
            JsValue::from(mk(tabs_query)),
            Attribute::READONLY,
        )
        .build();

    let chrome_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("runtime"),
            JsValue::from(runtime.clone()),
            Attribute::READONLY,
        )
        .property(
            js_string!("storage"),
            JsValue::from(storage.clone()),
            Attribute::READONLY,
        )
        .property(
            js_string!("scripting"),
            JsValue::from(scripting.clone()),
            Attribute::READONLY,
        )
        .property(
            js_string!("tabs"),
            JsValue::from(tabs.clone()),
            Attribute::READONLY,
        )
        .build();
    let browser_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("runtime"),
            JsValue::from(runtime),
            Attribute::READONLY,
        )
        .property(
            js_string!("storage"),
            JsValue::from(storage),
            Attribute::READONLY,
        )
        .property(
            js_string!("scripting"),
            JsValue::from(scripting),
            Attribute::READONLY,
        )
        .property(
            js_string!("tabs"),
            JsValue::from(tabs),
            Attribute::READONLY,
        )
        .build();
    let _ = ctx.register_global_property(
        js_string!("chrome"),
        chrome_obj,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
    let _ = ctx.register_global_property(
        js_string!("browser"),
        browser_obj,
        Attribute::WRITABLE | Attribute::CONFIGURABLE,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_ctx() -> Context {
        let mut ctx = Context::default();
        install(&mut ctx);
        ctx
    }

    #[test]
    fn synthesised_id_is_stable_and_32_chars() {
        let a = synthesize_id("hello", "1.0");
        let b = synthesize_id("hello", "1.0");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn load_then_get_manifest_returns_parsed_object() {
        unload();
        let mut ctx = fresh_ctx();
        let manifest = r#"{
            "name": "Hello",
            "version": "1.0.0",
            "manifest_version": 3,
            "description": "A test"
        }"#;
        let id = load(&mut ctx, manifest).expect("load");
        assert_eq!(id.len(), 32);
        // Re-install so chrome.runtime.id picks up the loaded ext.
        install(&mut ctx);
        let observed = ctx
            .eval(Source::from_bytes(b"chrome.runtime.id"))
            .unwrap()
            .as_string()
            .unwrap()
            .to_std_string_escaped();
        assert_eq!(observed, id);
        let name = ctx
            .eval(Source::from_bytes(b"chrome.runtime.getManifest().name"))
            .unwrap()
            .as_string()
            .unwrap()
            .to_std_string_escaped();
        assert_eq!(name, "Hello");
        unload();
    }

    #[test]
    fn runtime_get_url_uses_active_id() {
        unload();
        let mut ctx = fresh_ctx();
        load(&mut ctx, r#"{"name":"u","version":"1","manifest_version":3}"#).unwrap();
        install(&mut ctx);
        let url = ctx
            .eval(Source::from_bytes(b"chrome.runtime.getURL('popup.html')"))
            .unwrap()
            .as_string()
            .unwrap()
            .to_std_string_escaped();
        assert!(url.starts_with("chrome-extension://"));
        assert!(url.ends_with("/popup.html"));
        unload();
    }

    #[test]
    fn send_message_dispatches_to_listeners() {
        unload();
        let mut ctx = fresh_ctx();
        load(&mut ctx, r#"{"name":"m","version":"1","manifest_version":3}"#).unwrap();
        install(&mut ctx);
        ctx.eval(Source::from_bytes(
            b"globalThis.__hit = null;
              chrome.runtime.onMessage.addListener(function(msg) {
                globalThis.__hit = msg;
                return 'ack';
              });
              chrome.runtime.sendMessage({ kind: 'ping' });",
        ))
        .unwrap();
        let hit_kind = ctx
            .eval(Source::from_bytes(b"globalThis.__hit && globalThis.__hit.kind"))
            .unwrap()
            .as_string()
            .unwrap()
            .to_std_string_escaped();
        assert_eq!(hit_kind, "ping");
        unload();
    }

    #[test]
    fn storage_local_round_trip_persists_via_disk() {
        unload();
        STORAGE_CACHE.with(|s| s.borrow_mut().take());
        let mut ctx = fresh_ctx();
        load(
            &mut ctx,
            r#"{"name":"s","version":"1.0","manifest_version":3}"#,
        )
        .unwrap();
        install(&mut ctx);
        // Set, then drop the in-memory cache, then read back so we
        // exercise the disk path.
        ctx.eval(Source::from_bytes(
            b"chrome.storage.local.set({ k: 'v', n: 42, o: { nested: true } });",
        ))
        .unwrap();
        STORAGE_CACHE.with(|s| s.borrow_mut().take());
        let val = ctx
            .eval(Source::from_bytes(
                b"chrome.storage.local.get('k').then(o => o.k)",
            ))
            .unwrap();
        // .then returns a promise — the value is undefined until
        // the job queue runs. Pump it.
        ctx.run_jobs();
        let _ = val;
        // Easier: use sync-shaped get via direct evaluation that
        // reads after the resolved promise's microtask completes.
        let got = ctx
            .eval(Source::from_bytes(
                b"
                let out = '';
                chrome.storage.local.get(['k', 'n']).then(o => {
                    out = JSON.stringify(o);
                });
                ",
            ))
            .unwrap();
        let _ = got;
        ctx.run_jobs();
        let observed = ctx
            .eval(Source::from_bytes(b"out"))
            .unwrap()
            .as_string()
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        assert!(observed.contains("\"k\":\"v\""), "got {observed}");
        assert!(observed.contains("\"n\":42"), "got {observed}");
        // Clean up the disk file so tests don't accumulate state.
        if let Some(p) = storage_path() {
            let _ = std::fs::remove_file(&p);
        }
        unload();
    }
}
