use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use futures_core::Stream;
use futures_util::StreamExt;
use scraper::{Html, Selector};
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

use super::fetch::{FetchError, FetchedResponse, Fetcher};
use super::robots::RobotsPolicy;
use super::scope::CrawlScope;
use super::url::normalize_url;

pub type CrawlEventStream<'a> =
    Pin<Box<dyn Stream<Item = Result<CrawlEvent, CrawlError>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct CrawlDriverConfig {
    pub concurrency: usize,
    pub rate_limit_ms: u64,
    pub additional_seeds: Vec<CrawlSeed>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrawlSeedSource {
    Sitemap,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlSeed {
    pub url: Url,
    pub source: CrawlSeedSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrawlEvent {
    Item(CrawlItem),
    FetchFailed(CrawlFetchFailure),
    CandidateSkipped(CrawlSkippedCandidate),
    TraversalTruncated(CrawlTraversalTruncationReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlSkippedCandidate {
    pub url: String,
    pub reason: CrawlSkippedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlSkippedReason {
    RobotsDenied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlTraversalTruncationReason {
    MaxPagesReached,
    MaxDepthReached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlFetchFailure {
    pub url: String,
    pub depth: usize,
    pub error: FetchError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlItem {
    pub url: String,
    pub depth: usize,
    pub response: FetchedResponse,
    pub discovered_links: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrawlError {
    Configuration { detail: String },
    StartFetchFailed { url: String, error: FetchError },
    BackendUnavailable { url: String, error: FetchError },
}

impl fmt::Display for CrawlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrawlError::Configuration { detail } => {
                write!(formatter, "the crawl settings are not usable: {detail}")
            }
            CrawlError::StartFetchFailed { url, error } => {
                write!(formatter, "could not load the start page {url}: {error}")
            }
            CrawlError::BackendUnavailable { url, error } => {
                write!(formatter, "the crawl stopped at {url}: {error}")
            }
        }
    }
}

impl Error for CrawlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            CrawlError::StartFetchFailed { error, .. }
            | CrawlError::BackendUnavailable { error, .. } => Some(error),
            CrawlError::Configuration { .. } => None,
        }
    }
}

pub struct CrawlDriver {
    start_url: Url,
    fetcher: Arc<dyn Fetcher>,
    scope: CrawlScope,
    robots: Arc<RobotsPolicy>,
    config: CrawlDriverConfig,
}

impl CrawlDriver {
    pub fn new(
        start_url: Url,
        fetcher: Arc<dyn Fetcher>,
        scope: CrawlScope,
        robots: Arc<RobotsPolicy>,
        config: CrawlDriverConfig,
    ) -> Result<Self, CrawlError> {
        if config.concurrency == 0 {
            return Err(config_error("concurrency must be at least 1"));
        }
        if !matches!(start_url.scheme(), "http" | "https") {
            return Err(config_error(format!(
                "the start URL must start with http:// or https://, not {}:",
                start_url.scheme()
            )));
        }
        if start_url.host_str().is_none() {
            return Err(config_error("the start URL has no host name"));
        }

        let normalized_start = normalize_url(&start_url);
        if normalized_start != scope.start_url() {
            return Err(config_error(format!(
                "the start URL {normalized_start:?} is not the crawl scope's start URL {:?}",
                scope.start_url()
            )));
        }

        Ok(Self {
            start_url,
            fetcher,
            scope,
            robots,
            config,
        })
    }

    pub fn run(&self) -> CrawlEventStream<'_> {
        let start_url = self.start_url.clone();
        let fetcher = Arc::clone(&self.fetcher);
        let scope = self.scope.clone();
        let robots = Arc::clone(&self.robots);
        let config = self.config.clone();

        Box::pin(stream! {
            let max_pages = scope.max_pages();
            let rate_limits = Arc::new(Mutex::new(HashMap::new()));
            let runtime = CrawlRuntime {
                scope: &scope,
                robots: Arc::clone(&robots),
                fetcher: Arc::clone(&fetcher),
                max_pages,
                concurrency: config.concurrency,
                rate_limit_ms: config.rate_limit_ms,
                rate_limits: Arc::clone(&rate_limits),
            };
            let mut reserved = BTreeSet::new();
            let mut reserved_count = 0usize;
            let mut observations = CrawlObservationState::default();
            let start_identity = normalize_url(&start_url);

            if !scope.allows_candidate(&start_url, 0) {
                yield Err(config_error("the start URL is outside the crawl scope"));
                return;
            }

            // A blocked start page ends the crawl without an error. robots.txt
            // goes into the snapshot, so the block shows up as a finding.
            if !robots.allows(&start_url).await {
                yield Ok(CrawlEvent::CandidateSkipped(CrawlSkippedCandidate {
                    url: start_identity,
                    reason: CrawlSkippedReason::RobotsDenied,
                }));
                return;
            }

            if !reserve_budget(max_pages, &mut reserved_count) {
                return;
            }
            reserved.insert(start_identity.clone());

            let start_candidate = Candidate {
                normalized_url: start_identity.clone(),
                url: requested_url(&start_url),
                depth: 0,
            };
            let mut next_candidates = BTreeMap::new();
            match fetch_candidate(
                start_candidate,
                Arc::clone(&runtime.fetcher),
                runtime.scope,
                runtime.rate_limit_ms,
                Arc::clone(&rate_limits),
            )
            .await
            {
                FetchOutcome::Fetched {
                    item,
                    discovered_candidates,
                } => {
                    merge_candidates(&mut next_candidates, discovered_candidates);
                    yield Ok(CrawlEvent::Item(item));
                }
                FetchOutcome::Failed(failure) => {
                    if let Some(error) = backend_unavailable_crawl_error(&failure) {
                        yield Err(error);
                        return;
                    }
                    yield Err(CrawlError::StartFetchFailed {
                        url: failure.url,
                        error: failure.error,
                    });
                    return;
                }
            }

            let additional_seeds = sorted_additional_seeds(&config.additional_seeds);
            let additional_results = reserve_and_fetch_level(
                additional_seeds,
                0,
                &runtime,
                &mut reserved,
                &mut reserved_count,
                &mut observations,
            )
            .await;

            if let Some(error) = additional_results.fatal_error {
                yield Err(error);
                return;
            }
            for outcome in additional_results.outcomes {
                match outcome {
                    FetchOutcome::Fetched {
                        item,
                        discovered_candidates,
                    } => {
                        merge_candidates(&mut next_candidates, discovered_candidates);
                        yield Ok(CrawlEvent::Item(item));
                    }
                    FetchOutcome::Failed(failure) => {
                        if let Some(error) = backend_unavailable_crawl_error(&failure) {
                            yield Err(error);
                            return;
                        }
                        yield Ok(CrawlEvent::FetchFailed(failure));
                    }
                }
            }
            for skipped in additional_results.skipped_candidates {
                yield Ok(CrawlEvent::CandidateSkipped(skipped));
            }
            for reason in additional_results.truncations {
                yield Ok(CrawlEvent::TraversalTruncated(reason));
            }

            let mut depth = 1usize;
            let mut current_level = next_candidates;

            while !current_level.is_empty() {
                let level_results = reserve_and_fetch_level(
                    current_level,
                    depth,
                    &runtime,
                    &mut reserved,
                    &mut reserved_count,
                    &mut observations,
                )
                .await;

                if let Some(error) = level_results.fatal_error {
                    yield Err(error);
                    return;
                }
                let mut next_candidates = BTreeMap::new();
                for outcome in level_results.outcomes {
                    match outcome {
                        FetchOutcome::Fetched {
                            item,
                            discovered_candidates,
                        } => {
                            merge_candidates(&mut next_candidates, discovered_candidates);
                            yield Ok(CrawlEvent::Item(item));
                        }
                        FetchOutcome::Failed(failure) => {
                            if let Some(error) = backend_unavailable_crawl_error(&failure) {
                                yield Err(error);
                                return;
                            }
                            yield Ok(CrawlEvent::FetchFailed(failure));
                        }
                    }
                }
                for skipped in level_results.skipped_candidates {
                    yield Ok(CrawlEvent::CandidateSkipped(skipped));
                }
                for reason in level_results.truncations {
                    yield Ok(CrawlEvent::TraversalTruncated(reason));
                }

                depth = depth.saturating_add(1);
                current_level = next_candidates;
            }
        })
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    normalized_url: String,
    url: Url,
    depth: usize,
}

enum FetchOutcome {
    Fetched {
        item: CrawlItem,
        discovered_candidates: BTreeMap<String, Url>,
    },
    Failed(CrawlFetchFailure),
}

impl FetchOutcome {
    fn normalized_url(&self) -> &str {
        match self {
            FetchOutcome::Fetched { item, .. } => &item.url,
            FetchOutcome::Failed(failure) => &failure.url,
        }
    }
}

fn backend_unavailable_crawl_error(failure: &CrawlFetchFailure) -> Option<CrawlError> {
    matches!(failure.error, FetchError::BackendUnavailable { .. }).then(|| {
        CrawlError::BackendUnavailable {
            url: failure.url.clone(),
            error: failure.error.clone(),
        }
    })
}

type HostRateLimits = Arc<Mutex<HashMap<String, Instant>>>;

struct CrawlRuntime<'a> {
    scope: &'a CrawlScope,
    robots: Arc<RobotsPolicy>,
    fetcher: Arc<dyn Fetcher>,
    max_pages: usize,
    concurrency: usize,
    rate_limit_ms: u64,
    rate_limits: HostRateLimits,
}

#[derive(Default)]
struct CrawlObservationState {
    robots_denied: BTreeSet<String>,
    emitted_max_pages: bool,
    emitted_max_depth: bool,
}

struct LevelResults {
    outcomes: Vec<FetchOutcome>,
    skipped_candidates: Vec<CrawlSkippedCandidate>,
    truncations: Vec<CrawlTraversalTruncationReason>,
    fatal_error: Option<CrawlError>,
}

async fn reserve_and_fetch_level(
    candidates: BTreeMap<String, Url>,
    depth: usize,
    runtime: &CrawlRuntime<'_>,
    reserved: &mut BTreeSet<String>,
    reserved_count: &mut usize,
    observations: &mut CrawlObservationState,
) -> LevelResults {
    let mut selected = Vec::new();
    let mut skipped_candidates = Vec::new();
    let mut truncations = Vec::new();

    for (normalized_url, url) in candidates {
        if reserved.contains(&normalized_url) {
            continue;
        }
        if !runtime.scope.allows_candidate(&url, depth) {
            if depth_exceeds_scope(runtime.scope, &url, depth) {
                push_truncation_once(
                    observations,
                    &mut truncations,
                    CrawlTraversalTruncationReason::MaxDepthReached,
                );
            }
            continue;
        }
        if budget_full(runtime.max_pages, *reserved_count) {
            push_truncation_once(
                observations,
                &mut truncations,
                CrawlTraversalTruncationReason::MaxPagesReached,
            );
            break;
        }
        if !runtime.robots.allows(&url).await {
            if observations.robots_denied.insert(normalized_url.clone()) {
                skipped_candidates.push(CrawlSkippedCandidate {
                    url: normalized_url,
                    reason: CrawlSkippedReason::RobotsDenied,
                });
            }
            continue;
        }
        if !reserve_budget(runtime.max_pages, reserved_count) {
            push_truncation_once(
                observations,
                &mut truncations,
                CrawlTraversalTruncationReason::MaxPagesReached,
            );
            break;
        }
        reserved.insert(normalized_url.clone());
        selected.push(Candidate {
            normalized_url,
            url,
            depth,
        });
    }

    let mut outcomes = Vec::new();
    let mut fatal_error = None;
    let mut pending = futures_util::stream::iter(selected.into_iter().map(|candidate| {
        let fetcher = Arc::clone(&runtime.fetcher);
        let rate_limits = Arc::clone(&runtime.rate_limits);
        async move {
            fetch_candidate(
                candidate,
                fetcher,
                runtime.scope,
                runtime.rate_limit_ms,
                rate_limits,
            )
            .await
        }
    }))
    .buffer_unordered(runtime.concurrency);
    while let Some(outcome) = pending.next().await {
        if let FetchOutcome::Failed(failure) = &outcome {
            if let Some(error) = backend_unavailable_crawl_error(failure) {
                fatal_error = Some(error);
                break;
            }
        }
        outcomes.push(outcome);
    }
    drop(pending);
    outcomes.sort_by(|a, b| a.normalized_url().cmp(b.normalized_url()));
    skipped_candidates.sort_by(|left, right| left.url.cmp(&right.url));
    if fatal_error.is_some() {
        outcomes.clear();
        skipped_candidates.clear();
        truncations.clear();
    }
    LevelResults {
        outcomes,
        skipped_candidates,
        truncations,
        fatal_error,
    }
}

fn depth_exceeds_scope(scope: &CrawlScope, url: &Url, depth: usize) -> bool {
    scope.max_depth() != 0 && depth > scope.max_depth() && scope.allows_discovered_link(url)
}

fn push_truncation_once(
    observations: &mut CrawlObservationState,
    truncations: &mut Vec<CrawlTraversalTruncationReason>,
    reason: CrawlTraversalTruncationReason,
) {
    let already_emitted = match reason {
        CrawlTraversalTruncationReason::MaxPagesReached => &mut observations.emitted_max_pages,
        CrawlTraversalTruncationReason::MaxDepthReached => &mut observations.emitted_max_depth,
    };
    if !*already_emitted {
        *already_emitted = true;
        truncations.push(reason);
    }
}

async fn fetch_candidate(
    candidate: Candidate,
    fetcher: Arc<dyn Fetcher>,
    scope: &CrawlScope,
    rate_limit_ms: u64,
    rate_limits: HostRateLimits,
) -> FetchOutcome {
    wait_for_host_turn(&candidate.url, rate_limit_ms, rate_limits).await;

    match fetcher.fetch(&candidate.url).await {
        Ok(response) => {
            let discovered = discover_links(&response, scope);
            FetchOutcome::Fetched {
                item: CrawlItem {
                    url: candidate.normalized_url,
                    depth: candidate.depth,
                    response,
                    discovered_links: discovered.normalized_links,
                },
                discovered_candidates: discovered.candidates,
            }
        }
        Err(error) => FetchOutcome::Failed(CrawlFetchFailure {
            url: candidate.normalized_url,
            depth: candidate.depth,
            error,
        }),
    }
}

async fn wait_for_host_turn(url: &Url, rate_limit_ms: u64, rate_limits: HostRateLimits) {
    if rate_limit_ms == 0 {
        return;
    }

    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return;
    };
    let delay = Duration::from_millis(rate_limit_ms);

    loop {
        let sleep_for = {
            let mut limits = rate_limits.lock().await;
            let now = Instant::now();
            let next_allowed = limits.get(&host).copied().unwrap_or(now);
            if now >= next_allowed {
                limits.insert(host.clone(), now + delay);
                None
            } else {
                Some(next_allowed - now)
            }
        };

        match sleep_for {
            Some(duration) => tokio::time::sleep(duration).await,
            None => return,
        }
    }
}

struct DiscoveredLinks {
    normalized_links: Vec<String>,
    candidates: BTreeMap<String, Url>,
}

fn discover_links(response: &FetchedResponse, scope: &CrawlScope) -> DiscoveredLinks {
    let mut candidates = BTreeMap::new();

    if !is_html_response(response) {
        return DiscoveredLinks {
            normalized_links: Vec::new(),
            candidates,
        };
    }

    // A page that redirected to another site is not part of this one, so
    // its links are not followed.
    let final_url = match Url::parse(&response.final_url) {
        Ok(url) if scope.is_same_site(&url) => url,
        _ => {
            return DiscoveredLinks {
                normalized_links: Vec::new(),
                candidates,
            }
        }
    };

    let document = Html::parse_document(&response.body);
    let base = select_document_base(&document, &final_url);
    let anchor_selector = Selector::parse("a[href]").expect("static anchor selector parses");

    for anchor in document.select(&anchor_selector) {
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Ok(resolved) = base.join(href) else {
            continue;
        };
        let resolved = scope.localize(resolved);
        if !scope.allows_discovered_link(&resolved) {
            continue;
        }

        let normalized = normalize_url(&resolved);
        candidates
            .entry(normalized)
            .or_insert_with(|| requested_url(&resolved));
    }
    let normalized_links = candidates.keys().cloned().collect();

    // Canonical and hreflang targets are fetched too, so the checks can tell
    // whether they work. They are not links, so they stay out of the graph.
    let link_selector = Selector::parse("link[href]").expect("static link selector parses");
    for link in document.select(&link_selector) {
        let rel = link
            .value()
            .attr("rel")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let wanted = rel
            .split_ascii_whitespace()
            .any(|token| token == "canonical")
            || (rel
                .split_ascii_whitespace()
                .any(|token| token == "alternate")
                && link.value().attr("hreflang").is_some());
        if !wanted {
            continue;
        }
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Ok(resolved) = base.join(href) else {
            continue;
        };
        let resolved = scope.localize(resolved);
        if scope.allows_discovered_link(&resolved) {
            candidates
                .entry(normalize_url(&resolved))
                .or_insert_with(|| requested_url(&resolved));
        }
    }

    DiscoveredLinks {
        normalized_links,
        candidates,
    }
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

fn is_html_response(response: &FetchedResponse) -> bool {
    match response.content_type.as_deref() {
        Some("text/html" | "application/xhtml+xml") => true,
        Some(_) => false,
        None => sniff_html_prefix(&response.body),
    }
}

fn sniff_html_prefix(body: &str) -> bool {
    let trimmed = body.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}');
    let prefix = trimmed
        .chars()
        .take(32)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    prefix.starts_with("<!doctype html") || prefix.starts_with("<html")
}

fn is_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
}

fn sorted_additional_seeds(seeds: &[CrawlSeed]) -> BTreeMap<String, Url> {
    seeds
        .iter()
        .map(|seed| (normalize_url(&seed.url), requested_url(&seed.url)))
        .collect()
}

fn merge_candidates(target: &mut BTreeMap<String, Url>, source: BTreeMap<String, Url>) {
    for (normalized_url, url) in source {
        target.entry(normalized_url).or_insert(url);
    }
}

fn reserve_budget(max_pages: usize, reserved_count: &mut usize) -> bool {
    if budget_full(max_pages, *reserved_count) {
        return false;
    }
    *reserved_count += 1;
    true
}

fn budget_full(max_pages: usize, reserved_count: usize) -> bool {
    max_pages != 0 && reserved_count >= max_pages
}

fn config_error(detail: impl Into<String>) -> CrawlError {
    CrawlError::Configuration {
        detail: detail.into(),
    }
}

fn requested_url(url: &Url) -> Url {
    let mut without_fragment = url.clone();
    without_fragment.set_fragment(None);
    without_fragment
}
