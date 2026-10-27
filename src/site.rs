//! Which hosts belong to the site, and how to compare URLs across hosts.
//!
//! A preview build on `http://127.0.0.1:8080/` and production on
//! `https://www.example.com/` are the same site. Their pages are compared by
//! path, so `/docs/` on one host matches `/docs/` on the other. Besides the
//! path, the key of a URL says whether it uses the main host or its `www`
//! twin, and whether it uses the main scheme. So `http://example.com/a` and
//! `https://www.example.com/a` get different keys, because one of them
//! usually redirects to the other.

use url::Url;

use crate::crawl::normalize_url;
use crate::crawl::url::path_and_query;
use crate::extract::SeoRecord;

#[derive(Debug, Clone)]
struct Origin {
    host: String,
    bare_host: String,
    scheme: String,
    port: Option<u16>,
}

impl Origin {
    fn parse(url: &str) -> Option<Self> {
        let url = Url::parse(url).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        Some(Self {
            bare_host: strip_www(&host).to_owned(),
            host,
            scheme: url.scheme().to_owned(),
            port: url.port(),
        })
    }

    fn key(&self, url: &Url) -> Option<String> {
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        if strip_www(&host) != self.bare_host || url.port() != self.port {
            return None;
        }
        let host_role = if host == self.host { "main" } else { "twin" };
        let scheme_role = if url.scheme() == self.scheme {
            "main"
        } else {
            "other"
        };
        Some(format!(
            "site:{host_role}:{scheme_role}:{}",
            path_and_query(url)
        ))
    }
}

/// The set of origins that count as one site.
///
/// The first origin is the one that was crawled. Others are aliases that the
/// user named: the production site a preview stands in for, or the baseline's
/// host when two crawls are compared. Hosts that only appear in the site's
/// own content, such as a sitemap, are never trusted this way.
#[derive(Debug, Clone)]
pub struct Site {
    origins: Vec<Origin>,
}

impl Site {
    pub fn new<'a>(start_url: &str, aliases: impl IntoIterator<Item = &'a String>) -> Self {
        let mut origins = Vec::new();
        origins.extend(Origin::parse(start_url));
        origins.extend(aliases.into_iter().filter_map(|alias| Origin::parse(alias)));
        Self { origins }
    }

    /// Add another origin that belongs to the same site.
    pub fn with_alias(mut self, url: &str) -> Self {
        self.origins.extend(Origin::parse(url));
        self
    }

    /// A key that is the same for one page on any of the site's hosts. For a
    /// URL on another host, the key is the full URL.
    pub fn key(&self, raw_url: &str) -> String {
        match Url::parse(raw_url) {
            Ok(url) => self
                .origins
                .iter()
                .find_map(|origin| origin.key(&url))
                .unwrap_or_else(|| normalize_url(&url)),
            Err(_) => raw_url.to_owned(),
        }
    }

    pub fn contains(&self, raw_url: &str) -> bool {
        Url::parse(raw_url)
            .ok()
            .is_some_and(|url| self.origins.iter().any(|origin| origin.key(&url).is_some()))
    }

    /// A short form for people: the path for URLs on the site's main host
    /// and scheme, the full URL for everything else.
    pub fn display(&self, raw_url: &str) -> String {
        let Ok(url) = Url::parse(raw_url) else {
            return raw_url.to_owned();
        };
        let main = self
            .origins
            .iter()
            .filter_map(|origin| origin.key(&url))
            .any(|key| key.starts_with("site:main:main:"));
        if main {
            path_and_query(&url)
        } else {
            normalize_url(&url)
        }
    }

    /// Host of a URL when it is not part of the site, with the port when
    /// the URL names one.
    pub fn foreign_host(&self, raw_url: &str) -> Option<String> {
        if self.contains(raw_url) {
            return None;
        }
        let url = Url::parse(raw_url).ok()?;
        let host = url.host_str()?;
        Some(match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        })
    }

    /// Whether the canonical tag points at the page itself (or is missing).
    pub fn canonical_is_self(&self, record: &SeoRecord) -> bool {
        match &record.canonical {
            None => true,
            Some(canonical) => self.key(canonical) == self.key(&record.url),
        }
    }

    /// A page Google could index under its own URL: an HTML page that loads
    /// directly with 2xx, has no noindex, and does not name another URL as
    /// canonical.
    pub fn is_indexable(&self, record: &SeoRecord) -> bool {
        record.is_directly_indexable() && self.canonical_is_self(record)
    }
}

fn strip_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_matches_across_production_and_preview_hosts() {
        let site = Site::new(
            "http://127.0.0.1:8080/",
            &["https://www.example.com".to_owned()],
        );
        assert_eq!(
            site.key("http://127.0.0.1:8080/docs/"),
            site.key("https://www.example.com/docs/")
        );
        assert_ne!(
            site.key("http://127.0.0.1:8080/docs/"),
            site.key("https://example.com/docs/")
        );
        assert_eq!(
            site.key("https://staging.example.com/docs/"),
            "https://staging.example.com/docs/"
        );
        assert!(site.contains("https://example.com/docs/"));
        assert!(!site.contains("https://staging.example.com/docs/"));
        assert_eq!(
            site.foreign_host("https://staging.example.com/x"),
            Some("staging.example.com".to_owned())
        );
    }

    #[test]
    fn display_shortens_main_host_urls_only() {
        let site = Site::new("https://www.example.com/", std::iter::empty());
        assert_eq!(site.display("https://www.example.com/a?b=1"), "/a?b=1");
        assert_eq!(
            site.display("https://example.com/a"),
            "https://example.com/a"
        );
        assert_eq!(site.display("https://other.test/a"), "https://other.test/a");
    }
}
