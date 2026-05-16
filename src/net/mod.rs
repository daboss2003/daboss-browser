mod cookies;
mod dns;
mod error;
mod http;
mod transport;

pub use self::cookies::CookieJar;
#[allow(unused_imports)] // re-exported for future tab-scoped storage hooks
pub use self::cookies::Cookie;
pub use self::error::{Error, Result};
pub use self::http::{Request, Response};

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use url::Url;

pub struct Client {
    tls: Arc<ClientConfig>,
    max_response_bytes: usize,
    read_timeout: Duration,
    connect_timeout: Duration,
    max_redirects: u32,
    allow_loopback: bool,
    /// Shared cookie jar for this user agent. Mutated under `RefCell` so
    /// the network code stays single-threaded but multiple high-level
    /// callers can share one `Client` instance.
    cookies: RefCell<CookieJar>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            tls: Arc::new(transport::default_tls_config()),
            max_response_bytes: 50 * 1024 * 1024,
            read_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_redirects: 10,
            allow_loopback: false,
            cookies: RefCell::new(CookieJar::new()),
        }
    }

    pub fn with_allow_loopback(mut self, allow: bool) -> Self {
        self.allow_loopback = allow;
        self
    }

    pub fn get(&self, url: &str) -> Result<Response> {
        let url = Url::parse(url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        self.do_request(url, Method::Get, 0)
    }

    pub fn post(&self, url: &str, body: Vec<u8>, content_type: &str) -> Result<Response> {
        let url = Url::parse(url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        self.do_request(
            url,
            Method::Post {
                body,
                content_type: content_type.to_string(),
            },
            0,
        )
    }

    fn do_request(&self, url: Url, method: Method, depth: u32) -> Result<Response> {
        if depth >= self.max_redirects {
            return Err(Error::TooManyRedirects(self.max_redirects));
        }

        let use_tls = match url.scheme() {
            "https" => true,
            "http" => false,
            other => return Err(Error::UnsupportedScheme(other.to_string())),
        };

        let host = url
            .host_str()
            .ok_or_else(|| Error::InvalidUrl("missing host".into()))?
            .to_string();
        let port = url
            .port_or_known_default()
            .unwrap_or(if use_tls { 443 } else { 80 });

        let addrs = dns::resolve(&host, port, self.allow_loopback)?;
        let addr = addrs[0];

        tracing::debug!(%url, %addr, use_tls, "connecting");

        let mut conn = transport::Connection::open(
            addr,
            &host,
            use_tls,
            &self.tls,
            self.connect_timeout,
            self.read_timeout,
        )?;

        let path = build_path(&url);
        let mut request = match &method {
            Method::Get => Request::get(&host, &path),
            Method::Post { body, content_type } => {
                Request::post(&host, &path, body.clone(), content_type)
            }
        };
        // Attach any matching cookies from the jar.
        if let Some(cookie_header) = self.cookies.borrow().header_for(&url) {
            request
                .headers
                .push(("Cookie".to_string(), cookie_header));
        }
        request.write_to(&mut conn)?;

        let response = Response::read_from(conn, self.max_response_bytes)?;

        // Ingest any Set-Cookie headers before deciding whether to follow
        // a redirect — the spec scopes Set-Cookie to the redirect's
        // *origin* (the response we just got).
        {
            let iter = response
                .headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()));
            self.cookies.borrow_mut().ingest_set_cookies(&url, iter);
        }

        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            if let Some(location) = response.header("Location") {
                let next = url
                    .join(location)
                    .map_err(|e| Error::InvalidUrl(format!("bad redirect location: {e}")))?;
                tracing::info!(from = %url, to = %next, status = response.status, "redirect");
                // Per HTTP spec: 303 always switches to GET; 301/302 are
                // conventionally treated the same way by browsers for POST
                // submissions. 307/308 preserve the method.
                let follow_method = match (response.status, &method) {
                    (303, _) | (301 | 302, Method::Post { .. }) => Method::Get,
                    _ => method,
                };
                return self.do_request(next, follow_method, depth + 1);
            }
        }

        Ok(response)
    }
}

enum Method {
    Get,
    Post {
        body: Vec<u8>,
        content_type: String,
    },
}

fn build_path(url: &Url) -> String {
    let path = url.path();
    match url.query() {
        Some(q) => format!("{path}?{q}"),
        None if path.is_empty() => "/".to_string(),
        None => path.to_string(),
    }
}
