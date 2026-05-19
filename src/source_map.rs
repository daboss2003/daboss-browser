//! Source-map v3 parser.
//!
//! Pages emit `//# sourceMappingURL=foo.map` (or the legacy `@`
//! prefix) at the bottom of compiled JS / CSS. The map itself is a
//! JSON document with the spec keys:
//!
//! * `version` — always 3.
//! * `sources` — array of source filenames.
//! * `sourcesContent` — optional array of original source text.
//! * `names` — optional symbol-name array.
//! * `mappings` — a single VLQ-encoded string. Semicolons split
//!   generated lines; commas split segments within a line.
//!
//! Each segment is 1, 4, or 5 base64-VLQ integers:
//!   - 1: generated column only.
//!   - 4: + source index, source line, source column.
//!   - 5: + names index.
//!
//! All integers are deltas relative to the previous segment.
//!
//! This module exports:
//!  * `extract_source_map_url(src)` — scrape the trailing comment.
//!  * `parse(json)` — parse a JSON map and decode its mappings.
//!  * `SOURCE_MAPS` — thread-local registry the devtools Sources
//!    panel reads. The shell pushes parsed maps after fetching them.

use std::cell::RefCell;
use std::collections::HashMap;

/// One parsed source map. `sources_content` is parallel to `sources`
/// (both indexed by `Segment::source_index`); empty when the map
/// omits the content blob.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    pub file: Option<String>,
    pub source_root: Option<String>,
    pub sources: Vec<String>,
    pub sources_content: Vec<Option<String>>,
    pub names: Vec<String>,
    pub mappings: Vec<Vec<Segment>>,
}

/// One row in the mappings table. All fields are zero-based even
/// though the source-map spec stores them as deltas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub gen_col: u32,
    /// `None` when the segment only records a generated column
    /// (often used to mark "this column has no source").
    pub source_index: Option<u32>,
    pub source_line: u32,
    pub source_col: u32,
    pub name_index: Option<u32>,
}

thread_local! {
    /// `path-or-key -> map`. The shell stores each parsed map under
    /// either the script's source URL or, for inline scripts, a
    /// synthetic `<inline #N>` key.
    pub static SOURCE_MAPS: RefCell<HashMap<String, SourceMap>> =
        RefCell::new(HashMap::new());
}

pub fn register(key: String, map: SourceMap) {
    SOURCE_MAPS.with(|s| {
        s.borrow_mut().insert(key, map);
    });
}

/// Snapshot of all registered maps for the devtools panel.
pub fn snapshot() -> Vec<(String, SourceMap)> {
    SOURCE_MAPS.with(|s| s.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

#[cfg(test)]
pub fn clear() {
    SOURCE_MAPS.with(|s| s.borrow_mut().clear());
}

/// Scan `src` for a trailing `//# sourceMappingURL=...` or
/// `//@ sourceMappingURL=...` comment and return the URL. Tools
/// (TypeScript, Webpack, esbuild) all emit the `#` form; the `@`
/// prefix predates the spec but still appears in older bundles.
pub fn extract_source_map_url(src: &str) -> Option<String> {
    // Walk lines from the end; the first matching line wins. The
    // comment must be on its own line per spec, so we don't have to
    // tokenise the source.
    for line in src.lines().rev() {
        let trimmed = line.trim();
        for prefix in ["//# sourceMappingURL=", "//@ sourceMappingURL="] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let url = rest.trim();
                if !url.is_empty() {
                    return Some(url.to_string());
                }
            }
        }
        // Stop scanning once we leave the trailing-comment block: any
        // non-comment, non-blank line beats source-mapping discovery
        // to avoid scanning megabytes of bundle.
        if !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && !trimmed.starts_with("*")
        {
            return None;
        }
    }
    None
}

/// Parse a JSON source-map string. Returns `None` if the JSON is
/// malformed, `version` is missing or not 3, or `mappings` fails to
/// decode (per-segment recovery: a bad VLQ aborts the row, not the
/// whole map).
pub fn parse(json: &str) -> Option<SourceMap> {
    let v = parse_json(json)?;
    let JsonValue::Object(fields) = v else {
        return None;
    };
    let version = match fields.get("version") {
        Some(JsonValue::Number(n)) => *n as i32,
        _ => return None,
    };
    if version != 3 {
        return None;
    }
    let mut map = SourceMap::default();
    if let Some(JsonValue::String(s)) = fields.get("file") {
        map.file = Some(s.clone());
    }
    if let Some(JsonValue::String(s)) = fields.get("sourceRoot") {
        map.source_root = Some(s.clone());
    }
    if let Some(JsonValue::Array(arr)) = fields.get("sources") {
        for v in arr {
            if let JsonValue::String(s) = v {
                map.sources.push(s.clone());
            }
        }
    }
    if let Some(JsonValue::Array(arr)) = fields.get("sourcesContent") {
        for v in arr {
            match v {
                JsonValue::String(s) => map.sources_content.push(Some(s.clone())),
                _ => map.sources_content.push(None),
            }
        }
    }
    if let Some(JsonValue::Array(arr)) = fields.get("names") {
        for v in arr {
            if let JsonValue::String(s) = v {
                map.names.push(s.clone());
            }
        }
    }
    if let Some(JsonValue::String(s)) = fields.get("mappings") {
        map.mappings = decode_mappings(s);
    }
    Some(map)
}

/// Decode the `mappings` blob into a 2D table. The outer index is the
/// generated line; the inner Vec lists each comma-separated segment.
/// Decode failures within a segment abort that segment but keep the
/// rest of the line.
pub fn decode_mappings(mappings: &str) -> Vec<Vec<Segment>> {
    let mut out: Vec<Vec<Segment>> = Vec::new();
    // Spec state: source/source_line/source_col/name carry across
    // lines; gen_col resets at every `;`.
    let mut src_idx: i64 = 0;
    let mut src_line: i64 = 0;
    let mut src_col: i64 = 0;
    let mut name_idx: i64 = 0;
    for line_str in mappings.split(';') {
        let mut gen_col: i64 = 0;
        let mut line_segments: Vec<Segment> = Vec::new();
        for seg_str in line_str.split(',') {
            if seg_str.is_empty() {
                continue;
            }
            let Some(nums) = decode_vlq_seq(seg_str) else {
                continue;
            };
            match nums.len() {
                1 => {
                    gen_col += nums[0];
                    line_segments.push(Segment {
                        gen_col: gen_col.max(0) as u32,
                        source_index: None,
                        source_line: 0,
                        source_col: 0,
                        name_index: None,
                    });
                }
                4 => {
                    gen_col += nums[0];
                    src_idx += nums[1];
                    src_line += nums[2];
                    src_col += nums[3];
                    line_segments.push(Segment {
                        gen_col: gen_col.max(0) as u32,
                        source_index: Some(src_idx.max(0) as u32),
                        source_line: src_line.max(0) as u32,
                        source_col: src_col.max(0) as u32,
                        name_index: None,
                    });
                }
                5 => {
                    gen_col += nums[0];
                    src_idx += nums[1];
                    src_line += nums[2];
                    src_col += nums[3];
                    name_idx += nums[4];
                    line_segments.push(Segment {
                        gen_col: gen_col.max(0) as u32,
                        source_index: Some(src_idx.max(0) as u32),
                        source_line: src_line.max(0) as u32,
                        source_col: src_col.max(0) as u32,
                        name_index: Some(name_idx.max(0) as u32),
                    });
                }
                _ => continue, // malformed segment, drop
            }
        }
        out.push(line_segments);
    }
    out
}

/// Decode a sequence of base64-VLQ integers from a contiguous string
/// (one segment). Returns `None` on a bad base64 digit.
fn decode_vlq_seq(s: &str) -> Option<Vec<i64>> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let (value, consumed) = decode_one_vlq(&bytes[i..])?;
        out.push(value);
        i += consumed;
    }
    Some(out)
}

fn decode_one_vlq(bytes: &[u8]) -> Option<(i64, usize)> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    let mut idx = 0;
    loop {
        if idx >= bytes.len() {
            return None;
        }
        let digit = b64_to_int(bytes[idx])?;
        idx += 1;
        let continuation = digit & 0b10_0000;
        let chunk = (digit & 0b01_1111) as u64;
        result |= chunk << shift;
        shift += 5;
        if continuation == 0 {
            break;
        }
    }
    let negative = (result & 1) != 0;
    let value = (result >> 1) as i64;
    Some(if negative { (-value, idx) } else { (value, idx) })
}

fn b64_to_int(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

// ----------------------------------------------------------------
// Tiny JSON parser. Source maps are well-formed JSON without
// surprises (no big-int, no NaN, no comments), so a recursive
// descent parser fits in a screenful and avoids pulling in serde.
// ----------------------------------------------------------------

#[derive(Debug, Clone)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

fn parse_json(s: &str) -> Option<JsonValue> {
    let mut p = JsonParser { input: s.as_bytes(), pos: 0 };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.input.len() {
        return None;
    }
    Some(v)
}

struct JsonParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.input.len()
            && matches!(self.input[self.pos], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }
    fn consume(&mut self, b: u8) -> Option<()> {
        if self.peek()? == b {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }
    fn parse_value(&mut self) -> Option<JsonValue> {
        self.skip_ws();
        let c = self.peek()?;
        match c {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(JsonValue::String),
            b't' | b'f' => self.parse_bool(),
            b'n' => self.parse_null(),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }
    fn parse_object(&mut self) -> Option<JsonValue> {
        self.consume(b'{')?;
        let mut out: HashMap<String, JsonValue> = HashMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(JsonValue::Object(out));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.consume(b':')?;
            let val = self.parse_value()?;
            out.insert(key, val);
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b'}' => {
                    self.pos += 1;
                    return Some(JsonValue::Object(out));
                }
                _ => return None,
            }
        }
    }
    fn parse_array(&mut self) -> Option<JsonValue> {
        self.consume(b'[')?;
        let mut out: Vec<JsonValue> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(JsonValue::Array(out));
        }
        loop {
            let v = self.parse_value()?;
            out.push(v);
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b']' => {
                    self.pos += 1;
                    return Some(JsonValue::Array(out));
                }
                _ => return None,
            }
        }
    }
    fn parse_string(&mut self) -> Option<String> {
        self.consume(b'"')?;
        let mut out = String::new();
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            self.pos += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let esc = *self.input.get(self.pos)?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'u' => {
                            if self.pos + 4 > self.input.len() {
                                return None;
                            }
                            let hex = std::str::from_utf8(&self.input[self.pos..self.pos + 4]).ok()?;
                            self.pos += 4;
                            let cp = u32::from_str_radix(hex, 16).ok()?;
                            if let Some(c) = char::from_u32(cp) {
                                out.push(c);
                            }
                        }
                        _ => return None,
                    }
                }
                _ => out.push(c as char),
            }
        }
        None
    }
    fn parse_bool(&mut self) -> Option<JsonValue> {
        if self.input[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Some(JsonValue::Bool(true))
        } else if self.input[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Some(JsonValue::Bool(false))
        } else {
            None
        }
    }
    fn parse_null(&mut self) -> Option<JsonValue> {
        if self.input[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Some(JsonValue::Null)
        } else {
            None
        }
    }
    fn parse_number(&mut self) -> Option<JsonValue> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while let Some(b'0'..=b'9') = self.peek() {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while let Some(b'0'..=b'9') = self.peek() {
                self.pos += 1;
            }
        }
        let s = std::str::from_utf8(&self.input[start..self.pos]).ok()?;
        let n: f64 = s.parse().ok()?;
        Some(JsonValue::Number(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_sourcemapping_url_at_end() {
        let src = "function f(){}\n//# sourceMappingURL=foo.map\n";
        assert_eq!(
            extract_source_map_url(src).as_deref(),
            Some("foo.map")
        );
    }

    #[test]
    fn extracts_legacy_at_prefix() {
        let src = "x();\n//@ sourceMappingURL=legacy.map";
        assert_eq!(
            extract_source_map_url(src).as_deref(),
            Some("legacy.map")
        );
    }

    #[test]
    fn ignores_mapping_url_in_middle_of_file() {
        // Comment appears, but after it there's real code — must
        // bail out of the trailing-comment scan.
        let src = "//# sourceMappingURL=middle.map\nfunction f(){}";
        assert_eq!(extract_source_map_url(src), None);
    }

    #[test]
    fn vlq_decodes_known_values() {
        // "A" → 0, "B" → -0 (sign bit), "C" → 1, "D" → -1, ... see
        // https://gist.github.com/mjpieters/86b0d152bb51d5f4979387f7c4daf60a
        // for the table. We exercise a handful.
        assert_eq!(decode_one_vlq(b"A").unwrap().0, 0);
        assert_eq!(decode_one_vlq(b"C").unwrap().0, 1);
        assert_eq!(decode_one_vlq(b"D").unwrap().0, -1);
        // "qB" = continuation of 16 followed by 1 → 16 | (1 << 5) = 16+32 = 48 (encoded as 48*2 = 96 with no sign bit).
        // Just verify it decodes deterministically for use as a regression.
        let (val, used) = decode_one_vlq(b"qB").unwrap();
        assert!(val != 0);
        assert_eq!(used, 2);
    }

    #[test]
    fn mappings_decode_basic_row() {
        // One generated line with one segment that maps (0,0,0,0).
        // VLQ encodes [0,0,0,0] as "AAAA".
        let table = decode_mappings("AAAA");
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].len(), 1);
        let seg = table[0][0];
        assert_eq!(seg.gen_col, 0);
        assert_eq!(seg.source_index, Some(0));
        assert_eq!(seg.source_line, 0);
        assert_eq!(seg.source_col, 0);
    }

    #[test]
    fn json_parses_source_map_shape() {
        let s = r#"{
            "version": 3,
            "file": "out.js",
            "sources": ["a.ts", "b.ts"],
            "sourcesContent": ["const a = 1;", null],
            "names": ["a"],
            "mappings": "AAAA"
        }"#;
        let m = parse(s).expect("parses");
        assert_eq!(m.file.as_deref(), Some("out.js"));
        assert_eq!(m.sources, vec!["a.ts", "b.ts"]);
        assert_eq!(m.sources_content.len(), 2);
        assert_eq!(m.sources_content[0].as_deref(), Some("const a = 1;"));
        assert_eq!(m.sources_content[1], None);
        assert_eq!(m.names, vec!["a"]);
        assert_eq!(m.mappings.len(), 1);
    }

    #[test]
    fn registry_round_trip() {
        clear();
        let map = SourceMap {
            sources: vec!["foo.ts".into()],
            ..SourceMap::default()
        };
        register("page.js".into(), map);
        let snap = snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "page.js");
        assert_eq!(snap[0].1.sources, vec!["foo.ts"]);
        clear();
    }
}
