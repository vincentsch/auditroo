use url::Url;

use super::url::{normalize_url, path_and_query, ScopePattern};

/// Which URLs belong to the crawl.
#[derive(Debug, Clone)]
pub struct CrawlScope {
    start: Url,
    start_url: String,
    start_host: String,
    effective_port: Option<u16>,
    include: Vec<ScopePattern>,
    exclude: Vec<ScopePattern>,
    max_depth: usize,
    max_pages: usize,
    /// Hosts (without `www.`) and ports of the production sites this crawl
    /// stands in for, when it crawls a preview.
    production: Vec<(String, Option<u16>)>,
}

impl CrawlScope {
    /// `start_url` must be an http or https URL. The caller checks that.
    pub fn new(
        start_url: &Url,
        include: Vec<ScopePattern>,
        exclude: Vec<ScopePattern>,
        max_depth: usize,
        max_pages: usize,
    ) -> Self {
        Self {
            start: start_url.clone(),
            start_url: normalize_url(start_url),
            start_host: start_url
                .host_str()
                .unwrap_or_default()
                .to_ascii_lowercase(),
            effective_port: start_url.port(),
            include,
            exclude,
            max_depth,
            max_pages,
            production: Vec::new(),
        }
    }

    /// Treat URLs on `production` as this site's own URLs. A preview build
    /// usually names production, for example in canonical tags and the
    /// sitemap. Those URLs are read on the crawled host under the same path.
    pub fn with_production(mut self, production: &Url) -> Self {
        if let Some(host) = production.host_str() {
            self.production.push((
                strip_www(&host.to_ascii_lowercase()).to_owned(),
                production.port(),
            ));
        }
        self
    }

    /// For a production URL, the same page on the crawled host. Any other
    /// URL comes back unchanged.
    pub fn localize(&self, url: Url) -> Url {
        if self.is_same_site(&url) || !matches!(url.scheme(), "http" | "https") {
            return url;
        }
        let Some(candidate) = url.host_str().map(str::to_ascii_lowercase) else {
            return url;
        };
        let matches = self
            .production
            .iter()
            .any(|(host, port)| strip_www(&candidate) == host && url.port() == *port);
        if !matches {
            return url;
        }
        let mut local = self.start.clone();
        local.set_path(url.path());
        local.set_query(url.query());
        local.set_fragment(None);
        local
    }

    /// Whether the path and query pass --include and --exclude.
    pub fn allows_path(&self, url: &Url) -> bool {
        self.path_filters_ok(url)
    }

    pub fn allows_candidate(&self, url: &Url, depth: usize) -> bool {
        if !self.scheme_host_port_ok(url) {
            return false;
        }
        let is_start = normalize_url(url) == self.start_url;
        if !is_start && !self.path_filters_ok(url) {
            return false;
        }
        self.depth_ok(depth)
    }

    pub fn allows_discovered_link(&self, url: &Url) -> bool {
        self.scheme_host_port_ok(url) && self.path_filters_ok(url)
    }

    /// Same host (or its `www` twin) and port as the start URL; the scheme
    /// may differ.
    pub fn is_same_site(&self, url: &Url) -> bool {
        self.scheme_host_port_ok(url)
    }

    pub fn start_url(&self) -> &str {
        &self.start_url
    }

    pub fn max_pages(&self) -> usize {
        self.max_pages
    }

    pub fn max_depth(&self) -> usize {
        self.max_depth
    }

    fn scheme_host_port_ok(&self, url: &Url) -> bool {
        if !matches!(url.scheme(), "http" | "https") {
            return false;
        }

        let host = match url.host_str() {
            Some(host) => host.to_ascii_lowercase(),
            None => return false,
        };

        if strip_www(&host) != strip_www(&self.start_host) {
            return false;
        }

        url.port() == self.effective_port
    }

    fn path_filters_ok(&self, url: &Url) -> bool {
        let target = path_and_query(url);
        if self.exclude.iter().any(|pattern| pattern.matches(&target)) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|pattern| pattern.matches(&target))
    }

    fn depth_ok(&self, depth: usize) -> bool {
        self.max_depth == 0 || depth <= self.max_depth
    }
}

fn strip_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL parses")
    }

    fn scope(start_url: &str, include: &[&str], exclude: &[&str], max_depth: usize) -> CrawlScope {
        CrawlScope::new(
            &url(start_url),
            include
                .iter()
                .map(|pattern| ScopePattern::compile(pattern))
                .collect(),
            exclude
                .iter()
                .map(|pattern| ScopePattern::compile(pattern))
                .collect(),
            max_depth,
            25,
        )
    }

    #[test]
    fn accepts_same_host_and_www_twin_but_rejects_other_subdomains() {
        let apex = scope("https://example.com/", &[], &[], 0);
        assert!(apex.allows_candidate(&url("https://example.com/path"), 1));
        assert!(apex.allows_candidate(&url("https://www.example.com/path"), 1));
        assert!(!apex.allows_candidate(&url("https://shop.example.com/path"), 1));

        let www = scope("https://www.example.com/", &[], &[], 0);
        assert!(www.allows_candidate(&url("https://example.com/path"), 1));
        assert!(www.allows_candidate(&url("https://www.example.com/path"), 1));
        assert!(!www.allows_candidate(&url("https://shop.example.com/path"), 1));
    }

    #[test]
    fn accepts_http_https_same_site_when_effective_port_matches() {
        let default_port = scope("http://example.com/", &[], &[], 0);
        assert!(default_port.allows_candidate(&url("https://example.com/secure"), 1));

        let custom_port = scope("https://example.com:8443/", &[], &[], 0);
        assert!(custom_port.allows_candidate(&url("http://example.com:8443/path"), 1));
        assert!(!custom_port.allows_candidate(&url("https://example.com:9443/path"), 1));
        assert!(!custom_port.allows_candidate(&url("https://example.com/path"), 1));
    }

    #[test]
    fn rejects_non_http_and_hostless_urls() {
        let scope = scope("https://example.com/", &[], &[], 0);
        for input in [
            "ftp://example.com/path",
            "mailto:hello@example.com",
            "javascript:void(0)",
            "data:text/plain,hello",
        ] {
            let parsed = url(input);
            assert!(!scope.allows_candidate(&parsed, 1));
            assert!(!scope.allows_discovered_link(&parsed));
        }
    }

    #[test]
    fn applies_include_and_exclude_filters_to_path_and_query() {
        let scope = scope(
            "https://example.com/",
            &["/products/*"],
            &["/products/private/*", "*?sort=*"],
            0,
        );

        assert!(scope.allows_candidate(&url("https://example.com/products/shoes"), 1));
        assert!(scope.allows_candidate(&url("https://example.com/products/a/b/c"), 1));
        assert!(!scope.allows_candidate(&url("https://example.com/blog"), 1));
        assert!(!scope.allows_candidate(&url("https://example.com/products/private/a"), 1));
        assert!(!scope.allows_candidate(
            &url("https://example.com/products/shoes?sort=price&brand=a"),
            1
        ));
    }

    #[test]
    fn exclude_wins_over_include() {
        let scope = scope("https://example.com/", &["/docs/*"], &["/docs/private"], 0);
        assert!(!scope.allows_candidate(&url("https://example.com/docs/private"), 1));
    }

    #[test]
    fn depth_boundary_and_unlimited_depth_are_supported() {
        let limited = scope("https://example.com/", &[], &[], 2);
        assert!(limited.allows_candidate(&url("https://example.com/a"), 2));
        assert!(!limited.allows_candidate(&url("https://example.com/a"), 3));

        let unlimited = scope("https://example.com/", &[], &[], 0);
        assert!(unlimited.allows_candidate(&url("https://example.com/a"), 500));
    }

    #[test]
    fn start_candidate_bypasses_filters_but_discovered_link_does_not() {
        let scope = scope("https://example.com/", &["/products/*"], &[], 0);
        let start = url("https://example.com/");

        assert!(scope.allows_candidate(&start, 0));
        assert!(!scope.allows_discovered_link(&start));
    }

    #[test]
    fn production_urls_map_to_the_crawled_host() {
        let scope = scope("http://127.0.0.1:4321/", &[], &[], 0)
            .with_production(&url("https://www.example.com/"));
        assert_eq!(
            scope
                .localize(url("https://www.example.com/docs/?a=1#top"))
                .as_str(),
            "http://127.0.0.1:4321/docs/?a=1"
        );
        assert_eq!(
            scope.localize(url("https://example.com/docs/")).as_str(),
            "http://127.0.0.1:4321/docs/"
        );
        assert_eq!(
            scope
                .localize(url("https://staging.example.com/docs/"))
                .as_str(),
            "https://staging.example.com/docs/"
        );
        assert_eq!(
            scope
                .localize(url("https://www.example.com:8443/docs/"))
                .as_str(),
            "https://www.example.com:8443/docs/"
        );
        let plain = scope_without_production();
        assert_eq!(
            plain
                .localize(url("https://www.example.com/docs/"))
                .as_str(),
            "https://www.example.com/docs/"
        );
    }

    fn scope_without_production() -> CrawlScope {
        scope("http://127.0.0.1:4321/", &[], &[], 0)
    }

    #[test]
    fn accessors_return_configured_values() {
        let scope = CrawlScope::new(
            &url("https://example.com/root#frag"),
            Vec::new(),
            Vec::new(),
            7,
            0,
        );

        assert_eq!(scope.start_url(), "https://example.com/root");
        assert_eq!(scope.max_depth(), 7);
        assert_eq!(scope.max_pages(), 0);
    }
}
