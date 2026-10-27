mod support;

use std::net::TcpListener;

use auditroo::crawl::{FetchError, Fetcher, HttpFetcher, HttpFetcherConfig};
use support::{RecordedRequest, TestServer};
use url::Url;

fn fetcher() -> HttpFetcher {
    HttpFetcher::new(HttpFetcherConfig::default()).expect("default fetcher builds")
}

fn fetcher_with_header() -> HttpFetcher {
    HttpFetcher::new(HttpFetcherConfig {
        headers: vec![("x-auditroo-bypass".to_owned(), "secret".to_owned())],
        ..HttpFetcherConfig::default()
    })
    .expect("fetcher with custom header builds")
}

fn expected_default_user_agent() -> String {
    format!(
        "auditroo/{} (+https://github.com/vincentsch/auditroo)",
        env!("CARGO_PKG_VERSION")
    )
}

fn request_for<'a>(requests: &'a [RecordedRequest], path: &str) -> &'a RecordedRequest {
    requests
        .iter()
        .find(|request| request.path == path)
        .unwrap_or_else(|| panic!("recorded request for {path} exists: {requests:#?}"))
}

fn assert_header(request: &RecordedRequest, name: &str, expected: &str) {
    assert_eq!(request.header_value(name).as_deref(), Some(expected));
}

#[tokio::test]
async fn plain_200_captures_body_headers_and_final_url() {
    let server = TestServer::spawn().await;
    let url = server.url("/ok");

    let response = fetcher().fetch(&url).await.expect("fetch succeeds");

    assert_eq!(response.final_url, url.to_string());
    assert!(response.redirects.is_empty());
    assert_eq!(response.status, 200);
    assert_eq!(response.content_type.as_deref(), Some("text/html"));
    assert!(response.body.contains("ok"));
    assert!(!response.rendered);
    assert_eq!(response.headers.get_all_case_insensitive("x-test"), ["ok"]);
}

#[tokio::test]
async fn single_redirect_records_one_hop_and_final_response() {
    let server = TestServer::spawn().await;

    let response = fetcher()
        .fetch(&server.url("/one"))
        .await
        .expect("fetch succeeds");

    assert_eq!(response.status, 200);
    assert_eq!(response.final_url, server.url("/ok").to_string());
    assert_eq!(response.redirects.len(), 1);
    assert_eq!(response.redirects[0].from, server.url("/one").to_string());
    assert_eq!(response.redirects[0].status, 301);
    assert_eq!(response.redirects[0].to, server.url("/ok").to_string());
}

#[tokio::test]
async fn multi_hop_redirect_chain_is_ordered() {
    let server = TestServer::spawn().await;

    let response = fetcher()
        .fetch(&server.url("/chain/a"))
        .await
        .expect("fetch succeeds");

    assert_eq!(response.status, 200);
    assert_eq!(response.final_url, server.url("/chain/d").to_string());
    assert_eq!(response.redirects.len(), 3);
    assert_eq!(
        response.redirects[0].from,
        server.url("/chain/a").to_string()
    );
    assert_eq!(response.redirects[0].status, 301);
    assert_eq!(response.redirects[0].to, server.url("/chain/b").to_string());
    assert_eq!(
        response.redirects[1].from,
        server.url("/chain/b").to_string()
    );
    assert_eq!(response.redirects[1].status, 302);
    assert_eq!(response.redirects[1].to, server.url("/chain/c").to_string());
    assert_eq!(
        response.redirects[2].from,
        server.url("/chain/c").to_string()
    );
    assert_eq!(response.redirects[2].status, 307);
    assert_eq!(response.redirects[2].to, server.url("/chain/d").to_string());
}

#[tokio::test]
async fn relative_location_is_resolved_against_current_hop() {
    let server = TestServer::spawn().await;

    let response = fetcher()
        .fetch(&server.url("/relative/base"))
        .await
        .expect("fetch succeeds");

    assert_eq!(response.status, 200);
    assert_eq!(
        response.final_url,
        server.url("/relative/deep/page").to_string()
    );
    assert_eq!(response.redirects.len(), 1);
    assert_eq!(
        response.redirects[0].from,
        server.url("/relative/base").to_string()
    );
    assert_eq!(
        response.redirects[0].to,
        server.url("/relative/deep/page").to_string()
    );
}

#[tokio::test]
async fn malformed_redirect_location_is_fetch_error() {
    let server = TestServer::spawn().await;

    let error = fetcher()
        .fetch(&server.url("/bad-location"))
        .await
        .expect_err("bad redirect location fails");

    assert!(matches!(error, FetchError::MalformedRedirect { .. }));
}

#[tokio::test]
async fn non_http_redirect_location_is_fetch_error() {
    let server = TestServer::spawn().await;

    let error = fetcher()
        .fetch(&server.url("/non-http-location"))
        .await
        .expect_err("non-http redirect location fails");

    assert!(matches!(error, FetchError::MalformedRedirect { .. }));
}

#[tokio::test]
async fn redirect_loop_stops_at_cap_and_returns_terminal_3xx() {
    let server = TestServer::spawn().await;

    let response = fetcher()
        .fetch(&server.url("/loop"))
        .await
        .expect("capped redirect returns response");

    assert_eq!(response.status, 302);
    assert_eq!(response.final_url, server.url("/loop").to_string());
    assert_eq!(response.redirects.len(), 10);
    assert_eq!(
        response.headers.get_all_case_insensitive("location"),
        ["/loop"]
    );

    let requests = server.requests().await;
    assert_eq!(requests.len(), 11);
}

#[tokio::test]
async fn terminal_3xx_without_location_is_data() {
    let server = TestServer::spawn().await;

    let response = fetcher()
        .fetch(&server.url("/see-other-no-loc"))
        .await
        .expect("terminal 3xx returns response");

    assert_eq!(response.status, 303);
    assert_eq!(
        response.final_url,
        server.url("/see-other-no-loc").to_string()
    );
    assert!(response.redirects.is_empty());
}

#[tokio::test]
async fn http_404_and_500_are_data() {
    let server = TestServer::spawn().await;

    let not_found = fetcher()
        .fetch(&server.url("/status/404"))
        .await
        .expect("404 returns response");
    let server_error = fetcher()
        .fetch(&server.url("/status/500"))
        .await
        .expect("500 returns response");

    assert_eq!(not_found.status, 404);
    assert_eq!(not_found.body, "status 404");
    assert_eq!(server_error.status, 500);
    assert_eq!(server_error.body, "status 500");

    let requests = server.requests().await;
    assert_eq!(request_for(&requests, "/status/404").method, "GET");
}

#[tokio::test]
async fn repeated_x_robots_tag_values_are_preserved_case_insensitively() {
    let server = TestServer::spawn().await;

    let response = fetcher()
        .fetch(&server.url("/multi-xrobots"))
        .await
        .expect("fetch succeeds");

    assert_eq!(
        response.headers.get_all_case_insensitive("X-Robots-Tag"),
        ["noindex", "nofollow"]
    );
}

#[tokio::test]
async fn non_utf8_response_header_values_are_kept_lossily() {
    let server = TestServer::spawn().await;

    let response = fetcher()
        .fetch(&server.url("/binary-header"))
        .await
        .expect("binary header response succeeds");

    assert_eq!(response.headers.get_all_case_insensitive("x-binary"), ["�"]);
}

#[tokio::test]
async fn charset_decoding_and_non_html_content_are_returned() {
    let server = TestServer::spawn().await;

    let latin1 = fetcher()
        .fetch(&server.url("/latin1"))
        .await
        .expect("latin1 response succeeds");
    let json = fetcher()
        .fetch(&server.url("/json"))
        .await
        .expect("json response succeeds");

    assert_eq!(latin1.content_type.as_deref(), Some("text/html"));
    assert_eq!(latin1.body, "é");
    assert_eq!(json.content_type.as_deref(), Some("application/json"));
    assert_eq!(json.body, "{\"ok\":true}");
}

#[tokio::test]
async fn byte_fetch_preserves_raw_body_and_response_metadata() {
    let server = TestServer::spawn().await;
    let fetcher = fetcher();

    let response = fetcher
        .fetch_bytes(&server.url("/latin1"), &server.url("/start"))
        .await
        .expect("byte fetch succeeds");

    assert_eq!(response.final_url, server.url("/latin1").to_string());
    assert!(response.redirects.is_empty());
    assert_eq!(response.status, 200);
    assert_eq!(response.content_type.as_deref(), Some("text/html"));
    assert_eq!(response.bytes, vec![0xE9]);
}

#[tokio::test]
async fn body_cap_rejects_declared_and_streamed_oversized_bodies() {
    let server = TestServer::spawn().await;
    let capped = HttpFetcher::new(HttpFetcherConfig {
        max_body_bytes: 10,
        ..HttpFetcherConfig::default()
    })
    .expect("capped fetcher builds");

    let declared = capped
        .fetch(&server.url("/big-known"))
        .await
        .expect_err("declared oversized body fails");
    let streamed = capped
        .fetch(&server.url("/big-chunked"))
        .await
        .expect_err("streamed oversized body fails");

    assert!(matches!(declared, FetchError::BodyTooLarge { limit: 10 }));
    assert!(matches!(streamed, FetchError::BodyTooLarge { limit: 10 }));
}

#[tokio::test]
async fn configured_user_agent_is_sent_on_initial_and_redirect_followups() {
    let server = TestServer::spawn().await;
    let user_agent = "auditroo-test/0.1 (+https://example.test/repo)";
    let custom_fetcher = HttpFetcher::new(HttpFetcherConfig {
        user_agent: Some(user_agent.to_owned()),
        ..HttpFetcherConfig::default()
    })
    .expect("custom user agent fetcher builds");

    custom_fetcher
        .fetch(&server.url("/chain/a"))
        .await
        .expect("fetch succeeds");

    let requests = server.requests().await;
    for path in ["/chain/a", "/chain/b", "/chain/c", "/chain/d"] {
        assert_header(request_for(&requests, path), "user-agent", user_agent);
    }
}

#[tokio::test]
async fn default_user_agent_is_sent_on_initial_and_redirect_followups() {
    let server = TestServer::spawn().await;
    let expected = expected_default_user_agent();

    fetcher()
        .fetch(&server.url("/chain/a"))
        .await
        .expect("fetch succeeds");

    let requests = server.requests().await;
    for path in ["/chain/a", "/chain/b", "/chain/c", "/chain/d"] {
        assert_header(request_for(&requests, path), "user-agent", &expected);
    }
}

#[tokio::test]
async fn custom_headers_are_kept_for_same_host_and_port_redirects() {
    let server = TestServer::spawn().await;

    fetcher_with_header()
        .fetch(&server.url("/keep"))
        .await
        .expect("fetch succeeds");

    let requests = server.requests().await;
    assert_header(
        request_for(&requests, "/keep"),
        "x-auditroo-bypass",
        "secret",
    );
    assert_header(
        request_for(&requests, "/landing"),
        "x-auditroo-bypass",
        "secret",
    );
}

#[tokio::test]
async fn custom_headers_are_stripped_for_host_change_but_user_agent_remains() {
    let server = TestServer::spawn().await;
    // The server redirects to 127.0.0.1. Starting from `localhost` changes
    // the host name while the request still goes to the same test server.
    let start = Url::parse(&format!(
        "http://localhost:{}/host-change",
        server.addr().port()
    ))
    .expect("localhost URL parses");
    let expected_ua = expected_default_user_agent();

    fetcher_with_header()
        .fetch(&start)
        .await
        .expect("fetch succeeds");

    let requests = server.requests().await;
    let initial = request_for(&requests, "/host-change");
    let landing = request_for(&requests, "/landing");
    assert_header(initial, "x-auditroo-bypass", "secret");
    assert!(landing.header_values("x-auditroo-bypass").is_empty());
    assert_header(initial, "user-agent", &expected_ua);
    assert_header(landing, "user-agent", &expected_ua);
}

#[tokio::test]
async fn custom_headers_stay_stripped_after_host_change_redirect_chain() {
    let server = TestServer::spawn().await;
    let start = Url::parse(&format!(
        "http://localhost:{}/host-change-chain",
        server.addr().port()
    ))
    .expect("localhost URL parses");
    let expected_ua = expected_default_user_agent();

    fetcher_with_header()
        .fetch(&start)
        .await
        .expect("fetch succeeds");

    let requests = server.requests().await;
    let initial = request_for(&requests, "/host-change-chain");
    let changed_next = request_for(&requests, "/changed-next");
    let landing = request_for(&requests, "/landing");

    assert_header(initial, "x-auditroo-bypass", "secret");
    assert!(changed_next.header_values("x-auditroo-bypass").is_empty());
    assert!(landing.header_values("x-auditroo-bypass").is_empty());
    assert_header(initial, "user-agent", &expected_ua);
    assert_header(changed_next, "user-agent", &expected_ua);
    assert_header(landing, "user-agent", &expected_ua);
}

#[tokio::test]
async fn custom_headers_are_reattached_when_redirect_returns_to_origin() {
    let server = TestServer::spawn().await;

    fetcher_with_header()
        .fetch(&server.url("/return-chain/start"))
        .await
        .expect("fetch succeeds");

    let requests = server.requests().await;
    let initial = request_for(&requests, "/return-chain/start");
    let changed = request_for(&requests, "/return-chain/changed");
    let returned = request_for(&requests, "/return-chain/original");

    assert_header(initial, "x-auditroo-bypass", "secret");
    assert!(changed.header_values("x-auditroo-bypass").is_empty());
    assert_header(returned, "x-auditroo-bypass", "secret");
}

#[tokio::test]
async fn custom_headers_are_stripped_for_port_change_but_user_agent_remains() {
    let target = TestServer::spawn().await;
    // Two servers on different free ports give a redirect that changes only
    // the port, without leaving localhost.
    let primary =
        TestServer::spawn_with_port_change_target(Some(target.url("/landing").to_string())).await;
    let expected_ua = expected_default_user_agent();

    fetcher_with_header()
        .fetch(&primary.url("/port-change"))
        .await
        .expect("fetch succeeds");

    let primary_requests = primary.requests().await;
    let target_requests = target.requests().await;
    let initial = request_for(&primary_requests, "/port-change");
    let landing = request_for(&target_requests, "/landing");
    assert_header(initial, "x-auditroo-bypass", "secret");
    assert!(landing.header_values("x-auditroo-bypass").is_empty());
    assert_header(initial, "user-agent", &expected_ua);
    assert_header(landing, "user-agent", &expected_ua);
}

#[tokio::test]
async fn timeout_and_unreachable_url_are_fetch_errors() {
    let server = TestServer::spawn().await;
    let timeout_fetcher = HttpFetcher::new(HttpFetcherConfig {
        timeout_ms: 50,
        ..HttpFetcherConfig::default()
    })
    .expect("timeout fetcher builds");

    let timeout = timeout_fetcher
        .fetch(&server.url("/slow"))
        .await
        .expect_err("slow endpoint times out");
    assert!(matches!(timeout, FetchError::Timeout));

    let listener = TcpListener::bind("127.0.0.1:0").expect("unused port binds");
    let addr = listener.local_addr().expect("unused port has addr");
    drop(listener);
    let unreachable = Url::parse(&format!("http://{addr}/")).expect("unreachable URL parses");
    let error = fetcher()
        .fetch(&unreachable)
        .await
        .expect_err("unreachable URL fails");

    assert!(matches!(error, FetchError::Request { .. }));
}
