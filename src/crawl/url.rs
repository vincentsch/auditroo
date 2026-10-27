use std::fmt::Write as _;

use url::Url;

/// The URL as auditroo stores it. It has no fragment and no user name or
/// password, so two links to the same page give the same string and the
/// crawl fetches the page once.
pub fn normalize_url(url: &Url) -> String {
    if matches!(url.scheme(), "http" | "https") {
        if let Some(host) = url.host_str() {
            let mut out = String::with_capacity(url.as_str().len());
            out.push_str(url.scheme());
            out.push_str("://");
            out.push_str(host);
            if let Some(port) = url.port() {
                let _ = write!(out, ":{port}");
            }
            let path = url.path();
            out.push_str(if path.is_empty() { "/" } else { path });
            if let Some(query) = url.query() {
                out.push('?');
                out.push_str(query);
            }
            return out;
        }
    }

    let mut without_fragment = url.clone();
    without_fragment.set_fragment(None);
    without_fragment.to_string()
}

pub(crate) fn path_and_query(url: &Url) -> String {
    match url.query() {
        Some(query) => {
            let mut out = String::with_capacity(url.path().len() + 1 + query.len());
            out.push_str(url.path());
            out.push('?');
            out.push_str(query);
            out
        }
        None => url.path().to_owned(),
    }
}

/// An --include or --exclude pattern. Only `*` is special: it stands for
/// any text, including `/`.
#[derive(Debug, Clone)]
pub struct ScopePattern {
    original: String,
    segments: Vec<String>,
}

impl ScopePattern {
    /// Split the pattern at each `*`. Any text is a valid pattern.
    pub fn compile(pattern: &str) -> Self {
        Self {
            original: pattern.to_owned(),
            segments: pattern.split('*').map(str::to_owned).collect(),
        }
    }

    /// Whether the pattern matches a path with its query, such as
    /// `/search?q=shoes`.
    pub fn matches(&self, path_and_query: &str) -> bool {
        if self.segments.len() == 1 {
            return self.segments[0] == path_and_query;
        }

        let first = self.segments[0].as_str();
        if !path_and_query.starts_with(first) {
            return false;
        }

        let mut pos = first.len();
        let last_idx = self.segments.len() - 1;
        for segment in &self.segments[1..last_idx] {
            if segment.is_empty() {
                continue;
            }
            match path_and_query[pos..].find(segment.as_str()) {
                Some(index) => pos += index + segment.len(),
                None => return false,
            }
        }

        path_and_query[pos..].ends_with(self.segments[last_idx].as_str())
    }

    /// The pattern as the user wrote it.
    pub fn as_str(&self) -> &str {
        &self.original
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL parses")
    }

    #[test]
    fn normalize_http_url_identity_components() {
        assert_eq!(
            normalize_url(&url("HTTPS://EXAMPLE.com:443/path?B=2&a=1&a=&B=3#frag")),
            "https://example.com/path?B=2&a=1&a=&B=3"
        );
        assert_eq!(
            normalize_url(&url("http://EXAMPLE.com:80")),
            "http://example.com/"
        );
        assert_eq!(
            normalize_url(&url("https://example.com:8443/path")),
            "https://example.com:8443/path"
        );
        assert_eq!(
            normalize_url(&url("https://example.com/path/")),
            "https://example.com/path/"
        );
        assert_eq!(
            normalize_url(&url("https://[::1]:8443/a")),
            "https://[::1]:8443/a"
        );
        assert_eq!(
            normalize_url(&url("https://user:pass@example.com/secret")),
            "https://example.com/secret"
        );
    }

    #[test]
    fn normalize_preserves_www_as_identity_and_drops_fragments() {
        assert_eq!(
            normalize_url(&url("https://example.com/#one")),
            "https://example.com/"
        );
        assert_eq!(
            normalize_url(&url("https://www.example.com/#one")),
            "https://www.example.com/"
        );
    }

    #[test]
    fn normalize_preserves_parsed_path_and_query_serialization() {
        assert_eq!(
            normalize_url(&url("https://example.com/a%2Fb/%7E?q=a%2Fb&blank=&q=two")),
            "https://example.com/a%2Fb/%7E?q=a%2Fb&blank=&q=two"
        );
    }

    #[test]
    fn normalize_is_total_for_non_crawlable_urls() {
        for input in [
            "mailto:hello@example.com#frag",
            "tel:+123#frag",
            "javascript:void(0)#frag",
            "data:text/plain,hello#frag",
        ] {
            let normalized = normalize_url(&url(input));
            assert!(!normalized.contains("#frag"));
        }
    }

    #[test]
    fn path_and_query_uses_same_serialization() {
        assert_eq!(
            path_and_query(&url("https://example.com/a?b=1&b=2")),
            "/a?b=1&b=2"
        );
        assert_eq!(path_and_query(&url("https://example.com/a#frag")), "/a");
    }

    #[test]
    fn scope_pattern_supports_exact_and_star_matching() {
        assert!(ScopePattern::compile("/products").matches("/products"));
        assert!(!ScopePattern::compile("/products").matches("/products/"));
        assert!(ScopePattern::compile("/products/*/detail").matches("/products/shoes/blue/detail"));
        assert!(ScopePattern::compile("*?sort=*").matches("/products/shoes?sort=price&brand=a"));
        assert!(!ScopePattern::compile("*?sort=*").matches("/products/shoes?brand=a"));
        assert!(ScopePattern::compile("*").matches("/anything?including=query"));
        assert!(ScopePattern::compile("/a**c").matches("/abc"));
        assert!(ScopePattern::compile("*tail").matches("/head/tail"));
        assert!(ScopePattern::compile("/head*").matches("/head/tail"));
    }

    #[test]
    fn scope_pattern_preserves_original_text() {
        let pattern = ScopePattern::compile("*?sort=*");
        assert_eq!(pattern.as_str(), "*?sort=*");
    }
}
