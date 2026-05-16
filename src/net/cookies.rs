//! HTTP cookie jar (toy version).
//!
//! Stores cookies parsed from `Set-Cookie` response headers and renders a
//! single `Cookie:` header for outgoing requests when entries match.
//!
//! Supported attributes:
//!  * `Path`     — string prefix match against the request path
//!  * `Domain`   — case-insensitive host suffix match (with the standard
//!                 leading-dot tolerance, e.g. `.example.com` matches both
//!                 `example.com` and `www.example.com`)
//!  * `Max-Age`  — relative seconds; `0` deletes the cookie immediately
//!  * `Secure`   — cookie is only sent on HTTPS
//!  * `HttpOnly` — recorded but not enforced (no JS cookie API yet)
//!
//! Skipped intentionally for now:
//!  * `Expires` date parsing — `Max-Age` is the modern attribute and far
//!    easier to parse correctly; `Expires` cookies stay session-scoped.
//!  * `SameSite` — every cross-site cookie is treated as if it were
//!    `SameSite=None`. Real browsers default to `Lax`; we'll close this
//!    gap when we have multi-tab top-level navigation that matters.
//!
//! Threat model note: cookies are kept in memory only — no disk
//! persistence — so they never outlive the process. This keeps the cookie
//! attack surface bounded to the current run.

use std::time::{Duration, Instant};

use url::Url;

#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires_at: Option<Instant>,
    pub secure: bool,
    pub http_only: bool,
}

#[derive(Default, Debug)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a cookie. If one with the same (name, domain, path) tuple
    /// already exists, replace it. A cookie whose `Max-Age=0` is treated
    /// as a delete (it never makes it into the jar).
    pub fn insert(&mut self, cookie: Cookie) {
        // Max-Age=0 → remove existing match and don't insert.
        let now = Instant::now();
        if cookie.expires_at.map(|t| t <= now).unwrap_or(false) {
            self.cookies.retain(|c| {
                !(c.name == cookie.name
                    && c.domain.eq_ignore_ascii_case(&cookie.domain)
                    && c.path == cookie.path)
            });
            return;
        }
        if let Some(slot) = self.cookies.iter_mut().find(|c| {
            c.name == cookie.name
                && c.domain.eq_ignore_ascii_case(&cookie.domain)
                && c.path == cookie.path
        }) {
            *slot = cookie;
        } else {
            self.cookies.push(cookie);
        }
    }

    /// Drop every cookie whose `expires_at` is in the past relative to
    /// `now`. Session cookies (no `expires_at`) survive.
    #[allow(dead_code)] // exposed for future periodic cleanup
    pub fn purge_expired(&mut self, now: Instant) {
        self.cookies.retain(|c| match c.expires_at {
            Some(t) => t > now,
            None => true,
        });
    }

    /// Return the `Cookie:` header value for a given request URL, or
    /// `None` if no cookies match. Cookies are joined with `; `.
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let host = url.host_str()?;
        let path = if url.path().is_empty() { "/" } else { url.path() };
        let is_https = url.scheme() == "https";

        let mut hits: Vec<(usize, &Cookie)> = Vec::new();
        for c in &self.cookies {
            if c.secure && !is_https {
                continue;
            }
            if !domain_matches(host, &c.domain) {
                continue;
            }
            if !path_matches(path, &c.path) {
                continue;
            }
            hits.push((c.path.len(), c));
        }
        if hits.is_empty() {
            return None;
        }
        // Sort by path length desc so more specific cookies precede less
        // specific ones, per RFC 6265 §5.4.
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        let joined = hits
            .iter()
            .map(|(_, c)| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        Some(joined)
    }

    /// Parse and merge every `Set-Cookie` header from a response into the
    /// jar, scoped to `url` (used as the fallback domain / path).
    pub fn ingest_set_cookies<'a>(
        &mut self,
        url: &Url,
        headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) {
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("set-cookie") {
                continue;
            }
            if let Some(cookie) = parse_set_cookie(value, url) {
                self.insert(cookie);
            }
        }
    }

    /// Snapshot of stored cookies — exposed for tests.
    #[cfg(test)]
    pub fn cookies(&self) -> &[Cookie] {
        &self.cookies
    }
}

/// Parse a single `Set-Cookie` header value into a [`Cookie`]. Returns
/// `None` if the line doesn't have a `name=value` pair.
pub fn parse_set_cookie(line: &str, url: &Url) -> Option<Cookie> {
    let mut parts = line.split(';');
    let first = parts.next()?.trim();
    let (name, value) = split_kv(first)?;
    if name.is_empty() {
        return None;
    }

    let default_domain = url.host_str()?.to_string();
    let default_path = default_path_for(url);

    let mut cookie = Cookie {
        name: name.to_string(),
        value: value.to_string(),
        domain: default_domain,
        path: default_path,
        expires_at: None,
        secure: false,
        http_only: false,
    };

    for attr in parts {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        if attr.contains('=') {
            if let Some((k, v)) = split_kv(attr) {
                let k_low = k.to_ascii_lowercase();
                match k_low.as_str() {
                    "path" => {
                        if !v.is_empty() {
                            cookie.path = v.to_string();
                        }
                    }
                    "domain" => {
                        let d = v.trim_start_matches('.');
                        if !d.is_empty() {
                            cookie.domain = d.to_string();
                        }
                    }
                    "max-age" => {
                        if let Ok(secs) = v.trim().parse::<i64>() {
                            if secs <= 0 {
                                cookie.expires_at = Some(
                                    Instant::now()
                                        .checked_sub(Duration::from_secs(1))
                                        .unwrap_or_else(Instant::now),
                                );
                            } else {
                                cookie.expires_at =
                                    Some(Instant::now() + Duration::from_secs(secs as u64));
                            }
                        }
                    }
                    _ => {} // expires/samesite/etc. ignored
                }
            }
        } else {
            let low = attr.to_ascii_lowercase();
            if low == "secure" {
                cookie.secure = true;
            } else if low == "httponly" {
                cookie.http_only = true;
            }
        }
    }
    Some(cookie)
}

fn split_kv(s: &str) -> Option<(&str, &str)> {
    let mut it = s.splitn(2, '=');
    let k = it.next()?.trim();
    let v = it.next().map(str::trim).unwrap_or("");
    Some((k, v))
}

fn default_path_for(url: &Url) -> String {
    // RFC 6265 §5.1.4 default path algorithm.
    let path = url.path();
    if !path.starts_with('/') {
        return "/".into();
    }
    // The default path is the URL path up to (but not including) the
    // last `/`. If there's only the leading `/`, the default is `/`.
    match path.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => path[..i].into(),
        None => "/".into(),
    }
}

fn domain_matches(host: &str, cookie_domain: &str) -> bool {
    if host.eq_ignore_ascii_case(cookie_domain) {
        return true;
    }
    // host must end with `.cookie_domain`
    if host.len() <= cookie_domain.len() {
        return false;
    }
    let suffix_start = host.len() - cookie_domain.len();
    if !host[suffix_start..].eq_ignore_ascii_case(cookie_domain) {
        return false;
    }
    host.as_bytes()[suffix_start - 1] == b'.'
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if !request_path.starts_with(cookie_path) {
        return false;
    }
    // Boundary check: the character after the cookie_path prefix must
    // be `/`, or the cookie_path must itself end in `/`.
    if cookie_path.ends_with('/') {
        return true;
    }
    request_path[cookie_path.len()..].starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parses_simple_name_value() {
        let c = parse_set_cookie("sid=abc", &url("https://example.com/")).unwrap();
        assert_eq!(c.name, "sid");
        assert_eq!(c.value, "abc");
        assert_eq!(c.domain, "example.com");
        assert_eq!(c.path, "/");
        assert!(!c.secure);
        assert!(!c.http_only);
    }

    #[test]
    fn parses_attributes() {
        let c = parse_set_cookie(
            "sid=abc; Path=/api; Domain=.example.com; Secure; HttpOnly; Max-Age=60",
            &url("https://www.example.com/"),
        )
        .unwrap();
        assert_eq!(c.path, "/api");
        assert_eq!(c.domain, "example.com");
        assert!(c.secure);
        assert!(c.http_only);
        assert!(c.expires_at.is_some());
    }

    #[test]
    fn domain_matches_self_and_subdomains() {
        assert!(domain_matches("example.com", "example.com"));
        assert!(domain_matches("www.example.com", "example.com"));
        assert!(!domain_matches("evil.com", "example.com"));
        assert!(!domain_matches("notexample.com", "example.com"));
        assert!(!domain_matches("com", "example.com"));
    }

    #[test]
    fn path_matches_boundary_check() {
        assert!(path_matches("/", "/"));
        assert!(path_matches("/foo", "/foo"));
        assert!(path_matches("/foo/bar", "/foo"));
        assert!(path_matches("/foo/", "/foo"));
        assert!(!path_matches("/foobar", "/foo"));
    }

    #[test]
    fn jar_header_for_matches_secure_only_on_https() {
        let mut jar = CookieJar::new();
        let mut c = parse_set_cookie("a=1; Secure", &url("https://example.com/")).unwrap();
        c.secure = true;
        jar.insert(c);
        assert!(jar.header_for(&url("https://example.com/")).is_some());
        assert!(jar.header_for(&url("http://example.com/")).is_none());
    }

    #[test]
    fn jar_replaces_same_name_path_domain() {
        let mut jar = CookieJar::new();
        jar.insert(parse_set_cookie("a=1", &url("https://example.com/")).unwrap());
        jar.insert(parse_set_cookie("a=2", &url("https://example.com/")).unwrap());
        let h = jar.header_for(&url("https://example.com/")).unwrap();
        assert_eq!(h, "a=2");
    }

    #[test]
    fn jar_max_age_zero_deletes() {
        let mut jar = CookieJar::new();
        jar.insert(parse_set_cookie("a=1", &url("https://example.com/")).unwrap());
        jar.insert(
            parse_set_cookie("a=1; Max-Age=0", &url("https://example.com/")).unwrap(),
        );
        assert!(jar.header_for(&url("https://example.com/")).is_none());
    }

    #[test]
    fn ingest_set_cookies_picks_only_set_cookie_lines() {
        let mut jar = CookieJar::new();
        let headers = [
            ("Set-Cookie", "a=1"),
            ("Content-Type", "text/html"),
            ("set-cookie", "b=2"),
        ];
        jar.ingest_set_cookies(&url("https://example.com/"), headers.iter().copied());
        assert_eq!(jar.cookies().len(), 2);
        let h = jar.header_for(&url("https://example.com/")).unwrap();
        // Both present, joined with `; `.
        assert!(h.contains("a=1") && h.contains("b=2"));
    }

    #[test]
    fn more_specific_path_sorts_first() {
        let mut jar = CookieJar::new();
        jar.insert(parse_set_cookie("a=1; Path=/", &url("https://x/")).unwrap());
        jar.insert(
            parse_set_cookie("b=2; Path=/foo", &url("https://x/foo/")).unwrap(),
        );
        let h = jar.header_for(&url("https://x/foo/bar")).unwrap();
        assert!(h.starts_with("b=2"));
    }
}
