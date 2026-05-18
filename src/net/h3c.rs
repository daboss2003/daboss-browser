//! HTTP/3 client path, built on `quinn` (QUIC transport) + `h3`.
//!
//! Mirrors the structure of [`h2c::request_h2`]: a synchronous entry
//! point [`request_h3`] that drives the async stack inside a fresh
//! single-threaded tokio runtime. QUIC negotiation happens via TLS
//! ALPN `"h3"` over UDP; if the server doesn't accept QUIC at all we
//! report a fallback so the caller can retry on HTTP/2 or HTTP/1.1.
//!
//! Why per-request runtime: same trade-off as HTTP/2 — we get a
//! self-contained handshake + request + close per call, at the cost
//! of redoing the QUIC handshake every time. A persistent runtime +
//! connection pool would be a later refinement (and meaningful for
//! HTTP/3, since QUIC handshake is heavier than TCP).
//!
//! Scope cut for the toy:
//!   * No Alt-Svc / discovery. Caller decides to try HTTP/3.
//!   * No 0-RTT.
//!   * No connection migration.
//!   * Body returned as a single `Vec<u8>` (no streaming response).

use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use http::{HeaderName, HeaderValue, Method, Request, Uri};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::ClientConfig;
use rustls::pki_types::ServerName;

use super::error::{Error, Result};
use super::http::Response;

pub enum H3Outcome {
    /// HTTP/3 succeeded; carries the parsed response.
    Ok(Response),
    /// QUIC handshake failed / server doesn't speak h3. Caller should
    /// retry on the HTTP/2 or HTTP/1.1 transport.
    Fallback,
    /// Hard failure — same semantics as the sync path's `Error`.
    Err(Error),
}

pub fn request_h3(
    tls: &Arc<rustls::ClientConfig>,
    host: &str,
    port: u16,
    request_method: &str,
    request_path: &str,
    request_headers: &[(String, String)],
    request_body: Vec<u8>,
    connect_timeout: Duration,
    read_timeout: Duration,
    max_response_bytes: usize,
) -> H3Outcome {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return H3Outcome::Err(Error::Io(e)),
    };
    runtime.block_on(async move {
        match run_h3(
            tls,
            host,
            port,
            request_method,
            request_path,
            request_headers,
            request_body,
            connect_timeout,
            read_timeout,
            max_response_bytes,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => H3Outcome::Err(e),
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_h3(
    tls: &Arc<rustls::ClientConfig>,
    host: &str,
    port: u16,
    request_method: &str,
    request_path: &str,
    request_headers: &[(String, String)],
    request_body: Vec<u8>,
    connect_timeout: Duration,
    read_timeout: Duration,
    max_response_bytes: usize,
) -> Result<H3Outcome> {
    // We need a separate rustls config with ALPN set to `h3` for
    // QUIC. Clone the caller's roots so cert validation behaviour
    // matches the rest of the stack.
    let mut quic_tls = (**tls).clone();
    quic_tls.alpn_protocols = vec![b"h3".to_vec()];
    let quic_crypto = match QuicClientConfig::try_from(quic_tls) {
        Ok(c) => c,
        Err(e) => return Ok(H3Outcome::Err(Error::Tls(e.to_string()))),
    };
    let client_config = ClientConfig::new(Arc::new(quic_crypto));

    // Resolve to a (UDP) addr and bind a wildcard local socket.
    let server_addr = match tokio::net::lookup_host((host, port)).await {
        Ok(mut addrs) => match addrs.next() {
            Some(a) => a,
            None => return Ok(H3Outcome::Fallback),
        },
        Err(_) => return Ok(H3Outcome::Fallback),
    };
    let bind_addr = if server_addr.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    let mut endpoint = match quinn::Endpoint::client(bind_addr) {
        Ok(e) => e,
        Err(e) => return Ok(H3Outcome::Err(Error::Io(e))),
    };
    endpoint.set_default_client_config(client_config);

    let _server_name = ServerName::try_from(host.to_string())
        .map_err(|e| Error::Tls(format!("invalid server name {host}: {e}")))?;
    let connecting = match endpoint.connect(server_addr, host) {
        Ok(c) => c,
        Err(_) => return Ok(H3Outcome::Fallback),
    };
    let quinn_conn = match tokio::time::timeout(connect_timeout, connecting).await {
        Ok(Ok(c)) => c,
        // Any connect failure → fall back (server may not support
        // QUIC, or UDP may be blocked locally).
        Ok(Err(_)) | Err(_) => {
            endpoint.close(0u32.into(), b"giving up");
            return Ok(H3Outcome::Fallback);
        }
    };

    // Build the h3 client over the QUIC connection.
    let h3_quinn_conn = h3_quinn::Connection::new(quinn_conn);
    let (mut driver, mut send_request) =
        match h3::client::new(h3_quinn_conn).await {
            Ok(pair) => pair,
            Err(_) => {
                endpoint.close(0u32.into(), b"h3 new failed");
                return Ok(H3Outcome::Fallback);
            }
        };
    let driver_task = tokio::spawn(async move {
        // Drive the connection until either side closes it. Errors
        // are surfaced to in-flight requests via their stream.
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });

    // Build the HTTP request.
    let uri: Uri = format!("https://{host}{request_path}")
        .parse()
        .map_err(|e: http::uri::InvalidUri| Error::BadResponse(e.to_string()))?;
    let mut req = Request::builder()
        .method(
            Method::from_bytes(request_method.as_bytes())
                .map_err(|e| Error::BadResponse(e.to_string()))?,
        )
        .uri(uri);
    {
        let headers = req
            .headers_mut()
            .expect("Request::builder().headers_mut() returns Some on a fresh builder");
        for (k, v) in request_headers {
            if let (Ok(name), Ok(value)) =
                (HeaderName::try_from(k.as_str()), HeaderValue::from_str(v))
            {
                headers.insert(name, value);
            }
        }
    }
    let body_bytes = Bytes::from(request_body);
    let req = match req.body(()) {
        Ok(r) => r,
        Err(e) => return Ok(H3Outcome::Err(Error::BadResponse(e.to_string()))),
    };

    let mut stream = match tokio::time::timeout(read_timeout, send_request.send_request(req)).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(_)) | Err(_) => {
            driver_task.abort();
            endpoint.close(0u32.into(), b"send_request");
            return Ok(H3Outcome::Fallback);
        }
    };
    if !body_bytes.is_empty() {
        if stream.send_data(body_bytes).await.is_err() {
            driver_task.abort();
            endpoint.close(0u32.into(), b"send_data");
            return Ok(H3Outcome::Err(Error::BadResponse("send_data".into())));
        }
    }
    if stream.finish().await.is_err() {
        driver_task.abort();
        endpoint.close(0u32.into(), b"finish");
        return Ok(H3Outcome::Err(Error::BadResponse("finish".into())));
    }

    let response = match tokio::time::timeout(read_timeout, stream.recv_response()).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) | Err(_) => {
            driver_task.abort();
            endpoint.close(0u32.into(), b"recv_response");
            return Ok(H3Outcome::Err(Error::BadResponse("recv_response".into())));
        }
    };
    let status = response.status();
    let headers_out: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    // Drain body until EOF or size cap.
    let mut body = Vec::new();
    loop {
        match tokio::time::timeout(read_timeout, stream.recv_data()).await {
            Ok(Ok(Some(mut chunk))) => {
                while chunk.has_remaining() {
                    let n = chunk.chunk().len();
                    body.extend_from_slice(chunk.chunk());
                    chunk.advance(n);
                }
                if body.len() > max_response_bytes {
                    driver_task.abort();
                    endpoint.close(0u32.into(), b"body too large");
                    return Ok(H3Outcome::Err(Error::BadResponse(format!(
                        "response exceeded size cap of {max_response_bytes} bytes"
                    ))));
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(_)) | Err(_) => {
                driver_task.abort();
                endpoint.close(0u32.into(), b"recv_data");
                return Ok(H3Outcome::Err(Error::BadResponse("recv_data".into())));
            }
        }
    }

    endpoint.close(0u32.into(), b"done");
    // Wait briefly for connection cleanup so the next request on the
    // same port works.
    endpoint.wait_idle().await;
    driver_task.abort();

    let resp = Response {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or("").to_string(),
        headers: headers_out,
        body,
    };
    Ok(H3Outcome::Ok(resp))
}
