//! `Blob`, `File`, `FileReader`, and the `URL.createObjectURL` /
//! `URL.revokeObjectURL` extensions.
//!
//! Blobs are stored byte-for-byte in a per-engine registry indexed by
//! an integer id. The JS-visible handle just carries `__blob_id`,
//! `size`, and `type`; reads (`.text()`, `.arrayBuffer()`, FileReader)
//! consult the registry. URLs from `createObjectURL` look like
//! `blob:<uuid>` and resolve back to the same registry.
//!
//! `File` is a `Blob` with `name` + `lastModified` properties.
//!
//! `FileReader` resolves results synchronously (we already have the
//! bytes), but still fires `onloadstart` / `onload` / `onloadend`
//! events to satisfy spec listeners. `readyState` walks 0 → 1 → 2.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{
        builtins::{JsArrayBuffer, JsFunction, JsPromise, JsUint8Array},
        ObjectInitializer,
    },
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

const BLOB_ID_KEY: &str = "__blob_id";
const READER_ID_KEY: &str = "__reader_id";

#[derive(Clone)]
pub struct BlobEntry {
    pub bytes: Vec<u8>,
    pub mime: String,
}

pub type BlobRegistry = Rc<RefCell<HashMap<u32, BlobEntry>>>;
pub type ObjectUrlMap = Rc<RefCell<HashMap<String, u32>>>;

thread_local! {
    pub(crate) static JS_BLOBS: RefCell<Option<BlobRegistry>> =
        const { RefCell::new(None) };
    pub(crate) static JS_OBJECT_URLS: RefCell<Option<ObjectUrlMap>> =
        const { RefCell::new(None) };
    pub(crate) static BLOB_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_blob_id() -> u32 {
    BLOB_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    JS_BLOBS.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Rc::new(RefCell::new(HashMap::new())));
        }
    });
    JS_OBJECT_URLS.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Rc::new(RefCell::new(HashMap::new())));
        }
    });
    ctx.register_global_callable(
        js_string!("Blob"),
        2,
        NativeFunction::from_fn_ptr(blob_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("File"),
        3,
        NativeFunction::from_fn_ptr(file_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("FileReader"),
        0,
        NativeFunction::from_fn_ptr(file_reader_ctor),
    )
    .ok();
    install_url_globals(ctx);
}

fn install_url_globals(ctx: &mut Context) {
    // `URL` may or may not already exist as a constructor on the
    // global. We add `createObjectURL` / `revokeObjectURL` as static
    // methods either way: if `URL` is already an object, assign onto
    // it; otherwise create a fresh holder.
    let realm = ctx.realm().clone();
    let create_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(url_create_object_url),
    )
    .build();
    let revoke_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(url_revoke_object_url),
    )
    .build();
    let global = ctx.global_object();
    let existing = global.get(js_string!("URL"), ctx).ok();
    let url_obj = match existing.and_then(|v| v.as_object().cloned()) {
        Some(o) => o,
        None => ObjectInitializer::new(ctx).build(),
    };
    let _ = url_obj.set(
        js_string!("createObjectURL"),
        JsValue::from(create_fn),
        false,
        ctx,
    );
    let _ = url_obj.set(
        js_string!("revokeObjectURL"),
        JsValue::from(revoke_fn),
        false,
        ctx,
    );
    let _ = global.set(
        js_string!("URL"),
        JsValue::from(url_obj),
        false,
        ctx,
    );
}

// ============ Blob ============

fn blob_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let parts = args.first().cloned().unwrap_or(JsValue::undefined());
    let opts = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let mime = read_blob_options(&opts, ctx);
    let bytes = collect_blob_parts(&parts, ctx);
    let id = store_blob(bytes.clone(), mime.clone());
    Ok(build_blob_object(ctx, id, bytes.len() as u32, &mime, false))
}

fn read_blob_options(val: &JsValue, ctx: &mut Context) -> String {
    let Some(obj) = val.as_object() else {
        return String::new();
    };
    obj.get(js_string!("type"), ctx)
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default()
}

/// Concatenate a `BlobPart[]` argument into one byte buffer. Each part
/// can be a string (UTF-8 encoded), an existing Blob (we copy its
/// bytes), an ArrayBuffer / Uint8Array, or another indexable.
fn collect_blob_parts(val: &JsValue, ctx: &mut Context) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(arr) = val.as_object() else {
        return out;
    };
    let len = arr
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    for i in 0..len {
        let Ok(item) = arr.get(i, ctx) else { continue };
        append_part(&item, &mut out, ctx);
    }
    out
}

fn append_part(val: &JsValue, out: &mut Vec<u8>, ctx: &mut Context) {
    if val.is_string() {
        if let Ok(s) = val.to_string(ctx) {
            out.extend_from_slice(s.to_std_string_escaped().as_bytes());
        }
        return;
    }
    if let Some(obj) = val.as_object() {
        // Existing Blob → copy its registry bytes.
        if let Ok(id_val) = obj.get(js_string!(BLOB_ID_KEY), ctx) {
            if !id_val.is_undefined() {
                if let Ok(id) = id_val.to_u32(ctx) {
                    if let Some(bytes) = read_blob_bytes(id) {
                        out.extend_from_slice(&bytes);
                        return;
                    }
                }
            }
        }
        // Uint8Array fast path.
        if let Ok(u8a) = JsUint8Array::from_object(obj.clone()) {
            let len = u8a.length(ctx).unwrap_or(0);
            for i in 0..len {
                if let Ok(v) = u8a.at(i as i64, ctx) {
                    if let Ok(n) = v.to_u32(ctx) {
                        out.push(n as u8);
                    }
                }
            }
            return;
        }
        // ArrayBuffer fast path.
        if let Ok(ab) = JsArrayBuffer::from_object(obj.clone()) {
            let len = ab.byte_length();
            if let Ok(view) = JsUint8Array::from_array_buffer(ab, ctx) {
                for i in 0..len {
                    if let Ok(v) = view.at(i as i64, ctx) {
                        if let Ok(n) = v.to_u32(ctx) {
                            out.push(n as u8);
                        }
                    }
                }
            }
            return;
        }
        // Last resort: indexable numeric array.
        let len = obj
            .get(js_string!("length"), ctx)
            .ok()
            .and_then(|v| v.to_u32(ctx).ok())
            .unwrap_or(0);
        for i in 0..len {
            if let Ok(v) = obj.get(i, ctx) {
                if let Ok(n) = v.to_u32(ctx) {
                    out.push(n as u8);
                }
            }
        }
    }
}

pub fn store_blob(bytes: Vec<u8>, mime: String) -> u32 {
    let id = next_blob_id();
    if let Some(reg) = JS_BLOBS.with(|r| r.borrow().clone()) {
        reg.borrow_mut().insert(id, BlobEntry { bytes, mime });
    }
    id
}

pub fn read_blob_bytes(id: u32) -> Option<Vec<u8>> {
    JS_BLOBS.with(|r| {
        r.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().get(&id).map(|e| e.bytes.clone()))
    })
}

pub fn read_blob_entry(id: u32) -> Option<BlobEntry> {
    JS_BLOBS.with(|r| r.borrow().as_ref().and_then(|rc| rc.borrow().get(&id).cloned()))
}

pub fn blob_id_of(val: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = val.as_object()?;
    let v = obj.get(js_string!(BLOB_ID_KEY), ctx).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.to_u32(ctx).ok()
}

fn build_blob_object(
    ctx: &mut Context,
    id: u32,
    size: u32,
    mime: &str,
    is_file: bool,
) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(BLOB_ID_KEY),
        JsValue::from(id),
        Attribute::READONLY,
    );
    b.property(
        js_string!("size"),
        JsValue::from(size),
        Attribute::READONLY,
    );
    b.property(
        js_string!("type"),
        JsValue::from(js_string!(mime.to_string())),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(blob_text),
        js_string!("text"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(blob_array_buffer),
        js_string!("arrayBuffer"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(blob_slice),
        js_string!("slice"),
        3,
    );
    b.function(
        NativeFunction::from_fn_ptr(blob_stream),
        js_string!("stream"),
        0,
    );
    if is_file {
        // File-specific properties stamped by the caller after build.
    }
    JsValue::from(b.build())
}

fn blob_text(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let value = match blob_id_of(this, ctx).and_then(read_blob_bytes) {
        Some(bytes) => {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            JsValue::from(js_string!(s))
        }
        None => JsValue::from(js_string!("")),
    };
    Ok(JsPromise::resolve(value, ctx).into())
}

fn blob_array_buffer(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bytes = blob_id_of(this, ctx).and_then(read_blob_bytes).unwrap_or_default();
    let buf = JsArrayBuffer::from_byte_block(bytes, ctx)?;
    Ok(JsPromise::resolve(JsValue::from(buf), ctx).into())
}

fn blob_slice(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bytes = blob_id_of(this, ctx).and_then(read_blob_bytes).unwrap_or_default();
    let total = bytes.len() as i64;
    let mut normalise = |v: Option<&JsValue>, default: i64| -> i64 {
        let raw = v
            .and_then(|x| if x.is_undefined() { None } else { Some(x.clone()) })
            .and_then(|x| x.to_number(ctx).ok())
            .map(|n| n as i64)
            .unwrap_or(default);
        if raw < 0 {
            (total + raw).max(0)
        } else {
            raw.min(total)
        }
    };
    let start = normalise(args.first(), 0);
    let end = normalise(args.get(1), total);
    let mime = args
        .get(2)
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let slice = if end > start {
        bytes[start as usize..end as usize].to_vec()
    } else {
        Vec::new()
    };
    let size = slice.len() as u32;
    let id = store_blob(slice, mime.clone());
    Ok(build_blob_object(ctx, id, size, &mime, false))
}

fn blob_stream(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bytes = blob_id_of(this, ctx).and_then(read_blob_bytes).unwrap_or_default();
    Ok(super::streams::body_to_stream(ctx, &bytes))
}

// ============ File ============

fn file_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let parts = args.first().cloned().unwrap_or(JsValue::undefined());
    let name = args
        .get(1)
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let opts = args.get(2).cloned().unwrap_or(JsValue::undefined());
    let mime = read_blob_options(&opts, ctx);
    let last_modified = opts
        .as_object()
        .and_then(|o| o.get(js_string!("lastModified"), ctx).ok())
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0)
        });
    let bytes = collect_blob_parts(&parts, ctx);
    let id = store_blob(bytes.clone(), mime.clone());
    let blob = build_blob_object(ctx, id, bytes.len() as u32, &mime, true);
    if let Some(obj) = blob.as_object() {
        let _ = obj.set(
            js_string!("name"),
            JsValue::from(js_string!(name)),
            false,
            ctx,
        );
        let _ = obj.set(
            js_string!("lastModified"),
            JsValue::from(last_modified),
            false,
            ctx,
        );
    }
    Ok(blob)
}

// ============ FileReader ============

struct ReaderState {
    handle: Option<boa_engine::JsObject>,
}

thread_local! {
    static READERS: RefCell<HashMap<u32, ReaderState>> = RefCell::new(HashMap::new());
    static READER_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_reader_id() -> u32 {
    READER_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

fn file_reader_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = next_reader_id();
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(READER_ID_KEY),
        JsValue::from(id),
        Attribute::READONLY,
    );
    b.property(
        js_string!("readyState"),
        JsValue::from(0_u32),
        Attribute::all(),
    );
    b.property(js_string!("result"), JsValue::null(), Attribute::all());
    b.property(js_string!("error"), JsValue::null(), Attribute::all());
    for name in [
        "onloadstart",
        "onload",
        "onloadend",
        "onerror",
        "onabort",
        "onprogress",
    ] {
        b.property(js_string!(name), JsValue::null(), Attribute::all());
    }
    b.function(
        NativeFunction::from_fn_ptr(reader_read_as_text),
        js_string!("readAsText"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(reader_read_as_array_buffer),
        js_string!("readAsArrayBuffer"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(reader_read_as_data_url),
        js_string!("readAsDataURL"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(reader_read_as_binary_string),
        js_string!("readAsBinaryString"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(reader_abort),
        js_string!("abort"),
        0,
    );
    let handle = b.build();
    READERS.with(|r| {
        r.borrow_mut().insert(
            id,
            ReaderState {
                handle: Some(handle.clone()),
            },
        );
    });
    Ok(JsValue::from(handle))
}

fn reader_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(READER_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn read_blob_arg(args: &[JsValue], ctx: &mut Context) -> Option<Vec<u8>> {
    let blob = args.first()?;
    let id = blob_id_of(blob, ctx)?;
    read_blob_bytes(id)
}

fn fire_reader_event(reader_obj: &boa_engine::JsObject, name: &str, ctx: &mut Context) {
    let Ok(handler_val) = reader_obj.get(js_string!(name.to_string()), ctx) else {
        return;
    };
    let Some(handler_obj) = handler_val.as_object() else {
        return;
    };
    let Some(handler) = JsFunction::from_object(handler_obj.clone()) else {
        return;
    };
    let event = ObjectInitializer::new(ctx)
        .property(
            js_string!("type"),
            JsValue::from(js_string!(name.trim_start_matches("on").to_string())),
            Attribute::READONLY,
        )
        .property(
            js_string!("target"),
            JsValue::from(reader_obj.clone()),
            Attribute::READONLY,
        )
        .build();
    let _ = handler.call(
        &JsValue::from(reader_obj.clone()),
        &[JsValue::from(event)],
        ctx,
    );
}

fn finish_reader(reader_obj: boa_engine::JsObject, result: JsValue, ctx: &mut Context) {
    let _ = reader_obj.set(js_string!("readyState"), JsValue::from(2_u32), false, ctx);
    let _ = reader_obj.set(js_string!("result"), result, false, ctx);
    fire_reader_event(&reader_obj, "onloadstart", ctx);
    fire_reader_event(&reader_obj, "onload", ctx);
    fire_reader_event(&reader_obj, "onloadend", ctx);
}

fn reader_read_as_text(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = reader_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let reader_obj = match READERS.with(|r| r.borrow().get(&id).and_then(|s| s.handle.clone())) {
        Some(h) => h,
        None => return Ok(JsValue::undefined()),
    };
    let bytes = read_blob_arg(args, ctx).unwrap_or_default();
    let s = String::from_utf8_lossy(&bytes).into_owned();
    finish_reader(reader_obj, JsValue::from(js_string!(s)), ctx);
    Ok(JsValue::undefined())
}

fn reader_read_as_array_buffer(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = reader_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let reader_obj = match READERS.with(|r| r.borrow().get(&id).and_then(|s| s.handle.clone())) {
        Some(h) => h,
        None => return Ok(JsValue::undefined()),
    };
    let bytes = read_blob_arg(args, ctx).unwrap_or_default();
    let buf = JsArrayBuffer::from_byte_block(bytes, ctx)?;
    finish_reader(reader_obj, JsValue::from(buf), ctx);
    Ok(JsValue::undefined())
}

fn reader_read_as_data_url(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = reader_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let reader_obj = match READERS.with(|r| r.borrow().get(&id).and_then(|s| s.handle.clone())) {
        Some(h) => h,
        None => return Ok(JsValue::undefined()),
    };
    let blob_arg = args.first().cloned().unwrap_or(JsValue::undefined());
    let id_opt = blob_id_of(&blob_arg, ctx);
    let entry = id_opt.and_then(read_blob_entry).unwrap_or(BlobEntry {
        bytes: Vec::new(),
        mime: String::new(),
    });
    let mime = if entry.mime.is_empty() {
        "application/octet-stream".to_string()
    } else {
        entry.mime
    };
    let b64 = base64_encode(&entry.bytes);
    let url = format!("data:{mime};base64,{b64}");
    finish_reader(reader_obj, JsValue::from(js_string!(url)), ctx);
    Ok(JsValue::undefined())
}

fn reader_read_as_binary_string(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = reader_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let reader_obj = match READERS.with(|r| r.borrow().get(&id).and_then(|s| s.handle.clone())) {
        Some(h) => h,
        None => return Ok(JsValue::undefined()),
    };
    let bytes = read_blob_arg(args, ctx).unwrap_or_default();
    // Per spec: each byte becomes a UTF-16 code unit with the same
    // numeric value. JS strings round-trip 0..255 cleanly via char.
    let s: String = bytes.iter().map(|b| *b as char).collect();
    finish_reader(reader_obj, JsValue::from(js_string!(s)), ctx);
    Ok(JsValue::undefined())
}

fn reader_abort(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(obj) = this.as_object() {
        let _ = obj.set(js_string!("readyState"), JsValue::from(2_u32), false, ctx);
        fire_reader_event(obj, "onabort", ctx);
        fire_reader_event(obj, "onloadend", ctx);
    }
    Ok(JsValue::undefined())
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n =
            ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | bytes[i + 2] as u32;
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHA[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

// ============ URL.createObjectURL / URL.revokeObjectURL ============

fn url_create_object_url(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(arg) = args.first() else {
        return Ok(JsValue::null());
    };
    // MediaSource gets its own `blob:mediasource/<id>` form so the
    // `<video>` src setter can detect MSE attachments without a
    // separate registry lookup.
    if let Some(url) = super::mse::object_url_for(arg, ctx) {
        return Ok(JsValue::from(js_string!(url)));
    }
    let Some(id) = blob_id_of(arg, ctx) else {
        return Ok(JsValue::null());
    };
    // Build a synthetic blob: URL. UUID-ish via timestamp + counter.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let url = format!("blob:daboss/{stamp:x}-{id}");
    if let Some(map) = JS_OBJECT_URLS.with(|r| r.borrow().clone()) {
        map.borrow_mut().insert(url.clone(), id);
    }
    Ok(JsValue::from(js_string!(url)))
}

fn url_revoke_object_url(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(arg) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let url = arg.to_string(ctx)?.to_std_string_escaped();
    if let Some(map) = JS_OBJECT_URLS.with(|r| r.borrow().clone()) {
        map.borrow_mut().remove(&url);
    }
    Ok(JsValue::undefined())
}

/// Public helper: resolve a `blob:` URL back to its bytes + MIME so
/// `fetch("blob:...")` and `<img src="blob:...">` can read the data.
pub fn resolve_object_url(url: &str) -> Option<BlobEntry> {
    let id = JS_OBJECT_URLS.with(|r| {
        r.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().get(url).copied())
    })?;
    read_blob_entry(id)
}
