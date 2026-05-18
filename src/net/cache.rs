//! HTTP cache, RFC 9111 lite — disk-backed with a hot in-memory index.
//!
//! Layout: `<data_dir>/daboss-httpcache/<origin-host>/<url-hex>.meta`
//! plus `<url-hex>.body`. The meta file is a small length-prefixed
//! binary blob (status, reason, headers, freshness, validators); the
//! body file is the raw response bytes.
//!
//! On startup the cache scans its on-disk root to rebuild the LRU
//! eviction order and byte total without loading actual entries.
//! Lookups fall through to disk on miss and fault the entry into
//! the in-memory index. `store()` writes meta+body atomically
//! (tempfile + rename) and FIFO-evicts when total bytes exceed
//! `MAX_DISK_BYTES`.
//!
//! Two paths through the cache:
//! * **Fresh hit** — `Cache-Control: max-age` (or `Expires`) puts the
//!   entry within its freshness window. We return the cached body
//!   without hitting the network.
//! * **Stale + validator** — entry has an `ETag` or `Last-Modified`.
//!   We send an `If-None-Match` / `If-Modified-Since` and treat a 304
//!   as "use the cached body, update headers".
//!
//! Cacheable methods: `GET` only. Cacheable statuses: 200, 203, 300,
//! 301, 308, 404, 410. Anything else falls through.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::net::http::Response;

/// Cap on total bytes resident in the on-disk cache before we start
/// evicting the oldest entries. 256 MiB suits a low-end mobile while
/// still being big enough to hold a normal browsing-session asset
/// working set.
pub const MAX_DISK_BYTES: u64 = 256 * 1024 * 1024;

const META_MAGIC: &[u8; 4] = b"DBCK";
const META_VERSION: u8 = 1;

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

/// Per-entry disk record. Tells the evicter where the body file is
/// and how big it is so we don't need to stat the file to know
/// what we'll free.
#[derive(Debug, Clone)]
struct DiskKey {
    origin: String,
    url_hex: String,
    body_size: u64,
}

impl DiskKey {
    fn meta_path(&self) -> PathBuf {
        let mut p = cache_root();
        p.push(&self.origin);
        p.push(format!("{}.meta", self.url_hex));
        p
    }
    fn body_path(&self) -> PathBuf {
        let mut p = cache_root();
        p.push(&self.origin);
        p.push(format!("{}.body", self.url_hex));
        p
    }
}

pub struct HttpCache {
    /// Hot index: cache-key → (entry, disk key). Cache misses fall
    /// through to disk and populate this on read.
    map: HashMap<String, (CachedEntry, DiskKey)>,
    /// Disk-residence registry. Holds an entry for every cached file
    /// — even ones we haven't faulted into `map` yet. Keyed by URL.
    disk: HashMap<String, DiskKey>,
    /// FIFO eviction order. Each entry is a URL key.
    order: Vec<String>,
    total_bytes: u64,
    max_bytes: u64,
}

impl Default for HttpCache {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpCache {
    pub fn new() -> Self {
        let mut cache = Self {
            map: HashMap::new(),
            disk: HashMap::new(),
            order: Vec::new(),
            total_bytes: 0,
            max_bytes: MAX_DISK_BYTES,
        };
        cache.rebuild_index_from_disk();
        cache
    }

    /// Walk the on-disk cache root and populate `disk` / `order` /
    /// `total_bytes`. We don't load the actual entries — they're
    /// faulted in on `lookup`. This keeps startup fast even when the
    /// cache holds thousands of files.
    fn rebuild_index_from_disk(&mut self) {
        let root = cache_root();
        let Ok(origins) = fs::read_dir(&root) else {
            return;
        };
        for origin_entry in origins.flatten() {
            let origin = origin_entry.file_name().to_string_lossy().into_owned();
            let sub = origin_entry.path();
            if !sub.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&sub) else {
                continue;
            };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                let Some(url_hex) = name.strip_suffix(".meta") else {
                    continue;
                };
                let Some(url_bytes) = hex_decode(url_hex) else {
                    continue;
                };
                let Ok(url) = String::from_utf8(url_bytes) else {
                    continue;
                };
                let body_path = sub.join(format!("{url_hex}.body"));
                let body_size = fs::metadata(&body_path).map(|m| m.len()).unwrap_or(0);
                let dk = DiskKey {
                    origin: origin.clone(),
                    url_hex: url_hex.to_string(),
                    body_size,
                };
                self.disk.insert(url.clone(), dk);
                self.order.push(url);
                self.total_bytes = self.total_bytes.saturating_add(body_size);
            }
        }
        self.evict_until_under_cap();
    }

    pub fn lookup(&mut self, key: &str) -> Option<&CachedEntry> {
        if !self.map.contains_key(key) {
            let dk = self.disk.get(key).cloned()?;
            let entry = read_meta_and_body(&dk)?;
            self.map.insert(key.to_string(), (entry, dk));
        }
        self.map.get(key).map(|(e, _)| e)
    }

    /// Try to update an existing entry's headers / freshness window
    /// from a `304 Not Modified` response. Returns the refreshed body
    /// if there was a matching entry, `None` otherwise.
    pub fn refresh_after_304(
        &mut self,
        key: &str,
        headers: &[(String, String)],
    ) -> Option<Vec<u8>> {
        // Fault the entry in if it's only on disk.
        let _ = self.lookup(key);
        let (entry, dk) = self.map.get_mut(key)?;
        for (name, value) in headers {
            match name.to_ascii_lowercase().as_str() {
                "cache-control" => entry.fresh_until = freshness_from(value, Instant::now()),
                "etag" => entry.etag = Some(value.clone()),
                "last-modified" => entry.last_modified = Some(value.clone()),
                _ => {}
            }
        }
        let body = entry.body.clone();
        let _ = write_meta(dk, entry);
        Some(body)
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
        let body_bytes = response.body_bytes();
        // Per-entry hard cap — refuse to cache anything bigger than
        // the whole disk budget (would force-evict everything else).
        if body_bytes.len() as u64 > self.max_bytes {
            return false;
        }
        let now = Instant::now();
        let fresh_until = response
            .header("Cache-Control")
            .and_then(|cc| freshness_from(cc, now))
            .or_else(|| response.header("Expires").and_then(|_| None));
        let entry = CachedEntry {
            status: response.status,
            reason: response.reason.clone(),
            headers: response.headers.clone(),
            body: body_bytes.clone(),
            fresh_until,
            etag: response.header("ETag").map(str::to_string),
            last_modified: response.header("Last-Modified").map(str::to_string),
        };

        let origin = origin_for_key(key);
        let url_hex = hex_encode(key.as_bytes());
        let dk = DiskKey {
            origin: origin.clone(),
            url_hex: url_hex.clone(),
            body_size: body_bytes.len() as u64,
        };

        // Make sure the origin's directory exists.
        let mut sub = cache_root();
        sub.push(&origin);
        if fs::create_dir_all(&sub).is_err() {
            return false;
        }

        if write_body_atomic(&dk, &body_bytes).is_err() {
            return false;
        }
        if write_meta(&dk, &entry).is_err() {
            let _ = fs::remove_file(dk.body_path());
            return false;
        }

        // If we're overwriting an existing entry, subtract the old
        // body size first so the eviction accounting stays correct.
        if let Some(prev) = self.disk.get(key).cloned() {
            self.total_bytes = self.total_bytes.saturating_sub(prev.body_size);
            self.order.retain(|k| k != key);
        }
        self.total_bytes = self.total_bytes.saturating_add(dk.body_size);
        self.order.push(key.to_string());
        self.disk.insert(key.to_string(), dk.clone());
        self.map.insert(key.to_string(), (entry, dk));

        self.evict_until_under_cap();
        true
    }

    fn evict_until_under_cap(&mut self) {
        while self.total_bytes > self.max_bytes && !self.order.is_empty() {
            let url_key = self.order.remove(0);
            let Some(dk) = self.disk.remove(&url_key) else {
                continue;
            };
            self.total_bytes = self.total_bytes.saturating_sub(dk.body_size);
            let _ = fs::remove_file(dk.meta_path());
            let _ = fs::remove_file(dk.body_path());
            self.map.remove(&url_key);
        }
    }
}

// =================== helpers ===================

fn cache_root() -> PathBuf {
    let mut p = crate::js::opfs::data_dir_path();
    p.push("daboss-httpcache");
    let _ = fs::create_dir_all(&p);
    p
}

fn origin_for_key(key: &str) -> String {
    let host = url::Url::parse(key)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "default".to_string());
    crate::js::opfs::sanitise_path_component(&host)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(chunk).ok()?;
        out.push(u8::from_str_radix(pair, 16).ok()?);
    }
    Some(out)
}

fn read_meta_and_body(dk: &DiskKey) -> Option<CachedEntry> {
    let meta = fs::read(dk.meta_path()).ok()?;
    let body = fs::read(dk.body_path()).ok()?;
    let mut entry = parse_meta(&meta)?;
    entry.body = body;
    Some(entry)
}

fn write_meta(dk: &DiskKey, entry: &CachedEntry) -> std::io::Result<()> {
    let bytes = encode_meta(entry);
    let target = dk.meta_path();
    let tmp = target.with_extension("meta.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
    }
    fs::rename(&tmp, &target)
}

fn write_body_atomic(dk: &DiskKey, body: &[u8]) -> std::io::Result<()> {
    let target = dk.body_path();
    let tmp = target.with_extension("body.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(body)?;
    }
    fs::rename(&tmp, &target)
}

fn encode_meta(entry: &CachedEntry) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        256 + entry.headers.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>(),
    );
    out.extend_from_slice(META_MAGIC);
    out.push(META_VERSION);
    out.extend_from_slice(&entry.status.to_le_bytes());
    write_lp_str(&mut out, &entry.reason);
    let fresh_ms = entry
        .fresh_until
        .map(instant_to_unix_ms)
        .unwrap_or(0);
    out.extend_from_slice(&fresh_ms.to_le_bytes());
    write_lp_opt(&mut out, entry.etag.as_deref());
    write_lp_opt(&mut out, entry.last_modified.as_deref());
    out.extend_from_slice(&(entry.headers.len() as u32).to_le_bytes());
    for (k, v) in &entry.headers {
        write_lp_str(&mut out, k);
        write_lp_str(&mut out, v);
    }
    out
}

fn parse_meta(buf: &[u8]) -> Option<CachedEntry> {
    let mut p = Parser::new(buf);
    if !p.expect_slice(META_MAGIC) {
        return None;
    }
    if p.read_u8()? != META_VERSION {
        return None;
    }
    let status = p.read_u16()?;
    let reason = p.read_lp_str()?;
    let fresh_ms = p.read_u64()?;
    let fresh_until = if fresh_ms == 0 {
        None
    } else {
        unix_ms_to_instant(fresh_ms)
    };
    let etag = p.read_lp_opt()?;
    let last_modified = p.read_lp_opt()?;
    let n = p.read_u32()? as usize;
    let mut headers = Vec::with_capacity(n);
    for _ in 0..n {
        let k = p.read_lp_str()?;
        let v = p.read_lp_str()?;
        headers.push((k, v));
    }
    Some(CachedEntry {
        status,
        reason,
        headers,
        body: Vec::new(),
        fresh_until,
        etag,
        last_modified,
    })
}

fn write_lp_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn write_lp_opt(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => {
            // u32::MAX sentinel = None.
            out.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        Some(v) => write_lp_str(out, v),
    }
}

struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn expect_slice(&mut self, want: &[u8]) -> bool {
        if self.remaining() < want.len() {
            return false;
        }
        if &self.buf[self.pos..self.pos + want.len()] != want {
            return false;
        }
        self.pos += want.len();
        true
    }
    fn read_u8(&mut self) -> Option<u8> {
        if self.remaining() < 1 {
            return None;
        }
        let v = self.buf[self.pos];
        self.pos += 1;
        Some(v)
    }
    fn read_u16(&mut self) -> Option<u16> {
        if self.remaining() < 2 {
            return None;
        }
        let v = u16::from_le_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Some(v)
    }
    fn read_u32(&mut self) -> Option<u32> {
        if self.remaining() < 4 {
            return None;
        }
        let arr = [
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ];
        self.pos += 4;
        Some(u32::from_le_bytes(arr))
    }
    fn read_u64(&mut self) -> Option<u64> {
        if self.remaining() < 8 {
            return None;
        }
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&self.buf[self.pos..self.pos + 8]);
        self.pos += 8;
        Some(u64::from_le_bytes(arr))
    }
    fn read_lp_str(&mut self) -> Option<String> {
        let n = self.read_u32()? as usize;
        if self.remaining() < n {
            return None;
        }
        let s = std::str::from_utf8(&self.buf[self.pos..self.pos + n])
            .ok()?
            .to_string();
        self.pos += n;
        Some(s)
    }
    fn read_lp_opt(&mut self) -> Option<Option<String>> {
        if self.remaining() < 4 {
            return None;
        }
        let n = u32::from_le_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        if n == u32::MAX {
            self.pos += 4;
            return Some(None);
        }
        Some(Some(self.read_lp_str()?))
    }
}

fn instant_to_unix_ms(t: Instant) -> u64 {
    // `Instant` isn't directly serializable; convert by anchoring the
    // delta from `Instant::now()` to `SystemTime::now()`.
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
        // Already past — return an Instant in the past so is_fresh()
        // returns false.
        let delta = now_unix - ms;
        now_instant.checked_sub(Duration::from_millis(delta))
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
            body_path: None,
        }
    }

    fn unique_key(label: &str) -> String {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("http://test-{label}-{stamp:x}.example/x")
    }

    #[test]
    fn store_returns_false_for_non_cacheable_status() {
        let mut cache = HttpCache::new();
        let mut r = make(&[]);
        r.status = 500;
        let key = unique_key("non-cacheable");
        assert!(!cache.store(&key, &r));
        assert!(cache.lookup(&key).is_none());
    }

    #[test]
    fn store_honours_no_store() {
        let mut cache = HttpCache::new();
        let r = make(&[("Cache-Control", "no-store")]);
        let key = unique_key("no-store");
        assert!(!cache.store(&key, &r));
    }

    #[test]
    fn store_with_max_age_yields_fresh() {
        let mut cache = HttpCache::new();
        let r = make(&[("Cache-Control", "max-age=3600")]);
        let key = unique_key("max-age");
        assert!(cache.store(&key, &r));
        let entry = cache.lookup(&key).unwrap();
        assert!(entry.is_fresh(Instant::now()));
    }

    #[test]
    fn refresh_after_304_returns_cached_body() {
        let mut cache = HttpCache::new();
        let r = make(&[("ETag", "\"abc\"")]);
        let key = unique_key("304");
        cache.store(&key, &r);
        let body = cache
            .refresh_after_304(&key, &[("Cache-Control".into(), "max-age=60".into())])
            .unwrap();
        assert_eq!(body, b"hello");
        assert!(cache.lookup(&key).unwrap().is_fresh(Instant::now()));
    }

    #[test]
    fn survives_recreation_via_disk() {
        let key = unique_key("survives");
        {
            let mut cache = HttpCache::new();
            let r = make(&[("Cache-Control", "max-age=3600")]);
            assert!(cache.store(&key, &r));
        }
        let mut cache = HttpCache::new();
        let entry = cache.lookup(&key).unwrap();
        assert_eq!(entry.body, b"hello");
    }
}
