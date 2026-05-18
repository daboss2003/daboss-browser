mod adblock;
mod cache;
mod cookies;
mod csp;
mod dns;
mod error;
mod h2c;
mod h3c;
mod http;
pub mod permissions_policy;
pub mod sri;
pub(crate) mod transport;

pub use self::adblock::Blocklist;
pub use self::cookies::CookieJar;
#[allow(unused_imports)] // re-exported for future tab-scoped storage hooks
pub use self::cookies::Cookie;
pub use self::csp::Csp;
pub use self::error::{Error, Result};
pub use self::http::{Request, Response};
pub use self::permissions_policy::PermissionsPolicy;
pub use self::sri::{verify_integrity, SriVerdict};
// `RequestContext` is defined directly in this module — see below.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use url::Url;

/// Caller-supplied context for an outgoing request. Carries the security
/// state of whoever initiated the call so the network layer can enforce
/// CORS, mixed content blocking, Referrer-Policy, and SameSite.
#[derive(Default, Clone, Debug)]
pub struct RequestContext {
    /// URL of the page that originated the request. Used to compute the
    /// `Referer` header and to detect cross-origin / mixed-content cases.
    pub initiator: Option<Url>,
    /// `true` when this is a fetch / XHR call from JS (or anything
    /// otherwise gated by CORS). Cross-origin reads fail unless the
    /// response carries a permissive `Access-Control-Allow-Origin`.
    pub cors_required: bool,
    /// `true` when this is a user-initiated top-level navigation
    /// (typed URL, link click) rather than a subresource fetch.
    /// Affects which `SameSite=Lax` cookies are sent on the request
    /// — Lax cookies travel with top-level GETs but not with
    /// subresource loads.
    pub is_top_level_navigation: bool,
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

    pub fn with_top_level_navigation(mut self, on: bool) -> Self {
        self.is_top_level_navigation = on;
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
    /// When `true`, drop all cookies (both Cookie header on outgoing
    /// cross-site subresource requests, and incoming Set-Cookie from
    /// cross-site subresource responses). Matches Safari/Firefox
    /// default behaviour as of 2024. Top-level navigations and
    /// same-site subresources are unaffected.
    block_third_party_cookies: bool,
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
            block_third_party_cookies: true,
        }
    }

    pub fn with_allow_loopback(mut self, allow: bool) -> Self {
        self.allow_loopback = allow;
        self
    }

    /// Toggle third-party cookie blocking. Default `true` — matches
    /// modern Safari/Firefox. Set `false` for sites that depend on
    /// cross-site auth cookies (federated SSO, embedded chat).
    #[allow(dead_code)]
    pub fn with_third_party_cookies_blocked(mut self, on: bool) -> Self {
        self.block_third_party_cookies = on;
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
    /// readback. `document.cookie` reads always come from a script
    /// running on the same page as the cookie's destination, so we
    /// treat them as same-site / top-level / GET — every applicable
    /// cookie is visible.
    pub fn cookies_for(&self, url: &Url) -> String {
        self.cookies
            .borrow()
            .header_for(url, Some(url), true, true)
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
                    // Re-run CORS against the cached response's
                    // headers before serving. Without this a cache
                    // populated by a same-origin or no-CORS request
                    // could leak a private body to a later
                    // cross-origin reader.
                    if !cors_allows(&entry.headers, &url, context) {
                        tracing::warn!(
                            %url,
                            "cache: fresh hit blocked by CORS recheck",
                        );
                        return Err(Error::Cors(url.to_string()));
                    }
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
        // Attach any matching cookies from the jar. SameSite + safe-
        // method gating relies on the caller flagging whether this is
        // a top-level navigation and which HTTP method is in play.
        // Third-party cookie blocking sits on top: cross-site
        // subresource requests get NO cookies at all (regardless of
        // SameSite=None), absent an explicit opt-out.
        let is_safe_method = matches!(method, Method::Get);
        let third_party = is_third_party_subresource(&url, context);
        if !(self.block_third_party_cookies && third_party) {
            if let Some(cookie_header) = self.cookies.borrow().header_for(
                &url,
                context.initiator.as_ref(),
                context.is_top_level_navigation,
                is_safe_method,
            ) {
                request
                    .headers
                    .push(("Cookie".to_string(), cookie_header));
            }
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
        // *origin* (the response we just got). Third-party blocking
        // also applies here: a cross-site subresource response can't
        // plant a cookie either.
        if !(self.block_third_party_cookies && third_party) {
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

        // CORS enforcement happens BEFORE writing the cache so a
        // CORS-blocked response can't poison the cache for a later
        // same-origin reader.
        if !cors_allows(&response.headers, &url, context) {
            let want = context
                .initiator
                .as_ref()
                .map(|i| i.origin().ascii_serialization())
                .unwrap_or_default();
            tracing::warn!(
                %url,
                %want,
                "blocked cross-origin response without permissive CORS",
            );
            return Err(Error::Cors(url.to_string()));
        }

        // HTTP cache write / 304 refresh — only for GET. We do this
        // AFTER CORS so a CORS-blocked response never makes it onto
        // disk.
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
                    // (Set-Cookie / etc.) sees the latest.
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

/// True for cross-site subresource requests — the case
/// third-party cookie blocking targets. A request is "third
/// party" when the destination's registrable domain differs
/// from the initiator's AND it isn't a top-level navigation
/// (top-level nav cookies always flow so federated SSO,
/// link-out tracking, etc. survive).
fn is_third_party_subresource(url: &Url, context: &RequestContext) -> bool {
    let Some(initiator) = &context.initiator else {
        return false;
    };
    if context.is_top_level_navigation {
        return false;
    }
    !cookies::same_site_urls(url, initiator)
}

/// Whether response headers permit the current request under CORS.
/// Same-origin / no-CORS requests always pass. For cross-origin
/// fetch / XHR, the response must carry an
/// `Access-Control-Allow-Origin` of `*` or the initiator's origin.
fn cors_allows(headers: &[(String, String)], url: &Url, context: &RequestContext) -> bool {
    let Some(initiator) = &context.initiator else {
        return true;
    };
    if !context.cors_required {
        return true;
    }
    if initiator.origin() == url.origin() {
        return true;
    }
    let allow = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Access-Control-Allow-Origin"))
        .map(|(_, v)| v.as_str());
    match allow {
        Some(v) if v.trim() == "*" => true,
        Some(v) => v
            .trim()
            .eq_ignore_ascii_case(&initiator.origin().ascii_serialization()),
        None => false,
    }
}
