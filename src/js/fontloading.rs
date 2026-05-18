//! CSS Font Loading API (`document.fonts`, `FontFace`).
//!
//! Web pages call `new FontFace(family, source).load().then(face =>
//! document.fonts.add(face))` to fetch a custom font and bring it
//! into the document's font system. The bytes land in a per-engine
//! registry; the paint layer ingests them when it builds its
//! `cosmic_text::FontSystem` so the family becomes selectable in
//! CSS `font-family` rules.
//!
//! `@font-face` rules in CSS hit the same registry through
//! [`register_font_bytes`]; the browser shell walks
//! `Stylesheet.font_faces`, fetches the URLs through the existing
//! net client, and pushes the bytes here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{
        builtins::{JsArray, JsPromise},
        ObjectInitializer,
    },
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

#[derive(Clone)]
pub struct FontEntry {
    pub family: String,
    pub bytes: Vec<u8>,
    pub status: FontStatus,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FontStatus {
    Unloaded,
    Loading,
    Loaded,
    Error,
}

pub type FontRegistry = Rc<RefCell<HashMap<u32, FontEntry>>>;

thread_local! {
    pub(crate) static FONT_REGISTRY: RefCell<FontRegistry> =
        RefCell::new(Rc::new(RefCell::new(HashMap::new())));
    pub(crate) static FONT_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
    /// Fonts added to `document.fonts` (the "set" the Font Loading
    /// API maintains). Holds (FontFace id, JS handle).
    pub(crate) static DOCUMENT_FONTS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

fn next_id() -> u32 {
    FONT_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

const FONT_ID_KEY: &str = "__font_face_id";

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("FontFace"),
        2,
        NativeFunction::from_fn_ptr(font_face_ctor),
    )
    .ok();
    install_document_fonts(ctx);
}

fn install_document_fonts(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let methods: &[(&str, NativeFunction)] = &[
        ("add", NativeFunction::from_fn_ptr(document_fonts_add)),
        ("delete", NativeFunction::from_fn_ptr(document_fonts_delete)),
        ("clear", NativeFunction::from_fn_ptr(document_fonts_clear)),
        ("has", NativeFunction::from_fn_ptr(document_fonts_has)),
        ("check", NativeFunction::from_fn_ptr(document_fonts_check)),
        ("load", NativeFunction::from_fn_ptr(document_fonts_load)),
        ("forEach", NativeFunction::from_fn_ptr(document_fonts_for_each)),
    ];
    let mut entries: Vec<(&str, JsValue)> = Vec::new();
    for (name, f) in methods {
        let func = boa_engine::object::FunctionObjectBuilder::new(&realm, f.clone()).build();
        entries.push((name, JsValue::from(func)));
    }
    let ready_promise = build_ready_promise(ctx);
    let size_getter_realm = ctx.realm().clone();
    let size_getter = boa_engine::object::FunctionObjectBuilder::new(
        &size_getter_realm,
        NativeFunction::from_fn_ptr(document_fonts_size),
    )
    .build();
    let mut b = ObjectInitializer::new(ctx);
    for (name, val) in entries {
        b.property(js_string!(name), val, Attribute::READONLY);
    }
    b.property(
        js_string!("status"),
        JsValue::from(js_string!("loaded")),
        Attribute::all(),
    );
    b.property(js_string!("ready"), ready_promise, Attribute::READONLY);
    let fonts = b.build();
    let _ = fonts.define_property_or_throw(
        js_string!("size"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(size_getter)
            .enumerable(true)
            .configurable(true),
        ctx,
    );
    let global = ctx.global_object();
    if let Ok(doc_val) = global.get(js_string!("document"), ctx) {
        if let Some(doc) = doc_val.as_object() {
            let _ = doc.set(js_string!("fonts"), JsValue::from(fonts), false, ctx);
        }
    }
}

fn build_ready_promise(ctx: &mut Context) -> JsValue {
    // We resolve immediately — the toy considers fonts ready as soon
    // as their bytes hit the registry, which happens synchronously
    // in FontFace.load() / register_font_bytes().
    JsPromise::resolve(JsValue::undefined(), ctx).into()
}

// ============ FontFace constructor ============

fn font_face_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let family = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let source = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let id = next_id();
    let entry = FontEntry {
        family: family.clone(),
        bytes: Vec::new(),
        status: FontStatus::Unloaded,
    };
    FONT_REGISTRY.with(|r| {
        let rc = r.borrow().clone();
        rc.borrow_mut().insert(id, entry);
    });
    let handle = build_font_face_object(ctx, id);
    // Eagerly resolve sources passed as ArrayBuffer / Uint8Array;
    // string sources need an HTTP fetch and resolve via .load().
    if let Some(bytes) = read_bytes_inline(&source, ctx) {
        store_bytes(id, bytes);
    } else if source.is_string() {
        // Stash the URL on the JS handle so .load() can pick it up.
        let url = source.to_string(ctx)?.to_std_string_escaped();
        let url_str = strip_url_quoted(&url);
        if let Some(obj) = handle.as_object() {
            let _ = obj.set(
                js_string!("__font_url"),
                JsValue::from(js_string!(url_str)),
                false,
                ctx,
            );
        }
    }
    Ok(handle)
}

fn build_font_face_object(ctx: &mut Context, id: u32) -> JsValue {
    let family = FONT_REGISTRY.with(|r| {
        let rc = r.borrow().clone();
        let family = rc
            .borrow()
            .get(&id)
            .map(|e| e.family.clone())
            .unwrap_or_default();
        family
    });
    let loaded_realm = ctx.realm().clone();
    let loaded_getter = boa_engine::object::FunctionObjectBuilder::new(
        &loaded_realm,
        NativeFunction::from_fn_ptr(font_face_loaded),
    )
    .build();
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(FONT_ID_KEY), JsValue::from(id), Attribute::READONLY);
    b.property(
        js_string!("family"),
        JsValue::from(js_string!(family)),
        Attribute::all(),
    );
    b.property(
        js_string!("status"),
        JsValue::from(js_string!("unloaded")),
        Attribute::all(),
    );
    for prop in ["weight", "style", "stretch", "unicodeRange", "variant", "featureSettings"] {
        b.property(
            js_string!(prop),
            JsValue::from(js_string!("normal")),
            Attribute::all(),
        );
    }
    b.function(
        NativeFunction::from_fn_ptr(font_face_load),
        js_string!("load"),
        0,
    );
    let handle = b.build();
    let _ = handle.define_property_or_throw(
        js_string!("loaded"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(loaded_getter)
            .enumerable(true)
            .configurable(true),
        ctx,
    );
    JsValue::from(handle)
}

fn font_face_load(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = read_font_id(this, ctx) else {
        return Ok(JsPromise::resolve(this.clone(), ctx).into());
    };
    let url = this
        .as_object()
        .and_then(|o| o.get(js_string!("__font_url"), ctx).ok())
        .and_then(|v| {
            if v.is_undefined() || v.is_null() {
                None
            } else {
                v.to_string(ctx).ok().map(|s| s.to_std_string_escaped())
            }
        })
        .unwrap_or_default();
    if !url.is_empty() {
        // Resolve relative to the page's base URL and fetch through
        // the shared client.
        let bytes = fetch_font_bytes(&url);
        if let Some(b) = bytes {
            store_bytes(id, b);
        } else {
            mark_font_error(id);
        }
    } else {
        // No URL — assume the constructor already populated bytes.
        FONT_REGISTRY.with(|r| {
            let rc = r.borrow().clone();
            let mut reg = rc.borrow_mut();
            if let Some(e) = reg.get_mut(&id) {
                if !e.bytes.is_empty() {
                    e.status = FontStatus::Loaded;
                }
            }
        });
    }
    if let Some(obj) = this.as_object() {
        let s = current_status_str(id);
        let _ = obj.set(
            js_string!("status"),
            JsValue::from(js_string!(s.to_string())),
            false,
            ctx,
        );
    }
    Ok(JsPromise::resolve(this.clone(), ctx).into())
}

fn font_face_loaded(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(this.clone(), ctx).into())
}

fn fetch_font_bytes(url: &str) -> Option<Vec<u8>> {
    let resolved = super::engine::JS_BASE_URL.with(|slot| {
        if let Some(base) = slot.borrow().as_ref() {
            base.join(url).ok()
        } else {
            url::Url::parse(url).ok()
        }
    })?;
    super::engine::JS_FETCH_CLIENT.with(|slot| {
        let client = slot.borrow().as_ref()?.clone();
        let resp = client.get(resolved.as_str()).ok()?;
        if !(200..300).contains(&resp.status) {
            return None;
        }
        Some(resp.body)
    })
}

fn store_bytes(id: u32, bytes: Vec<u8>) {
    FONT_REGISTRY.with(|r| {
        let rc = r.borrow().clone();
        let mut reg = rc.borrow_mut();
        if let Some(e) = reg.get_mut(&id) {
            e.bytes = bytes;
            e.status = FontStatus::Loaded;
        }
    });
}

fn mark_font_error(id: u32) {
    FONT_REGISTRY.with(|r| {
        let rc = r.borrow().clone();
        let mut reg = rc.borrow_mut();
        if let Some(e) = reg.get_mut(&id) {
            e.status = FontStatus::Error;
        }
    });
}

fn current_status_str(id: u32) -> &'static str {
    let s = FONT_REGISTRY.with(|r| {
        let rc = r.borrow().clone();
        let status = rc
            .borrow()
            .get(&id)
            .map(|e| e.status)
            .unwrap_or(FontStatus::Unloaded);
        status
    });
    match s {
        FontStatus::Unloaded => "unloaded",
        FontStatus::Loading => "loading",
        FontStatus::Loaded => "loaded",
        FontStatus::Error => "error",
    }
}

fn read_font_id(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(FONT_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn read_bytes_inline(val: &JsValue, ctx: &mut Context) -> Option<Vec<u8>> {
    use boa_engine::object::builtins::{JsArrayBuffer, JsUint8Array};
    let obj = val.as_object()?;
    if let Ok(u8a) = JsUint8Array::from_object(obj.clone()) {
        let len = u8a.length(ctx).unwrap_or(0);
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let v = u8a.at(i as i64, ctx).ok()?;
            out.push(v.to_u32(ctx).ok()? as u8);
        }
        return Some(out);
    }
    if let Ok(ab) = JsArrayBuffer::from_object(obj.clone()) {
        let len = ab.byte_length();
        let view = JsUint8Array::from_array_buffer(ab, ctx).ok()?;
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let v = view.at(i as i64, ctx).ok()?;
            out.push(v.to_u32(ctx).ok()? as u8);
        }
        return Some(out);
    }
    None
}

fn strip_url_quoted(s: &str) -> String {
    let s = s.trim();
    if let Some(stripped) = s.strip_prefix("url(").and_then(|t| t.strip_suffix(')')) {
        let inner = stripped.trim();
        return inner
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
    }
    s.trim_matches('"').trim_matches('\'').to_string()
}

// ============ document.fonts ============

fn document_fonts_add(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = args.first().and_then(|v| read_font_id(v, ctx)) else {
        return Ok(JsValue::undefined());
    };
    DOCUMENT_FONTS.with(|s| {
        let mut v = s.borrow_mut();
        if !v.contains(&id) {
            v.push(id);
        }
    });
    Ok(args.first().cloned().unwrap_or(JsValue::undefined()))
}

fn document_fonts_delete(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = args.first().and_then(|v| read_font_id(v, ctx)) else {
        return Ok(JsValue::from(false));
    };
    let removed = DOCUMENT_FONTS.with(|s| {
        let mut v = s.borrow_mut();
        if let Some(pos) = v.iter().position(|x| *x == id) {
            v.remove(pos);
            true
        } else {
            false
        }
    });
    Ok(JsValue::from(removed))
}

fn document_fonts_clear(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    DOCUMENT_FONTS.with(|s| s.borrow_mut().clear());
    Ok(JsValue::undefined())
}

fn document_fonts_has(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = args.first().and_then(|v| read_font_id(v, ctx)) else {
        return Ok(JsValue::from(false));
    };
    let present = DOCUMENT_FONTS.with(|s| s.borrow().contains(&id));
    Ok(JsValue::from(present))
}

fn document_fonts_check(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    // Per spec: returns true when every font matching the shorthand
    // is already loaded. We resolve immediately since loads complete
    // synchronously in the toy.
    Ok(JsValue::from(true))
}

fn document_fonts_load(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Resolve with an empty array — the spec's return type is
    // `Promise<FontFace[]>` of fonts matching the shorthand. Our
    // toy doesn't currently track that mapping.
    let arr = JsArray::new(ctx);
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

fn document_fonts_size(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let n = DOCUMENT_FONTS.with(|s| s.borrow().len() as u32);
    Ok(JsValue::from(n))
}

fn document_fonts_for_each(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsFunction;
    let Some(cb) = args
        .first()
        .and_then(|v| v.as_object().cloned())
        .and_then(JsFunction::from_object)
    else {
        return Ok(JsValue::undefined());
    };
    let ids: Vec<u32> = DOCUMENT_FONTS.with(|s| s.borrow().clone());
    for id in ids {
        let face = build_font_face_object(ctx, id);
        let _ = cb.call(
            &JsValue::undefined(),
            &[face.clone(), face, JsValue::undefined()],
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

/// Public hook for the browser shell: walks `Stylesheet.font_faces`
/// after a fresh page load, fetches each URL through the existing
/// net client, and registers the bytes so paint's `FontSystem`
/// picks them up.
pub fn register_font_face_css_rules<'a>(
    rules: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    for (family, url_or_data) in rules {
        let bytes = if url_or_data.starts_with("data:") {
            data_url_bytes(url_or_data)
        } else {
            fetch_font_bytes(url_or_data)
        };
        let Some(bytes) = bytes else {
            continue;
        };
        let id = next_id();
        FONT_REGISTRY.with(|r| {
            let rc = r.borrow().clone();
            rc.borrow_mut().insert(
                id,
                FontEntry {
                    family: family.to_string(),
                    bytes,
                    status: FontStatus::Loaded,
                },
            );
        });
        DOCUMENT_FONTS.with(|s| s.borrow_mut().push(id));
    }
}

fn data_url_bytes(url: &str) -> Option<Vec<u8>> {
    let body = url.strip_prefix("data:")?;
    let (header, payload) = body.split_once(',')?;
    if header.ends_with(";base64") {
        base64_decode(payload)
    } else {
        Some(payload.as_bytes().to_vec())
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let pad = s.iter().rev().take_while(|b| **b == b'=').count();
    let usable_len = s.len() - pad;
    let mut out = Vec::with_capacity(usable_len * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for b in &s[..usable_len] {
        let v = match *b {
            b'A'..=b'Z' => *b - b'A',
            b'a'..=b'z' => *b - b'a' + 26,
            b'0'..=b'9' => *b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// All loaded font bytes, paired with their advertised family. Paint
/// consults this when building its `cosmic_text::FontSystem` so the
/// fontdb knows about the JS-registered web fonts.
pub fn registered_font_bytes() -> Vec<(String, Vec<u8>)> {
    let registry = FONT_REGISTRY.with(|r| r.borrow().clone());
    let inner = registry.borrow();
    inner
        .values()
        .filter(|e| e.status == FontStatus::Loaded && !e.bytes.is_empty())
        .map(|e| (e.family.clone(), e.bytes.clone()))
        .collect()
}
