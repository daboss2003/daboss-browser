//! HLS playlist parser + segment-fetch driver.
//!
//! RFC 8216 defines two flavours of M3U8:
//!
//!   * **Master** playlist — lists variant streams keyed by
//!     bandwidth / resolution / codecs. Each `#EXT-X-STREAM-INF:<attr>`
//!     line is followed by a URI line pointing at a Media playlist.
//!
//!   * **Media** playlist — lists actual segment URIs in order.
//!     Each `#EXTINF:<duration>,<title>` line is followed by a URI
//!     for the segment (usually `.ts` or `.m4s`).
//!
//! We parse both, pick the highest-bandwidth variant from a master
//! playlist, and expose the segment list so the caller can fetch
//! them sequentially. Live-playlist refresh, byte-range requests,
//! EXT-X-MAP init segments, EXT-X-KEY encryption, and discontinuity
//! tags are deliberately out of scope for the first slice — VOD
//! single-bitrate playlists already cover the bulk of HLS content.

use url::Url;

use super::error::{Error, Result};
use super::{Client, RequestContext};

/// One variant in a master playlist. Sortable by `bandwidth` so the
/// adaptive picker can grab the largest, smallest, or a target.
#[derive(Debug, Clone)]
pub struct Variant {
    pub uri: String,
    pub bandwidth: u64,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub uri: String,
    pub duration_secs: f32,
}

#[derive(Debug, Clone)]
pub struct MasterPlaylist {
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone)]
pub struct MediaPlaylist {
    pub target_duration_secs: u32,
    pub segments: Vec<Segment>,
    pub end_list: bool,
}

#[derive(Debug, Clone)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

/// Parse `text` as an M3U8 playlist. Returns `None` if the file
/// doesn't start with the `#EXTM3U` tag — we don't try to recover
/// from corrupt input.
pub fn parse(text: &str) -> Option<Playlist> {
    let mut lines = text.lines().map(str::trim);
    let first = lines.next()?;
    if !first.starts_with("#EXTM3U") {
        return None;
    }

    // First pass: determine whether the playlist mentions any
    // STREAM-INF tags. If so, it's a master playlist.
    if text.contains("#EXT-X-STREAM-INF") {
        return Some(Playlist::Master(parse_master(text)?));
    }
    Some(Playlist::Media(parse_media(text)?))
}

fn parse_master(text: &str) -> Option<MasterPlaylist> {
    let mut variants: Vec<Variant> = Vec::new();
    let mut pending: Option<(u64, Option<(u32, u32)>, Option<String>)> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = parse_attr_list(rest);
            let bandwidth = attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("BANDWIDTH"))
                .and_then(|(_, v)| v.parse::<u64>().ok())
                .unwrap_or(0);
            let resolution = attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("RESOLUTION"))
                .and_then(|(_, v)| {
                    let mut split = v.split('x');
                    let w = split.next()?.parse().ok()?;
                    let h = split.next()?.parse().ok()?;
                    Some((w, h))
                });
            let codecs = attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("CODECS"))
                .map(|(_, v)| v.clone());
            pending = Some((bandwidth, resolution, codecs));
        } else if line.starts_with('#') {
            continue;
        } else if let Some((bandwidth, resolution, codecs)) = pending.take() {
            variants.push(Variant {
                uri: line.to_string(),
                bandwidth,
                resolution,
                codecs,
            });
        }
    }
    Some(MasterPlaylist { variants })
}

fn parse_media(text: &str) -> Option<MediaPlaylist> {
    let mut target_duration_secs = 0u32;
    let mut end_list = false;
    let mut segments: Vec<Segment> = Vec::new();
    let mut pending_duration: Option<f32> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            // `#EXTINF:<duration>,<title>` — only the duration is
            // load-bearing.
            let d_str = rest.split(',').next().unwrap_or("");
            pending_duration = d_str.parse::<f32>().ok();
        } else if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration_secs = rest.parse::<u32>().unwrap_or(0);
        } else if line.starts_with("#EXT-X-ENDLIST") {
            end_list = true;
        } else if line.starts_with('#') {
            continue;
        } else {
            let dur = pending_duration.take().unwrap_or(0.0);
            segments.push(Segment {
                uri: line.to_string(),
                duration_secs: dur,
            });
        }
    }
    Some(MediaPlaylist {
        target_duration_secs,
        segments,
        end_list,
    })
}

/// Parse `KEY=VALUE,KEY=VALUE,...` with quoted strings respected so
/// commas inside `"H.264,mp4a.40.2"` don't split keys.
fn parse_attr_list(s: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b',') {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            break;
        }
        let key = String::from_utf8_lossy(&bytes[key_start..i]).into_owned();
        i += 1; // skip '='
        let value = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let v_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let v = String::from_utf8_lossy(&bytes[v_start..i]).into_owned();
            if i < bytes.len() {
                i += 1; // skip closing "
            }
            v
        } else {
            let v_start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            String::from_utf8_lossy(&bytes[v_start..i]).into_owned()
        };
        out.push((key.trim().to_string(), value.trim().to_string()));
    }
    out
}

/// Heuristic: does `headers.Content-Type` or `url` look like an HLS
/// playlist? Pages that serve `.m3u8` without a useful MIME still
/// flow through.
pub fn looks_like_hls(content_type: Option<&str>, url: &Url) -> bool {
    if let Some(ct) = content_type {
        let low = ct.to_ascii_lowercase();
        if low.contains("mpegurl") || low.contains("application/x-mpegurl") {
            return true;
        }
    }
    url.path().to_ascii_lowercase().ends_with(".m3u8")
}

/// Convenience: pick the highest-bandwidth variant from a master
/// playlist. Returns `None` if the variant list is empty.
pub fn pick_top_variant(master: &MasterPlaylist) -> Option<&Variant> {
    master.variants.iter().max_by_key(|v| v.bandwidth)
}

/// Drive an HLS playlist URL all the way down to a concatenated
/// byte buffer ffmpeg can decode:
///   1. Fetch the (possibly master) playlist.
///   2. If master, follow the top variant.
///   3. Sequentially fetch each segment, concatenating into one
///      buffer.
///
/// Returns the concatenated body or an [`Error`] on first fetch
/// failure. Caller is responsible for writing the buffer to disk
/// + handing the path to ffmpeg.
///
/// Limitations of this slice:
///   * Single-pass; no adaptive bitrate switching mid-playback.
///   * No live refresh — `#EXT-X-ENDLIST` is assumed.
///   * Total-byte cap inherits from the network client.
pub fn fetch_and_concat(
    client: &Client,
    playlist_url: &Url,
    ctx: RequestContext,
) -> Result<Vec<u8>> {
    let resp = client.get_with(playlist_url.as_str(), ctx.clone())?;
    if !(200..300).contains(&resp.status) {
        return Err(Error::BadResponse(format!(
            "playlist {}: HTTP {}",
            playlist_url, resp.status
        )));
    }
    let text = std::str::from_utf8(&resp.body).map_err(|e| Error::BadResponse(e.to_string()))?;
    let parsed = parse(text).ok_or_else(|| {
        Error::BadResponse(format!(
            "playlist {playlist_url} doesn't start with #EXTM3U"
        ))
    })?;
    let media: MediaPlaylist = match parsed {
        Playlist::Media(m) => m,
        Playlist::Master(master) => {
            let variant = pick_top_variant(&master).ok_or_else(|| {
                Error::BadResponse("master playlist has no variants".into())
            })?;
            let variant_url = playlist_url
                .join(&variant.uri)
                .map_err(|e| Error::BadResponse(e.to_string()))?;
            let variant_resp =
                client.get_with(variant_url.as_str(), ctx.clone())?;
            if !(200..300).contains(&variant_resp.status) {
                return Err(Error::BadResponse(format!(
                    "variant {}: HTTP {}",
                    variant_url, variant_resp.status
                )));
            }
            let variant_text = std::str::from_utf8(&variant_resp.body)
                .map_err(|e| Error::BadResponse(e.to_string()))?;
            match parse(variant_text).ok_or_else(|| {
                Error::BadResponse("variant playlist invalid".into())
            })? {
                Playlist::Media(m) => m,
                Playlist::Master(_) => {
                    return Err(Error::BadResponse(
                        "variant resolved to another master playlist".into(),
                    ));
                }
            }
        }
    };

    let mut out = Vec::new();
    for seg in &media.segments {
        let seg_url = playlist_url
            .join(&seg.uri)
            .map_err(|e| Error::BadResponse(e.to_string()))?;
        let seg_resp = client.get_with(seg_url.as_str(), ctx.clone())?;
        if !(200..300).contains(&seg_resp.status) {
            return Err(Error::BadResponse(format!(
                "segment {}: HTTP {}",
                seg_url, seg_resp.status
            )));
        }
        out.extend_from_slice(&seg_resp.body);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=640x360,CODECS=\"avc1.42e01e,mp4a.40.2\"\n\
        low.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\n\
        high.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\n\
        mid.m3u8\n";

    const MEDIA: &str = "#EXTM3U\n\
        #EXT-X-VERSION:3\n\
        #EXT-X-TARGETDURATION:6\n\
        #EXT-X-MEDIA-SEQUENCE:0\n\
        #EXTINF:5.0,\n\
        seg0.ts\n\
        #EXTINF:6.0,\n\
        seg1.ts\n\
        #EXTINF:4.5,\n\
        seg2.ts\n\
        #EXT-X-ENDLIST\n";

    #[test]
    fn parses_master_playlist_with_three_variants() {
        let p = parse(MASTER).expect("parses");
        match p {
            Playlist::Master(m) => {
                assert_eq!(m.variants.len(), 3);
                assert_eq!(m.variants[0].bandwidth, 1_280_000);
                assert_eq!(m.variants[0].resolution, Some((640, 360)));
                assert!(m.variants[0]
                    .codecs
                    .as_deref()
                    .unwrap()
                    .contains("avc1"));
                assert_eq!(m.variants[1].uri, "high.m3u8");
                assert_eq!(m.variants[2].uri, "mid.m3u8");
            }
            _ => panic!("expected master"),
        }
    }

    #[test]
    fn parses_media_playlist_with_segments_and_endlist() {
        let p = parse(MEDIA).expect("parses");
        match p {
            Playlist::Media(m) => {
                assert_eq!(m.target_duration_secs, 6);
                assert!(m.end_list);
                assert_eq!(m.segments.len(), 3);
                assert_eq!(m.segments[0].uri, "seg0.ts");
                assert!((m.segments[1].duration_secs - 6.0).abs() < 0.01);
                assert!((m.segments[2].duration_secs - 4.5).abs() < 0.01);
            }
            _ => panic!("expected media"),
        }
    }

    #[test]
    fn rejects_files_without_extm3u_header() {
        let p = parse("garbage\nseg0.ts\n");
        assert!(p.is_none());
    }

    #[test]
    fn pick_top_variant_returns_largest_bandwidth() {
        let p = parse(MASTER).expect("parses");
        match p {
            Playlist::Master(m) => {
                let top = pick_top_variant(&m).expect("at least one variant");
                assert_eq!(top.bandwidth, 5_000_000);
                assert_eq!(top.uri, "high.m3u8");
            }
            _ => panic!("expected master"),
        }
    }

    #[test]
    fn looks_like_hls_detects_mime_and_extension() {
        let mime_yes = Some("application/vnd.apple.mpegurl");
        let mime_no = Some("video/mp4");
        let u_yes: Url = "https://x.test/playlist.m3u8".parse().unwrap();
        let u_no: Url = "https://x.test/movie.mp4".parse().unwrap();
        assert!(looks_like_hls(mime_yes, &u_no));
        assert!(looks_like_hls(mime_no, &u_yes));
        assert!(!looks_like_hls(mime_no, &u_no));
        assert!(!looks_like_hls(None, &u_no));
    }

    #[test]
    fn attr_list_keeps_quoted_commas() {
        let attrs = parse_attr_list(
            "BANDWIDTH=1280000,RESOLUTION=640x360,CODECS=\"avc1.42e01e,mp4a.40.2\"",
        );
        let codecs = attrs.iter().find(|(k, _)| k == "CODECS").unwrap();
        assert_eq!(codecs.1, "avc1.42e01e,mp4a.40.2");
    }
}
