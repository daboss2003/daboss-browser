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
        // identity: refuse compression so we don't have to decode gzip/br yet.
        ("Accept-Encoding".into(), "identity".into()),
        // Drop the connection after one request — keeps the parser simple.
        ("Connection".into(), "close".into()),
    ]
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn read_from<R: Read>(reader: R, max_bytes: usize) -> Result<Self> {
        let mut br = BufReader::new(reader);
        let (status, reason) = read_status_line(&mut br)?;
        let headers = read_headers(&mut br)?;

        let body = if let Some(te) = find_header(&headers, "Transfer-Encoding") {
            if te.eq_ignore_ascii_case("chunked") {
                read_chunked(&mut br, max_bytes)?
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
            read_exact_vec(&mut br, n)?
        } else {
            read_until_close(&mut br, max_bytes)?
        };

        Ok(Self {
            status,
            reason,
            headers,
            body,
        })
    }
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

fn read_exact_vec<R: Read>(r: &mut R, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_until_close<R: Read>(r: &mut R, max_bytes: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = r.read(&mut tmp)?;
        if n == 0 {
            return Ok(buf);
        }
        if buf.len() + n > max_bytes {
            return Err(Error::ResponseTooLarge(max_bytes));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn read_chunked<R: BufRead>(r: &mut R, max_bytes: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        let n = r.read_line(&mut size_line)?;
        if n == 0 {
            return Err(Error::BadResponse("connection closed inside chunked body".into()));
        }
        // chunk-ext (";k=v") allowed by spec, we ignore it
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| Error::BadResponse(format!("bad chunk size: {size_line:?}")))?;
        if size == 0 {
            // consume trailers until blank line
            loop {
                let mut tr = String::new();
                let n = r.read_line(&mut tr)?;
                if n == 0 || tr.trim_end_matches(['\r', '\n']).is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > max_bytes {
            return Err(Error::ResponseTooLarge(max_bytes));
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..])?;
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
    fn rejects_malformed_status_line() {
        let data = b"NOT_HTTP 200 OK\r\n\r\n";
        let err = Response::read_from(Cursor::new(&data[..]), 1024).unwrap_err();
        assert!(matches!(err, Error::BadResponse(_)));
    }
}
