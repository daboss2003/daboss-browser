//! HTTP/2 client path.
//!
//! Synchronous entry point [`request_h2`] that drives the `h2` crate
//! inside a fresh single-threaded tokio runtime built per request.
//! We do an async TLS handshake via `tokio-rustls` with
//! `["h2", "http/1.1"]` ALPN; if the server picks `h2` we run the
//! request, otherwise we report a fallback so the caller can retry on
//! the existing sync HTTP/1.1 transport.
//!
//! Per-request runtime is wasteful — there's one TLS handshake + h2
//! `SETTINGS` exchange every time — but it lets HTTP/2 ride next to
//! the existing sync net code without converting the whole stack to
//! async. A persistent runtime + connection pool would be a later
//! refinement.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Request, Uri};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

use super::error::{Error, Result};
use super::http::Response;

pub enum H2Outcome {
    /// h2 was negotiated and we got a response back.
    Ok(Response),
    /// TLS handshake worked but ALPN picked `http/1.1` (or nothing).
    /// Caller should retry on the sync transport.
    FallbackToH1,
    /// Hard failure — same semantics as the sync path's `Error`.
    Err(Error),
}

pub fn request_h2(
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
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return H2Outcome::Err(Error::Io(e)),
    };
    runtime.block_on(async move {
        match run_h2(
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
            Err(e) => H2Outcome::Err(e),
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_h2(
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
) -> Result<H2Outcome> {
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

    // Inspect ALPN. If the server didn't pick h2, fall back so the
    // caller can use the existing sync HTTP/1.1 transport.
    let alpn = {
        let (_, session) = tls_stream.get_ref();
        session.alpn_protocol().map(|p| p.to_vec())
    };
    if alpn.as_deref() != Some(b"h2") {
        return Ok(H2Outcome::FallbackToH1);
    }

    let handshake = h2::client::handshake(tls_stream)
        .await
        .map_err(|e| Error::Tls(format!("h2 handshake: {e}")))?;
    let (send_request, connection) = handshake;
    // Drive the h2 connection in the background until the response is
    // fully read.
    let conn_task = tokio::spawn(async move {
        let _ = connection.await;
    });

    let uri: Uri = format!("https://{host}{request_path}")
        .parse()
        .map_err(|e: http::uri::InvalidUri| Error::InvalidUrl(e.to_string()))?;
    let method = match request_method.to_ascii_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "DELETE" => Method::DELETE,
        "HEAD" => Method::HEAD,
        "OPTIONS" => Method::OPTIONS,
        "PATCH" => Method::PATCH,
        other => return Err(Error::BadResponse(format!("unsupported method: {other}"))),
    };

    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in request_headers {
        // `Host` is implicit in h2 via :authority — drop it.
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
    let req = builder
        .body(())
        .map_err(|e| Error::BadResponse(format!("bad request: {e}")))?;

    let mut send_request = send_request
        .ready()
        .await
        .map_err(|e| Error::BadResponse(format!("h2 not ready: {e}")))?;
    let (response_fut, mut send_stream) = send_request
        .send_request(req, request_body.is_empty())
        .map_err(|e| Error::BadResponse(format!("h2 send: {e}")))?;
    if !request_body.is_empty() {
        send_stream
            .send_data(Bytes::from(request_body), true)
            .map_err(|e| Error::BadResponse(format!("h2 send_data: {e}")))?;
    }

    let response = tokio::time::timeout(read_timeout, response_fut)
        .await
        .map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "h2 response timeout",
            ))
        })?
        .map_err(|e| Error::BadResponse(format!("h2 recv: {e}")))?;
    let (parts, mut body) = response.into_parts();

    let mut body_bytes = Vec::<u8>::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|e| Error::BadResponse(format!("h2 body: {e}")))?;
        if body_bytes.len() + chunk.len() > max_response_bytes {
            return Err(Error::ResponseTooLarge(max_response_bytes));
        }
        body_bytes.extend_from_slice(&chunk);
        // Refresh flow-control window. Cheap because we just received.
        let _ = body.flow_control().release_capacity(chunk.len());
    }

    // Tear down the connection task — it'll exit cleanly once the
    // server closes.
    conn_task.abort();

    let mut headers = Vec::with_capacity(parts.headers.len());
    for (name, value) in parts.headers.iter() {
        if let Ok(s) = value.to_str() {
            headers.push((name.as_str().to_string(), s.to_string()));
        }
    }

    Ok(H2Outcome::Ok(Response {
        status: parts.status.as_u16(),
        reason: parts.status.canonical_reason().unwrap_or("").to_string(),
        headers,
        body: body_bytes,
    }))
}
