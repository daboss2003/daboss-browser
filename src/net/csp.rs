//! Tiny `Content-Security-Policy` parser. We only enforce `script-src`
//! (and its fallback `default-src`) for inline-script gating — enough
//! to honour the most common deployment of CSP without pretending to
//! cover the full directive set.
//!
//! `'unsafe-inline'` is the only source expression that permits an
//! inline `<script>` to run. Anything else (specific hostnames,
//! `'self'`, hashes, nonces) implicitly forbids inline scripts.

#[derive(Debug, Clone)]
pub struct Csp {
    /// Source expressions tied to `script-src` (or the fallback
    /// `default-src` when `script-src` wasn't given). Lower-cased.
    pub script_src: Vec<String>,
    /// `true` if no `script-src` directive was present.
    pub script_src_missing: bool,
    /// `true` when the policy contains
    /// `require-trusted-types-for 'script'`. Forces innerHTML /
    /// outerHTML / similar DOM-XSS sinks to consume Trusted Types
    /// objects rather than raw strings.
    pub require_trusted_types_for_script: bool,
}

impl Default for Csp {
    /// The default policy is "no policy" — inline scripts run, network
    /// requests are unrestricted. The browser shell uses this when the
    /// page response carries no `Content-Security-Policy` header.
    fn default() -> Self {
        Self {
            script_src: Vec::new(),
            script_src_missing: true,
            require_trusted_types_for_script: false,
        }
    }
}

impl Csp {
    /// Parse a `Content-Security-Policy` header value.
    pub fn parse(header: &str) -> Self {
        let mut script_src: Option<Vec<String>> = None;
        let mut default_src: Option<Vec<String>> = None;
        let mut require_tt = false;
        for directive in header.split(';') {
            let parts: Vec<&str> = directive.split_ascii_whitespace().collect();
            if parts.is_empty() {
                continue;
            }
            let name = parts[0].to_ascii_lowercase();
            let sources: Vec<String> =
                parts[1..].iter().map(|s| s.to_ascii_lowercase()).collect();
            match name.as_str() {
                "script-src" => script_src = Some(sources),
                "default-src" => default_src = Some(sources),
                "require-trusted-types-for" => {
                    // Spec lists 'script' as the only currently
                    // defined value.
                    if sources.iter().any(|s| s == "'script'") {
                        require_tt = true;
                    }
                }
                _ => {}
            }
        }
        match script_src {
            Some(s) => Csp {
                script_src: s,
                script_src_missing: false,
                require_trusted_types_for_script: require_tt,
            },
            None => Csp {
                script_src: default_src.unwrap_or_default(),
                script_src_missing: true,
                require_trusted_types_for_script: require_tt,
            },
        }
    }

    /// `true` when inline `<script>` content is permitted by this
    /// policy. If no policy was set (i.e. `Csp::default()`), inline
    /// scripts run — CSP is opt-in.
    pub fn allows_inline_scripts(&self) -> bool {
        // A policy with neither `script-src` nor `default-src` (the
        // result of an empty header) is equivalent to "no policy",
        // which permits everything.
        if self.script_src_missing && self.script_src.is_empty() {
            return true;
        }
        self.script_src.iter().any(|s| s == "'unsafe-inline'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_csp_allows_inline() {
        let c = Csp::default();
        assert!(c.allows_inline_scripts());
    }

    #[test]
    fn script_src_self_blocks_inline() {
        let c = Csp::parse("script-src 'self' https://example.com");
        assert!(!c.allows_inline_scripts());
    }

    #[test]
    fn script_src_unsafe_inline_allows() {
        let c = Csp::parse("script-src 'self' 'unsafe-inline'");
        assert!(c.allows_inline_scripts());
    }

    #[test]
    fn default_src_used_when_script_src_missing() {
        let c = Csp::parse("default-src 'self'");
        assert!(!c.allows_inline_scripts());
    }

    #[test]
    fn default_src_unsafe_inline_propagates() {
        let c = Csp::parse("default-src 'unsafe-inline'");
        assert!(c.allows_inline_scripts());
    }

    #[test]
    fn require_trusted_types_for_script_parses() {
        let c = Csp::parse("require-trusted-types-for 'script'");
        assert!(c.require_trusted_types_for_script);
        let c = Csp::parse("default-src 'self'");
        assert!(!c.require_trusted_types_for_script);
    }
}
