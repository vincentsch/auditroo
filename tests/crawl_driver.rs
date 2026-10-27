use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use auditroo::crawl::{
    BackendUnavailableReason, CrawlDriver, CrawlDriverConfig, CrawlError, CrawlEvent, CrawlItem,
    CrawlScope, CrawlSeed, CrawlSeedSource, CrawlSkippedReason, CrawlTraversalTruncationReason,
    FetchError, FetchedResponse, Fetcher, ResponseHeaders, RobotsPolicy, ScopePattern,
};
use futures_util::StreamExt;
use url::Url;

#[derive(Default)]
struct FakeFetcher {
    responses: Mutex<BTreeMap<String, Result<FetchedResponse, FetchError>>>,
    delays: Mutex<BTreeMap<String, Duration>>,
    calls: Mutex<Vec<FetchCall>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

#[derive(Debug, Clone)]
struct FetchCall {
    url: String,
    at: Instant,
}

impl FakeFetcher {
    fn with_response(&self, url: &str, response: FetchedResponse) {
        self.responses
            .lock()
            .unwrap()
            .insert(url.to_owned(), Ok(response));
    }

    fn with_error(&self, url: &str, error: FetchError) {
        self.responses
            .lock()
            .unwrap()
            .insert(url.to_owned(), Err(error));
    }

    fn with_delay(&self, url: &str, delay: Duration) {
        self.delays.lock().unwrap().insert(url.to_owned(), delay);
    }

    fn count(&self, url: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.url == url)
            .count()
    }

    fn call_time(&self, url: &str) -> Instant {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .find(|call| call.url == url)
            .unwrap_or_else(|| panic!("fetch call for {url} exists"))
            .at
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    fn record_active(&self, active: usize) {
        let mut current = self.max_active.load(Ordering::SeqCst);
        while active > current {
            match self.max_active.compare_exchange(
                current,
                active,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
}

#[async_trait]
impl Fetcher for FakeFetcher {
    async fn fetch(&self, url: &Url) -> Result<FetchedResponse, FetchError> {
        let key = url.to_string();
        self.calls.lock().unwrap().push(FetchCall {
            url: key.clone(),
            at: Instant::now(),
        });

        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.record_active(active);

        let delay = self.delays.lock().unwrap().get(&key).copied();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }

        let result = self
            .responses
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or_else(|| Ok(response(&key, 404, Some("text/plain"), "not found")));

        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

fn url(input: &str) -> Url {
    Url::parse(input).expect("test URL parses")
}

fn response(
    final_url: &str,
    status: u16,
    content_type: Option<&str>,
    body: &str,
) -> FetchedResponse {
    FetchedResponse {
        final_url: final_url.to_owned(),
        redirects: Vec::new(),
        status,
        headers: ResponseHeaders::new(),
        content_type: content_type.map(str::to_owned),
        body: body.to_owned(),
        rendered: false,
    }
}

fn html(final_url: &str, body: &str) -> FetchedResponse {
    response(final_url, 200, Some("text/html"), body)
}

fn robots(origin: &str, body: &str) -> FetchedResponse {
    response(
        &format!("{origin}/robots.txt"),
        200,
        Some("text/plain"),
        body,
    )
}

fn scope(
    start_url: &Url,
    include: &[&str],
    exclude: &[&str],
    max_depth: usize,
    max_pages: usize,
) -> CrawlScope {
    CrawlScope::new(
        start_url,
        include
            .iter()
            .map(|pattern| ScopePattern::compile(pattern))
            .collect(),
        exclude
            .iter()
            .map(|pattern| ScopePattern::compile(pattern))
            .collect(),
        max_depth,
        max_pages,
    )
}

fn config(
    concurrency: usize,
    rate_limit_ms: u64,
    additional_seeds: Vec<CrawlSeed>,
) -> CrawlDriverConfig {
    CrawlDriverConfig {
        concurrency,
        rate_limit_ms,
        additional_seeds,
    }
}

fn seed(input: &str, source: CrawlSeedSource) -> CrawlSeed {
    CrawlSeed {
        url: url(input),
        source,
    }
}

async fn driver(
    start_url: Url,
    fetcher: Arc<FakeFetcher>,
    scope: CrawlScope,
    respect_robots: bool,
    config: CrawlDriverConfig,
) -> CrawlDriver {
    let policy_fetcher: Arc<dyn Fetcher> = fetcher.clone();
    let robots = Arc::new(RobotsPolicy::load(&start_url, policy_fetcher, respect_robots).await);
    let driver_fetcher: Arc<dyn Fetcher> = fetcher;
    CrawlDriver::new(start_url, driver_fetcher, scope, robots, config).expect("driver builds")
}

async fn collect_ok(driver: &CrawlDriver) -> Vec<CrawlEvent> {
    driver
        .run()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("crawl event succeeds"))
        .collect()
}

fn item_urls(events: &[CrawlEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            CrawlEvent::Item(item) => Some(item.url.clone()),
            CrawlEvent::FetchFailed(_)
            | CrawlEvent::CandidateSkipped(_)
            | CrawlEvent::TraversalTruncated(_) => None,
        })
        .collect()
}

fn skipped_candidates(events: &[CrawlEvent]) -> Vec<(String, CrawlSkippedReason)> {
    events
        .iter()
        .filter_map(|event| match event {
            CrawlEvent::CandidateSkipped(skipped) => Some((skipped.url.clone(), skipped.reason)),
            _ => None,
        })
        .collect()
}

fn truncations(events: &[CrawlEvent]) -> Vec<CrawlTraversalTruncationReason> {
    events
        .iter()
        .filter_map(|event| match event {
            CrawlEvent::TraversalTruncated(reason) => Some(*reason),
            _ => None,
        })
        .collect()
}

fn item<'a>(events: &'a [CrawlEvent], target: &str) -> &'a CrawlItem {
    events
        .iter()
        .find_map(|event| match event {
            CrawlEvent::Item(item) if item.url == target => Some(item),
            _ => None,
        })
        .unwrap_or_else(|| panic!("item for {target} exists"))
}

#[tokio::test]
async fn crawls_linked_site_breadth_first_and_dedupes_cycles() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"
            <html><body>
              <a href="/b">b</a>
              <a href="/a#top">a</a>
              <a href="/excluded">excluded</a>
              <a href="mailto:seo@example.test">mail</a>
              <a href="tel:+123">tel</a>
              <a href="javascript:void(0)">js</a>
              <a href="data:text/plain,hello">data</a>
            </body></html>
            "#,
        ),
    );
    fetcher.with_response(
        "https://example.test/a",
        html("https://example.test/a", r#"<a href="/c">c</a>"#),
    );
    fetcher.with_response(
        "https://example.test/b",
        html(
            "https://example.test/b",
            r#"<a href="/c#frag">c</a><a href="/a">a</a>"#,
        ),
    );
    fetcher.with_response(
        "https://example.test/c",
        html("https://example.test/c", r#"<a href="/">home</a>"#),
    );

    let start = url("https://example.test/");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &["/excluded*"], 0, 0),
        false,
        config(4, 0, Vec::new()),
    )
    .await;

    let events = collect_ok(&driver).await;

    assert_eq!(
        item_urls(&events),
        vec![
            "https://example.test/",
            "https://example.test/a",
            "https://example.test/b",
            "https://example.test/c",
        ]
    );
    assert_eq!(
        item(&events, "https://example.test/").discovered_links,
        vec![
            "https://example.test/a".to_owned(),
            "https://example.test/b".to_owned()
        ]
    );
    assert_eq!(
        item(&events, "https://example.test/b").discovered_links,
        vec![
            "https://example.test/a".to_owned(),
            "https://example.test/c".to_owned()
        ]
    );
    assert_eq!(fetcher.count("https://example.test/c"), 1);
    assert_eq!(fetcher.count("https://example.test/excluded"), 0);
}

#[tokio::test]
async fn backend_unavailable_failure_stops_the_crawl() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"<html><body><a href="/next">next</a></body></html>"#,
        ),
    );
    fetcher.with_error(
        "https://example.test/next",
        FetchError::BackendUnavailable {
            reason: BackendUnavailableReason::CdpDisconnected,
        },
    );

    let driver = driver(
        url("https://example.test/"),
        fetcher,
        scope(&url("https://example.test/"), &[], &[], 0, 0),
        false,
        config(1, 0, Vec::new()),
    )
    .await;
    let events = driver.run().collect::<Vec<_>>().await;

    assert!(matches!(events.first(), Some(Ok(CrawlEvent::Item(_)))));
    assert!(matches!(
        events.get(1),
        Some(Err(CrawlError::BackendUnavailable {
            url,
            error: FetchError::BackendUnavailable {
                reason: BackendUnavailableReason::CdpDisconnected
            }
        })) if url == "https://example.test/next"
    ));
    assert_eq!(events.len(), 2, "{events:#?}");
}

#[tokio::test]
async fn backend_unavailable_failure_cancels_the_current_level() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"<html><body><a href="/a">a</a><a href="/b">b</a></body></html>"#,
        ),
    );
    fetcher.with_error(
        "https://example.test/a",
        FetchError::BackendUnavailable {
            reason: BackendUnavailableReason::CdpDisconnected,
        },
    );
    fetcher.with_delay("https://example.test/b", Duration::from_secs(10));

    let start = url("https://example.test/");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 0),
        false,
        config(2, 0, Vec::new()),
    )
    .await;
    let events = tokio::time::timeout(Duration::from_millis(500), driver.run().collect::<Vec<_>>())
        .await
        .expect("backend failure cancels delayed sibling fetch");

    assert!(matches!(events.first(), Some(Ok(CrawlEvent::Item(_)))));
    assert!(matches!(
        events.get(1),
        Some(Err(CrawlError::BackendUnavailable {
            url,
            error: FetchError::BackendUnavailable {
                reason: BackendUnavailableReason::CdpDisconnected
            }
        })) if url == "https://example.test/a"
    ));
    assert_eq!(events.len(), 2, "{events:#?}");
}

#[tokio::test]
async fn link_discovery_uses_first_valid_base_or_final_url_fallback() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/base",
        response(
            "https://example.test/final/page",
            200,
            None,
            "\u{feff}\n  <!DOCTYPE html><html><head>\
             <base href=\"mailto:seo@example.test\">\
             <base href=\"/root/\">\
             </head><body>\
             <a href=\"child\">child</a>\
             <a href=\"/abs?b=1#frag\">abs</a>\
             <a href=\"//example.test/network\">network</a>\
             <a href=\"https://other.test/out\">out</a>\
             <a href=\"/excluded\">excluded</a>\
             </body></html>",
        ),
    );
    fetcher.with_response(
        "https://example.test/fallback",
        html(
            "https://example.test/final/fallback/page",
            "<html><head>\
             <base href=\"http://[::1\">\
             <base href=\"mailto:seo@example.test\">\
             </head><body><a href=\"child\">child</a></body></html>",
        ),
    );

    let start = url("https://example.test/base");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &["/excluded*"], 0, 2),
        false,
        config(
            2,
            0,
            vec![seed(
                "https://example.test/fallback",
                CrawlSeedSource::Manual,
            )],
        ),
    )
    .await;

    let events = collect_ok(&driver).await;

    assert_eq!(
        item(&events, "https://example.test/base").discovered_links,
        vec![
            "https://example.test/abs?b=1".to_owned(),
            "https://example.test/network".to_owned(),
            "https://example.test/root/child".to_owned(),
        ]
    );
    assert_eq!(
        item(&events, "https://example.test/fallback").discovered_links,
        vec!["https://example.test/final/fallback/child".to_owned()]
    );
}

#[tokio::test]
async fn declared_non_html_and_http_errors_are_still_items() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/missing",
        response(
            "https://example.test/missing",
            404,
            Some("text/html"),
            "<html><body>missing</body></html>",
        ),
    );
    fetcher.with_response(
        "https://example.test/json",
        response(
            "https://example.test/json",
            200,
            Some("application/json"),
            r#"{"html":"<a href='/hidden'>hidden</a>"}"#,
        ),
    );

    let start = url("https://example.test/missing");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 2),
        false,
        config(
            2,
            0,
            vec![seed("https://example.test/json", CrawlSeedSource::Manual)],
        ),
    )
    .await;

    let events = collect_ok(&driver).await;

    let missing = item(&events, "https://example.test/missing");
    assert_eq!(missing.response.status, 404);
    assert!(item(&events, "https://example.test/json")
        .discovered_links
        .is_empty());
}

#[tokio::test]
async fn configuration_errors_are_reported_by_new() {
    let fetcher = Arc::new(FakeFetcher::default());
    let valid_start = url("https://example.test/");
    let policy_fetcher: Arc<dyn Fetcher> = fetcher.clone();
    let robots = Arc::new(RobotsPolicy::load(&valid_start, policy_fetcher, false).await);
    let valid_scope = scope(&valid_start, &[], &[], 0, 0);

    let driver_fetcher: Arc<dyn Fetcher> = fetcher.clone();
    assert!(matches!(
        CrawlDriver::new(
            valid_start.clone(),
            driver_fetcher,
            valid_scope.clone(),
            Arc::clone(&robots),
            config(0, 0, Vec::new()),
        ),
        Err(CrawlError::Configuration { .. })
    ));

    let driver_fetcher: Arc<dyn Fetcher> = fetcher.clone();
    assert!(matches!(
        CrawlDriver::new(
            url("mailto:seo@example.test"),
            driver_fetcher,
            valid_scope.clone(),
            Arc::clone(&robots),
            config(1, 0, Vec::new()),
        ),
        Err(CrawlError::Configuration { .. })
    ));

    let driver_fetcher: Arc<dyn Fetcher> = fetcher;
    assert!(matches!(
        CrawlDriver::new(
            url("https://example.test/other"),
            driver_fetcher,
            valid_scope,
            robots,
            config(1, 0, Vec::new()),
        ),
        Err(CrawlError::Configuration { .. })
    ));
}

#[tokio::test]
async fn robots_disallowed_start_is_fatal_but_disabled_policy_crawls() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/robots.txt",
        robots(
            "https://example.test",
            "User-agent: auditroo\nDisallow: /\n",
        ),
    );
    fetcher.with_response(
        "https://example.test/",
        html("https://example.test/", "<html></html>"),
    );

    let start = url("https://example.test/");
    let denied = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 0),
        true,
        config(1, 0, Vec::new()),
    )
    .await;
    let denied_events = denied.run().collect::<Vec<_>>().await;

    assert!(matches!(
        &denied_events[..],
        [Ok(CrawlEvent::CandidateSkipped(skipped))]
            if skipped.url == "https://example.test/"
                && skipped.reason == CrawlSkippedReason::RobotsDenied
    ));
    assert_eq!(fetcher.count("https://example.test/"), 0);

    let allowed = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 0),
        false,
        config(1, 0, Vec::new()),
    )
    .await;
    let allowed_events = collect_ok(&allowed).await;

    assert_eq!(item_urls(&allowed_events), vec!["https://example.test/"]);
}

#[tokio::test]
async fn robots_filtered_links_remain_discovered_without_consuming_page_budget() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/robots.txt",
        robots(
            "https://example.test",
            "User-agent: auditroo\nDisallow: /aaa-blocked\nAllow: /\n",
        ),
    );
    fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"<a href="/aaa-blocked">blocked</a><a href="/zzz-open">open</a>"#,
        ),
    );
    fetcher.with_response(
        "https://example.test/zzz-open",
        html("https://example.test/zzz-open", "<html></html>"),
    );

    let start = url("https://example.test/");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 2),
        true,
        config(3, 0, Vec::new()),
    )
    .await;

    let events = collect_ok(&driver).await;

    assert_eq!(
        item(&events, "https://example.test/").discovered_links,
        vec![
            "https://example.test/aaa-blocked".to_owned(),
            "https://example.test/zzz-open".to_owned(),
        ]
    );
    assert_eq!(
        item_urls(&events),
        vec!["https://example.test/", "https://example.test/zzz-open"]
    );
    assert_eq!(
        skipped_candidates(&events),
        vec![(
            "https://example.test/aaa-blocked".to_owned(),
            CrawlSkippedReason::RobotsDenied,
        )]
    );
    assert_eq!(fetcher.count("https://example.test/aaa-blocked"), 0);
    assert_eq!(fetcher.count("https://example.test/zzz-open"), 1);
}

#[tokio::test]
async fn page_budget_stops_before_loading_later_origin_robots() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/robots.txt",
        robots("https://example.test", "User-agent: auditroo\nAllow: /\n"),
    );
    fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"
            <a href="/aaa-open">open</a>
            <a href="https://www.example.test/zzz-late">later origin</a>
            "#,
        ),
    );
    fetcher.with_response(
        "https://example.test/aaa-open",
        html("https://example.test/aaa-open", "<html></html>"),
    );

    let start = url("https://example.test/");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 2),
        true,
        config(3, 0, Vec::new()),
    )
    .await;

    let events = collect_ok(&driver).await;

    assert_eq!(
        item(&events, "https://example.test/").discovered_links,
        vec![
            "https://example.test/aaa-open".to_owned(),
            "https://www.example.test/zzz-late".to_owned(),
        ]
    );
    assert_eq!(
        item_urls(&events),
        vec!["https://example.test/", "https://example.test/aaa-open"]
    );
    assert_eq!(
        truncations(&events),
        vec![CrawlTraversalTruncationReason::MaxPagesReached]
    );
    assert_eq!(fetcher.count("https://example.test/aaa-open"), 1);
    assert_eq!(fetcher.count("https://www.example.test/robots.txt"), 0);
    assert_eq!(fetcher.count("https://www.example.test/zzz-late"), 0);
}

#[tokio::test]
async fn www_twin_robots_rules_are_loaded_and_applied_by_driver() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/robots.txt",
        robots("https://example.test", "User-agent: auditroo\nAllow: /\n"),
    );
    fetcher.with_response(
        "https://www.example.test/robots.txt",
        robots(
            "https://www.example.test",
            "User-agent: auditroo\nDisallow: /www-blocked\nAllow: /\n",
        ),
    );
    fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"
            <a href="https://www.example.test/www-blocked">blocked</a>
            <a href="https://www.example.test/www-open">open</a>
            "#,
        ),
    );
    fetcher.with_response(
        "https://www.example.test/www-open",
        html("https://www.example.test/www-open", "<html></html>"),
    );

    let start = url("https://example.test/");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 2),
        true,
        config(3, 0, Vec::new()),
    )
    .await;

    let events = collect_ok(&driver).await;

    assert_eq!(
        item(&events, "https://example.test/").discovered_links,
        vec![
            "https://www.example.test/www-blocked".to_owned(),
            "https://www.example.test/www-open".to_owned(),
        ]
    );
    assert_eq!(
        item_urls(&events),
        vec!["https://example.test/", "https://www.example.test/www-open"]
    );
    assert_eq!(fetcher.count("https://www.example.test/robots.txt"), 1);
    assert_eq!(fetcher.count("https://www.example.test/www-blocked"), 0);
    assert_eq!(fetcher.count("https://www.example.test/www-open"), 1);
}

#[tokio::test]
async fn additional_seeds_are_depth_zero_but_not_start_exempt() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/",
        html("https://example.test/", "<html></html>"),
    );
    fetcher.with_response(
        "https://example.test/allowed-seed",
        html("https://example.test/allowed-seed", "<html></html>"),
    );

    let start = url("https://example.test/");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &["/allowed*"], &[], 0, 0),
        false,
        config(
            4,
            0,
            vec![
                seed("https://example.test/skip", CrawlSeedSource::Manual),
                seed("https://other.test/allowed-other", CrawlSeedSource::Sitemap),
                seed(
                    "https://example.test/allowed-seed",
                    CrawlSeedSource::Sitemap,
                ),
            ],
        ),
    )
    .await;

    let events = collect_ok(&driver).await;

    assert_eq!(
        item_urls(&events),
        vec!["https://example.test/", "https://example.test/allowed-seed"]
    );
    assert_eq!(item(&events, "https://example.test/allowed-seed").depth, 0);
    assert_eq!(fetcher.count("https://example.test/skip"), 0);
    assert_eq!(fetcher.count("https://other.test/allowed-other"), 0);
}

#[tokio::test]
async fn depth_and_page_caps_stop_fetches_without_hiding_edges() {
    let capped_fetcher = Arc::new(FakeFetcher::default());
    capped_fetcher.with_response(
        "https://example.test/",
        html("https://example.test/", r#"<a href="/a">a</a>"#),
    );
    capped_fetcher.with_response(
        "https://example.test/a",
        html("https://example.test/a", "<html></html>"),
    );
    let capped_start = url("https://example.test/");
    let capped_driver = driver(
        capped_start.clone(),
        Arc::clone(&capped_fetcher),
        scope(&capped_start, &[], &[], 0, 1),
        false,
        config(2, 0, Vec::new()),
    )
    .await;

    let capped_events = collect_ok(&capped_driver).await;
    assert_eq!(item_urls(&capped_events), vec!["https://example.test/"]);
    assert_eq!(
        truncations(&capped_events),
        vec![CrawlTraversalTruncationReason::MaxPagesReached]
    );
    assert_eq!(
        item(&capped_events, "https://example.test/").discovered_links,
        vec!["https://example.test/a"]
    );
    assert_eq!(capped_fetcher.count("https://example.test/a"), 0);

    let depth_fetcher = Arc::new(FakeFetcher::default());
    depth_fetcher.with_response(
        "https://example.test/",
        html("https://example.test/", r#"<a href="/level1">one</a>"#),
    );
    depth_fetcher.with_response(
        "https://example.test/level1",
        html(
            "https://example.test/level1",
            r#"<a href="/level2">two</a>"#,
        ),
    );
    let depth_start = url("https://example.test/");
    let depth_driver = driver(
        depth_start.clone(),
        Arc::clone(&depth_fetcher),
        scope(&depth_start, &[], &[], 1, 0),
        false,
        config(2, 0, Vec::new()),
    )
    .await;

    let depth_events = collect_ok(&depth_driver).await;
    assert_eq!(
        item_urls(&depth_events),
        vec!["https://example.test/", "https://example.test/level1"]
    );
    assert_eq!(
        truncations(&depth_events),
        vec![CrawlTraversalTruncationReason::MaxDepthReached]
    );
    assert_eq!(
        item(&depth_events, "https://example.test/level1").discovered_links,
        vec!["https://example.test/level2"]
    );
    assert_eq!(depth_fetcher.count("https://example.test/level2"), 0);
}

#[tokio::test]
async fn non_start_fetch_error_is_recoverable_but_start_error_is_fatal() {
    let recoverable_fetcher = Arc::new(FakeFetcher::default());
    recoverable_fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"<a href="/fail">fail</a><a href="/ok">ok</a>"#,
        ),
    );
    recoverable_fetcher.with_error("https://example.test/fail", FetchError::Timeout);
    recoverable_fetcher.with_response(
        "https://example.test/ok",
        html("https://example.test/ok", "<html></html>"),
    );

    let start = url("https://example.test/");
    let recoverable_driver = driver(
        start.clone(),
        Arc::clone(&recoverable_fetcher),
        scope(&start, &[], &[], 0, 0),
        false,
        config(2, 0, Vec::new()),
    )
    .await;

    let events = recoverable_driver.run().collect::<Vec<_>>().await;
    assert!(
        matches!(&events[0], Ok(CrawlEvent::Item(item)) if item.url == "https://example.test/")
    );
    assert!(matches!(
        &events[1],
        Ok(CrawlEvent::FetchFailed(failure))
            if failure.url == "https://example.test/fail"
                && failure.depth == 1
                && matches!(failure.error, FetchError::Timeout)
    ));
    assert!(
        matches!(&events[2], Ok(CrawlEvent::Item(item)) if item.url == "https://example.test/ok")
    );

    let fatal_fetcher = Arc::new(FakeFetcher::default());
    fatal_fetcher.with_error(
        "https://example.test/",
        FetchError::Request {
            detail: "offline".to_owned(),
        },
    );
    fatal_fetcher.with_response(
        "https://example.test/seed",
        html("https://example.test/seed", "<html></html>"),
    );
    let fatal_driver = driver(
        start.clone(),
        Arc::clone(&fatal_fetcher),
        scope(&start, &[], &[], 0, 0),
        false,
        config(
            2,
            0,
            vec![seed("https://example.test/seed", CrawlSeedSource::Manual)],
        ),
    )
    .await;

    let fatal_events = fatal_driver.run().collect::<Vec<_>>().await;
    assert!(matches!(
        &fatal_events[..],
        [Err(CrawlError::StartFetchFailed { url, error })]
            if url == "https://example.test/"
                && matches!(error, FetchError::Request { .. })
    ));
    assert_eq!(fatal_fetcher.count("https://example.test/seed"), 0);
}

#[tokio::test]
async fn concurrency_is_bounded_and_output_order_ignores_completion_order() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"
            <a href="/d">d</a>
            <a href="/b">b</a>
            <a href="/a">a</a>
            <a href="/c">c</a>
            "#,
        ),
    );
    for path in ["a", "b", "c", "d"] {
        let target = format!("https://example.test/{path}");
        fetcher.with_response(&target, html(&target, "<html></html>"));
        fetcher.with_delay(&target, Duration::from_millis(40));
    }

    let start = url("https://example.test/");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 0),
        false,
        config(2, 0, Vec::new()),
    )
    .await;

    let events = collect_ok(&driver).await;

    assert_eq!(
        item_urls(&events),
        vec![
            "https://example.test/",
            "https://example.test/a",
            "https://example.test/b",
            "https://example.test/c",
            "https://example.test/d",
        ]
    );
    assert!(fetcher.max_active() <= 2);
    assert_eq!(fetcher.max_active(), 2);
}

#[tokio::test]
async fn same_host_rate_limit_separates_top_level_fetch_starts() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"<a href="/a">a</a><a href="/b">b</a>"#,
        ),
    );
    fetcher.with_response(
        "https://example.test/a",
        html("https://example.test/a", "<html></html>"),
    );
    fetcher.with_response(
        "https://example.test/b",
        html("https://example.test/b", "<html></html>"),
    );

    let start = url("https://example.test/");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 0),
        false,
        config(3, 30, Vec::new()),
    )
    .await;

    let events = collect_ok(&driver).await;

    assert_eq!(
        item_urls(&events),
        vec![
            "https://example.test/",
            "https://example.test/a",
            "https://example.test/b",
        ]
    );
    let a_started = fetcher.call_time("https://example.test/a");
    let b_started = fetcher.call_time("https://example.test/b");
    assert!(
        b_started.duration_since(a_started) >= Duration::from_millis(25),
        "expected same-host start gap, got {:?}",
        b_started.duration_since(a_started)
    );
}

#[tokio::test]
async fn requested_identity_is_independent_from_final_redirect_target() {
    let fetcher = Arc::new(FakeFetcher::default());
    fetcher.with_response(
        "https://example.test/",
        html(
            "https://example.test/",
            r#"<a href="/direct">direct</a><a href="/redirect">redirect</a>"#,
        ),
    );
    fetcher.with_response(
        "https://example.test/direct",
        html("https://example.test/direct", "<html></html>"),
    );
    fetcher.with_response(
        "https://example.test/redirect",
        html("https://example.test/direct", "<html></html>"),
    );

    let start = url("https://example.test/");
    let driver = driver(
        start.clone(),
        Arc::clone(&fetcher),
        scope(&start, &[], &[], 0, 0),
        false,
        config(2, 0, Vec::new()),
    )
    .await;

    let events = collect_ok(&driver).await;

    assert_eq!(
        item_urls(&events),
        vec![
            "https://example.test/",
            "https://example.test/direct",
            "https://example.test/redirect",
        ]
    );
    assert_eq!(
        item(&events, "https://example.test/redirect")
            .response
            .final_url,
        "https://example.test/direct"
    );
    assert_eq!(fetcher.count("https://example.test/direct"), 1);
    assert_eq!(fetcher.count("https://example.test/redirect"), 1);
}
