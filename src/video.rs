//! `<video>` decoding via `ffmpeg-sidecar` — drives the system
//! `ffmpeg` CLI as a child process, pipes raw RGBA frames out on
//! stdout. No build-time C dep; whatever codecs your system FFmpeg
//! supports (H.264 / H.265 / VP9 / AV1 / …) work here too.
//!
//! Each `<video>` element gets its own decode thread reading from the
//! ffmpeg stdout pipe. Frames stream into a shared `latest_frame`
//! slot; the paint pass picks the newest at composite time and draws
//! it at the element's box rect.
//!
//! Accepted caveats:
//!  * No A/V sync: video plays at its decoded cadence; audio is
//!    ignored (a separate ffmpeg subprocess could feed our audio
//!    stack with PCM, deferred).
//!  * No precise seek: there's a `pause()` that drains and a `play()`
//!    that resumes by restarting decode from the beginning (cheap
//!    enough for the toy; not spec-correct).
//!  * One-frame buffer: the decoder always overwrites; jittery
//!    consumers see the most recent frame, not a queue.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Default)]
struct PlaybackState {
    latest_frame: Option<DecodedFrame>,
    playing: bool,
    ended: bool,
}

pub struct VideoElement {
    state: Arc<Mutex<PlaybackState>>,
    stop: Arc<AtomicBool>,
    /// Process handle so we can kill ffmpeg on drop.
    child: Arc<Mutex<Option<Child>>>,
    _decoder_thread: JoinHandle<()>,
    /// Audio player driving the parallel-decoded audio track. Held
    /// here so dropping the video also drops the audio.
    _audio: Option<crate::audio::AudioElement>,
    #[allow(dead_code)] // exposed for the upcoming `videoWidth` JS prop
    pub intrinsic_width: u32,
    #[allow(dead_code)] // exposed for the upcoming `videoHeight` JS prop
    pub intrinsic_height: u32,
}

impl VideoElement {
    /// Spawn an ffmpeg subprocess to decode `bytes` (the full media
    /// container, written to a temp file on disk). Picks the video
    /// stream's intrinsic resolution from a probe pass first.
    pub fn from_bytes(bytes: Vec<u8>, autoplay: bool, loop_playback: bool) -> Option<Self> {
        let path = write_tempfile(&bytes).ok()?;
        let (intrinsic_width, intrinsic_height) = probe_dimensions(&path).unwrap_or((640, 360));

        let mut cmd = Command::new("ffmpeg");
        if loop_playback {
            cmd.args(["-stream_loop", "-1"]);
        }
        cmd.args([
            "-i",
            path.to_str()?,
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-an", // drop audio — we ignore it for the toy
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[video] failed to spawn ffmpeg: {e}");
                let _ = std::fs::remove_file(&path);
                return None;
            }
        };
        let stdout = child.stdout.take()?;

        let state = Arc::new(Mutex::new(PlaybackState {
            playing: autoplay,
            ..PlaybackState::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let child_handle = Arc::new(Mutex::new(Some(child)));

        let state_for_thread = state.clone();
        let stop_for_thread = stop.clone();
        let child_for_thread = child_handle.clone();
        let path_for_thread = path.clone();
        let frame_bytes = intrinsic_width as usize * intrinsic_height as usize * 4;

        let decoder_thread = std::thread::spawn(move || {
            let mut reader = stdout;
            let mut buf = vec![0u8; frame_bytes];
            // Pace the producer at ~30 fps unless the upstream is
            // faster. ffmpeg streams at decode rate by default; we
            // don't model presentation time.
            loop {
                if stop_for_thread.load(Ordering::Relaxed) {
                    break;
                }
                let want_decode = state_for_thread
                    .lock()
                    .map(|s| s.playing)
                    .unwrap_or(false);
                if !want_decode {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    continue;
                }
                match reader.read_exact(&mut buf) {
                    Ok(()) => {
                        if let Ok(mut s) = state_for_thread.lock() {
                            s.latest_frame = Some(DecodedFrame {
                                width: intrinsic_width,
                                height: intrinsic_height,
                                rgba: buf.clone(),
                            });
                        }
                        std::thread::sleep(std::time::Duration::from_millis(33));
                    }
                    Err(_) => {
                        // EOF or pipe broken — mark ended and exit.
                        if let Ok(mut s) = state_for_thread.lock() {
                            s.ended = true;
                            s.playing = false;
                        }
                        break;
                    }
                }
            }
            if let Ok(mut guard) = child_for_thread.lock() {
                if let Some(mut c) = guard.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
            let _ = std::fs::remove_file(&path_for_thread);
        });

        // Parallel-decode the audio track to PCM via a second ffmpeg
        // pass and pipe it through cpal. No clock-based A/V sync —
        // we accept whatever drift accumulates over a clip.
        let audio_element = decode_audio_via_ffmpeg(&path, autoplay, loop_playback);

        Some(Self {
            state,
            stop,
            child: child_handle,
            _decoder_thread: decoder_thread,
            _audio: audio_element,
            intrinsic_width,
            intrinsic_height,
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

    pub fn current_frame(&self) -> Option<DecodedFrame> {
        let s = self.state.lock().ok()?;
        s.latest_frame.as_ref().map(|f| DecodedFrame {
            width: f.width,
            height: f.height,
            rgba: f.rgba.clone(),
        })
    }
}

impl Drop for VideoElement {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut c) = guard.take() {
                let _ = c.kill();
            }
        }
    }
}

fn probe_dimensions(path: &std::path::Path) -> Option<(u32, u32)> {
    // Use ffprobe (ships with FFmpeg) for a quick width/height read.
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=s=x:p=0",
            path.to_str()?,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&out.stdout).ok()?.trim();
    let mut parts = s.split('x');
    let w = parts.next()?.parse::<u32>().ok()?;
    let h = parts.next()?.parse::<u32>().ok()?;
    Some((w, h))
}

/// Pull the audio track out of the same media file via a second
/// ffmpeg subprocess that writes a WAV-like PCM stream to stdout.
/// `audio::decode_any` then ingests the bytes and the standard
/// cpal player drives playback. Returns `None` if the file has no
/// audio or ffmpeg refuses for any reason.
fn decode_audio_via_ffmpeg(
    path: &std::path::Path,
    autoplay: bool,
    loop_playback: bool,
) -> Option<crate::audio::AudioElement> {
    // Probe for audio stream presence; if none, skip.
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
            path.to_str()?,
        ])
        .output()
        .ok()?;
    if !probe.status.success() || probe.stdout.is_empty() {
        return None;
    }

    let mut cmd = Command::new("ffmpeg");
    if loop_playback {
        cmd.args(["-stream_loop", "-1"]);
    }
    cmd.args([
        "-i",
        path.to_str()?,
        "-hide_banner",
        "-loglevel",
        "error",
        "-vn", // ignore video
        "-f",
        "wav", // simple PCM container symphonia decodes immediately
        "-acodec",
        "pcm_s16le",
        "-ar",
        "44100",
        "-ac",
        "2",
        "-",
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::null());

    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let mut buf = Vec::new();
    // Best-effort read of the full PCM stream. Long videos block this
    // thread — for the toy that's acceptable; a streaming queue would
    // be the next-step refinement.
    use std::io::Read;
    let _ = stdout.read_to_end(&mut buf);
    let _ = child.wait();
    let wav = crate::audio::decode_any(&buf)?;
    let element = crate::audio::AudioElement::from_wav(wav)?;
    if autoplay {
        element.play();
    }
    let _ = loop_playback; // already handled by ffmpeg's -stream_loop
    Some(element)
}

fn write_tempfile(bytes: &[u8]) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    let mut path = std::env::temp_dir();
    let name = format!(
        "daboss-video-{}.bin",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    path.push(name);
    let mut f = std::fs::File::create(&path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(path)
}
