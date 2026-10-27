use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod support;

use auditroo::adapters::{
    evaluate_quality, BudgetComparison, QualityError, QualitySourceState, TargetVerification,
};
use auditroo::audit::{evaluate, Evaluation};
use auditroo::config::{resolve_settings_with_env, EnvironmentError, SettingsOverrides};
use auditroo::findings::{Rule, Severity};
use auditroo::report::ReportCommand;
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

fn quality_config(extra_sources: &str, extra_budgets: &str) -> String {
    format!(
        r#"
        [gate]
        fail_on = "high"

        [quality]
        enabled = true
        gate = true

        [[quality.template]]
        id = "pricing"
        url = "https://staging.example.test/pricing"

        [[quality.source]]
        id = "lighthouse"
        template = "pricing"
        kind = "lighthouse_json"
        required = true
        path = '{}'

        [[quality.source]]
        id = "pagespeed"
        template = "pricing"
        kind = "pagespeed_json"
        required = true
        path = '{}'

        [[quality.source]]
        id = "pa11y"
        template = "pricing"
        kind = "pa11y_json"
        required = true
        path = '{}'

        [[quality.source]]
        id = "html"
        template = "pricing"
        kind = "nu_html_json"
        required = true
        path = '{}'

        {extra_sources}

        [[quality.budget]]
        id = "perf-score"
        source = "lighthouse"
        metric = "performance.score"
        min = 0.85
        severity = "high"

        [[quality.budget]]
        id = "page-speed-accessibility"
        source = "pagespeed"
        metric = "accessibility.score"
        min = 0.9
        severity = "medium"

        [[quality.budget]]
        id = "pa11y-errors"
        source = "pa11y"
        metric = "accessibility.error_count"
        max = 0
        severity = "critical"

        [[quality.budget]]
        id = "html-warnings"
        source = "html"
        metric = "html.warning_count"
        max = 0
        severity = "medium"

        {extra_budgets}
        "#,
        fixture_path("tests/fixtures/adapters/lighthouse-pricing.json"),
        fixture_path("tests/fixtures/adapters/pagespeed-pricing.json"),
        fixture_path("tests/fixtures/adapters/pa11y-pricing.json"),
        fixture_path("tests/fixtures/adapters/nu-html-pricing.json"),
    )
}

#[tokio::test]
async fn quality_adapters_emit_budget_findings_with_adapter_provenance() {
    let (_dir, path) = write_config(&quality_config("", ""));
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");

    let evaluation = evaluate_quality(&settings.quality)
        .await
        .expect("quality evaluation succeeds");

    assert!(evaluation.enabled);
    assert!(evaluation.gate);
    assert_eq!(evaluation.source_summaries.len(), 4);
    assert_eq!(evaluation.adapter_findings.len(), 4);
    assert!(evaluation.source_summaries.iter().all(|summary| {
        summary.state == QualitySourceState::Succeeded
            && summary
                .payload_sha256
                .as_deref()
                .is_some_and(|hash| hash.len() == 64)
    }));
    let summaries = evaluation
        .source_summaries
        .iter()
        .map(|summary| (summary.source_id.as_str(), summary))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        summaries["lighthouse"].lighthouse_version.as_deref(),
        Some("12.0.0")
    );
    assert_eq!(
        summaries["lighthouse"].final_displayed_url.as_deref(),
        Some("https://staging.example.test/pricing")
    );
    assert_eq!(
        summaries["pagespeed"].lighthouse_version.as_deref(),
        Some("12.0.0")
    );

    let by_budget = evaluation
        .adapter_findings
        .iter()
        .map(|finding| (finding.budget_id.as_str(), finding))
        .collect::<BTreeMap<_, _>>();
    let lighthouse = by_budget["perf-score"];
    assert_eq!(lighthouse.provenance, "adapter");
    assert_eq!(lighthouse.template_id, "pricing");
    assert_eq!(lighthouse.url, "https://staging.example.test/pricing");
    assert_eq!(lighthouse.observed, 0.72);
    assert_eq!(lighthouse.expected.comparison, BudgetComparison::Min);
    assert_eq!(lighthouse.expected.unit, "ratio");
    assert_eq!(
        lighthouse.source.target_verification,
        TargetVerification::Matched
    );
    assert_eq!(
        lighthouse.source.lighthouse_version.as_deref(),
        Some("12.0.0")
    );
    assert_eq!(
        lighthouse.source.final_displayed_url.as_deref(),
        Some("https://staging.example.test/pricing")
    );
    assert_eq!(lighthouse.id, "adapter::lighthouse::pricing::perf-score");
    assert!(lighthouse
        .source
        .payload_sha256
        .as_deref()
        .is_some_and(|hash| hash.len() == 64));

    let pa11y = by_budget["pa11y-errors"];
    assert_eq!(pa11y.observed, 1.0);
    assert_eq!(pa11y.expected.comparison, BudgetComparison::Max);
    assert!(pa11y
        .evidence
        .iter()
        .any(|evidence| evidence.selector.as_deref() == Some("img.hero")));

    // Failed budgets become findings of the normal report and gate.
    let snapshot = support::snapshot_of(
        "https://staging.example.test/",
        vec![support::page_record("https://staging.example.test/")],
    );
    let report = evaluate(Evaluation {
        command: ReportCommand::Check,
        baseline: None,
        current: &snapshot,
        settings: &settings,
        notes: Vec::new(),
    })
    .await
    .expect("report evaluates");
    let budgets: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| finding.rule == Rule::QualityBudget)
        .collect();
    assert_eq!(budgets.len(), 4);
    assert!(budgets.iter().any(
        |finding| finding.check_id.as_deref() == Some("pa11y-errors")
            && finding.severity == Severity::Critical
    ));
    assert!(!report.passed);
    assert_eq!(report.quality.as_ref().map(Vec::len), Some(4));
    let junit = auditroo::render::junit::render(&report);
    assert!(junit.contains("auditroo.quality_budget"));
}

// Uses Unix tools (sh, yes) as stand-ins for quality tools.
#[cfg(unix)]
#[tokio::test]
async fn command_quality_source_stops_when_stdout_exceeds_payload_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(
        &config_path,
        r#"
        [quality]
        enabled = true

        [[quality.template]]
        id = "pricing"
        url = "https://staging.example.test/pricing"

        [[quality.source]]
        id = "html-cli"
        template = "pricing"
        kind = "nu_html_command"
        required = true
        argv = ["yes", "{template_url}"]
        timeout_seconds = 5

        [[quality.budget]]
        id = "errors"
        source = "html-cli"
        metric = "html.error_count"
        max = 0
        severity = "high"
        "#,
    )
    .expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");

    let error = evaluate_quality(&settings.quality)
        .await
        .expect_err("oversized command stdout fails");

    match error {
        QualityError::Source(error) => {
            assert_eq!(error.source_id, "html-cli");
            assert_eq!(error.code, "stdout_too_large");
            assert!(error.stdout_truncated);
            assert!(!error.timed_out);
        }
    }
}

#[tokio::test]
async fn optional_quality_source_failures_are_reported_without_failing_the_run() {
    let missing = fixture_path("tests/fixtures/adapters/missing.json");
    let config = format!(
        r#"
        [quality]
        enabled = true
        gate = true

        [[quality.template]]
        id = "pricing"
        url = "https://staging.example.test/pricing"

        [[quality.source]]
        id = "optional-source"
        template = "pricing"
        kind = "pa11y_json"
        required = false
        path = '{missing}'

        [[quality.budget]]
        id = "errors"
        source = "optional-source"
        metric = "accessibility.error_count"
        max = 0
        severity = "high"
        "#
    );
    let (_dir, path) = write_config(&config);
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");

    let evaluation = evaluate_quality(&settings.quality)
        .await
        .expect("optional failures are summarized");

    assert_eq!(evaluation.adapter_findings.len(), 0);
    assert_eq!(evaluation.source_summaries.len(), 1);
    let summary = &evaluation.source_summaries[0];
    assert_eq!(summary.state, QualitySourceState::SkippedOptional);
    assert_eq!(summary.budgets_unevaluated, 1);
    assert!(summary.warnings[0].contains("source_unavailable"));
}

#[tokio::test]
async fn optional_quality_parse_failures_keep_payload_provenance() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source_path = dir.path().join("mismatched-lighthouse.json");
    let raw = r#"{
      "requestedUrl": "https://other.example.test/pricing",
      "finalDisplayedUrl": "https://other.example.test/pricing",
      "lighthouseVersion": "12.0.0",
      "categories": {
        "performance": { "score": 0.5, "title": "Performance" }
      }
    }"#;
    std::fs::write(&source_path, raw).expect("source writes");
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            [quality]
            enabled = true
            gate = false

            [[quality.template]]
            id = "pricing"
            url = "https://staging.example.test/pricing"

            [[quality.source]]
            id = "optional-lighthouse"
            template = "pricing"
            kind = "lighthouse_json"
            required = false
            path = '{}'

            [[quality.budget]]
            id = "perf-score"
            source = "optional-lighthouse"
            metric = "performance.score"
            min = 0.9
            severity = "high"
            "#,
            source_path.display()
        ),
    )
    .expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");

    let evaluation = evaluate_quality(&settings.quality)
        .await
        .expect("optional target mismatch is summarized");

    let summary = &evaluation.source_summaries[0];
    assert_eq!(summary.state, QualitySourceState::SkippedOptional);
    assert_eq!(summary.target_verification, TargetVerification::Mismatch);
    assert_eq!(summary.payload_bytes, Some(raw.len() as u64));
    assert!(summary
        .payload_sha256
        .as_deref()
        .is_some_and(|hash| hash.len() == 64));
    assert_eq!(summary.lighthouse_version.as_deref(), Some("12.0.0"));
    assert_eq!(
        summary.final_displayed_url.as_deref(),
        Some("https://other.example.test/pricing")
    );
}

#[tokio::test]
async fn lighthouse_metrics_reject_missing_units_and_out_of_range_scores() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing_unit = dir.path().join("missing-unit.json");
    std::fs::write(
        &missing_unit,
        r#"{
          "requestedUrl": "https://staging.example.test/pricing",
          "categories": { "performance": { "score": 0.8 } },
          "audits": {
            "largest-contentful-paint": {
              "numericValue": 1200,
              "title": "Largest Contentful Paint"
            }
          }
        }"#,
    )
    .expect("source writes");
    let invalid_score = dir.path().join("invalid-score.json");
    std::fs::write(
        &invalid_score,
        r#"{
          "requestedUrl": "https://staging.example.test/pricing",
          "categories": { "performance": { "score": 1.2 } }
        }"#,
    )
    .expect("source writes");

    for (path, expected_code, metric) in [
        (
            missing_unit.as_path(),
            "unit_mismatch",
            "performance.largest_contentful_paint_ms",
        ),
        (
            invalid_score.as_path(),
            "invalid_metric",
            "performance.score",
        ),
    ] {
        let config_path = dir.path().join(format!("{expected_code}-auditroo.toml"));
        std::fs::write(
            &config_path,
            format!(
                r#"
                [quality]
                enabled = true

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
                id = "budget"
                source = "lighthouse"
                metric = "{metric}"
                max = 2000
                severity = "high"
                "#,
                path.display()
            ),
        )
        .expect("config writes");
        let settings =
            resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
                .expect("config resolves");
        let error = evaluate_quality(&settings.quality)
            .await
            .expect_err("malformed metric fails the source");
        match error {
            QualityError::Source(error) => assert_eq!(error.code, expected_code),
        }
    }
}

#[tokio::test]
async fn required_quality_source_failures_return_operational_errors() {
    let missing = fixture_path("tests/fixtures/adapters/missing.json");
    let config = format!(
        r#"
        [quality]
        enabled = true

        [[quality.template]]
        id = "pricing"
        url = "https://staging.example.test/pricing"

        [[quality.source]]
        id = "required-source"
        template = "pricing"
        kind = "nu_html_json"
        required = true
        path = '{missing}'

        [[quality.budget]]
        id = "errors"
        source = "required-source"
        metric = "html.error_count"
        max = 0
        severity = "high"
        "#
    );
    let (_dir, path) = write_config(&config);
    let settings = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
        .expect("config resolves");

    let error = evaluate_quality(&settings.quality)
        .await
        .expect_err("required source fails");

    match error {
        QualityError::Source(error) => {
            assert_eq!(error.source_id, "required-source");
            assert_eq!(error.code, "source_unavailable");
        }
    }
}

#[tokio::test]
async fn quality_json_rejects_duplicate_keys_before_budget_evaluation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let duplicate_path = dir.path().join("duplicate.json");
    std::fs::write(
        &duplicate_path,
        r#"{"messages":[{"type":"error"}],"messages":[]}"#,
    )
    .expect("fixture writes");
    let config = format!(
        r#"
        [quality]
        enabled = true

        [[quality.template]]
        id = "pricing"
        url = "https://staging.example.test/pricing"

        [[quality.source]]
        id = "html"
        template = "pricing"
        kind = "nu_html_json"
        required = true
        path = '{}'

        [[quality.budget]]
        id = "errors"
        source = "html"
        metric = "html.error_count"
        max = 0
        severity = "high"
        "#,
        duplicate_path.display()
    );
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(&config_path, config).expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");

    let error = evaluate_quality(&settings.quality)
        .await
        .expect_err("duplicate JSON key is rejected");

    match error {
        QualityError::Source(error) => {
            assert_eq!(error.source_id, "html");
            assert_eq!(error.code, "duplicate_json_key");
        }
    }
}

// Uses Unix tools (sh, yes) as stand-ins for quality tools.
#[cfg(unix)]
#[tokio::test]
async fn command_quality_sources_expand_template_urls_and_accept_configured_exit_codes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let script = dir.path().join("pa11y-fixture.sh");
    std::fs::write(
        &script,
        r#"printf '%s' '[{"type":"error","selector":"main img","message":"Missing alt"}]'; exit 3
"#,
    )
    .expect("script writes");
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            [quality]
            enabled = true

            [[quality.template]]
            id = "pricing"
            url = "https://staging.example.test/pricing"

            [[quality.source]]
            id = "pa11y-cli"
            template = "pricing"
            kind = "pa11y_command"
            required = true
            argv = ["sh", '{}', "{{template_url}}"]
            accepted_exit_codes = [0, 3]
            timeout_seconds = 5

            [[quality.budget]]
            id = "errors"
            source = "pa11y-cli"
            metric = "accessibility.error_count"
            max = 0
            severity = "high"
            "#,
            script.display()
        ),
    )
    .expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");

    let evaluation = evaluate_quality(&settings.quality)
        .await
        .expect("command source succeeds");

    assert_eq!(evaluation.adapter_findings.len(), 1);
    assert_eq!(evaluation.adapter_findings[0].source_id, "pa11y-cli");
    assert_eq!(evaluation.adapter_findings[0].observed, 1.0);
}

// Uses Unix tools (sh, yes) as stand-ins for quality tools.
#[cfg(unix)]
#[tokio::test]
async fn command_quality_source_errors_preserve_unaccepted_exit_status() {
    let dir = tempfile::tempdir().expect("temp dir");
    let script = dir.path().join("pa11y-fail.sh");
    std::fs::write(&script, r#"printf '%s' '[]'; exit 7"#).expect("script writes");
    let config_path = dir.path().join("auditroo.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
            [quality]
            enabled = true

            [[quality.template]]
            id = "pricing"
            url = "https://staging.example.test/pricing"

            [[quality.source]]
            id = "pa11y-cli"
            template = "pricing"
            kind = "pa11y_command"
            required = true
            argv = ["sh", '{}', "{{template_url}}"]
            accepted_exit_codes = [0]
            timeout_seconds = 5

            [[quality.budget]]
            id = "errors"
            source = "pa11y-cli"
            metric = "accessibility.error_count"
            max = 0
            severity = "high"
            "#,
            script.display()
        ),
    )
    .expect("config writes");
    let settings =
        resolve_settings_with_env(Some(&config_path), SettingsOverrides::default(), env(&[]))
            .expect("config resolves");

    let error = evaluate_quality(&settings.quality)
        .await
        .expect_err("unaccepted exit status fails");

    match error {
        QualityError::Source(error) => {
            assert_eq!(error.source_id, "pa11y-cli");
            assert_eq!(error.code, "unaccepted_exit_status");
            assert_eq!(error.exit_status, Some(7));
            assert!(!error.timed_out);
            assert!(!error.stdout_truncated);
        }
    }
}
