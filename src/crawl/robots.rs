//! robots.txt handling for the crawl and for the robots checks.
//!
//! The crawler asks [`RobotsPolicy::allows`] before it fetches a page. The
//! policy downloads the robots.txt of each origin the crawl visits, also when
//! the crawl ignores robots.txt, because the files go into the snapshot. The
//! checks later test each page against its own origin's file for Googlebot,
//! Bingbot and the AI crawlers.
//!
//! `User-agent` lines are read like Google's parser reads them: only the
//! leading `[A-Za-z_-]` characters count, so `Googlebot/2.1` and
//! `Googlebot*` both name Googlebot.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use texting_robots::Robot;
use tokio::sync::OnceCell;
use url::Url;

use super::fetch::{FetchedResponse, Fetcher};
use super::url::path_and_query;

/// Google reads at most 500 KiB of a robots.txt file. auditroo stores a
/// little more.
const MAX_STORED_ROBOTS_BYTES: usize = 512 * 1024;

/// The robots.txt file of one origin as the crawler saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotsFile {
    pub url: String,
    /// HTTP status, or `None` when the request itself failed.
    pub status: Option<u16>,
    /// File content for a 200 response, cut at 512 KiB.
    pub body: Option<String>,
    /// Why the file could not be read, when the request failed.
    pub error: Option<String>,
}

impl RobotsFile {
    /// Whether `agent` may fetch `url` according to this file.
    pub fn allows(&self, agent: &str, url: &Url) -> bool {
        AgentRules::new(self, agent).allows(url)
    }

    /// Whether Google stops crawling the site because of this file.
    ///
    /// Google treats a 5xx or 429 status as "the whole site is disallowed"
    /// until the file loads again, and a failed request the same way. Any
    /// other 4xx counts as no robots.txt at all, so everything is allowed.
    pub fn blocks_everything_by_error(&self) -> bool {
        match self.status {
            Some(status) => status == 429 || (500..600).contains(&status),
            None => true,
        }
    }
}

/// The parsed rules for one crawler, so a check over many URLs parses the
/// file once.
pub struct AgentRules {
    robot: Option<Robot>,
    allow_all: bool,
}

impl AgentRules {
    pub fn new(file: &RobotsFile, agent: &str) -> Self {
        match (file.status, &file.body) {
            (Some(200), Some(body)) => {
                let robot = Robot::new(agent, google_user_agents(body).as_bytes()).ok();
                Self {
                    allow_all: robot.is_none(),
                    robot,
                }
            }
            _ => Self {
                robot: None,
                allow_all: !file.blocks_everything_by_error(),
            },
        }
    }

    pub fn allows(&self, url: &Url) -> bool {
        match &self.robot {
            Some(robot) => robot.allowed(&path_and_query(url)),
            None => self.allow_all,
        }
    }
}

enum RobotsDecision {
    AllowAll,
    Rules(Box<Robot>),
}

struct OriginRobots {
    decision: RobotsDecision,
    file: RobotsFile,
    sitemaps: Vec<String>,
}

/// Decides whether auditroo may fetch a URL. It loads the robots.txt of each
/// origin once, the first time the crawl needs it.
pub struct RobotsPolicy {
    fetcher: Arc<dyn Fetcher>,
    respect_robots: bool,
    start_origin: String,
    cache: Arc<Mutex<HashMap<String, Arc<OnceCell<OriginRobots>>>>>,
}

impl fmt::Debug for RobotsPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cache_len = self
            .cache
            .lock()
            .map(|cache| cache.len())
            .unwrap_or_default();
        formatter
            .debug_struct("RobotsPolicy")
            .field("respect_robots", &self.respect_robots)
            .field("start_origin", &self.start_origin)
            .field("loaded_origins", &cache_len)
            .finish_non_exhaustive()
    }
}

impl Clone for RobotsPolicy {
    fn clone(&self) -> Self {
        Self {
            fetcher: Arc::clone(&self.fetcher),
            respect_robots: self.respect_robots,
            start_origin: self.start_origin.clone(),
            cache: Arc::clone(&self.cache),
        }
    }
}

impl RobotsPolicy {
    /// Load the start origin's robots.txt. The file is fetched also when
    /// `respect_robots` is false, because it goes into the snapshot.
    pub async fn load(
        start_url: &Url,
        fetcher: Arc<dyn Fetcher>,
        respect_robots: bool,
    ) -> RobotsPolicy {
        let start_origin = origin_key(start_url);
        let policy = RobotsPolicy {
            fetcher,
            respect_robots,
            start_origin: start_origin.clone(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        };

        let cell = policy.cell_for(&start_origin);
        cell.get_or_init(|| policy.fetch_origin(start_origin)).await;

        policy
    }

    /// Whether auditroo may fetch `url`. The origin's robots.txt is loaded
    /// on first use, also when the crawl ignores robots.txt, because the
    /// Googlebot checks need every file in the snapshot.
    pub async fn allows(&self, url: &Url) -> bool {
        let origin = origin_key(url);
        let cell = self.cell_for(&origin);
        let state = cell.get_or_init(|| self.fetch_origin(origin)).await;
        if !self.respect_robots {
            return true;
        }

        match &state.decision {
            RobotsDecision::AllowAll => true,
            RobotsDecision::Rules(robot) => robot.allowed(&path_and_query(url)),
        }
    }

    /// `Sitemap:` lines from every robots.txt file loaded so far.
    pub fn sitemap_urls(&self) -> Vec<String> {
        let cache = self.cache.lock().unwrap();
        let urls: BTreeSet<String> = cache
            .values()
            .filter_map(|cell| cell.get())
            .flat_map(|state| state.sitemaps.iter().cloned())
            .collect();
        urls.into_iter().collect()
    }

    /// Every robots.txt file loaded so far, keyed by origin.
    pub fn files(&self) -> BTreeMap<String, RobotsFile> {
        let cache = self.cache.lock().unwrap();
        cache
            .iter()
            .filter_map(|(origin, cell)| Some((origin.clone(), cell.get()?.file.clone())))
            .collect()
    }

    /// The start origin's robots.txt as it was fetched.
    pub fn start_file(&self) -> RobotsFile {
        let cache = self.cache.lock().unwrap();
        cache
            .get(&self.start_origin)
            .and_then(|cell| cell.get())
            .map(|state| state.file.clone())
            .unwrap_or_else(|| RobotsFile {
                url: format!("{}/robots.txt", self.start_origin),
                status: None,
                body: None,
                error: Some("robots.txt was not loaded".to_owned()),
            })
    }

    fn cell_for(&self, origin: &str) -> Arc<OnceCell<OriginRobots>> {
        let mut cache = self.cache.lock().unwrap();
        cache
            .entry(origin.to_owned())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone()
    }

    async fn fetch_origin(&self, origin: String) -> OriginRobots {
        let robots_url_text = format!("{origin}/robots.txt");
        let robots_url = match Url::parse(&robots_url_text) {
            Ok(url) => url,
            Err(error) => {
                return failed(
                    &robots_url_text,
                    format!("the robots.txt URL is not valid ({error})"),
                );
            }
        };

        match self.fetcher.fetch(&robots_url).await {
            Ok(response) => map_response(&robots_url, response),
            Err(error) => failed(&robots_url_text, error.to_string()),
        }
    }
}

fn map_response(robots_url: &Url, response: FetchedResponse) -> OriginRobots {
    let status = response.status;
    if status != 200 {
        // A 4xx (missing) file allows everything. Google treats 5xx and 429
        // as a full block. auditroo crawls anyway and reports the block as a
        // finding, so the rest of the site is still checked.
        return OriginRobots {
            decision: RobotsDecision::AllowAll,
            file: RobotsFile {
                url: robots_url.to_string(),
                status: Some(status),
                body: None,
                error: None,
            },
            sitemaps: Vec::new(),
        };
    }

    let mut body = response.body;
    if body.len() > MAX_STORED_ROBOTS_BYTES {
        let mut cut = MAX_STORED_ROBOTS_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    let sitemaps = parse_sitemaps(&body, robots_url);
    let decision = match Robot::new("auditroo", google_user_agents(&body).as_bytes()) {
        Ok(robot) => RobotsDecision::Rules(Box::new(robot)),
        Err(_) => RobotsDecision::AllowAll,
    };
    OriginRobots {
        decision,
        file: RobotsFile {
            url: robots_url.to_string(),
            status: Some(200),
            body: Some(body),
            error: None,
        },
        sitemaps,
    }
}

fn failed(url: &str, error: String) -> OriginRobots {
    OriginRobots {
        decision: RobotsDecision::AllowAll,
        file: RobotsFile {
            url: url.to_owned(),
            status: None,
            body: None,
            error: Some(error),
        },
        sitemaps: Vec::new(),
    }
}

/// Rewrite `User-agent` values the way Google reads them: only the leading
/// `[A-Za-z_-]` characters stay. `Googlebot/2.1` and `Googlebot*` become
/// `Googlebot`, and a value that starts with `*` stays `*`.
fn google_user_agents(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let Some(rest) = strip_prefix_ci(trimmed, "user-agent") else {
            out.push_str(line);
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix(':') else {
            out.push_str(line);
            continue;
        };
        let value = value.trim_start();
        let token: String = if value.starts_with('*') {
            "*".to_owned()
        } else {
            value
                .chars()
                .take_while(|ch| ch.is_ascii_alphabetic() || *ch == '-' || *ch == '_')
                .collect()
        };
        out.push_str("User-agent: ");
        out.push_str(&token);
        out.push('\n');
    }
    out
}

pub(crate) fn origin_key(url: &Url) -> String {
    use std::fmt::Write as _;

    let mut key = String::new();
    key.push_str(url.scheme());
    key.push_str("://");
    if let Some(host) = url.host_str() {
        key.push_str(&host.to_ascii_lowercase());
    }
    if let Some(port) = url.port() {
        let _ = write!(key, ":{port}");
    }
    key
}

fn parse_sitemaps(body: &str, robots_url: &Url) -> Vec<String> {
    let mut urls = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = strip_prefix_ci(trimmed, "sitemap:") else {
            continue;
        };
        let value = rest.trim();
        if value.is_empty() {
            continue;
        }

        if Url::parse(value).is_ok() {
            urls.push(value.to_owned());
        } else if let Ok(resolved) = robots_url.join(value) {
            urls.push(resolved.to_string());
        }
    }
    urls
}

fn strip_prefix_ci<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    match input.get(..prefix.len()) {
        Some(head) if head.eq_ignore_ascii_case(prefix) => Some(&input[prefix.len()..]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawl::fetch::{FetchError, ResponseHeaders};
    use async_trait::async_trait;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct FakeFetcher {
        responses: Mutex<BTreeMap<String, Result<FetchedResponse, FetchError>>>,
        counts: Mutex<BTreeMap<String, usize>>,
    }

    impl FakeFetcher {
        fn with_response(self: &Arc<Self>, url: &str, response: FetchedResponse) {
            self.responses
                .lock()
                .unwrap()
                .insert(url.to_owned(), Ok(response));
        }

        fn with_error(self: &Arc<Self>, url: &str, error: FetchError) {
            self.responses
                .lock()
                .unwrap()
                .insert(url.to_owned(), Err(error));
        }

        fn count(&self, url: &str) -> usize {
            self.counts.lock().unwrap().get(url).copied().unwrap_or(0)
        }
    }

    #[async_trait]
    impl Fetcher for FakeFetcher {
        async fn fetch(&self, url: &Url) -> Result<FetchedResponse, FetchError> {
            let key = url.to_string();
            *self.counts.lock().unwrap().entry(key.clone()).or_insert(0) += 1;
            self.responses
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Ok(response(&key, 404, "")))
        }
    }

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL parses")
    }

    fn response(final_url: &str, status: u16, body: &str) -> FetchedResponse {
        FetchedResponse {
            final_url: final_url.to_owned(),
            redirects: Vec::new(),
            status,
            headers: ResponseHeaders::new(),
            content_type: Some("text/plain".to_owned()),
            body: body.to_owned(),
            rendered: false,
        }
    }

    fn robots_url(origin: &str) -> String {
        format!("{origin}/robots.txt")
    }

    async fn policy(
        start_url: &str,
        fetcher: Arc<FakeFetcher>,
        respect_robots: bool,
    ) -> RobotsPolicy {
        RobotsPolicy::load(&url(start_url), fetcher as Arc<dyn Fetcher>, respect_robots).await
    }

    #[tokio::test]
    async fn applies_auditroo_rules_and_allows_other_paths() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: auditroo\nDisallow: /private\nAllow: /\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), true).await;

        assert!(!policy.allows(&url("https://example.test/private")).await);
        assert!(policy.allows(&url("https://example.test/public")).await);
    }

    #[tokio::test]
    async fn fragments_do_not_affect_dollar_anchored_rules() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: auditroo\nDisallow: /private$\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), true).await;

        assert!(
            !policy
                .allows(&url("https://example.test/private#section"))
                .await
        );
        assert!(
            policy
                .allows(&url("https://example.test/private/page#section"))
                .await
        );
    }

    #[tokio::test]
    async fn auditroo_group_has_precedence_over_star_group() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: *\nDisallow: /\n\nUser-agent: AuditRoo\nAllow: /\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), true).await;

        assert!(policy.allows(&url("https://example.test/private")).await);
    }

    #[tokio::test]
    async fn star_group_is_fallback_and_unmatched_files_allow_all() {
        let star_fetcher = Arc::new(FakeFetcher::default());
        star_fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: *\nDisallow: /\n",
            ),
        );
        let star = policy("https://example.test/", Arc::clone(&star_fetcher), true).await;
        assert!(!star.allows(&url("https://example.test/anything")).await);

        let unrelated_fetcher = Arc::new(FakeFetcher::default());
        unrelated_fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: OtherBot\nDisallow: /\n",
            ),
        );
        let unrelated = policy(
            "https://example.test/",
            Arc::clone(&unrelated_fetcher),
            true,
        )
        .await;
        assert!(
            unrelated
                .allows(&url("https://example.test/anything"))
                .await
        );
    }

    #[tokio::test]
    async fn crawl_delay_does_not_change_allow_decision() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: auditroo\nCrawl-delay: 999\nDisallow: /private\nAllow: /\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), true).await;

        assert!(!policy.allows(&url("https://example.test/private")).await);
        assert!(policy.allows(&url("https://example.test/public")).await);
    }

    #[tokio::test]
    async fn www_and_apex_origins_are_loaded_separately() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: auditroo\nDisallow: /apex\n",
            ),
        );
        fetcher.with_response(
            &robots_url("https://www.example.test"),
            response(
                &robots_url("https://www.example.test"),
                200,
                "User-agent: auditroo\nDisallow: /www\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), true).await;

        assert!(!policy.allows(&url("https://example.test/apex")).await);
        assert!(policy.allows(&url("https://example.test/www")).await);
        assert!(!policy.allows(&url("https://www.example.test/www")).await);
        assert!(policy.allows(&url("https://www.example.test/apex")).await);
        assert_eq!(fetcher.count(&robots_url("https://example.test")), 1);
        assert_eq!(fetcher.count(&robots_url("https://www.example.test")), 1);
    }

    #[tokio::test]
    async fn same_origin_is_fetched_once_across_repeated_allows() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: auditroo\nAllow: /\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), true).await;
        assert!(policy.allows(&url("https://example.test/a")).await);
        assert!(policy.allows(&url("https://example.test/b")).await);

        assert_eq!(fetcher.count(&robots_url("https://example.test")), 1);
    }

    #[tokio::test]
    async fn lazy_origin_is_fetched_once_for_concurrent_allows() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(&robots_url("https://example.test"), 404, ""),
        );
        fetcher.with_response(
            &robots_url("https://www.example.test"),
            response(
                &robots_url("https://www.example.test"),
                200,
                "User-agent: auditroo\nAllow: /\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), true).await;
        let first_url = url("https://www.example.test/a");
        let second_url = url("https://www.example.test/b");
        let (first, second) = tokio::join!(policy.allows(&first_url), policy.allows(&second_url),);

        assert!(first);
        assert!(second);
        assert_eq!(fetcher.count(&robots_url("https://www.example.test")), 1);
    }

    #[tokio::test]
    async fn cloned_policy_shares_loaded_origin_cache() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(&robots_url("https://example.test"), 404, ""),
        );
        fetcher.with_response(
            &robots_url("https://www.example.test"),
            response(
                &robots_url("https://www.example.test"),
                200,
                "User-agent: auditroo\nAllow: /\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), true).await;
        let cloned = policy.clone();

        assert!(policy.allows(&url("https://www.example.test/a")).await);
        assert!(cloned.allows(&url("https://www.example.test/b")).await);
        assert_eq!(fetcher.count(&robots_url("https://www.example.test")), 1);
        assert_eq!(cloned.start_file(), policy.start_file());
    }

    #[tokio::test]
    async fn sitemap_urls_are_resolved_union_deduped_and_sorted() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: auditroo\nAllow: /\nSitemap: https://cdn.example/sitemap.xml\nSitemap: /relative.xml\n",
            ),
        );
        fetcher.with_response(
            &robots_url("https://www.example.test"),
            response(
                &robots_url("https://www.example.test"),
                200,
                "User-agent: auditroo\nAllow: /\nSitemap: /relative.xml\nSitemap: https://cdn.example/sitemap.xml\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), true).await;
        assert!(policy.allows(&url("https://www.example.test/")).await);

        assert_eq!(
            policy.sitemap_urls(),
            vec![
                "https://cdn.example/sitemap.xml".to_owned(),
                "https://example.test/relative.xml".to_owned(),
                "https://www.example.test/relative.xml".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn missing_client_error_failed_terminal_redirect_and_fetch_error_are_allow_all() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://missing.test"),
            response(&robots_url("https://missing.test"), 404, "missing"),
        );
        fetcher.with_response(
            &robots_url("https://unauthorized.test"),
            response(
                &robots_url("https://unauthorized.test"),
                401,
                "unauthorized",
            ),
        );
        fetcher.with_response(
            &robots_url("https://forbidden.test"),
            response(&robots_url("https://forbidden.test"), 403, "forbidden"),
        );
        fetcher.with_response(
            &robots_url("https://busy.test"),
            response(&robots_url("https://busy.test"), 503, "busy"),
        );
        fetcher.with_response(
            &robots_url("https://redirect.test"),
            response(&robots_url("https://redirect.test"), 301, ""),
        );
        fetcher.with_error(
            &robots_url("https://error.test"),
            FetchError::Request {
                detail: "connection refused".to_owned(),
            },
        );

        let policy = policy("https://missing.test/", Arc::clone(&fetcher), true).await;
        assert!(
            policy
                .allows(&url("https://unauthorized.test/private"))
                .await
        );
        assert!(policy.allows(&url("https://forbidden.test/private")).await);
        assert!(policy.allows(&url("https://busy.test/private")).await);
        assert!(policy.allows(&url("https://redirect.test/private")).await);
        assert!(policy.allows(&url("https://error.test/private")).await);

        let start = policy.start_file();
        assert_eq!(start.status, Some(404));
        assert!(start.allows("Googlebot", &url("https://missing.test/private")));
    }

    #[tokio::test]
    async fn unexpected_success_and_informational_statuses_are_failed_allow_all() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://no-content.test"),
            response(&robots_url("https://no-content.test"), 204, ""),
        );
        fetcher.with_response(
            &robots_url("https://processing.test"),
            response(&robots_url("https://processing.test"), 102, ""),
        );

        let policy = policy("https://no-content.test/", Arc::clone(&fetcher), true).await;
        assert!(policy.allows(&url("https://processing.test/private")).await);

        assert_eq!(policy.start_file().status, Some(204));
    }

    #[tokio::test]
    async fn ignored_robots_still_loads_the_start_file_and_its_sitemaps() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: auditroo\nDisallow: /\nSitemap: /listed.xml\n",
            ),
        );

        let policy = policy("https://example.test/", Arc::clone(&fetcher), false).await;

        assert!(policy.allows(&url("https://example.test/private")).await);
        assert_eq!(fetcher.count(&robots_url("https://example.test")), 1);
        assert_eq!(
            policy.sitemap_urls(),
            vec!["https://example.test/listed.xml".to_owned()]
        );
        let file = policy.start_file();
        assert_eq!(file.status, Some(200));
        assert!(!file.allows("auditroo", &url("https://example.test/private")));
        assert!(file.allows("Googlebot", &url("https://example.test/private")));
    }

    #[test]
    fn stored_file_follows_google_status_rules() {
        let page = url("https://example.test/page");
        let file = |status: Option<u16>, body: Option<&str>| RobotsFile {
            url: "https://example.test/robots.txt".to_owned(),
            status,
            body: body.map(str::to_owned),
            error: None,
        };
        assert!(file(Some(404), None).allows("Googlebot", &page));
        assert!(file(Some(403), None).allows("Googlebot", &page));
        assert!(!file(Some(503), None).allows("Googlebot", &page));
        assert!(!file(None, None).allows("Googlebot", &page));
        // Google treats 429 like a server error, not like a missing file.
        assert!(!file(Some(429), None).allows("Googlebot", &page));
        assert!(file(Some(429), None).blocks_everything_by_error());
        assert!(!file(Some(404), None).blocks_everything_by_error());

        let rules = file(
            Some(200),
            Some("User-agent: Googlebot\nDisallow: /\n\nUser-agent: *\nAllow: /\n"),
        );
        assert!(!rules.allows("Googlebot", &page));
        assert!(rules.allows("Bingbot", &page));
        assert!(!AgentRules::new(&rules, "googlebot").allows(&page));
        assert!(AgentRules::new(&rules, "GPTBot").allows(&page));
    }

    #[test]
    fn user_agent_values_match_the_way_google_reads_them() {
        let page = url("https://example.test/page");
        for line in [
            "User-agent: Googlebot*",
            "User-agent: Googlebot/2.1",
            "user-agent:googlebot",
            "User-Agent : Googlebot # the main crawler",
        ] {
            let rules = RobotsFile {
                url: "https://example.test/robots.txt".to_owned(),
                status: Some(200),
                body: Some(format!("{line}\nDisallow: /\n\nUser-agent: *\nAllow: /\n")),
                error: None,
            };
            assert!(!rules.allows("Googlebot", &page), "{line}");
            assert!(rules.allows("Bingbot", &page), "{line}");
        }
        // Googlebot-Image is another crawler, not Googlebot.
        let image = RobotsFile {
            url: "https://example.test/robots.txt".to_owned(),
            status: Some(200),
            body: Some("User-agent: Googlebot-Image\nDisallow: /\n".to_owned()),
            error: None,
        };
        assert!(image.allows("Googlebot", &page));
        assert!(!image.allows("Googlebot-Image", &page));
    }

    #[tokio::test]
    async fn every_visited_origin_keeps_its_file_also_when_robots_is_ignored() {
        let fetcher = Arc::new(FakeFetcher::default());
        fetcher.with_response(
            &robots_url("https://example.test"),
            response(
                &robots_url("https://example.test"),
                200,
                "User-agent: *\nAllow: /\n",
            ),
        );
        fetcher.with_response(
            &robots_url("https://www.example.test"),
            response(
                &robots_url("https://www.example.test"),
                200,
                "User-agent: Googlebot\nDisallow: /\n",
            ),
        );
        let policy = policy("https://example.test/", Arc::clone(&fetcher), false).await;
        assert!(policy.allows(&url("https://www.example.test/a")).await);

        let files = policy.files();
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec!["https://example.test", "https://www.example.test"]
        );
        assert!(!files["https://www.example.test"]
            .allows("Googlebot", &url("https://www.example.test/a")));
    }

    #[test]
    fn origin_keys_use_effective_port_and_preserve_www() {
        assert_eq!(
            origin_key(&url("HTTPS://EXAMPLE.TEST:443/a")),
            "https://example.test"
        );
        assert_eq!(
            origin_key(&url("https://example.test:443/a")),
            "https://example.test"
        );
        assert_eq!(
            origin_key(&url("https://www.example.test:8443/a")),
            "https://www.example.test:8443"
        );
        assert_eq!(origin_key(&url("http://[::1]:8080/a")), "http://[::1]:8080");
    }
}
