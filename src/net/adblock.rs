//! Hostname-based ad blocking (Phase 9 first cut).
//!
//! This is the **simplest** layer of EasyList — a flat set of hostnames
//! that, when requested, should resolve to an empty response. We don't
//! parse full ABP syntax (`||domain^`, element-hiding rules, exception
//! filters), and we don't fetch the live EasyList; the bundled defaults
//! ship inside the binary so the blocker works offline. A future
//! sub-phase will swap in the real filterlist.
//!
//! Matching: the request's host matches a blocklist entry when the host
//! equals the entry or has it as a dot-bounded suffix. So an entry of
//! `doubleclick.net` blocks `doubleclick.net`, `ads.doubleclick.net`,
//! `cm.g.doubleclick.net`, etc.
//!
//! Wired in at [`super::Client::do_request`] so every navigation,
//! external stylesheet fetch, image prefetch, iframe load, and `fetch()`
//! call goes through the same gate.

use std::collections::HashSet;

use url::Url;

pub struct Blocklist {
    hosts: HashSet<String>,
    enabled: bool,
}

impl Blocklist {
    /// Built-in minimal blocklist. Real EasyList is ~95k rules; this
    /// trims to a few obvious offenders that show up on the kinds of
    /// pages the toy browser typically loads. The intent is "the blocker
    /// is on and demonstrably doing something", not "this is a faithful
    /// EasyList implementation."
    pub fn default_bundled() -> Self {
        let entries = [
            // Google ads / analytics
            "doubleclick.net",
            "googleadservices.com",
            "googlesyndication.com",
            "google-analytics.com",
            "googletagmanager.com",
            "googletagservices.com",
            "adservice.google.com",
            // Facebook tracking
            "connect.facebook.net",
            "facebook.com/tr",
            // Amazon ads
            "amazon-adsystem.com",
            // Generic ad networks
            "criteo.com",
            "criteo.net",
            "outbrain.com",
            "taboola.com",
            "adnxs.com",
            "rubiconproject.com",
            "pubmatic.com",
            "openx.net",
            "casalemedia.com",
            "bidswitch.net",
            "advertising.com",
            "moatads.com",
            "scorecardresearch.com",
            // Generic analytics
            "quantserve.com",
            "hotjar.com",
            "mixpanel.com",
            "segment.com",
            "segment.io",
            "amplitude.com",
            "fullstory.com",
            "newrelic.com",
            "nr-data.net",
            "branch.io",
        ];
        let hosts: HashSet<String> = entries.iter().map(|s| s.to_ascii_lowercase()).collect();
        Self {
            hosts,
            enabled: true,
        }
    }

    #[allow(dead_code)] // exposed for future user-toggle / tests
    pub fn disabled() -> Self {
        Self {
            hosts: HashSet::new(),
            enabled: false,
        }
    }

    pub fn is_blocked(&self, url: &Url) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        // Exact match.
        if self.hosts.contains(&host) {
            return true;
        }
        // Dot-bounded suffix match: split off subdomain labels and check
        // progressively shorter suffixes. `a.b.c.example.com` checks
        // `b.c.example.com`, `c.example.com`, `example.com`.
        let mut suffix = host.as_str();
        while let Some(idx) = suffix.find('.') {
            suffix = &suffix[idx + 1..];
            if suffix.is_empty() {
                break;
            }
            if self.hosts.contains(suffix) {
                return true;
            }
        }
        false
    }
}

impl Default for Blocklist {
    fn default() -> Self {
        Self::default_bundled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn exact_host_is_blocked() {
        let bl = Blocklist::default_bundled();
        assert!(bl.is_blocked(&url("https://doubleclick.net/")));
    }

    #[test]
    fn subdomain_is_blocked_via_suffix_match() {
        let bl = Blocklist::default_bundled();
        assert!(bl.is_blocked(&url("https://ads.doubleclick.net/path")));
        assert!(bl.is_blocked(&url("https://cm.g.doubleclick.net/")));
    }

    #[test]
    fn unrelated_host_passes() {
        let bl = Blocklist::default_bundled();
        assert!(!bl.is_blocked(&url("https://example.com/")));
        // Boundary check: should NOT block evil-doubleclick.net which
        // ends with the substring but isn't a subdomain.
        assert!(!bl.is_blocked(&url("https://evil-doubleclick.net/")));
    }

    #[test]
    fn disabled_blocklist_passes_everything() {
        let bl = Blocklist::disabled();
        assert!(!bl.is_blocked(&url("https://doubleclick.net/")));
    }
}
