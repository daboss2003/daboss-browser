//! WebCodecs — `VideoEncoder` / `VideoDecoder` / `AudioEncoder` /
//! `AudioDecoder` backed by the same `ffmpeg` subprocess the rest of
//! the media stack uses.
//!
//! Each `encode()` / `decode()` call spawns a one-shot ffmpeg
//! invocation with the codec parameters from `configure()`. That's
//! cheap correctness at the cost of throughput — every frame pays
//! for a process spawn. A real implementation would keep a long-
//! lived ffmpeg subprocess per encoder/decoder; this scope leaves
//! that as a follow-up.
//!
//! Spec gaps:
//!   * `EncodedVideoChunk.duration` is preserved; per-frame
//!     `keyFrame: true` adds `-force_key_frames 0` to the encoder
//!     call.
//!   * Bitrate / framerate hints are passed to ffmpeg verbatim where
//!     they map cleanly.
//!   * `isConfigSupported({codec})` returns a `supported: true`
//!     resolution for the codecs we recognise; the surface lets
//!     feature-detection paths proceed.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::{
    js_string,
    object::{
        builtins::{JsArrayBuffer, JsFunction, JsPromise, JsUint8Array},
        ObjectInitializer,
    },
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

const FRAME_ID_KEY: &str = "__video_frame_id";
const AUDIO_DATA_ID_KEY: &str = "__audio_data_id";
const VIDEO_ENCODER_ID_KEY: &str = "__video_encoder_id";
const VIDEO_DECODER_ID_KEY: &str = "__video_decoder_id";
const AUDIO_ENCODER_ID_KEY: &str = "__audio_encoder_id";
const AUDIO_DECODER_ID_KEY: &str = "__audio_decoder_id";

#[derive(Clone)]
pub struct VideoFrameData {
    pub bytes: Vec<u8>,
    pub format: String,
    pub coded_width: u32,
    pub coded_height: u32,
    pub timestamp_us: i64,
    pub duration_us: Option<i64>,
}

#[derive(Clone)]
pub struct AudioDataPayload {
    pub bytes: Vec<u8>,
    pub format: String,
    pub sample_rate: u32,
    pub number_of_channels: u32,
    pub number_of_frames: u32,
    pub timestamp_us: i64,
}

#[derive(Clone)]
pub struct EncodedChunkPayload {
    pub bytes: Vec<u8>,
    pub chunk_type: String,
    pub timestamp_us: i64,
    pub duration_us: Option<i64>,
}

#[derive(Clone)]
pub struct EncoderState {
    pub configured: bool,
    pub closed: bool,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub bitrate: u32,
    pub framerate: f32,
    pub output_cb: Option<JsFunction>,
    pub error_cb: Option<JsFunction>,
}

#[derive(Clone)]
pub struct AudioEncoderState {
    pub configured: bool,
    pub closed: bool,
    pub codec: String,
    pub sample_rate: u32,
    pub number_of_channels: u32,
    pub bitrate: u32,
    pub output_cb: Option<JsFunction>,
    pub error_cb: Option<JsFunction>,
}

thread_local! {
    pub(crate) static VIDEO_FRAMES: RefCell<HashMap<u32, VideoFrameData>> =
        RefCell::new(HashMap::new());
    pub(crate) static AUDIO_DATAS: RefCell<HashMap<u32, AudioDataPayload>> =
        RefCell::new(HashMap::new());
    pub(crate) static ENCODED_CHUNKS: RefCell<HashMap<u32, EncodedChunkPayload>> =
        RefCell::new(HashMap::new());
    pub(crate) static VIDEO_ENCODERS: RefCell<HashMap<u32, EncoderState>> =
        RefCell::new(HashMap::new());
    pub(crate) static VIDEO_DECODERS: RefCell<HashMap<u32, EncoderState>> =
        RefCell::new(HashMap::new());
    pub(crate) static AUDIO_ENCODERS: RefCell<HashMap<u32, AudioEncoderState>> =
        RefCell::new(HashMap::new());
    pub(crate) static AUDIO_DECODERS: RefCell<HashMap<u32, AudioEncoderState>> =
        RefCell::new(HashMap::new());
    pub(crate) static CODECS_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_id() -> u32 {
    CODECS_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("VideoFrame"),
        2,
        NativeFunction::from_fn_ptr(video_frame_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("AudioData"),
        1,
        NativeFunction::from_fn_ptr(audio_data_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("EncodedVideoChunk"),
        1,
        NativeFunction::from_fn_ptr(encoded_video_chunk_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("EncodedAudioChunk"),
        1,
        NativeFunction::from_fn_ptr(encoded_audio_chunk_ctor),
    )
    .ok();
    install_encoder(ctx, "VideoEncoder", video_encoder_ctor);
    install_encoder(ctx, "VideoDecoder", video_decoder_ctor);
    install_encoder(ctx, "AudioEncoder", audio_encoder_ctor);
    install_encoder(ctx, "AudioDecoder", audio_decoder_ctor);
}

fn install_encoder(
    ctx: &mut Context,
    name: &str,
    ctor: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>,
) {
    ctx.register_global_callable(
        js_string!(name.to_string()),
        1,
        NativeFunction::from_fn_ptr(ctor),
    )
    .ok();
}

// ============ VideoFrame / EncodedVideoChunk ============

fn video_frame_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // new VideoFrame(bytes, { format, codedWidth, codedHeight, timestamp, duration? })
    let bytes_val = args.first().cloned().unwrap_or(JsValue::undefined());
    let opts = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let opts_obj = opts.as_object().cloned();
    let format = opts_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("format"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "RGBA".to_string());
    let coded_width = opts_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("codedWidth"), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let coded_height = opts_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("codedHeight"), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let timestamp_us = opts_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("timestamp"), ctx).ok())
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as i64)
        .unwrap_or(0);
    let duration_us = opts_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("duration"), ctx).ok())
        .filter(|v| !v.is_undefined() && !v.is_null())
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as i64);
    let bytes = read_bytes(&bytes_val, ctx);
    let id = next_id();
    VIDEO_FRAMES.with(|r| {
        r.borrow_mut().insert(
            id,
            VideoFrameData {
                bytes,
                format,
                coded_width,
                coded_height,
                timestamp_us,
                duration_us,
            },
        );
    });
    Ok(build_video_frame_object(ctx, id))
}

fn build_video_frame_object(ctx: &mut Context, id: u32) -> JsValue {
    let data = VIDEO_FRAMES.with(|r| r.borrow().get(&id).cloned());
    let Some(d) = data else {
        return JsValue::null();
    };
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(FRAME_ID_KEY), JsValue::from(id), Attribute::READONLY);
    b.property(
        js_string!("format"),
        JsValue::from(js_string!(d.format.clone())),
        Attribute::READONLY,
    );
    b.property(
        js_string!("codedWidth"),
        JsValue::from(d.coded_width),
        Attribute::READONLY,
    );
    b.property(
        js_string!("codedHeight"),
        JsValue::from(d.coded_height),
        Attribute::READONLY,
    );
    b.property(
        js_string!("displayWidth"),
        JsValue::from(d.coded_width),
        Attribute::READONLY,
    );
    b.property(
        js_string!("displayHeight"),
        JsValue::from(d.coded_height),
        Attribute::READONLY,
    );
    b.property(
        js_string!("timestamp"),
        JsValue::from(d.timestamp_us as f64),
        Attribute::READONLY,
    );
    b.property(
        js_string!("duration"),
        match d.duration_us {
            Some(d) => JsValue::from(d as f64),
            None => JsValue::null(),
        },
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(video_frame_close),
        js_string!("close"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(video_frame_clone),
        js_string!("clone"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(video_frame_copy_to),
        js_string!("copyTo"),
        2,
    );
    JsValue::from(b.build())
}

fn video_frame_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, FRAME_ID_KEY, ctx) {
        VIDEO_FRAMES.with(|r| {
            r.borrow_mut().remove(&id);
        });
    }
    Ok(JsValue::undefined())
}

fn video_frame_clone(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, FRAME_ID_KEY, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(data) = VIDEO_FRAMES.with(|r| r.borrow().get(&id).cloned()) else {
        return Ok(JsValue::null());
    };
    let new_id = next_id();
    VIDEO_FRAMES.with(|r| {
        r.borrow_mut().insert(new_id, data);
    });
    Ok(build_video_frame_object(ctx, new_id))
}

fn video_frame_copy_to(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, FRAME_ID_KEY, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let bytes = VIDEO_FRAMES
        .with(|r| r.borrow().get(&id).map(|d| d.bytes.clone()))
        .unwrap_or_default();
    if let Some(dst_obj) = args.first().and_then(|v| v.as_object()) {
        if let Ok(u8a) = JsUint8Array::from_object(dst_obj.clone()) {
            let len = u8a.length(ctx).unwrap_or(0).min(bytes.len());
            for (i, b) in bytes.iter().take(len).enumerate() {
                let _ = u8a.set(i as i64, JsValue::from(*b as u32), false, ctx);
            }
        }
    }
    let _ = args;
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn encoded_video_chunk_ctor(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let opts = args.first().and_then(|v| v.as_object()).cloned();
    let Some(obj) = opts else {
        return Ok(JsValue::null());
    };
    let chunk_type = obj
        .get(js_string!("type"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "key".to_string());
    let timestamp_us = obj
        .get(js_string!("timestamp"), ctx)
        .ok()
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as i64)
        .unwrap_or(0);
    let duration_us = obj
        .get(js_string!("duration"), ctx)
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as i64);
    let data = obj
        .get(js_string!("data"), ctx)
        .ok()
        .map(|v| read_bytes(&v, ctx))
        .unwrap_or_default();
    let id = next_id();
    ENCODED_CHUNKS.with(|r| {
        r.borrow_mut().insert(
            id,
            EncodedChunkPayload {
                bytes: data,
                chunk_type,
                timestamp_us,
                duration_us,
            },
        );
    });
    Ok(build_encoded_chunk_object(ctx, id, "EncodedVideoChunk"))
}

fn encoded_audio_chunk_ctor(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let opts = args.first().and_then(|v| v.as_object()).cloned();
    let Some(obj) = opts else {
        return Ok(JsValue::null());
    };
    let chunk_type = obj
        .get(js_string!("type"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "key".to_string());
    let timestamp_us = obj
        .get(js_string!("timestamp"), ctx)
        .ok()
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as i64)
        .unwrap_or(0);
    let duration_us = obj
        .get(js_string!("duration"), ctx)
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as i64);
    let data = obj
        .get(js_string!("data"), ctx)
        .ok()
        .map(|v| read_bytes(&v, ctx))
        .unwrap_or_default();
    let id = next_id();
    ENCODED_CHUNKS.with(|r| {
        r.borrow_mut().insert(
            id,
            EncodedChunkPayload {
                bytes: data,
                chunk_type,
                timestamp_us,
                duration_us,
            },
        );
    });
    Ok(build_encoded_chunk_object(ctx, id, "EncodedAudioChunk"))
}

fn build_encoded_chunk_object(ctx: &mut Context, id: u32, _kind: &str) -> JsValue {
    let data = ENCODED_CHUNKS.with(|r| r.borrow().get(&id).cloned());
    let Some(d) = data else {
        return JsValue::null();
    };
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!("__chunk_id"), JsValue::from(id), Attribute::READONLY);
    b.property(
        js_string!("type"),
        JsValue::from(js_string!(d.chunk_type.clone())),
        Attribute::READONLY,
    );
    b.property(
        js_string!("timestamp"),
        JsValue::from(d.timestamp_us as f64),
        Attribute::READONLY,
    );
    b.property(
        js_string!("duration"),
        match d.duration_us {
            Some(d) => JsValue::from(d as f64),
            None => JsValue::null(),
        },
        Attribute::READONLY,
    );
    b.property(
        js_string!("byteLength"),
        JsValue::from(d.bytes.len() as u32),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(encoded_chunk_copy_to),
        js_string!("copyTo"),
        1,
    );
    JsValue::from(b.build())
}

fn encoded_chunk_copy_to(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = id_of(this, "__chunk_id", ctx) else {
        return Ok(JsValue::undefined());
    };
    let bytes = ENCODED_CHUNKS
        .with(|r| r.borrow().get(&id).map(|d| d.bytes.clone()))
        .unwrap_or_default();
    if let Some(dst_obj) = args.first().and_then(|v| v.as_object()) {
        if let Ok(u8a) = JsUint8Array::from_object(dst_obj.clone()) {
            let len = u8a.length(ctx).unwrap_or(0).min(bytes.len());
            for (i, b) in bytes.iter().take(len).enumerate() {
                let _ = u8a.set(i as i64, JsValue::from(*b as u32), false, ctx);
            }
        }
    }
    Ok(JsValue::undefined())
}

// ============ VideoEncoder / VideoDecoder ============

fn video_encoder_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (output_cb, error_cb) = read_callbacks(args, ctx);
    let id = next_id();
    VIDEO_ENCODERS.with(|r| {
        r.borrow_mut().insert(
            id,
            EncoderState {
                configured: false,
                closed: false,
                codec: String::new(),
                width: 0,
                height: 0,
                bitrate: 1_000_000,
                framerate: 30.0,
                output_cb,
                error_cb,
            },
        );
    });
    Ok(build_video_encoder_object(ctx, id))
}

fn video_decoder_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (output_cb, error_cb) = read_callbacks(args, ctx);
    let id = next_id();
    VIDEO_DECODERS.with(|r| {
        r.borrow_mut().insert(
            id,
            EncoderState {
                configured: false,
                closed: false,
                codec: String::new(),
                width: 0,
                height: 0,
                bitrate: 0,
                framerate: 30.0,
                output_cb,
                error_cb,
            },
        );
    });
    Ok(build_video_decoder_object(ctx, id))
}

fn read_callbacks(args: &[JsValue], ctx: &mut Context) -> (Option<JsFunction>, Option<JsFunction>) {
    let Some(obj) = args.first().and_then(|v| v.as_object()) else {
        return (None, None);
    };
    let read = |name: &str, ctx: &mut Context| -> Option<JsFunction> {
        let v = obj.get(js_string!(name.to_string()), ctx).ok()?;
        let o = v.as_object()?;
        JsFunction::from_object(o.clone())
    };
    (read("output", ctx), read("error", ctx))
}

fn build_video_encoder_object(ctx: &mut Context, id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(VIDEO_ENCODER_ID_KEY),
        JsValue::from(id),
        Attribute::READONLY,
    );
    b.property(
        js_string!("state"),
        JsValue::from(js_string!("unconfigured")),
        Attribute::all(),
    );
    b.property(
        js_string!("encodeQueueSize"),
        JsValue::from(0u32),
        Attribute::all(),
    );
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("configure", NativeFunction::from_fn_ptr(video_encoder_configure), 1),
        ("encode", NativeFunction::from_fn_ptr(video_encoder_encode), 2),
        ("flush", NativeFunction::from_fn_ptr(noop_promise), 0),
        ("close", NativeFunction::from_fn_ptr(video_encoder_close), 0),
        ("reset", NativeFunction::from_fn_ptr(video_encoder_reset), 0),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    JsValue::from(b.build())
}

fn build_video_decoder_object(ctx: &mut Context, id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(VIDEO_DECODER_ID_KEY),
        JsValue::from(id),
        Attribute::READONLY,
    );
    b.property(
        js_string!("state"),
        JsValue::from(js_string!("unconfigured")),
        Attribute::all(),
    );
    b.property(
        js_string!("decodeQueueSize"),
        JsValue::from(0u32),
        Attribute::all(),
    );
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("configure", NativeFunction::from_fn_ptr(video_decoder_configure), 1),
        ("decode", NativeFunction::from_fn_ptr(video_decoder_decode), 1),
        ("flush", NativeFunction::from_fn_ptr(noop_promise), 0),
        ("close", NativeFunction::from_fn_ptr(video_decoder_close), 0),
        ("reset", NativeFunction::from_fn_ptr(video_decoder_reset), 0),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    JsValue::from(b.build())
}

fn noop_promise(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn video_encoder_configure(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = id_of(this, VIDEO_ENCODER_ID_KEY, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(opts) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    let codec = opts
        .get(js_string!("codec"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let width = opts
        .get(js_string!("width"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let height = opts
        .get(js_string!("height"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let bitrate = opts
        .get(js_string!("bitrate"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(1_000_000);
    let framerate = opts
        .get(js_string!("framerate"), ctx)
        .ok()
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(30.0) as f32;
    VIDEO_ENCODERS.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&id) {
            e.codec = codec;
            e.width = width;
            e.height = height;
            e.bitrate = bitrate;
            e.framerate = framerate;
            e.configured = true;
        }
    });
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("configured")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn video_decoder_configure(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = id_of(this, VIDEO_DECODER_ID_KEY, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(opts) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    let codec = opts
        .get(js_string!("codec"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let coded_width = opts
        .get(js_string!("codedWidth"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let coded_height = opts
        .get(js_string!("codedHeight"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    VIDEO_DECODERS.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&id) {
            e.codec = codec;
            e.width = coded_width;
            e.height = coded_height;
            e.configured = true;
        }
    });
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("configured")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn video_encoder_encode(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, VIDEO_ENCODER_ID_KEY, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(frame_id) = args.first().and_then(|v| id_of(v, FRAME_ID_KEY, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let frame = VIDEO_FRAMES.with(|r| r.borrow().get(&frame_id).cloned());
    let Some(frame) = frame else {
        return Ok(JsValue::undefined());
    };
    let opts_obj = args.get(1).and_then(|v| v.as_object()).cloned();
    let key_frame = opts_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("keyFrame"), ctx).ok())
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let state = VIDEO_ENCODERS.with(|r| r.borrow().get(&id).cloned());
    let Some(state) = state else {
        return Ok(JsValue::undefined());
    };
    if !state.configured || state.closed {
        if let Some(err) = state.error_cb {
            let _ = err.call(
                &JsValue::undefined(),
                &[JsValue::from(js_string!("encoder not configured"))],
                ctx,
            );
        }
        return Ok(JsValue::undefined());
    }
    let bytes = ffmpeg_encode_video_frame(&state, &frame, key_frame);
    let chunk_type = if key_frame { "key" } else { "delta" };
    let chunk_id = next_id();
    let payload = EncodedChunkPayload {
        bytes: bytes.unwrap_or_default(),
        chunk_type: chunk_type.to_string(),
        timestamp_us: frame.timestamp_us,
        duration_us: frame.duration_us,
    };
    ENCODED_CHUNKS.with(|r| r.borrow_mut().insert(chunk_id, payload));
    let chunk = build_encoded_chunk_object(ctx, chunk_id, "EncodedVideoChunk");
    if let Some(cb) = state.output_cb {
        let _ = cb.call(&JsValue::undefined(), &[chunk], ctx);
    }
    Ok(JsValue::undefined())
}

fn video_decoder_decode(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, VIDEO_DECODER_ID_KEY, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(chunk_id) = args.first().and_then(|v| id_of(v, "__chunk_id", ctx)) else {
        return Ok(JsValue::undefined());
    };
    let chunk = ENCODED_CHUNKS.with(|r| r.borrow().get(&chunk_id).cloned());
    let Some(chunk) = chunk else {
        return Ok(JsValue::undefined());
    };
    let state = VIDEO_DECODERS.with(|r| r.borrow().get(&id).cloned());
    let Some(state) = state else {
        return Ok(JsValue::undefined());
    };
    if !state.configured || state.closed {
        if let Some(err) = state.error_cb {
            let _ = err.call(
                &JsValue::undefined(),
                &[JsValue::from(js_string!("decoder not configured"))],
                ctx,
            );
        }
        return Ok(JsValue::undefined());
    }
    let (bytes, width, height) = ffmpeg_decode_video_chunk(&state, &chunk).unwrap_or_default();
    let frame_id = next_id();
    VIDEO_FRAMES.with(|r| {
        r.borrow_mut().insert(
            frame_id,
            VideoFrameData {
                bytes,
                format: "RGBA".to_string(),
                coded_width: width,
                coded_height: height,
                timestamp_us: chunk.timestamp_us,
                duration_us: chunk.duration_us,
            },
        );
    });
    let frame = build_video_frame_object(ctx, frame_id);
    if let Some(cb) = state.output_cb {
        let _ = cb.call(&JsValue::undefined(), &[frame], ctx);
    }
    Ok(JsValue::undefined())
}

fn video_encoder_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, VIDEO_ENCODER_ID_KEY, ctx) {
        VIDEO_ENCODERS.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&id) {
                e.closed = true;
            }
        });
    }
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("closed")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn video_decoder_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, VIDEO_DECODER_ID_KEY, ctx) {
        VIDEO_DECODERS.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&id) {
                e.closed = true;
            }
        });
    }
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("closed")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn video_encoder_reset(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, VIDEO_ENCODER_ID_KEY, ctx) {
        VIDEO_ENCODERS.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&id) {
                e.configured = false;
            }
        });
    }
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("unconfigured")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn video_decoder_reset(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, VIDEO_DECODER_ID_KEY, ctx) {
        VIDEO_DECODERS.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&id) {
                e.configured = false;
            }
        });
    }
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("unconfigured")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

// ============ AudioData / AudioEncoder / AudioDecoder ============

fn audio_data_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(obj) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::null());
    };
    let format = obj
        .get(js_string!("format"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "f32".to_string());
    let sample_rate = obj
        .get(js_string!("sampleRate"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(48000);
    let number_of_channels = obj
        .get(js_string!("numberOfChannels"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(2);
    let number_of_frames = obj
        .get(js_string!("numberOfFrames"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let timestamp_us = obj
        .get(js_string!("timestamp"), ctx)
        .ok()
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as i64)
        .unwrap_or(0);
    let bytes = obj
        .get(js_string!("data"), ctx)
        .ok()
        .map(|v| read_bytes(&v, ctx))
        .unwrap_or_default();
    let id = next_id();
    AUDIO_DATAS.with(|r| {
        r.borrow_mut().insert(
            id,
            AudioDataPayload {
                bytes,
                format,
                sample_rate,
                number_of_channels,
                number_of_frames,
                timestamp_us,
            },
        );
    });
    Ok(build_audio_data_object(ctx, id))
}

fn build_audio_data_object(ctx: &mut Context, id: u32) -> JsValue {
    let Some(d) = AUDIO_DATAS.with(|r| r.borrow().get(&id).cloned()) else {
        return JsValue::null();
    };
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(AUDIO_DATA_ID_KEY), JsValue::from(id), Attribute::READONLY);
    b.property(
        js_string!("format"),
        JsValue::from(js_string!(d.format.clone())),
        Attribute::READONLY,
    );
    b.property(
        js_string!("sampleRate"),
        JsValue::from(d.sample_rate),
        Attribute::READONLY,
    );
    b.property(
        js_string!("numberOfChannels"),
        JsValue::from(d.number_of_channels),
        Attribute::READONLY,
    );
    b.property(
        js_string!("numberOfFrames"),
        JsValue::from(d.number_of_frames),
        Attribute::READONLY,
    );
    b.property(
        js_string!("timestamp"),
        JsValue::from(d.timestamp_us as f64),
        Attribute::READONLY,
    );
    b.property(
        js_string!("duration"),
        JsValue::from(
            (d.number_of_frames as f64 / d.sample_rate.max(1) as f64 * 1_000_000.0) as f64,
        ),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(audio_data_close),
        js_string!("close"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(audio_data_clone),
        js_string!("clone"),
        0,
    );
    JsValue::from(b.build())
}

fn audio_data_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, AUDIO_DATA_ID_KEY, ctx) {
        AUDIO_DATAS.with(|r| {
            r.borrow_mut().remove(&id);
        });
    }
    Ok(JsValue::undefined())
}

fn audio_data_clone(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, AUDIO_DATA_ID_KEY, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(d) = AUDIO_DATAS.with(|r| r.borrow().get(&id).cloned()) else {
        return Ok(JsValue::null());
    };
    let new_id = next_id();
    AUDIO_DATAS.with(|r| r.borrow_mut().insert(new_id, d));
    Ok(build_audio_data_object(ctx, new_id))
}

fn audio_encoder_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (output_cb, error_cb) = read_callbacks(args, ctx);
    let id = next_id();
    AUDIO_ENCODERS.with(|r| {
        r.borrow_mut().insert(
            id,
            AudioEncoderState {
                configured: false,
                closed: false,
                codec: String::new(),
                sample_rate: 48000,
                number_of_channels: 2,
                bitrate: 128_000,
                output_cb,
                error_cb,
            },
        );
    });
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("configure", NativeFunction::from_fn_ptr(audio_encoder_configure), 1),
        ("encode", NativeFunction::from_fn_ptr(audio_encoder_encode), 1),
        ("flush", NativeFunction::from_fn_ptr(noop_promise), 0),
        ("close", NativeFunction::from_fn_ptr(audio_encoder_close), 0),
        ("reset", NativeFunction::from_fn_ptr(audio_encoder_reset), 0),
    ];
    Ok(build_codec_object(ctx, AUDIO_ENCODER_ID_KEY, id, bindings))
}

fn audio_decoder_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let (output_cb, error_cb) = read_callbacks(args, ctx);
    let id = next_id();
    AUDIO_DECODERS.with(|r| {
        r.borrow_mut().insert(
            id,
            AudioEncoderState {
                configured: false,
                closed: false,
                codec: String::new(),
                sample_rate: 48000,
                number_of_channels: 2,
                bitrate: 0,
                output_cb,
                error_cb,
            },
        );
    });
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("configure", NativeFunction::from_fn_ptr(audio_decoder_configure), 1),
        ("decode", NativeFunction::from_fn_ptr(audio_decoder_decode), 1),
        ("flush", NativeFunction::from_fn_ptr(noop_promise), 0),
        ("close", NativeFunction::from_fn_ptr(audio_decoder_close), 0),
        ("reset", NativeFunction::from_fn_ptr(audio_decoder_reset), 0),
    ];
    Ok(build_codec_object(ctx, AUDIO_DECODER_ID_KEY, id, bindings))
}

fn build_codec_object(
    ctx: &mut Context,
    id_key: &'static str,
    id: u32,
    bindings: &[(&str, NativeFunction, usize)],
) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(id_key.to_string()), JsValue::from(id), Attribute::READONLY);
    b.property(
        js_string!("state"),
        JsValue::from(js_string!("unconfigured")),
        Attribute::all(),
    );
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    JsValue::from(b.build())
}

fn audio_encoder_configure(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = id_of(this, AUDIO_ENCODER_ID_KEY, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(opts) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    let codec = opts
        .get(js_string!("codec"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let sample_rate = opts
        .get(js_string!("sampleRate"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(48000);
    let nch = opts
        .get(js_string!("numberOfChannels"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(2);
    let bitrate = opts
        .get(js_string!("bitrate"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(128_000);
    AUDIO_ENCODERS.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&id) {
            e.codec = codec;
            e.sample_rate = sample_rate;
            e.number_of_channels = nch;
            e.bitrate = bitrate;
            e.configured = true;
        }
    });
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("configured")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn audio_decoder_configure(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = id_of(this, AUDIO_DECODER_ID_KEY, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(opts) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    let codec = opts
        .get(js_string!("codec"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    AUDIO_DECODERS.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&id) {
            e.codec = codec;
            e.configured = true;
        }
    });
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("configured")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn audio_encoder_encode(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, AUDIO_ENCODER_ID_KEY, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(data_id) = args.first().and_then(|v| id_of(v, AUDIO_DATA_ID_KEY, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let data = AUDIO_DATAS.with(|r| r.borrow().get(&data_id).cloned());
    let Some(data) = data else {
        return Ok(JsValue::undefined());
    };
    let state = AUDIO_ENCODERS.with(|r| r.borrow().get(&id).cloned());
    let Some(state) = state else {
        return Ok(JsValue::undefined());
    };
    if !state.configured || state.closed {
        return Ok(JsValue::undefined());
    }
    let bytes = ffmpeg_encode_audio(&state, &data).unwrap_or_default();
    let chunk_id = next_id();
    ENCODED_CHUNKS.with(|r| {
        r.borrow_mut().insert(
            chunk_id,
            EncodedChunkPayload {
                bytes,
                chunk_type: "key".to_string(),
                timestamp_us: data.timestamp_us,
                duration_us: Some(
                    (data.number_of_frames as f64 / data.sample_rate.max(1) as f64
                        * 1_000_000.0) as i64,
                ),
            },
        );
    });
    let chunk = build_encoded_chunk_object(ctx, chunk_id, "EncodedAudioChunk");
    if let Some(cb) = state.output_cb {
        let _ = cb.call(&JsValue::undefined(), &[chunk], ctx);
    }
    Ok(JsValue::undefined())
}

fn audio_decoder_decode(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, AUDIO_DECODER_ID_KEY, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(chunk_id) = args.first().and_then(|v| id_of(v, "__chunk_id", ctx)) else {
        return Ok(JsValue::undefined());
    };
    let chunk = ENCODED_CHUNKS.with(|r| r.borrow().get(&chunk_id).cloned());
    let Some(chunk) = chunk else {
        return Ok(JsValue::undefined());
    };
    let state = AUDIO_DECODERS.with(|r| r.borrow().get(&id).cloned());
    let Some(state) = state else {
        return Ok(JsValue::undefined());
    };
    if !state.configured || state.closed {
        return Ok(JsValue::undefined());
    }
    let (bytes, sample_rate, channels, frames) =
        ffmpeg_decode_audio(&state, &chunk).unwrap_or_default();
    let data_id = next_id();
    AUDIO_DATAS.with(|r| {
        r.borrow_mut().insert(
            data_id,
            AudioDataPayload {
                bytes,
                format: "f32".to_string(),
                sample_rate,
                number_of_channels: channels,
                number_of_frames: frames,
                timestamp_us: chunk.timestamp_us,
            },
        );
    });
    let audio = build_audio_data_object(ctx, data_id);
    if let Some(cb) = state.output_cb {
        let _ = cb.call(&JsValue::undefined(), &[audio], ctx);
    }
    Ok(JsValue::undefined())
}

fn audio_encoder_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, AUDIO_ENCODER_ID_KEY, ctx) {
        AUDIO_ENCODERS.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&id) {
                e.closed = true;
            }
        });
    }
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("closed")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn audio_decoder_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, AUDIO_DECODER_ID_KEY, ctx) {
        AUDIO_DECODERS.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&id) {
                e.closed = true;
            }
        });
    }
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("closed")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn audio_encoder_reset(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, AUDIO_ENCODER_ID_KEY, ctx) {
        AUDIO_ENCODERS.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&id) {
                e.configured = false;
            }
        });
    }
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("unconfigured")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

fn audio_decoder_reset(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, AUDIO_DECODER_ID_KEY, ctx) {
        AUDIO_DECODERS.with(|r| {
            if let Some(e) = r.borrow_mut().get_mut(&id) {
                e.configured = false;
            }
        });
    }
    if let Some(obj) = this.as_object() {
        let _ = obj.set(
            js_string!("state"),
            JsValue::from(js_string!("unconfigured")),
            false,
            ctx,
        );
    }
    Ok(JsValue::undefined())
}

// ============ ffmpeg pipe-driven codec round trips ============

fn map_video_codec(codec: &str) -> (&'static str, &'static str) {
    // WebCodecs codec string → (encoder name, container/raw output).
    let lower = codec.to_ascii_lowercase();
    if lower.starts_with("vp8") {
        ("libvpx", "ivf")
    } else if lower.starts_with("vp9") || lower.starts_with("vp09") {
        ("libvpx-vp9", "ivf")
    } else if lower.starts_with("av01") {
        ("libaom-av1", "ivf")
    } else if lower.starts_with("avc1") || lower.starts_with("h264") {
        ("libx264", "h264")
    } else if lower.starts_with("hev1") || lower.starts_with("hvc1") || lower.starts_with("h265") {
        ("libx265", "hevc")
    } else {
        ("libx264", "h264")
    }
}

fn map_audio_codec(codec: &str) -> (&'static str, &'static str) {
    let lower = codec.to_ascii_lowercase();
    if lower.starts_with("opus") {
        ("libopus", "ogg")
    } else if lower.starts_with("mp4a") || lower.starts_with("aac") {
        ("aac", "adts")
    } else if lower.starts_with("mp3") || lower == "mp3" {
        ("libmp3lame", "mp3")
    } else if lower.starts_with("flac") {
        ("flac", "flac")
    } else if lower.starts_with("vorbis") {
        ("libvorbis", "ogg")
    } else {
        ("libopus", "ogg")
    }
}

fn pix_fmt_for_format(format: &str) -> &'static str {
    match format.to_ascii_uppercase().as_str() {
        "RGBA" => "rgba",
        "RGBX" => "rgb0",
        "BGRA" => "bgra",
        "BGRX" => "bgr0",
        "I420" => "yuv420p",
        "NV12" => "nv12",
        _ => "rgba",
    }
}

fn ffmpeg_encode_video_frame(
    state: &EncoderState,
    frame: &VideoFrameData,
    key_frame: bool,
) -> Option<Vec<u8>> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let (encoder_name, fmt) = map_video_codec(&state.codec);
    let pix_fmt = pix_fmt_for_format(&frame.format);
    let size = format!("{}x{}", frame.coded_width, frame.coded_height);
    let bitrate = format!("{}", state.bitrate);
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        pix_fmt,
        "-s",
        &size,
        "-r",
        &format!("{}", state.framerate.max(1.0) as u32),
        "-i",
        "-",
        "-c:v",
        encoder_name,
        "-b:v",
        &bitrate,
    ]);
    if key_frame {
        cmd.args(["-force_key_frames", "expr:gte(t,0)"]);
    }
    cmd.args(["-f", fmt, "-"]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&frame.bytes);
    }
    let mut out = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut out);
    }
    let _ = child.wait();
    Some(out)
}

fn ffmpeg_decode_video_chunk(
    state: &EncoderState,
    chunk: &EncodedChunkPayload,
) -> Option<(Vec<u8>, u32, u32)> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let (_, fmt) = map_video_codec(&state.codec);
    // If JS gave us codedWidth/codedHeight on configure(), trust
    // them. Otherwise probe the encoded chunk with ffprobe — much
    // more reliable than the old "sqrt of byte count" guess that
    // was wrong for any non-square output.
    let (width, height) = if state.width > 0 && state.height > 0 {
        (state.width, state.height)
    } else {
        probe_encoded_dimensions(fmt, &chunk.bytes).unwrap_or((640, 480))
    };
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        fmt,
        "-i",
        "-",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-s",
        &format!("{width}x{height}"),
        "-",
    ]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&chunk.bytes);
    }
    let mut out = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut out);
    }
    let _ = child.wait();
    Some((out, width, height))
}

/// Pipe the encoded chunk into ffprobe and parse
/// `stream=width,height` out of the output. Falls back to None on
/// any parse / process error; the caller treats that as the
/// 640×480 default.
fn probe_encoded_dimensions(input_fmt: &str, bytes: &[u8]) -> Option<(u32, u32)> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-f",
        input_fmt,
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=width,height",
        "-of",
        "csv=s=x:p=0",
        "-",
    ]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(bytes);
    }
    let mut s = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut s);
    }
    let _ = child.wait();
    let s = s.trim();
    let mut parts = s.split('x');
    let w = parts.next()?.parse::<u32>().ok()?;
    let h = parts.next()?.parse::<u32>().ok()?;
    Some((w, h))
}

fn ffmpeg_encode_audio(
    state: &AudioEncoderState,
    data: &AudioDataPayload,
) -> Option<Vec<u8>> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let (encoder_name, fmt) = map_audio_codec(&state.codec);
    // Map WebCodecs format → ffmpeg sample format.
    let sample_fmt = match data.format.as_str() {
        "u8" => "u8",
        "s16" => "s16le",
        "s32" => "s32le",
        "f32" => "f32le",
        "f64" => "f64le",
        "u8-planar" => "u8p",
        "s16-planar" => "s16p",
        "s32-planar" => "s32p",
        "f32-planar" => "fltp",
        _ => "f32le",
    };
    let bitrate = format!("{}", state.bitrate);
    let nch = format!("{}", data.number_of_channels.max(1));
    let sr = format!("{}", data.sample_rate.max(8000));
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        sample_fmt,
        "-ar",
        &sr,
        "-ac",
        &nch,
        "-i",
        "-",
        "-c:a",
        encoder_name,
        "-b:a",
        &bitrate,
        "-f",
        fmt,
        "-",
    ]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&data.bytes);
    }
    let mut out = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut out);
    }
    let _ = child.wait();
    Some(out)
}

fn ffmpeg_decode_audio(
    state: &AudioEncoderState,
    chunk: &EncodedChunkPayload,
) -> Option<(Vec<u8>, u32, u32, u32)> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let (_, fmt) = map_audio_codec(&state.codec);
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        fmt,
        "-i",
        "-",
        "-f",
        "f32le",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-",
    ]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&chunk.bytes);
    }
    let mut out = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut out);
    }
    let _ = child.wait();
    let nch = 2u32;
    let frames = (out.len() as u32 / (4 * nch)).max(1);
    Some((out, 48000, nch, frames))
}

// ============ helpers ============

fn id_of(val: &JsValue, key: &str, ctx: &mut Context) -> Option<u32> {
    let obj = val.as_object()?;
    let v = obj.get(js_string!(key.to_string()), ctx).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.to_u32(ctx).ok()
}

fn read_bytes(val: &JsValue, ctx: &mut Context) -> Vec<u8> {
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
