//! `FormData` JS class — multipart form data wrapper that `fetch`
//! recognises as a request body.
//!
//! `new FormData(formEl?)` builds an empty container, or seeds entries
//! from a `<form>`'s inputs (toy: we walk the form's descendants and
//! pull `name`/`value` attributes from `<input>` / `<select>` /
//! `<textarea>`).
//!
//! `.append(name, value, filename?)` — value can be a string or a
//! Blob/File. Multiple entries with the same name are allowed.
//! `.set(name, value, filename?)` — replaces all entries with `name`.
//! `.get(name)` / `.getAll(name)` / `.has(name)` / `.delete(name)` —
//! standard.
//!
//! `fetch(url, { body: formData })` calls [`serialise_formdata`] to
//! convert to a `multipart/form-data; boundary=...` body.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use super::file::{blob_id_of, read_blob_entry};

const FORMDATA_ID_KEY: &str = "__formdata_id";

#[derive(Clone)]
pub enum FormDataValue {
    String(String),
    Blob {
        bytes: Vec<u8>,
        filename: String,
        mime: String,
    },
}

#[derive(Default, Clone)]
pub struct FormDataState {
    pub entries: Vec<(String, FormDataValue)>,
}

pub type FormDataRegistry = Rc<RefCell<HashMap<u32, FormDataState>>>;

thread_local! {
    pub(crate) static JS_FORMDATA: RefCell<Option<FormDataRegistry>> =
        const { RefCell::new(None) };
    pub(crate) static FORMDATA_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_id() -> u32 {
    FORMDATA_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    JS_FORMDATA.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Rc::new(RefCell::new(HashMap::new())));
        }
    });
    ctx.register_global_callable(
        js_string!("FormData"),
        1,
        NativeFunction::from_fn_ptr(formdata_ctor),
    )
    .ok();
}

fn formdata_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = next_id();
    let mut state = FormDataState::default();
    // If a form-like object is passed, harvest `name=value` entries
    // off its `elements` collection.
    if let Some(form_val) = args.first() {
        seed_from_form(form_val, &mut state, ctx);
    }
    if let Some(reg) = JS_FORMDATA.with(|r| r.borrow().clone()) {
        reg.borrow_mut().insert(id, state);
    }
    Ok(build_formdata_object(ctx, id))
}

fn build_formdata_object(ctx: &mut Context, id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(FORMDATA_ID_KEY),
        JsValue::from(id),
        Attribute::READONLY,
    );
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("append", NativeFunction::from_fn_ptr(formdata_append), 3),
        ("set", NativeFunction::from_fn_ptr(formdata_set), 3),
        ("get", NativeFunction::from_fn_ptr(formdata_get), 1),
        ("getAll", NativeFunction::from_fn_ptr(formdata_get_all), 1),
        ("has", NativeFunction::from_fn_ptr(formdata_has), 1),
        ("delete", NativeFunction::from_fn_ptr(formdata_delete), 1),
        ("entries", NativeFunction::from_fn_ptr(formdata_entries), 0),
        ("keys", NativeFunction::from_fn_ptr(formdata_keys), 0),
        ("values", NativeFunction::from_fn_ptr(formdata_values), 0),
        ("forEach", NativeFunction::from_fn_ptr(formdata_for_each), 1),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    JsValue::from(b.build())
}

fn seed_from_form(form_val: &JsValue, state: &mut FormDataState, ctx: &mut Context) {
    let Some(form) = form_val.as_object() else {
        return;
    };
    // Try `.elements` (NodeList-like) first; fall back to children.
    let elements = form
        .get(js_string!("elements"), ctx)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let collection = elements.unwrap_or_else(|| form.clone());
    let len = collection
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    for i in 0..len {
        let Ok(item) = collection.get(i, ctx) else { continue };
        let Some(el) = item.as_object() else { continue };
        let name = el
            .get(js_string!("name"), ctx)
            .ok()
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let value = el
            .get(js_string!("value"), ctx)
            .ok()
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default();
        state.entries.push((name, FormDataValue::String(value)));
    }
}

fn id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(FORMDATA_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn with_state<R>(
    this: &JsValue,
    ctx: &mut Context,
    f: impl FnOnce(&mut FormDataState) -> R,
) -> Option<R> {
    let id = id_of(this, ctx)?;
    let reg = JS_FORMDATA.with(|r| r.borrow().clone())?;
    let mut borrow = reg.borrow_mut();
    let state = borrow.get_mut(&id)?;
    Some(f(state))
}

fn to_form_value(val: &JsValue, filename: Option<String>, ctx: &mut Context) -> FormDataValue {
    if let Some(id) = blob_id_of(val, ctx) {
        if let Some(entry) = read_blob_entry(id) {
            let mime = if entry.mime.is_empty() {
                "application/octet-stream".to_string()
            } else {
                entry.mime
            };
            // `File` carries its own `name`; default to "blob" otherwise.
            let filename = filename.unwrap_or_else(|| {
                val.as_object()
                    .and_then(|o| o.get(js_string!("name"), ctx).ok())
                    .and_then(|v| {
                        if v.is_undefined() || v.is_null() {
                            None
                        } else {
                            v.to_string(ctx).ok()
                        }
                    })
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_else(|| "blob".to_string())
            });
            return FormDataValue::Blob {
                bytes: entry.bytes,
                filename,
                mime,
            };
        }
    }
    let s = val
        .to_string(ctx)
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    FormDataValue::String(s)
}

fn formdata_append(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let value = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let filename = args
        .get(2)
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?;
    let fv = to_form_value(&value, filename, ctx);
    with_state(this, ctx, |state| state.entries.push((name, fv)));
    Ok(JsValue::undefined())
}

fn formdata_set(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let value = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let filename = args
        .get(2)
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?;
    let fv = to_form_value(&value, filename, ctx);
    with_state(this, ctx, |state| {
        state.entries.retain(|(n, _)| n != &name);
        state.entries.push((name, fv));
    });
    Ok(JsValue::undefined())
}

fn formdata_get(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let entry = with_state(this, ctx, |state| {
        state
            .entries
            .iter()
            .find(|(n, _)| n == &name)
            .map(|(_, v)| v.clone())
    })
    .flatten();
    Ok(form_value_to_js(entry, ctx))
}

fn formdata_get_all(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let values: Vec<FormDataValue> = with_state(this, ctx, |state| {
        state
            .entries
            .iter()
            .filter(|(n, _)| n == &name)
            .map(|(_, v)| v.clone())
            .collect()
    })
    .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for v in values {
        let _ = arr.push(form_value_to_js(Some(v), ctx), ctx);
    }
    Ok(arr.into())
}

fn formdata_has(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let has = with_state(this, ctx, |state| state.entries.iter().any(|(n, _)| n == &name))
        .unwrap_or(false);
    Ok(JsValue::from(has))
}

fn formdata_delete(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    with_state(this, ctx, |state| state.entries.retain(|(n, _)| n != &name));
    Ok(JsValue::undefined())
}

fn formdata_entries(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let entries: Vec<(String, FormDataValue)> =
        with_state(this, ctx, |state| state.entries.clone()).unwrap_or_default();
    let arr = JsArray::new(ctx);
    for (k, v) in entries {
        let pair = JsArray::new(ctx);
        let _ = pair.push(JsValue::from(js_string!(k)), ctx);
        let _ = pair.push(form_value_to_js(Some(v), ctx), ctx);
        let _ = arr.push(JsValue::from(pair), ctx);
    }
    Ok(arr.into())
}

fn formdata_keys(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let names: Vec<String> = with_state(this, ctx, |state| {
        state.entries.iter().map(|(k, _)| k.clone()).collect()
    })
    .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for k in names {
        let _ = arr.push(JsValue::from(js_string!(k)), ctx);
    }
    Ok(arr.into())
}

fn formdata_values(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let values: Vec<FormDataValue> =
        with_state(this, ctx, |state| state.entries.iter().map(|(_, v)| v.clone()).collect())
            .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for v in values {
        let _ = arr.push(form_value_to_js(Some(v), ctx), ctx);
    }
    Ok(arr.into())
}

fn formdata_for_each(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(cb_val) = args.first() else {
        return Ok(JsValue::undefined());
    };
    let Some(cb_obj) = cb_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Some(cb) = boa_engine::object::builtins::JsFunction::from_object(cb_obj.clone()) else {
        return Ok(JsValue::undefined());
    };
    let entries: Vec<(String, FormDataValue)> =
        with_state(this, ctx, |state| state.entries.clone()).unwrap_or_default();
    let this_clone = this.clone();
    for (k, v) in entries {
        let value_js = form_value_to_js(Some(v), ctx);
        let _ = cb.call(
            &JsValue::undefined(),
            &[
                value_js,
                JsValue::from(js_string!(k)),
                this_clone.clone(),
            ],
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn form_value_to_js(v: Option<FormDataValue>, ctx: &mut Context) -> JsValue {
    match v {
        Some(FormDataValue::String(s)) => JsValue::from(js_string!(s)),
        Some(FormDataValue::Blob {
            bytes,
            filename,
            mime,
        }) => {
            let id = super::file::store_blob(bytes.clone(), mime.clone());
            let blob = ObjectInitializer::new(ctx)
                .property(
                    js_string!("__blob_id"),
                    JsValue::from(id),
                    Attribute::READONLY,
                )
                .property(
                    js_string!("size"),
                    JsValue::from(bytes.len() as u32),
                    Attribute::READONLY,
                )
                .property(
                    js_string!("type"),
                    JsValue::from(js_string!(mime)),
                    Attribute::READONLY,
                )
                .property(
                    js_string!("name"),
                    JsValue::from(js_string!(filename)),
                    Attribute::READONLY,
                )
                .build();
            JsValue::from(blob)
        }
        None => JsValue::null(),
    }
}

/// Public helper: extract a FormDataState by handle. `fetch` uses this
/// to detect and serialise a FormData body.
pub fn formdata_state_of(val: &JsValue, ctx: &mut Context) -> Option<FormDataState> {
    let id = id_of(val, ctx)?;
    let reg = JS_FORMDATA.with(|r| r.borrow().clone())?;
    let result = reg.borrow().get(&id).cloned();
    result
}

/// Serialise a FormData into a multipart body. Returns `(body_bytes,
/// content_type)`. The boundary is timestamp-derived; we don't
/// scan the body for collisions since strings rarely contain
/// "------daboss-...".
pub fn serialise_formdata(state: &FormDataState) -> (Vec<u8>, String) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let boundary = format!("----daboss-formdata-{stamp:x}");
    let mut out = Vec::new();
    for (name, value) in &state.entries {
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        match value {
            FormDataValue::String(s) => {
                let header = format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n");
                out.extend_from_slice(header.as_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            FormDataValue::Blob {
                bytes,
                filename,
                mime,
            } => {
                let header = format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n"
                );
                out.extend_from_slice(header.as_bytes());
                out.extend_from_slice(bytes);
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    let ct = format!("multipart/form-data; boundary={boundary}");
    (out, ct)
}
