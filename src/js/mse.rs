//! Media Source Extensions — `MediaSource` + `SourceBuffer`.
//!
//! Each `new MediaSource()` mints a `MediaSource` state entry. JS
//! calls `URL.createObjectURL(ms)` to get a `blob:mediasource/...`
//! URL, then sets `<video>.src = url`. When the video element wires
//! up, we transition the MediaSource to `open` and fire `sourceopen`.
//!
//! The JS then calls `addSourceBuffer(mime)` to get a SourceBuffer
//! and feeds it `appendBuffer(uint8Array)` segments. We accumulate
//! the bytes per-buffer. `endOfStream()` finalises: we concatenate
//! every appended segment in order and hand the result to the
//! existing [`crate::video::VideoElement`] pipeline so playback
//! proceeds through the same ffmpeg + cpal path as `<video src>`.
//!
//! Out of scope for the toy:
//!   * True streaming playback. We only start ffmpeg on
//!     `endOfStream()`. Live HLS where data arrives forever won't
//!     play until the stream ends.
//!   * `mode = "sequence"` timestamp rewriting (we always use
//!     segments mode and trust the muxer's PTS).
//!   * `SourceBuffer.remove(start, end)` / time-range trimming.
//!   * Multiple SourceBuffers feeding one MediaSource — we
//!     concatenate them in addSourceBuffer order, which only works
//!     for single-stream containers.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, builtins::JsFunction, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::dom::NodeId;

const MS_ID_KEY: &str = "__media_source_id";
const SB_ID_KEY: &str = "__source_buffer_id";

#[derive(Clone)]
pub enum MediaSourceState {
    Closed,
    Open,
    Ended,
}

pub struct MediaSourceEntry {
    pub state: MediaSourceState,
    pub source_buffers: Vec<u32>,
    /// `<video>` node this MediaSource has been attached to (after
    /// the page set `video.src = createObjectURL(ms)`).
    pub attached_video: Option<NodeId>,
    /// JS handle so we can fire `sourceopen` / `sourceended`.
    pub handle: Option<boa_engine::JsObject>,
    /// Concat-on-finalise accumulator. Each entry is one
    /// SourceBuffer's full byte stream.
    pub finalising: bool,
}

pub struct SourceBufferEntry {
    pub mime: String,
    pub bytes: Vec<u8>,
    pub media_source_id: u32,
    pub handle: Option<boa_engine::JsObject>,
}

thread_local! {
    pub(crate) static MEDIA_SOURCES: RefCell<HashMap<u32, MediaSourceEntry>> =
        RefCell::new(HashMap::new());
    pub(crate) static SOURCE_BUFFERS: RefCell<HashMap<u32, SourceBufferEntry>> =
        RefCell::new(HashMap::new());
    pub(crate) static MEDIA_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_id() -> u32 {
    MEDIA_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("MediaSource"),
        0,
        NativeFunction::from_fn_ptr(media_source_ctor),
    )
    .ok();
}

fn media_source_ctor(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = next_id();
    MEDIA_SOURCES.with(|r| {
        r.borrow_mut().insert(
            id,
            MediaSourceEntry {
                state: MediaSourceState::Closed,
                source_buffers: Vec::new(),
                attached_video: None,
                handle: None,
                finalising: false,
            },
        );
    });
    let handle = build_media_source_object(ctx, id);
    if let Some(obj) = handle.as_object() {
        MEDIA_SOURCES.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&id) {
                e.handle = Some(obj.clone());
            }
        });
    }
    Ok(handle)
}

fn build_media_source_object(ctx: &mut Context, ms_id: u32) -> JsValue {
    let realm = ctx.realm().clone();
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(MS_ID_KEY), JsValue::from(ms_id), Attribute::READONLY);
    b.property(
        js_string!("readyState"),
        JsValue::from(js_string!("closed")),
        Attribute::all(),
    );
    b.property(
        js_string!("duration"),
        JsValue::from(f64::NAN),
        Attribute::all(),
    );
    for name in ["onsourceopen", "onsourceended", "onsourceclose"] {
        b.property(js_string!(name), JsValue::null(), Attribute::all());
    }
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("addSourceBuffer", NativeFunction::from_fn_ptr(ms_add_source_buffer), 1),
        ("removeSourceBuffer", NativeFunction::from_fn_ptr(ms_remove_source_buffer), 1),
        ("endOfStream", NativeFunction::from_fn_ptr(ms_end_of_stream), 1),
        ("clearLiveSeekableRange", NativeFunction::from_fn_ptr(noop), 0),
        ("setLiveSeekableRange", NativeFunction::from_fn_ptr(noop), 2),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    let handle = b.build();
    // Live accessors for collections.
    let sb_getter = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(ms_get_source_buffers),
    )
    .build();
    let asb_getter = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(ms_get_active_source_buffers),
    )
    .build();
    let _ = handle.define_property_or_throw(
        js_string!("sourceBuffers"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(sb_getter)
            .enumerable(true)
            .configurable(true),
        ctx,
    );
    let _ = handle.define_property_or_throw(
        js_string!("activeSourceBuffers"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(asb_getter)
            .enumerable(true)
            .configurable(true),
        ctx,
    );
    JsValue::from(handle)
}

fn noop(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn ms_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(MS_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn ms_add_source_buffer(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(ms_id) = ms_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let mime = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let sb_id = next_id();
    SOURCE_BUFFERS.with(|r| {
        r.borrow_mut().insert(
            sb_id,
            SourceBufferEntry {
                mime,
                bytes: Vec::new(),
                media_source_id: ms_id,
                handle: None,
            },
        );
    });
    MEDIA_SOURCES.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&ms_id) {
            e.source_buffers.push(sb_id);
        }
    });
    let handle = build_source_buffer_object(ctx, sb_id);
    if let Some(obj) = handle.as_object() {
        SOURCE_BUFFERS.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&sb_id) {
                e.handle = Some(obj.clone());
            }
        });
    }
    Ok(handle)
}

fn ms_remove_source_buffer(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(ms_id) = ms_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(sb_id) = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!(SB_ID_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
    else {
        return Ok(JsValue::undefined());
    };
    MEDIA_SOURCES.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&ms_id) {
            e.source_buffers.retain(|id| *id != sb_id);
        }
    });
    SOURCE_BUFFERS.with(|r| {
        r.borrow_mut().remove(&sb_id);
    });
    Ok(JsValue::undefined())
}

fn ms_end_of_stream(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(ms_id) = ms_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    // Concatenate every SourceBuffer's bytes in addSourceBuffer
    // order, then kick off ffmpeg decode against the result.
    let (sb_ids, handle, video_node) = MEDIA_SOURCES.with(|r| {
        let mut map = r.borrow_mut();
        if let Some(e) = map.get_mut(&ms_id) {
            e.state = MediaSourceState::Ended;
            e.finalising = true;
            (
                e.source_buffers.clone(),
                e.handle.clone(),
                e.attached_video,
            )
        } else {
            (Vec::new(), None, None)
        }
    });
    let mut combined = Vec::new();
    SOURCE_BUFFERS.with(|r| {
        let map = r.borrow();
        for id in &sb_ids {
            if let Some(sb) = map.get(id) {
                combined.extend_from_slice(&sb.bytes);
            }
        }
    });
    // Snapshot the existing readyState so the JS visible property
    // flips to "ended".
    if let Some(obj) = handle.as_ref() {
        let _ = obj.set(
            js_string!("readyState"),
            JsValue::from(js_string!("ended")),
            false,
            ctx,
        );
        fire_handler(obj, "onsourceended", ctx);
    }
    if let Some(node) = video_node {
        if !combined.is_empty() {
            // Hand off to the standard VideoElement pipeline via the
            // engine's video registry.
            crate::js::engine::JS_VIDEO_ELEMENTS.with(|slot| {
                if let Some(rc) = slot.borrow().as_ref() {
                    if let Some(video) = crate::video::VideoElement::from_bytes(
                        combined.clone(),
                        true,
                        false,
                    ) {
                        rc.borrow_mut().insert(node, video);
                    }
                }
            });
        }
    }
    Ok(JsValue::undefined())
}

fn ms_get_source_buffers(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(ms_id) = ms_id_of(this, ctx) else {
        return Ok(JsArray::new(ctx).into());
    };
    let sb_ids: Vec<u32> = MEDIA_SOURCES
        .with(|r| r.borrow().get(&ms_id).map(|e| e.source_buffers.clone()))
        .unwrap_or_default();
    let arr = JsArray::new(ctx);
    for id in sb_ids {
        let handle = SOURCE_BUFFERS
            .with(|r| r.borrow().get(&id).and_then(|e| e.handle.clone()));
        if let Some(h) = handle {
            let _ = arr.push(JsValue::from(h), ctx);
        }
    }
    Ok(arr.into())
}

fn ms_get_active_source_buffers(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    ms_get_source_buffers(this, args, ctx)
}

// ============ SourceBuffer ============

fn build_source_buffer_object(ctx: &mut Context, sb_id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(SB_ID_KEY), JsValue::from(sb_id), Attribute::READONLY);
    b.property(js_string!("updating"), JsValue::from(false), Attribute::all());
    b.property(
        js_string!("mode"),
        JsValue::from(js_string!("segments")),
        Attribute::all(),
    );
    b.property(
        js_string!("timestampOffset"),
        JsValue::from(0.0_f64),
        Attribute::all(),
    );
    b.property(
        js_string!("appendWindowStart"),
        JsValue::from(0.0_f64),
        Attribute::all(),
    );
    b.property(
        js_string!("appendWindowEnd"),
        JsValue::from(f64::INFINITY),
        Attribute::all(),
    );
    for name in ["onupdate", "onupdatestart", "onupdateend", "onerror", "onabort"] {
        b.property(js_string!(name), JsValue::null(), Attribute::all());
    }
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("appendBuffer", NativeFunction::from_fn_ptr(sb_append_buffer), 1),
        ("abort", NativeFunction::from_fn_ptr(sb_abort), 0),
        ("remove", NativeFunction::from_fn_ptr(sb_remove), 2),
        ("changeType", NativeFunction::from_fn_ptr(sb_change_type), 1),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    JsValue::from(b.build())
}

fn sb_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(SB_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn sb_append_buffer(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(sb_id) = sb_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let bytes = args
        .first()
        .map(|v| read_bytes(v, ctx))
        .unwrap_or_default();
    SOURCE_BUFFERS.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&sb_id) {
            e.bytes.extend_from_slice(&bytes);
        }
    });
    // Fire updateend synchronously. Real spec queues this on the
    // microtask queue; tests/libraries typically tolerate either.
    if let Some(obj) = this.as_object() {
        let _ = obj.set(js_string!("updating"), JsValue::from(false), false, ctx);
        fire_handler(obj, "onupdate", ctx);
        fire_handler(obj, "onupdateend", ctx);
    }
    Ok(JsValue::undefined())
}

fn sb_abort(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn sb_remove(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn sb_change_type(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(sb_id) = sb_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let mime = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    SOURCE_BUFFERS.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&sb_id) {
            e.mime = mime;
        }
    });
    Ok(JsValue::undefined())
}

// ============ attach: called when <video>.src = blob:mediasource/... ============

/// `URL.createObjectURL(mediaSource)` calls this to mint a unique
/// URL pointing at this MediaSource. The format is
/// `blob:mediasource/<id>` so the video element setter can detect
/// it without a separate registry lookup.
pub fn object_url_for(ms_obj: &JsValue, ctx: &mut Context) -> Option<String> {
    let id = ms_obj
        .as_object()
        .and_then(|o| o.get(js_string!(MS_ID_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())?;
    Some(format!("blob:mediasource/{id}"))
}

/// Called from the `<video>` src handling path when the URL matches
/// a MediaSource. Flips the MediaSource to `open` and fires
/// `sourceopen`. Returns true if the URL refers to a known
/// MediaSource so the caller can skip its normal HTTP-fetch path.
pub fn try_attach(url: &str, node: NodeId, ctx: &mut Context) -> bool {
    let id = match url.strip_prefix("blob:mediasource/") {
        Some(s) => match s.parse::<u32>() {
            Ok(n) => n,
            Err(_) => return false,
        },
        None => return false,
    };
    let handle = MEDIA_SOURCES.with(|r| {
        let mut map = r.borrow_mut();
        let entry = match map.get_mut(&id) {
            Some(e) => e,
            None => return None,
        };
        entry.state = MediaSourceState::Open;
        entry.attached_video = Some(node);
        entry.handle.clone()
    });
    if let Some(obj) = handle {
        let _ = obj.set(
            js_string!("readyState"),
            JsValue::from(js_string!("open")),
            false,
            ctx,
        );
        fire_handler(&obj, "onsourceopen", ctx);
    }
    true
}

fn fire_handler(obj: &boa_engine::JsObject, name: &str, ctx: &mut Context) {
    let Ok(v) = obj.get(js_string!(name.to_string()), ctx) else {
        return;
    };
    let Some(handler_obj) = v.as_object() else {
        return;
    };
    let Some(f) = JsFunction::from_object(handler_obj.clone()) else {
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
            JsValue::from(obj.clone()),
            Attribute::READONLY,
        )
        .build();
    let _ = f.call(&JsValue::from(obj.clone()), &[JsValue::from(event)], ctx);
}

fn read_bytes(val: &JsValue, ctx: &mut Context) -> Vec<u8> {
    use boa_engine::object::builtins::{JsArrayBuffer, JsUint8Array};
    let Some(obj) = val.as_object() else {
        return Vec::new();
    };
    if let Ok(u8a) = JsUint8Array::from_object(obj.clone()) {
        let len = u8a.length(ctx).unwrap_or(0);
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            if let Ok(v) = u8a.at(i as i64, ctx) {
                if let Ok(n) = v.to_u32(ctx) {
                    out.push(n as u8);
                }
            }
        }
        return out;
    }
    if let Ok(ab) = JsArrayBuffer::from_object(obj.clone()) {
        let len = ab.byte_length();
        let view = match JsUint8Array::from_array_buffer(ab, ctx) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            if let Ok(v) = view.at(i as i64, ctx) {
                if let Ok(n) = v.to_u32(ctx) {
                    out.push(n as u8);
                }
            }
        }
        return out;
    }
    Vec::new()
}
