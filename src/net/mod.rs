mod adblock;
mod cache;
mod cookies;
mod csp;
mod dns;
mod error;
mod h2c;
mod h3c;
mod http;
pub(crate) mod transport;

pub use self::adblock::Blocklist;
pub use self::cookies::CookieJar;
#[allow(unused_imports)] // re-exported for future tab-scoped storage hooks
pub use self::cookies::Cookie;
pub use self::csp::Csp;
pub use self::error::{Error, Result};
pub use self::http::{Request, Response};
// `RequestContext` is defined directly in this module — see below.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use url::Url;

/// Caller-supplied context for an outgoing request. Carries the security
/// state of whoever initiated the call so the network layer can enforce
/// CORS, mixed content blocking, and Referrer-Policy.
#[derive(Default, Clone, Debug)]
pub struct RequestContext {
    /// URL of the page that originated the request. Used to compute the
    /// `Referer` header and to detect cross-origin / mixed-content cases.
    pub initiator: Option<Url>,
    /// `true` when this is a fetch / XHR call from JS (or anything
    /// otherwise gated by CORS). Cross-origin reads fail unless the
    /// response carries a permissive `Access-Control-Allow-Origin`.
    pub cors_required: bool,
}

impl RequestContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_initiator(mut self, url: Url) -> Self {
        self.initiator = Some(url);
        self
    }

    pub fn with_cors(mut self, on: bool) -> Self {
        self.cors_required = on;
        self
    }
}

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
    /// Hostname blocker — short-circuits ad/tracker requests before any
    /// DNS or TCP work. Set via [`Client::with_blocklist`].
    blocklist: Blocklist,
    /// HSTS host set — every host we've seen with a
    /// `Strict-Transport-Security` response header. Future `http://`
    /// requests to these hosts are upgraded to `https://`.
    hsts: RefCell<HashSet<String>>,
    /// In-memory HTTP cache. Stores GET responses that opt in via
    /// `Cache-Control` and validates with `ETag` / `If-None-Match` on
    /// stale-with-validator hits.
    cache: RefCell<cache::HttpCache>,
    /// HTTP/1.1 keep-alive connection pool. Keyed by
    /// `(host_lower, port, use_tls)`. After a successful response the
    /// transport reads its buffered reader back into the pool unless
    /// the server (or this side, on an error) requested close.
    pool: RefCell<
        std::collections::HashMap<(String, u16, bool), Vec<std::io::BufReader<transport::Connection>>>,
    >,
    /// Hosts known to prefer HTTP/1.1 (we tried h2 ALPN and the server
    /// negotiated http/1.1 or refused). Keyed by `(host_lower, port)`.
    h2_blacklist: RefCell<HashSet<(String, u16)>>,
    /// Hosts that refused our HTTP/3 attempt — or for which QUIC is
    /// blocked locally. Same shape / lifetime as `h2_blacklist`. We
    /// only try h3 once per host before falling back permanently for
    /// the lifetime of the client.
    h3_blacklist: RefCell<HashSet<(String, u16)>>,
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
            blocklist: Blocklist::default_bundled(),
            hsts: RefCell::new(HashSet::new()),
            cache: RefCell::new(cache::HttpCache::new()),
            pool: RefCell::new(std::collections::HashMap::new()),
            h2_blacklist: RefCell::new(HashSet::new()),
            h3_blacklist: RefCell::new(HashSet::new()),
        }
    }

    pub fn with_allow_loopback(mut self, allow: bool) -> Self {
        self.allow_loopback = allow;
        self
    }

    /// Swap in a different blocklist (e.g. an empty one for tests).
    #[allow(dead_code)]
    pub fn with_blocklist(mut self, bl: Blocklist) -> Self {
        self.blocklist = bl;
        self
    }

    /// Build the `Cookie:` header value that we'd send to `url` right
    /// now. Exposed so the JS subsystem can implement `document.cookie`
    /// readback. Returns an empty string if nothing matches.
    pub fn cookies_for(&self, url: &Url) -> String {
        self.cookies
            .borrow()
            .header_for(url)
            .unwrap_or_default()
    }

    /// Parse a `Set-Cookie`-style string and insert into the jar with
    /// `url` as the scope. Backs `document.cookie = "..."` writes.
    pub fn set_cookie_for(&self, url: &Url, set_cookie: &str) {
        if let Some(cookie) = cookies::parse_set_cookie(set_cookie, url) {
            self.cookies.borrow_mut().insert(cookie);
        }
    }

    pub fn get(&self, url: &str) -> Result<Response> {
        self.get_with(url, RequestContext::default())
    }

    pub fn post(&self, url: &str, body: Vec<u8>, content_type: &str) -> Result<Response> {
        self.post_with(url, body, content_type, RequestContext::default())
    }

    /// `get` with a security context (CORS / Referer / mixed-content
    /// checks). Use this from fetch / XHR call sites so cross-origin
    /// behaviour is correct.
    pub fn get_with(&self, url: &str, context: RequestContext) -> Result<Response> {
        let url = Url::parse(url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        self.do_request(url, Method::Get, 0, &context)
    }

    /// `post` with a security context.
    pub fn post_with(
        &self,
        url: &str,
        body: Vec<u8>,
        content_type: &str,
        context: RequestContext,
    ) -> Result<Response> {
        let url = Url::parse(url).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        self.do_request(
            url,
            Method::Post {
                body,
                content_type: content_type.to_string(),
            },
            0,
            &context,
        )
    }

    fn open_connection(
        &self,
        host: &str,
        port: u16,
        use_tls: bool,
    ) -> Result<std::io::BufReader<transport::Connection>> {
        let addrs = dns::resolve(host, port, self.allow_loopback)?;
        let addr = addrs[0];
        tracing::debug!(host, port, use_tls, "opening fresh connection");
        let conn = transport::Connection::open(
            addr,
            host,
            use_tls,
            &self.tls,
            self.connect_timeout,
            self.read_timeout,
        )?;
        Ok(std::io::BufReader::new(conn))
    }

    fn send_via_pool(
        &self,
        key: &(String, u16, bool),
        host: &str,
        port: u16,
        use_tls: bool,
        request: &Request,
    ) -> Result<Response> {
        // Try a pooled connection first.
        let pooled = self
            .pool
            .borrow_mut()
            .get_mut(key)
            .and_then(|v| v.pop());
        if let Some(mut br) = pooled {
            if let Ok(resp) = Self::send_and_recv(&mut br, request, self.max_response_bytes) {
                self.return_to_pool(key, br, &resp);
                return Ok(resp);
            }
            // Pooled connection was stale — drop and fall through.
            tracing::debug!(?key, "pool: stale connection, retrying fresh");
        }
        // Open fresh.
        let mut br = self.open_connection(host, port, use_tls)?;
        let resp = Self::send_and_recv(&mut br, request, self.max_response_bytes)?;
        self.return_to_pool(key, br, &resp);
        Ok(resp)
    }

    fn send_and_recv(
        br: &mut std::io::BufReader<transport::Connection>,
        request: &Request,
        max_bytes: usize,
    ) -> Result<Response> {
        request.write_to(br.get_mut())?;
        Response::read_from_buf(br, max_bytes)
    }

    fn return_to_pool(
        &self,
        key: &(String, u16, bool),
        br: std::io::BufReader<transport::Connection>,
        response: &Response,
    ) {
        // If the server (or our request) said `Connection: close`, drop.
        let close = response
            .header("Connection")
            .map(|v| v.eq_ignore_ascii_case("close"))
            .unwrap_or(false);
        if close {
            return;
        }
        // Don't reuse on weird statuses — be conservative.
        if !(200..500).contains(&response.status) {
            return;
        }
        let mut pool = self.pool.borrow_mut();
        let bucket = pool.entry(key.clone()).or_default();
        // Cap per-host pool size — most browsers limit to 6.
        if bucket.len() < 6 {
            bucket.push(br);
        }
    }

    fn do_request(
        &self,
        url: Url,
        method: Method,
        depth: u32,
        context: &RequestContext,
    ) -> Result<Response> {
        if depth >= self.max_redirects {
            return Err(Error::TooManyRedirects(self.max_redirects));
        }

        // HSTS upgrade — if a host previously sent
        // `Strict-Transport-Security` we never speak HTTP to it again.
        let mut url = url;
        if url.scheme() == "http" {
            let host_lower = url.host_str().map(str::to_ascii_lowercase);
            if let Some(host) = host_lower {
                let hit = self.hsts.borrow().contains(&host);
                if hit {
                    let _ = url.set_scheme("https");
                    tracing::info!(%host, "HSTS upgrade to https");
                }
            }
        }

        // Mixed-content block — an HTTPS initiator must not pull a
        // subresource over plain HTTP.
        if let Some(init) = &context.initiator {
            if init.scheme() == "https" && url.scheme() == "http" {
                tracing::warn!(%init, %url, "mixed content blocked");
                return Err(Error::MixedContent(url.to_string()));
            }
        }

        // Hostname-based ad blocker — short-circuit before any DNS /
        // TCP work for entries on the bundled blocklist.
        if self.blocklist.is_blocked(&url) {
            tracing::info!(%url, "blocked by adblock");
            return Err(Error::Blocked(url.host_str().unwrap_or("").to_string()));
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

        // HTTP cache lookup — only meaningful for GETs.
        let cache_key = url.to_string();
        let is_get = matches!(method, Method::Get);
        if is_get {
            let now = std::time::Instant::now();
            let cached_meta = self.cache.borrow_mut().lookup(&cache_key).cloned();
            if let Some(entry) = cached_meta {
                if entry.is_fresh(now) {
                    tracing::debug!(%url, "cache: fresh hit");
                    if let Some((body, body_path)) =
                        self.cache.borrow_mut().read_body_for_response(&cache_key)
                    {
                        return Ok(Response {
                            status: entry.status,
                            reason: entry.reason,
                            headers: entry.headers,
                            body,
                            body_path,
                        });
                    }
                }
            }
        }

        let path = build_path(&url);
        let mut request = match &method {
            Method::Get => Request::get(&host, &path),
            Method::Post { body, content_type } => {
                Request::post(&host, &path, body.clone(), content_type)
            }
        };

        // If we have a stale-with-validator entry, send the validators.
        if is_get {
            let entry = self.cache.borrow_mut().lookup(&cache_key).cloned();
            if let Some(entry) = entry {
                if let Some(etag) = entry.etag {
                    request.headers.push(("If-None-Match".to_string(), etag));
                }
                if let Some(lm) = entry.last_modified {
                    request
                        .headers
                        .push(("If-Modified-Since".to_string(), lm));
                }
            }
        }
        // Attach any matching cookies from the jar.
        if let Some(cookie_header) = self.cookies.borrow().header_for(&url) {
            request
                .headers
                .push(("Cookie".to_string(), cookie_header));
        }

        // Referer / Origin headers from the initiator. We use the
        // strict-origin-when-cross-origin policy: full URL same-origin,
        // origin only cross-origin, nothing when downgrading.
        if let Some(initiator) = &context.initiator {
            let same_origin = url.origin() == initiator.origin();
            let downgrade = initiator.scheme() == "https" && url.scheme() == "http";
            if !downgrade {
                let referer = if same_origin {
                    initiator.to_string()
                } else {
                    initiator.origin().ascii_serialization() + "/"
                };
                request.headers.push(("Referer".to_string(), referer));
            }
            if context.cors_required && !same_origin {
                let origin = initiator.origin().ascii_serialization();
                request.headers.push(("Origin".to_string(), origin));
            }
        }

        // Try HTTP/3 → HTTP/2 → HTTP/1.1 in order for HTTPS hosts.
        // Each tier blacklists the host on failure for the rest of
        // the client's lifetime so subsequent requests skip straight
        // to whichever protocol the server actually speaks.
        let pool_key = (host.to_ascii_lowercase(), port, use_tls);
        let h2_key = (host.to_ascii_lowercase(), port);
        let h3_key = (host.to_ascii_lowercase(), port);

        // 1) HTTP/3 over QUIC.
        let h3_response = if use_tls && !self.h3_blacklist.borrow().contains(&h3_key) {
            match h3c::request_h3(
                &self.tls,
                &host,
                port,
                &request.method,
                &request.path,
                &request.headers,
                request.body.clone(),
                self.connect_timeout,
                self.read_timeout,
                self.max_response_bytes,
            ) {
                h3c::H3Outcome::Ok(resp) => Some(resp),
                h3c::H3Outcome::Fallback => {
                    self.h3_blacklist.borrow_mut().insert(h3_key.clone());
                    None
                }
                h3c::H3Outcome::Err(e) => {
                    tracing::debug!(?e, "h3 request failed; falling back");
                    self.h3_blacklist.borrow_mut().insert(h3_key.clone());
                    None
                }
            }
        } else {
            None
        };

        // 2) HTTP/2 if h3 didn't take.
        let response = if let Some(r) = h3_response {
            r
        } else if use_tls && !self.h2_blacklist.borrow().contains(&h2_key) {
            let h2_response = match h2c::request_h2(
                &self.tls,
                &host,
                port,
                &request.method,
                &request.path,
                &request.headers,
                request.body.clone(),
                self.connect_timeout,
                self.read_timeout,
                self.max_response_bytes,
            ) {
                h2c::H2Outcome::Ok(resp) => Some(resp),
                h2c::H2Outcome::FallbackToH1 => {
                    self.h2_blacklist.borrow_mut().insert(h2_key.clone());
                    None
                }
                h2c::H2Outcome::Err(e) => {
                    tracing::debug!(?e, "h2 request failed; falling back to h1");
                    self.h2_blacklist.borrow_mut().insert(h2_key.clone());
                    None
                }
            };
            match h2_response {
                Some(r) => r,
                None => self.send_via_pool(&pool_key, &host, port, use_tls, &request)?,
            }
        } else {
            // 3) Plain HTTP/1.1.
            self.send_via_pool(&pool_key, &host, port, use_tls, &request)?
        };

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

        // Strict-Transport-Security — once a host opts in over HTTPS,
        // remember it for the rest of the run. We ignore `max-age` /
        // `includeSubDomains` parsing for the toy; the presence of the
        // header is the only signal we honour.
        if url.scheme() == "https" && response.header("Strict-Transport-Security").is_some() {
            if let Some(host) = url.host_str() {
                self.hsts.borrow_mut().insert(host.to_ascii_lowercase());
            }
        }

        // HTTP cache write / 304 refresh — only for GET.
        if is_get {
            if response.status == 304 {
                let refreshed = self
                    .cache
                    .borrow_mut()
                    .refresh_after_304(&cache_key, &response.headers);
                if let Some((body, body_path)) = refreshed {
                    tracing::debug!(%url, "cache: 304 refresh");
                    // Substitute the cached body but keep the new
                    // response's headers so the cascade up the stack
                    // (CORS / Set-Cookie / etc.) sees the latest.
                    return Ok(Response {
                        status: 200,
                        reason: "OK".into(),
                        headers: response.headers.clone(),
                        body,
                        body_path,
                    });
                }
            } else if (200..400).contains(&response.status) {
                self.cache.borrow_mut().store(&cache_key, &response);
            }
        }

        // CORS enforcement for fetch / XHR. Same-origin requests skip
        // the check entirely.
        if let Some(initiator) = &context.initiator {
            if context.cors_required && initiator.origin() != url.origin() {
                let allow = response.header("Access-Control-Allow-Origin");
                let want = initiator.origin().ascii_serialization();
                let permitted = match allow {
                    Some("*") => true,
                    Some(v) => v.trim().eq_ignore_ascii_case(&want),
                    None => false,
                };
                if !permitted {
                    tracing::warn!(
                        %url,
                        %want,
                        allow = ?allow,
                        "blocked cross-origin response without permissive CORS",
                    );
                    return Err(Error::Cors(url.to_string()));
                }
            }
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
                return self.do_request(next, follow_method, depth + 1, context);
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
