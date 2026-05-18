use std::io::{BufRead, BufReader, Read, Write};

use super::error::{Error, Result};

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub host: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn get(host: &str, path: &str) -> Self {
        Self {
            method: "GET".into(),
            path: path.into(),
            host: host.into(),
            headers: default_headers(),
            body: Vec::new(),
        }
    }

    pub fn post(host: &str, path: &str, body: Vec<u8>, content_type: &str) -> Self {
        let mut headers = default_headers();
        headers.push(("Content-Type".into(), content_type.into()));
        headers.push(("Content-Length".into(), body.len().to_string()));
        Self {
            method: "POST".into(),
            path: path.into(),
            host: host.into(),
            headers,
            body,
        }
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> Result<()> {
        write!(w, "{} {} HTTP/1.1\r\n", self.method, self.path)?;
        write!(w, "Host: {}\r\n", self.host)?;
        for (name, value) in &self.headers {
            write!(w, "{name}: {value}\r\n")?;
        }
        w.write_all(b"\r\n")?;
        if !self.body.is_empty() {
            w.write_all(&self.body)?;
        }
        w.flush()?;
        Ok(())
    }
}

fn default_headers() -> Vec<(String, String)> {
    vec![
        ("User-Agent".into(), "daboss/0.1".into()),
        ("Accept".into(), "*/*".into()),
        // Advertise the compressions we can decode. gzip/deflate go
        // through flate2; br through the brotli crate. `identity` stays
        // in the list so a server can opt out.
        ("Accept-Encoding".into(), "gzip, br, deflate, identity".into()),
        // Opt into HTTP/1.1 keep-alive. The transport layer pools the
        // connection back after each response unless the server (or
        // we, on an error) decide to close.
        ("Connection".into(), "keep-alive".into()),
    ]
}

/// Responses larger than this threshold get spilled to a tempfile on
/// disk instead of held in memory, so a multi-GB asset doesn't OOM
/// the page. Tuned for low-end Android: 4 MB covers virtually all
/// pages, scripts, stylesheets, and small images; anything bigger
/// (video, large binaries) streams.
pub const RESPONSE_BODY_SPILL_THRESHOLD: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    /// In-memory body bytes when the response fit under
    /// `RESPONSE_BODY_SPILL_THRESHOLD`. Empty when `body_path` is
    /// set — read from disk instead.
    pub body: Vec<u8>,
    /// Path to the on-disk body for large responses. Set when the
    /// body exceeded the spill threshold; `body` is empty in that
    /// case. The file is owned by this Response and unlinked when
    /// it drops.
    pub body_path: Option<std::path::PathBuf>,
}

impl Drop for Response {
    fn drop(&mut self) {
        if let Some(p) = self.body_path.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Total body size in bytes — works whether the body is in
    /// memory or on disk. Returns 0 if the on-disk file is missing.
    pub fn body_size(&self) -> u64 {
        if let Some(p) = &self.body_path {
            std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
        } else {
            self.body.len() as u64
        }
    }

    /// Read the full body into a `Vec<u8>` regardless of where it
    /// lives. Callers should prefer streaming for large responses;
    /// this exists for code paths that genuinely need the whole
    /// buffer (small assets, HTML pages, etc.).
    pub fn body_bytes(&self) -> Vec<u8> {
        if let Some(p) = &self.body_path {
            std::fs::read(p).unwrap_or_default()
        } else {
            self.body.clone()
        }
    }

    #[allow(dead_code)] // kept for tests and one-shot integrations
    pub fn read_from<R: Read>(reader: R, max_bytes: usize) -> Result<Self> {
        let mut br = BufReader::new(reader);
        Self::read_from_buf(&mut br, max_bytes)
    }

    /// Same as `read_from` but takes a caller-managed buffered reader.
    /// Lets the connection-pool path keep the same `BufReader` (and the
    /// underlying TCP/TLS stream) live across requests.
    pub fn read_from_buf<R: BufRead>(br: &mut R, max_bytes: usize) -> Result<Self> {
        let (status, reason) = read_status_line(br)?;
        let headers = read_headers(br)?;

        let mut sink = BodySink::new(max_bytes);
        if let Some(te) = find_header(&headers, "Transfer-Encoding") {
            if te.eq_ignore_ascii_case("chunked") {
                read_chunked_into(br, &mut sink)?;
            } else {
                return Err(Error::BadResponse(format!(
                    "unsupported transfer-encoding: {te}"
                )));
            }
        } else if let Some(cl) = find_header(&headers, "Content-Length") {
            let n = cl
                .trim()
                .parse::<usize>()
                .map_err(|_| Error::BadResponse(format!("bad content-length: {cl}")))?;
            if n > max_bytes {
                return Err(Error::ResponseTooLarge(max_bytes));
            }
            read_exact_into(br, n, &mut sink)?;
        } else {
            read_until_close_into(br, &mut sink)?;
        }
        let (body, body_path) = sink.finish()?;

        // Apply Content-Encoding decoders. Streams from disk when
        // the raw body spilled, so a 1 GB gzipped response decodes
        // in 64 KiB chunks without ever materialising the whole
        // compressed-or-decompressed buffer in RAM.
        let (body, body_path) = match find_header(&headers, "Content-Encoding") {
            None => (body, body_path),
            Some(enc) => decode_content_encoding(enc, body, body_path, max_bytes)?,
        };

        Ok(Self {
            status,
            reason,
            headers,
            body,
            body_path,
        })
    }
}

/// Body accumulator that buffers in-memory until it crosses
/// [`RESPONSE_BODY_SPILL_THRESHOLD`], then continues writing into a
/// tempfile. Caps total size at `max_bytes`.
pub(crate) struct BodySink {
    buffer: Vec<u8>,
    file: Option<std::fs::File>,
    path: Option<std::path::PathBuf>,
    total: usize,
    threshold: usize,
    max_bytes: usize,
}

impl BodySink {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            file: None,
            path: None,
            total: 0,
            threshold: RESPONSE_BODY_SPILL_THRESHOLD.min(max_bytes),
            max_bytes,
        }
    }

    /// Would writing `n` more bytes push us past `max_bytes`? Used
    /// to preflight a chunked / Content-Length advertised size
    /// before we start reading from the wire.
    pub fn would_exceed(&self, n: usize) -> bool {
        self.total.saturating_add(n) > self.max_bytes
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.total + bytes.len() > self.max_bytes {
            return Err(Error::ResponseTooLarge(self.max_bytes));
        }
        // Still buffering in memory?
        if self.file.is_none() && self.total + bytes.len() <= self.threshold {
            self.buffer.extend_from_slice(bytes);
            self.total += bytes.len();
            return Ok(());
        }
        // First time crossing the threshold — open a tempfile and
        // flush whatever's already buffered.
        if self.file.is_none() {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let mut path = std::env::temp_dir();
            path.push(format!("daboss-resp-{stamp:x}.bin"));
            let mut file = std::fs::File::create(&path).map_err(Error::Io)?;
            use std::io::Write;
            file.write_all(&self.buffer).map_err(Error::Io)?;
            self.buffer = Vec::new();
            self.file = Some(file);
            self.path = Some(path);
        }
        if let Some(f) = self.file.as_mut() {
            use std::io::Write;
            f.write_all(bytes).map_err(Error::Io)?;
        }
        self.total += bytes.len();
        Ok(())
    }

    /// Finalise: returns (inline_bytes, on_disk_path). Exactly one is
    /// populated based on whether we spilled.
    pub fn finish(mut self) -> Result<(Vec<u8>, Option<std::path::PathBuf>)> {
        if let Some(mut f) = self.file.take() {
            use std::io::Write;
            f.flush().map_err(Error::Io)?;
            drop(f);
            Ok((Vec::new(), self.path.take()))
        } else {
            Ok((std::mem::take(&mut self.buffer), None))
        }
    }
}

/// Decode a body whose `Content-Encoding` header lists one or more
/// codings (RFC 9110 §8.4). Streams from disk when the raw body
/// spilled past the spill threshold; otherwise stays in memory.
/// Each coding layer pipes through a fresh `BodySink` so the
/// decoded output can re-spill if it's also large.
fn decode_content_encoding(
    enc: &str,
    body: Vec<u8>,
    body_path: Option<std::path::PathBuf>,
    max_bytes: usize,
) -> Result<(Vec<u8>, Option<std::path::PathBuf>)> {
    let mut cur_body = body;
    let mut cur_path = body_path;
    for codings in enc.split(',') {
        let coding = codings.trim().to_ascii_lowercase();
        if coding.is_empty() || coding == "identity" {
            continue;
        }
        let mut sink = BodySink::new(max_bytes);
        decode_stream(&coding, &cur_body, cur_path.as_deref(), &mut sink)?;
        // Tempfile holding the input layer can be unlinked now.
        if let Some(old) = cur_path.take() {
            let _ = std::fs::remove_file(old);
        }
        let (out_body, out_path) = sink.finish()?;
        cur_body = out_body;
        cur_path = out_path;
    }
    Ok((cur_body, cur_path))
}

fn decode_stream(
    coding: &str,
    body: &[u8],
    body_path: Option<&std::path::Path>,
    sink: &mut BodySink,
) -> Result<()> {
    use std::io::Cursor;
    // Build a Read over whichever side has the bytes.
    let input: Box<dyn Read> = match body_path {
        Some(p) => Box::new(std::fs::File::open(p).map_err(Error::Io)?),
        None => Box::new(Cursor::new(body.to_vec())),
    };
    match coding {
        "gzip" | "x-gzip" => pipe_through(flate2::read::GzDecoder::new(input), sink),
        "deflate" => decode_deflate_stream(body, body_path, sink),
        "br" => {
            let mut reader = brotli::Decompressor::new(input, 4096);
            pipe_through(&mut reader, sink)
        }
        other => Err(Error::BadResponse(format!(
            "unsupported content-encoding: {other}"
        ))),
    }
}

fn decode_deflate_stream(
    body: &[u8],
    body_path: Option<&std::path::Path>,
    sink: &mut BodySink,
) -> Result<()> {
    // Sniff the first two bytes for a zlib header (0x78 + a checksum
    // byte). Most servers send the zlib-wrapped form per spec; raw
    // deflate is the older browser-compat fallback.
    let head: Vec<u8> = match body_path {
        Some(p) => {
            let mut f = std::fs::File::open(p).map_err(Error::Io)?;
            let mut buf = [0u8; 2];
            let n = f.read(&mut buf).map_err(Error::Io)?;
            buf[..n].to_vec()
        }
        None => body.iter().take(2).copied().collect(),
    };
    let zlib_form = head.first() == Some(&0x78);
    let input: Box<dyn Read> = match body_path {
        Some(p) => Box::new(std::fs::File::open(p).map_err(Error::Io)?),
        None => Box::new(std::io::Cursor::new(body.to_vec())),
    };
    if zlib_form {
        let dec = flate2::read::ZlibDecoder::new(input);
        pipe_through(dec, sink)
    } else {
        let dec = flate2::read::DeflateDecoder::new(input);
        pipe_through(dec, sink)
    }
}

fn pipe_through<R: Read>(mut reader: R, sink: &mut BodySink) -> Result<()> {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(Error::BadResponse(format!("decode read: {e}"))),
        };
        sink.write(&buf[..n])?;
    }
    Ok(())
}

fn read_status_line<R: BufRead>(r: &mut R) -> Result<(u16, String)> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Err(Error::BadResponse("connection closed before status line".into()));
    }
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    let status = parts.next().unwrap_or("");
    let reason = parts.next().unwrap_or("").to_string();
    if !version.starts_with("HTTP/") {
        return Err(Error::BadResponse(format!("bad status line: {line}")));
    }
    let status = status
        .parse::<u16>()
        .map_err(|_| Error::BadResponse(format!("bad status code: {status}")))?;
    Ok((status, reason))
}

fn read_headers<R: BufRead>(r: &mut R) -> Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Err(Error::BadResponse("connection closed inside headers".into()));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return Ok(headers);
        }
        if headers.len() >= 100 {
            return Err(Error::BadResponse("too many headers".into()));
        }
        let (name, value) = trimmed
            .split_once(':')
            .ok_or_else(|| Error::BadResponse(format!("bad header line: {trimmed}")))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn read_exact_into<R: Read>(r: &mut R, n: usize, sink: &mut BodySink) -> Result<()> {
    // Read `n` bytes through a 64 KiB scratch buffer so a 1 GB
    // Content-Length response never allocates 1 GB up front.
    let mut buf = [0u8; 64 * 1024];
    let mut remaining = n;
    while remaining > 0 {
        let want = remaining.min(buf.len());
        let read = r.read(&mut buf[..want])?;
        if read == 0 {
            return Err(Error::BadResponse(
                "connection closed inside content-length body".into(),
            ));
        }
        sink.write(&buf[..read])?;
        remaining -= read;
    }
    Ok(())
}

fn read_until_close_into<R: Read>(r: &mut R, sink: &mut BodySink) -> Result<()> {
    let mut tmp = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        sink.write(&tmp[..n])?;
    }
}

fn read_chunked_into<R: BufRead>(r: &mut R, sink: &mut BodySink) -> Result<()> {
    let mut tmp = [0u8; 64 * 1024];
    loop {
        let mut size_line = String::new();
        let n = r.read_line(&mut size_line)?;
        if n == 0 {
            return Err(Error::BadResponse(
                "connection closed inside chunked body".into(),
            ));
        }
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| Error::BadResponse(format!("bad chunk size: {size_line:?}")))?;
        if size == 0 {
            // consume trailers until blank line
            loop {
                let mut tr = String::new();
                let n = r.read_line(&mut tr)?;
                if n == 0 || tr.trim_end_matches(['\r', '\n']).is_empty() {
                    return Ok(());
                }
            }
        }
        // Preflight against the cap so a server that advertises a
        // huge single chunk fails fast (matches the
        // Content-Length-too-large behaviour).
        if sink.would_exceed(size) {
            return Err(Error::ResponseTooLarge(sink.max_bytes));
        }
        // Stream `size` bytes through the scratch buffer into the sink.
        let mut remaining = size;
        while remaining > 0 {
            let want = remaining.min(tmp.len());
            let read = r.read(&mut tmp[..want])?;
            if read == 0 {
                return Err(Error::BadResponse(
                    "connection closed mid-chunk".into(),
                ));
            }
            sink.write(&tmp[..read])?;
            remaining -= read;
        }
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf)?;
        if &crlf != b"\r\n" {
            return Err(Error::BadResponse("missing crlf after chunk".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_simple_response() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let resp = Response::read_from(Cursor::new(&data[..]), 1024).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.reason, "OK");
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn parses_chunked() {
        let data = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
            5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let resp = Response::read_from(Cursor::new(&data[..]), 1024).unwrap();
        assert_eq!(resp.body, b"hello world");
    }

    #[test]
    fn enforces_size_cap_via_content_length() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\n\r\n";
        let err = Response::read_from(Cursor::new(&data[..]), 100).unwrap_err();
        assert!(matches!(err, Error::ResponseTooLarge(_)));
    }

    #[test]
    fn enforces_size_cap_via_chunked() {
        // first chunk alone exceeds the cap
        let data = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nfa\r\n";
        let err = Response::read_from(Cursor::new(&data[..]), 100).unwrap_err();
        assert!(matches!(err, Error::ResponseTooLarge(_)));
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let data = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 0\r\n\r\n";
        let resp = Response::read_from(Cursor::new(&data[..]), 1024).unwrap();
        assert_eq!(resp.header("content-type"), Some("text/plain"));
        assert_eq!(resp.header("CONTENT-TYPE"), Some("text/plain"));
    }

    #[test]
    fn decodes_gzip_content_encoding() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"hello compressed world").unwrap();
        let compressed = enc.finish().unwrap();
        let mut wire = Vec::new();
        wire.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: ");
        wire.extend_from_slice(compressed.len().to_string().as_bytes());
        wire.extend_from_slice(b"\r\n\r\n");
        wire.extend_from_slice(&compressed);

        let resp = Response::read_from(Cursor::new(wire), 64 * 1024).unwrap();
        assert_eq!(resp.body, b"hello compressed world");
    }

    #[test]
    fn decodes_brotli_content_encoding() {
        let mut compressed = Vec::new();
        let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
        std::io::Write::write_all(&mut writer, b"brotlified contents").unwrap();
        drop(writer);

        let mut wire = Vec::new();
        wire.extend_from_slice(b"HTTP/1.1 200 OK\r\nContent-Encoding: br\r\nContent-Length: ");
        wire.extend_from_slice(compressed.len().to_string().as_bytes());
        wire.extend_from_slice(b"\r\n\r\n");
        wire.extend_from_slice(&compressed);

        let resp = Response::read_from(Cursor::new(wire), 64 * 1024).unwrap();
        assert_eq!(resp.body, b"brotlified contents");
    }

    #[test]
    fn identity_content_encoding_is_passthrough() {
        let data =
            b"HTTP/1.1 200 OK\r\nContent-Encoding: identity\r\nContent-Length: 5\r\n\r\nhello";
        let resp = Response::read_from(Cursor::new(&data[..]), 1024).unwrap();
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn unknown_encoding_errors_instead_of_serving_garbage() {
        let data =
            b"HTTP/1.1 200 OK\r\nContent-Encoding: xyzzy\r\nContent-Length: 3\r\n\r\nabc";
        let err = Response::read_from(Cursor::new(&data[..]), 1024).unwrap_err();
        assert!(matches!(err, Error::BadResponse(_)));
    }

    #[test]
    fn rejects_malformed_status_line() {
        let data = b"NOT_HTTP 200 OK\r\n\r\n";
        let err = Response::read_from(Cursor::new(&data[..]), 1024).unwrap_err();
        assert!(matches!(err, Error::BadResponse(_)));
    }
}
