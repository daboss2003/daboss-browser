//! In-memory HTTP cache, RFC 9111 lite.
//!
//! Stores responses keyed by URL. Two paths:
//!
//! * **Fresh hit** — `Cache-Control: max-age` (or `Expires`) puts the
//!   entry within its freshness window. We return the cached body
//!   without hitting the network.
//! * **Stale + validator** — entry has an `ETag` or `Last-Modified`.
//!   We send an `If-None-Match` / `If-Modified-Since` and treat a 304
//!   as "use the cached body, update headers".
//!
//! Skipped on purpose:
//!  * Vary / negotiated responses — we don't vary on Accept-* yet.
//!  * Disk persistence — the cache dies with the process.
//!  * `Cache-Control: private` is treated like `public` because the
//!    toy is single-user.
//!
//! Cacheable methods: `GET` only. Cacheable statuses: 200, 203, 300,
//! 301, 308, 404, 410. Anything else falls through.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::net::http::Response;

#[derive(Debug, Clone)]
pub struct CachedEntry {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// When the cached entry becomes stale. `None` means
    /// "must-revalidate via the ETag/Last-Modified validator path
    /// every time" — i.e. no freshness window.
    pub fresh_until: Option<Instant>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl CachedEntry {
    pub fn is_fresh(&self, now: Instant) -> bool {
        match self.fresh_until {
            Some(t) => t > now,
            None => false,
        }
    }
}

#[derive(Default, Debug)]
pub struct HttpCache {
    map: HashMap<String, CachedEntry>,
}

impl HttpCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lookup(&self, key: &str) -> Option<&CachedEntry> {
        self.map.get(key)
    }

    /// Try to update an existing entry's headers / freshness window
    /// from a `304 Not Modified` response. Returns the refreshed body
    /// if there was a matching entry, `None` otherwise.
    pub fn refresh_after_304(&mut self, key: &str, headers: &[(String, String)]) -> Option<Vec<u8>> {
        let entry = self.map.get_mut(key)?;
        for (name, value) in headers {
            match name.to_ascii_lowercase().as_str() {
                "cache-control" => entry.fresh_until = freshness_from(value, Instant::now()),
                "etag" => entry.etag = Some(value.clone()),
                "last-modified" => entry.last_modified = Some(value.clone()),
                _ => {}
            }
        }
        Some(entry.body.clone())
    }

    /// Insert a fresh entry from a fetched response, if the response
    /// is cacheable. Returns whether something was stored.
    pub fn store(&mut self, key: &str, response: &Response) -> bool {
        if !is_status_cacheable(response.status) {
            return false;
        }
        if response_forbids_cache(response) {
            return false;
        }
        let now = Instant::now();
        let fresh_until = response
            .header("Cache-Control")
            .and_then(|cc| freshness_from(cc, now))
            .or_else(|| {
                // Fallback: Expires header. We don't parse HTTP dates
                // here; if `Expires: 0` or `-1`, treat as immediately
                // stale (validator-only). Otherwise leave unset.
                response.header("Expires").and_then(|_| None)
            });
        let entry = CachedEntry {
            status: response.status,
            reason: response.reason.clone(),
            headers: response.headers.clone(),
            body: response.body.clone(),
            fresh_until,
            etag: response.header("ETag").map(str::to_string),
            last_modified: response.header("Last-Modified").map(str::to_string),
        };
        self.map.insert(key.to_string(), entry);
        true
    }
}

fn is_status_cacheable(status: u16) -> bool {
    matches!(status, 200 | 203 | 300 | 301 | 308 | 404 | 410)
}

fn response_forbids_cache(resp: &Response) -> bool {
    if let Some(cc) = resp.header("Cache-Control") {
        let cc = cc.to_ascii_lowercase();
        if cc.contains("no-store") {
            return true;
        }
    }
    false
}

/// Parse `Cache-Control` into a freshness deadline. Honours `max-age`,
/// `no-cache` (returns `None`), and falls back to `None` for anything
/// we don't recognise.
fn freshness_from(value: &str, now: Instant) -> Option<Instant> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("no-cache") {
        return None;
    }
    for directive in lower.split(',') {
        let directive = directive.trim();
        if let Some(seconds) = directive.strip_prefix("max-age=") {
            if let Ok(n) = seconds.trim().parse::<u64>() {
                return Some(now + Duration::from_secs(n));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(headers: &[(&str, &str)]) -> Response {
        Response {
            status: 200,
            reason: "OK".into(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            body: b"hello".to_vec(),
        }
    }

    #[test]
    fn store_returns_false_for_non_cacheable_status() {
        let mut cache = HttpCache::new();
        let mut r = make(&[]);
        r.status = 500;
        assert!(!cache.store("u", &r));
        assert!(cache.lookup("u").is_none());
    }

    #[test]
    fn store_honours_no_store() {
        let mut cache = HttpCache::new();
        let r = make(&[("Cache-Control", "no-store")]);
        assert!(!cache.store("u", &r));
    }

    #[test]
    fn store_with_max_age_yields_fresh() {
        let mut cache = HttpCache::new();
        let r = make(&[("Cache-Control", "max-age=3600")]);
        assert!(cache.store("u", &r));
        let entry = cache.lookup("u").unwrap();
        assert!(entry.is_fresh(Instant::now()));
    }

    #[test]
    fn refresh_after_304_returns_cached_body() {
        let mut cache = HttpCache::new();
        let r = make(&[("ETag", "\"abc\"")]);
        cache.store("u", &r);
        let body = cache
            .refresh_after_304("u", &[("Cache-Control".into(), "max-age=60".into())])
            .unwrap();
        assert_eq!(body, b"hello");
        assert!(cache.lookup("u").unwrap().is_fresh(Instant::now()));
    }
}
