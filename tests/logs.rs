use auditroo::bots::{BotCatalog, BotRuleConfig, ClientType};
use auditroo::logs::{
    import_log_sources, resolve_log_sources, GenericJsonFieldMap, LogFormat, LogSourceDefinition,
    MAX_RAW_RECORD_BYTES, MAX_SCALAR_FIELD_BYTES,
};

fn source(
    path: &str,
    format: LogFormat,
    field_map: Option<GenericJsonFieldMap>,
) -> LogSourceDefinition {
    LogSourceDefinition {
        id: Some(
            path.rsplit('/')
                .next()
                .expect("fixture path has file name")
                .replace('.', "-"),
        ),
        path: path.into(),
        display_path: path.to_owned(),
        format,
        origin: "https://example.test".to_owned(),
        field_map,
    }
}

fn generic_map() -> GenericJsonFieldMap {
    GenericJsonFieldMap {
        timestamp: "ts".to_owned(),
        method: "method".to_owned(),
        status: "status".to_owned(),
        url: None,
        path: Some("path".to_owned()),
        user_agent: Some("ua".to_owned()),
        referer: None,
        host: None,
        bytes: Some("bytes".to_owned()),
        origin_status: None,
        cache_status: None,
    }
}

#[test]
fn imports_supported_log_formats_into_normalized_entries() {
    let sources = resolve_log_sources(&[
        source("tests/fixtures/logs/common.log", LogFormat::Common, None),
        source(
            "tests/fixtures/logs/combined.log",
            LogFormat::Combined,
            None,
        ),
        source(
            "tests/fixtures/logs/cloudflare.jsonl",
            LogFormat::CloudflareJsonl,
            None,
        ),
        source(
            "tests/fixtures/logs/cloudflare.csv",
            LogFormat::CloudflareCsv,
            None,
        ),
        source(
            "tests/fixtures/logs/generic.jsonl",
            LogFormat::GenericJsonl,
            Some(generic_map()),
        ),
    ])
    .expect("sources resolve");
    let catalog = BotCatalog::with_config_rules(vec![BotRuleConfig {
        id: "example-crawler".to_owned(),
        family_id: "example-crawler".to_owned(),
        display_name: "ExampleCrawler".to_owned(),
        client_type: ClientType::GenericAutomated,
        pattern: "ExampleCrawler/".to_owned(),
    }])
    .expect("catalog builds");

    let mut entries = Vec::new();
    let summary =
        import_log_sources(&sources, &catalog, |entry| entries.push(entry)).expect("logs import");

    assert_eq!(summary.parsed_entries, 5);
    assert_eq!(summary.parse_error_count, 0);
    assert_eq!(summary.normalization_error_count, 0);
    assert_eq!(entries.len(), 5);
    assert!(entries.iter().any(
        |entry| entry.normalized_url == "https://example.test/combined"
            && entry.client_type == ClientType::Search
            && entry.raw_user_agent.as_deref() == Some("Googlebot/2.1")
    ));
    assert!(entries.iter().any(|entry| entry.normalized_url
        == "https://example.test/cloudflare-jsonl"
        && entry.client_type == ClientType::AiBot
        && entry.status == 403
        && entry.origin_status == Some(403)));
    assert!(entries.iter().any(
        |entry| entry.normalized_url == "https://example.test/generic"
            && entry.bot_family.as_deref() == Some("example-crawler")
    ));
    assert_eq!(
        summary
            .counts_by_client_type
            .get(&ClientType::Unknown)
            .copied(),
        Some(1)
    );
}

#[test]
fn import_summary_bounds_errors_without_raw_log_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("bad.log");
    std::fs::write(
        &path,
        "203.0.113.1 - - [10/Oct/2000:13:55:36 -0700] \"-\" 200 1\n",
    )
    .expect("bad log writes");
    let sources = resolve_log_sources(&[LogSourceDefinition {
        id: Some("bad".to_owned()),
        path,
        display_path: "bad.log".to_owned(),
        format: LogFormat::Common,
        origin: "https://example.test".to_owned(),
        field_map: None,
    }])
    .expect("source resolves");
    let summary =
        import_log_sources(&sources, &BotCatalog::builtin(), |_| {}).expect("import runs");
    assert_eq!(summary.parsed_entries, 0);
    assert_eq!(summary.parse_error_count, 1);
    assert_eq!(summary.error_samples.len(), 1);
    assert!(!summary.error_samples[0].reason.contains("203.0.113.1"));
}

#[test]
fn oversized_records_are_record_errors_and_later_rows_continue() {
    let dir = tempfile::tempdir().expect("temp dir");
    let oversized = "x".repeat(MAX_RAW_RECORD_BYTES + 1);

    let text_path = dir.path().join("text.log");
    std::fs::write(
        &text_path,
        format!(
            "{oversized}\n203.0.113.1 - - [10/Oct/2000:13:55:36 -0700] \"GET /ok HTTP/1.1\" 200 1\n"
        ),
    )
    .expect("text log writes");
    let sources = resolve_log_sources(&[LogSourceDefinition {
        id: Some("text".to_owned()),
        path: text_path,
        display_path: "text.log".to_owned(),
        format: LogFormat::Common,
        origin: "https://example.test".to_owned(),
        field_map: None,
    }])
    .expect("text source resolves");
    let summary =
        import_log_sources(&sources, &BotCatalog::builtin(), |_| {}).expect("text imports");
    assert_eq!(summary.total_records, 2);
    assert_eq!(summary.parse_error_count, 1);
    assert_eq!(summary.parsed_entries, 1);

    let jsonl_path = dir.path().join("edge.jsonl");
    std::fs::write(
        &jsonl_path,
        format!(
            "{oversized}\n{{\"EdgeStartTimestamp\":\"2000-10-10T20:55:36Z\",\"ClientRequestMethod\":\"GET\",\"ClientRequestURI\":\"/ok\",\"EdgeResponseStatus\":200}}\n"
        ),
    )
    .expect("jsonl log writes");
    let sources = resolve_log_sources(&[LogSourceDefinition {
        id: Some("jsonl".to_owned()),
        path: jsonl_path,
        display_path: "edge.jsonl".to_owned(),
        format: LogFormat::CloudflareJsonl,
        origin: "https://example.test".to_owned(),
        field_map: None,
    }])
    .expect("jsonl source resolves");
    let summary =
        import_log_sources(&sources, &BotCatalog::builtin(), |_| {}).expect("jsonl imports");
    assert_eq!(summary.total_records, 2);
    assert_eq!(summary.parse_error_count, 1);
    assert_eq!(summary.parsed_entries, 1);

    let csv_path = dir.path().join("edge.csv");
    std::fs::write(
        &csv_path,
        format!(
            "EdgeStartTimestamp,ClientRequestMethod,ClientRequestURI,EdgeResponseStatus\n2000-10-10T20:55:36Z,GET,/{oversized},200\n2000-10-10T20:56:36Z,GET,/ok,200\n"
        ),
    )
    .expect("csv log writes");
    let sources = resolve_log_sources(&[LogSourceDefinition {
        id: Some("csv".to_owned()),
        path: csv_path,
        display_path: "edge.csv".to_owned(),
        format: LogFormat::CloudflareCsv,
        origin: "https://example.test".to_owned(),
        field_map: None,
    }])
    .expect("csv source resolves");
    let summary =
        import_log_sources(&sources, &BotCatalog::builtin(), |_| {}).expect("csv imports");
    assert_eq!(summary.total_records, 2);
    assert_eq!(summary.parse_error_count, 1);
    assert_eq!(summary.parsed_entries, 1);
}

#[test]
fn json_sources_retain_and_validate_declared_host_evidence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cloudflare_path = dir.path().join("edge.jsonl");
    std::fs::write(
        &cloudflare_path,
        "\
{\"EdgeStartTimestamp\":\"2000-10-10T20:55:36Z\",\"ClientRequestMethod\":\"GET\",\"ClientRequestURI\":\"/hosted\",\"ClientRequestHost\":\"logs.example.test\",\"EdgeResponseStatus\":200}\n\
{\"EdgeStartTimestamp\":\"2000-10-10T20:56:36Z\",\"ClientRequestMethod\":\"GET\",\"ClientRequestURI\":\"/bad-host\",\"ClientRequestHost\":42,\"EdgeResponseStatus\":200}\n",
    )
    .expect("cloudflare log writes");

    let generic_path = dir.path().join("generic.jsonl");
    let oversized_host = "h".repeat(MAX_SCALAR_FIELD_BYTES + 1);
    std::fs::write(
        &generic_path,
        format!(
            "{{\"ts\":\"2000-10-10T20:55:36Z\",\"method\":\"GET\",\"path\":\"/generic-host\",\"status\":200,\"host\":\"generic.example.test\"}}\n{{\"ts\":\"2000-10-10T20:56:36Z\",\"method\":\"GET\",\"path\":\"/oversized-host\",\"status\":200,\"host\":\"{oversized_host}\"}}\n"
        ),
    )
    .expect("generic log writes");

    let mut map = generic_map();
    map.host = Some("host".to_owned());
    let sources = resolve_log_sources(&[
        LogSourceDefinition {
            id: Some("cloudflare".to_owned()),
            path: cloudflare_path,
            display_path: "edge.jsonl".to_owned(),
            format: LogFormat::CloudflareJsonl,
            origin: "https://example.test".to_owned(),
            field_map: None,
        },
        LogSourceDefinition {
            id: Some("generic".to_owned()),
            path: generic_path,
            display_path: "generic.jsonl".to_owned(),
            format: LogFormat::GenericJsonl,
            origin: "https://example.test".to_owned(),
            field_map: Some(map),
        },
    ])
    .expect("sources resolve");

    let mut entries = Vec::new();
    let summary = import_log_sources(&sources, &BotCatalog::builtin(), |entry| {
        entries.push(entry)
    })
    .expect("logs import");
    assert_eq!(summary.parsed_entries, 2);
    assert_eq!(summary.parse_error_count, 2);
    assert!(entries.iter().any(|entry| {
        entry.normalized_url == "https://example.test/hosted"
            && entry.observed_host.as_deref() == Some("logs.example.test")
    }));
    assert!(entries.iter().any(|entry| {
        entry.normalized_url == "https://example.test/generic-host"
            && entry.observed_host.as_deref() == Some("generic.example.test")
    }));
}
