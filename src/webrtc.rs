//! WebRTC integration via the `webrtc` crate (webrtc-rs).
//!
//! Exposes a `PeerConnection` wrapper that the JS bindings hand out
//! when scripts construct `new RTCPeerConnection()`. The actual
//! protocol stack — STUN candidate gathering, ICE connectivity
//! checks, DTLS handshake, SCTP framing for data channels — all
//! happens inside the library.
//!
//! Threading: webrtc-rs is async (tokio). We keep a single
//! application-wide tokio runtime on a background thread and submit
//! futures to it via `Runtime::block_on` for sync method calls. Event
//! delivery (`onicecandidate`, `ondatachannel`, message receipt)
//! works through Rust-side channels that the JS engine drains at the
//! same point it drains microtasks and timer fires.
//!
//! Scope kept honest:
//!  * Data channels only. Audio / video tracks are not exposed (no
//!    getUserMedia, no `<video>` capture pipeline yet).
//!  * Default-ICE-server config (Google STUN). Custom `iceServers`
//!    array is accepted but only the `urls` field is honoured.
//!  * No SDP munging.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::runtime::Runtime;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

/// One queued event surfacing from a PeerConnection to JS land.
#[derive(Debug, Clone)]
pub enum PcEvent {
    /// A locally-gathered ICE candidate. `None` means the gathering
    /// pass finished (the spec's "end-of-candidates" signal).
    IceCandidate(Option<String>),
    /// Connection-state transition (`new` / `connecting` / `connected`
    /// / `disconnected` / `failed` / `closed`).
    ConnectionState(String),
    /// Remote opened a data channel; payload is the label.
    DataChannel(String),
    /// Data channel emitted a message addressed at the data channel
    /// of label `(label, payload_text)`.
    DataMessage(String, String),
    /// Data channel opened (local label).
    DataChannelOpen(String),
}

pub struct PeerConnection {
    runtime: Arc<Runtime>,
    inner: Arc<RTCPeerConnection>,
    events: Arc<Mutex<VecDeque<PcEvent>>>,
}

impl PeerConnection {
    /// Build a new RTCPeerConnection. `ice_urls` is the optional list
    /// of STUN/TURN server URLs. Empty defaults to Google's public
    /// STUN.
    pub fn new(runtime: Arc<Runtime>, ice_urls: Vec<String>) -> Option<Self> {
        let cfg = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: if ice_urls.is_empty() {
                    vec!["stun:stun.l.google.com:19302".to_string()]
                } else {
                    ice_urls
                },
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut media = MediaEngine::default();
        if media.register_default_codecs().is_err() {
            return None;
        }
        let registry = match register_default_interceptors(Registry::new(), &mut media) {
            Ok(r) => r,
            Err(_) => return None,
        };
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .build();

        let pc = match runtime.block_on(async { api.new_peer_connection(cfg).await }) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                eprintln!("[webrtc] new_peer_connection: {e}");
                return None;
            }
        };
        let events: Arc<Mutex<VecDeque<PcEvent>>> = Arc::new(Mutex::new(VecDeque::new()));

        // Wire callbacks → event queue.
        let ev_ice = events.clone();
        pc.on_ice_candidate(Box::new(move |candidate| {
            let q = ev_ice.clone();
            Box::pin(async move {
                let payload = candidate.map(|c| c.to_string());
                if let Ok(mut q) = q.lock() {
                    q.push_back(PcEvent::IceCandidate(payload));
                }
            })
        }));

        let ev_state = events.clone();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            let q = ev_state.clone();
            let name = format!("{state}").to_lowercase();
            Box::pin(async move {
                if let Ok(mut q) = q.lock() {
                    q.push_back(PcEvent::ConnectionState(name));
                }
            })
        }));

        let ev_dc = events.clone();
        pc.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
            let q_open = ev_dc.clone();
            let label = channel.label().to_string();
            let label_for_msg = label.clone();
            let label_for_open = label.clone();
            let q_msg = ev_dc.clone();
            channel.on_message(Box::new(move |msg| {
                let q = q_msg.clone();
                let label = label_for_msg.clone();
                Box::pin(async move {
                    let payload =
                        String::from_utf8(msg.data.to_vec()).unwrap_or_default();
                    if let Ok(mut q) = q.lock() {
                        q.push_back(PcEvent::DataMessage(label, payload));
                    }
                })
            }));
            Box::pin(async move {
                if let Ok(mut q) = q_open.lock() {
                    q.push_back(PcEvent::DataChannel(label_for_open));
                }
            })
        }));

        Some(Self {
            runtime,
            inner: pc,
            events,
        })
    }

    pub fn create_offer(&self) -> Option<String> {
        let pc = self.inner.clone();
        self.runtime.block_on(async move {
            let offer = pc.create_offer(None).await.ok()?;
            pc.set_local_description(offer.clone()).await.ok()?;
            Some(offer.sdp)
        })
    }

    pub fn create_answer(&self) -> Option<String> {
        let pc = self.inner.clone();
        self.runtime.block_on(async move {
            let answer = pc.create_answer(None).await.ok()?;
            pc.set_local_description(answer.clone()).await.ok()?;
            Some(answer.sdp)
        })
    }

    pub fn set_remote_description(&self, sdp_type: &str, sdp: &str) -> bool {
        let pc = self.inner.clone();
        let desc = match sdp_type {
            "offer" => RTCSessionDescription::offer(sdp.to_string()),
            "answer" => RTCSessionDescription::answer(sdp.to_string()),
            "pranswer" => RTCSessionDescription::pranswer(sdp.to_string()),
            _ => return false,
        };
        let Ok(desc) = desc else {
            return false;
        };
        self.runtime
            .block_on(async move { pc.set_remote_description(desc).await.is_ok() })
    }

    pub fn add_ice_candidate(&self, candidate_init: &str) -> bool {
        let pc = self.inner.clone();
        let payload = candidate_init.to_string();
        self.runtime.block_on(async move {
            pc.add_ice_candidate(webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
                candidate: payload,
                ..Default::default()
            })
            .await
            .is_ok()
        })
    }

    /// Open a data channel from this side. Stores it so JS can drive
    /// `send` against the same channel later by label.
    pub fn create_data_channel(&self, label: &str) -> Option<Arc<RTCDataChannel>> {
        let pc = self.inner.clone();
        let label_owned = label.to_string();
        let init = RTCDataChannelInit::default();
        let label_clone = label_owned.clone();
        let dc = self.runtime.block_on(async move {
            pc.create_data_channel(&label_owned, Some(init)).await.ok()
        })?;
        let q_open = self.events.clone();
        let label_for_open = label_clone.clone();
        dc.on_open(Box::new(move || {
            let q = q_open.clone();
            let label = label_for_open.clone();
            Box::pin(async move {
                if let Ok(mut q) = q.lock() {
                    q.push_back(PcEvent::DataChannelOpen(label));
                }
            })
        }));
        let q_msg = self.events.clone();
        let label_for_msg = label_clone.clone();
        dc.on_message(Box::new(move |msg| {
            let q = q_msg.clone();
            let label = label_for_msg.clone();
            Box::pin(async move {
                let payload = String::from_utf8(msg.data.to_vec()).unwrap_or_default();
                if let Ok(mut q) = q.lock() {
                    q.push_back(PcEvent::DataMessage(label, payload));
                }
            })
        }));
        Some(dc)
    }

    pub fn drain_events(&self) -> Vec<PcEvent> {
        match self.events.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn close(&self) {
        let pc = self.inner.clone();
        let _ = self.runtime.block_on(async move { pc.close().await });
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        self.close();
    }
}

/// Send a string payload over a data channel. Sync wrapper.
pub fn data_channel_send(
    runtime: &Arc<Runtime>,
    dc: &Arc<RTCDataChannel>,
    payload: &str,
) -> bool {
    let dc = dc.clone();
    let payload = payload.to_string();
    runtime.block_on(async move { dc.send_text(payload).await.is_ok() })
}

/// One-time runtime builder for WebRTC + h2.
pub fn build_runtime() -> Option<Arc<Runtime>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .ok()?;
    Some(Arc::new(rt))
}
