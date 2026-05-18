//! Web Audio API surface for `<canvas>`-based mic visualizers.
//!
//! `new AudioContext()` returns an object with the standard subset
//! visualizers use:
//!   * `createMediaStreamSource(stream)` reads `stream.__capture_idx`
//!     and produces a node whose downstream nodes can pull live mic
//!     samples from the [`crate::capture::CaptureStream::mic_samples`]
//!     buffer.
//!   * `createAnalyser()` returns an AnalyserNode with
//!     `getByteFrequencyData` (windowed FFT magnitudes) and
//!     `getByteTimeDomainData` (mic waveform mapped to 0..255).
//!   * `createGain()` and `destination` are no-op stubs (mic samples
//!     don't loop back to the speakers; pages still chain through
//!     them, so we accept and discard).
//!
//! Connection tracking is intentionally minimal: `node.connect(dest)`
//! copies the source's `__capture_idx` onto `dest`. That lets any
//! downstream AnalyserNode in a chain `source → gain → analyser` find
//! the same mic stream.

use std::cell::RefCell;
use std::f32::consts::PI;

use boa_engine::{
    js_string,
    object::{builtins::JsArray, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::capture::CaptureStream;

const CAPTURE_IDX_KEY: &str = "__capture_idx";
const FFT_SIZE_KEY: &str = "__fft_size";

thread_local! {
    /// `performance.now()`-style origin so `currentTime` starts at 0
    /// for the first AudioContext on each engine.
    static AC_ORIGIN: RefCell<Option<std::time::Instant>> = const { RefCell::new(None) };
}

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("AudioContext"),
        0,
        NativeFunction::from_fn_ptr(audio_context_constructor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("webkitAudioContext"),
        0,
        NativeFunction::from_fn_ptr(audio_context_constructor),
    )
    .ok();
}

fn audio_context_constructor(
    _: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    AC_ORIGIN.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(std::time::Instant::now());
        }
    });
    let destination = ObjectInitializer::new(ctx).build();
    let realm = ctx.realm().clone();
    let current_time_getter = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(audio_context_current_time),
    )
    .build();
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!("sampleRate"),
        JsValue::from(48000u32),
        Attribute::READONLY,
    );
    b.property(
        js_string!("state"),
        JsValue::from(js_string!("running")),
        Attribute::all(),
    );
    b.property(
        js_string!("destination"),
        JsValue::from(destination),
        Attribute::READONLY,
    );
    b.accessor(
        js_string!("currentTime"),
        Some(current_time_getter),
        None,
        Attribute::ENUMERABLE,
    );
    b.function(
        NativeFunction::from_fn_ptr(create_media_stream_source),
        js_string!("createMediaStreamSource"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(create_analyser),
        js_string!("createAnalyser"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(create_gain),
        js_string!("createGain"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(noop_promise),
        js_string!("resume"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(noop_promise),
        js_string!("suspend"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(noop_promise),
        js_string!("close"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn audio_context_current_time(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let secs = AC_ORIGIN.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|i| i.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    });
    Ok(JsValue::from(secs))
}

fn noop_promise(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsPromise;
    Ok(JsPromise::resolve(JsValue::undefined(), ctx).into())
}

fn create_media_stream_source(
    _: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let idx = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!(CAPTURE_IDX_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok());
    Ok(build_audio_node(ctx, idx))
}

fn create_analyser(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let node = build_audio_node(ctx, None);
    let obj = node
        .as_object()
        .cloned()
        .expect("build_audio_node returns object");
    let _ = obj.set(
        js_string!(FFT_SIZE_KEY),
        JsValue::from(2048u32),
        false,
        ctx,
    );
    let _ = obj.set(
        js_string!("fftSize"),
        JsValue::from(2048u32),
        false,
        ctx,
    );
    let _ = obj.set(
        js_string!("frequencyBinCount"),
        JsValue::from(1024u32),
        false,
        ctx,
    );
    let _ = obj.set(
        js_string!("minDecibels"),
        JsValue::from(-100.0_f64),
        false,
        ctx,
    );
    let _ = obj.set(
        js_string!("maxDecibels"),
        JsValue::from(-30.0_f64),
        false,
        ctx,
    );
    let _ = obj.set(
        js_string!("smoothingTimeConstant"),
        JsValue::from(0.8_f64),
        false,
        ctx,
    );
    let realm = ctx.realm().clone();
    let freq = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(analyser_get_byte_frequency_data),
    )
    .build();
    let time = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(analyser_get_byte_time_domain_data),
    )
    .build();
    let _ = obj.set(
        js_string!("getByteFrequencyData"),
        JsValue::from(freq),
        false,
        ctx,
    );
    let _ = obj.set(
        js_string!("getByteTimeDomainData"),
        JsValue::from(time),
        false,
        ctx,
    );
    Ok(JsValue::from(obj))
}

fn create_gain(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let gain = ObjectInitializer::new(ctx)
        .property(
            js_string!("value"),
            JsValue::from(1.0_f64),
            Attribute::all(),
        )
        .build();
    let node = build_audio_node(ctx, None);
    let obj = node
        .as_object()
        .cloned()
        .expect("build_audio_node returns object");
    let _ = obj.set(
        js_string!("gain"),
        JsValue::from(gain),
        false,
        ctx,
    );
    Ok(JsValue::from(obj))
}

fn build_audio_node(ctx: &mut Context, capture_idx: Option<u32>) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    if let Some(idx) = capture_idx {
        b.property(
            js_string!(CAPTURE_IDX_KEY),
            JsValue::from(idx),
            Attribute::all(),
        );
    }
    b.function(
        NativeFunction::from_fn_ptr(audio_node_connect),
        js_string!("connect"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(audio_node_disconnect),
        js_string!("disconnect"),
        0,
    );
    JsValue::from(b.build())
}

/// `node.connect(dest)` — propagate `__capture_idx` so any downstream
/// AnalyserNode can find the mic stream the source was wired to.
fn audio_node_connect(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(src) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Some(dest) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    if let Ok(idx_val) = src.get(js_string!(CAPTURE_IDX_KEY), ctx) {
        if !idx_val.is_undefined() {
            let _ = dest.set(js_string!(CAPTURE_IDX_KEY), idx_val, false, ctx);
        }
    }
    // Return `dest` so JS chains: `src.connect(filter).connect(gain)`.
    Ok(JsValue::from(dest.clone()))
}

fn audio_node_disconnect(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

/// `analyser.getByteFrequencyData(uint8Array)` — pull recent mic
/// samples, take a window-of-fftSize Hann-windowed FFT, write per-bin
/// magnitudes scaled to 0..255 into `uint8Array`. If no mic stream is
/// connected, writes silence.
fn analyser_get_byte_frequency_data(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(arr_obj) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    let length = arr_obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0) as usize;
    if length == 0 {
        return Ok(JsValue::undefined());
    }
    let fft_size = this
        .as_object()
        .and_then(|o| o.get(js_string!(FFT_SIZE_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(2048)
        .max(2) as usize;
    let samples = capture_samples(this, ctx, fft_size);
    let magnitudes = fft_magnitudes(&samples);
    // Map fftSize/2 bins onto the caller's `length` entries (which
    // should already equal frequencyBinCount, but tolerate mismatches
    // by linear nearest-neighbour mapping).
    let bin_count = magnitudes.len().min(length);
    for i in 0..length {
        let m = if i < bin_count {
            magnitudes[i]
        } else {
            0.0
        };
        let scaled = (m.clamp(0.0, 1.0) * 255.0) as u32;
        let _ = arr_obj.set(i as u32, JsValue::from(scaled), false, ctx);
    }
    Ok(JsValue::undefined())
}

/// `analyser.getByteTimeDomainData(uint8Array)` — write `[-1, 1]` mic
/// waveform mapped to `0..255` with 128 = silence.
fn analyser_get_byte_time_domain_data(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(arr_obj) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    let length = arr_obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0) as usize;
    if length == 0 {
        return Ok(JsValue::undefined());
    }
    let samples = capture_samples(this, ctx, length);
    for i in 0..length {
        let s = samples.get(i).copied().unwrap_or(0.0);
        let byte = ((s.clamp(-1.0, 1.0) * 0.5 + 0.5) * 255.0) as u32;
        let _ = arr_obj.set(i as u32, JsValue::from(byte), false, ctx);
    }
    let _ = JsArray::new(ctx); // touch ctx so unused-import lint stays quiet in some builds
    Ok(JsValue::undefined())
}

/// Pull the most recent `wanted` mono samples from the CaptureStream
/// the analyser is connected to. Mixes interleaved channels down to
/// mono. Returns an empty Vec if no upstream source set.
fn capture_samples(this: &JsValue, ctx: &mut Context, wanted: usize) -> Vec<f32> {
    let Some(this_obj) = this.as_object() else {
        return Vec::new();
    };
    let idx_val = match this_obj.get(js_string!(CAPTURE_IDX_KEY), ctx) {
        Ok(v) if !v.is_undefined() && !v.is_null() => v,
        _ => return Vec::new(),
    };
    let Ok(idx) = idx_val.to_u32(ctx) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    super::media::JS_CAPTURE_REGISTRY.with(|slot| {
        let Some(rc) = slot.borrow().as_ref().cloned() else {
            return;
        };
        let reg = rc.borrow();
        let Some(stream) = reg.get(idx as usize).and_then(|s| s.as_ref()) else {
            return;
        };
        out = downmix_recent(stream, wanted);
    });
    out
}

fn downmix_recent(stream: &CaptureStream, wanted: usize) -> Vec<f32> {
    let Ok(samples) = stream.mic_samples.lock() else {
        return Vec::new();
    };
    let channels = stream.mic_channels.max(1) as usize;
    let total_frames = samples.len() / channels;
    let take_frames = wanted.min(total_frames);
    let start_frame = total_frames - take_frames;
    let mut out = Vec::with_capacity(take_frames);
    for f in 0..take_frames {
        let base = (start_frame + f) * channels;
        let mut sum = 0.0;
        for c in 0..channels {
            sum += samples[base + c];
        }
        out.push(sum / channels as f32);
    }
    out
}

/// In-place radix-2 Cooley-Tukey FFT magnitude. Pads (or truncates)
/// input to the next-power-of-two ≤ 4096 and returns `n/2` normalized
/// magnitudes in `[0, 1]`. For visualizer-quality work this is plenty;
/// dB scaling is the caller's responsibility.
fn fft_magnitudes(samples: &[f32]) -> Vec<f32> {
    let n = samples.len().next_power_of_two().min(4096);
    if n < 2 {
        return Vec::new();
    }
    let mut re = vec![0.0_f32; n];
    let mut im = vec![0.0_f32; n];
    // Hann-window into the real buffer.
    for i in 0..n.min(samples.len()) {
        let w = 0.5 - 0.5 * (2.0 * PI * i as f32 / (n as f32 - 1.0)).cos();
        re[i] = samples[i] * w;
    }
    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j &= !bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    // Cooley-Tukey butterfly.
    let mut size = 2usize;
    while size <= n {
        let half = size / 2;
        let table_step = 2.0 * PI / size as f32;
        for i in (0..n).step_by(size) {
            for k in 0..half {
                let angle = -table_step * k as f32;
                let wr = angle.cos();
                let wi = angle.sin();
                let tr = wr * re[i + k + half] - wi * im[i + k + half];
                let ti = wr * im[i + k + half] + wi * re[i + k + half];
                re[i + k + half] = re[i + k] - tr;
                im[i + k + half] = im[i + k] - ti;
                re[i + k] += tr;
                im[i + k] += ti;
            }
        }
        size <<= 1;
    }
    // Magnitude per bin, normalised by N/2 so a unit sine wave gives
    // mag ≈ 1 in its single bin.
    let half = n / 2;
    let mut mags = Vec::with_capacity(half);
    let scale = 2.0 / n as f32;
    for k in 0..half {
        let m = (re[k] * re[k] + im[k] * im[k]).sqrt() * scale;
        mags.push(m.min(1.0));
    }
    mags
}
