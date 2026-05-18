//! Server-Sent Events (text/event-stream) client.
//!
//! Spec: https://html.spec.whatwg.org/multipage/server-sent-events.html
//!
//! Each `EventSourceConnection` owns a background thread that holds a
//! long-lived HTTP/1.1 connection open and parses incoming SSE frames
//! into `SseEvent`s. The reader pushes events into a queue the JS
//! engine drains alongside microtasks / timers / WebSocket frames.
//!
//! Auto-reconnect: when the connection drops (EOF or error), the
//! reader sleeps for `reconnect_ms` (default 3 seconds, settable via
//! the server's `retry:` field) and reissues the request with the
//! latest `Last-Event-ID` header.
//!
//! Out of scope: HTTP/2 streaming (we issue HTTP/1.1 only — the
//! existing `net::Client` HTTP/2 path is request-oriented and doesn't
//! surface chunk streaming), redirect handling beyond a single hop.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::net::transport::{default_tls_config, Connection};

const STATE_CONNECTING: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_CLOSED: u8 = 2;

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SseInbound {
    Open,
    Message(SseEvent),
    Error(String),
    Closed,
}

pub struct EventSourceConnection {
    pub inbound: Arc<Mutex<VecDeque<SseInbound>>>,
    pub ready_state: Arc<AtomicU8>,
    closed: Arc<AtomicBool>,
    _reader: JoinHandle<()>,
}

impl EventSourceConnection {
    /// Open `url` and start streaming. Returns `None` if the URL is
    /// malformed; transport / HTTP errors are surfaced as
    /// `SseInbound::Error` events instead.
    pub fn connect(url: &str) -> Option<Self> {
        let parsed = url::Url::parse(url).ok()?;
        let scheme = parsed.scheme().to_string();
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let host = parsed.host_str()?.to_string();
        let port = parsed.port_or_known_default()?;
        let mut path = parsed.path().to_string();
        if let Some(q) = parsed.query() {
            path.push('?');
            path.push_str(q);
        }
        if path.is_empty() {
            path = "/".to_string();
        }
        let use_tls = scheme == "https";

        let inbound: Arc<Mutex<VecDeque<SseInbound>>> = Arc::new(Mutex::new(VecDeque::new()));
        let ready_state = Arc::new(AtomicU8::new(STATE_CONNECTING));
        let closed = Arc::new(AtomicBool::new(false));

        let inbound_for_reader = inbound.clone();
        let state_for_reader = ready_state.clone();
        let closed_for_reader = closed.clone();
        let host_for_reader = host.clone();
        let port_for_reader = port;
        let path_for_reader = path.clone();
        let scheme_for_reader = use_tls;

        let reader = std::thread::spawn(move || {
            let tls = Arc::new(default_tls_config());
            let mut last_event_id: Option<String> = None;
            // Default reconnect time per spec is 3 seconds.
            let mut reconnect_ms: u64 = 3_000;

            'outer: loop {
                if closed_for_reader.load(Ordering::Relaxed) {
                    break;
                }
                // Resolve hostname → address. Failures back off and
                // retry.
                let addrs = match (host_for_reader.as_str(), port_for_reader)
                    .to_socket_addrs_helper()
                {
                    Ok(a) => a,
                    Err(_) => {
                        push(
                            &inbound_for_reader,
                            SseInbound::Error("dns resolution failed".into()),
                        );
                        std::thread::sleep(Duration::from_millis(reconnect_ms));
                        continue;
                    }
                };
                let Some(addr) = addrs.into_iter().next() else {
                    push(
                        &inbound_for_reader,
                        SseInbound::Error("no addresses".into()),
                    );
                    std::thread::sleep(Duration::from_millis(reconnect_ms));
                    continue;
                };

                let mut conn = match Connection::open(
                    addr,
                    &host_for_reader,
                    scheme_for_reader,
                    &tls,
                    Duration::from_secs(10),
                    // SSE streams idle for long stretches — bigger
                    // read timeout than ordinary requests.
                    Duration::from_secs(60),
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        push(&inbound_for_reader, SseInbound::Error(e.to_string()));
                        std::thread::sleep(Duration::from_millis(reconnect_ms));
                        continue;
                    }
                };

                // Issue the GET. Set Accept: text/event-stream and a
                // Last-Event-ID header for resumption.
                let mut req = format!(
                    "GET {} HTTP/1.1\r\n\
                     Host: {}\r\n\
                     User-Agent: daboss/0.1\r\n\
                     Accept: text/event-stream\r\n\
                     Cache-Control: no-cache\r\n",
                    path_for_reader, host_for_reader
                );
                if let Some(id) = &last_event_id {
                    req.push_str(&format!("Last-Event-ID: {id}\r\n"));
                }
                req.push_str("Connection: keep-alive\r\n\r\n");
                if conn.write_all(req.as_bytes()).is_err() || conn.flush().is_err() {
                    push(
                        &inbound_for_reader,
                        SseInbound::Error("write failed".into()),
                    );
                    std::thread::sleep(Duration::from_millis(reconnect_ms));
                    continue;
                }

                // Parse response headers (HTTP/1.1) before switching to
                // the event-stream parser. We accept anything 2xx; any
                // non-2xx becomes an Error and triggers reconnect.
                let mut reader = BufReader::new(conn);
                let mut status_line = String::new();
                if reader.read_line(&mut status_line).is_err() {
                    push(&inbound_for_reader, SseInbound::Error("eof reading status".into()));
                    std::thread::sleep(Duration::from_millis(reconnect_ms));
                    continue;
                }
                let status_ok = parse_status_ok(&status_line);
                // Drain headers.
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        break;
                    }
                    if line == "\r\n" || line == "\n" || line.is_empty() {
                        break;
                    }
                }
                if !status_ok {
                    push(
                        &inbound_for_reader,
                        SseInbound::Error(format!("HTTP error: {}", status_line.trim())),
                    );
                    std::thread::sleep(Duration::from_millis(reconnect_ms));
                    continue;
                }

                state_for_reader.store(STATE_OPEN, Ordering::Relaxed);
                push(&inbound_for_reader, SseInbound::Open);

                // SSE event parser: accumulate lines until a blank
                // line, then emit the event. Track `id:` for
                // resumption and `retry:` for reconnect timing.
                let mut event_name = String::new();
                let mut data_buf = String::new();
                let mut last_id_in_event: Option<String> = None;

                loop {
                    if closed_for_reader.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => {
                            // EOF — break inner loop, reconnect.
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    let line = line.trim_end_matches('\n').trim_end_matches('\r');
                    if line.is_empty() {
                        // Dispatch the accumulated event.
                        if !data_buf.is_empty() {
                            // Strip trailing newline left from the
                            // accumulator's per-line append.
                            let mut data = std::mem::take(&mut data_buf);
                            if data.ends_with('\n') {
                                data.pop();
                            }
                            let name = if event_name.is_empty() {
                                "message".to_string()
                            } else {
                                std::mem::take(&mut event_name)
                            };
                            if let Some(id) = &last_id_in_event {
                                last_event_id = Some(id.clone());
                            }
                            push(
                                &inbound_for_reader,
                                SseInbound::Message(SseEvent {
                                    event: name,
                                    data,
                                    id: last_id_in_event.take(),
                                }),
                            );
                        }
                        event_name.clear();
                        data_buf.clear();
                        continue;
                    }
                    if line.starts_with(':') {
                        // Comment line — ignore.
                        continue;
                    }
                    let (field, value) = match line.split_once(':') {
                        Some((f, v)) => (f, v.trim_start_matches(' ')),
                        None => (line, ""),
                    };
                    match field {
                        "event" => {
                            event_name = value.to_string();
                        }
                        "data" => {
                            data_buf.push_str(value);
                            data_buf.push('\n');
                        }
                        "id" => {
                            // The spec says NULs cancel the id; we
                            // skip that nuance.
                            last_id_in_event = Some(value.to_string());
                        }
                        "retry" => {
                            if let Ok(ms) = value.parse::<u64>() {
                                reconnect_ms = ms;
                            }
                        }
                        _ => {}
                    }
                }

                // Connection dropped; reconnect after the configured
                // delay unless we're shutting down.
                state_for_reader.store(STATE_CONNECTING, Ordering::Relaxed);
                if closed_for_reader.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(reconnect_ms));
            }

            state_for_reader.store(STATE_CLOSED, Ordering::Relaxed);
            push(&inbound_for_reader, SseInbound::Closed);
        });

        Some(Self {
            inbound,
            ready_state,
            closed,
            _reader: reader,
        })
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.ready_state.store(STATE_CLOSED, Ordering::Relaxed);
    }

    pub fn drain(&self) -> Vec<SseInbound> {
        match self.inbound.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }
}

impl Drop for EventSourceConnection {
    fn drop(&mut self) {
        self.close();
    }
}

/// Cap on queued SSE events before we drop the oldest. Live
/// streams (chat tickers, log feeds) can outpace JS handlers; this
/// floor keeps the queue at ≤ 1024 events.
const INBOUND_QUEUE_CAP: usize = 1024;

fn push(queue: &Arc<Mutex<VecDeque<SseInbound>>>, ev: SseInbound) {
    if let Ok(mut q) = queue.lock() {
        while q.len() >= INBOUND_QUEUE_CAP {
            q.pop_front();
        }
        q.push_back(ev);
    }
}

fn parse_status_ok(status_line: &str) -> bool {
    // Status line: "HTTP/1.1 200 OK". Accept anything 200-299.
    let mut parts = status_line.split_whitespace();
    parts.next(); // protocol
    let code = parts.next().and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
    (200..300).contains(&code)
}

/// Wrap `std::net::ToSocketAddrs` so the caller doesn't need to import
/// the trait. Resolves `(host, port)` to an iterator of `SocketAddr`s.
trait ToSocketAddrsHelper {
    fn to_socket_addrs_helper(self) -> std::io::Result<Vec<std::net::SocketAddr>>;
}

impl ToSocketAddrsHelper for (&str, u16) {
    fn to_socket_addrs_helper(self) -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        Ok(self.to_socket_addrs()?.collect())
    }
}
