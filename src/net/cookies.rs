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
//! Persistence: cookies with a `Max-Age` (i.e. `expires_at: Some(_)`)
//! survive process restarts via a single binary file at
//! `<data_dir>/daboss-cookies/jar.bin`. Session cookies (no Max-Age)
//! stay in-memory only, matching the browser convention. Writes are
//! atomic (tempfile + rename) so a crash mid-write can't corrupt the
//! jar.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use url::Url;

const JAR_MAGIC: &[u8; 4] = b"DBCJ";
const JAR_VERSION: u8 = 3;

/// RFC 6265bis §5.2 SameSite. Default per modern browser (Chrome 80+
/// / Firefox 96+) is `Lax` when the attribute isn't present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// Sent only on same-site requests. Cross-site requests — even
    /// top-level navigations — never include the cookie.
    Strict,
    /// Sent on same-site requests AND on cross-site top-level
    /// navigations with safe methods (GET). Default for cookies
    /// without an explicit `SameSite=` attribute.
    Lax,
    /// Sent on all requests, but only if the cookie also carries
    /// `Secure`. Cookies with `SameSite=None` and no `Secure` are
    /// rejected at parse time.
    None_,
}

impl Default for SameSite {
    fn default() -> Self {
        SameSite::Lax
    }
}

#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires_at: Option<Instant>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
    /// CHIPS partition key — when set, this cookie only matches
    /// requests whose top-level host (or its FPS primary) equals
    /// this value. `None` for unpartitioned (legacy) cookies, which
    /// match regardless of top-level context. The host stored here
    /// is the top-level origin's host at the time the cookie was
    /// received via `Set-Cookie: ...; Partitioned`.
    pub partition_key: Option<String>,
}

#[derive(Default, Debug)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
    /// Set to false in unit tests so jar mutations don't write to a
    /// shared disk file that would leak state between tests. Real
    /// callers (the `Client`) leave this `true`.
    disk_backed: bool,
}

impl CookieJar {
    pub fn new() -> Self {
        let mut jar = Self {
            cookies: Vec::new(),
            disk_backed: true,
        };
        jar.load_from_disk();
        jar
    }

    /// Test-only constructor that skips disk persistence so multiple
    /// in-process jars don't trip over each other's saved state.
    #[cfg(test)]
    pub fn new_in_memory() -> Self {
        Self {
            cookies: Vec::new(),
            disk_backed: false,
        }
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
            self.persist();
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
        self.persist();
    }

    /// Drop every cookie whose `expires_at` is in the past relative to
    /// `now`. Session cookies (no `expires_at`) survive.
    #[allow(dead_code)] // exposed for future periodic cleanup
    pub fn purge_expired(&mut self, now: Instant) {
        self.cookies.retain(|c| match c.expires_at {
            Some(t) => t > now,
            None => true,
        });
        self.persist();
    }

    /// Return the `Cookie:` header value for a given request URL, or
    /// `None` if no cookies match. Cookies are joined with `; `.
    ///
    /// `initiator` is the page that triggered the request (for
    /// SameSite enforcement and third-party blocking).
    /// `is_top_level_nav` is true when the user is navigating to
    /// `url` (typed address, link click). `is_safe_method` is true
    /// for GET — `Lax` cookies require both.
    pub fn header_for(
        &self,
        url: &Url,
        initiator: Option<&Url>,
        is_top_level_nav: bool,
        is_safe_method: bool,
    ) -> Option<String> {
        self.header_for_with_top(url, initiator, None, is_top_level_nav, is_safe_method)
    }

    /// CHIPS-aware variant of [`header_for`]. `top_level_url` is the
    /// outermost document — its host (collapsed through First-Party
    /// Sets) drives whether `Partitioned` cookies match the request.
    /// When `None`, partitioned cookies are excluded (i.e. the caller
    /// is implicitly third-party from a partitioning standpoint).
    pub fn header_for_with_top(
        &self,
        url: &Url,
        initiator: Option<&Url>,
        top_level_url: Option<&Url>,
        is_top_level_nav: bool,
        is_safe_method: bool,
    ) -> Option<String> {
        let host = url.host_str()?;
        let path = if url.path().is_empty() { "/" } else { url.path() };
        let is_https = url.scheme() == "https";
        let same_site_request = match initiator {
            None => true,
            Some(init) => same_site_urls(url, init),
        };
        let top_key: Option<String> = top_level_url
            .and_then(|u| u.host_str())
            .map(|h| {
                crate::net::first_party_set::primary_for(h)
                    .unwrap_or(h)
                    .to_string()
            });

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
            // CHIPS scoping: partitioned cookies only flow when the
            // request's top-level matches the cookie's recorded
            // partition. An absent partition key means "legacy
            // unpartitioned" — those flow everywhere they otherwise
            // would.
            if let Some(cookie_key) = c.partition_key.as_deref() {
                match top_key.as_deref() {
                    Some(req_key) if req_key.eq_ignore_ascii_case(cookie_key) => {}
                    _ => continue,
                }
            }
            if !same_site_request {
                match c.same_site {
                    SameSite::Strict => continue,
                    SameSite::Lax => {
                        // Send only on top-level GET-style navigations.
                        if !(is_top_level_nav && is_safe_method) {
                            continue;
                        }
                    }
                    SameSite::None_ => {
                        // SameSite=None requires Secure to even be
                        // stored, but double-check here.
                        if !c.secure {
                            continue;
                        }
                    }
                }
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
    parse_set_cookie_with_top(line, url, None)
}

/// Same as [`parse_set_cookie`] but also stamps a CHIPS partition
/// key if the Set-Cookie line carries the `Partitioned` attribute.
/// `top_level_url` is the URL of the page that owns the response
/// — its host (or FPS primary) becomes the partition.
pub fn parse_set_cookie_with_top(
    line: &str,
    url: &Url,
    top_level_url: Option<&Url>,
) -> Option<Cookie> {
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
        same_site: SameSite::Lax,
        partition_key: None,
    };
    let mut saw_partitioned = false;

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
                    "samesite" => {
                        cookie.same_site = match v.trim().to_ascii_lowercase().as_str() {
                            "strict" => SameSite::Strict,
                            "none" => SameSite::None_,
                            // "lax" or anything we don't understand
                            // → the modern default.
                            _ => SameSite::Lax,
                        };
                    }
                    _ => {} // expires/etc. ignored
                }
            }
        } else {
            let low = attr.to_ascii_lowercase();
            if low == "secure" {
                cookie.secure = true;
            } else if low == "httponly" {
                cookie.http_only = true;
            } else if low == "partitioned" {
                saw_partitioned = true;
            }
        }
    }
    // RFC 6265bis §4.1.2.7: SameSite=None requires Secure. Cookies
    // that violate the rule are rejected at storage time.
    if cookie.same_site == SameSite::None_ && !cookie.secure {
        return None;
    }
    // CHIPS: Partitioned cookies are bound to the top-level
    // origin's host. Without a top-level URL we can't compute one,
    // so the attribute is ignored (cookie becomes unpartitioned).
    // The spec also requires SameSite=None + Secure for
    // partitioned cookies — enforce that to avoid unscoped
    // first-party leaks.
    if saw_partitioned {
        if cookie.same_site != SameSite::None_ || !cookie.secure {
            return None;
        }
        if let Some(top) = top_level_url.and_then(|u| u.host_str()) {
            // Collapse to FPS primary when applicable so all members
            // of a set share the partition.
            let key = crate::net::first_party_set::primary_for(top)
                .unwrap_or(top)
                .to_string();
            cookie.partition_key = Some(key);
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

/// Hand-curated subset of the Public Suffix List — the most common
/// multi-label suffixes. Covers `.co.uk`, `.com.au`, `.github.io`,
/// `.vercel.app`, etc. without bundling the full 13k-entry PSL.
/// Real browsers consult the upstream PSL; this gets the
/// top-of-the-long-tail right.
const MULTI_LABEL_SUFFIXES: &[&str] = &[
    // ccTLD second-level domains
    "co.uk", "ac.uk", "gov.uk", "ltd.uk", "me.uk", "net.uk", "nhs.uk", "org.uk",
    "com.au", "net.au", "org.au", "edu.au", "gov.au",
    "co.nz", "net.nz", "org.nz", "ac.nz", "govt.nz",
    "co.jp", "ac.jp", "ne.jp", "or.jp", "go.jp",
    "com.br", "net.br", "org.br", "gov.br", "edu.br",
    "co.kr", "ne.kr", "or.kr", "go.kr",
    "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn",
    "co.in", "net.in", "org.in", "gov.in",
    "co.za", "org.za", "gov.za",
    "com.mx", "org.mx",
    "com.ar", "org.ar",
    "com.sg", "org.sg",
    "com.hk", "org.hk",
    "com.tr",
    "com.tw", "org.tw",
    "com.my", "org.my",
    // hosted-service PSL entries
    "github.io",
    "gitlab.io",
    "vercel.app",
    "netlify.app",
    "herokuapp.com",
    "azurewebsites.net",
    "cloudfront.net",
    "amazonaws.com",
    "s3.amazonaws.com",
    "appspot.com",
    "firebaseapp.com",
    "web.app",
    "workers.dev",
    "pages.dev",
    "fly.dev",
    "onrender.com",
    "supabase.co",
    "shopify.com",
    "wordpress.com",
    "blogspot.com",
];

/// Compute the registrable domain (eTLD+1) of `host`. Honours the
/// curated multi-label suffix list above and falls back to the
/// rightmost two DNS labels for everything else (correct for the
/// long tail of two-label TLDs like .com / .net / .io).
pub fn registrable_domain(host: &str) -> &str {
    // First, see if `host` ends in a known multi-label suffix.
    // Pick the LONGEST match so a host like `static.s3.amazonaws.com`
    // resolves against `s3.amazonaws.com` rather than the shorter
    // `amazonaws.com` entry that also ends-matches.
    let lower_host = host;
    let best: Option<&&str> = MULTI_LABEL_SUFFIXES
        .iter()
        .filter(|s| {
            let dot_then = format!(".{}", s);
            lower_host.len() > s.len() && lower_host.ends_with(&dot_then)
        })
        .max_by_key(|s| s.len());
    if let Some(suffix) = best {
        let dot_then = format!(".{suffix}");
        let prefix = &lower_host[..lower_host.len() - dot_then.len()];
        let label_start = prefix.rfind('.').map(|i| i + 1).unwrap_or(0);
        return &lower_host[label_start..];
    }
    // Fall back to the last-2-labels heuristic.
    let Some(last_dot) = lower_host.rfind('.') else {
        return lower_host;
    };
    match lower_host[..last_dot].rfind('.') {
        Some(prior_dot) => &lower_host[prior_dot + 1..],
        None => lower_host,
    }
}

pub fn same_site_urls(a: &Url, b: &Url) -> bool {
    let ha = a.host_str().unwrap_or("");
    let hb = b.host_str().unwrap_or("");
    if ha.is_empty() || hb.is_empty() {
        return false;
    }
    registrable_domain(ha).eq_ignore_ascii_case(registrable_domain(hb))
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

// =================== persistence ===================

impl CookieJar {
    fn persist(&self) {
        if !self.disk_backed {
            return;
        }
        let persistent: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| c.expires_at.is_some())
            .collect();
        let bytes = encode_jar(&persistent);
        let target = jar_path();
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let tmp = target.with_extension("bin.tmp");
        if let Ok(mut f) = fs::File::create(&tmp) {
            if f.write_all(&bytes).is_err() {
                return;
            }
            drop(f);
            let _ = fs::rename(&tmp, &target);
        }
    }

    fn load_from_disk(&mut self) {
        let path = jar_path();
        let Ok(bytes) = fs::read(&path) else {
            return;
        };
        if let Some(loaded) = decode_jar(&bytes) {
            let now = Instant::now();
            // Skip cookies that are already expired.
            self.cookies = loaded
                .into_iter()
                .filter(|c| c.expires_at.map(|t| t > now).unwrap_or(true))
                .collect();
        }
    }
}

fn jar_path() -> PathBuf {
    let mut p = crate::js::opfs::data_dir_path();
    p.push("daboss-cookies");
    p.push("jar.bin");
    p
}

fn encode_jar(cookies: &[&Cookie]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + cookies.len() * 64);
    out.extend_from_slice(JAR_MAGIC);
    out.push(JAR_VERSION);
    out.extend_from_slice(&(cookies.len() as u32).to_le_bytes());
    for c in cookies {
        write_lp(&mut out, c.name.as_bytes());
        write_lp(&mut out, c.value.as_bytes());
        write_lp(&mut out, c.domain.as_bytes());
        write_lp(&mut out, c.path.as_bytes());
        let ms = c.expires_at.map(instant_to_unix_ms).unwrap_or(0);
        out.extend_from_slice(&ms.to_le_bytes());
        out.push(c.secure as u8);
        out.push(c.http_only as u8);
        out.push(match c.same_site {
            SameSite::Strict => 0,
            SameSite::Lax => 1,
            SameSite::None_ => 2,
        });
        // Partition key (CHIPS). Empty length-prefixed string =
        // unpartitioned cookie. Stored alongside the other fields
        // so a v3 jar carries everything needed to round-trip.
        write_lp(
            &mut out,
            c.partition_key.as_deref().unwrap_or("").as_bytes(),
        );
    }
    out
}

fn decode_jar(buf: &[u8]) -> Option<Vec<Cookie>> {
    let mut p = 0usize;
    if buf.len() < 9 || &buf[..4] != JAR_MAGIC {
        return None;
    }
    p += 4;
    if buf[p] != JAR_VERSION {
        return None;
    }
    p += 1;
    let n = read_u32(buf, &mut p)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let name = read_lp(buf, &mut p)?;
        let value = read_lp(buf, &mut p)?;
        let domain = read_lp(buf, &mut p)?;
        let path = read_lp(buf, &mut p)?;
        let ms = read_u64(buf, &mut p)?;
        if p + 3 > buf.len() {
            return None;
        }
        let secure = buf[p] != 0;
        let http_only = buf[p + 1] != 0;
        let same_site = match buf[p + 2] {
            0 => SameSite::Strict,
            2 => SameSite::None_,
            _ => SameSite::Lax,
        };
        p += 3;
        let expires_at = if ms == 0 {
            None
        } else {
            unix_ms_to_instant(ms)
        };
        let partition_bytes = read_lp(buf, &mut p)?;
        let partition_key = if partition_bytes.is_empty() {
            None
        } else {
            Some(String::from_utf8(partition_bytes).ok()?)
        };
        out.push(Cookie {
            name: String::from_utf8(name).ok()?,
            value: String::from_utf8(value).ok()?,
            domain: String::from_utf8(domain).ok()?,
            path: String::from_utf8(path).ok()?,
            expires_at,
            secure,
            http_only,
            same_site,
            partition_key,
        });
    }
    Some(out)
}

fn write_lp(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn read_u32(buf: &[u8], p: &mut usize) -> Option<u32> {
    if *p + 4 > buf.len() {
        return None;
    }
    let v = u32::from_le_bytes([buf[*p], buf[*p + 1], buf[*p + 2], buf[*p + 3]]);
    *p += 4;
    Some(v)
}

fn read_u64(buf: &[u8], p: &mut usize) -> Option<u64> {
    if *p + 8 > buf.len() {
        return None;
    }
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&buf[*p..*p + 8]);
    *p += 8;
    Some(u64::from_le_bytes(arr))
}

fn read_lp(buf: &[u8], p: &mut usize) -> Option<Vec<u8>> {
    let n = read_u32(buf, p)? as usize;
    if *p + n > buf.len() {
        return None;
    }
    let out = buf[*p..*p + n].to_vec();
    *p += n;
    Some(out)
}

fn instant_to_unix_ms(t: Instant) -> u64 {
    let now_instant = Instant::now();
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if t >= now_instant {
        let delta = t.duration_since(now_instant);
        now_unix.saturating_add(delta.as_millis() as u64)
    } else {
        let delta = now_instant.duration_since(t);
        now_unix.saturating_sub(delta.as_millis() as u64)
    }
}

fn unix_ms_to_instant(ms: u64) -> Option<Instant> {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let now_instant = Instant::now();
    if ms >= now_unix {
        Some(now_instant + Duration::from_millis(ms - now_unix))
    } else {
        let delta = now_unix - ms;
        now_instant.checked_sub(Duration::from_millis(delta))
    }
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
    fn registrable_domain_handles_known_multi_label_suffixes() {
        assert_eq!(registrable_domain("foo.bar.co.uk"), "bar.co.uk");
        assert_eq!(registrable_domain("alice.github.io"), "alice.github.io");
        assert_eq!(registrable_domain("user.gitlab.io"), "user.gitlab.io");
        assert_eq!(
            registrable_domain("static.s3.amazonaws.com"),
            "static.s3.amazonaws.com"
        );
        // Plain TLDs still fall through.
        assert_eq!(registrable_domain("www.example.com"), "example.com");
        assert_eq!(registrable_domain("example.com"), "example.com");
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

    fn same_site_ctx(u: &Url) -> (Option<&Url>, bool, bool) {
        (Some(u), true, true)
    }

    #[test]
    fn jar_header_for_matches_secure_only_on_https() {
        let mut jar = CookieJar::new_in_memory();
        let mut c = parse_set_cookie("a=1; Secure", &url("https://example.com/")).unwrap();
        c.secure = true;
        jar.insert(c);
        let u_https = url("https://example.com/");
        let u_http = url("http://example.com/");
        let (init, tln, safe) = same_site_ctx(&u_https);
        assert!(jar.header_for(&u_https, init, tln, safe).is_some());
        let (init, tln, safe) = same_site_ctx(&u_http);
        assert!(jar.header_for(&u_http, init, tln, safe).is_none());
    }

    #[test]
    fn jar_replaces_same_name_path_domain() {
        let mut jar = CookieJar::new_in_memory();
        jar.insert(parse_set_cookie("a=1", &url("https://example.com/")).unwrap());
        jar.insert(parse_set_cookie("a=2", &url("https://example.com/")).unwrap());
        let u = url("https://example.com/");
        let (init, tln, safe) = same_site_ctx(&u);
        let h = jar.header_for(&u, init, tln, safe).unwrap();
        assert_eq!(h, "a=2");
    }

    #[test]
    fn jar_max_age_zero_deletes() {
        let mut jar = CookieJar::new_in_memory();
        jar.insert(parse_set_cookie("a=1", &url("https://example.com/")).unwrap());
        jar.insert(
            parse_set_cookie("a=1; Max-Age=0", &url("https://example.com/")).unwrap(),
        );
        let u = url("https://example.com/");
        let (init, tln, safe) = same_site_ctx(&u);
        assert!(jar.header_for(&u, init, tln, safe).is_none());
    }

    #[test]
    fn ingest_set_cookies_picks_only_set_cookie_lines() {
        let mut jar = CookieJar::new_in_memory();
        let headers = [
            ("Set-Cookie", "a=1"),
            ("Content-Type", "text/html"),
            ("set-cookie", "b=2"),
        ];
        jar.ingest_set_cookies(&url("https://example.com/"), headers.iter().copied());
        assert_eq!(jar.cookies().len(), 2);
        let u = url("https://example.com/");
        let (init, tln, safe) = same_site_ctx(&u);
        let h = jar.header_for(&u, init, tln, safe).unwrap();
        // Both present, joined with `; `.
        assert!(h.contains("a=1") && h.contains("b=2"));
    }

    #[test]
    fn more_specific_path_sorts_first() {
        let mut jar = CookieJar::new_in_memory();
        jar.insert(parse_set_cookie("a=1; Path=/", &url("https://x/")).unwrap());
        jar.insert(
            parse_set_cookie("b=2; Path=/foo", &url("https://x/foo/")).unwrap(),
        );
        let u = url("https://x/foo/bar");
        let (init, tln, safe) = same_site_ctx(&u);
        let h = jar.header_for(&u, init, tln, safe).unwrap();
        assert!(h.starts_with("b=2"));
    }

    #[test]
    fn samesite_strict_blocks_cross_site() {
        let mut jar = CookieJar::new_in_memory();
        jar.insert(
            parse_set_cookie("a=1; SameSite=Strict", &url("https://example.com/")).unwrap(),
        );
        let dest = url("https://example.com/");
        let initiator = url("https://other.example.org/");
        // Cross-site → blocked even on top-level navigation.
        assert!(jar.header_for(&dest, Some(&initiator), true, true).is_none());
        // Same-site → fine.
        assert!(jar.header_for(&dest, Some(&dest), false, true).is_some());
    }

    #[test]
    fn samesite_lax_blocks_cross_site_subresource() {
        let mut jar = CookieJar::new_in_memory();
        // Default (no SameSite attribute) is Lax.
        jar.insert(parse_set_cookie("a=1", &url("https://example.com/")).unwrap());
        let dest = url("https://example.com/");
        let initiator = url("https://other.example.org/");
        // Cross-site subresource (top_level=false) → blocked.
        assert!(jar.header_for(&dest, Some(&initiator), false, true).is_none());
        // Cross-site top-level GET → allowed.
        assert!(jar.header_for(&dest, Some(&initiator), true, true).is_some());
        // Cross-site top-level POST → blocked (non-safe method).
        assert!(jar.header_for(&dest, Some(&initiator), true, false).is_none());
    }

    #[test]
    fn samesite_none_rejected_without_secure() {
        assert!(parse_set_cookie(
            "a=1; SameSite=None",
            &url("https://example.com/"),
        )
        .is_none());
        let c =
            parse_set_cookie("a=1; SameSite=None; Secure", &url("https://example.com/"))
                .unwrap();
        assert_eq!(c.same_site, SameSite::None_);
        assert!(c.secure);
    }

    #[test]
    fn partitioned_attribute_requires_samesite_none_and_secure() {
        // Without SameSite=None+Secure the parser rejects the cookie.
        assert!(parse_set_cookie_with_top(
            "p=1; Partitioned",
            &url("https://tracker.example/"),
            Some(&url("https://news.example/")),
        )
        .is_none());
        // With both attributes present, the cookie persists with a
        // partition key derived from the top-level host.
        let c = parse_set_cookie_with_top(
            "p=1; SameSite=None; Secure; Partitioned",
            &url("https://tracker.example/"),
            Some(&url("https://news.example/")),
        )
        .expect("parses");
        assert_eq!(c.partition_key.as_deref(), Some("news.example"));
    }

    #[test]
    fn partitioned_cookie_only_matches_owning_top_level() {
        let mut jar = CookieJar::new_in_memory();
        let dest = url("https://tracker.example/");
        let top_a = url("https://news.example/");
        let top_b = url("https://blog.example/");
        let c = parse_set_cookie_with_top(
            "id=42; SameSite=None; Secure; Partitioned",
            &dest,
            Some(&top_a),
        )
        .expect("parses");
        jar.insert(c);
        // Same destination + same top-level → cookie flows.
        assert!(
            jar.header_for_with_top(&dest, Some(&top_a), Some(&top_a), false, true)
                .is_some()
        );
        // Same destination but different top-level → cookie blocked.
        assert!(
            jar.header_for_with_top(&dest, Some(&top_b), Some(&top_b), false, true)
                .is_none()
        );
    }

    #[test]
    fn partitioned_cookie_persistence_round_trip() {
        let cookies = vec![Cookie {
            name: "id".into(),
            value: "1".into(),
            domain: "example.com".into(),
            path: "/".into(),
            expires_at: None,
            secure: true,
            http_only: false,
            same_site: SameSite::None_,
            partition_key: Some("news.example".into()),
        }];
        let bytes = encode_jar(&cookies.iter().collect::<Vec<_>>());
        let round = decode_jar(&bytes).expect("decode");
        assert_eq!(round.len(), 1);
        assert_eq!(round[0].partition_key.as_deref(), Some("news.example"));
    }

    #[test]
    fn fps_members_collapse_partition_key_to_primary() {
        // A Partitioned cookie under youtube.com (member) embedded
        // under google.com (primary) collapses onto the primary, so
        // a sibling member like blogger.com sees the same cookie.
        let dest = url("https://tracker.example/");
        let top_member = url("https://youtube.com/");
        let c = parse_set_cookie_with_top(
            "id=42; SameSite=None; Secure; Partitioned",
            &dest,
            Some(&top_member),
        )
        .expect("parses");
        assert_eq!(c.partition_key.as_deref(), Some("google.com"));
    }
}
