#![allow(dead_code)]

pub mod site;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::{CONTENT_TYPE, LOCATION};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri};
use axum::routing::any;
use axum::Router;
use futures_util::stream;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use url::Url;

pub const ROBOTS_BODY: &str = "\
User-agent: auditroo
Disallow: /private
Allow: /

User-agent: *
Disallow: /

Sitemap: https://example.test/sitemap-from-robots.xml
";

/// A request that the local test server received.
///
/// The fetcher tests check the headers of each redirect hop. Names are
/// stored in lowercase with all their values, like `ResponseHeaders`.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub path: String,
    #[allow(dead_code)]
    pub method: String,
    pub headers: BTreeMap<String, Vec<String>>,
}

impl RecordedRequest {
    pub fn header_values(&self, name: &str) -> Vec<String> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    }

    pub fn header_value(&self, name: &str) -> Option<String> {
        self.header_values(name).into_iter().next()
    }
}

#[derive(Clone)]
struct AppState {
    addr: SocketAddr,
    records: Arc<Mutex<Vec<RecordedRequest>>>,
    port_change_target: Option<String>,
    robots_redirect_target: Option<String>,
}

/// A small HTTP server inside the test process, for the fetcher tests.
///
/// It listens on a free localhost port and records every request. Its fixed
/// routes cover redirects, large bodies, charsets and which headers go
/// where. Nothing goes over the internet.
pub struct TestServer {
    addr: SocketAddr,
    records: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl TestServer {
    pub async fn spawn() -> Self {
        Self::spawn_with_port_change_target(None).await
    }

    /// Spawn a server whose `/port-change` route redirects to `port_change_target`.
    ///
    /// With a second server, a test can check that custom headers are
    /// dropped when a redirect changes only the port.
    #[allow(dead_code)]
    pub async fn spawn_with_port_change_target(port_change_target: Option<String>) -> Self {
        Self::spawn_with_targets(port_change_target, None).await
    }

    /// Spawn a server whose `/robots.txt` can redirect.
    #[allow(dead_code)]
    pub async fn spawn_with_robots_redirect_target(robots_redirect_target: Option<String>) -> Self {
        Self::spawn_with_targets(None, robots_redirect_target).await
    }

    async fn spawn_with_targets(
        port_change_target: Option<String>,
        robots_redirect_target: Option<String>,
    ) -> Self {
        let records = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test server binds");
        let addr = listener.local_addr().expect("test server has local addr");
        let state = AppState {
            addr,
            records: Arc::clone(&records),
            port_change_target,
            robots_redirect_target,
        };
        let app = Router::new()
            .route("/{*path}", any(handle))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test server runs");
        });

        Self { addr, records }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{}", self.addr, path)).expect("test URL parses")
    }

    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.records.lock().await.clone()
    }
}

async fn handle(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response<Body> {
    record_request(&state, &method, &uri, &headers).await;

    // All routes are in one table, so you can read them next to the tests
    // that use them.
    match uri.path() {
        "/robots.txt" => match state.robots_redirect_target.as_deref() {
            Some(target) => redirect(StatusCode::MOVED_PERMANENTLY, target),
            None => response(
                StatusCode::OK,
                &[(CONTENT_TYPE.as_str(), "text/plain")],
                Body::from(ROBOTS_BODY),
            ),
        },
        "/ok" => response(
            StatusCode::OK,
            &[
                ("content-type", "text/html; charset=utf-8"),
                ("x-test", "ok"),
            ],
            Body::from("<html><body>ok</body></html>"),
        ),
        "/chain/a" => redirect(StatusCode::MOVED_PERMANENTLY, "/chain/b"),
        "/chain/b" => redirect(StatusCode::FOUND, "/chain/c"),
        "/chain/c" => redirect(StatusCode::TEMPORARY_REDIRECT, "/chain/d"),
        "/chain/d" => response(
            StatusCode::OK,
            &[("content-type", "text/html")],
            Body::from("chain done"),
        ),
        "/one" => redirect(StatusCode::MOVED_PERMANENTLY, "/ok"),
        "/relative/base" => redirect(StatusCode::FOUND, "deep/page"),
        "/relative/deep/page" => response(StatusCode::OK, &[], Body::from("relative")),
        "/loop" => redirect(StatusCode::FOUND, "/loop"),
        "/see-other-no-loc" => response(StatusCode::SEE_OTHER, &[], Body::empty()),
        "/bad-location" => redirect(StatusCode::FOUND, "http://[::1"),
        "/non-http-location" => redirect(StatusCode::FOUND, "mailto:seo@example.com"),
        "/binary-header" => binary_header_response(),
        "/multi-xrobots" => multi_xrobots(),
        "/latin1" => response(
            StatusCode::OK,
            &[("content-type", "text/html; charset=iso-8859-1")],
            Body::from(vec![0xE9]),
        ),
        "/json" => response(
            StatusCode::OK,
            &[("content-type", "application/json")],
            Body::from("{\"ok\":true}"),
        ),
        "/big-known" => response(
            StatusCode::OK,
            &[("content-length", "64"), ("content-type", "text/plain")],
            Body::from("x".repeat(64)),
        ),
        "/big-chunked" => chunked_big_response(),
        "/slow" => {
            tokio::time::sleep(Duration::from_millis(300)).await;
            response(StatusCode::OK, &[], Body::from("slow"))
        }
        "/keep" => redirect(
            StatusCode::MOVED_PERMANENTLY,
            &format!("http://{}/landing", state.addr),
        ),
        "/host-change" => redirect(
            StatusCode::MOVED_PERMANENTLY,
            &format!("http://{}/landing", state.addr),
        ),
        "/host-change-chain" => redirect(
            StatusCode::MOVED_PERMANENTLY,
            &format!("http://{}/changed-next", state.addr),
        ),
        "/changed-next" => redirect(
            StatusCode::MOVED_PERMANENTLY,
            &format!("http://{}/landing", state.addr),
        ),
        "/return-chain/start" => redirect(
            StatusCode::MOVED_PERMANENTLY,
            &format!(
                "http://localhost:{}/return-chain/changed",
                state.addr.port()
            ),
        ),
        "/return-chain/changed" => redirect(
            StatusCode::MOVED_PERMANENTLY,
            &format!("http://{}/return-chain/original", state.addr),
        ),
        "/return-chain/original" => response(StatusCode::OK, &[], Body::from("reattached")),
        "/port-change" => redirect(
            StatusCode::MOVED_PERMANENTLY,
            state
                .port_change_target
                .as_deref()
                .expect("port-change target configured"),
        ),
        "/landing" => response(StatusCode::OK, &[], Body::from("landing")),
        path if path.starts_with("/status/") => status_response(path),
        _ => response(StatusCode::NOT_FOUND, &[], Body::from("not found")),
    }
}

async fn record_request(state: &AppState, method: &Method, uri: &Uri, headers: &HeaderMap) {
    let mut recorded_headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        recorded_headers
            .entry(name.as_str().to_ascii_lowercase())
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }

    state.records.lock().await.push(RecordedRequest {
        path: uri.path().to_owned(),
        method: method.as_str().to_owned(),
        headers: recorded_headers,
    });
}

fn response(status: StatusCode, headers: &[(&str, &str)], body: Body) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    for (name, value) in headers {
        let header_name =
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("test header name is valid");
        let header_value = HeaderValue::from_str(value).expect("test header value is valid");
        response.headers_mut().append(header_name, header_value);
    }
    response
}

fn redirect(status: StatusCode, location: &str) -> Response<Body> {
    response(status, &[(LOCATION.as_str(), location)], Body::empty())
}

fn multi_xrobots() -> Response<Body> {
    let mut response = response(StatusCode::OK, &[], Body::from("robots"));
    response
        .headers_mut()
        .append("X-Robots-Tag", HeaderValue::from_static("noindex"));
    response
        .headers_mut()
        .append("x-robots-tag", HeaderValue::from_static("nofollow"));
    response
}

fn binary_header_response() -> Response<Body> {
    let mut response = response(StatusCode::OK, &[], Body::from("binary header"));
    response.headers_mut().append(
        "x-binary",
        HeaderValue::from_bytes(&[0xFF]).expect("test header value is valid"),
    );
    response
}

fn chunked_big_response() -> Response<Body> {
    let chunks = vec![
        Ok::<Bytes, Infallible>(Bytes::from(vec![b'a'; 8])),
        Ok(Bytes::from(vec![b'b'; 8])),
        Ok(Bytes::from(vec![b'c'; 8])),
    ];
    response(
        StatusCode::OK,
        &[(CONTENT_TYPE.as_str(), "text/plain")],
        Body::from_stream(stream::iter(chunks)),
    )
}

fn status_response(path: &str) -> Response<Body> {
    let code = path
        .trim_start_matches("/status/")
        .parse::<u16>()
        .expect("status code path segment parses");
    let status = StatusCode::from_u16(code).expect("test status code is valid");
    response(status, &[], Body::from(format!("status {code}")))
}

/// A local site that a test can change between two crawls. The server
/// stops when the fixture is dropped.
#[allow(dead_code)]
pub struct FixtureSite {
    addr: SocketAddr,
    routes: Arc<Mutex<BTreeMap<String, FixtureResponse>>>,
    records: Arc<Mutex<Vec<RecordedRequest>>>,
    not_found: Arc<Mutex<Option<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct FixtureResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub delay: Duration,
}

type FixtureSiteState = (
    Arc<Mutex<BTreeMap<String, FixtureResponse>>>,
    Arc<Mutex<Vec<RecordedRequest>>>,
    Arc<Mutex<Option<Vec<u8>>>>,
);

#[allow(dead_code)]
impl FixtureSite {
    pub async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes = Arc::new(Mutex::new(BTreeMap::<String, FixtureResponse>::new()));
        let records = Arc::new(Mutex::new(Vec::new()));
        let not_found = Arc::new(Mutex::new(None));
        let state = (
            Arc::clone(&routes),
            Arc::clone(&records),
            Arc::clone(&not_found),
        );
        let app = Router::new()
            .fallback(any(
                |State((routes, records, not_found)): State<FixtureSiteState>,
                 method: Method,
                 uri: Uri,
                 headers: HeaderMap| async move {
                    let mut recorded_headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
                    for (name, value) in &headers {
                        recorded_headers
                            .entry(name.to_string())
                            .or_default()
                            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
                    }
                    records.lock().await.push(RecordedRequest {
                        path: uri.path().to_owned(),
                        method: method.to_string(),
                        headers: recorded_headers,
                    });
                    let route = routes.lock().await.get(uri.path()).cloned();
                    match route {
                        Some(route) => {
                            tokio::time::sleep(route.delay).await;
                            let headers: Vec<_> = route
                                .headers
                                .iter()
                                .map(|(name, value)| (name.as_str(), value.as_str()))
                                .collect();
                            response(
                                StatusCode::from_u16(route.status).unwrap(),
                                &headers,
                                Body::from(route.body),
                            )
                        }
                        None => match not_found.lock().await.clone() {
                            Some(body) => response(
                                StatusCode::NOT_FOUND,
                                &[("content-type", "text/html")],
                                Body::from(body),
                            ),
                            None => response(StatusCode::NOT_FOUND, &[], Body::from("not found")),
                        },
                    }
                },
            ))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            addr,
            routes,
            records,
            not_found,
            task,
        }
    }

    /// HTML body for paths without a route.
    pub async fn set_not_found(&self, body: String) {
        *self.not_found.lock().await = Some(body.into_bytes());
    }

    pub fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{}", self.addr, path)).unwrap()
    }

    pub async fn set(&self, path: &str, status: u16, content_type: &str, body: &str) {
        self.set_response(
            path,
            FixtureResponse {
                status,
                headers: vec![("content-type".into(), content_type.into())],
                body: body
                    .replace("{{ORIGIN}}", &format!("http://{}", self.addr))
                    .into_bytes(),
                delay: Duration::ZERO,
            },
        )
        .await;
    }

    pub async fn set_response(&self, path: &str, route: FixtureResponse) {
        self.routes.lock().await.insert(path.to_owned(), route);
    }

    pub async fn redirect(&self, path: &str, target: &str) {
        self.set_response(
            path,
            FixtureResponse {
                status: 302,
                headers: vec![("location".into(), target.into())],
                body: Vec::new(),
                delay: Duration::ZERO,
            },
        )
        .await;
    }

    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.records.lock().await.clone()
    }
}

impl Drop for FixtureSite {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An HTML page record that loaded with 200 and links nowhere.
#[allow(dead_code)]
pub fn page_record(url: &str) -> auditroo::extract::SeoRecord {
    auditroo::extract::SeoRecord {
        url: url.to_owned(),
        final_url: url.to_owned(),
        content_type: Some("text/html".to_owned()),
        indexability: auditroo::extract::Indexability {
            robots_meta: Vec::new(),
            googlebot_meta: Vec::new(),
            x_robots_tag: Vec::new(),
            is_indexable: true,
        },
        canonical: Some(url.to_owned()),
        title: Some(format!("Title of {url}")),
        meta_description: None,
        headings: auditroo::extract::Headings {
            h1: vec!["Heading".to_owned()],
            ..Default::default()
        },
        structured_data: Vec::new(),
        hreflang: Vec::new(),
        http_status: 200,
        redirect_chain: Vec::new(),
        rendered: false,
    }
}

/// A snapshot of `records` where the start page links to every other page.
#[allow(dead_code)]
pub fn snapshot_of(
    start_url: &str,
    records: Vec<auditroo::extract::SeoRecord>,
) -> auditroo::snapshot::Snapshot {
    let links: Vec<String> = records
        .iter()
        .map(|record| record.url.clone())
        .filter(|url| url != start_url)
        .collect();
    let link_graph = records
        .iter()
        .map(|record| {
            let targets = if record.url == start_url {
                links.clone()
            } else {
                Vec::new()
            };
            (record.url.clone(), targets)
        })
        .collect();
    let origin = Url::parse(start_url).expect("start URL parses");
    auditroo::snapshot::assemble(
        auditroo::snapshot::SnapshotInput {
            start_url: start_url.to_owned(),
            production_urls: Vec::new(),
            crawl_observation: auditroo::snapshot::CrawlObservation {
                respect_robots: true,
                ..Default::default()
            },
            robots: [(
                origin.origin().ascii_serialization(),
                auditroo::crawl::RobotsFile {
                    url: origin.join("/robots.txt").unwrap().to_string(),
                    status: Some(404),
                    body: None,
                    error: None,
                },
            )]
            .into_iter()
            .collect(),
            records,
            sitemap: auditroo::sitemap::SitemapDiscovery {
                status: auditroo::sitemap::SitemapDiscoveryStatus::Absent,
                documents: Vec::new(),
                declared_urls: Vec::new(),
            },
            link_graph,
            crawl_failures: Vec::new(),
        },
        time::OffsetDateTime::now_utc(),
        None,
    )
    .expect("test snapshot assembles")
}
