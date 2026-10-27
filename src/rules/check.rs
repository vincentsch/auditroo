//! Problems that one crawl shows on its own, without a baseline.

use std::collections::{BTreeMap, BTreeSet};

use url::Url;

use super::{
    describe_response, noindex_source, quote, records_by_key, redirect_target,
    shared_redirect_severity, start_issue,
};
use crate::crawl::robots::origin_key;
use crate::crawl::AgentRules;
use crate::extract::SeoRecord;
use crate::findings::{Finding, Rule, Severity};
use crate::site::Site;
use crate::sitemap::{SitemapDiscoveryStatus, SitemapDocumentOutcome};
use crate::snapshot::Snapshot;

/// Run every built-in check on one snapshot.
pub fn check(snapshot: &Snapshot) -> Vec<Finding> {
    check_with_site(snapshot, snapshot.site())
}

/// Run the checks with a given set of hosts that count as the site, for
/// example production and the preview when `diff` compares the two.
pub fn check_with_site(snapshot: &Snapshot, site: Site) -> Vec<Finding> {
    let googlebot = snapshot
        .robots
        .iter()
        .map(|(origin, file)| (origin.clone(), AgentRules::new(file, "Googlebot")))
        .collect();
    let context = Context {
        snapshot,
        by_key: records_by_key(snapshot, &site),
        googlebot,
        site,
    };
    let mut findings = Vec::new();
    context.robots(&mut findings);
    context.start_page(&mut findings);
    context.links(&mut findings);
    context.pages(&mut findings);
    context.duplicate_titles(&mut findings);
    context.sitemap(&mut findings);
    context.hreflang(&mut findings);
    collapse_same_url(findings)
}

struct Context<'a> {
    snapshot: &'a Snapshot,
    site: Site,
    by_key: BTreeMap<String, &'a SeoRecord>,
    /// Googlebot's rules per origin, from each origin's own robots.txt.
    googlebot: BTreeMap<String, AgentRules>,
}

/// The message for a robots.txt file that Google cannot use.
pub(crate) fn robots_error_message(file: &crate::crawl::RobotsFile) -> String {
    match (file.status, &file.error) {
        (Some(status), _) => format!(
            "returns {status}. Google stops crawling the whole site while robots.txt fails"
        ),
        (None, Some(error)) => format!(
            "could not be loaded ({error}). Google treats this like a server error and stops crawling the site"
        ),
        (None, None) => "could not be loaded".to_owned(),
    }
}

impl Context<'_> {
    /// Whether Googlebot may crawl `url` by its origin's robots.txt. `None`
    /// when the crawl did not load that file, or the file fails (that is
    /// reported on its own).
    fn googlebot_allows(&self, url: &str) -> Option<bool> {
        let parsed = Url::parse(url).ok()?;
        let origin = origin_key(&parsed);
        if self
            .snapshot
            .robots
            .get(&origin)?
            .blocks_everything_by_error()
        {
            return None;
        }
        Some(self.googlebot.get(&origin)?.allows(&parsed))
    }

    fn robots(&self, findings: &mut Vec<Finding>) {
        let start_origin = Url::parse(&self.snapshot.start_url)
            .map(|url| origin_key(&url))
            .unwrap_or_default();
        // The start host's file always counts. Another host of the site,
        // such as the `www` twin, only when the crawl found pages there.
        let has_pages = |origin: &str| {
            self.snapshot.pages().any(|record| {
                record.is_ok()
                    && Url::parse(&record.url).is_ok_and(|url| origin_key(&url) == origin)
            })
        };
        for (origin, file) in &self.snapshot.robots {
            if file.blocks_everything_by_error() && (*origin == start_origin || has_pages(origin)) {
                findings.push(Finding::new(
                    Rule::RobotsError,
                    Some(&file.url),
                    robots_error_message(file),
                ));
            }
        }
        if self.googlebot_allows(&self.snapshot.start_url) == Some(false) {
            findings.push(Finding::new(
                Rule::RobotsBlocksSite,
                Some(&self.snapshot.start_url),
                "robots.txt blocks Googlebot from this page, so Google cannot crawl the site from here",
            ));
        }
    }

    fn start_page(&self, findings: &mut Vec<Finding>) {
        let Some(start) = self.snapshot.records.get(&self.snapshot.start_url) else {
            return;
        };
        if let Some(issue) = start_issue(start, &self.site) {
            findings.push(Finding::new(Rule::HomeError, Some(&start.url), issue));
        } else if start.is_page() && start.is_ok() && start.has_noindex() {
            findings.push(Finding::new(
                Rule::HomeNoindex,
                Some(&start.url),
                format!("has noindex ({})", noindex_source(start)),
            ));
        }
    }

    fn links(&self, findings: &mut Vec<Finding>) {
        for (target, sources) in linked_from(self.snapshot) {
            if let Some(record) = self.snapshot.records.get(target) {
                let finding = if record.redirect_loop() {
                    Finding::new(
                        Rule::RedirectLoop,
                        Some(target),
                        describe_response(record, &self.site),
                    )
                } else if record.is_error() {
                    Finding::new(
                        Rule::BrokenLink,
                        Some(target),
                        describe_response(record, &self.site),
                    )
                } else if !record.is_ok() {
                    Finding::new(
                        Rule::BrokenLink,
                        Some(target),
                        format!(
                            "returns {} without a redirect target that auditroo could follow",
                            record.http_status
                        ),
                    )
                } else if record.redirect_chain.len() >= 2 {
                    Finding::new(
                        Rule::RedirectChain,
                        Some(target),
                        format!(
                            "redirects {} times before it reaches {}",
                            record.redirect_chain.len(),
                            self.site.display(&record.final_url)
                        ),
                    )
                } else {
                    continue;
                };
                findings.push(finding.related(sources));
            } else if let Some(failure) = self.snapshot.crawl_failures.get(target) {
                let mut message = format!("could not be loaded: {}", failure.kind.describe());
                if let Some(detail) = &failure.detail {
                    message.push_str(&format!(" ({detail})"));
                }
                findings.push(
                    Finding::new(Rule::BrokenLink, Some(target), message)
                        .severity(Rule::FetchFailed.severity())
                        .related(sources),
                );
            }
        }
    }

    fn pages(&self, findings: &mut Vec<Finding>) {
        for record in self.snapshot.pages() {
            if !record.is_ok() || record.navigation_changed() {
                continue;
            }
            let url = Some(record.url.as_str());
            if let Some(block) = record
                .structured_data
                .iter()
                .find(|block| !block.valid_json)
            {
                let reason = block
                    .error
                    .as_deref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default();
                findings.push(Finding::new(
                    Rule::StructuredDataInvalid,
                    url,
                    format!("has a JSON-LD block that is not valid JSON{reason}"),
                ));
            }
            if self.site.is_indexable(record) {
                if record
                    .title
                    .as_deref()
                    .is_none_or(|title| title.trim().is_empty())
                {
                    findings.push(Finding::new(Rule::TitleMissing, url, "has no <title>"));
                }
                if record
                    .headings
                    .h1
                    .iter()
                    .all(|heading| heading.trim().is_empty())
                {
                    findings.push(Finding::new(Rule::H1Missing, url, "has no <h1> heading"));
                }
            }
            self.canonical(record, findings);
        }
    }

    fn canonical(&self, record: &SeoRecord, findings: &mut Vec<Finding>) {
        let Some(canonical) = &record.canonical else {
            return;
        };
        if self.site.canonical_is_self(record) {
            return;
        }
        let url = Some(record.url.as_str());
        let shown = self.site.display(canonical);
        if let Some(host) = self.site.foreign_host(canonical) {
            // Only high here, because some sites point canonical tags to
            // another domain on purpose, for example for articles that first
            // appeared there. In diff, a canonical tag that newly moves to
            // another host is critical.
            findings.push(
                Finding::new(
                    Rule::CanonicalExternal,
                    url,
                    format!(
                        "names {shown} as its canonical URL. If that is on purpose, for example for articles that first appeared there, accept it with the ignore rule {{ rule = \"canonical_external\", url = \"{}\" }}",
                        self.site.display(&record.url)
                    ),
                )
                .group(host),
            );
            return;
        }
        let Some(target) = self.by_key.get(&self.site.key(canonical)) else {
            return;
        };
        let finding = if target.redirect_loop() || !target.is_ok() {
            Finding::new(
                Rule::CanonicalToError,
                url,
                format!(
                    "names {shown} as its canonical URL, but {shown} {}",
                    describe_response(target, &self.site)
                ),
            )
        } else if target.navigation_changed() {
            Finding::new(
                Rule::CanonicalToRedirect,
                url,
                format!(
                    "names {shown} as its canonical URL, but {shown} {}",
                    describe_response(target, &self.site)
                ),
            )
        } else if target.has_noindex() {
            Finding::new(
                Rule::CanonicalToNoindex,
                url,
                format!("names {shown} as its canonical URL, but {shown} has noindex"),
            )
        } else {
            return;
        };
        findings.push(finding);
    }

    fn duplicate_titles(&self, findings: &mut Vec<Finding>) {
        let mut by_title: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for record in self.snapshot.pages() {
            if !self.site.is_indexable(record) {
                continue;
            }
            if let Some(title) = record.title.as_deref().map(str::trim) {
                if !title.is_empty() {
                    by_title
                        .entry(title.to_owned())
                        .or_default()
                        .push(&record.url);
                }
            }
        }
        for (title, urls) in by_title {
            if urls.len() < 2 {
                continue;
            }
            for url in &urls {
                let others = urls
                    .iter()
                    .filter(|other| *other != url)
                    .map(|other| (*other).to_owned());
                findings.push(
                    Finding::new(
                        Rule::TitleDuplicate,
                        Some(url),
                        format!(
                            "shares the title {} with {} other page{}",
                            quote(&title),
                            urls.len() - 1,
                            if urls.len() == 2 { "" } else { "s" }
                        ),
                    )
                    .group(quote(&title))
                    .related(others),
                );
            }
        }
    }

    fn sitemap(&self, findings: &mut Vec<Finding>) {
        let sitemap = &self.snapshot.sitemap;
        match sitemap.discovery.status {
            SitemapDiscoveryStatus::Absent => {
                findings.push(Finding::new(
                    Rule::SitemapMissing,
                    None,
                    "no sitemap was found at /sitemap.xml, and robots.txt names none",
                ));
            }
            _ => {
                for document in &sitemap.discovery.documents {
                    let message = match document.outcome {
                        SitemapDocumentOutcome::Missing => {
                            format!("returns {}", document.final_status.unwrap_or(404))
                        }
                        SitemapDocumentOutcome::ParsedUrlSet
                        | SitemapDocumentOutcome::ParsedSitemapIndex
                            if document.invalid_entries > 0 =>
                        {
                            let invalid = document.invalid_entries;
                            let severity = if document.entries == 0 {
                                Severity::High
                            } else {
                                Severity::Medium
                            };
                            findings.push(
                                Finding::new(
                                    Rule::SitemapBroken,
                                    Some(&document.requested_url),
                                    format!(
                                        "has {invalid} entr{} without a full http(s) URL in <loc>, for example a relative path. Google ignores {}",
                                        if invalid == 1 { "y" } else { "ies" },
                                        if invalid == 1 { "it" } else { "them" }
                                    ),
                                )
                                .severity(severity),
                            );
                            continue;
                        }
                        SitemapDocumentOutcome::Invalid => format!(
                            "is not a valid sitemap: {}",
                            document.detail.as_deref().unwrap_or("unknown problem")
                        ),
                        SitemapDocumentOutcome::Unavailable => format!(
                            "could not be read: {}",
                            document.detail.as_deref().unwrap_or("unknown problem")
                        ),
                        _ => continue,
                    };
                    findings.push(Finding::new(
                        Rule::SitemapBroken,
                        Some(&document.requested_url),
                        message,
                    ));
                }
            }
        }

        let sitemap_note = "but the sitemap lists it";
        // Listed URLs that redirect to the same page, such as a login page.
        let mut redirect_targets: BTreeMap<String, usize> = BTreeMap::new();
        let mut on_site = 0usize;
        for url in &sitemap.discovery.declared_urls {
            if self.site.contains(url) {
                on_site += 1;
            }
            if let Some(record) = self.snapshot.records.get(url) {
                if record.navigation_changed() && record.is_ok() && !record.redirect_loop() {
                    *redirect_targets
                        .entry(redirect_target(record, &self.site).0)
                        .or_default() += 1;
                }
            }
        }
        for url in &sitemap.discovery.declared_urls {
            let target = Some(url.as_str());
            if let Some(host) = self.site.foreign_host(url) {
                findings.push(
                    Finding::new(
                        Rule::SitemapUrlExternal,
                        target,
                        "is listed in the sitemap, but it is on another host",
                    )
                    .group(host),
                );
                continue;
            }
            if let Some(record) = self.snapshot.records.get(url) {
                if record.redirect_loop()
                    || record.is_error()
                    || !record.is_ok() && !record.navigation_changed()
                {
                    findings.push(Finding::new(
                        Rule::SitemapUrlError,
                        target,
                        format!("{}, {sitemap_note}", describe_response(record, &self.site)),
                    ));
                } else if record.navigation_changed() {
                    let (key, shown) = redirect_target(record, &self.site);
                    let count = redirect_targets.get(&key).copied().unwrap_or(0);
                    let mut finding = Finding::new(
                        Rule::SitemapUrlRedirect,
                        target,
                        format!("{}, {sitemap_note}", describe_response(record, &self.site)),
                    );
                    if let Some(severity) = shared_redirect_severity(count, on_site, record) {
                        finding = finding.severity(severity).group(shown);
                    }
                    findings.push(finding);
                } else if record.is_page() && record.has_noindex() {
                    findings.push(Finding::new(
                        Rule::SitemapUrlNoindex,
                        target,
                        format!("has noindex ({}), {sitemap_note}", noindex_source(record)),
                    ));
                } else if record.is_page() && !self.site.canonical_is_self(record) {
                    let canonical = record.canonical.as_deref().unwrap_or_default();
                    findings.push(Finding::new(
                        Rule::SitemapUrlCanonicalised,
                        target,
                        format!(
                            "names {} as its canonical URL, {sitemap_note}",
                            self.site.display(canonical)
                        ),
                    ));
                }
            } else if let Some(failure) = self.snapshot.crawl_failures.get(url) {
                findings.push(Finding::new(
                    Rule::SitemapUrlError,
                    target,
                    format!(
                        "could not be loaded ({}), {sitemap_note}",
                        failure.kind.describe()
                    ),
                ));
            }
            if self.googlebot_allows(url) == Some(false) {
                findings.push(Finding::new(
                    Rule::SitemapUrlBlocked,
                    target,
                    format!("is blocked for Googlebot by robots.txt, {sitemap_note}"),
                ));
            }
        }

        if !self.snapshot.truncated() {
            let declared: BTreeSet<&String> = sitemap.discovery.declared_urls.iter().collect();
            for orphan in &self.snapshot.orphans {
                if declared.contains(orphan) {
                    findings.push(Finding::new(
                        Rule::OrphanPage,
                        Some(orphan),
                        "is in the sitemap, but it cannot be reached by following links from the start page",
                    ));
                }
            }
        }
    }

    fn hreflang(&self, findings: &mut Vec<Finding>) {
        for record in self.snapshot.pages() {
            if !self.site.is_indexable(record) {
                continue;
            }
            let own_key = self.site.key(&record.url);
            for alternate in &record.hreflang {
                if !self.site.contains(&alternate.href) {
                    continue;
                }
                let target_key = self.site.key(&alternate.href);
                if target_key == own_key {
                    continue;
                }
                let Some(target) = self.by_key.get(&target_key) else {
                    continue;
                };
                let shown = self.site.display(&alternate.href);
                let finding =
                    if target.redirect_loop() || !target.is_ok() || target.navigation_changed() {
                        Finding::new(
                            Rule::HreflangBroken,
                            Some(&record.url),
                            format!(
                                "lists {shown} as its \"{}\" version, but {shown} {}",
                                alternate.hreflang,
                                describe_response(target, &self.site)
                            ),
                        )
                    } else if !target
                        .hreflang
                        .iter()
                        .any(|back| self.site.key(&back.href) == own_key)
                    {
                        Finding::new(
                            Rule::HreflangNoReturn,
                            Some(&record.url),
                            format!(
                            "lists {shown} as its \"{}\" version, but {shown} does not link back",
                            alternate.hreflang
                        ),
                        )
                    } else {
                        continue;
                    };
                findings.push(finding.group(shown));
            }
        }
    }
}

/// Internal link targets with the pages that link to them.
pub(crate) fn linked_from(snapshot: &Snapshot) -> BTreeMap<&str, Vec<String>> {
    let mut linked: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (source, targets) in &snapshot.link_graph {
        for target in targets {
            if target != source {
                linked.entry(target).or_default().push(source.clone());
            }
        }
    }
    linked
}

/// A URL that is broken, loops and is in the sitemap gets one finding
/// instead of three.
fn collapse_same_url(findings: Vec<Finding>) -> Vec<Finding> {
    const ORDER: [Rule; 5] = [
        Rule::HomeError,
        Rule::RedirectLoop,
        Rule::BrokenLink,
        Rule::SitemapUrlError,
        Rule::SitemapUrlRedirect,
    ];
    let mut best: BTreeMap<String, usize> = BTreeMap::new();
    for (index, finding) in findings.iter().enumerate() {
        let (Some(url), Some(rank)) = (
            &finding.url,
            ORDER.iter().position(|rule| *rule == finding.rule),
        ) else {
            continue;
        };
        let keep = match best.get(url) {
            Some(existing) => {
                let existing_rank = ORDER
                    .iter()
                    .position(|rule| *rule == findings[*existing].rule)
                    .unwrap_or(usize::MAX);
                rank < existing_rank
            }
            None => true,
        };
        if keep {
            best.insert(url.clone(), index);
        }
    }

    let mut output = Vec::new();
    let mut follow_on: BTreeMap<usize, Vec<Rule>> = BTreeMap::new();
    for (index, finding) in findings.iter().enumerate() {
        let collapsible = ORDER.contains(&finding.rule);
        match finding.url.as_ref().and_then(|url| best.get(url)) {
            Some(winner) if collapsible && *winner != index => {
                follow_on.entry(*winner).or_default().push(finding.rule);
            }
            _ => output.push(index),
        }
    }
    output
        .into_iter()
        .map(|index| {
            let mut finding = findings[index].clone();
            if let Some(rules) = follow_on.remove(&index) {
                if rules
                    .iter()
                    .any(|rule| matches!(rule, Rule::SitemapUrlError | Rule::SitemapUrlRedirect))
                {
                    finding.message.push_str("; the sitemap also lists it");
                }
                finding.follow_on.extend(rules);
            }
            finding
        })
        .collect()
}
