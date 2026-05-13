mod dns;
mod error;
mod http;
mod transport;

pub use self::error::{Error, Result};
pub use self::http::{Request, Response};

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
        }
    }

    pub fn with_allow_loopback(mut self, allow: bool) -> Self {
        self.allow_loopback = allow;
        self
    }

    pub fn get(&self, url: &str) -> Result<Response> {
        let url = Url::parse(url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        self.get_url(url, 0)
    }

    fn get_url(&self, url: Url, depth: u32) -> Result<Response> {
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
        let request = Request::get(&host, &path);
        request.write_to(&mut conn)?;

        let response = Response::read_from(conn, self.max_response_bytes)?;

        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            if let Some(location) = response.header("Location") {
                let next = url
                    .join(location)
                    .map_err(|e| Error::InvalidUrl(format!("bad redirect location: {e}")))?;
                tracing::info!(from = %url, to = %next, status = response.status, "redirect");
                return self.get_url(next, depth + 1);
            }
        }

        Ok(response)
    }
}

fn build_path(url: &Url) -> String {
    let path = url.path();
    match url.query() {
        Some(q) => format!("{path}?{q}"),
        None if path.is_empty() => "/".to_string(),
        None => path.to_string(),
    }
}
