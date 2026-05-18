//! `navigator.clipboard` + `DataTransfer` JS bindings.
//!
//! Clipboard read/write call into [`arboard`] so copy/paste round-trips
//! through the real OS clipboard. Operations return Promises per the
//! spec; we resolve them synchronously since arboard is sync. On
//! failure (e.g. no display server in CI) the Promise rejects with
//! the underlying error message.
//!
//! `DataTransfer` is the payload object dragstart/dragover/drop events
//! carry. JS code listening for drag events can call `.setData(type,
//! data)` to stash a payload, then `.getData(type)` on drop. We give
//! pages the construct + getter/setter shape but don't yet fire the
//! events from native mouse handling — that wiring lives in `main.rs`
//! and is part of the same task arc.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

thread_local! {
    /// Process-wide fallback when arboard fails (no display server,
    /// sandboxed runtime, etc.). In-process pages can still
    /// copy/paste against this storage.
    pub(crate) static CLIPBOARD_FALLBACK: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn install(ctx: &mut Context) {
    install_navigator_clipboard(ctx);
    install_data_transfer(ctx);
}

fn install_navigator_clipboard(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let write_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(clipboard_write_text),
    )
    .build();
    let read_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(clipboard_read_text),
    )
    .build();
    let write_items_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(clipboard_write_items),
    )
    .build();
    let read_items_fn = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(clipboard_read_items),
    )
    .build();
    let clipboard = ObjectInitializer::new(ctx)
        .property(js_string!("writeText"), JsValue::from(write_fn), Attribute::READONLY)
        .property(js_string!("readText"), JsValue::from(read_fn), Attribute::READONLY)
        .property(js_string!("write"), JsValue::from(write_items_fn), Attribute::READONLY)
        .property(js_string!("read"), JsValue::from(read_items_fn), Attribute::READONLY)
        .build();
    let global = ctx.global_object();
    if let Ok(nav_val) = global.get(js_string!("navigator"), ctx) {
        if let Some(nav) = nav_val.as_object() {
            let _ = nav.set(
                js_string!("clipboard"),
                JsValue::from(clipboard),
                false,
                ctx,
            );
        }
    }
}

fn os_clipboard_get() -> Result<String, String> {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb
            .get_text()
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn os_clipboard_set(text: &str) -> Result<(), String> {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb
            .set_text(text.to_string())
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn clipboard_write_text(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    // Always update the in-process fallback so same-session reads
    // succeed even when the OS clipboard is unavailable.
    CLIPBOARD_FALLBACK.with(|c| *c.borrow_mut() = text.clone());
    match os_clipboard_set(&text) {
        Ok(()) => Ok(JsPromise::resolve(JsValue::undefined(), ctx).into()),
        Err(_e) => {
            // The fallback still has the text. Resolve so pages that
            // ignore platform-clipboard failures still proceed.
            Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
        }
    }
}

fn clipboard_read_text(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = match os_clipboard_get() {
        Ok(t) => t,
        Err(_) => CLIPBOARD_FALLBACK.with(|c| c.borrow().clone()),
    };
    Ok(JsPromise::resolve(JsValue::from(js_string!(text)), ctx).into())
}

fn clipboard_write_items(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Spec form: `navigator.clipboard.write([new ClipboardItem({'text/plain': blob})])`.
    // Toy form: walk every item, look for a `text/plain` entry, and
    // copy whichever first resolves to text. We don't write
    // image/png to the OS clipboard yet.
    let Some(items_val) = args.first() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let Some(items_obj) = items_val.as_object() else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let len = items_obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    for i in 0..len {
        let Ok(item) = items_obj.get(i, ctx) else { continue };
        let Some(item_obj) = item.as_object() else { continue };
        if let Ok(types) = item_obj.get(js_string!("types"), ctx) {
            if let Some(types_obj) = types.as_object() {
                let tlen = types_obj
                    .get(js_string!("length"), ctx)
                    .ok()
                    .and_then(|v| v.to_u32(ctx).ok())
                    .unwrap_or(0);
                for j in 0..tlen {
                    let Ok(t) = types_obj.get(j, ctx) else { continue };
                    let kind = t.to_string(ctx)?.to_std_string_escaped();
                    if kind != "text/plain" {
                        continue;
                    }
                    if let Ok(blob_val) = item_obj.get(js_string!("text/plain"), ctx) {
                        if let Some(id) = super::file::blob_id_of(&blob_val, ctx) {
                            if let Some(entry) = super::file::read_blob_entry(id) {
                                let text = String::from_utf8_lossy(&entry.bytes).into_owned();
                                CLIPBOARD_FALLBACK
                                    .with(|c| *c.borrow_mut() = text.clone());
                                let _ = os_clipboard_set(&text);
                                return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn clipboard_read_items(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let text = match os_clipboard_get() {
        Ok(t) => t,
        Err(_) => CLIPBOARD_FALLBACK.with(|c| c.borrow().clone()),
    };
    // Wrap as a ClipboardItem-shaped object with `types: ["text/plain"]`
    // and a `getType(t)` that returns a Promise resolving to a Blob.
    let bytes = text.into_bytes();
    let blob_id = super::file::store_blob(bytes.clone(), "text/plain".to_string());
    let blob = ObjectInitializer::new(ctx)
        .property(
            js_string!("__blob_id"),
            JsValue::from(blob_id),
            Attribute::READONLY,
        )
        .property(
            js_string!("size"),
            JsValue::from(bytes.len() as u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("type"),
            JsValue::from(js_string!("text/plain")),
            Attribute::READONLY,
        )
        .build();
    let types = JsArray::new(ctx);
    let _ = types.push(JsValue::from(js_string!("text/plain")), ctx);
    let item = ObjectInitializer::new(ctx)
        .property(js_string!("types"), JsValue::from(types), Attribute::READONLY)
        .property(
            js_string!("text/plain"),
            JsValue::from(blob),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(clipboard_item_get_type),
            js_string!("getType"),
            1,
        )
        .build();
    let arr = JsArray::new(ctx);
    let _ = arr.push(JsValue::from(item), ctx);
    Ok(JsPromise::resolve(JsValue::from(arr), ctx).into())
}

fn clipboard_item_get_type(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(obj) = this.as_object() else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let key = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let value = obj
        .get(js_string!(key), ctx)
        .ok()
        .unwrap_or(JsValue::null());
    Ok(JsPromise::resolve(value, ctx).into())
}

// ============ DataTransfer ============

const DT_ID_KEY: &str = "__dt_id";

#[derive(Default, Clone)]
pub struct DataTransferState {
    pub items: Vec<(String, String)>,
    pub files: Vec<u32>, // blob ids
    pub drop_effect: String,
    pub effect_allowed: String,
}

pub type DataTransferRegistry = Rc<RefCell<HashMap<u32, DataTransferState>>>;

thread_local! {
    pub(crate) static JS_DATATRANSFER: RefCell<Option<DataTransferRegistry>> =
        const { RefCell::new(None) };
    pub(crate) static DT_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_dt_id() -> u32 {
    DT_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

fn install_data_transfer(ctx: &mut Context) {
    JS_DATATRANSFER.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Rc::new(RefCell::new(HashMap::new())));
        }
    });
    ctx.register_global_callable(
        js_string!("DataTransfer"),
        0,
        NativeFunction::from_fn_ptr(data_transfer_ctor),
    )
    .ok();
}

fn data_transfer_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(build_data_transfer(ctx))
}

/// Build a fresh DataTransfer JS handle. Public so that event-firing
/// code in `main.rs` / engine event dispatch can attach one to drag
/// / drop event objects.
pub fn build_data_transfer(ctx: &mut Context) -> JsValue {
    let id = next_dt_id();
    if let Some(reg) = JS_DATATRANSFER.with(|r| r.borrow().clone()) {
        reg.borrow_mut().insert(
            id,
            DataTransferState {
                drop_effect: "none".to_string(),
                effect_allowed: "all".to_string(),
                ..DataTransferState::default()
            },
        );
    }
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(DT_ID_KEY), JsValue::from(id), Attribute::READONLY);
    b.property(
        js_string!("dropEffect"),
        JsValue::from(js_string!("none")),
        Attribute::all(),
    );
    b.property(
        js_string!("effectAllowed"),
        JsValue::from(js_string!("all")),
        Attribute::all(),
    );
    b.function(NativeFunction::from_fn_ptr(dt_get_data), js_string!("getData"), 1);
    b.function(NativeFunction::from_fn_ptr(dt_set_data), js_string!("setData"), 2);
    b.function(
        NativeFunction::from_fn_ptr(dt_clear_data),
        js_string!("clearData"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(dt_get_types),
        js_string!("__readTypes"),
        0,
    );
    let handle = b.build();
    // Stamp dynamic `types` / `files` accessors after build so we can
    // refer to the same `id`. JS reads `.types` / `.files` as plain
    // properties; we refresh them on access by storing live getters.
    let realm = ctx.realm().clone();
    let types_getter = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(dt_get_types),
    )
    .build();
    let files_getter = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(dt_get_files),
    )
    .build();
    let _ = handle.define_property_or_throw(
        js_string!("types"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(types_getter)
            .enumerable(true)
            .configurable(true),
        ctx,
    );
    let _ = handle.define_property_or_throw(
        js_string!("files"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(files_getter)
            .enumerable(true)
            .configurable(true),
        ctx,
    );
    JsValue::from(handle)
}

fn dt_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(DT_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn with_dt<R>(
    this: &JsValue,
    ctx: &mut Context,
    f: impl FnOnce(&mut DataTransferState) -> R,
) -> Option<R> {
    let id = dt_id_of(this, ctx)?;
    let reg = JS_DATATRANSFER.with(|r| r.borrow().clone())?;
    let mut borrow = reg.borrow_mut();
    let state = borrow.get_mut(&id)?;
    Some(f(state))
}

fn dt_get_data(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let fmt = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let value = with_dt(this, ctx, |state| {
        state
            .items
            .iter()
            .find(|(k, _)| canonical_format(k) == canonical_format(&fmt))
            .map(|(_, v)| v.clone())
    })
    .flatten()
    .unwrap_or_default();
    Ok(JsValue::from(js_string!(value)))
}

fn dt_set_data(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let fmt = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let value = args
        .get(1)
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    with_dt(this, ctx, |state| {
        state.items.retain(|(k, _)| canonical_format(k) != canonical_format(&fmt));
        state.items.push((canonical_format(&fmt), value));
    });
    Ok(JsValue::undefined())
}

fn dt_clear_data(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let fmt = args
        .first()
        .filter(|v| !v.is_undefined())
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?;
    with_dt(this, ctx, |state| match fmt {
        Some(f) => state.items.retain(|(k, _)| canonical_format(k) != canonical_format(&f)),
        None => state.items.clear(),
    });
    Ok(JsValue::undefined())
}

fn dt_get_types(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let types: Vec<String> =
        with_dt(this, ctx, |state| state.items.iter().map(|(k, _)| k.clone()).collect())
            .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for t in types {
        let _ = arr.push(JsValue::from(js_string!(t)), ctx);
    }
    Ok(arr.into())
}

fn dt_get_files(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let blob_ids: Vec<u32> =
        with_dt(this, ctx, |state| state.files.clone()).unwrap_or_default();
    let arr = JsArray::new(ctx);
    for id in blob_ids {
        let Some(entry) = super::file::read_blob_entry(id) else { continue };
        let f = ObjectInitializer::new(ctx)
            .property(
                js_string!("__blob_id"),
                JsValue::from(id),
                Attribute::READONLY,
            )
            .property(
                js_string!("size"),
                JsValue::from(entry.bytes.len() as u32),
                Attribute::READONLY,
            )
            .property(
                js_string!("type"),
                JsValue::from(js_string!(entry.mime.clone())),
                Attribute::READONLY,
            )
            .property(
                js_string!("name"),
                JsValue::from(js_string!(format!("dropped-{id}"))),
                Attribute::READONLY,
            )
            .build();
        let _ = arr.push(JsValue::from(f), ctx);
    }
    Ok(arr.into())
}

/// `"Text"`/`"text"` → `"text/plain"`, `"URL"` → `"text/uri-list"`,
/// per HTML spec drag normalisation.
fn canonical_format(fmt: &str) -> String {
    match fmt.to_ascii_lowercase().as_str() {
        "text" => "text/plain".to_string(),
        "url" => "text/uri-list".to_string(),
        other => other.to_string(),
    }
}
