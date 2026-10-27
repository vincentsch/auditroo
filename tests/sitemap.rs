use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use auditroo::crawl::{
    CrawlScope, CrawlSeedSource, Fetcher, HttpFetcher, HttpFetcherConfig, RedirectHop,
    RobotsPolicy, ScopePattern,
};
use auditroo::extract::{Indexability, SeoRecord};
use auditroo::site::Site;
use auditroo::sitemap::{
    compute_sitemap_coverage, discover_sitemaps, discover_sitemaps_from_urls, sitemap_seeds,
    SitemapDiscoveryConfig, SitemapDiscoveryStatus, SitemapDocumentOutcome,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_TYPE, LOCATION};
use axum::http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri};
use axum::routing::any;
use axum::Router;
use flate2::write::GzEncoder;
use flate2::Compression;
use tokio::net::TcpListener;
use url::Url;

#[derive(Debug, Clone)]
struct RecordedRequest {
    path: String,
    headers: BTreeMap<String, Vec<String>>,
}

impl RecordedRequest {
    fn header_values(&self, name: &str) -> Vec<String> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default()
    }

    fn header_value(&self, name: &str) -> Option<String> {
        self.header_values(name).into_iter().next()
    }
}

#[derive(Clone)]
struct RouteResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct SitemapServerState {
    addr: SocketAddr,
    routes: Arc<Mutex<BTreeMap<String, RouteResponse>>>,
    records: Arc<Mutex<Vec<RecordedRequest>>>,
}

struct SitemapServer {
    state: SitemapServerState,
}

impl SitemapServer {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test server binds");
        let addr = listener.local_addr().expect("test server has local addr");
        let state = SitemapServerState {
            addr,
            routes: Arc::new(Mutex::new(BTreeMap::new())),
            records: Arc::new(Mutex::new(Vec::new())),
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
        Url::parse(&format!("http://{}{}", self.state.addr, path)).expect("test URL parses")
    }

    fn url_with_host(&self, host: &str, path: &str) -> Url {
        Url::parse(&format!(
            "http://{}:{}{}",
            host,
            self.state.addr.port(),
            path
        ))
        .expect("test URL parses")
    }

    fn add_text(&self, path: &str, status: u16, content_type: &str, body: impl Into<String>) {
        self.add_bytes(
            path,
            status,
            &[(CONTENT_TYPE.as_str(), content_type)],
            body.into().into_bytes(),
        );
    }

    fn add_bytes(&self, path: &str, status: u16, headers: &[(&str, &str)], body: Vec<u8>) {
        self.state.routes.lock().unwrap().insert(
            path.to_owned(),
            RouteResponse {
                status,
                headers: headers
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                body,
            },
        );
    }

    fn add_redirect(&self, path: &str, status: u16, location: &str) {
        self.add_bytes(path, status, &[(LOCATION.as_str(), location)], Vec::new());
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.records.lock().unwrap().clone()
    }
}

async fn handle(
    State(state): State<SitemapServerState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response<Body> {
    record_request(&state, method, &uri, &headers);

    let route = state.routes.lock().unwrap().get(uri.path()).cloned();
    match route {
        Some(route) => route_response(route),
        None => route_response(RouteResponse {
            status: 404,
            headers: Vec::new(),
            body: b"not found".to_vec(),
        }),
    }
}

fn record_request(state: &SitemapServerState, _method: Method, uri: &Uri, headers: &HeaderMap) {
    let mut recorded_headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        recorded_headers
            .entry(name.as_str().to_ascii_lowercase())
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }

    state.records.lock().unwrap().push(RecordedRequest {
        path: uri.path().to_owned(),
        headers: recorded_headers,
    });
}

fn route_response(route: RouteResponse) -> Response<Body> {
    let mut response = Response::new(Body::from(route.body));
    *response.status_mut() = StatusCode::from_u16(route.status).expect("test status is valid");
    for (name, value) in route.headers {
        let header_name =
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name is valid");
        let header_value = HeaderValue::from_str(&value).expect("header value is valid");
        response.headers_mut().append(header_name, header_value);
    }
    response
}

fn request_for<'a>(requests: &'a [RecordedRequest], path: &str) -> &'a RecordedRequest {
    requests
        .iter()
        .find(|request| request.path == path)
        .unwrap_or_else(|| panic!("recorded request for {path} exists: {requests:#?}"))
}

fn scope(start_url: &Url, include: &[&str], exclude: &[&str]) -> CrawlScope {
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
        0,
        0,
    )
}

fn fetcher() -> Arc<HttpFetcher> {
    Arc::new(HttpFetcher::new(HttpFetcherConfig::default()).expect("fetcher builds"))
}

fn fetcher_with_header() -> Arc<HttpFetcher> {
    Arc::new(
        HttpFetcher::new(HttpFetcherConfig {
            headers: vec![("x-auditroo-bypass".to_owned(), "secret".to_owned())],
            ..HttpFetcherConfig::default()
        })
        .expect("fetcher builds"),
    )
}

fn record(url: &str, status: u16, indexable: bool, redirected: bool) -> SeoRecord {
    SeoRecord {
        url: url.to_owned(),
        final_url: url.to_owned(),
        content_type: Some("text/html".to_owned()),
        indexability: Indexability {
            robots_meta: Vec::new(),
            googlebot_meta: Vec::new(),
            x_robots_tag: Vec::new(),
            is_indexable: indexable,
        },
        canonical: None,
        title: None,
        meta_description: None,
        headings: Default::default(),
        structured_data: Vec::new(),
        hreflang: Vec::new(),
        http_status: status,
        redirect_chain: redirected
            .then(|| RedirectHop {
                from: url.to_owned(),
                status: 301,
                to: format!("{url}-final"),
            })
            .into_iter()
            .collect(),
        rendered: false,
    }
}

fn sorted(mut urls: Vec<String>) -> Vec<String> {
    urls.sort();
    urls
}

fn gzip(body: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(body.as_bytes())
        .expect("gzip fixture writes");
    encoder.finish().expect("gzip fixture finishes")
}

#[tokio::test]
async fn robots_hint_discovery_produces_seeds_and_complete_coverage() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");
    let home = start.to_string();
    let sitemap_only = server.url("/sitemap-only").to_string();
    let noindex = server.url("/noindex").to_string();
    let redirected = server.url("/redirected").to_string();
    let client_navigation = server.url("/client-navigation").to_string();
    let undeclared = server.url("/undeclared").to_string();

    server.add_text(
        "/robots.txt",
        200,
        "text/plain",
        format!(
            "User-agent: auditroo\nAllow: /\nSitemap: {}\n",
            server.url("/sitemap.xml")
        ),
    );
    server.add_text(
        "/sitemap.xml",
        200,
        "application/xml",
        format!(
            r#"<?xml version="1.0"?>
            <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
              <url><loc>{home}</loc></url>
              <url><loc>{sitemap_only}</loc></url>
              <url><loc>{noindex}</loc></url>
              <url><loc>{redirected}</loc></url>
              <url><loc>{client_navigation}</loc></url>
            </urlset>"#
        ),
    );

    let fetcher = fetcher();
    let robots_fetcher: Arc<dyn Fetcher> = fetcher.clone();
    let robots = RobotsPolicy::load(&start, robots_fetcher, true).await;
    let discovery = discover_sitemaps(
        &start,
        fetcher.as_ref(),
        &robots,
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;

    assert_eq!(
        discovery.status,
        SitemapDiscoveryStatus::Complete,
        "{discovery:#?}"
    );
    assert_eq!(
        discovery.declared_urls,
        sorted(vec![
            home.clone(),
            sitemap_only.clone(),
            noindex.clone(),
            redirected.clone(),
            client_navigation.clone(),
        ])
    );
    assert_eq!(
        sitemap_seeds(&discovery, &scope(&start, &[], &[]))
            .iter()
            .map(|seed| (seed.url.to_string(), seed.source.clone()))
            .collect::<Vec<_>>(),
        discovery
            .declared_urls
            .iter()
            .map(|url| (url.clone(), CrawlSeedSource::Sitemap))
            .collect::<Vec<_>>()
    );

    let mut client_navigation_record = record(&client_navigation, 200, true, false);
    client_navigation_record.final_url = server.url("/client-navigation-state").to_string();
    let records = [
        record(&home, 200, true, false),
        record(&noindex, 200, false, false),
        record(&redirected, 200, true, true),
        client_navigation_record,
        record(&undeclared, 200, true, false),
    ];
    let coverage = compute_sitemap_coverage(
        discovery,
        records.iter(),
        &Site::new("https://example.test/", std::iter::empty()),
    );

    assert_eq!(coverage.declared_not_observed, vec![sitemap_only]);
    assert_eq!(
        coverage.declared_observed_not_directly_indexable,
        sorted(vec![client_navigation, noindex, redirected])
    );
    assert_eq!(coverage.indexable_not_declared, vec![undeclared]);
}

#[tokio::test]
async fn sitemap_index_partial_state_preserves_positive_facts_transactionally() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");
    let a = server.url("/a").to_string();
    let b = server.url("/b").to_string();
    let prefix_from_bad = server.url("/prefix-from-bad").to_string();
    let undeclared = server.url("/undeclared").to_string();

    server.add_text(
        "/index.xml",
        200,
        "application/xml",
        format!(
            r#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
              <sitemap><loc>{}</loc></sitemap>
              <sitemap><loc>ftp://example.test/not-http.xml</loc></sitemap>
              <sitemap><loc>{}</loc></sitemap>
              <sitemap><loc>{}</loc></sitemap>
              <sitemap><loc>{}</loc></sitemap>
            </sitemapindex>"#,
            server.url("/b.xml"),
            server.url("/a.xml"),
            server.url("/bad.xml"),
            server.url("/b.xml")
        ),
    );
    server.add_text(
        "/a.xml",
        200,
        "application/xml",
        format!(
            r#"<urlset>
              <url><loc><![CDATA[{a}]]></loc></url>
              <url><loc>relative</loc></url>
              <url><loc>mailto:seo@example.test</loc></url>
              <url><image:image><image:loc>https://cdn.example.test/image.jpg</image:loc></image:image></url>
            </urlset>"#
        ),
    );
    server.add_text(
        "/b.xml",
        200,
        "application/xml",
        format!(r#"<urlset><url><loc>{b}</loc></url></urlset>"#),
    );
    server.add_text(
        "/bad.xml",
        200,
        "application/xml",
        format!(r#"<urlset><url><loc>{prefix_from_bad}</loc></url>"#),
    );

    let discovery = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![server.url("/index.xml").to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;

    assert_eq!(discovery.status, SitemapDiscoveryStatus::Partial);
    assert_eq!(discovery.declared_urls, sorted(vec![a.clone(), b.clone()]));
    assert!(!discovery.declared_urls.contains(&prefix_from_bad));
    assert!(discovery.documents.iter().any(|document| {
        document.outcome == SitemapDocumentOutcome::Invalid
            && document.requested_url == server.url("/bad.xml").to_string()
    }));
    assert!(discovery.documents.iter().any(|document| {
        document.outcome == SitemapDocumentOutcome::ParsedSitemapIndex
            && document
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("1 child sitemap entry without a full URL"))
    }));

    let records = [
        record(&a, 200, true, false),
        record(&b, 200, true, false),
        record(&undeclared, 200, true, false),
    ];
    let coverage = compute_sitemap_coverage(
        discovery,
        records.iter(),
        &Site::new("https://example.test/", std::iter::empty()),
    );
    assert!(coverage.indexable_not_declared.is_empty());
}

#[tokio::test]
async fn fallback_missing_is_absent_but_hinted_missing_is_invalid() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");

    let absent = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        Vec::<String>::new(),
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;
    assert_eq!(absent.status, SitemapDiscoveryStatus::Absent);
    assert_eq!(absent.documents[0].outcome, SitemapDocumentOutcome::Missing);

    let hinted_missing = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![server.url("/missing.xml").to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;
    assert_eq!(hinted_missing.status, SitemapDiscoveryStatus::Invalid);
    assert_eq!(
        hinted_missing.documents[0].outcome,
        SitemapDocumentOutcome::Missing
    );
}

#[tokio::test]
async fn rejected_root_diagnostics_redact_url_userinfo() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");

    let discovery = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec!["ftp://audit:secret@example.test/private-sitemap.xml".to_owned()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;

    assert_eq!(discovery.status, SitemapDiscoveryStatus::Invalid);
    assert_eq!(discovery.documents.len(), 1);
    assert_eq!(
        discovery.documents[0].requested_url,
        "ftp://example.test/private-sitemap.xml"
    );

    let encoded = serde_json::to_string(&discovery).expect("discovery serializes");
    assert!(!encoded.contains("audit:secret"));
    assert!(!encoded.contains("secret@example"));
}

#[tokio::test]
async fn gzip_sitemaps_and_size_failures_are_classified() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");
    let compressed_url = server.url("/compressed");

    server.add_bytes(
        "/compressed",
        200,
        &[("content-type", "application/octet-stream")],
        gzip(&format!(
            r#"<urlset><url><loc>{}</loc></url></urlset>"#,
            server.url("/compressed-page")
        )),
    );
    server.add_bytes(
        "/corrupt",
        200,
        &[("content-type", "application/octet-stream")],
        vec![0x1f, 0x8b, 0x00, 0x00],
    );
    server.add_bytes(
        "/too-large",
        200,
        &[("content-type", "application/octet-stream")],
        gzip("<urlset><url><loc>http://example.test/large</loc></url></urlset>"),
    );

    let complete = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![compressed_url.to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;
    assert_eq!(complete.status, SitemapDiscoveryStatus::Complete);
    assert_eq!(
        complete.documents[0].outcome,
        SitemapDocumentOutcome::ParsedUrlSet
    );

    let invalid = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![server.url("/corrupt").to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;
    assert_eq!(invalid.status, SitemapDiscoveryStatus::Invalid);
    assert_eq!(
        invalid.documents[0].outcome,
        SitemapDocumentOutcome::Invalid
    );

    let unavailable = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![server.url("/too-large").to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig {
            max_decompressed_bytes: 10,
            ..SitemapDiscoveryConfig::default()
        },
    )
    .await;
    assert_eq!(unavailable.status, SitemapDiscoveryStatus::Unavailable);
    assert_eq!(
        unavailable.documents[0].outcome,
        SitemapDocumentOutcome::Unavailable
    );
}

#[tokio::test]
async fn redirect_location_failures_keep_observed_metadata_and_redact_targets() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");
    let malformed = server.url("/malformed-location.xml");
    let non_http = server.url("/non-http-location.xml");

    server.add_redirect("/malformed-location.xml", 302, "http://[::1");
    server.add_redirect(
        "/non-http-location.xml",
        302,
        "ftp://audit:secret@example.test/private.xml",
    );

    let discovery = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![malformed.to_string(), non_http.to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;

    assert_eq!(discovery.status, SitemapDiscoveryStatus::Unavailable);
    assert_eq!(discovery.documents.len(), 2);

    for expected in [malformed, non_http] {
        let requested = expected.to_string();
        let diagnostic = discovery
            .documents
            .iter()
            .find(|document| document.requested_url == requested)
            .unwrap_or_else(|| panic!("diagnostic for {requested} exists: {discovery:#?}"));
        assert_eq!(diagnostic.final_url.as_deref(), Some(expected.as_str()));
        assert_eq!(diagnostic.final_status, Some(302));
        assert_eq!(diagnostic.outcome, SitemapDocumentOutcome::Unavailable);
    }

    let encoded = serde_json::to_string(&discovery).expect("discovery serializes");
    assert!(!encoded.contains("audit:secret"));
    assert!(!encoded.contains("secret@example"));
}

#[tokio::test]
async fn wire_byte_cap_diagnostic_keeps_observed_terminal_metadata() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");
    let sitemap = server.url("/large.xml");
    let fetcher = Arc::new(
        HttpFetcher::new(HttpFetcherConfig {
            max_body_bytes: 10,
            ..HttpFetcherConfig::default()
        })
        .expect("fetcher builds"),
    );

    server.add_bytes(
        "/large.xml",
        200,
        &[
            ("content-type", "application/xml"),
            ("content-length", "64"),
        ],
        vec![b'x'; 64],
    );

    let discovery = discover_sitemaps_from_urls(
        &start,
        fetcher.as_ref(),
        vec![sitemap.to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;

    assert_eq!(discovery.status, SitemapDiscoveryStatus::Unavailable);
    assert_eq!(discovery.documents.len(), 1);
    let diagnostic = &discovery.documents[0];
    assert_eq!(diagnostic.requested_url, sitemap.to_string());
    assert_eq!(diagnostic.final_url.as_deref(), Some(sitemap.as_str()));
    assert_eq!(diagnostic.final_status, Some(200));
    assert_eq!(diagnostic.outcome, SitemapDocumentOutcome::Unavailable);
    assert!(diagnostic
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("larger than 10 bytes")));
}

#[tokio::test]
async fn document_and_depth_caps_emit_bounded_summary_diagnostics() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");

    server.add_text(
        "/index.xml",
        200,
        "application/xml",
        format!(
            r#"<sitemapindex>
              <sitemap><loc>{}</loc></sitemap>
              <sitemap><loc>{}</loc></sitemap>
              <sitemap><loc>{}</loc></sitemap>
            </sitemapindex>"#,
            server.url("/a.xml"),
            server.url("/b.xml"),
            server.url("/c.xml")
        ),
    );

    let document_cap = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![server.url("/index.xml").to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig {
            max_sitemap_documents: 1,
            ..SitemapDiscoveryConfig::default()
        },
    )
    .await;

    assert_eq!(document_cap.status, SitemapDiscoveryStatus::Partial);
    assert_eq!(document_cap.documents.len(), 2);
    assert_eq!(
        document_cap
            .documents
            .iter()
            .filter(|document| document.outcome == SitemapDocumentOutcome::SkippedLimit)
            .count(),
        1
    );
    assert!(document_cap.documents.iter().any(|document| {
        document.outcome == SitemapDocumentOutcome::SkippedLimit
            && document
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("3 sitemap files were not read"))
    }));

    let depth_cap = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![server.url("/index.xml").to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig {
            max_depth: 0,
            ..SitemapDiscoveryConfig::default()
        },
    )
    .await;

    assert_eq!(depth_cap.status, SitemapDiscoveryStatus::Partial);
    assert_eq!(depth_cap.documents.len(), 2);
    assert_eq!(
        depth_cap
            .documents
            .iter()
            .filter(|document| document.outcome == SitemapDocumentOutcome::SkippedLimit)
            .count(),
        1
    );
    assert!(depth_cap.documents.iter().any(|document| {
        document.outcome == SitemapDocumentOutcome::SkippedLimit
            && document
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("3 sitemap files were not read"))
    }));
}

#[tokio::test]
async fn declared_url_cap_is_summarized_on_the_parsed_document() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");

    server.add_text(
        "/urls.xml",
        200,
        "application/xml",
        format!(
            r#"<urlset>
              <url><loc>{}</loc></url>
              <url><loc>{}</loc></url>
              <url><loc>{}</loc></url>
            </urlset>"#,
            server.url("/b"),
            server.url("/a"),
            server.url("/c")
        ),
    );

    let discovery = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![server.url("/urls.xml").to_string()],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig {
            max_declared_urls: 1,
            ..SitemapDiscoveryConfig::default()
        },
    )
    .await;

    assert_eq!(discovery.status, SitemapDiscoveryStatus::Partial);
    assert_eq!(discovery.documents.len(), 1);
    assert_eq!(
        discovery.documents[0].outcome,
        SitemapDocumentOutcome::ParsedUrlSet
    );
    assert_eq!(discovery.declared_urls, vec![server.url("/a").to_string()]);
    assert!(discovery.documents[0]
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("so 2 more were not read")));
}

#[tokio::test]
async fn sitemap_credentials_are_anchored_to_start_origin() {
    let primary = SitemapServer::spawn().await;
    let secondary = SitemapServer::spawn().await;
    let start = primary.url("/");
    let fetcher = fetcher_with_header();

    primary.add_text("/same.xml", 200, "application/xml", "<urlset/>");
    let remote_sitemap = secondary.url("/remote.xml");
    primary.add_redirect("/redirect.xml", 301, remote_sitemap.as_str());
    secondary.add_text("/remote.xml", 200, "application/xml", "<urlset/>");

    let discovery = discover_sitemaps_from_urls(
        &start,
        fetcher.as_ref(),
        vec![
            primary.url("/same.xml").to_string(),
            primary.url("/redirect.xml").to_string(),
        ],
        &scope(&start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;
    assert_eq!(
        discovery.status,
        SitemapDiscoveryStatus::Complete,
        "{discovery:#?}"
    );

    let primary_requests = primary.requests();
    let secondary_requests = secondary.requests();
    assert_eq!(
        request_for(&primary_requests, "/same.xml")
            .header_value("x-auditroo-bypass")
            .as_deref(),
        Some("secret")
    );
    assert_eq!(
        request_for(&primary_requests, "/redirect.xml")
            .header_value("x-auditroo-bypass")
            .as_deref(),
        Some("secret")
    );
    assert!(request_for(&secondary_requests, "/remote.xml")
        .header_values("x-auditroo-bypass")
        .is_empty());
    assert!(request_for(&secondary_requests, "/remote.xml")
        .header_value("user-agent")
        .is_some());

    let host_change = SitemapServer::spawn().await;
    host_change.add_text("/same.xml", 200, "application/xml", "<urlset/>");
    let host_change_start = host_change.url_with_host("localhost", "/");
    let host_change_discovery = discover_sitemaps_from_urls(
        &host_change_start,
        fetcher.as_ref(),
        vec![host_change.url("/same.xml").to_string()],
        &scope(&host_change_start, &[], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;
    assert_eq!(
        host_change_discovery.status,
        SitemapDiscoveryStatus::Complete
    );
    // A sitemap on another host is read where it lives, and the start
    // host's headers are not sent there.
    assert!(request_for(&host_change.requests(), "/same.xml")
        .header_values("x-auditroo-bypass")
        .is_empty());
}

#[tokio::test]
async fn production_sitemap_urls_are_checked_on_the_preview_host() {
    let preview = SitemapServer::spawn().await;
    let start = preview.url("/");
    preview.add_text(
        "/sitemap-index.xml",
        200,
        "application/xml",
        r#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <sitemap><loc>https://www.example.com/sitemap-0.xml</loc></sitemap>
          </sitemapindex>"#,
    );
    preview.add_text(
        "/sitemap-0.xml",
        200,
        "application/xml",
        r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <url><loc>https://www.example.com/</loc></url>
            <url><loc>https://www.example.com/docs/?page=2</loc></url>
          </urlset>"#,
    );

    // robots.txt of a preview build still names the production sitemap.
    let production = Url::parse("https://www.example.com/").unwrap();
    let discovery = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec!["https://www.example.com/sitemap-index.xml".to_owned()],
        &scope(&start, &[], &[]).with_production(&production),
        SitemapDiscoveryConfig::default(),
    )
    .await;

    assert_eq!(
        discovery.status,
        SitemapDiscoveryStatus::Complete,
        "{discovery:#?}"
    );
    assert_eq!(
        discovery.declared_urls,
        vec![
            preview.url("/").to_string(),
            preview.url("/docs/?page=2").to_string(),
        ]
    );
    let paths: Vec<String> = preview
        .requests()
        .iter()
        .map(|request| request.path.clone())
        .collect();
    assert!(paths.contains(&"/sitemap-index.xml".to_owned()));
    assert!(paths.contains(&"/sitemap-0.xml".to_owned()));
}

#[tokio::test]
async fn urls_on_other_hosts_are_kept_but_not_crawled() {
    let preview = SitemapServer::spawn().await;
    let start = preview.url("/");
    preview.add_text(
        "/sitemap.xml",
        200,
        "application/xml",
        format!(
            r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
                <url><loc>{start}</loc></url>
                <url><loc>https://staging.example.com/docs/</loc></url>
              </urlset>"#
        ),
    );
    let scope = scope(&start, &[], &[]);
    let discovery = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![preview.url("/sitemap.xml").to_string()],
        &scope,
        SitemapDiscoveryConfig::default(),
    )
    .await;
    assert_eq!(
        discovery.declared_urls,
        sorted(vec![
            start.to_string(),
            "https://staging.example.com/docs/".to_owned()
        ])
    );
    assert_eq!(
        sitemap_seeds(&discovery, &scope)
            .iter()
            .map(|seed| seed.url.to_string())
            .collect::<Vec<_>>(),
        vec![start.to_string()]
    );
}

#[tokio::test]
async fn scope_filtering_keeps_start_url_exemption_without_partial_state() {
    let server = SitemapServer::spawn().await;
    let start = server.url("/");
    let included = server.url("/included-page?a=1&b=2").to_string();
    let included_xml = included.replace('&', "&amp;");
    let out_of_scope = server.url("/other").to_string();

    server.add_text(
        "/sitemap.xml",
        200,
        "application/xml",
        format!(
            "\u{feff}<urlset>
              <url><loc>{}</loc></url>
              <url><loc>{included_xml}</loc></url>
              <url><loc>{out_of_scope}</loc></url>
            </urlset>",
            start
        ),
    );

    let discovery = discover_sitemaps_from_urls(
        &start,
        fetcher().as_ref(),
        vec![server.url("/sitemap.xml").to_string()],
        &scope(&start, &["/included*"], &[]),
        SitemapDiscoveryConfig::default(),
    )
    .await;

    assert_eq!(
        discovery.status,
        SitemapDiscoveryStatus::Complete,
        "{discovery:#?}"
    );
    assert_eq!(
        discovery.declared_urls,
        sorted(vec![start.to_string(), included])
    );
    assert!(!discovery.declared_urls.contains(&out_of_scope));
}
