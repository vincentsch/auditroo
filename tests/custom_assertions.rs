use std::collections::BTreeMap;
use std::net::SocketAddr;

use assert_cmd::Command;
use auditroo::assertions::{evaluate_assertions, extract_custom_values, CustomExtraction};
use auditroo::audit::{evaluate, Evaluation};
use auditroo::config::{
    resolve_settings_with_env, EnvironmentError, ResolvedSettings, SettingsOverrides,
};
use auditroo::crawl::{CrawlItem, FetchedResponse, ResponseHeaders, RobotsFile};
use auditroo::extract::extract;
use auditroo::findings::{Rule, Severity};
use auditroo::report::ReportCommand;
use auditroo::sitemap::{SitemapDiscovery, SitemapDiscoveryStatus};
use auditroo::snapshot::{assemble, read, CrawlObservation, Snapshot, SnapshotInput};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode, Uri};
use axum::routing::any;
use axum::Router;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;
use time::OffsetDateTime;
use tokio::net::TcpListener;
use url::Url;

fn env(vars: &[(&str, &str)]) -> impl FnMut(&str) -> Result<String, EnvironmentError> {
    let map = vars
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<BTreeMap<_, _>>();
    move |name| map.get(name).cloned().ok_or(EnvironmentError::Missing)
}

fn write_config(raw: &str) -> (TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("auditroo.toml");
    std::fs::write(&path, raw).expect("config writes");
    (dir, path)
}

fn crawl_item(
    url: &str,
    content_type: Option<&str>,
    headers: ResponseHeaders,
    body: &str,
) -> CrawlItem {
    CrawlItem {
        url: url.to_owned(),
        depth: 0,
        response: FetchedResponse {
            final_url: url.to_owned(),
            redirects: Vec::new(),
            status: 200,
            headers,
            content_type: content_type.map(str::to_owned),
            body: body.to_owned(),
            rendered: false,
        },
        discovered_links: Vec::new(),
    }
}

#[test]
fn configured_extractors_capture_typed_values_and_source_states() {
    let (_dir, path) = write_config(
        r##"
        [[extract]]
        id = "hero"
        kind = "css_text"
        selector = "#hero"

        [[extract]]
        id = "cta-href"
        kind = "css_attr"
        selector = "a.cta"
        attr = "href"

        [[extract]]
        id = "item-count"
        kind = "css_count"
        selector = ".item"

        [[extract]]
        id = "total"
        kind = "regex"
        source = "dom_text"
        pattern = "Total\\s+(\\d+)"
        group = 1

        [[extract]]
        id = "product-nodes"
        kind = "json_ld"
        schema_type = "Product"

        [[extract]]
        id = "price"
        kind = "json_pointer"
        schema_type = "Product"
        pointer = "/offers/price"

        [[extract]]
        id = "safe-header"
        kind = "header"
        name = "x-safe"

        [[extract]]
        id = "extension"
        kind = "url_part"
        part = "extension"
        "##,
    );
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");
    let item = crawl_item(
        "https://example.test/products/widget.html?ref=ci",
        Some("text/html"),
        ResponseHeaders::from_pairs([("X-Safe", "ok")]),
        r#"
        <h1 id="hero">Main&nbsp; Product</h1>
        <a class="cta" href="/buy">Buy</a>
        <span class="item"></span><span class="item"></span>
        <p>Total 42</p>
        <script>var hidden = "Total 99";</script>
        <script type="application/ld+json">
          {"@type":"Product","offers":{"price":19.95,"availability":"InStock"}}
        </script>
        "#,
    );

    let values = extract_custom_values(&item, &settings.extractors);
    assert_eq!(
        values["hero"].value.as_ref().unwrap(),
        &serde_json::json!(["Main Product"])
    );
    assert_eq!(
        values["cta-href"].value.as_ref().unwrap(),
        &serde_json::json!(["/buy"])
    );
    assert_eq!(
        values["item-count"].value.as_ref().unwrap(),
        &serde_json::json!(2)
    );
    assert_eq!(
        values["total"].value.as_ref().unwrap(),
        &serde_json::json!(["42"])
    );
    assert_eq!(
        values["price"].value.as_ref().unwrap(),
        &serde_json::json!([19.95])
    );
    assert_eq!(
        values["safe-header"].value.as_ref().unwrap(),
        &serde_json::json!(["ok"])
    );
    assert_eq!(
        values["extension"].value.as_ref().unwrap(),
        &serde_json::json!("html")
    );

    let non_html = crawl_item(
        "https://example.test/products/widget.html",
        Some("application/json"),
        ResponseHeaders::new(),
        "{}",
    );
    let unsupported = extract_custom_values(&non_html, &settings.extractors);
    assert_eq!(
        unsupported["hero"].state,
        auditroo::assertions::ExtractionState::UnsupportedSource
    );
}

#[test]
fn config_rejects_unsafe_headers_and_static_operator_mismatches() {
    let (_dir, restricted) = write_config(
        r#"
        [[extract]]
        id = "auth"
        kind = "header"
        name = "authorization"
        "#,
    );
    assert!(
        resolve_settings_with_env(Some(&restricted), SettingsOverrides::default(), env(&[]))
            .is_err()
    );

    let (_dir, bad_operator) = write_config(
        r#"
        [[assert]]
        id = "bool-regex"
        field = "indexability.is_indexable"
        operator = "matches"
        pattern = "true"
        severity = "high"
        "#,
    );
    assert!(
        resolve_settings_with_env(Some(&bad_operator), SettingsOverrides::default(), env(&[]))
            .is_err()
    );

    let (_dir, empty_exclude) = write_config(
        r#"
        [[extract]]
        id = "title-text"
        kind = "css_text"
        selector = "title"
        exclude = []
        "#,
    );
    assert!(resolve_settings_with_env(
        Some(&empty_exclude),
        SettingsOverrides::default(),
        env(&[])
    )
    .is_err());
}

#[test]
fn extraction_collection_stops_at_configured_limits() {
    let (_dir, path) = write_config(
        r#"
        [[extract]]
        id = "items"
        kind = "css_text"
        selector = ".item"
        max_values = 1

        [[extract]]
        id = "empty-regex"
        kind = "regex"
        source = "body"
        pattern = ""
        max_values = 1
        "#,
    );
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");
    let item = crawl_item(
        "https://example.test/",
        Some("text/html"),
        ResponseHeaders::new(),
        r#"<span class="item">one</span><span class="item">two</span>abc"#,
    );
    let values = extract_custom_values(&item, &settings.extractors);
    assert_eq!(
        values["items"].state,
        auditroo::assertions::ExtractionState::LimitExceeded
    );
    assert_eq!(
        values["empty-regex"].state,
        auditroo::assertions::ExtractionState::LimitExceeded
    );
}

#[test]
fn string_list_empty_state_considers_all_values_not_just_evidence() {
    let (_dir, path) = write_config(
        r#"
        [[extract]]
        id = "delayed-text"
        kind = "css_text"
        selector = ".delayed"
        max_values = 25
        "#,
    );
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");
    let empty_items = (0..20)
        .map(|_| r#"<span class="delayed"></span>"#)
        .collect::<String>();
    let item = crawl_item(
        "https://example.test/",
        Some("text/html"),
        ResponseHeaders::new(),
        &format!(r#"{empty_items}<span class="delayed">ready</span>"#),
    );

    let values = extract_custom_values(&item, &settings.extractors);
    assert_eq!(
        values["delayed-text"].state,
        auditroo::assertions::ExtractionState::Value
    );
    assert_eq!(
        values["delayed-text"].value.as_ref().unwrap(),
        &serde_json::json!([
            "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "ready"
        ])
    );
}

fn at() -> OffsetDateTime {
    OffsetDateTime::parse(
        "2026-08-18T09:00:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("timestamp parses")
}

fn snapshot_from_item(item: &CrawlItem, settings: &ResolvedSettings) -> Snapshot {
    let record = extract(item);
    let custom = (!settings.extractors.is_empty()).then(|| CustomExtraction {
        catalog: settings.extractors.clone(),
        values: [(
            record.url.clone(),
            extract_custom_values(item, &settings.extractors),
        )]
        .into_iter()
        .collect(),
    });
    assemble(
        SnapshotInput {
            start_url: record.url.clone(),
            production_urls: Vec::new(),
            crawl_observation: CrawlObservation {
                respect_robots: true,
                max_pages: 10_000,
                ..CrawlObservation::default()
            },
            robots: [(
                "https://www.example.test".to_owned(),
                RobotsFile {
                    url: "https://www.example.test/robots.txt".to_owned(),
                    status: Some(404),
                    body: None,
                    error: None,
                },
            )]
            .into_iter()
            .collect(),
            link_graph: [(record.url.clone(), Vec::new())].into_iter().collect(),
            records: vec![record],
            sitemap: SitemapDiscovery {
                status: SitemapDiscoveryStatus::Absent,
                documents: Vec::new(),
                declared_urls: Vec::new(),
            },
            crawl_failures: Vec::new(),
        },
        at(),
        custom,
    )
    .expect("snapshot assembles")
}

async fn diff_report(
    baseline: &Snapshot,
    current: &Snapshot,
    settings: &ResolvedSettings,
) -> auditroo::report::Report {
    evaluate(Evaluation {
        command: ReportCommand::Diff,
        baseline: Some(baseline),
        current,
        settings,
        notes: Vec::new(),
    })
    .await
    .expect("report evaluates")
}

#[tokio::test]
async fn assertion_findings_gate_and_ignore_without_mutating_changes() {
    let (_dir, path) = write_config(
        r#"
        [gate]
        fail_on = "high"
        ignore = [{ assertion = "product-canonical-host" }]

        [[extract]]
        id = "product-nodes"
        kind = "json_ld"
        include = ["/products/*"]
        schema_type = "Product"

        [[assert]]
        id = "product-offer-shape"
        extractor = "product-nodes"
        include = ["/products/*"]
        operator = "json_shape"
        required = [{ pointer = "/offers/price" }, { pointer = "/offers/availability", type = "string" }]
        severity = "high"

        [[assert]]
        id = "product-canonical-host"
        field = "canonical"
        include = ["/products/*"]
        operator = "url_allowed_host"
        allowed_hosts = ["www.example.test"]
        severity = "critical"
        "#,
    );
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");
    let item = crawl_item(
        "https://www.example.test/products/widget",
        Some("text/html"),
        ResponseHeaders::new(),
        r#"
        <link rel="canonical" href="https://staging.example.test/products/widget">
        <script type="application/ld+json">{"@type":"Product","offers":{"price":"19.95"}}</script>
        "#,
    );
    let baseline = snapshot_from_item(&item, &settings);
    let current = snapshot_from_item(&item, &settings);
    let report = diff_report(&baseline, &current, &settings).await;
    let assertions: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| finding.rule == Rule::Assertion)
        .collect();
    assert_eq!(assertions.len(), 2, "{:#?}", report.findings);
    assert!(assertions.iter().any(|finding| finding.check_id.as_deref()
        == Some("product-canonical-host")
        && finding.severity == Severity::Critical
        && finding.ignored));
    assert!(assertions.iter().any(|finding| finding.check_id.as_deref()
        == Some("product-offer-shape")
        && !finding.ignored));
    assert_eq!(report.counts.critical, 0);
    assert_eq!(report.counts.ignored, 1);
    assert!(!report.passed);

    // A failing custom check is named by its own id, the way an ignore rule
    // accepts it.
    let human = auditroo::render::human::render(&report, Default::default());
    assert!(
        human.contains("      assertion: product-offer-shape\n"),
        "{human}"
    );
    assert!(!human.contains("rule: assertion"), "{human}");
    let markdown = auditroo::render::markdown::render(&report);
    assert!(
        markdown.contains("`assertion: product-offer-shape`"),
        "{markdown}"
    );
}

#[tokio::test]
async fn long_assertion_message_is_bounded() {
    let message = "x".repeat(1024);
    let (_dir, path) = write_config(&format!(
        r#"
        [[assert]]
        id = "title-policy"
        field = "title"
        operator = "equals"
        expected = "Expected"
        message = "{message}"
        severity = "high"
        "#
    ));
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");
    let item = crawl_item(
        "https://www.example.test/",
        Some("text/html"),
        ResponseHeaders::new(),
        "<title>Actual</title>",
    );
    let baseline = snapshot_from_item(&item, &settings);
    let current = snapshot_from_item(&item, &settings);
    let report = diff_report(&baseline, &current, &settings).await;
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.rule == Rule::Assertion)
        .expect("assertion finding");
    assert!(finding
        .message
        .starts_with("fails the custom check title-policy: "));
    assert_eq!(
        finding.message.chars().count(),
        "fails the custom check title-policy: ".len() + 240
    );
}

#[test]
fn empty_assertion_message_uses_default_failure_reason() {
    let (_dir, path) = write_config(
        r#"
        [[assert]]
        id = "title-policy"
        field = "title"
        operator = "equals"
        expected = "Expected"
        message = ""
        severity = "high"
        "#,
    );
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");
    let item = crawl_item(
        "https://www.example.test/",
        Some("text/html"),
        ResponseHeaders::new(),
        "<title>Actual</title>",
    );
    let baseline = snapshot_from_item(&item, &settings);
    let current = snapshot_from_item(&item, &settings);
    let findings = evaluate_assertions(
        &baseline,
        &current,
        &settings.extractors,
        &settings.assertions,
    )
    .expect("assertions evaluate");
    assert_eq!(
        findings[0].failure_reason,
        "the value is not the expected value"
    );
}

#[derive(Clone, Copy)]
enum Variant {
    Baseline,
    Regressed,
}

#[derive(Clone)]
struct FixtureState {
    addr: SocketAddr,
    variant: Variant,
}

struct FixtureServer {
    state: FixtureState,
}

impl FixtureServer {
    async fn spawn(variant: Variant) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("server binds");
        let addr = listener.local_addr().expect("server addr");
        let state = FixtureState { addr, variant };
        let app = Router::new()
            .route("/{*path}", any(handle))
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server runs");
        });
        Self { state }
    }

    fn url(&self, path: &str) -> String {
        Url::parse(&format!("http://{}{}", self.state.addr, path))
            .expect("test URL parses")
            .to_string()
    }
}

async fn handle(
    State(state): State<FixtureState>,
    uri: Uri,
    _headers: HeaderMap,
) -> Response<Body> {
    match uri.path() {
        "/robots.txt" => response(404, "text/plain", ""),
        "/products/widget" => {
            let origin = format!("http://{}", state.addr);
            let body = match state.variant {
                Variant::Baseline => format!(
                    r#"
                    <title>Widget</title>
                    <link rel="canonical" href="{origin}/products/widget">
                    <script type="application/ld+json">
                    {{"@type":"Product","offers":{{"price":"19.95","availability":"InStock"}}}}
                    </script>
                    "#
                ),
                Variant::Regressed => format!(
                    r#"
                    <title>Widget</title>
                    <link rel="canonical" href="{origin}/products/widget">
                    <script type="application/ld+json">
                    {{"@type":"Product","offers":{{"price":"19.95"}}}}
                    </script>
                    <script type="application/ld+json">
                    {{"@type":"Product","offers":{{"availability":"InStock"}}}}
                    </script>
                    "#
                ),
            };
            response(200, "text/html", body)
        }
        "/checkout" | "/pricing" => {
            let origin = format!("http://{}", state.addr);
            let path = uri.path();
            let body = match state.variant {
                Variant::Baseline => format!(
                    r#"
                    <title>Checkout</title>
                    <link rel="canonical" href="{origin}{path}">
                    <script>window.analyticsId = "checkout-analytics-123";</script>
                    "#
                ),
                Variant::Regressed => format!(
                    r#"
                    <title>Checkout</title>
                    <link rel="canonical" href="{origin}{path}">
                    <a href="https://staging.example.test{path}">debug checkout</a>
                    "#
                ),
            };
            response(200, "text/html", body)
        }
        "/docs/guide" => {
            let origin = format!("http://{}", state.addr);
            let body = match state.variant {
                Variant::Baseline => format!(
                    r#"
                    <title>Guide</title>
                    <meta name="description" content="A complete guide.">
                    <link rel="canonical" href="{origin}/docs/guide">
                    <h1>Guide</h1>
                    <script type="application/ld+json">
                    {{"@type":"BreadcrumbList","itemListElement":[]}}
                    </script>
                    <script type="application/ld+json">
                    {{"@type":"Article","headline":"Guide","dateModified":"2026-08-18"}}
                    </script>
                    "#
                ),
                Variant::Regressed => format!(
                    r#"
                    <link rel="canonical" href="{origin}/docs/guide">
                    <h1>Guide</h1><h1>Duplicate</h1>
                    <script type="application/ld+json">
                    {{"@type":"Article","headline":"Guide"}}
                    </script>
                    "#
                ),
            };
            response(200, "text/html", body)
        }
        _ => response(404, "text/plain", "not found"),
    }
}

fn response(status: u16, content_type: &str, body: impl Into<String>) -> Response<Body> {
    let mut response = Response::new(Body::from(body.into()));
    *response.status_mut() = StatusCode::from_u16(status).expect("status valid");
    response.headers_mut().append(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("content type valid"),
    );
    response
}

fn command_in(dir: &TempDir) -> Command {
    let mut command = Command::cargo_bin("auditroo").expect("binary exists");
    command.current_dir(dir.path());
    command
}

fn copied_recipe_config(
    dir: &TempDir,
    recipe: &str,
    replacements: &[(&str, &str)],
) -> std::path::PathBuf {
    let config = dir.path().join(format!("{recipe}.toml"));
    command_in(dir)
        .args([
            "recipes",
            "copy",
            recipe,
            "--config",
            config.to_str().unwrap(),
        ])
        .assert()
        .success();
    let mut raw_config = std::fs::read_to_string(&config).expect("config reads");
    for (placeholder, value) in replacements {
        raw_config = raw_config.replace(placeholder, value);
    }
    std::fs::write(&config, raw_config).expect("config writes");
    config
}

#[test]
fn recipe_copy_failures_are_reported_without_writing_configs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let unknown = dir.path().join("unknown.toml");
    command_in(&dir)
        .args([
            "recipes",
            "copy",
            "unknown-recipe",
            "--config",
            unknown.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(2);
    assert!(!unknown.exists());

    let missing_parent = dir.path().join("missing").join("auditroo.toml");
    command_in(&dir)
        .args([
            "recipes",
            "copy",
            "product-pages",
            "--config",
            missing_parent.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(2);
    assert!(!missing_parent.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn copied_product_recipe_dry_run_and_diff_renderers_share_finding_ids() {
    let baseline = FixtureServer::spawn(Variant::Baseline).await;
    let current = FixtureServer::spawn(Variant::Regressed).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let config = copied_recipe_config(
        &dir,
        "product-pages",
        &[("PLACEHOLDER_PRODUCTION_HOST", "127.0.0.1")],
    );

    command_in(&dir)
        .args(["recipes", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("product-pages"));
    command_in(&dir)
        .args(["recipes", "show", "product-pages"])
        .assert()
        .success()
        .stdout(predicate::str::contains("product-offer-shape"));

    let baseline_path = dir.path().join("baseline.json");
    command_in(&dir)
        .args([
            "snapshot",
            &baseline.url("/products/widget"),
            "--config",
            config.to_str().unwrap(),
            "--out",
            baseline_path.to_str().unwrap(),
        ])
        .assert()
        .success();
    let snapshot: Snapshot = read(&baseline_path).expect("snapshot reads");
    assert!(snapshot.custom_extraction.is_some());

    command_in(&dir)
        .args([
            "assert",
            "--dry-run",
            "--config",
            config.to_str().unwrap(),
            "--snapshot",
            baseline_path.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "product-offer-shape (json_shape on extractor product-nodes): checks 1 of 1 URLs",
        ))
        .stdout(predicate::str::contains("values stored for 1"));

    let stale_config = dir.path().join("stale-product.toml");
    let stale_raw = std::fs::read_to_string(&config)
        .expect("config reads")
        .replace("schema_type = \"Product\"", "schema_type = \"Offer\"");
    std::fs::write(&stale_config, stale_raw).expect("stale config writes");
    command_in(&dir)
        .args([
            "assert",
            "--dry-run",
            "--config",
            stale_config.to_str().unwrap(),
            "--snapshot",
            baseline_path.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("product-nodes"))
        .stdout(predicate::str::contains(
            "the snapshot has no values for this version of the extractor",
        ));

    let json_output = command_in(&dir)
        .args([
            "diff",
            &current.url("/products/widget"),
            "--baseline",
            baseline_path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&json_output).expect("json parses");
    assert_eq!(json["schema_version"], 1);
    assert!(has_assertion(&json, "product-offer-shape"));
    assert!(has_assertion(&json, "product-node-count"));

    let junit = dir.path().join("assertions.xml");
    command_in(&dir)
        .args([
            "diff",
            &current.url("/products/widget"),
            "--baseline",
            baseline_path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--junit",
            junit.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("product-offer-shape"));
    let junit_raw = std::fs::read_to_string(junit).expect("junit reads");
    assert!(junit_raw.contains("product-offer-shape"));
}

fn has_assertion(report: &Value, id: &str) -> bool {
    report["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .any(|finding| finding["rule"] == "assertion" && finding["check_id"] == id)
}

#[tokio::test(flavor = "multi_thread")]
async fn copied_checkout_recipe_flags_checkout_regressions() {
    let baseline = FixtureServer::spawn(Variant::Baseline).await;
    let current = FixtureServer::spawn(Variant::Regressed).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let config = copied_recipe_config(
        &dir,
        "checkout-and-pricing",
        &[
            ("PLACEHOLDER_PRODUCTION_HOST", "127.0.0.1"),
            (
                "PLACEHOLDER_STAGING_HOST_PATTERN",
                r"staging\\.example\\.test",
            ),
            ("PLACEHOLDER_ANALYTICS_ID", "checkout-analytics-123"),
        ],
    );

    let baseline_path = dir.path().join("baseline-checkout.json");
    command_in(&dir)
        .args([
            "snapshot",
            &baseline.url("/checkout"),
            "--config",
            config.to_str().unwrap(),
            "--out",
            baseline_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let json_output = command_in(&dir)
        .args([
            "diff",
            &current.url("/checkout"),
            "--baseline",
            baseline_path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&json_output).expect("json parses");
    assert!(has_assertion(&json, "checkout-no-staging-host"));
    assert!(has_assertion(&json, "checkout-analytics-present"));
}

#[tokio::test(flavor = "multi_thread")]
async fn copied_documentation_recipe_flags_docs_regressions() {
    let baseline = FixtureServer::spawn(Variant::Baseline).await;
    let current = FixtureServer::spawn(Variant::Regressed).await;
    let dir = tempfile::tempdir().expect("temp dir");
    let config = copied_recipe_config(
        &dir,
        "documentation-site",
        &[("PLACEHOLDER_PRODUCTION_HOST", "127.0.0.1")],
    );

    let baseline_path = dir.path().join("baseline-docs.json");
    command_in(&dir)
        .args([
            "snapshot",
            &baseline.url("/docs/guide"),
            "--config",
            config.to_str().unwrap(),
            "--out",
            baseline_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let json_output = command_in(&dir)
        .args([
            "diff",
            &current.url("/docs/guide"),
            "--baseline",
            baseline_path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&json_output).expect("json parses");
    assert!(has_assertion(&json, "docs-single-h1"));
    assert!(has_assertion(&json, "docs-breadcrumb-present"));
    assert!(has_assertion(&json, "docs-article-shape"));
}
