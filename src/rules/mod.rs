//! The built-in rules: `check` looks at one crawl, `diff` compares two.

pub mod check;
pub mod diff;

use std::collections::BTreeMap;

use crate::extract::{is_noindex, SeoRecord};
use crate::findings::Severity;
use crate::site::Site;
use crate::snapshot::Snapshot;

/// A crawler whose access auditroo checks in robots.txt.
#[derive(Debug, Clone, Copy)]
pub struct Crawler {
    /// The user-agent token robots.txt groups use.
    pub token: &'static str,
    /// Name shown in findings.
    pub name: &'static str,
    /// Severity when robots.txt newly blocks this crawler from pages.
    pub severity: Severity,
}

/// Search engines first, then AI search and assistant crawlers, then AI
/// training crawlers. Sites often block training crawlers on purpose, so a
/// change there is reported but does not fail a build by default.
pub const CRAWLERS: &[Crawler] = &[
    Crawler {
        token: "Googlebot",
        name: "Googlebot",
        severity: Severity::Critical,
    },
    Crawler {
        token: "Bingbot",
        name: "Bingbot",
        severity: Severity::High,
    },
    Crawler {
        token: "OAI-SearchBot",
        name: "OAI-SearchBot (ChatGPT search)",
        severity: Severity::Medium,
    },
    Crawler {
        token: "ChatGPT-User",
        name: "ChatGPT-User",
        severity: Severity::Medium,
    },
    Crawler {
        token: "PerplexityBot",
        name: "PerplexityBot",
        severity: Severity::Medium,
    },
    Crawler {
        token: "Claude-SearchBot",
        name: "Claude-SearchBot",
        severity: Severity::Medium,
    },
    Crawler {
        token: "GPTBot",
        name: "GPTBot (OpenAI training)",
        severity: Severity::Low,
    },
    Crawler {
        token: "ClaudeBot",
        name: "ClaudeBot (Anthropic training)",
        severity: Severity::Low,
    },
    Crawler {
        token: "Google-Extended",
        name: "Google-Extended (Gemini training)",
        severity: Severity::Low,
    },
    Crawler {
        token: "CCBot",
        name: "CCBot (Common Crawl)",
        severity: Severity::Low,
    },
];

/// Records of a snapshot keyed by their site key.
pub(crate) fn records_by_key<'a>(
    snapshot: &'a Snapshot,
    site: &Site,
) -> BTreeMap<String, &'a SeoRecord> {
    let mut map = BTreeMap::new();
    for record in snapshot.records.values() {
        map.entry(site.key(&record.url)).or_insert(record);
    }
    map
}

/// "returns 404", "redirects to /new/, which returns 404", and so on.
pub(crate) fn describe_response(record: &SeoRecord, site: &Site) -> String {
    if record.redirect_loop() {
        return format!(
            "is stuck in a redirect loop: {}",
            redirect_path(record, site)
        );
    }
    if record.navigation_changed() {
        let target = site.display(&record.final_url);
        return if record.is_ok() {
            format!("redirects to {target}")
        } else {
            format!(
                "redirects to {target}, which returns {}",
                record.http_status
            )
        };
    }
    format!("returns {}", record.http_status)
}

/// What stops the start page from being a page of the site, such as an
/// error or a redirect to another site. `None` when it loads, directly or
/// through redirects within the site.
pub fn start_issue(start: &SeoRecord, site: &Site) -> Option<String> {
    if start.redirect_loop() || !start.is_ok() {
        return Some(describe_response(start, site));
    }
    if start.navigation_changed() && !site.contains(&start.final_url) {
        return Some(format!(
            "redirects to {}, which is not part of the site",
            start.final_url
        ));
    }
    if !start.is_page() {
        return Some(format!(
            "is not an HTML page ({})",
            start.content_type.as_deref().unwrap_or("unknown type")
        ));
    }
    None
}

/// Where a redirect ends, without the query string, as a site key and for
/// display. "/login/?next=/a/" and "/login/?next=/b/" are the same page.
pub(crate) fn redirect_target(record: &SeoRecord, site: &Site) -> (String, String) {
    match url::Url::parse(&record.final_url) {
        Ok(mut url) => {
            url.set_query(None);
            (site.key(url.as_str()), site.display(url.as_str()))
        }
        Err(_) => (record.final_url.clone(), record.final_url.clone()),
    }
}

/// Severity for one of `count` pages that redirect to the same URL, out of
/// `total` pages that could have. Most pages going to one URL is a login
/// wall or a catch-all rule; a few going there with a temporary redirect is
/// a login wall on part of the site.
pub(crate) fn shared_redirect_severity(
    count: usize,
    total: usize,
    record: &SeoRecord,
) -> Option<Severity> {
    let temporary = matches!(
        record.redirect_chain.first().map(|hop| hop.status),
        Some(302 | 303 | 307)
    );
    if count >= 2 && count * 2 >= total {
        Some(Severity::Critical)
    } else if count >= 3 && temporary {
        Some(Severity::High)
    } else {
        None
    }
}

/// "/a -> /b -> /a" for a record's redirect hops.
pub(crate) fn redirect_path(record: &SeoRecord, site: &Site) -> String {
    let mut parts = vec![site.display(&record.url)];
    for hop in &record.redirect_chain {
        parts.push(site.display(&hop.to));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut shown = Vec::new();
    for part in parts {
        let repeat = !seen.insert(part.clone());
        shown.push(part);
        if repeat {
            break;
        }
    }
    shown.join(" -> ")
}

/// Where a noindex comes from, for messages.
pub(crate) fn noindex_source(record: &SeoRecord) -> String {
    let blocks = |token: &String| is_noindex(token);
    if record.indexability.robots_meta.iter().any(blocks) {
        format!(
            "robots meta tag \"{}\"",
            record.indexability.robots_meta.join(", ")
        )
    } else if record.indexability.googlebot_meta.iter().any(blocks) {
        format!(
            "googlebot meta tag \"{}\"",
            record.indexability.googlebot_meta.join(", ")
        )
    } else if record.indexability.x_robots_tag.iter().any(blocks) {
        format!(
            "X-Robots-Tag header \"{}\"",
            record.indexability.x_robots_tag.join(", ")
        )
    } else {
        "noindex".to_owned()
    }
}

/// Drop `noindex` that comes from the X-Robots-Tag header. Preview hosts
/// such as Vercel and Netlify add that header to every preview deployment.
pub fn strip_header_noindex(snapshot: &mut Snapshot) -> usize {
    let mut stripped = 0;
    for record in snapshot.records.values_mut() {
        let before = record.indexability.x_robots_tag.len();
        record
            .indexability
            .x_robots_tag
            .retain(|token| !is_noindex(token));
        if record.indexability.x_robots_tag.len() != before {
            stripped += 1;
            let meta_blocks = record
                .indexability
                .robots_meta
                .iter()
                .chain(&record.indexability.googlebot_meta)
                .any(|token| is_noindex(token));
            record.indexability.is_indexable = record.is_ok() && !meta_blocks;
        }
    }
    stripped
}

/// Shorten long text for one-line display.
pub(crate) fn quote(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(120).collect();
    if collapsed.chars().count() > 120 {
        out.push_str("...");
    }
    format!("\"{out}\"")
}
