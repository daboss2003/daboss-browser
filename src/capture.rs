//! Camera + microphone capture for `getUserMedia()`.
//!
//! Each `CaptureStream` owns:
//!   * a `nokhwa` camera handle when video is requested,
//!   * a `cpal` input stream when audio is requested,
//!   * a background thread (camera path only) that pushes RGBA frames
//!     into a shared latest-frame slot.
//!
//! JS surfaces the stream via the existing `MediaStream`-shaped JS
//! object; the paint layer pulls the latest frame from the slot when
//! a `<video>.srcObject = stream` element renders.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};
use nokhwa::CallbackCamera;

pub struct CaptureFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// One live capture session. Drop releases the camera + mic.
pub struct CaptureStream {
    pub latest_frame: Arc<Mutex<Option<CaptureFrame>>>,
    pub mic_samples: Arc<Mutex<Vec<f32>>>,
    pub mic_rate: u32,
    pub mic_channels: u16,
    stop: Arc<AtomicBool>,
    _camera_thread: Option<JoinHandle<()>>,
    _camera: Option<CallbackCamera>,
    _mic_stream: Option<cpal::Stream>,
}

impl CaptureStream {
    pub fn open(video: bool, audio: bool) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let latest_frame: Arc<Mutex<Option<CaptureFrame>>> = Arc::new(Mutex::new(None));
        let mic_samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let mut camera_thread = None;
        let mut camera = None;
        let mut mic_stream = None;
        let mut mic_rate = 0;
        let mut mic_channels = 0;

        if video {
            let frame_slot = latest_frame.clone();
            let stop_for_cam = stop.clone();
            let format = RequestedFormat::new::<RgbAFormat>(
                RequestedFormatType::AbsoluteHighestFrameRate,
            );
            match CallbackCamera::new(CameraIndex::Index(0), format, move |buf| {
                if stop_for_cam.load(Ordering::Relaxed) {
                    return;
                }
                let resolution = buf.resolution();
                let width = resolution.width();
                let height = resolution.height();
                let Ok(decoded) = buf.decode_image::<RgbAFormat>() else {
                    return;
                };
                let pixels = decoded.into_raw();
                if let Ok(mut slot) = frame_slot.lock() {
                    *slot = Some(CaptureFrame {
                        width,
                        height,
                        rgba: pixels,
                    });
                }
            }) {
                Ok(mut cam) => {
                    if let Err(e) = cam.open_stream() {
                        eprintln!("[capture] camera open_stream: {e}");
                    }
                    camera = Some(cam);
                }
                Err(e) => {
                    eprintln!("[capture] camera init: {e}");
                }
            }
        }

        if audio {
            let host = cpal::default_host();
            if let Some(device) = host.default_input_device() {
                if let Ok(config) = device.default_input_config() {
                    mic_rate = config.sample_rate().0;
                    mic_channels = config.channels();
                    let samples_slot = mic_samples.clone();
                    let stream_result = match config.sample_format() {
                        cpal::SampleFormat::F32 => device.build_input_stream(
                            &config.into(),
                            move |data: &[f32], _| {
                                if let Ok(mut buf) = samples_slot.lock() {
                                    buf.extend_from_slice(data);
                                    // Keep at most ~10s buffered so we
                                    // don't grow unbounded.
                                    if buf.len() > 44_100 * 2 * 10 {
                                        let drop = buf.len() - 44_100 * 2 * 10;
                                        buf.drain(0..drop);
                                    }
                                }
                            },
                            |e| eprintln!("[capture] mic stream error: {e}"),
                            None,
                        ),
                        cpal::SampleFormat::I16 => device.build_input_stream(
                            &config.into(),
                            move |data: &[i16], _| {
                                if let Ok(mut buf) = samples_slot.lock() {
                                    for &s in data {
                                        buf.push(s as f32 / i16::MAX as f32);
                                    }
                                }
                            },
                            |e| eprintln!("[capture] mic stream error: {e}"),
                            None,
                        ),
                        cpal::SampleFormat::U16 => device.build_input_stream(
                            &config.into(),
                            move |data: &[u16], _| {
                                if let Ok(mut buf) = samples_slot.lock() {
                                    for &s in data {
                                        buf.push((s as f32 - 32768.0) / 32768.0);
                                    }
                                }
                            },
                            |e| eprintln!("[capture] mic stream error: {e}"),
                            None,
                        ),
                        _ => return None,
                    };
                    if let Ok(stream) = stream_result {
                        let _ = stream.play();
                        mic_stream = Some(stream);
                    }
                }
            }
        }

        let _ = camera_thread.insert(std::thread::spawn(|| {}));
        Some(Self {
            latest_frame,
            mic_samples,
            mic_rate,
            mic_channels,
            stop,
            _camera_thread: camera_thread,
            _camera: camera,
            _mic_stream: mic_stream,
        })
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut cam) = self._camera.take() {
            let _ = cam.stop_stream();
        }
    }
}
