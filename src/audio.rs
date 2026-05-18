//! Audio playback subsystem (toy scope).
//!
//! Pure-Rust WAV decoding (16-bit PCM, the dominant on-the-web format)
//! piped through `cpal` for cross-platform output. Covers `<audio
//! autoplay>` and JS-driven `audio.play()` / `audio.pause()` / current
//! time tracking.
//!
//! Explicitly **not** in scope this commit:
//!  * MP3 / AAC / Opus / Ogg / FLAC. Each needs its own decoder
//!    (`symphonia` handles MP3/FLAC/Ogg-Vorbis purely-in-Rust; AAC
//!    needs `fdk-aac`). Adding them is a focused follow-up — the
//!    `AudioElement` API below is decoder-agnostic.
//!  * `<video>`. Container demux + video codec is its own multi-week
//!    project (av1 via `dav1d`, h264 via `openh264` C dep, mp4 via
//!    `re_mp4`, audio sync clock, decode threading).
//!  * Web Audio API (`AudioContext`, gain nodes, etc.).
//!
//! Threading: cpal opens its own output thread; we hand it a callback
//! that mutates a `PlaybackState`. Browser code stays single-threaded.

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use symphonia::core::audio::AudioBufferRef;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::sample::Sample;

pub struct WavSamples {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved PCM samples, [-1.0, 1.0].
    pub samples: Vec<f32>,
}

/// Decode any audio format symphonia recognises (MP3, FLAC, Vorbis,
/// WAV, AAC inside ADTS or MP4) into f32 interleaved samples. The
/// hand-rolled WAV path stays as the fallback so unit tests don't
/// need to drag the symphonia pipeline through tiny inputs.
pub fn decode_any(bytes: &[u8]) -> Option<WavSamples> {
    if let Some(s) = decode_via_symphonia(bytes) {
        return Some(s);
    }
    decode_wav(bytes)
}

fn decode_via_symphonia(bytes: &[u8]) -> Option<WavSamples> {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let hint = Hint::new();
    // No filename hint — let symphonia probe by content.
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;
    let mut format = probed.format;
    let track = format.default_track()?.clone();
    let codec_params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .ok()?;

    let sample_rate = codec_params.sample_rate?;
    let channel_layout = codec_params.channels?;
    let channels = channel_layout.count() as u16;

    let mut interleaved = Vec::<f32>::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(_) => break,
        };
        if packet.track_id() != track.id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(b) => b,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        };
        append_interleaved(decoded, &mut interleaved, channels as usize);
    }
    if interleaved.is_empty() {
        return None;
    }
    Some(WavSamples {
        sample_rate,
        channels,
        samples: interleaved,
    })
}

fn append_interleaved(buf: AudioBufferRef<'_>, out: &mut Vec<f32>, channels: usize) {
    use symphonia::core::audio::Signal;

    match buf {
        AudioBufferRef::U8(b) => convert_planar_to_interleaved(&b, out, channels, |s: u8| {
            (s as f32 - 128.0) / 128.0
        }),
        AudioBufferRef::U16(b) => convert_planar_to_interleaved(&b, out, channels, |s: u16| {
            (s as f32 - 32768.0) / 32768.0
        }),
        AudioBufferRef::U24(b) => convert_planar_to_interleaved(
            &b,
            out,
            channels,
            |s: symphonia::core::sample::u24| {
                (s.0 as f32 - 8_388_608.0) / 8_388_608.0
            },
        ),
        AudioBufferRef::U32(b) => convert_planar_to_interleaved(&b, out, channels, |s: u32| {
            (s as f64 / u32::MAX as f64 - 0.5) as f32 * 2.0
        }),
        AudioBufferRef::S8(b) => convert_planar_to_interleaved(&b, out, channels, |s: i8| {
            s as f32 / i8::MAX as f32
        }),
        AudioBufferRef::S16(b) => convert_planar_to_interleaved(&b, out, channels, |s: i16| {
            s as f32 / i16::MAX as f32
        }),
        AudioBufferRef::S24(b) => convert_planar_to_interleaved(
            &b,
            out,
            channels,
            |s: symphonia::core::sample::i24| s.inner() as f32 / 8_388_607.0,
        ),
        AudioBufferRef::S32(b) => convert_planar_to_interleaved(&b, out, channels, |s: i32| {
            s as f32 / i32::MAX as f32
        }),
        AudioBufferRef::F32(b) => {
            let frames = b.frames();
            for f in 0..frames {
                for c in 0..channels {
                    out.push(b.chan(c)[f]);
                }
            }
        }
        AudioBufferRef::F64(b) => {
            let frames = b.frames();
            for f in 0..frames {
                for c in 0..channels {
                    out.push(b.chan(c)[f] as f32);
                }
            }
        }
    }
}

fn convert_planar_to_interleaved<S: Sample + Copy>(
    buf: &symphonia::core::audio::AudioBuffer<S>,
    out: &mut Vec<f32>,
    channels: usize,
    to_f32: impl Fn(S) -> f32,
) {
    use symphonia::core::audio::Signal;
    let frames = buf.frames();
    for f in 0..frames {
        for c in 0..channels {
            out.push(to_f32(buf.chan(c)[f]));
        }
    }
}

/// Parse a minimal subset of the WAV container: RIFF header,
/// uncompressed PCM `fmt ` chunk, `data` chunk. 16-bit and 8-bit
/// integer PCM are converted to `f32` in [-1.0, 1.0]. Returns `None`
/// for anything else.
pub fn decode_wav(bytes: &[u8]) -> Option<WavSamples> {
    if bytes.len() < 44 {
        return None;
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut audio_format = 1u16; // PCM
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let chunk_id = &bytes[pos..pos + 4];
        let chunk_size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
        let chunk_start = pos + 8;
        let chunk_end = chunk_start.checked_add(chunk_size)?;
        if chunk_end > bytes.len() {
            return None;
        }
        match chunk_id {
            b"fmt " => {
                if chunk_size < 16 {
                    return None;
                }
                audio_format = u16::from_le_bytes(bytes[chunk_start..chunk_start + 2].try_into().ok()?);
                channels = u16::from_le_bytes(bytes[chunk_start + 2..chunk_start + 4].try_into().ok()?);
                sample_rate = u32::from_le_bytes(bytes[chunk_start + 4..chunk_start + 8].try_into().ok()?);
                bits_per_sample = u16::from_le_bytes(bytes[chunk_start + 14..chunk_start + 16].try_into().ok()?);
            }
            b"data" => {
                data = Some(&bytes[chunk_start..chunk_end]);
            }
            _ => {}
        }
        // chunks are word-aligned; pad odd sizes by one byte.
        pos = chunk_end + (chunk_size & 1);
    }
    let data = data?;
    if audio_format != 1 || channels == 0 || sample_rate == 0 {
        return None;
    }
    let samples = match bits_per_sample {
        16 => data
            .chunks_exact(2)
            .map(|c| {
                let v = i16::from_le_bytes([c[0], c[1]]);
                v as f32 / i16::MAX as f32
            })
            .collect(),
        8 => data
            .iter()
            .map(|&b| (b as f32 - 128.0) / 128.0)
            .collect(),
        _ => return None,
    };
    Some(WavSamples {
        sample_rate,
        channels,
        samples,
    })
}

/// Shared playback state. The cpal output thread reads from this every
/// callback; the main thread mutates `playing` and `cursor` in
/// response to JS `play()` / `pause()`.
pub struct PlaybackState {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
    pub cursor: usize,
    pub playing: bool,
    pub loop_playback: bool,
    pub volume: f32,
}

/// A single `<audio>` instance: holds the cpal stream alive and a
/// handle to the shared playback state.
pub struct AudioElement {
    state: Arc<Mutex<PlaybackState>>,
    _stream: cpal::Stream,
}

impl AudioElement {
    /// Build an `AudioElement` from decoded WAV samples. Opens the
    /// default output device and starts the stream paused.
    pub fn from_wav(wav: WavSamples) -> Option<Self> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let config = device.default_output_config().ok()?;
        let device_rate = config.sample_rate().0;
        let device_channels = config.channels();

        let state = Arc::new(Mutex::new(PlaybackState {
            samples: wav.samples,
            sample_rate: wav.sample_rate,
            channels: wav.channels,
            cursor: 0,
            playing: false,
            loop_playback: false,
            volume: 1.0,
        }));

        let stream_state = state.clone();
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device
                .build_output_stream(
                    &config.into(),
                    move |data: &mut [f32], _| {
                        fill_buffer(&stream_state, data, device_rate, device_channels);
                    },
                    |e| eprintln!("[audio] stream error: {e}"),
                    None,
                )
                .ok()?,
            cpal::SampleFormat::I16 => {
                let cb_state = state.clone();
                device
                    .build_output_stream(
                        &config.into(),
                        move |data: &mut [i16], _| {
                            let mut buf = vec![0.0_f32; data.len()];
                            fill_buffer(&cb_state, &mut buf, device_rate, device_channels);
                            for (out, sample) in data.iter_mut().zip(buf.iter()) {
                                *out = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                            }
                        },
                        |e| eprintln!("[audio] stream error: {e}"),
                        None,
                    )
                    .ok()?
            }
            cpal::SampleFormat::U16 => {
                let cb_state = state.clone();
                device
                    .build_output_stream(
                        &config.into(),
                        move |data: &mut [u16], _| {
                            let mut buf = vec![0.0_f32; data.len()];
                            fill_buffer(&cb_state, &mut buf, device_rate, device_channels);
                            for (out, sample) in data.iter_mut().zip(buf.iter()) {
                                let v = (sample.clamp(-1.0, 1.0) + 1.0) * 0.5;
                                *out = (v * u16::MAX as f32) as u16;
                            }
                        },
                        |e| eprintln!("[audio] stream error: {e}"),
                        None,
                    )
                    .ok()?
            }
            _ => return None,
        };
        stream.play().ok()?;
        Some(Self {
            state,
            _stream: stream,
        })
    }

    pub fn play(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.playing = true;
        }
    }

    pub fn pause(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.playing = false;
        }
    }

    #[allow(dead_code)] // exposed for future JS bindings on `<audio>` props
    pub fn is_playing(&self) -> bool {
        self.state.lock().map(|s| s.playing).unwrap_or(false)
    }

    #[allow(dead_code)] // future JS binding for `audio.currentTime`
    pub fn current_time(&self) -> f64 {
        match self.state.lock() {
            Ok(s) => s.cursor as f64 / (s.sample_rate as f64 * s.channels as f64),
            Err(_) => 0.0,
        }
    }

    #[allow(dead_code)] // future JS binding for `audio.duration`
    pub fn duration(&self) -> f64 {
        match self.state.lock() {
            Ok(s) => s.samples.len() as f64 / (s.sample_rate as f64 * s.channels as f64),
            Err(_) => 0.0,
        }
    }

    pub fn set_volume(&self, v: f32) {
        if let Ok(mut s) = self.state.lock() {
            s.volume = v.clamp(0.0, 1.0);
        }
    }

    pub fn set_loop(&self, on: bool) {
        if let Ok(mut s) = self.state.lock() {
            s.loop_playback = on;
        }
    }

    /// Hand out a lightweight clock the video decoder uses to pace
    /// frame presentation. Reads the same `cursor` cpal advances on
    /// the output thread, so playback drift on either side stays in
    /// sync to within a single output buffer.
    pub fn clock(&self) -> AudioClock {
        AudioClock {
            state: self.state.clone(),
        }
    }
}

/// Wall-clock view of an [`AudioElement`]. Cheap to clone and read
/// from any thread; the video decoder takes one and queries it once
/// per frame to decide whether to present, sleep, or drop.
#[derive(Clone)]
pub struct AudioClock {
    state: Arc<Mutex<PlaybackState>>,
}

impl AudioClock {
    /// Current decoded-audio playhead in seconds, or `None` if audio
    /// is paused / not yet started. The video decoder falls back to a
    /// wall clock when we return `None`.
    pub fn now_secs(&self) -> Option<f64> {
        let s = self.state.lock().ok()?;
        if !s.playing {
            return None;
        }
        let channels = s.channels.max(1) as f64;
        let rate = s.sample_rate.max(1) as f64;
        Some(s.cursor as f64 / (rate * channels))
    }
}

/// Fill the device's output buffer by pulling samples from the
/// playback state. Handles channel up/downmix (mono ↔ stereo) and
/// linear sample-rate resampling on the fly.
fn fill_buffer(
    state: &Arc<Mutex<PlaybackState>>,
    output: &mut [f32],
    device_rate: u32,
    device_channels: u16,
) {
    let Ok(mut s) = state.lock() else {
        output.fill(0.0);
        return;
    };
    if !s.playing || s.samples.is_empty() {
        output.fill(0.0);
        return;
    }
    let src_rate = s.sample_rate.max(1) as f32;
    let dst_rate = device_rate.max(1) as f32;
    let ratio = src_rate / dst_rate;
    let src_channels = s.channels.max(1) as usize;
    let dst_channels = device_channels.max(1) as usize;
    let volume = s.volume;

    let frames = output.len() / dst_channels;
    for frame in 0..frames {
        let src_idx = s.cursor + (frame as f32 * ratio) as usize * src_channels;
        if src_idx + src_channels > s.samples.len() {
            if s.loop_playback {
                s.cursor = 0;
            } else {
                s.playing = false;
            }
            // Zero out the tail so we don't loop garbage.
            for out_ch in 0..dst_channels {
                output[frame * dst_channels + out_ch] = 0.0;
            }
            continue;
        }
        // Up/downmix.
        let l = s.samples[src_idx];
        let r = if src_channels >= 2 {
            s.samples[src_idx + 1]
        } else {
            l
        };
        match dst_channels {
            1 => output[frame] = ((l + r) * 0.5) * volume,
            _ => {
                output[frame * dst_channels] = l * volume;
                output[frame * dst_channels + 1] = r * volume;
                for c in 2..dst_channels {
                    output[frame * dst_channels + c] = 0.0;
                }
            }
        }
    }
    let frames_advanced = (frames as f32 * ratio) as usize;
    s.cursor = (s.cursor + frames_advanced * src_channels).min(s.samples.len());
    if s.cursor >= s.samples.len() {
        if s.loop_playback {
            s.cursor = 0;
        } else {
            s.playing = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_wav(samples_le: &[u8], sample_rate: u32, channels: u16, bps: u16) -> Vec<u8> {
        let mut wav = Vec::new();
        // RIFF header
        wav.extend_from_slice(b"RIFF");
        let total = 4 + 8 + 16 + 8 + samples_le.len();
        wav.extend_from_slice(&(total as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        // fmt chunk
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        let byte_rate = sample_rate * channels as u32 * (bps / 8) as u32;
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        let block_align = channels * (bps / 8);
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bps.to_le_bytes());
        // data chunk
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples_le.len() as u32).to_le_bytes());
        wav.extend_from_slice(samples_le);
        wav
    }

    #[test]
    fn decode_wav_round_trip_16bit_mono() {
        // 4 samples at i16 max.
        let mut samples = Vec::new();
        for _ in 0..4 {
            samples.extend_from_slice(&i16::MAX.to_le_bytes());
        }
        let wav = make_wav(&samples, 48_000, 1, 16);
        let decoded = decode_wav(&wav).expect("should decode");
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.samples.len(), 4);
        // i16::MAX maps to ~1.0.
        assert!((decoded.samples[0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn decode_garbage_returns_none() {
        assert!(decode_wav(b"this is not a wav").is_none());
    }

    #[test]
    fn decode_wav_round_trip_8bit_stereo() {
        let samples = vec![128u8, 200, 50, 128]; // two stereo frames
        let wav = make_wav(&samples, 22_050, 2, 8);
        let decoded = decode_wav(&wav).expect("should decode");
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.samples.len(), 4);
        // 128 → 0.0, 200 → ~0.56.
        assert!((decoded.samples[0] - 0.0).abs() < 1e-3);
        assert!(decoded.samples[1] > 0.5);
    }
}
