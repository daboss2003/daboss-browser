//! Synchronous WebSocket client.
//!
//! Builds on `tungstenite` — the canonical pure-Rust WS implementation
//! — which gives us the RFC 6455 handshake + frame parser. We use the
//! `rustls-tls-webpki-roots` feature so `wss://` reuses the same root
//! store as the rest of the network code.
//!
//! Each `WebSocketConnection` owns a reader thread that blocks on
//! `read_message` and pushes incoming frames into a shared queue. The
//! JS engine drains the queue on every tick alongside microtasks,
//! observers, and RTC events.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tungstenite::{client::IntoClientRequest, Message};

const STATE_CONNECTING: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_CLOSING: u8 = 2;
const STATE_CLOSED: u8 = 3;

/// Server → client message visible to JS.
#[derive(Debug, Clone)]
pub enum WsInbound {
    Open,
    Text(String),
    Binary(Vec<u8>),
    Closed,
    Error(String),
}

pub struct WebSocketConnection {
    /// Sender side of the connection. tungstenite splits read + write
    /// onto the same `WebSocket<Stream>`, so we wrap it in a mutex.
    socket: Arc<Mutex<Option<tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>>>>,
    /// Incoming-message queue drained by JS on each tick.
    inbound: Arc<Mutex<VecDeque<WsInbound>>>,
    /// State exposed to JS via `readyState`. Mirrors the spec
    /// constants 0..3.
    pub ready_state: Arc<AtomicU8>,
    closed: Arc<AtomicBool>,
    _reader: JoinHandle<()>,
}

impl WebSocketConnection {
    /// Connect to `url` (ws:// or wss://). Returns `None` on
    /// handshake failure.
    pub fn connect(url: &str) -> Option<Self> {
        let request = url.into_client_request().ok()?;
        let (socket, _resp) = match tungstenite::connect(request) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[ws] connect failed: {e}");
                return None;
            }
        };
        // Put the socket on a non-blocking read schedule via a small
        // read timeout so the reader thread can shut down cleanly.
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = socket.get_ref() {
            let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(500)));
        }

        let socket = Arc::new(Mutex::new(Some(socket)));
        let inbound: Arc<Mutex<VecDeque<WsInbound>>> = Arc::new(Mutex::new(VecDeque::new()));
        inbound.lock().unwrap().push_back(WsInbound::Open);
        let ready_state = Arc::new(AtomicU8::new(STATE_OPEN));
        let closed = Arc::new(AtomicBool::new(false));

        let socket_for_reader = socket.clone();
        let inbound_for_reader = inbound.clone();
        let state_for_reader = ready_state.clone();
        let closed_for_reader = closed.clone();
        let reader = std::thread::spawn(move || loop {
            if closed_for_reader.load(Ordering::Relaxed) {
                break;
            }
            // Acquire the socket briefly to read one frame. Drop the
            // guard before pushing to the inbound queue so concurrent
            // `send()` from JS isn't blocked.
            let msg = {
                let mut guard = match socket_for_reader.lock() {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match guard.as_mut() {
                    Some(s) => s.read(),
                    None => break,
                }
            };
            match msg {
                Ok(Message::Text(t)) => {
                    inbound_for_reader
                        .lock()
                        .unwrap()
                        .push_back(WsInbound::Text(t.to_string()));
                }
                Ok(Message::Binary(b)) => {
                    inbound_for_reader
                        .lock()
                        .unwrap()
                        .push_back(WsInbound::Binary(b.to_vec()));
                }
                Ok(Message::Close(_)) => {
                    state_for_reader.store(STATE_CLOSED, Ordering::Relaxed);
                    inbound_for_reader.lock().unwrap().push_back(WsInbound::Closed);
                    break;
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {
                    // Tungstenite handles ping/pong replies automatically.
                }
                Err(tungstenite::Error::Io(io)) if io.kind() == std::io::ErrorKind::WouldBlock
                    || io.kind() == std::io::ErrorKind::TimedOut =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
                Err(e) => {
                    inbound_for_reader
                        .lock()
                        .unwrap()
                        .push_back(WsInbound::Error(e.to_string()));
                    state_for_reader.store(STATE_CLOSED, Ordering::Relaxed);
                    break;
                }
            }
        });

        Some(Self {
            socket,
            inbound,
            ready_state,
            closed,
            _reader: reader,
        })
    }

    pub fn send_text(&self, text: &str) -> bool {
        let mut guard = match self.socket.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        match guard.as_mut() {
            Some(s) => s.send(Message::text(text)).is_ok(),
            None => false,
        }
    }

    #[allow(dead_code)] // exposed for future Uint8Array `.send()` payloads
    pub fn send_binary(&self, bytes: Vec<u8>) -> bool {
        let mut guard = match self.socket.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        match guard.as_mut() {
            Some(s) => s.send(Message::binary(bytes)).is_ok(),
            None => false,
        }
    }

    pub fn close(&self) {
        self.ready_state.store(STATE_CLOSING, Ordering::Relaxed);
        self.closed.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.socket.lock() {
            if let Some(s) = guard.as_mut() {
                let _ = s.close(None);
            }
        }
        self.ready_state.store(STATE_CLOSED, Ordering::Relaxed);
    }

    pub fn drain_inbound(&self) -> Vec<WsInbound> {
        self.inbound
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }
}

impl Drop for WebSocketConnection {
    fn drop(&mut self) {
        self.close();
    }
}

#[allow(dead_code)]
pub const READY_STATE_CONNECTING: u8 = STATE_CONNECTING;
