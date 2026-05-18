//! `CompressionStream` + `DecompressionStream` backed by `flate2`.
//!
//! Each instance is shaped like a TransformStream: it has a
//! `.writable` WritableStream and a `.readable` ReadableStream.
//! Writes pump bytes through `flate2`'s streaming encoder/decoder;
//! the resulting bytes land in the readable side as `Uint8Array`
//! chunks. Closing the writer flushes the encoder and closes the
//! readable.
//!
//! Supported algorithms (per spec):
//!   * "gzip"        — gzip wrapper around deflate
//!   * "deflate"     — zlib wrapper around deflate (default Web style)
//!   * "deflate-raw" — raw deflate, no header/trailer

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::Write;

use boa_engine::{
    js_string,
    object::{builtins::{JsPromise, JsUint8Array}, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use flate2::write::{DeflateDecoder, DeflateEncoder, GzDecoder, GzEncoder, ZlibDecoder, ZlibEncoder};
use flate2::Compression;

const STREAM_ID_KEY: &str = "__compression_id";
const WRITER_ID_KEY: &str = "__compression_writer_id";

#[derive(Copy, Clone)]
pub enum Algo {
    Gzip,
    Deflate,
    DeflateRaw,
}

enum CodecState {
    Encode(EncoderKind),
    Decode(DecoderKind),
}

enum EncoderKind {
    Gz(GzEncoder<Vec<u8>>),
    Zlib(ZlibEncoder<Vec<u8>>),
    Raw(DeflateEncoder<Vec<u8>>),
}

enum DecoderKind {
    Gz(GzDecoder<Vec<u8>>),
    Zlib(ZlibDecoder<Vec<u8>>),
    Raw(DeflateDecoder<Vec<u8>>),
}

pub struct StreamState {
    pub codec: CodecState,
    pub closed: bool,
    pub output_queue: VecDeque<Vec<u8>>,
    /// Pending `read()` continuations on the readable side; we hand
    /// them resolvers and resolve when output bytes are available.
    pub readers: Vec<(boa_engine::object::builtins::JsFunction, boa_engine::object::builtins::JsFunction)>,
}

thread_local! {
    pub(crate) static COMPRESSION_STREAMS: RefCell<HashMap<u32, StreamState>> =
        RefCell::new(HashMap::new());
    pub(crate) static COMPRESSION_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_id() -> u32 {
    COMPRESSION_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("CompressionStream"),
        1,
        NativeFunction::from_fn_ptr(compression_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("DecompressionStream"),
        1,
        NativeFunction::from_fn_ptr(decompression_ctor),
    )
    .ok();
}

fn parse_algo(s: &str) -> Option<Algo> {
    match s {
        "gzip" => Some(Algo::Gzip),
        "deflate" => Some(Algo::Deflate),
        "deflate-raw" => Some(Algo::DeflateRaw),
        _ => None,
    }
}

fn compression_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let algo_str = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let Some(algo) = parse_algo(&algo_str) else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message(format!("CompressionStream: unsupported format '{algo_str}'"))
            .into());
    };
    let codec = CodecState::Encode(match algo {
        Algo::Gzip => EncoderKind::Gz(GzEncoder::new(Vec::new(), Compression::default())),
        Algo::Deflate => EncoderKind::Zlib(ZlibEncoder::new(Vec::new(), Compression::default())),
        Algo::DeflateRaw => EncoderKind::Raw(DeflateEncoder::new(Vec::new(), Compression::default())),
    });
    Ok(build_stream_object(ctx, codec))
}

fn decompression_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let algo_str = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let Some(algo) = parse_algo(&algo_str) else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message(format!("DecompressionStream: unsupported format '{algo_str}'"))
            .into());
    };
    let codec = CodecState::Decode(match algo {
        Algo::Gzip => DecoderKind::Gz(GzDecoder::new(Vec::new())),
        Algo::Deflate => DecoderKind::Zlib(ZlibDecoder::new(Vec::new())),
        Algo::DeflateRaw => DecoderKind::Raw(DeflateDecoder::new(Vec::new())),
    });
    Ok(build_stream_object(ctx, codec))
}

fn build_stream_object(ctx: &mut Context, codec: CodecState) -> JsValue {
    let id = next_id();
    COMPRESSION_STREAMS.with(|r| {
        r.borrow_mut().insert(
            id,
            StreamState {
                codec,
                closed: false,
                output_queue: VecDeque::new(),
                readers: Vec::new(),
            },
        );
    });
    let writable = build_writable(ctx, id);
    let readable = build_readable(ctx, id);
    ObjectInitializer::new(ctx)
        .property(
            js_string!(STREAM_ID_KEY),
            JsValue::from(id),
            Attribute::READONLY,
        )
        .property(js_string!("writable"), writable, Attribute::READONLY)
        .property(js_string!("readable"), readable, Attribute::READONLY)
        .build()
        .into()
}

fn build_writable(ctx: &mut Context, id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(STREAM_ID_KEY), JsValue::from(id), Attribute::READONLY);
    b.property(js_string!("locked"), JsValue::from(false), Attribute::all());
    b.function(
        NativeFunction::from_fn_ptr(writable_get_writer),
        js_string!("getWriter"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(writable_close),
        js_string!("close"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(writable_abort),
        js_string!("abort"),
        1,
    );
    JsValue::from(b.build())
}

fn build_readable(ctx: &mut Context, id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(STREAM_ID_KEY), JsValue::from(id), Attribute::READONLY);
    b.property(js_string!("locked"), JsValue::from(false), Attribute::all());
    b.function(
        NativeFunction::from_fn_ptr(readable_get_reader),
        js_string!("getReader"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(readable_cancel),
        js_string!("cancel"),
        1,
    );
    JsValue::from(b.build())
}

fn id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(STREAM_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn writable_get_writer(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, ctx) else {
        return Err(boa_engine::JsNativeError::error()
            .with_message("CompressionStream writable not registered")
            .into());
    };
    Ok(ObjectInitializer::new(ctx)
        .property(
            js_string!(WRITER_ID_KEY),
            JsValue::from(id),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(writer_write),
            js_string!("write"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(writer_close),
            js_string!("close"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(writer_abort),
            js_string!("abort"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(writer_release_lock),
            js_string!("releaseLock"),
            0,
        )
        .build()
        .into())
}

fn writer_id(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(WRITER_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
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

fn writer_write(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = writer_id(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let bytes = args
        .first()
        .map(|v| read_bytes(v, ctx))
        .unwrap_or_default();
    feed_codec(id, &bytes);
    drain_to_readers(ctx, id);
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writer_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = writer_id(this, ctx) {
        finish_codec(id);
        drain_to_readers(ctx, id);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writer_abort(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = writer_id(this, ctx) {
        finish_codec(id);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writer_release_lock(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn writable_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, ctx) {
        finish_codec(id);
        drain_to_readers(ctx, id);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn writable_abort(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, ctx) {
        finish_codec(id);
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn readable_get_reader(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, ctx) else {
        return Err(boa_engine::JsNativeError::error()
            .with_message("CompressionStream readable not registered")
            .into());
    };
    Ok(ObjectInitializer::new(ctx)
        .property(
            js_string!(STREAM_ID_KEY),
            JsValue::from(id),
            Attribute::READONLY,
        )
        .function(
            NativeFunction::from_fn_ptr(reader_read),
            js_string!("read"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(reader_cancel),
            js_string!("cancel"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(reader_release_lock),
            js_string!("releaseLock"),
            0,
        )
        .build()
        .into())
}

fn reader_read(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    let chunk = COMPRESSION_STREAMS.with(|r| {
        let mut map = r.borrow_mut();
        let state = map.get_mut(&id)?;
        let next = state.output_queue.pop_front();
        Some((next, state.closed))
    });
    let Some((next, closed)) = chunk else {
        return Ok(JsPromise::resolve(JsValue::undefined(), ctx).into());
    };
    if let Some(bytes) = next {
        let u8a = JsUint8Array::from_iter(bytes.into_iter(), ctx)?;
        let result = build_read_result(ctx, JsValue::from(u8a), false);
        return Ok(JsPromise::resolve(result, ctx).into());
    }
    if closed {
        let result = build_read_result(ctx, JsValue::undefined(), true);
        return Ok(JsPromise::resolve(result, ctx).into());
    }
    // Park a pending read.
    let (promise, resolvers) = JsPromise::new_pending(ctx);
    COMPRESSION_STREAMS.with(|r| {
        if let Some(state) = r.borrow_mut().get_mut(&id) {
            state.readers.push((resolvers.resolve, resolvers.reject));
        }
    });
    Ok(promise.into())
}

fn reader_cancel(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(id) = id_of(this, ctx) {
        COMPRESSION_STREAMS.with(|r| {
            if let Some(state) = r.borrow_mut().get_mut(&id) {
                state.closed = true;
                state.output_queue.clear();
            }
        });
    }
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn readable_cancel(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    reader_cancel(this, &[], ctx)
}

fn reader_release_lock(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn build_read_result(ctx: &mut Context, value: JsValue, done: bool) -> JsValue {
    ObjectInitializer::new(ctx)
        .property(js_string!("value"), value, Attribute::READONLY)
        .property(js_string!("done"), JsValue::from(done), Attribute::READONLY)
        .build()
        .into()
}

// ============ codec drivers ============

fn feed_codec(id: u32, input: &[u8]) {
    COMPRESSION_STREAMS.with(|r| {
        let mut map = r.borrow_mut();
        let Some(state) = map.get_mut(&id) else { return };
        if state.closed {
            return;
        }
        let produced = match &mut state.codec {
            CodecState::Encode(EncoderKind::Gz(enc)) => write_and_flush(enc, input),
            CodecState::Encode(EncoderKind::Zlib(enc)) => write_and_flush(enc, input),
            CodecState::Encode(EncoderKind::Raw(enc)) => write_and_flush(enc, input),
            CodecState::Decode(DecoderKind::Gz(dec)) => write_and_flush(dec, input),
            CodecState::Decode(DecoderKind::Zlib(dec)) => write_and_flush(dec, input),
            CodecState::Decode(DecoderKind::Raw(dec)) => write_and_flush(dec, input),
        };
        if !produced.is_empty() {
            state.output_queue.push_back(produced);
        }
    });
}

trait FlushDrain {
    fn drain_output(&mut self) -> Vec<u8>;
}

impl FlushDrain for GzEncoder<Vec<u8>> {
    fn drain_output(&mut self) -> Vec<u8> {
        std::mem::take(self.get_mut())
    }
}
impl FlushDrain for ZlibEncoder<Vec<u8>> {
    fn drain_output(&mut self) -> Vec<u8> {
        std::mem::take(self.get_mut())
    }
}
impl FlushDrain for DeflateEncoder<Vec<u8>> {
    fn drain_output(&mut self) -> Vec<u8> {
        std::mem::take(self.get_mut())
    }
}
impl FlushDrain for GzDecoder<Vec<u8>> {
    fn drain_output(&mut self) -> Vec<u8> {
        std::mem::take(self.get_mut())
    }
}
impl FlushDrain for ZlibDecoder<Vec<u8>> {
    fn drain_output(&mut self) -> Vec<u8> {
        std::mem::take(self.get_mut())
    }
}
impl FlushDrain for DeflateDecoder<Vec<u8>> {
    fn drain_output(&mut self) -> Vec<u8> {
        std::mem::take(self.get_mut())
    }
}

fn write_and_flush<W: Write + FlushDrain>(w: &mut W, input: &[u8]) -> Vec<u8> {
    let _ = w.write_all(input);
    let _ = w.flush();
    w.drain_output()
}

fn finish_codec(id: u32) {
    COMPRESSION_STREAMS.with(|r| {
        let mut map = r.borrow_mut();
        let Some(state) = map.get_mut(&id) else { return };
        let final_bytes = match std::mem::replace(
            &mut state.codec,
            CodecState::Decode(DecoderKind::Raw(DeflateDecoder::new(Vec::new()))),
        ) {
            CodecState::Encode(EncoderKind::Gz(enc)) => enc.finish().unwrap_or_default(),
            CodecState::Encode(EncoderKind::Zlib(enc)) => enc.finish().unwrap_or_default(),
            CodecState::Encode(EncoderKind::Raw(enc)) => enc.finish().unwrap_or_default(),
            CodecState::Decode(DecoderKind::Gz(dec)) => dec.finish().unwrap_or_default(),
            CodecState::Decode(DecoderKind::Zlib(dec)) => dec.finish().unwrap_or_default(),
            CodecState::Decode(DecoderKind::Raw(dec)) => dec.finish().unwrap_or_default(),
        };
        state.closed = true;
        if !final_bytes.is_empty() {
            state.output_queue.push_back(final_bytes);
        }
    });
}

fn drain_to_readers(ctx: &mut Context, id: u32) {
    loop {
        let resolved = COMPRESSION_STREAMS.with(
            |r| -> Option<(boa_engine::object::builtins::JsFunction, Option<Vec<u8>>, bool)> {
                let mut map = r.borrow_mut();
                let state = map.get_mut(&id)?;
                if state.readers.is_empty() {
                    return None;
                }
                let next_chunk = state.output_queue.pop_front();
                if next_chunk.is_none() && !state.closed {
                    return None;
                }
                let (resolve, _) = state.readers.remove(0);
                Some((resolve, next_chunk, state.closed))
            },
        );
        let Some((resolve, chunk, closed)) = resolved else {
            break;
        };
        match chunk {
            Some(bytes) => {
                let u8a = match JsUint8Array::from_iter(bytes.into_iter(), ctx) {
                    Ok(arr) => JsValue::from(arr),
                    Err(_) => JsValue::undefined(),
                };
                let result = build_read_result(ctx, u8a, false);
                let _ = resolve.call(&JsValue::undefined(), &[result], ctx);
            }
            None if closed => {
                let result = build_read_result(ctx, JsValue::undefined(), true);
                let _ = resolve.call(&JsValue::undefined(), &[result], ctx);
            }
            _ => {}
        }
    }
}
