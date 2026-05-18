//! Subresource Integrity (SRI) — W3C SRI Level 2.
//!
//! Verifies the `integrity` attribute on `<script>` / `<link
//! rel="stylesheet">` against the fetched response body. The format
//! is a space-separated list of `algo-base64hash` tokens, e.g.
//!
//! ```text
//! sha384-oqVuAfXRKap7fdgcCY5uykM6+R9GqQ8K/uxy9rx7HNQlGYl1kPzQho1wx4JwY8wC
//! ```
//!
//! Multiple tokens act as alternatives — verification succeeds if
//! ANY token matches. The strongest-supported algorithm always
//! wins per the spec; we approximate that by accepting any match
//! across the candidate list.
//!
//! Supported: sha256, sha384, sha512. Anything else is ignored
//! (treated as a no-op; the resource passes if no recognized
//! algorithm is listed, matching the spec's "no metadata" path).

use ring::digest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SriAlgo {
    Sha256,
    Sha384,
    Sha512,
}

impl SriAlgo {
    fn ring_alg(self) -> &'static digest::Algorithm {
        match self {
            SriAlgo::Sha256 => &digest::SHA256,
            SriAlgo::Sha384 => &digest::SHA384,
            SriAlgo::Sha512 => &digest::SHA512,
        }
    }
}

/// Result of verifying an `integrity` attribute against a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SriVerdict {
    /// No `integrity` attribute (or only unrecognized algorithms) —
    /// resource is allowed through.
    NoMetadata,
    /// Body digest matched one of the listed hashes.
    Matched,
    /// Body digest didn't match any listed hash. Resource MUST be
    /// blocked.
    Failed,
}

impl SriVerdict {
    pub fn allows_load(self) -> bool {
        matches!(self, SriVerdict::NoMetadata | SriVerdict::Matched)
    }
}

/// Parse and verify an `integrity` attribute against `body`.
pub fn verify_integrity(integrity: &str, body: &[u8]) -> SriVerdict {
    let mut had_recognized = false;
    for tok in integrity.split_ascii_whitespace() {
        let Some((algo_str, b64)) = tok.split_once('-') else {
            continue;
        };
        let algo = match algo_str.to_ascii_lowercase().as_str() {
            "sha256" => SriAlgo::Sha256,
            "sha384" => SriAlgo::Sha384,
            "sha512" => SriAlgo::Sha512,
            _ => continue,
        };
        had_recognized = true;
        let Some(expected) = base64_decode(b64) else {
            continue;
        };
        let actual = digest::digest(algo.ring_alg(), body);
        if constant_time_eq(actual.as_ref(), &expected) {
            return SriVerdict::Matched;
        }
    }
    if had_recognized {
        SriVerdict::Failed
    } else {
        SriVerdict::NoMetadata
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

const STD_ALPHA: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    // SRI accepts both unpadded and padded base64.
    let trimmed = s.trim_end_matches('=');
    let mut decode_table = [255u8; 256];
    for (i, b) in STD_ALPHA.iter().enumerate() {
        decode_table[*b as usize] = i as u8;
    }
    let bytes = trimmed.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        let v = decode_table[b as usize];
        if v == 255 {
            return None;
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_metadata_passes() {
        assert_eq!(verify_integrity("", b"hello"), SriVerdict::NoMetadata);
        assert_eq!(
            verify_integrity("foo-bar", b"hello"),
            SriVerdict::NoMetadata
        );
    }

    #[test]
    fn matching_sha256_passes() {
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        // base64 = LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=
        let i = "sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";
        assert_eq!(verify_integrity(i, b"hello"), SriVerdict::Matched);
    }

    #[test]
    fn matching_sha384_passes() {
        // sha384("hello") base64.
        // openssl dgst -sha384 -binary | base64 →
        // 59e1748777448c69de6b800d7a33bbfb9ff1b463e44354c3553bcdb9c666fa90125a3c79f90397bdf5f6a13de828684f
        // base64: WeF0h3dEjGnea4ANejO7+5/xtGPkQ1TDVTvNucZm+pASWjx5+QOXvfX2oT3oKGhP
        let i = "sha384-WeF0h3dEjGnea4ANejO7+5/xtGPkQ1TDVTvNucZm+pASWjx5+QOXvfX2oT3oKGhP";
        assert_eq!(verify_integrity(i, b"hello"), SriVerdict::Matched);
    }

    #[test]
    fn failing_hash_blocks() {
        let i = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        assert_eq!(verify_integrity(i, b"hello"), SriVerdict::Failed);
    }

    #[test]
    fn alternatives_any_match_passes() {
        let i = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA= \
                 sha256-LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=";
        assert_eq!(verify_integrity(i, b"hello"), SriVerdict::Matched);
    }

    #[test]
    fn unrecognized_only_is_no_metadata() {
        assert_eq!(
            verify_integrity("md5-abc sha1-def", b"x"),
            SriVerdict::NoMetadata
        );
    }
}
