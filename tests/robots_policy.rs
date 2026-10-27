mod support;

use std::sync::Arc;

use auditroo::crawl::{Fetcher, HttpFetcher, HttpFetcherConfig, RobotsPolicy};
use support::{RecordedRequest, TestServer, ROBOTS_BODY};

fn fetcher_with_user_agent_and_header(user_agent: &str) -> Arc<dyn Fetcher> {
    Arc::new(
        HttpFetcher::new(HttpFetcherConfig {
            user_agent: Some(user_agent.to_owned()),
            headers: vec![("x-auditroo-bypass".to_owned(), "secret".to_owned())],
            ..HttpFetcherConfig::default()
        })
        .expect("fetcher builds"),
    )
}

fn fetcher_with_user_agent(user_agent: &str) -> Arc<dyn Fetcher> {
    Arc::new(
        HttpFetcher::new(HttpFetcherConfig {
            user_agent: Some(user_agent.to_owned()),
            ..HttpFetcherConfig::default()
        })
        .expect("fetcher builds"),
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
async fn robots_txt_is_fetched_through_configured_fetcher() {
    let server = TestServer::spawn().await;
    let user_agent = "AuditrooTest/1";
    let policy = RobotsPolicy::load(
        &server.url("/start"),
        fetcher_with_user_agent_and_header(user_agent),
        true,
    )
    .await;

    let requests = server.requests().await;
    let robots = request_for(&requests, "/robots.txt");
    assert_header(robots, "user-agent", user_agent);
    assert_header(robots, "x-auditroo-bypass", "secret");

    assert!(!policy.allows(&server.url("/private")).await);
    assert!(policy.allows(&server.url("/public")).await);
    assert_eq!(
        policy.sitemap_urls(),
        vec!["https://example.test/sitemap-from-robots.xml".to_owned()]
    );
    assert!(ROBOTS_BODY.contains("User-agent: auditroo"));
}

#[tokio::test]
async fn outgoing_user_agent_does_not_change_robots_product_token() {
    let server = TestServer::spawn().await;
    let user_agent = "CustomBot/9";
    let policy =
        RobotsPolicy::load(&server.url("/"), fetcher_with_user_agent(user_agent), true).await;

    assert!(!policy.allows(&server.url("/private")).await);
    assert!(policy.allows(&server.url("/public")).await);

    let requests = server.requests().await;
    assert_header(
        request_for(&requests, "/robots.txt"),
        "user-agent",
        user_agent,
    );
}

#[tokio::test]
async fn redirected_robots_keeps_user_agent_strips_custom_header_and_keys_requested_origin() {
    let target = TestServer::spawn().await;
    let primary =
        TestServer::spawn_with_robots_redirect_target(Some(target.url("/robots.txt").to_string()))
            .await;
    let user_agent = "AuditrooRedirectTest/1";
    let policy = RobotsPolicy::load(
        &primary.url("/start"),
        fetcher_with_user_agent_and_header(user_agent),
        true,
    )
    .await;

    let primary_requests = primary.requests().await;
    let target_requests = target.requests().await;
    let primary_robots = request_for(&primary_requests, "/robots.txt");
    let target_robots = request_for(&target_requests, "/robots.txt");

    assert_header(primary_robots, "user-agent", user_agent);
    assert_header(primary_robots, "x-auditroo-bypass", "secret");
    assert_header(target_robots, "user-agent", user_agent);
    assert!(target_robots.header_values("x-auditroo-bypass").is_empty());

    assert!(!policy.allows(&primary.url("/private")).await);
    let file = policy.start_file();
    assert_eq!(file.url, format!("http://{}/robots.txt", primary.addr()));
    assert_eq!(file.status, Some(200));
}
