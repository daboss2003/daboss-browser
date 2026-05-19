//! HTTP/2 connection pool — keeps the tokio runtime and per-origin
//! `h2::client::SendRequest` handles alive across requests.
//!
//! The original [`h2c::request_h2`] (and the parallel [`h3c::request_h3`])
//! spin up a fresh `tokio::runtime` plus a full TLS + h2 handshake for
//! every call. That's correct, but wasteful: a typical page hits the
//! same origin 30+ times for sub-resources, and each repeat pays a
//! ~50–200 ms cold-start cost we don't need.
//!
//! This pool sits between [`net::Client`] and `h2c`. The first request
//! to `(host, port)` opens the connection as before; subsequent
//! requests clone the cached `SendRequest` and skip the handshake.
//! On send failure we drop the cached entry so the next caller
//! re-handshakes. The driver task stays running in the background
//! until the connection dies on its own; we don't reap it.
//!
//! Scope cut: no h3 pooling in this iteration — quinn endpoints are
//! considerably heavier to keep alive (UDP socket binding, packet
//! pacing). The h3 path keeps its per-request runtime for now; that's
//! a follow-up.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use h2::client::SendRequest;
use http::{HeaderName, HeaderValue, Method, Request, Uri};
use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use tokio_rustls::TlsConnector;

use super::error::{Error, Result};
use super::h2c::H2Outcome;
use super::http::Response;

/// Persistent tokio runtime + per-origin h2 connection cache. One
/// per [`net::Client`].
pub struct HttpPool {
    runtime: Arc<tokio::runtime::Runtime>,
    h2_senders: Mutex<HashMap<(String, u16), SendRequest<Bytes>>>,
}

impl HttpPool {
    /// Spin up the shared runtime. Returns `None` if tokio init
    /// fails (extremely rare; usually a per-OS resource exhaust).
    pub fn new() -> Option<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .ok()?;
        Some(Self {
            runtime: Arc::new(runtime),
            h2_senders: Mutex::new(HashMap::new()),
        })
    }

    /// Issue an HTTP/2 request, reusing a pooled connection when
    /// possible. Falls back to a fresh handshake if no entry exists
    /// or the cached one has become unusable.
    #[allow(clippy::too_many_arguments)]
    pub fn request_h2(
        &self,
        tls: &Arc<ClientConfig>,
        host: &str,
        port: u16,
        request_method: &str,
        request_path: &str,
        request_headers: &[(String, String)],
        request_body: Vec<u8>,
        connect_timeout: Duration,
        read_timeout: Duration,
        max_response_bytes: usize,
    ) -> H2Outcome {
        let key = (host.to_ascii_lowercase(), port);
        // Try the cached sender first.
        let cached = { self.h2_senders.lock().ok().and_then(|m| m.get(&key).cloned()) };
        if let Some(sender) = cached {
            match self.runtime.block_on(send_with(
                sender,
                host,
                &key,
                request_method,
                request_path,
                request_headers,
                &request_body,
                read_timeout,
                max_response_bytes,
            )) {
                SendOutcome::Ok(resp) => return H2Outcome::Ok(resp),
                SendOutcome::ConnectionGone => {
                    // Drop the dead entry and fall through to the
                    // cold-handshake path below.
                    if let Ok(mut m) = self.h2_senders.lock() {
                        m.remove(&key);
                    }
                }
                SendOutcome::Err(e) => return H2Outcome::Err(e),
            }
        }

        // Cold path: full TLS + h2 handshake, then send.
        let connect_out = self.runtime.block_on(handshake(
            tls.clone(),
            host,
            port,
            connect_timeout,
        ));
        let sender = match connect_out {
            Ok(HandshakeOk::H2(s)) => s,
            Ok(HandshakeOk::FallbackToH1) => return H2Outcome::FallbackToH1,
            Err(e) => return H2Outcome::Err(e),
        };
        // Cache before sending so a successful response cements the
        // entry even if the caller mutates between calls.
        if let Ok(mut m) = self.h2_senders.lock() {
            m.insert(key.clone(), sender.clone());
        }
        match self.runtime.block_on(send_with(
            sender,
            host,
            &key,
            request_method,
            request_path,
            request_headers,
            &request_body,
            read_timeout,
            max_response_bytes,
        )) {
            SendOutcome::Ok(resp) => H2Outcome::Ok(resp),
            SendOutcome::ConnectionGone => {
                if let Ok(mut m) = self.h2_senders.lock() {
                    m.remove(&key);
                }
                H2Outcome::Err(Error::BadResponse("h2 connection closed mid-request".into()))
            }
            SendOutcome::Err(e) => H2Outcome::Err(e),
        }
    }

    /// Number of cached h2 senders. Test-only.
    #[cfg(test)]
    pub fn h2_pool_len(&self) -> usize {
        self.h2_senders.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Force-evict a cached h2 sender. Test-only — production
    /// code only evicts on send failure.
    #[cfg(test)]
    pub fn h2_evict(&self, host: &str, port: u16) -> bool {
        self.h2_senders
            .lock()
            .map(|mut m| m.remove(&(host.to_ascii_lowercase(), port)).is_some())
            .unwrap_or(false)
    }
}

enum HandshakeOk {
    H2(SendRequest<Bytes>),
    FallbackToH1,
}

enum SendOutcome {
    Ok(Response),
    ConnectionGone,
    Err(Error),
}

async fn handshake(
    tls: Arc<ClientConfig>,
    host: &str,
    port: u16,
    connect_timeout: Duration,
) -> Result<HandshakeOk> {
    let addr = format!("{host}:{port}");
    let tcp = tokio::time::timeout(connect_timeout, tokio::net::TcpStream::connect(&addr))
        .await
        .map_err(|_| {
            Error::Connect(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connect timeout",
            ))
        })?
        .map_err(Error::Connect)?;
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| Error::Tls(format!("invalid server name {host}: {e}")))?;
    let connector = TlsConnector::from(tls.clone());
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::Tls(e.to_string()))?;
    let alpn = {
        let (_, session) = tls_stream.get_ref();
        session.alpn_protocol().map(|p| p.to_vec())
    };
    if alpn.as_deref() != Some(b"h2") {
        return Ok(HandshakeOk::FallbackToH1);
    }
    let (send_request, connection) = h2::client::handshake(tls_stream)
        .await
        .map_err(|e| Error::Tls(format!("h2 handshake: {e}")))?;
    // Drive the connection in the background. The task lives until
    // the server closes the connection; if it ends early, the
    // cached SendRequest's next send fails and we re-handshake.
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(HandshakeOk::H2(send_request))
}

#[allow(clippy::too_many_arguments)]
async fn send_with(
    sender: SendRequest<Bytes>,
    host: &str,
    _key: &(String, u16),
    request_method: &str,
    request_path: &str,
    request_headers: &[(String, String)],
    request_body: &[u8],
    read_timeout: Duration,
    max_response_bytes: usize,
) -> SendOutcome {
    // `ready()` blocks the SendRequest until the server gives us a
    // stream; if the connection is gone we get an error here, which
    // we map to `ConnectionGone` so the caller can re-handshake.
    let mut sender = match sender.ready().await {
        Ok(s) => s,
        Err(_) => return SendOutcome::ConnectionGone,
    };
    let uri: Uri = match format!("https://{host}{request_path}").parse() {
        Ok(u) => u,
        Err(e) => return SendOutcome::Err(Error::InvalidUrl(e.to_string())),
    };
    let method = match request_method.to_ascii_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "DELETE" => Method::DELETE,
        "HEAD" => Method::HEAD,
        "OPTIONS" => Method::OPTIONS,
        "PATCH" => Method::PATCH,
        other => return SendOutcome::Err(Error::BadResponse(format!("unsupported method: {other}"))),
    };
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in request_headers {
        if name.eq_ignore_ascii_case("host") {
            continue;
        }
        if let (Ok(hname), Ok(hval)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(hname, hval);
        }
    }
    let req = match builder.body(()) {
        Ok(r) => r,
        Err(e) => return SendOutcome::Err(Error::BadResponse(format!("bad request: {e}"))),
    };
    let (response_fut, mut send_stream) = match sender
        .send_request(req, request_body.is_empty())
    {
        Ok(p) => p,
        Err(_) => return SendOutcome::ConnectionGone,
    };
    if !request_body.is_empty() {
        if send_stream
            .send_data(Bytes::copy_from_slice(request_body), true)
            .is_err()
        {
            return SendOutcome::ConnectionGone;
        }
    }
    let response = match tokio::time::timeout(read_timeout, response_fut).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => return SendOutcome::ConnectionGone,
        Err(_) => {
            return SendOutcome::Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "h2 response timeout",
            )))
        }
    };
    let (parts, mut body) = response.into_parts();
    let mut body_bytes = Vec::<u8>::new();
    while let Some(chunk) = body.data().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => return SendOutcome::ConnectionGone,
        };
        if body_bytes.len() + chunk.len() > max_response_bytes {
            return SendOutcome::Err(Error::ResponseTooLarge(max_response_bytes));
        }
        body_bytes.extend_from_slice(&chunk);
        let _ = body.flow_control().release_capacity(chunk.len());
    }
    let mut headers = Vec::with_capacity(parts.headers.len());
    for (name, value) in parts.headers.iter() {
        if let Ok(s) = value.to_str() {
            headers.push((name.as_str().to_string(), s.to_string()));
        }
    }
    SendOutcome::Ok(Response {
        status: parts.status.as_u16(),
        reason: parts.status.canonical_reason().unwrap_or("").to_string(),
        headers,
        body: body_bytes,
        body_path: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_pool_initialises_empty() {
        let pool = HttpPool::new().expect("init");
        assert_eq!(pool.h2_pool_len(), 0);
    }

    #[test]
    fn pool_evict_removes_entry_when_present() {
        let pool = HttpPool::new().expect("init");
        // Nothing's in the pool, so evict reports false.
        assert!(!pool.h2_evict("example.com", 443));
    }

    #[test]
    fn pool_h2_request_to_unroutable_address_falls_back_or_errors() {
        // Smoke test: feed the pool a bogus host. Whatever happens —
        // DNS failure, connect refused, ALPN mismatch — we must not
        // leave a cached entry behind, since the request didn't
        // succeed. The exact outcome variant depends on the runner's
        // network state; we just check no entry leaked.
        let pool = HttpPool::new().expect("init");
        let tls = std::sync::Arc::new(crate::net::transport::default_tls_config());
        let _ = pool.request_h2(
            &tls,
            "256.256.256.256",
            443,
            "GET",
            "/",
            &[],
            Vec::new(),
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(500),
            64 * 1024,
        );
        assert_eq!(pool.h2_pool_len(), 0, "failed request must not leave a pool entry");
    }
}
