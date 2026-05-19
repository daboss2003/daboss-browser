//! First-Party Sets — a small curated table of host → primary
//! mappings.
//!
//! A First-Party Set declares that a group of domains is owned by
//! the same entity and should be treated as a single party for
//! storage / cookie purposes. The browser collapses storage
//! partitions when both the top-level and inner origin belong to
//! the same set, and rejects cross-set Partitioned cookies as
//! third-party.
//!
//! Chrome ships its FPS list as a JSON file fetched from the
//! `chromium/related_website_sets` repo. For the toy we keep a
//! handful of well-known sets inline so we have something to test
//! the integration against. A future session can swap in the live
//! list fetch.

/// Return the canonical "primary" host for `host` if it belongs to
/// a First-Party Set, else `None`. A host that maps to a primary
/// shares storage with every other host that maps to the same
/// primary.
pub fn primary_for(host: &str) -> Option<&'static str> {
    // Normalise: strip the leading `www.` (case-insensitively) so
    // `www.example`, `WWW.example`, and `example` resolve into the
    // same set.
    let needle = strip_www(host);
    for (member, primary) in SETS {
        if member.eq_ignore_ascii_case(needle) {
            return Some(primary);
        }
    }
    None
}

fn strip_www(host: &str) -> &str {
    if host.len() >= 4 && host[..4].eq_ignore_ascii_case("www.") {
        &host[4..]
    } else {
        host
    }
}

/// Two hosts belong to the same FPS iff they share a primary OR
/// both are unmapped + identical. The latter case fast-paths the
/// common single-domain comparison.
pub fn same_party(a: &str, b: &str) -> bool {
    let a = strip_www(a);
    let b = strip_www(b);
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    match (primary_for(a), primary_for(b)) {
        (Some(pa), Some(pb)) => pa.eq_ignore_ascii_case(pb),
        _ => false,
    }
}

/// `(member, primary)` rows — every member of an FPS is listed
/// explicitly including the primary itself (so `primary_for("x.com")`
/// returns `Some("x.com")`).
///
/// To extend this list, drop a new row in. Hostnames are lower-case
/// punycoded; comparisons ignore case + strip `www.`.
const SETS: &[(&str, &str)] = &[
    // Google's classic example — google.com is the primary and
    // youtube.com / blogger.com / android.com are associated sites.
    ("google.com", "google.com"),
    ("youtube.com", "google.com"),
    ("blogger.com", "google.com"),
    ("android.com", "google.com"),
    // Microsoft Account ecosystem.
    ("microsoft.com", "microsoft.com"),
    ("live.com", "microsoft.com"),
    ("office.com", "microsoft.com"),
    ("outlook.com", "microsoft.com"),
    // GitHub / GitHub Pages.
    ("github.com", "github.com"),
    ("github.io", "github.com"),
    // The Wikimedia Foundation properties.
    ("wikipedia.org", "wikipedia.org"),
    ("wikimedia.org", "wikipedia.org"),
    ("wiktionary.org", "wikipedia.org"),
    // A toy set we use in tests so we don't need a real-world
    // domain to exercise the FPS code path.
    ("daboss-test-a.example", "daboss-test-a.example"),
    ("daboss-test-b.example", "daboss-test-a.example"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_returns_self_for_primary_member() {
        assert_eq!(primary_for("google.com"), Some("google.com"));
    }

    #[test]
    fn primary_collapses_associated_sites_to_primary() {
        assert_eq!(primary_for("youtube.com"), Some("google.com"));
        assert_eq!(primary_for("blogger.com"), Some("google.com"));
    }

    #[test]
    fn primary_ignores_leading_www_and_case() {
        assert_eq!(primary_for("WWW.YouTube.com"), Some("google.com"));
    }

    #[test]
    fn primary_returns_none_for_unmapped_hosts() {
        assert_eq!(primary_for("example.com"), None);
        assert_eq!(primary_for("totally-unrelated.test"), None);
    }

    #[test]
    fn same_party_matches_within_set_and_rejects_outside() {
        assert!(same_party("youtube.com", "blogger.com"));
        assert!(same_party("github.io", "github.com"));
        assert!(!same_party("github.com", "google.com"));
        assert!(!same_party("github.com", "example.com"));
        // Same domain that isn't in any set still matches itself.
        assert!(same_party("example.com", "example.com"));
    }
}
