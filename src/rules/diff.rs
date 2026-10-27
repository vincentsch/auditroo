//! Compare a baseline crawl with the current crawl.
//!
//! Pages are matched by path, so a preview host can be compared with
//! production. The comparison has three parts:
//!
//! 1. Per-page changes: status, redirects, noindex, canonical, title and the
//!    other page fields. When a page now fails (an error status or a redirect
//!    loop), only that failure is reported, not the title and heading changes
//!    that come with an error page.
//! 2. Site-wide changes: robots.txt rules per crawler, sitemap contents and
//!    pages that lost all internal links.
//! 3. New problems: the `check` rules run on both crawls, and problems that
//!    only the current crawl has are reported.

use std::collections::{BTreeMap, BTreeSet};

use url::Url;

use super::check::{check_with_site, linked_from, robots_error_message};
use super::{
    describe_response, noindex_source, quote, redirect_path, redirect_target,
    shared_redirect_severity, CRAWLERS,
};
use crate::crawl::url::path_and_query;
use crate::crawl::{AgentRules, RobotsFile};
use crate::extract::SeoRecord;
use crate::findings::{format_count, Finding, Rule, Severity};
use crate::graph::inbound_links;
use crate::site::Site;
use crate::sitemap::{SitemapDiscoveryStatus, SitemapDocumentOutcome};
use crate::snapshot::Snapshot;

/// Findings plus notes about what could not be compared.
#[derive(Debug, Clone, Default)]
pub struct DiffOutcome {
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
    /// Indexable baseline pages the current crawl did not reach.
    pub not_compared: usize,
}

pub fn diff(baseline: &Snapshot, current: &Snapshot) -> DiffOutcome {
    let mut site = current.site().with_alias(&baseline.start_url);
    for production in &baseline.production_urls {
        site = site.with_alias(production);
    }
    let before = keyed(baseline, &site);
    let after = keyed(current, &site);
    let mut findings = Vec::new();
    let mut notes = Vec::new();

    let current_failures: BTreeMap<String, &crate::snapshot::CrawlFailureSummary> = current
        .crawl_failures
        .iter()
        .map(|(url, failure)| (site.key(url), failure))
        .collect();
    let mut not_checked = 0usize;
    for (key, old) in &before {
        match after.get(key) {
            Some(new) => compare_pages(old, new, &site, &after, &mut findings),
            None => {
                if let Some(failure) = current_failures.get(key) {
                    if old.is_ok() {
                        let mut message =
                            format!("could not be loaded: {}", failure.kind.describe());
                        if let Some(detail) = &failure.detail {
                            message.push_str(&format!(" ({detail})"));
                        }
                        let severity = if site.is_indexable(old) {
                            Severity::High
                        } else {
                            Severity::Medium
                        };
                        findings.push(
                            Finding::new(Rule::FetchFailed, Some(&old.url), message)
                                .severity(severity),
                        );
                    }
                } else if site.is_indexable(old)
                    && !old.is_not_found_template()
                    && !current.start_blocked_by_robots()
                {
                    not_checked += 1;
                }
            }
        }
    }
    for (key, new) in &after {
        if !before.contains_key(key)
            && new.is_page()
            && new.is_ok()
            && !new.navigation_changed()
            && !new.is_not_found_template()
        {
            findings.push(Finding::new(
                Rule::PageAdded,
                Some(&new.url),
                "is a new page",
            ));
        }
    }
    escalate_redirects(&mut findings, &before, &after, &site);

    let robots = robots_changes(baseline, current, &site, &mut findings);
    // A failing robots.txt also hides the sitemap it names; the robots
    // finding covers that.
    if !robots.broke {
        sitemap_changes(baseline, current, &site, &mut findings);
    }
    if !current.truncated() && !current.start_blocked_by_robots() {
        orphan_changes(baseline, current, &site, &mut findings);
    }
    new_problems(baseline, current, &site, &robots, &mut findings);
    let findings = collapse_cascades(findings, current, &site);

    if not_checked > 0 {
        let one = not_checked == 1;
        let them = if one { "it" } else { "them" };
        let reason = if current.truncated() {
            "the crawl stopped early (see --max-pages and --max-depth)".to_owned()
        } else if current
            .crawl_observation
            .skipped_candidates
            .values()
            .any(|reasons| reasons.contains(&crate::snapshot::CrawlSkipReason::RobotsDenied))
        {
            // The crawl note about robots.txt names --ignore-robots.
            format!("robots.txt blocks auditroo from {them}")
        } else {
            format!(
                "{} outside this crawl's --include and --exclude rules",
                if one { "it is" } else { "they are" }
            )
        };
        notes.push(format!(
            "{} baseline page{} could not be checked because {reason}.",
            format_count(not_checked),
            if one { "" } else { "s" }
        ));
    }
    if current.start_blocked_by_robots() {
        notes.push(
            "robots.txt blocks auditroo from the start page, so only robots.txt itself was compared. Use --ignore-robots to crawl the site anyway."
                .to_owned(),
        );
    }
    DiffOutcome {
        findings,
        notes,
        not_compared: not_checked,
    }
}

fn keyed<'a>(snapshot: &'a Snapshot, site: &Site) -> BTreeMap<String, &'a SeoRecord> {
    let mut map = BTreeMap::new();
    for record in snapshot.records.values() {
        map.entry(site.key(&record.url)).or_insert(record);
    }
    map
}

fn works(record: &SeoRecord) -> bool {
    record.is_ok() && !record.navigation_changed()
}

fn compare_pages(
    old: &SeoRecord,
    new: &SeoRecord,
    site: &Site,
    current: &BTreeMap<String, &SeoRecord>,
    findings: &mut Vec<Finding>,
) {
    let url = Some(new.url.as_str());
    match (works(old), works(new)) {
        (true, false) => {
            let finding = if new.redirect_loop() {
                Finding::new(
                    Rule::RedirectLoop,
                    url,
                    format!(
                        "is now stuck in a redirect loop: {}",
                        redirect_path(new, site)
                    ),
                )
                .severity(Severity::Critical)
            } else if new.is_ok() {
                let hop = new.redirect_chain.first().map(|hop| hop.status);
                Finding::new(
                    Rule::PageRedirected,
                    url,
                    format!(
                        "now redirects to {}{}",
                        site.display(&new.final_url),
                        hop.map(|status| format!(" ({status})")).unwrap_or_default()
                    ),
                )
            } else {
                // Images and PDFs are files, not pages.
                let rule = if old.is_page() {
                    Rule::PageBroken
                } else {
                    Rule::FileBroken
                };
                Finding::new(
                    rule,
                    url,
                    format!(
                        "now {} (it returned {} before)",
                        describe_response(new, site),
                        old.http_status
                    ),
                )
            };
            // A failing page matters most when Google could index it before.
            let finding = if site.is_indexable(old) || finding.rule == Rule::PageRedirected {
                finding
            } else {
                finding.severity(Severity::High)
            };
            findings.push(finding);
        }
        (false, true) => {
            findings.push(Finding::new(
                Rule::PageFixed,
                url,
                format!("works again (before it {})", describe_response(old, site)),
            ));
        }
        (false, false) => compare_redirects(old, new, site, findings),
        (true, true) if old.is_page() && new.is_page() => {
            compare_content(old, new, site, current, findings)
        }
        (true, true) if old.is_page() => {
            let severity = if site.is_indexable(old) {
                Severity::Critical
            } else {
                Severity::High
            };
            findings.push(
                Finding::new(
                    Rule::PageNotHtml,
                    url,
                    format!(
                        "now returns {} instead of an HTML page",
                        new.content_type
                            .as_deref()
                            .unwrap_or("a file of unknown type")
                    ),
                )
                .severity(severity),
            );
        }
        (true, true) => {}
    }
}

fn compare_redirects(old: &SeoRecord, new: &SeoRecord, site: &Site, findings: &mut Vec<Finding>) {
    if !old.navigation_changed() || !new.navigation_changed() {
        return;
    }
    let url = Some(new.url.as_str());
    if new.redirect_loop() && !old.redirect_loop() {
        findings.push(
            Finding::new(
                Rule::RedirectLoop,
                url,
                format!(
                    "is now stuck in a redirect loop: {}",
                    redirect_path(new, site)
                ),
            )
            .severity(Severity::Critical),
        );
        return;
    }
    if old.is_ok() && !new.is_ok() {
        findings.push(Finding::new(
            Rule::PageBroken,
            url,
            format!("now {}", describe_response(new, site)),
        ));
        return;
    }
    if site.key(&old.final_url) != site.key(&new.final_url) {
        findings.push(Finding::new(
            Rule::RedirectChanged,
            url,
            format!(
                "now redirects to {} instead of {}",
                site.display(&new.final_url),
                site.display(&old.final_url)
            ),
        ));
    } else if new.redirect_chain.len() > old.redirect_chain.len() {
        findings.push(
            Finding::new(
                Rule::RedirectChanged,
                url,
                format!(
                    "now takes {} redirects instead of {}: {}",
                    new.redirect_chain.len(),
                    old.redirect_chain.len(),
                    redirect_path(new, site)
                ),
            )
            .severity(Severity::Medium)
            .change(Some(redirect_path(old, site)), None),
        );
    }
}

fn compare_content(
    old: &SeoRecord,
    new: &SeoRecord,
    site: &Site,
    current: &BTreeMap<String, &SeoRecord>,
    findings: &mut Vec<Finding>,
) {
    let url = Some(new.url.as_str());

    if old.indexability.is_indexable && new.has_noindex() {
        findings.push(Finding::new(
            Rule::NoindexAdded,
            url,
            format!("now has noindex ({})", noindex_source(new)),
        ));
    } else if old.has_noindex() && new.indexability.is_indexable {
        findings.push(Finding::new(
            Rule::NoindexRemoved,
            url,
            format!(
                "no longer has noindex, so Google can index it (before: {})",
                noindex_source(old)
            ),
        ));
    }

    compare_canonical(old, new, site, current, findings);

    let old_title = clean(old.title.as_deref());
    let new_title = clean(new.title.as_deref());
    if old_title != new_title {
        match (&old_title, &new_title) {
            (Some(_), None) => findings.push(
                Finding::new(Rule::TitleRemoved, url, "lost its <title>")
                    .change(old_title.as_deref().map(quote), None),
            ),
            (_, Some(title)) => findings.push(
                Finding::new(Rule::TitleChanged, url, "has a new title")
                    .change(
                        old_title.as_deref().map(quote),
                        new_title.as_deref().map(quote),
                    )
                    .group(quote(title)),
            ),
            (None, None) => {}
        }
    }

    let old_description = clean(old.meta_description.as_deref());
    let new_description = clean(new.meta_description.as_deref());
    if old_description != new_description {
        let rule = if new_description.is_none() {
            Rule::DescriptionRemoved
        } else {
            Rule::DescriptionChanged
        };
        let message = if new_description.is_none() {
            "lost its meta description"
        } else {
            "has a new meta description"
        };
        findings.push(Finding::new(rule, url, message).change(
            old_description.as_deref().map(quote),
            new_description.as_deref().map(quote),
        ));
    }

    let old_h1 = joined(&old.headings.h1);
    let new_h1 = joined(&new.headings.h1);
    if old_h1 != new_h1 {
        if new_h1.is_none() {
            findings.push(
                Finding::new(Rule::H1Removed, url, "lost its <h1> heading")
                    .change(old_h1.as_deref().map(quote), None),
            );
        } else {
            findings.push(
                Finding::new(Rule::H1Changed, url, "has a new <h1> heading")
                    .change(old_h1.as_deref().map(quote), new_h1.as_deref().map(quote)),
            );
        }
    }
    if old.headings.h2 != new.headings.h2 || old.headings.h3 != new.headings.h3 {
        findings.push(Finding::new(
            Rule::HeadingsChanged,
            url,
            "has different H2 or H3 headings",
        ));
    }

    let old_types = schema_types(old);
    let new_types = schema_types(new);
    let new_has_invalid = new.structured_data.iter().any(|block| !block.valid_json);
    if !old_types.is_empty() && new.structured_data.is_empty() {
        findings.push(
            Finding::new(
                Rule::StructuredDataRemoved,
                url,
                format!("lost its JSON-LD ({})", old_types.join(", ")),
            )
            .change(Some(old_types.join(", ")), None),
        );
    } else if old_types != new_types && !new_has_invalid {
        let lost: Vec<&String> = old_types
            .iter()
            .filter(|kind| !new_types.contains(kind))
            .collect();
        // A lost type can mean a lost rich result, such as product prices.
        let (message, severity) = if lost.is_empty() {
            ("has new JSON-LD types".to_owned(), Severity::Low)
        } else {
            (
                format!(
                    "lost the JSON-LD type{} {}",
                    if lost.len() == 1 { "" } else { "s" },
                    lost.iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Severity::Medium,
            )
        };
        findings.push(
            Finding::new(Rule::StructuredDataChanged, url, message)
                .severity(severity)
                .change(
                    Some(types_or_none(&old_types)),
                    Some(types_or_none(&new_types)),
                ),
        );
    }

    let old_alternates = alternates(old, site);
    let new_alternates = alternates(new, site);
    if old_alternates != new_alternates {
        findings.push(
            Finding::new(Rule::HreflangChanged, url, "has different hreflang links").change(
                Some(describe_alternates(old, site)),
                Some(describe_alternates(new, site)),
            ),
        );
    }
}

fn compare_canonical(
    old: &SeoRecord,
    new: &SeoRecord,
    site: &Site,
    current: &BTreeMap<String, &SeoRecord>,
    findings: &mut Vec<Finding>,
) {
    let old_key = old.canonical.as_deref().map(|url| site.key(url));
    let new_key = new.canonical.as_deref().map(|url| site.key(url));
    if old_key == new_key {
        return;
    }
    let url = Some(new.url.as_str());
    let shown = |record: &SeoRecord| {
        record
            .canonical
            .as_deref()
            .map(|canonical| site.display(canonical))
    };
    let change = (shown(old), shown(new));
    let old_host = old
        .canonical
        .as_deref()
        .and_then(|canonical| site.foreign_host(canonical));
    let finding = match &new.canonical {
        None => Finding::new(Rule::CanonicalRemoved, url, "lost its canonical tag"),
        Some(canonical) => match site.foreign_host(canonical) {
            // A new host is the problem. A new path on the host the page
            // already named is a changed canonical tag.
            Some(host) if old_host.as_ref() != Some(&host) => {
                let severity = if new.indexability.is_indexable {
                    Severity::Critical
                } else {
                    Severity::High
                };
                Finding::new(
                    Rule::CanonicalExternal,
                    url,
                    format!("canonical tag now points to another host ({host})"),
                )
                .severity(severity)
                .group(host)
            }
            Some(_) => Finding::new(
                Rule::CanonicalChanged,
                url,
                format!("canonical tag now names {}", site.display(canonical)),
            ),
            None if site.canonical_is_self(new) => Finding::new(
                Rule::CanonicalChanged,
                url,
                "canonical tag now names the page itself",
            )
            .severity(Severity::Medium),
            None => {
                let mut message = format!("canonical tag now names {}", site.display(canonical));
                // Say when the new target itself does not work.
                if let Some(target) = current.get(&site.key(canonical)) {
                    if target.redirect_loop() || !target.is_ok() || target.navigation_changed() {
                        message.push_str(&format!(", which {}", describe_response(target, site)));
                    } else if target.has_noindex() {
                        message.push_str(", which has noindex");
                    }
                }
                Finding::new(Rule::CanonicalChanged, url, message)
            }
        },
    };
    findings.push(finding.change(change.0, change.1));
}

/// What changed in robots.txt, for deciding which `check` problems it explains.
#[derive(Default)]
struct RobotsChange {
    /// robots.txt now fails, which blocks every crawler.
    broke: bool,
    /// Keys of the pages Googlebot may no longer crawl.
    googlebot_blocked: BTreeSet<String>,
}

fn robots_changes(
    baseline: &Snapshot,
    current: &Snapshot,
    site: &Site,
    findings: &mut Vec<Finding>,
) -> RobotsChange {
    let mut change = RobotsChange::default();

    // Files that fail now and worked before. Each origin is compared with
    // the same origin of the baseline (production's for a preview).
    let old_files: BTreeMap<String, &RobotsFile> = baseline
        .robots
        .values()
        .map(|file| (site.key(&file.url), file))
        .collect();
    let start_file = current.start_robots().map(|file| file.url.clone());
    for file in current.robots.values() {
        let worked = old_files
            .get(&site.key(&file.url))
            .is_some_and(|old| !old.blocks_everything_by_error());
        if worked && file.blocks_everything_by_error() {
            let message = match &file.error {
                Some(error) if file.status.is_none() => format!(
                    "can no longer be loaded ({error}). Google treats this like a server error and stops crawling the site"
                ),
                _ => format!("now {}", robots_error_message(file)),
            };
            findings.push(Finding::new(Rule::RobotsError, Some(&file.url), message));
            if start_file.as_deref() == Some(file.url.as_str()) {
                change.broke = true;
            }
        }
    }
    if change.broke {
        return change;
    }

    let pages: Vec<&SeoRecord> = baseline
        .records
        .values()
        .filter(|record| site.is_indexable(record))
        .collect();
    // Crawlers that lose the same pages are reported together, so one
    // `Disallow` line for everyone is one finding, not ten.
    let mut by_pages: Vec<(Vec<String>, Vec<&super::Crawler>)> = Vec::new();
    for crawler in CRAWLERS {
        // Parsed rules per file, one cache per crawl: the baseline and the
        // current crawl often have a file at the same URL.
        let mut old_rules: BTreeMap<String, AgentRules> = BTreeMap::new();
        let mut new_rules: BTreeMap<String, AgentRules> = BTreeMap::new();
        let allows = |rules: &mut BTreeMap<String, AgentRules>, file: &RobotsFile, url: &Url| {
            rules
                .entry(file.url.clone())
                .or_insert_with(|| AgentRules::new(file, crawler.token))
                .allows(url)
        };
        let mut blocked = Vec::new();
        for record in &pages {
            let now = current_url(record, current, site);
            let (Some(old_file), Some(new_file), Ok(old_url), Ok(new_url)) = (
                baseline.robots_file(&record.url),
                current.robots_file(&now),
                Url::parse(&record.url),
                Url::parse(&now),
            ) else {
                continue;
            };
            // A failing file is reported above, not page by page.
            if old_file.blocks_everything_by_error() || new_file.blocks_everything_by_error() {
                continue;
            }
            if allows(&mut old_rules, old_file, &old_url)
                && !allows(&mut new_rules, new_file, &new_url)
            {
                blocked.push(now);
            }
        }
        if blocked.is_empty() {
            continue;
        }
        if crawler.token == "Googlebot" {
            change.googlebot_blocked = blocked.iter().map(|url| site.key(url)).collect();
        }
        match by_pages.iter_mut().find(|(pages, _)| *pages == blocked) {
            Some((_, crawlers)) => crawlers.push(crawler),
            None => by_pages.push((blocked, vec![crawler])),
        }
    }
    for (blocked, crawlers) in by_pages {
        let total = blocked.len();
        let first = crawlers[0];
        let who = match crawlers.len() {
            1 => first.name.to_owned(),
            2 => format!("{} and {}", first.name, crawlers[1].name),
            count => format!("{} and {} other crawlers", first.name, count - 1),
        };
        let severity = crawlers
            .iter()
            .map(|crawler| crawler.severity)
            .max()
            .unwrap_or(Severity::Medium);
        let mut finding = Finding::new(
            Rule::RobotsBlocked,
            None,
            format!(
                "robots.txt now blocks {who} from {} page{} that {} could crawl before",
                format_count(total),
                if total == 1 { "" } else { "s" },
                if crawlers.len() == 1 { "it" } else { "they" }
            ),
        )
        .severity(severity)
        .group(who)
        .related(blocked);
        if crawlers.len() > 2 {
            finding = finding.detail(format!(
                "crawlers: {}",
                crawlers
                    .iter()
                    .map(|crawler| crawler.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        findings.push(finding);
    }
    change
}

/// The URL of a baseline page on the current host, for display.
fn current_url(record: &SeoRecord, current: &Snapshot, site: &Site) -> String {
    let key = site.key(&record.url);
    current
        .records
        .values()
        .find(|candidate| site.key(&candidate.url) == key)
        .map(|candidate| candidate.url.clone())
        .or_else(|| {
            let start = Url::parse(&current.start_url).ok()?;
            let old = Url::parse(&record.url).ok()?;
            let mut rebased = start;
            rebased.set_path(old.path());
            rebased.set_query(old.query());
            Some(rebased.to_string())
        })
        .unwrap_or_else(|| record.url.clone())
}

fn sitemap_changes(
    baseline: &Snapshot,
    current: &Snapshot,
    site: &Site,
    findings: &mut Vec<Finding>,
) {
    let old_discovery = &baseline.sitemap.discovery;
    let new_discovery = &current.sitemap.discovery;
    let usable = |status: &SitemapDiscoveryStatus| {
        matches!(
            status,
            SitemapDiscoveryStatus::Complete | SitemapDiscoveryStatus::Partial
        )
    };

    // Sitemap files that were read before and fail now.
    let old_documents: BTreeMap<String, &crate::sitemap::SitemapDocumentDiagnostic> = old_discovery
        .documents
        .iter()
        .map(|document| (site.key(&document.requested_url), document))
        .collect();
    let mut broken = Vec::new();
    for document in &new_discovery.documents {
        let failed = matches!(
            document.outcome,
            SitemapDocumentOutcome::Missing
                | SitemapDocumentOutcome::Invalid
                | SitemapDocumentOutcome::Unavailable
        );
        let worked_before = old_documents
            .get(&site.key(&document.requested_url))
            .is_some_and(|old| {
                matches!(
                    old.outcome,
                    SitemapDocumentOutcome::ParsedUrlSet
                        | SitemapDocumentOutcome::ParsedSitemapIndex
                )
            });
        if failed && worked_before {
            let message = match document.outcome {
                SitemapDocumentOutcome::Missing => {
                    format!("now returns {}", document.final_status.unwrap_or(404))
                }
                SitemapDocumentOutcome::Invalid => format!(
                    "is no longer a valid sitemap: {}",
                    document.detail.as_deref().unwrap_or("unknown problem")
                ),
                _ => format!(
                    "can no longer be read: {}",
                    document.detail.as_deref().unwrap_or("unknown problem")
                ),
            };
            broken.push(Finding::new(
                Rule::SitemapBroken,
                Some(&document.requested_url),
                message,
            ));
        }
    }

    // Indexable pages that the sitemap listed before and does not list now,
    // while the page itself is still fine. A page that the sitemap now lists
    // on another host is reported as `sitemap_url_external` instead.
    if usable(&old_discovery.status) {
        let new_declared: BTreeSet<String> = if usable(&new_discovery.status) {
            new_discovery
                .declared_urls
                .iter()
                .map(|url| site.key(url))
                .collect()
        } else {
            BTreeSet::new()
        };
        let moved_away: BTreeSet<String> = new_discovery
            .declared_urls
            .iter()
            .filter(|url| !site.contains(url))
            .filter_map(|url| Url::parse(url).ok())
            .map(|url| path_and_query(&url))
            .collect();
        let current_pages = keyed(current, site);
        let removed: Vec<String> = old_discovery
            .declared_urls
            .iter()
            .filter(|url| {
                let key = site.key(url);
                let moved =
                    Url::parse(url).is_ok_and(|url| moved_away.contains(&path_and_query(&url)));
                !new_declared.contains(&key)
                    && !moved
                    && current_pages
                        .get(&key)
                        .is_some_and(|record| site.is_indexable(record))
            })
            .map(|url| {
                current_pages
                    .get(&site.key(url))
                    .map(|record| record.url.clone())
                    .unwrap_or_else(|| url.clone())
            })
            .collect();
        if !removed.is_empty() {
            let total = removed.len();
            if let Some(first) = broken.first_mut() {
                first.message.push_str(&format!(
                    ", so {} URL{} {} no longer in any sitemap",
                    format_count(total),
                    if total == 1 { "" } else { "s" },
                    if total == 1 { "is" } else { "are" }
                ));
                *first = first.clone().related(removed);
            } else {
                findings.push(
                    Finding::new(
                        Rule::SitemapUrlsRemoved,
                        None,
                        format!(
                            "{} URL{} that the sitemap listed before {} no longer in it, although the page{} still work{}",
                            format_count(total),
                            if total == 1 { "" } else { "s" },
                            if total == 1 { "is" } else { "are" },
                            if total == 1 { "" } else { "s" },
                            if total == 1 { "s" } else { "" },
                        ),
                    )
                    .related(removed),
                );
            }
        }
    }
    findings.extend(broken);
}

fn orphan_changes(
    baseline: &Snapshot,
    current: &Snapshot,
    site: &Site,
    findings: &mut Vec<Finding>,
) {
    if baseline.truncated() {
        return;
    }
    let old_inbound: BTreeMap<String, BTreeSet<String>> =
        inbound_links(&baseline.link_graph, &baseline.records)
            .into_iter()
            .map(|(url, sources)| (site.key(&url), sources))
            .collect();
    let new_inbound: BTreeMap<String, BTreeSet<String>> =
        inbound_links(&current.link_graph, &current.records)
            .into_iter()
            .map(|(url, sources)| (site.key(&url), sources))
            .collect();
    let old_orphans: BTreeSet<String> = baseline.orphans.iter().map(|url| site.key(url)).collect();
    let old_pages = keyed(baseline, site);
    for orphan in &current.orphans {
        let key = site.key(orphan);
        let Some(old) = old_pages.get(&key) else {
            continue;
        };
        if old_orphans.contains(&key) || !site.is_indexable(old) {
            continue;
        }
        let finding = match new_inbound.get(&key) {
            // Still linked, but only from pages that are also cut off.
            Some(sources) if !sources.is_empty() => Finding::new(
                Rule::Orphaned,
                Some(orphan),
                "can no longer be reached by following links from the start page. Only pages that are also cut off link to it",
            )
            .related(sources.iter().cloned()),
            _ => Finding::new(
                Rule::Orphaned,
                Some(orphan),
                "is no longer linked from any crawled page",
            )
            .related(
                old_inbound
                    .get(&key)
                    .into_iter()
                    .flatten()
                    .map(|source| current_url_for(source, current, site)),
            ),
        };
        findings.push(finding);
    }
}

fn current_url_for(url: &str, current: &Snapshot, site: &Site) -> String {
    let key = site.key(url);
    current
        .records
        .keys()
        .find(|candidate| site.key(candidate) == key)
        .cloned()
        .unwrap_or_else(|| url.to_owned())
}

/// Problems from `check` that the current crawl has and the baseline did not.
fn new_problems(
    baseline: &Snapshot,
    current: &Snapshot,
    site: &Site,
    robots: &RobotsChange,
    findings: &mut Vec<Finding>,
) {
    let identity = |finding: &Finding| {
        (
            finding.rule,
            finding
                .url
                .as_deref()
                .map(|url| site.key(url))
                .unwrap_or_default(),
            finding.group.clone().unwrap_or_default(),
        )
    };
    let old: BTreeSet<_> = check_with_site(baseline, site.clone())
        .iter()
        .map(identity)
        .collect();
    let old_sitemap_hosts: BTreeSet<String> = baseline
        .sitemap
        .discovery
        .documents
        .iter()
        .filter_map(|document| site.foreign_host(&document.requested_url))
        .collect();
    let transitions: BTreeMap<String, BTreeSet<Rule>> =
        findings.iter().fold(BTreeMap::new(), |mut map, finding| {
            if let Some(url) = &finding.url {
                map.entry(site.key(url)).or_default().insert(finding.rule);
            }
            map
        });

    for mut finding in check_with_site(current, site.clone()) {
        if old.contains(&identity(&finding)) {
            continue;
        }
        let key = finding
            .url
            .as_deref()
            .map(|url| site.key(url))
            .unwrap_or_default();
        let covered = transitions.get(&key).is_some_and(|rules| {
            rules.contains(&finding.rule)
                || covered_by(finding.rule)
                    .iter()
                    .any(|rule| rules.contains(rule))
        });
        let robots_covered = match finding.rule {
            Rule::SitemapUrlBlocked => robots.broke || robots.googlebot_blocked.contains(&key),
            Rule::RobotsBlocksSite => robots.broke || !robots.googlebot_blocked.is_empty(),
            Rule::RobotsError | Rule::SitemapMissing | Rule::SitemapBroken => robots.broke,
            _ => false,
        };
        if covered || robots_covered {
            continue;
        }
        // Sitemap entries that newly name another host, or a sitemap that
        // moved to a host it never used, are the mark of a build with the
        // wrong site URL, such as a staging host.
        let moved_sitemap = finding.rule == Rule::SitemapBroken
            && finding
                .url
                .as_deref()
                .and_then(|url| site.foreign_host(url))
                .is_some_and(|host| !old_sitemap_hosts.contains(&host));
        if moved_sitemap {
            finding
                .message
                .push_str(". The sitemap was on this site before and is now on another host");
        }
        if finding.rule == Rule::SitemapUrlExternal || moved_sitemap {
            finding.severity = Severity::Critical;
        }
        findings.push(finding);
    }
}

/// Transition rules that already describe a new `check` problem.
fn covered_by(rule: Rule) -> &'static [Rule] {
    match rule {
        Rule::BrokenLink | Rule::SitemapUrlError | Rule::HomeError => &[
            Rule::PageBroken,
            Rule::FileBroken,
            Rule::RedirectLoop,
            Rule::FetchFailed,
            Rule::RedirectChanged,
        ],
        Rule::RedirectLoop => &[Rule::RedirectLoop],
        Rule::SitemapUrlRedirect | Rule::RedirectChain => &[
            Rule::PageRedirected,
            Rule::RedirectChanged,
            Rule::PageBroken,
        ],
        Rule::CanonicalExternal
        | Rule::SitemapUrlCanonicalised
        | Rule::CanonicalToRedirect
        | Rule::CanonicalToError
        | Rule::CanonicalToNoindex => &[Rule::CanonicalExternal, Rule::CanonicalChanged],
        Rule::SitemapUrlNoindex | Rule::HomeNoindex => &[Rule::NoindexAdded],
        Rule::TitleMissing => &[Rule::TitleRemoved],
        Rule::TitleDuplicate => &[Rule::TitleChanged],
        Rule::H1Missing => &[Rule::H1Removed],
        Rule::OrphanPage => &[Rule::Orphaned],
        _ => &[],
    }
}

/// When a page now fails, its other changes come from the error page. Report
/// only the failure, count the other changes on it, and add the pages that
/// link to it.
fn collapse_cascades(findings: Vec<Finding>, current: &Snapshot, site: &Site) -> Vec<Finding> {
    const ROOTS: [Rule; 7] = [
        Rule::PageBroken,
        Rule::FileBroken,
        Rule::PageNotHtml,
        Rule::RedirectLoop,
        Rule::FetchFailed,
        Rule::PageRedirected,
        Rule::RedirectChanged,
    ];
    let roots: BTreeMap<String, usize> = findings
        .iter()
        .enumerate()
        .filter(|(_, finding)| ROOTS.contains(&finding.rule))
        .filter_map(|(index, finding)| finding.url.as_ref().map(|url| (site.key(url), index)))
        .collect();
    let links = linked_from(current);

    let mut follow_on: BTreeMap<usize, Vec<Rule>> = BTreeMap::new();
    let mut keep = Vec::new();
    for (index, finding) in findings.iter().enumerate() {
        let root = finding
            .url
            .as_ref()
            .and_then(|url| roots.get(&site.key(url)))
            .copied();
        match root {
            Some(root) if root != index && finding.rule != Rule::RobotsBlocked => {
                follow_on.entry(root).or_default().push(finding.rule);
            }
            _ => keep.push(index),
        }
    }
    keep.into_iter()
        .map(|index| {
            let mut finding = findings[index].clone();
            if let Some(rules) = follow_on.remove(&index) {
                // The finding gets the severity of the worst change on the
                // page, so a start page that now leaves the site is still
                // critical.
                for rule_index in findings
                    .iter()
                    .enumerate()
                    .filter(|(other, candidate)| {
                        *other != index
                            && candidate.url.as_ref().map(|url| site.key(url))
                                == finding.url.as_ref().map(|url| site.key(url))
                            && rules.contains(&candidate.rule)
                    })
                    .map(|(other, _)| other)
                {
                    finding.severity = finding.severity.max(findings[rule_index].severity);
                }
                finding.follow_on.extend(rules);
            }
            if ROOTS.contains(&finding.rule) && finding.related.is_empty() {
                if let Some(url) = &finding.url {
                    if let Some(sources) = links.get(url.as_str()) {
                        finding = finding.related(sources.iter().cloned());
                    }
                }
            }
            finding
        })
        .collect()
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty())
}

fn joined(values: &[String]) -> Option<String> {
    let text = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    (!text.is_empty()).then_some(text)
}

fn schema_types(record: &SeoRecord) -> Vec<String> {
    let mut types: Vec<String> = record
        .structured_data
        .iter()
        .filter(|block| block.valid_json)
        .flat_map(|block| {
            if block.types.is_empty() {
                vec!["untyped".to_owned()]
            } else {
                block.types.clone()
            }
        })
        .collect();
    types.sort();
    types.dedup();
    types
}

/// Pages that now redirect matter more when many go to the same URL, which
/// usually means a login wall or a catch-all rule, or when the target is no
/// page Google could index. A redirect to the page's own new address stays
/// medium.
fn escalate_redirects(
    findings: &mut [Finding],
    before: &BTreeMap<String, &SeoRecord>,
    after: &BTreeMap<String, &SeoRecord>,
    site: &Site,
) {
    let redirected = |finding: &Finding| -> Option<&SeoRecord> {
        if finding.rule != Rule::PageRedirected {
            return None;
        }
        after.get(&site.key(finding.url.as_deref()?)).copied()
    };
    let mut per_target: BTreeMap<String, usize> = BTreeMap::new();
    for record in findings.iter().filter_map(redirected) {
        *per_target
            .entry(redirect_target(record, site).0)
            .or_default() += 1;
    }
    let indexable_before = before
        .values()
        .filter(|record| site.is_indexable(record))
        .count();
    for finding in findings.iter_mut() {
        let Some(record) = redirected(finding) else {
            continue;
        };
        let (key, shown) = redirect_target(record, site);
        let count = per_target[&key];
        // The redirect record carries the target's own response.
        let target_indexable = record.is_page() && !record.has_noindex();
        let severity = shared_redirect_severity(count, indexable_before, record).unwrap_or(
            if target_indexable {
                Severity::Medium
            } else {
                Severity::High
            },
        );
        if count >= 2 {
            finding.group = Some(shown);
        }
        finding.severity = finding.severity.max(severity);
    }
}

fn types_or_none(types: &[String]) -> String {
    if types.is_empty() {
        "none".to_owned()
    } else {
        types.join(", ")
    }
}

fn alternates(record: &SeoRecord, site: &Site) -> BTreeSet<(String, String)> {
    record
        .hreflang
        .iter()
        .map(|alternate| {
            (
                alternate.hreflang.to_ascii_lowercase(),
                site.key(&alternate.href),
            )
        })
        .collect()
}

fn describe_alternates(record: &SeoRecord, site: &Site) -> String {
    if record.hreflang.is_empty() {
        return "none".to_owned();
    }
    record
        .hreflang
        .iter()
        .map(|alternate| format!("{} {}", alternate.hreflang, site.display(&alternate.href)))
        .collect::<Vec<_>>()
        .join(", ")
}
