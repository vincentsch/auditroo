//! Pull the SEO facts out of one fetched page.

use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::crawl::normalize_url;
use crate::crawl::{CrawlItem, RedirectHop, ResponseHeaders};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeoRecord {
    pub url: String,
    pub final_url: String,
    /// Media type without parameters, for example `text/html`.
    #[serde(default)]
    pub content_type: Option<String>,
    pub indexability: Indexability,
    pub canonical: Option<String>,
    pub title: Option<String>,
    pub meta_description: Option<String>,
    pub headings: Headings,
    pub structured_data: Vec<JsonLdBlock>,
    pub hreflang: Vec<HreflangAlternate>,
    pub http_status: u16,
    pub redirect_chain: Vec<RedirectHop>,
    pub rendered: bool,
}

/// The indexing rules that apply to Googlebot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Indexability {
    /// Directives from `<meta name="robots">`.
    pub robots_meta: Vec<String>,
    /// Directives from `<meta name="googlebot">`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub googlebot_meta: Vec<String>,
    /// Directives from X-Robots-Tag headers that apply to Googlebot: those
    /// without a crawler name and those for `googlebot`.
    pub x_robots_tag: Vec<String>,
    pub is_indexable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Headings {
    pub h1: Vec<String>,
    pub h2: Vec<String>,
    pub h3: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonLdBlock {
    pub raw: String,
    pub valid_json: bool,
    /// schema.org types of the block's top-level nodes, including the nodes
    /// in `@graph` and every entry of an `@type` list. Sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
    /// The JSON parser's message when the block is not valid JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HreflangAlternate {
    pub hreflang: String,
    pub href: String,
}

pub fn extract(item: &CrawlItem) -> SeoRecord {
    let response = &item.response;
    let content_type = response.content_type.clone();
    let x_robots_tag = extract_x_robots_tag(&response.headers);
    if !looks_like_html(content_type.as_deref(), &response.body) {
        // Images, PDFs and other files only matter for their status code.
        return SeoRecord {
            url: item.url.clone(),
            final_url: response.final_url.clone(),
            content_type,
            indexability: Indexability {
                robots_meta: Vec::new(),
                googlebot_meta: Vec::new(),
                is_indexable: is_indexable(response.status, &[], &x_robots_tag),
                x_robots_tag,
            },
            canonical: None,
            title: None,
            meta_description: None,
            headings: Headings::default(),
            structured_data: Vec::new(),
            hreflang: Vec::new(),
            http_status: response.status,
            redirect_chain: response.redirects.clone(),
            rendered: response.rendered,
        };
    }

    let document = Html::parse_document(&response.body);
    let parsed_final_url = Url::parse(&response.final_url).ok();
    let document_base = parsed_final_url
        .as_ref()
        .map(|final_url| select_document_base(&document, final_url));
    let robots_meta = extract_meta_directives(&document, "robots");
    let googlebot_meta = extract_meta_directives(&document, "googlebot");
    let meta: Vec<String> = robots_meta.iter().chain(&googlebot_meta).cloned().collect();
    let is_indexable = is_indexable(response.status, &meta, &x_robots_tag);

    SeoRecord {
        url: item.url.clone(),
        final_url: response.final_url.clone(),
        content_type,
        indexability: Indexability {
            robots_meta,
            googlebot_meta,
            x_robots_tag,
            is_indexable,
        },
        canonical: extract_canonical(&document, document_base.as_ref()),
        title: extract_title(&document),
        meta_description: extract_meta_description(&document),
        headings: extract_headings(&document),
        structured_data: extract_structured_data(&document),
        hreflang: extract_hreflang(&document, document_base.as_ref()),
        http_status: response.status,
        redirect_chain: response.redirects.clone(),
        rendered: response.rendered,
    }
}

/// HTML when the server says so, or when it sends no type and the body
/// starts like an HTML document.
pub(crate) fn looks_like_html(content_type: Option<&str>, body: &str) -> bool {
    match content_type {
        Some(media_type) => media_type == "text/html" || media_type == "application/xhtml+xml",
        None => {
            let trimmed =
                body.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
            let prefix = trimmed
                .chars()
                .take(32)
                .flat_map(char::to_lowercase)
                .collect::<String>();
            prefix.starts_with("<!doctype html") || prefix.starts_with("<html") || body.is_empty()
        }
    }
}

impl SeoRecord {
    /// An HTML page, as opposed to an image, PDF or other file.
    pub fn is_page(&self) -> bool {
        match self.content_type.as_deref() {
            Some(media_type) => media_type == "text/html" || media_type == "application/xhtml+xml",
            None => true,
        }
    }

    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.http_status)
    }

    pub fn is_error(&self) -> bool {
        self.http_status >= 400
    }

    /// The response came from another URL than the one requested, through a
    /// redirect or a client-side navigation.
    pub fn navigation_changed(&self) -> bool {
        navigation_changed(self)
    }

    pub fn has_noindex(&self) -> bool {
        self.indexability
            .robots_meta
            .iter()
            .chain(&self.indexability.googlebot_meta)
            .chain(&self.indexability.x_robots_tag)
            .any(|directive| is_noindex(directive))
    }

    /// An HTML page that loads directly with 2xx and has no noindex. This
    /// does not look at the canonical tag; see `Site::is_indexable`.
    pub fn is_directly_indexable(&self) -> bool {
        self.is_page() && self.indexability.is_indexable && !self.navigation_changed()
    }

    /// The redirect hops form a loop.
    pub fn redirect_loop(&self) -> bool {
        let mut seen = std::collections::BTreeSet::new();
        if let Ok(url) = Url::parse(&self.url) {
            seen.insert(normalize_url(&url));
        }
        self.redirect_chain.iter().any(|hop| {
            let target = Url::parse(&hop.to)
                .map(|url| normalize_url(&url))
                .unwrap_or_else(|_| hop.to.clone());
            !seen.insert(target)
        })
    }
}

pub fn navigation_changed(record: &SeoRecord) -> bool {
    if !record.redirect_chain.is_empty() {
        return true;
    }

    let Ok(requested_url) = Url::parse(&record.url) else {
        return true;
    };
    let Ok(final_url) = Url::parse(&record.final_url) else {
        return true;
    };

    normalize_url(&requested_url) != normalize_url(&final_url)
}

pub fn is_directly_indexable(record: &SeoRecord) -> bool {
    record.is_directly_indexable()
}

/// A page whose links count: an HTML page that loaded with 2xx, directly or
/// after redirects.
pub fn contributes_outbound_edges(record: &SeoRecord) -> bool {
    record.is_page() && record.is_ok()
}

/// `noindex`, or `none`, which means noindex and nofollow.
pub fn is_noindex(directive: &str) -> bool {
    directive == "noindex" || directive == "none"
}

fn extract_meta_directives(document: &Html, name: &str) -> Vec<String> {
    let meta_selector = Selector::parse("meta").expect("static meta selector parses");
    document
        .select(&meta_selector)
        .filter(|meta| attr_eq_ignore_ascii_case(meta, "name", name))
        .filter_map(|meta| meta.value().attr("content"))
        .flat_map(parse_directives)
        .collect()
}

/// Rules with a value after a colon. A colon after any other word names
/// the crawler the following rules are for.
const DIRECTIVES_WITH_VALUE: [&str; 4] = [
    "max-snippet",
    "max-image-preview",
    "max-video-preview",
    "unavailable_after",
];

/// X-Robots-Tag directives that apply to Googlebot. A header value may start
/// with a crawler name, as in `googlebot: noindex` or `otherbot: noindex,
/// nofollow`. The name applies to the rules after it until the next name.
fn extract_x_robots_tag(headers: &ResponseHeaders) -> Vec<String> {
    let mut directives = Vec::new();
    for value in headers.get_all_case_insensitive("x-robots-tag") {
        let mut for_google = true;
        for part in value.split(',').map(str::trim) {
            let mut directive = part;
            if let Some((name, rest)) = part.split_once(':') {
                let name = name.trim();
                let is_agent = !name.is_empty()
                    && name
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
                    && !DIRECTIVES_WITH_VALUE
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(name));
                if is_agent {
                    for_google = name.eq_ignore_ascii_case("googlebot");
                    directive = rest.trim();
                }
            }
            if for_google && !directive.is_empty() {
                directives.push(directive.to_ascii_lowercase());
            }
        }
    }
    directives
}

fn is_indexable(status: u16, meta: &[String], x_robots_tag: &[String]) -> bool {
    (200..300).contains(&status)
        && !meta.iter().any(|directive| is_noindex(directive))
        && !x_robots_tag.iter().any(|directive| is_noindex(directive))
}

fn extract_canonical(document: &Html, document_base: Option<&Url>) -> Option<String> {
    let link_selector = Selector::parse("link[href]").expect("static link selector parses");
    let canonical = document
        .select(&link_selector)
        .find(|link| rel_contains(link, "canonical"))?;
    let href = canonical.value().attr("href")?;

    resolve_url(document_base, href).map(|url| url.to_string())
}

fn extract_structured_data(document: &Html) -> Vec<JsonLdBlock> {
    let script_selector = Selector::parse("script[type]").expect("static script selector parses");
    document
        .select(&script_selector)
        .filter(|script| {
            attr_media_type_eq_ignore_ascii_case(script, "type", "application/ld+json")
        })
        .map(|script| {
            let raw = element_text(&script);
            let (valid_json, types, error) = match serde_json::from_str::<Value>(&raw) {
                Ok(value) => {
                    let mut types = Vec::new();
                    collect_json_ld_types(&value, &mut types);
                    types.sort();
                    types.dedup();
                    (true, types, None)
                }
                Err(error) => (false, Vec::new(), Some(error.to_string())),
            };

            JsonLdBlock {
                raw,
                valid_json,
                types,
                error,
            }
        })
        .collect()
}

/// Types of the top-level nodes: the block itself, the items of a top-level
/// list and the nodes in `@graph`. Nested values such as an author do not
/// count. auditroo does not validate the schema. The types only show which
/// rich results a page offers.
fn collect_json_ld_types(value: &Value, types: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_json_ld_types(item, types);
            }
        }
        Value::Object(object) => {
            match object.get("@type") {
                Some(Value::String(name)) => types.push(name.clone()),
                Some(Value::Array(names)) => {
                    types.extend(names.iter().filter_map(Value::as_str).map(str::to_owned))
                }
                _ => {}
            }
            if let Some(graph) = object.get("@graph") {
                collect_json_ld_types(graph, types);
            }
        }
        _ => {}
    }
}

fn extract_hreflang(document: &Html, document_base: Option<&Url>) -> Vec<HreflangAlternate> {
    let link_selector = Selector::parse("link[href]").expect("static link selector parses");
    document
        .select(&link_selector)
        .filter(|link| rel_contains(link, "alternate"))
        .filter_map(|link| {
            let hreflang = link.value().attr("hreflang")?.trim().to_ascii_lowercase();
            if hreflang.is_empty() {
                return None;
            }

            let href = link.value().attr("href")?;
            let href = resolve_url(document_base, href)?.to_string();

            Some(HreflangAlternate { hreflang, href })
        })
        .collect()
}

fn extract_title(document: &Html) -> Option<String> {
    let title_selector = Selector::parse("title").expect("static title selector parses");
    document
        .select(&title_selector)
        .next()
        .map(|title| collapse_whitespace(&element_text(&title)))
}

fn extract_meta_description(document: &Html) -> Option<String> {
    let meta_selector = Selector::parse("meta").expect("static meta selector parses");
    document
        .select(&meta_selector)
        .find(|meta| attr_eq_ignore_ascii_case(meta, "name", "description"))
        .and_then(|meta| meta.value().attr("content"))
        .map(|content| content.trim().to_owned())
}

fn extract_headings(document: &Html) -> Headings {
    Headings {
        h1: extract_heading_texts(document, "h1"),
        h2: extract_heading_texts(document, "h2"),
        h3: extract_heading_texts(document, "h3"),
    }
}

fn extract_heading_texts(document: &Html, selector: &str) -> Vec<String> {
    let selector = Selector::parse(selector).expect("static heading selector parses");
    document
        .select(&selector)
        .map(|heading| collapse_whitespace(&element_text(&heading)))
        .collect()
}

fn select_document_base(document: &Html, final_url: &Url) -> Url {
    let base_selector = Selector::parse("base[href]").expect("static base selector parses");
    document
        .select(&base_selector)
        .filter_map(|base| base.value().attr("href"))
        .filter_map(|href| final_url.join(href).ok())
        .find(is_http_url)
        .unwrap_or_else(|| final_url.clone())
}

fn resolve_url(document_base: Option<&Url>, href: &str) -> Option<Url> {
    document_base
        .and_then(|base| base.join(href).ok())
        .or_else(|| Url::parse(href).ok())
}

fn is_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
}

fn parse_directives(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
}

fn attr_eq_ignore_ascii_case(element: &ElementRef<'_>, name: &str, expected: &str) -> bool {
    element
        .value()
        .attr(name)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}

fn attr_media_type_eq_ignore_ascii_case(
    element: &ElementRef<'_>,
    name: &str,
    expected: &str,
) -> bool {
    element.value().attr(name).is_some_and(|value| {
        value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case(expected)
    })
}

fn rel_contains(element: &ElementRef<'_>, expected: &str) -> bool {
    element.value().attr("rel").is_some_and(|rel| {
        rel.split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case(expected))
    })
}

fn element_text(element: &ElementRef<'_>) -> String {
    element.text().collect::<String>()
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
