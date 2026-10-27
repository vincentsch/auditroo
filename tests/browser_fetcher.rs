#![cfg(feature = "browser")]

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use auditroo::crawl::{
    normalize_url, BrowserFetcher, BrowserFetcherConfig, CrawlDriver, CrawlDriverConfig,
    CrawlEvent, CrawlItem, CrawlScope, Fetcher, HttpFetcher, HttpFetcherConfig, RobotsPolicy,
    ScopePattern,
};
use auditroo::extract::{extract, navigation_changed};
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{HeaderName, CONTENT_TYPE, LOCATION};
use axum::http::{HeaderValue, Response, StatusCode, Uri};
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use tokio::net::TcpListener;
use url::Url;

// Each fixture starts its own Chromium. The tests take turns, so a small CI
// machine does not run several browsers at once.
static BROWSER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone)]
struct RenderServerState {
    origin: String,
    paths: Arc<Mutex<Vec<String>>>,
}

struct RenderServer {
    state: RenderServerState,
}

impl RenderServer {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test server binds");
        let addr = listener.local_addr().expect("test server has local addr");
        let origin = format!("http://{addr}");
        let state = RenderServerState {
            origin,
            paths: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/{*path}", any(handle))
            .with_state(state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test server runs");
        });

        Self { state }
    }

    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("{}{}", self.state.origin, path)).expect("test URL parses")
    }

    fn paths(&self) -> Vec<String> {
        self.state.paths.lock().unwrap().clone()
    }
}

async fn handle(State(state): State<RenderServerState>, uri: Uri) -> Response<Body> {
    state.paths.lock().unwrap().push(uri.path().to_owned());
    match uri.path() {
        "/robots.txt" => text(200, "text/plain", "User-agent: *\nAllow: /\n"),
        "/redirect" => redirect(&format!("{}/page", state.origin)),
        "/page" => text(200, "text/html", rendered_fixture(&state.origin)),
        "/hash" => text(200, "text/html", hash_navigation_fixture()),
        "/cross-start" => text_with_header(
            200,
            "text/html",
            "x-page-generation",
            "initial",
            cross_document_start_fixture(),
        ),
        "/cross-final" => text_with_header(
            201,
            "text/html; charset=utf-8",
            "x-page-generation",
            "final",
            "<!doctype html><html><head><title>Final Document</title></head><body></body></html>",
        ),
        _ => text(404, "text/plain", "not found"),
    }
}

fn text(status: u16, content_type: &str, body: impl Into<String>) -> Response<Body> {
    let mut response = Response::new(Body::from(body.into()));
    *response.status_mut() = StatusCode::from_u16(status).expect("status is valid");
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("content type is valid"),
    );
    response
}

fn text_with_header(
    status: u16,
    content_type: &str,
    name: &str,
    value: &str,
    body: impl Into<String>,
) -> Response<Body> {
    let mut response = text(status, content_type, body);
    response.headers_mut().insert(
        HeaderName::from_bytes(name.as_bytes()).expect("header name is valid"),
        HeaderValue::from_str(value).expect("header value is valid"),
    );
    response
}

fn redirect(location: &str) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(
        LOCATION,
        HeaderValue::from_str(location).expect("location is valid"),
    );
    response
}

fn cross_document_start_fixture() -> &'static str {
    r#"
    <!doctype html>
    <html>
      <head>
        <title>Initial Document</title>
        <script>
          window.addEventListener('load', () => {
            setTimeout(() => {
              window.location.href = '/cross-final';
            }, 75);
          });
        </script>
      </head>
      <body></body>
    </html>
    "#
}

fn hash_navigation_fixture() -> &'static str {
    r#"
    <!doctype html>
    <html>
      <head>
        <title>Initial Hash Document</title>
        <script>
          window.addEventListener('load', () => {
            setTimeout(() => {
              document.title = 'Hash Ready';
              window.location.hash = 'ready';
            }, 75);
          });
        </script>
      </head>
      <body></body>
    </html>
    "#
}

fn rendered_fixture(origin: &str) -> String {
    format!(
        r#"
        <!doctype html>
        <html>
          <head>
            <title>Plain Title</title>
            <script>
              window.addEventListener('load', () => {{
                setTimeout(() => {{
                  document.title = 'Rendered Title';
                  const canonical = document.createElement('link');
                  canonical.rel = 'canonical';
                  canonical.href = '{origin}/canonical-rendered';
                  document.head.appendChild(canonical);

                  const alternate = document.createElement('link');
                  alternate.rel = 'alternate';
                  alternate.hreflang = 'en-US';
                  alternate.href = '{origin}/en/rendered';
                  document.head.appendChild(alternate);

                  const structured = document.createElement('script');
                  structured.type = 'application/ld+json';
                  structured.textContent = JSON.stringify({{
                    '@context': 'https://schema.org',
                    '@type': 'WebPage'
                  }});
                  document.head.appendChild(structured);

                  const link = document.createElement('a');
                  link.href = '{origin}/rendered-link';
                  link.textContent = 'Rendered link';
                  document.body.appendChild(link);

                  history.pushState({{}}, '', '/client-state');
                }}, 75);
              }});
            </script>
          </head>
          <body><h1>Plain heading</h1></body>
        </html>
        "#
    )
}

async fn maybe_browser_fetcher(
    start_url: Url,
) -> Option<(BrowserFetcher, auditroo::crawl::BrowserSession)> {
    browser_with_config(BrowserFetcherConfig {
        start_url,
        user_agent: None,
        headers: Vec::new(),
        timeout_ms: 10_000,
        max_body_bytes: 1_048_576,
        chrome_executable: None,
    })
    .await
}

async fn browser_with_config(
    mut config: BrowserFetcherConfig,
) -> Option<(BrowserFetcher, auditroo::crawl::BrowserSession)> {
    let chrome_executable = std::env::var_os("AUDITROO_CHROME").map(PathBuf::from);
    config.chrome_executable = chrome_executable;
    match BrowserFetcher::launch(config).await {
        Ok(pair) => Some(pair),
        Err(error) if std::env::var_os("AUDITROO_REQUIRE_BROWSER_TESTS").is_some() => {
            panic!("browser test setup failed: {error}")
        }
        Err(error) => {
            eprintln!("skipping browser integration test: {error}");
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_timeout_cleans_context_and_allows_the_next_page() {
    let _browser_guard = BROWSER_TEST_LOCK.lock().await;
    use std::time::Duration;
    use support::{FixtureResponse, FixtureSite};
    let site = FixtureSite::spawn().await;
    site.set("/slow", 200, "text/html", "<script>localStorage.setItem('fixture', 'secret')</script><script src='/slow.js'></script>").await;
    site.set_response(
        "/slow.js",
        FixtureResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/javascript".into())],
            body: Vec::new(),
            delay: Duration::from_secs(4),
        },
    )
    .await;
    site.set(
        "/fast",
        200,
        "text/html",
        "<script>document.title = localStorage.getItem('fixture') || 'Fresh context'</script>",
    )
    .await;
    let Some((fetcher, session)) = browser_with_config(BrowserFetcherConfig {
        start_url: site.url("/slow"),
        timeout_ms: 1_500,
        ..BrowserFetcherConfig::default()
    })
    .await
    else {
        return;
    };
    let timed_out = fetcher.fetch(&site.url("/slow")).await;
    let recovered = fetcher.fetch(&site.url("/fast")).await;
    session.shutdown().await.expect("browser shuts down");
    assert!(matches!(
        timed_out,
        Err(auditroo::crawl::FetchError::Timeout)
    ));
    let recovered = recovered.expect("backend remains usable after timeout");
    assert!(recovered.body.contains("<title>Fresh context</title>"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_secret_headers_are_stripped_on_other_origins_and_subresources() {
    let _browser_guard = BROWSER_TEST_LOCK.lock().await;
    use support::FixtureSite;
    let origin = FixtureSite::spawn().await;
    let other = FixtureSite::spawn().await;
    other
        .set(
            "/foreign.js",
            200,
            "text/javascript",
            "document.title = 'Loaded foreign script'",
        )
        .await;
    other
        .set("/landing", 200, "text/html", "<title>Other origin</title>")
        .await;
    origin
        .set(
            "/page",
            200,
            "text/html",
            &format!(
                "<script src='{}'></script><script src='/same.js'></script>",
                other.url("/foreign.js")
            ),
        )
        .await;
    origin
        .set(
            "/same.js",
            200,
            "text/javascript",
            "document.body.dataset.ready = 'yes'",
        )
        .await;
    origin
        .redirect("/redirect", other.url("/landing").as_str())
        .await;
    let Some((fetcher, session)) = browser_with_config(BrowserFetcherConfig {
        start_url: origin.url("/page"),
        headers: vec![("x-fixture-secret".into(), "synthetic-private-value".into())],
        ..BrowserFetcherConfig::default()
    })
    .await
    else {
        return;
    };
    let page = fetcher.fetch(&origin.url("/page")).await;
    let redirected = fetcher.fetch(&origin.url("/redirect")).await;
    session.shutdown().await.expect("browser shuts down");
    assert!(page.unwrap().body.contains("Loaded foreign script"));
    assert_eq!(
        redirected.unwrap().final_url,
        other.url("/landing").as_str()
    );
    for path in ["/page", "/same.js", "/redirect"] {
        assert!(origin
            .requests()
            .await
            .iter()
            .any(|request| request.path == path
                && request.header_value("x-fixture-secret").as_deref()
                    == Some("synthetic-private-value")));
    }
    for path in ["/foreign.js", "/landing"] {
        assert!(other
            .requests()
            .await
            .iter()
            .any(|request| request.path == path
                && request.header_value("x-fixture-secret").is_none()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_fetcher_preserves_hash_navigation_in_final_url() {
    let _browser_guard = BROWSER_TEST_LOCK.lock().await;
    let server = RenderServer::spawn().await;
    let start = server.url("/hash");
    let Some((fetcher, session)) = maybe_browser_fetcher(start.clone()).await else {
        return;
    };

    let response = fetcher.fetch(&start).await.expect("browser fetch succeeds");
    session.shutdown().await.expect("browser shuts down");

    let mut expected = start.clone();
    expected.set_fragment(Some("ready"));
    assert_eq!(response.final_url, expected.to_string());
    assert!(response.redirects.is_empty());
    assert_eq!(response.status, 200);
    assert!(response.body.contains("Hash Ready"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_fetcher_uses_later_document_metadata_after_script_navigation() {
    let _browser_guard = BROWSER_TEST_LOCK.lock().await;
    let server = RenderServer::spawn().await;
    let start = server.url("/cross-start");
    let Some((fetcher, session)) = maybe_browser_fetcher(start.clone()).await else {
        return;
    };

    let response = fetcher.fetch(&start).await.expect("browser fetch succeeds");
    session.shutdown().await.expect("browser shuts down");

    assert_eq!(response.status, 201);
    assert_eq!(response.final_url, server.url("/cross-final").to_string());
    assert!(response.redirects.is_empty());
    assert_eq!(response.content_type.as_deref(), Some("text/html"));
    assert_eq!(
        response
            .headers
            .get_all_case_insensitive("x-page-generation"),
        ["final"]
    );
    assert!(response.body.contains("Final Document"));
    assert!(!response.body.contains("Initial Document"));

    let paths = server.paths();
    assert!(paths.iter().any(|path| path == "/cross-start"));
    assert!(paths.iter().any(|path| path == "/cross-final"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_fetcher_extracts_rendered_dom_and_final_url() {
    let _browser_guard = BROWSER_TEST_LOCK.lock().await;
    let server = RenderServer::spawn().await;
    let start = server.url("/redirect");
    let Some((fetcher, session)) = maybe_browser_fetcher(start.clone()).await else {
        return;
    };

    let robots_fetcher: Arc<dyn Fetcher> =
        Arc::new(HttpFetcher::new(HttpFetcherConfig::default()).expect("HTTP fetcher builds"));
    let robots = Arc::new(RobotsPolicy::load(&start, robots_fetcher, false).await);
    let crawl_fetcher: Arc<dyn Fetcher> = Arc::new(fetcher);
    let scope = CrawlScope::new(
        &start,
        Vec::<ScopePattern>::new(),
        Vec::<ScopePattern>::new(),
        0,
        1,
    );
    let driver = CrawlDriver::new(
        start.clone(),
        crawl_fetcher,
        scope,
        robots,
        CrawlDriverConfig {
            concurrency: 1,
            rate_limit_ms: 0,
            additional_seeds: Vec::new(),
        },
    )
    .expect("driver builds");
    let events = driver.run().collect::<Vec<_>>().await;

    session.shutdown().await.expect("browser shuts down");

    let item = events
        .iter()
        .find_map(|event| match event {
            Ok(CrawlEvent::Item(item)) => Some(item),
            _ => None,
        })
        .expect("rendered page was crawled");
    assert_eq!(item.url, normalize_url(&start));
    assert_eq!(item.response.status, 200);
    assert_eq!(
        item.response.final_url,
        server.url("/client-state").to_string()
    );
    assert_eq!(item.response.redirects.len(), 1);
    assert_eq!(
        item.response.redirects[0].from,
        server.url("/redirect").to_string()
    );
    assert_eq!(item.response.redirects[0].status, 302);
    assert_eq!(
        item.response.redirects[0].to,
        server.url("/page").to_string()
    );
    assert!(item.response.rendered);
    assert!(item
        .discovered_links
        .contains(&server.url("/rendered-link").to_string()));

    let record = extract(item);
    assert!(navigation_changed(&record));
    assert_eq!(record.title.as_deref(), Some("Rendered Title"));
    assert_eq!(
        record.canonical.as_deref(),
        Some(server.url("/canonical-rendered").as_str())
    );
    assert_eq!(record.structured_data.len(), 1);
    assert_eq!(record.structured_data[0].types, vec!["WebPage"]);
    assert_eq!(record.hreflang.len(), 1);
    assert_eq!(
        record.hreflang[0].href,
        server.url("/en/rendered").to_string()
    );

    let http_fetcher = HttpFetcher::new(HttpFetcherConfig::default()).expect("HTTP fetcher builds");
    let http_response = http_fetcher
        .fetch(&start)
        .await
        .expect("HTTP fetch succeeds");
    let http_item = CrawlItem {
        url: normalize_url(&start),
        depth: 0,
        response: http_response,
        discovered_links: Vec::new(),
    };
    let http_record = extract(&http_item);
    assert_eq!(http_record.title.as_deref(), Some("Plain Title"));
    assert!(http_record.canonical.is_none());
    assert!(http_record.structured_data.is_empty());
    assert!(http_record.hreflang.is_empty());

    assert!(server.paths().iter().any(|path| path == "/redirect"));
    assert!(server.paths().iter().any(|path| path == "/page"));
}
