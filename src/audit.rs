//! The steps every command shares: crawl a site into a snapshot, then turn
//! one or two snapshots into a report.

use std::collections::BTreeMap;
#[cfg(feature = "browser")]
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use time::OffsetDateTime;
use url::Url;

use crate::adapters::{evaluate_quality, AdapterFinding, BudgetComparison};
use crate::assertions::{
    evaluate_assertions, extract_custom_values, AssertionFinding, CustomExtraction,
};
use crate::config::{canonical_observation_patterns, ResolvedSettings};
use crate::crawl::{
    normalize_url, CrawlDriver, CrawlDriverConfig, CrawlEvent, CrawlScope, CrawlSeed,
    CrawlSeedSource, CrawlSkippedReason, CrawlTraversalTruncationReason, FetchError, Fetcher,
    HttpFetcher, HttpFetcherConfig, RobotsPolicy, ScopePattern,
};
#[cfg(feature = "browser")]
use crate::crawl::{BrowserFetcher, BrowserFetcherConfig, BrowserSession};
use crate::error::CliError;
use crate::external::apply_impact;
use crate::extract::extract;
use crate::findings::{Finding, Rule, Severity};
use crate::graph::LinkGraphAccumulator;
use crate::progress::Progress;
use crate::report::{Report, ReportCommand, ReportInput};
use crate::rules::{check::check, diff::diff, start_issue};
use crate::sitemap::{
    discover_sitemaps_from_urls, sitemap_seeds, SitemapDiscoveryConfig, SitemapDocumentDiagnostic,
    SitemapDocumentOutcome,
};
use crate::snapshot::{
    assemble, CrawlObservation, CrawlSkipReason, CrawlTruncationReason, Snapshot, SnapshotInput,
};

/// What to crawl besides what the start page links to.
#[derive(Debug, Clone, Default)]
pub struct CrawlExtras {
    /// More URLs to fetch, usually the pages of a baseline.
    pub seeds: Vec<Url>,
    /// The live sites this crawl stands in for, when it crawls a preview or
    /// a local build. URLs on those hosts, for example in canonical tags,
    /// are read on the crawled host instead.
    pub production: Vec<Url>,
}

impl CrawlExtras {
    /// Seeds and production mapping for a crawl that is compared with
    /// `baseline`: the baseline's host, and the hosts it stood in for.
    pub fn compared_with(baseline: &Snapshot, current_start: &Url) -> Self {
        Self {
            seeds: baseline_seeds(baseline, current_start),
            production: std::iter::once(&baseline.start_url)
                .chain(&baseline.production_urls)
                .filter_map(|url| Url::parse(url).ok())
                .collect(),
        }
    }
}

/// Crawl a site and build its snapshot.
pub async fn crawl(
    start_url: Url,
    settings: &ResolvedSettings,
    extras: CrawlExtras,
    progress: &mut Progress,
    label: &str,
) -> Result<Snapshot, CliError> {
    let http_fetcher = Arc::new(HttpFetcher::new(HttpFetcherConfig {
        user_agent: settings.crawl.user_agent.clone(),
        headers: settings
            .crawl
            .headers
            .iter()
            .map(|header| (header.name.clone(), header.value.clone()))
            .collect(),
        header_origin: Some(start_url.clone()),
        timeout_ms: settings.crawl.timeout_ms,
        max_body_bytes: settings.crawl.max_body_bytes,
        max_redirects: HttpFetcherConfig::default().max_redirects,
    })?);
    let mut scope = CrawlScope::new(
        &start_url,
        compile(&settings.crawl.include),
        compile(&settings.crawl.exclude),
        settings.crawl.max_depth,
        settings.crawl.max_pages,
    );
    // The live site from --production-url or auditroo.toml counts for every
    // crawl, next to the baseline's hosts.
    let named = settings
        .crawl
        .production_url
        .as_deref()
        .and_then(|url| Url::parse(url).ok());
    let mut production: Vec<Url> = Vec::new();
    for url in extras.production.into_iter().chain(named) {
        if !scope.is_same_site(&url) && !production.contains(&url) {
            scope = scope.with_production(&url);
            production.push(url);
        }
    }
    let page_backend =
        select_page_fetch_backend(&start_url, settings, Arc::clone(&http_fetcher)).await?;

    progress.start(label, start_url.as_ref());
    let robots_fetcher: Arc<dyn Fetcher> = http_fetcher.clone();
    let robots = Arc::new(
        RobotsPolicy::load(&start_url, robots_fetcher, settings.crawl.respect_robots).await,
    );
    let sitemap_roots = if settings.crawl.sitemaps.is_empty() {
        robots.sitemap_urls()
    } else {
        settings.crawl.sitemaps.clone()
    };
    let discovery = discover_sitemaps_from_urls(
        &start_url,
        http_fetcher.as_ref(),
        sitemap_roots,
        &scope,
        SitemapDiscoveryConfig {
            rate_limit_ms: settings.crawl.rate_limit_ms,
            ..SitemapDiscoveryConfig::default()
        },
    )
    .await;
    let mut seeds = sitemap_seeds(&discovery, &scope)
        .into_iter()
        .filter(|seed| seed.source == CrawlSeedSource::Sitemap)
        .collect::<Vec<_>>();
    seeds.extend(extras.seeds.into_iter().map(|url| CrawlSeed {
        url,
        source: CrawlSeedSource::Manual,
    }));

    let driver = CrawlDriver::new(
        start_url.clone(),
        Arc::clone(&page_backend.fetcher),
        scope,
        Arc::clone(&robots),
        CrawlDriverConfig {
            concurrency: settings.crawl.concurrency,
            rate_limit_ms: settings.crawl.rate_limit_ms,
            additional_seeds: seeds,
        },
    )?;

    let mut records = Vec::new();
    let mut graph = LinkGraphAccumulator::new();
    let mut failures = Vec::new();
    let mut custom_values = BTreeMap::new();
    let mut observation = observation_from_settings(settings);
    let mut events = driver.run();
    let mut crawl_error = None;
    while let Some(event) = events.next().await {
        match event {
            Ok(CrawlEvent::Item(item)) => {
                let extracted = extract_custom_values(&item, &settings.extractors);
                let record = extract(&item);
                graph.observe(&item, &record);
                if !extracted.is_empty() {
                    custom_values.insert(record.url.clone(), extracted);
                }
                records.push(record);
            }
            Ok(CrawlEvent::FetchFailed(failure)) => failures.push(failure),
            Ok(CrawlEvent::CandidateSkipped(skipped)) => {
                let reason = match skipped.reason {
                    CrawlSkippedReason::RobotsDenied => CrawlSkipReason::RobotsDenied,
                };
                observation
                    .skipped_candidates
                    .entry(skipped.url)
                    .or_default()
                    .push(reason);
            }
            Ok(CrawlEvent::TraversalTruncated(reason)) => {
                observation.global_truncation_reasons.push(match reason {
                    CrawlTraversalTruncationReason::MaxPagesReached => {
                        CrawlTruncationReason::MaxPagesReached
                    }
                    CrawlTraversalTruncationReason::MaxDepthReached => {
                        CrawlTruncationReason::MaxDepthReached
                    }
                });
            }
            Err(error) => {
                crawl_error = Some(error);
                break;
            }
        }
        progress.update(records.len(), failures.len());
    }
    drop(events);
    drop(driver);

    let shutdown_result = page_backend.shutdown().await;
    if let Some(error) = crawl_error {
        return Err(error.into());
    }
    shutdown_result?;
    progress.finish(records.len(), failures.len());

    let custom_extraction = (!settings.extractors.is_empty()).then(|| CustomExtraction {
        catalog: settings.extractors.clone(),
        values: custom_values,
    });
    let snapshot = assemble(
        SnapshotInput {
            start_url: normalize_url(&start_url),
            production_urls: production.iter().map(normalize_url).collect(),
            crawl_observation: observation,
            robots: robots.files(),
            records,
            sitemap: discovery,
            link_graph: graph.finish(),
            crawl_failures: failures,
        },
        OffsetDateTime::now_utc(),
        custom_extraction,
    )
    .map_err(|error| CliError::runtime(format!("could not build the snapshot: {error}")))?;
    Ok(snapshot)
}

fn compile(patterns: &[String]) -> Vec<ScopePattern> {
    patterns
        .iter()
        .map(|pattern| ScopePattern::compile(pattern))
        .collect()
}

/// The baseline's pages on the current host, so the current crawl fetches
/// every page the baseline had, even when nothing links to it any more.
pub fn baseline_seeds(baseline: &Snapshot, current_start: &Url) -> Vec<Url> {
    let Ok(baseline_start) = Url::parse(&baseline.start_url) else {
        return Vec::new();
    };
    baseline
        .records
        .keys()
        .filter_map(|url| Url::parse(url).ok())
        .filter(|url| {
            url.host_str() == baseline_start.host_str()
                && url.scheme() == baseline_start.scheme()
                && url.port() == baseline_start.port()
        })
        .map(|url| {
            let mut rebased = current_start.clone();
            rebased.set_path(url.path());
            rebased.set_query(url.query());
            rebased.set_fragment(None);
            rebased
        })
        .collect()
}

pub(crate) fn observation_from_settings(settings: &ResolvedSettings) -> CrawlObservation {
    CrawlObservation {
        include: canonical_observation_patterns(&settings.crawl.include),
        exclude: canonical_observation_patterns(&settings.crawl.exclude),
        max_depth: settings.crawl.max_depth,
        max_pages: settings.crawl.max_pages,
        respect_robots: settings.crawl.respect_robots,
        render_js: settings.crawl.render_js,
        global_truncation_reasons: Vec::new(),
        skipped_candidates: BTreeMap::new(),
    }
}

/// Check that a baseline can be compared with a crawl that uses `settings`.
/// Settings that change what a page looks like must match; settings that
/// only change how much is crawled may differ and are mentioned in a note.
pub fn compare_settings(
    baseline: &Snapshot,
    settings: &ResolvedSettings,
) -> Result<Vec<String>, CliError> {
    let observed = &baseline.crawl_observation;
    let mut notes = Vec::new();
    if observed.render_js != settings.crawl.render_js {
        let (was, now) = if observed.render_js {
            ("on", "off")
        } else {
            ("off", "on")
        };
        let fix = if observed.render_js {
            "Add --render-js"
        } else {
            "Remove --render-js (and render_js in auditroo.toml)"
        };
        return Err(CliError::usage(format!(
            "the baseline was crawled with JavaScript rendering {was}, and this run has it {now}. Rendered and raw HTML differ too much to compare. {fix}, or take a new baseline with the same setting"
        )));
    }
    let include = canonical_observation_patterns(&settings.crawl.include);
    let exclude = canonical_observation_patterns(&settings.crawl.exclude);
    if observed.include != include || observed.exclude != exclude {
        notes.push(format!(
            "The baseline used different --include/--exclude rules ({} / {}) than this run ({} / {}). Pages outside this run's rules are not compared.",
            list_or_none(&observed.include),
            list_or_none(&observed.exclude),
            list_or_none(&include),
            list_or_none(&exclude)
        ));
    }
    if observed.max_pages != settings.crawl.max_pages
        || observed.max_depth != settings.crawl.max_depth
    {
        notes.push(format!(
            "The baseline used --max-pages {} and --max-depth {}; this run uses {} and {}.",
            observed.max_pages,
            observed.max_depth,
            settings.crawl.max_pages,
            settings.crawl.max_depth
        ));
    }
    if observed.respect_robots != settings.crawl.respect_robots {
        notes.push(if settings.crawl.respect_robots {
            "The baseline was crawled with --ignore-robots and this run follows robots.txt."
                .to_owned()
        } else {
            "This run ignores robots.txt for crawling (--ignore-robots); the baseline followed it."
                .to_owned()
        });
    }
    Ok(notes)
}

fn list_or_none(patterns: &[String]) -> String {
    if patterns.is_empty() {
        "none".to_owned()
    } else {
        patterns.join(", ")
    }
}

/// Why the start page gives auditroo nothing to check, or `None` when it
/// loaded as an HTML page of the site.
pub fn start_problem(snapshot: &Snapshot) -> Option<String> {
    let start = snapshot.records.get(&snapshot.start_url)?;
    start_issue(start, &snapshot.site()).map(|issue| format!("the start page {issue}"))
}

/// A crawl that robots.txt stopped at the start page only makes sense to
/// report when Googlebot is also blocked. Otherwise robots.txt only blocks
/// auditroo, and nothing was checked.
pub fn ensure_crawled(snapshot: &Snapshot) -> Result<(), CliError> {
    if !snapshot.start_blocked_by_robots() {
        return Ok(());
    }
    let googlebot_allowed = match (snapshot.start_robots(), Url::parse(&snapshot.start_url)) {
        (Some(file), Ok(start)) => file.allows("Googlebot", &start),
        _ => true,
    };
    if googlebot_allowed {
        return Err(CliError::usage(format!(
            "robots.txt on {} blocks auditroo but not Googlebot, so nothing was crawled. Run with --ignore-robots to crawl your own site anyway",
            snapshot.start_url
        )));
    }
    Ok(())
}

/// Check that a crawl can serve as the side every other page is compared
/// with. `what` names it for the message, for example "production".
pub fn ensure_comparable(snapshot: &Snapshot, what: &str) -> Result<(), CliError> {
    ensure_crawled(snapshot)?;
    match start_problem(snapshot) {
        Some(problem) => Err(CliError::runtime(format!(
            "{what} at {} cannot be used for the comparison, because {problem}. If a firewall or a login blocks auditroo, docs/own-site.md shows how to let it in",
            snapshot.start_url
        ))),
        None => Ok(()),
    }
}

/// The origin that most pages name in their canonical tag, when it is not
/// part of the site. That is either a build with the wrong site URL, or a
/// local build or preview that auditroo was not told about.
pub fn foreign_canonical_origin(snapshot: &Snapshot) -> Option<String> {
    let site = snapshot.site();
    let pages: Vec<_> = snapshot
        .pages()
        .filter(|record| record.is_ok() && !record.navigation_changed() && !record.has_noindex())
        .collect();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for record in &pages {
        if let Some(canonical) = record.canonical.as_deref() {
            if !site.contains(canonical) {
                *counts.entry(origin_of(canonical)).or_default() += 1;
            }
        }
    }
    let (origin, count) = counts.into_iter().max_by_key(|(_, count)| *count)?;
    (count >= 2 && count * 2 >= pages.len()).then_some(origin)
}

/// A note for crawls whose pages name another host as canonical.
pub fn foreign_canonical_note(origin: &str) -> String {
    format!(
        "Most pages name {origin} in their canonical tag, so auditroo does not count them as indexable here. If this is a local build or a preview of {origin}, run auditroo with --production-url {origin}, so its URLs count as this site's own. If not, the build uses the wrong site URL."
    )
}

/// Notes about one crawl that a reader should know.
pub fn crawl_notes(snapshot: &Snapshot) -> Vec<String> {
    let mut notes = Vec::new();
    let site = snapshot.site();
    let mut elsewhere: BTreeMap<String, Vec<&SitemapDocumentDiagnostic>> = BTreeMap::new();
    for document in &snapshot.sitemap.discovery.documents {
        if !site.contains(&document.requested_url) {
            elsewhere
                .entry(origin_of(&document.requested_url))
                .or_default()
                .push(document);
        }
    }
    for (origin, documents) in elsewhere {
        let read = documents.iter().any(|document| {
            matches!(
                document.outcome,
                SitemapDocumentOutcome::ParsedUrlSet | SitemapDocumentOutcome::ParsedSitemapIndex
            )
        });
        if read {
            notes.push(format!(
                "A sitemap is on {origin}, so auditroo fetched it there. Google accepts a sitemap on another host when robots.txt names it."
            ));
        } else {
            let reason = match (&documents[0].detail, documents[0].final_status) {
                (Some(detail), _) => detail.clone(),
                (None, Some(status)) => format!("it returns {status}"),
                (None, None) => "it could not be loaded".to_owned(),
            };
            let end = if reason.ends_with(['.', '?']) {
                ""
            } else {
                "."
            };
            notes.push(format!(
                "A sitemap is on {origin}, but auditroo could not read it there: {reason}{end}"
            ));
        }
    }
    if let Some(start) = snapshot.records.get(&snapshot.start_url) {
        if start.navigation_changed() && !site.contains(&start.final_url) {
            notes.push(format!(
                "The start URL redirects to {}, which is outside the crawl. Start from that URL to check the site.",
                start.final_url
            ));
        }
    }
    if snapshot.truncated() {
        notes.push(format!(
            "The crawl stopped at the page limit ({} pages, --max-pages) or depth limit (--max-depth), so some pages were not checked.",
            snapshot.crawl_observation.max_pages
        ));
    }
    let denied = snapshot
        .crawl_observation
        .skipped_candidates
        .iter()
        .filter(|(url, reasons)| {
            *url != &snapshot.start_url && reasons.contains(&CrawlSkipReason::RobotsDenied)
        })
        .count();
    if denied > 0 {
        notes.push(format!(
            "robots.txt blocked auditroo from {denied} URL{}. Use --ignore-robots to also check {}.",
            if denied == 1 { "" } else { "s" },
            if denied == 1 { "it" } else { "them" }
        ));
    }
    notes
}

fn origin_of(url: &str) -> String {
    Url::parse(url)
        .map(|url| crate::crawl::robots::origin_key(&url))
        .unwrap_or_else(|_| url.to_owned())
}

/// Inputs for turning snapshots into a report.
pub struct Evaluation<'a> {
    pub command: ReportCommand,
    pub baseline: Option<&'a Snapshot>,
    pub current: &'a Snapshot,
    pub settings: &'a ResolvedSettings,
    pub notes: Vec<String>,
}

/// Run the rules, custom checks and quality budgets, then decide whether the
/// run fails.
pub async fn evaluate(evaluation: Evaluation<'_>) -> Result<Report, CliError> {
    let Evaluation {
        command,
        baseline,
        current,
        settings,
        mut notes,
    } = evaluation;

    let (mut findings, not_compared) = match baseline {
        Some(baseline) => {
            let outcome = diff(baseline, current);
            notes.extend(outcome.notes);
            (outcome.findings, outcome.not_compared)
        }
        None => (check(current), 0),
    };
    for note in crawl_notes(current) {
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
    // Two builds that both name the same other host are builds of that
    // site. A preview that newly names one is a wrong site URL, and the
    // findings say so, so no hint there.
    if let Some(origin) = foreign_canonical_origin(current) {
        let shared = baseline.is_none_or(|baseline| {
            foreign_canonical_origin(baseline).as_deref() == Some(origin.as_str())
        });
        if shared {
            notes.push(foreign_canonical_note(&origin));
        }
    }

    let assertion_findings = evaluate_assertions(
        baseline.unwrap_or(current),
        current,
        &settings.extractors,
        &settings.assertions,
    )?;
    findings.extend(assertion_findings.into_iter().map(assertion_finding));

    let quality = evaluate_quality(&settings.quality).await?;
    let quality_summaries = quality.enabled.then(|| quality.source_summaries.clone());
    for adapter in quality.adapter_findings {
        let mut finding = adapter_finding(adapter);
        if !quality.gate {
            finding.severity = Severity::Info;
        }
        findings.push(finding);
    }

    let mut report = Report::new(ReportInput {
        command,
        current,
        baseline,
        findings,
        notes,
        gate: &settings.gate,
    });
    report.quality = quality_summaries;
    report.not_compared = not_compared;
    apply_impact(
        &mut report,
        baseline.unwrap_or(current),
        current,
        &settings.impact,
    )?;
    report.finish();
    Ok(report)
}

fn assertion_finding(finding: AssertionFinding) -> Finding {
    let mut converted = Finding::new(
        Rule::Assertion,
        Some(&finding.url),
        format!(
            "fails the custom check {}: {}",
            finding.assertion_id, finding.failure_reason
        ),
    )
    .severity(finding.severity)
    .group(finding.assertion_id.clone());
    converted.check_id = Some(finding.assertion_id);
    if !finding.evidence.is_empty() {
        converted = converted.detail(format!("found: {}", finding.evidence.join(" | ")));
    }
    converted
}

fn adapter_finding(finding: AdapterFinding) -> Finding {
    let comparison = match finding.expected.comparison {
        BudgetComparison::Min => "at least",
        BudgetComparison::Max => "at most",
    };
    let unit = if finding.expected.unit.is_empty() || finding.expected.unit == "score" {
        String::new()
    } else {
        format!(" {}", finding.expected.unit)
    };
    let mut converted = Finding::new(
        Rule::QualityBudget,
        Some(&finding.url),
        format!(
            "{} is {}{unit}; the budget {} allows {comparison} {}{unit}",
            finding.metric_id, finding.observed, finding.budget_id, finding.expected.threshold
        ),
    )
    .severity(finding.severity)
    .group(finding.budget_id.clone());
    converted.check_id = Some(finding.budget_id);
    converted
}

struct PageFetchBackend {
    fetcher: Arc<dyn Fetcher>,
    #[cfg(feature = "browser")]
    session: Option<BrowserSession>,
}

impl PageFetchBackend {
    async fn shutdown(self) -> Result<(), FetchError> {
        #[cfg(feature = "browser")]
        {
            if let Some(session) = self.session {
                session.shutdown().await?;
            }
        }
        Ok(())
    }
}

#[cfg(not(feature = "browser"))]
async fn select_page_fetch_backend(
    _start_url: &Url,
    settings: &ResolvedSettings,
    http_fetcher: Arc<HttpFetcher>,
) -> Result<PageFetchBackend, CliError> {
    if settings.crawl.render_js {
        return Err(CliError::usage(
            "--render-js needs an auditroo binary built with the browser feature. Release binaries do not have it. Build one with `cargo install --locked --features browser --git https://github.com/vincentsch/auditroo`, as docs/javascript.md explains",
        ));
    }
    Ok(PageFetchBackend {
        fetcher: http_fetcher,
    })
}

#[cfg(feature = "browser")]
async fn select_page_fetch_backend(
    start_url: &Url,
    settings: &ResolvedSettings,
    http_fetcher: Arc<HttpFetcher>,
) -> Result<PageFetchBackend, CliError> {
    if !settings.crawl.render_js {
        return Ok(PageFetchBackend {
            fetcher: http_fetcher,
            session: None,
        });
    }

    let executable = std::env::var_os("AUDITROO_CHROME").map(PathBuf::from);
    let (fetcher, session) = BrowserFetcher::launch(BrowserFetcherConfig {
        start_url: start_url.clone(),
        user_agent: settings.crawl.user_agent.clone(),
        headers: settings
            .crawl
            .headers
            .iter()
            .map(|header| (header.name.clone(), header.value.clone()))
            .collect(),
        timeout_ms: settings.crawl.timeout_ms,
        max_body_bytes: settings.crawl.max_body_bytes,
        chrome_executable: executable,
    })
    .await
    .map_err(|error| {
        CliError::usage(format!(
            "could not start Chrome or Chromium for --render-js ({error}). Install it, or set AUDITROO_CHROME to its path"
        ))
    })?;

    Ok(PageFetchBackend {
        fetcher: Arc::new(fetcher),
        session: Some(session),
    })
}
