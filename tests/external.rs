use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod support;

use auditroo::audit::{evaluate, Evaluation};
use auditroo::config::{
    resolve_settings_with_env, EnvironmentError, ResolvedSettings, SettingsOverrides,
};
use auditroo::external::{
    annotate_log_finding_targets, import_impact, ImpactError, ImpactJoinKind, ImpactSort,
    ImpactSourceState,
};
use auditroo::extract::{Headings, Indexability, SeoRecord};
use auditroo::findings::{Finding, Rule};
use auditroo::report::{Report, ReportCommand};
use auditroo::snapshot::Snapshot;
use tempfile::TempDir;

fn env(vars: &[(&str, &str)]) -> impl FnMut(&str) -> Result<String, EnvironmentError> {
    let map = vars
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<BTreeMap<_, _>>();
    move |name| map.get(name).cloned().ok_or(EnvironmentError::Missing)
}

fn fixture_path(relative: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(relative)
        .display()
        .to_string()
}

fn write_config(raw: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("auditroo.toml");
    std::fs::write(&path, raw).expect("config writes");
    (dir, path)
}

fn page(url: &str, title: &str) -> SeoRecord {
    SeoRecord {
        url: url.to_owned(),
        final_url: url.to_owned(),
        content_type: Some("text/html".to_owned()),
        indexability: Indexability {
            robots_meta: Vec::new(),
            googlebot_meta: Vec::new(),
            x_robots_tag: Vec::new(),
            is_indexable: true,
        },
        canonical: Some(url.to_owned()),
        title: Some(title.to_owned()),
        meta_description: Some("Page description".to_owned()),
        headings: Headings {
            h1: vec![title.to_owned()],
            h2: Vec::new(),
            h3: Vec::new(),
        },
        structured_data: Vec::new(),
        hreflang: Vec::new(),
        http_status: 200,
        redirect_chain: Vec::new(),
        rendered: false,
    }
}

fn snapshot(start_url: &str, records: Vec<SeoRecord>, _second: u8) -> Snapshot {
    support::snapshot_of(start_url, records)
}

async fn report_for(
    baseline: &Snapshot,
    current: &Snapshot,
    settings: &ResolvedSettings,
) -> Report {
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

fn finding<'a>(report: &'a Report, url: &str, rule: Rule) -> &'a Finding {
    report
        .findings
        .iter()
        .find(|finding| finding.url.as_deref() == Some(url) && finding.rule == rule)
        .unwrap_or_else(|| panic!("no {rule} finding for {url}: {:#?}", report.findings))
}

fn migrated_snapshots() -> (Snapshot, Snapshot) {
    (
        snapshot(
            "https://www.example.test/",
            vec![
                page("https://www.example.test/", "Home"),
                page("https://www.example.test/pricing", "Pricing"),
            ],
            0,
        ),
        snapshot(
            "https://staging.example.test/",
            vec![
                page("https://staging.example.test/", "Home"),
                page("https://staging.example.test/pricing", "Updated pricing"),
            ],
            1,
        ),
    )
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 0.000_001,
        "{actual} was not close to {expected}"
    );
}

fn write_search_console(path: &Path, rows: &[&str]) {
    let mut csv = "Page,Clicks,Impressions\n".to_owned();
    for row in rows {
        csv.push_str(row);
        csv.push('\n');
    }
    std::fs::write(path, csv).expect("CSV writes");
}

#[tokio::test]
async fn impact_join_precedence_is_applied_per_source() {
    let dir = tempfile::tempdir().expect("temp dir");
    let baseline_source = dir.path().join("baseline.csv");
    let current_source = dir.path().join("current.csv");
    write_search_console(
        &baseline_source,
        &["https://www.example.test/pricing,20,2000"],
    );
    write_search_console(
        &current_source,
        &["https://staging.example.test/pricing,10,1000"],
    );
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            [impact]
            enabled = true
            sort = "severity_then_impact"
            primary = {{ source = "current-source", metric = "impressions" }}

            [[impact.source]]
            id = "baseline-source"
            kind = "search_console_csv"
            path = '{}'
            required = true

            [[impact.source]]
            id = "current-source"
            kind = "search_console_csv"
            path = '{}'
            required = true
            "#,
            baseline_source.display(),
            current_source.display()
        ),
    )
    .expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");
    let (baseline, current) = migrated_snapshots();
    let report = report_for(&baseline, &current, &settings).await;

    let change = finding(
        &report,
        "https://staging.example.test/pricing",
        Rule::TitleChanged,
    );
    let annotations = &change.impact;
    assert_eq!(annotations.len(), 2);
    assert_eq!(annotations[0].source_id, "baseline-source");
    assert_eq!(annotations[0].join_kind, ImpactJoinKind::OriginRebased);
    assert_eq!(
        annotations[0].matched_url,
        "https://www.example.test/pricing"
    );
    assert_eq!(annotations[1].source_id, "current-source");
    assert_eq!(annotations[1].join_kind, ImpactJoinKind::Exact);
    assert_eq!(
        annotations[1].matched_url,
        "https://staging.example.test/pricing"
    );

    let log_annotations = annotate_log_finding_targets(
        report.impact.as_ref(),
        &["https://staging.example.test/pricing".to_owned()],
        &baseline.start_url,
        &current.start_url,
        false,
    );
    assert_eq!(log_annotations.len(), 2);
    assert!(log_annotations
        .iter()
        .any(|annotation| annotation.join_kind == ImpactJoinKind::Exact));
    assert!(log_annotations
        .iter()
        .any(|annotation| annotation.join_kind == ImpactJoinKind::OriginRebased));
}

#[tokio::test]
async fn impact_imports_aggregate_metrics_and_annotate_related_findings() {
    let config = format!(
        r#"
        [impact]
        enabled = true
        sort = "severity_then_impact"
        primary = {{ source = "gsc", metric = "impressions" }}

        [[impact.source]]
        id = "gsc"
        kind = "search_console_csv"
        path = '{}'
        required = true
        period_start = "2026-07-01"
        period_end = "2026-07-31"

        [quality]
        enabled = true
        gate = false

        [[quality.template]]
        id = "pricing"
        url = "https://staging.example.test/pricing"

        [[quality.source]]
        id = "lighthouse"
        template = "pricing"
        kind = "lighthouse_json"
        required = true
        path = '{}'

        [[quality.budget]]
        id = "perf-score"
        source = "lighthouse"
        metric = "performance.score"
        min = 0.85
        severity = "high"
        "#,
        fixture_path("tests/fixtures/external/search-console-pages.csv"),
        fixture_path("tests/fixtures/adapters/lighthouse-pricing.json"),
    );
    let (_dir, path) = write_config(&config);
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");
    let (baseline, current) = migrated_snapshots();
    let report = report_for(&baseline, &current, &settings).await;

    assert!(report.passed);
    let impact = report.impact.as_ref().expect("impact section exists");
    assert!(impact.enabled);
    assert_eq!(impact.sort, ImpactSort::SeverityThenImpact);
    assert_eq!(impact.import_counts.sources, 1);
    assert_eq!(impact.import_counts.url_contexts, 2);
    assert_eq!(impact.import_counts.annotations, 3);
    assert_eq!(impact.import_counts.unmatched_contexts, 1);

    let summary = &impact.source_summaries[0];
    assert_eq!(summary.rows, 4);
    assert_eq!(summary.usable_rows, 3);
    assert_eq!(summary.unique_urls, 2);
    assert_eq!(summary.duplicate_aggregate_count, 1);
    assert_eq!(summary.invalid_rows, 1);
    assert_eq!(summary.period_start.as_deref(), Some("2026-07-01"));
    assert_eq!(summary.period_end.as_deref(), Some("2026-07-31"));

    let pricing = impact
        .url_contexts
        .iter()
        .find(|context| context.normalized_url == "https://www.example.test/pricing")
        .expect("pricing context exists");
    assert_eq!(pricing.provenance, "external_context");
    assert_eq!(pricing.raw_url_samples.len(), 2);
    assert_close(pricing.metrics["clicks"], 120.0);
    assert_close(pricing.metrics["impressions"], 5000.0);
    assert_close(pricing.metrics["ctr"], 0.024);
    assert_close(pricing.metrics["position"], 4.8);

    let change = finding(
        &report,
        "https://staging.example.test/pricing",
        Rule::TitleChanged,
    );
    let annotation = &change.impact[0];
    assert_eq!(annotation.join_kind, ImpactJoinKind::OriginRebased);
    assert_eq!(annotation.provenance, "external_context");
    assert_eq!(annotation.matched_url, "https://www.example.test/pricing");

    let adapter = &finding(
        &report,
        "https://staging.example.test/pricing",
        Rule::QualityBudget,
    )
    .impact[0];
    assert_eq!(adapter.join_kind, ImpactJoinKind::OriginRebased);
    assert_eq!(adapter.context_id, pricing.context_id);
}

#[tokio::test]
async fn impact_annotations_remain_source_specific_for_shared_urls_and_log_targets() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = dir.path().join("first.csv");
    let second = dir.path().join("second.csv");
    write_search_console(&first, &["https://www.example.test/pricing,10,1000"]);
    write_search_console(&second, &["https://www.example.test/pricing,20,2000"]);
    let config = format!(
        r#"
        [impact]
        enabled = true
        sort = "severity_then_impact"
        primary = {{ source = "first-source", metric = "impressions" }}

        [[impact.source]]
        id = "first-source"
        kind = "search_console_csv"
        path = '{}'
        required = true

        [[impact.source]]
        id = "second-source"
        kind = "search_console_csv"
        path = '{}'
        required = true
        "#,
        first.display(),
        second.display()
    );
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(&config_path, config).expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");
    let (baseline, current) = migrated_snapshots();
    let report = report_for(&baseline, &current, &settings).await;

    let change = finding(
        &report,
        "https://staging.example.test/pricing",
        Rule::TitleChanged,
    );
    let annotations = &change.impact;
    assert_eq!(annotations.len(), 2);
    assert_eq!(
        annotations
            .iter()
            .map(|annotation| annotation.source_id.as_str())
            .collect::<Vec<_>>(),
        vec!["first-source", "second-source"]
    );
    assert!(annotations
        .iter()
        .all(|annotation| annotation.join_kind == ImpactJoinKind::OriginRebased));

    let log_annotations = annotate_log_finding_targets(
        report.impact.as_ref(),
        &["https://staging.example.test/pricing".to_owned()],
        &baseline.start_url,
        &current.start_url,
        false,
    );
    assert_eq!(log_annotations.len(), 2);
    assert!(log_annotations
        .iter()
        .all(|annotation| annotation.join_kind == ImpactJoinKind::OriginRebased));

    let human = auditroo::render::human::render(&report, Default::default());
    assert!(human.contains("1,000 impressions"), "{human}");
}

#[tokio::test]
async fn canonical_impact_fallback_keeps_context_from_each_source() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = dir.path().join("first.csv");
    let second = dir.path().join("second.csv");
    write_search_console(&first, &["https://example.test/pricing,5,500"]);
    write_search_console(&second, &["https://example.test/pricing,8,800"]);
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            [impact]
            enabled = true

            [[impact.source]]
            id = "first-source"
            kind = "search_console_csv"
            path = '{}'
            required = true

            [[impact.source]]
            id = "second-source"
            kind = "search_console_csv"
            path = '{}'
            required = true
            "#,
            first.display(),
            second.display()
        ),
    )
    .expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");
    let mut old_sale = page("https://example.test/sale", "Sale");
    old_sale.canonical = Some("https://example.test/pricing".to_owned());
    let mut new_sale = page("https://example.test/sale", "Updated Sale");
    new_sale.canonical = Some("https://example.test/pricing".to_owned());
    let baseline = snapshot(
        "https://example.test/",
        vec![
            page("https://example.test/", "Home"),
            old_sale,
            page("https://example.test/pricing", "Pricing"),
        ],
        0,
    );
    let current = snapshot(
        "https://example.test/",
        vec![
            page("https://example.test/", "Home"),
            new_sale,
            page("https://example.test/pricing", "Pricing"),
        ],
        1,
    );
    let report = report_for(&baseline, &current, &settings).await;

    let sale = finding(&report, "https://example.test/sale", Rule::TitleChanged);
    let annotations = &sale.impact;
    assert_eq!(annotations.len(), 2);
    assert!(annotations
        .iter()
        .all(|annotation| annotation.join_kind == ImpactJoinKind::Canonical));
    assert!(annotations
        .iter()
        .all(|annotation| annotation.matched_url == "https://example.test/pricing"));
}

#[test]
fn search_console_derives_ctr_without_ctr_column_and_rejects_bad_required_metrics() {
    let dir = tempfile::tempdir().expect("temp dir");
    let csv_path = dir.path().join("search.csv");
    write_search_console(
        &csv_path,
        &[
            "https://www.example.test/pricing,10,100",
            "https://www.example.test/blog,5,not-a-number",
        ],
    );
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            [impact]
            enabled = true

            [[impact.source]]
            id = "search"
            kind = "search_console_csv"
            path = '{}'
            required = true
            "#,
            csv_path.display()
        ),
    )
    .expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");

    let impact = import_impact(&settings.impact).expect("impact imports");

    let pricing = impact
        .url_contexts
        .iter()
        .find(|context| context.normalized_url == "https://www.example.test/pricing")
        .expect("pricing context exists");
    assert_close(pricing.metrics["ctr"], 0.1);
    let summary = &impact.source_summaries[0];
    assert_eq!(summary.rows, 2);
    assert_eq!(summary.usable_rows, 1);
    assert_eq!(summary.invalid_rows, 1);
    assert!(summary
        .warnings
        .iter()
        .any(|warning| warning.contains("the impressions value is not valid")));
}

#[test]
fn optional_impact_source_failures_preserve_byte_and_row_accounting() {
    let dir = tempfile::tempdir().expect("temp dir");
    let csv_path = dir.path().join("bad.csv");
    write_search_console(&csv_path, &["not-a-url,1,1"]);
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            [impact]
            enabled = true

            [[impact.source]]
            id = "optional-search"
            kind = "search_console_csv"
            path = '{}'
            required = false
            "#,
            csv_path.display()
        ),
    )
    .expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");

    let impact = import_impact(&settings.impact).expect("optional source is skipped");

    let summary = &impact.source_summaries[0];
    assert_eq!(summary.state, ImpactSourceState::SkippedOptional);
    assert!(summary.payload_bytes.is_some());
    assert!(summary
        .payload_sha256
        .as_ref()
        .is_some_and(|hash| hash.len() == 64));
    assert_eq!(summary.rows, 1);
    assert_eq!(summary.invalid_rows, 1);
    assert_eq!(impact.import_counts.sources, 1);
    assert_eq!(impact.import_counts.url_contexts, 0);
    assert!(summary
        .warnings
        .iter()
        .any(|warning| warning.contains("no_usable_rows")));
}

#[test]
fn analytics_imports_relative_urls_and_revenue_metrics() {
    let config = format!(
        r#"
        [impact]
        enabled = true
        sort = "stable"

        [[impact.source]]
        id = "analytics"
        kind = "analytics_csv"
        path = '{}'
        required = true
        origin = "https://www.example.test"
        currency = "USD"
        field_map = {{ url = "Landing page", sessions = "Sessions", pageviews = "Views", conversions = "Conversions", revenue = "Revenue" }}
        "#,
        fixture_path("tests/fixtures/external/analytics-pages.csv"),
    );
    let (_dir, path) = write_config(&config);
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");

    let impact = import_impact(&settings.impact).expect("impact imports");

    assert!(impact.enabled);
    assert_eq!(impact.sort, ImpactSort::Stable);
    assert_eq!(impact.import_counts.url_contexts, 2);
    let pricing = impact
        .url_contexts
        .iter()
        .find(|context| context.normalized_url == "https://www.example.test/pricing")
        .expect("pricing context exists");
    assert_eq!(pricing.currency.as_deref(), Some("USD"));
    assert_close(pricing.metrics["sessions"], 50.0);
    assert_close(pricing.metrics["pageviews"], 75.0);
    assert_close(pricing.metrics["conversions"], 4.0);
    assert_close(pricing.metrics["revenue"], 110.25);
    assert_eq!(pricing.raw_url_samples, vec!["/pricing", "/pricing"]);
    assert_eq!(impact.source_summaries[0].duplicate_aggregate_count, 1);
}

#[test]
fn required_impact_source_errors_are_not_downgraded_to_context_warnings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let csv_path = dir.path().join("duplicate-header.csv");
    std::fs::write(&csv_path, "Landing page,Sessions,Sessions\n/pricing,1,2\n")
        .expect("CSV writes");
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            [impact]
            enabled = true

            [[impact.source]]
            id = "analytics"
            kind = "analytics_csv"
            path = '{}'
            required = true
            origin = "https://www.example.test"
            field_map = {{ url = "Landing page", sessions = "Sessions" }}
            "#,
            csv_path.display()
        ),
    )
    .expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");

    let error = import_impact(&settings.impact).expect_err("required source fails");

    match error {
        ImpactError::Source(error) => {
            assert_eq!(error.source_id, "analytics");
            assert_eq!(error.code, "duplicate_header");
        }
        other => panic!("unexpected error: {other}"),
    }
}
