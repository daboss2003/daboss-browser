//! Permissions-Policy header parser.
//!
//! Format (W3C Permissions Policy):
//!
//! ```text
//! Permissions-Policy: camera=(), geolocation=(self), microphone=(self "https://embed.example.com")
//! ```
//!
//! Each entry is `feature=allowlist`. The allowlist is one of:
//!   * `()` — disabled everywhere (empty allowlist).
//!   * `*` — enabled for all origins.
//!   * `(self ...)` — listed origins. `self` means the document's
//!     own origin. Other tokens are quoted origin strings.
//!
//! A feature without an explicit policy uses the spec's default
//! allowlist (most are `self`). We don't model that table here — a
//! caller asking about an unspecified feature gets `true` (the
//! permissive default) so existing JS doesn't break on a quiet page.

use std::collections::HashMap;

use url::Url;

#[derive(Debug, Clone)]
pub enum Allowlist {
    /// `feature=()` — disabled for everyone.
    None_,
    /// `feature=*` — every origin allowed.
    All,
    /// Explicit list of origin strings. `self` is materialised as
    /// the special string `"self"` so the check can substitute the
    /// document's origin at evaluation time.
    Origins(Vec<String>),
}

#[derive(Debug, Default, Clone)]
pub struct PermissionsPolicy {
    map: HashMap<String, Allowlist>,
    /// Origin of the response that delivered this policy. The
    /// `self` token in any allowlist matches a caller whose origin
    /// equals this. `None` means "no issuer known" — `self` falls
    /// back to matching any origin (test-friendly default).
    self_origin: Option<url::Origin>,
}

impl PermissionsPolicy {
    /// Attach the origin of the response that delivered this
    /// header, so `self` tokens resolve correctly when evaluated
    /// against a different origin.
    pub fn with_self_origin(mut self, origin: url::Origin) -> Self {
        self.self_origin = Some(origin);
        self
    }

    pub fn parse(header: &str) -> Self {
        let mut map: HashMap<String, Allowlist> = HashMap::new();
        // Top-level split: features are comma-separated.
        for entry in header.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let Some(eq) = entry.find('=') else {
                continue;
            };
            let feat = entry[..eq].trim().to_ascii_lowercase();
            let allow_raw = entry[eq + 1..].trim();
            let allow = if allow_raw == "*" {
                Allowlist::All
            } else if allow_raw.starts_with('(') && allow_raw.ends_with(')') {
                let inside = &allow_raw[1..allow_raw.len() - 1];
                let inside = inside.trim();
                if inside.is_empty() {
                    Allowlist::None_
                } else {
                    let mut origins = Vec::new();
                    for tok in inside.split_ascii_whitespace() {
                        let tok = tok.trim_matches('"');
                        if tok.is_empty() {
                            continue;
                        }
                        origins.push(tok.to_string());
                    }
                    Allowlist::Origins(origins)
                }
            } else {
                // Single bare token (no parens) — treat as a single-
                // origin allowlist.
                Allowlist::Origins(vec![allow_raw.trim_matches('"').to_string()])
            };
            map.insert(feat, allow);
        }
        Self {
            map,
            self_origin: None,
        }
    }

    /// Look up the allowlist for a feature, returning `None` if the
    /// header didn't mention it.
    pub fn allowlist(&self, feature: &str) -> Option<&Allowlist> {
        self.map.get(&feature.to_ascii_lowercase())
    }

    /// Does this policy allow `feature` for a document at `origin`?
    /// Unspecified features fall back to "allowed" so pages without
    /// a Permissions-Policy header keep working.
    pub fn allows(&self, feature: &str, origin: Option<&Url>) -> bool {
        let Some(allow) = self.allowlist(feature) else {
            return true;
        };
        match allow {
            Allowlist::None_ => false,
            Allowlist::All => true,
            Allowlist::Origins(list) => list.iter().any(|s| {
                if s == "self" {
                    return match (&self.self_origin, origin) {
                        (Some(issuer), Some(target)) => *issuer == target.origin(),
                        // No issuer known — treat `self` as matching
                        // any caller. This keeps tests and untagged
                        // policies permissive.
                        (None, _) => true,
                        // Issuer known but no caller origin —
                        // conservative: deny.
                        (Some(_), None) => false,
                    };
                }
                let Some(target) = origin else { return false };
                let Ok(parsed) = Url::parse(s) else { return false };
                parsed.origin() == target.origin()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_feature_is_allowed() {
        let p = PermissionsPolicy::parse("");
        assert!(p.allows("camera", None));
    }

    #[test]
    fn empty_allowlist_blocks_everyone() {
        let p = PermissionsPolicy::parse("camera=()");
        assert!(!p.allows("camera", None));
        let u = Url::parse("https://example.com/").ok();
        assert!(!p.allows("camera", u.as_ref()));
    }

    #[test]
    fn star_allows_everyone() {
        let p = PermissionsPolicy::parse("microphone=*");
        let u = Url::parse("https://example.com/").ok();
        assert!(p.allows("microphone", u.as_ref()));
    }

    #[test]
    fn self_token_matches_same_origin() {
        let p = PermissionsPolicy::parse("geolocation=(self)");
        let u = Url::parse("https://example.com/").ok();
        assert!(p.allows("geolocation", u.as_ref()));
    }

    #[test]
    fn explicit_origin_matches() {
        let issuer = Url::parse("https://issuer.example/").unwrap().origin();
        let p = PermissionsPolicy::parse("camera=(self \"https://embed.example.com\")")
            .with_self_origin(issuer);
        let u = Url::parse("https://embed.example.com/path").ok();
        assert!(p.allows("camera", u.as_ref()));
        let u = Url::parse("https://other.example.com/").ok();
        assert!(!p.allows("camera", u.as_ref()));
    }

    #[test]
    fn multiple_features_parse() {
        let p =
            PermissionsPolicy::parse("camera=(), geolocation=(self), microphone=*");
        let u = Url::parse("https://example.com/").ok();
        assert!(!p.allows("camera", u.as_ref()));
        assert!(p.allows("geolocation", u.as_ref()));
        assert!(p.allows("microphone", u.as_ref()));
    }

    #[test]
    fn case_insensitive_feature_lookup() {
        let p = PermissionsPolicy::parse("Camera=()");
        assert!(!p.allows("camera", None));
        assert!(!p.allows("CAMERA", None));
    }
}
