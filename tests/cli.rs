//! The `auditroo` binary end to end: commands, files, output formats and exit
//! codes. The demo sites serve as fixtures.

mod support;

use assert_cmd::Command;
use auditroo::demo::{DemoServer, Variant};
use predicates::prelude::*;
use serde_json::Value;
use support::site::DocsSite;
use tempfile::TempDir;

fn auditroo(dir: &TempDir) -> Command {
    let mut command = Command::cargo_bin("auditroo").expect("binary exists");
    command.current_dir(dir.path()).env_remove("NO_COLOR");
    command
}

#[test]
fn help_has_descriptions_defaults_and_examples() {
    let dir = tempfile::tempdir().unwrap();
    auditroo(&dir)
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("check"))
        .stdout(predicate::str::contains("compare"))
        .stdout(predicate::str::contains("demo"))
        .stdout(predicate::str::contains("Exit codes:"))
        .stdout(predicate::str::contains("3  network or runtime error"));
    auditroo(&dir)
        .args(["check", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[default: 10000]"))
        .stdout(predicate::str::contains("[default: high"))
        .stdout(predicate::str::contains("Examples:"))
        .stdout(predicate::str::contains("--ignore-robots"));
    auditroo(&dir)
        .args(["compare", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("<PRODUCTION_URL> <PREVIEW_URL>"));
}

#[test]
fn demo_shows_a_failing_comparison_without_network() {
    let dir = tempfile::tempdir().unwrap();
    auditroo(&dir)
        .arg("demo")
        .assert()
        .success()
        .stdout(predicate::str::contains("FAIL"))
        .stdout(predicate::str::contains(
            "/coffee/ethiopia-yirgacheffe/ now has noindex",
        ))
        .stdout(predicate::str::contains("staging.harbor-coffee.test"))
        .stdout(predicate::str::contains("robots.txt now blocks Googlebot"))
        .stdout(predicate::str::contains(
            "what to do: Remove the noindex from the robots meta tag",
        ))
        .stdout(predicate::str::contains("      rule: noindex_added\n"))
        // Findings below the fail-on level get no advice line.
        .stdout(predicate::str::contains("rule: title_changed").not())
        .stdout(predicate::str::contains("exit with code 1"));

    let output = auditroo(&dir)
        .args(["demo", "--json", "--html", "demo.html"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["artifact_kind"], "report");
    assert_eq!(report["passed"], false);
    let html = std::fs::read_to_string(dir.path().join("demo.html")).unwrap();
    assert!(html.contains("<html"));
    // Narrow screens must not break the line inside a flag.
    assert!(html.contains("pass <span class=\"flag\">--production-url</span> with"));

    auditroo(&dir)
        .args(["demo", "--format", "markdown"])
        .assert()
        .success()
        .stdout(predicate::str::contains("### auditroo:"))
        .stdout(predicate::str::contains("[!CAUTION]"))
        .stdout(predicate::str::contains("| Critical |"))
        .stdout(predicate::str::contains("| What to do | Rule |"))
        .stdout(predicate::str::contains(
            "| Fix the JSON syntax. | `rule: structured_data_invalid` |",
        ));
}

#[tokio::test(flavor = "multi_thread")]
async fn check_compare_and_report_against_the_demo_sites() {
    let production = DemoServer::start(Variant::Production).await.unwrap();
    let preview = DemoServer::start(Variant::Preview).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    auditroo(&dir)
        .args([
            "check",
            &production.url(),
            "--rate-limit-ms",
            "0",
            "--quiet",
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("PASS"));
    let output = auditroo(&dir)
        .args(["check", &preview.url(), "--rate-limit-ms", "0", "--quiet"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("/shipping/ returns 404"))
        .get_output()
        .stdout
        .clone();
    // /about/ and /shipping/ are two broken links in one group. Each of them
    // says what to do, not only the last one.
    let output = String::from_utf8(output).unwrap();
    assert_eq!(
        output.matches("      rule: broken_link\n").count(),
        2,
        "{output}"
    );
    let about = output.find("/about/ returns 500").expect("about finding");
    let shipping = output.find("/shipping/ returns 404").expect("shipping");
    assert!(
        output[about.min(shipping)..about.max(shipping)]
            .contains("what to do: Fix the linked page"),
        "{output}"
    );

    auditroo(&dir)
        .args([
            "compare",
            &production.url(),
            &preview.url(),
            "--rate-limit-ms",
            "0",
            "--out",
            "report.json",
            "--html",
            "report.html",
            "--junit",
            "auditroo.xml",
            "--markdown",
            "summary.md",
        ])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("problems would hurt search"))
        .stderr(predicate::str::contains("Crawling production"));
    for file in ["report.json", "report.html", "auditroo.xml", "summary.md"] {
        assert!(dir.path().join(file).exists(), "{file} was written");
    }
    let junit = std::fs::read_to_string(dir.path().join("auditroo.xml")).unwrap();
    assert!(junit.contains("<failure"));

    auditroo(&dir)
        .args(["report", "report.json", "--format", "github"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("::error title=auditroo"));
    auditroo(&dir)
        .args(["report", "report.json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("FAIL"));

    // Lower the bar and ignore the rest: the gate passes.
    auditroo(&dir)
        .args([
            "compare",
            &production.url(),
            &preview.url(),
            "--rate-limit-ms",
            "0",
            "--quiet",
            "--fail-on",
            "critical",
            "--ignore",
            "{ rule = \"noindex_added\" }",
            "--ignore",
            "{ rule = \"page_broken\" }",
            "--ignore",
            "{ rule = \"canonical_external\" }",
            "--ignore",
            "{ rule = \"robots_blocked\" }",
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("matched an ignore rule"));
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_then_diff() {
    let production = DemoServer::start(Variant::Production).await.unwrap();
    let preview = DemoServer::start(Variant::Preview).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    auditroo(&dir)
        .args([
            "snapshot",
            &production.url(),
            "--out",
            "baseline.json",
            "--rate-limit-ms",
            "0",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Saved a snapshot"))
        .stdout(predicate::str::contains("Next: auditroo diff"));
    auditroo(&dir)
        .args(["report", "baseline.json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Snapshot of"));
    // A snapshot has no findings, so report-only outputs are an error
    // instead of a silent success that writes nothing.
    auditroo(&dir)
        .args(["report", "baseline.json", "--html", "snapshot.html"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("this file is a snapshot"));
    assert!(!dir.path().join("snapshot.html").exists());

    auditroo(&dir)
        .args([
            "diff",
            &production.url(),
            "--baseline",
            "baseline.json",
            "--rate-limit-ms",
            "0",
            "-q",
        ])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("No SEO regressions found"));
    let output = auditroo(&dir)
        .args([
            "diff",
            &preview.url(),
            "--baseline",
            "baseline.json",
            "--rate-limit-ms",
            "0",
            "-q",
            "--json",
        ])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["command"], "diff");
    assert!(report["counts"]["critical"].as_u64().unwrap() >= 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_and_config_errors_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    auditroo(&dir)
        .args(["check", "example.com"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("https://example.com/"));
    auditroo(&dir)
        .args(["diff", "http://127.0.0.1:9/", "--baseline", "missing.json"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("missing.json"))
        .stderr(predicate::str::contains("file not found"));
    // The output folder is checked before the crawl starts, so the closed
    // port is never contacted and the error is a usage error.
    auditroo(&dir)
        .args([
            "snapshot",
            "http://127.0.0.1:9/",
            "--out",
            "missing/baseline.json",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("does not exist"));
    auditroo(&dir)
        .args(["check", "http://127.0.0.1:9/", "--no-such-flag"])
        .assert()
        .code(2);
    // A crawl pattern without a leading slash would match nothing.
    auditroo(&dir)
        .args(["check", "http://127.0.0.1:9/", "--exclude", "search*"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("\"/search*\""));
    auditroo(&dir)
        .args(["check", "http://user:s3cr3t@127.0.0.1:9/"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("s3cr3t").not());
    std::fs::write(
        dir.path().join("auditroo.toml"),
        "[crawl]\nconcurrency = \"4\"\n",
    )
    .unwrap();
    auditroo(&dir)
        .args(["check", "http://127.0.0.1:9/"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("auditroo.toml"))
        .stderr(predicate::str::contains("\"4\""));
    std::fs::write(
        dir.path().join("auditroo.toml"),
        "[crawl]\nheaders = [{ name = \"x-test\", value = \"${MISSING_SECRET}\" }]\n",
    )
    .unwrap();
    auditroo(&dir)
        .args(["check", "http://127.0.0.1:9/"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("MISSING_SECRET"));
    if cfg!(not(feature = "browser")) {
        std::fs::remove_file(dir.path().join("auditroo.toml")).unwrap();
        auditroo(&dir)
            .args(["check", "http://127.0.0.1:9/", "--render-js"])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("browser"));
    }
}

#[test]
fn network_errors_exit_3_with_the_cause() {
    let dir = tempfile::tempdir().unwrap();
    auditroo(&dir)
        .args(["check", "http://127.0.0.1:9/", "--quiet"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("refused"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_site_that_cannot_be_crawled_is_never_a_baseline() {
    // Production behind a firewall that answers 403 to auditroo.
    let mut blocked = DocsSite::new();
    let paths: Vec<String> = blocked.routes.keys().cloned().collect();
    for path in paths {
        blocked.set(&path, support::site::Route::status(403));
    }
    let blocked = blocked.serve(None).await;
    let preview = DocsSite::new().serve(None).await;
    let dir = tempfile::tempdir().unwrap();
    auditroo(&dir)
        .args([
            "compare",
            blocked.url("/").as_str(),
            preview.url("/").as_str(),
            "--rate-limit-ms",
            "0",
            "-q",
        ])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("production at"))
        .stderr(predicate::str::contains("the start page returns 403"));
    auditroo(&dir)
        .args([
            "snapshot",
            blocked.url("/").as_str(),
            "--out",
            "baseline.json",
            "--rate-limit-ms",
            "0",
            "-q",
        ])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("nothing was saved"));
    assert!(!dir.path().join("baseline.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn monitoring_never_passes_or_promotes_a_crawl_that_robots_txt_stopped() {
    let server = DocsSite::new().serve(None).await;
    let dir = tempfile::tempdir().unwrap();
    auditroo(&dir)
        .args([
            "snapshot",
            server.url("/").as_str(),
            "--out",
            "baseline.json",
            "--rate-limit-ms",
            "0",
            "-q",
        ])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("auditroo.toml"),
        format!(
            r#"
            [crawl]
            rate_limit_ms = 0

            [history]
            database = "history.sqlite3"
            artifact_root = "runs"
            baseline_root = "baselines"

            [[site]]
            id = "docs"
            start_url = "{start}"
            baseline = {{ mode = "label", label = "last-known-good", bootstrap_path = "baseline.json" }}
            "#,
            start = server.url("/"),
        ),
    )
    .unwrap();
    // robots.txt now blocks auditroo, while Googlebot may still crawl.
    server
        .set(
            "/robots.txt",
            200,
            "text/plain",
            "User-agent: auditroo\nDisallow: /\n\nUser-agent: *\nAllow: /\n",
        )
        .await;
    let output = auditroo(&dir)
        .args(["monitor", "run", "--site", "docs", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        result["site_results"][0]["audit_result"],
        "operational_error"
    );
    assert!(result["site_results"][0]["operational_error"]
        .as_str()
        .unwrap()
        .contains("blocks auditroo but not Googlebot"));
}

#[tokio::test(flavor = "multi_thread")]
async fn headers_reach_pages_robots_and_sitemaps_of_the_start_host() {
    let server = DocsSite::new().serve(None).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("auditroo.toml"),
        "[crawl]\nrate_limit_ms = 0\nheaders = [{ name = \"x-auditroo-bypass\", value = \"${AUDITROO_SECRET}\" }]\n",
    )
    .unwrap();
    auditroo(&dir)
        .env("AUDITROO_SECRET", "owner-secret")
        .args([
            "snapshot",
            server.url("/").as_str(),
            "--out",
            "baseline.json",
        ])
        .assert()
        .success();
    let requests = server.requests().await;
    for wanted in ["/robots.txt", "/sitemap-index.xml", "/docs/"] {
        let request = requests
            .iter()
            .find(|request| request.path == wanted)
            .unwrap_or_else(|| panic!("{wanted} was requested"));
        assert_eq!(
            request.header_value("x-auditroo-bypass").as_deref(),
            Some("owner-secret")
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn noindex_headers_can_be_ignored_for_previews() {
    let production = DocsSite::new().serve(None).await;
    let mut preview_site = DocsSite::new();
    for path in preview_site.routes.keys().cloned().collect::<Vec<_>>() {
        preview_site.header(&path, "x-robots-tag", "noindex");
    }
    let preview = preview_site.serve(None).await;
    let dir = tempfile::tempdir().unwrap();
    let compare = |extra: &[&str]| {
        let mut command = auditroo(&dir);
        command.args([
            "compare",
            production.url("/").as_str(),
            preview.url("/").as_str(),
            "--rate-limit-ms",
            "0",
            "--quiet",
        ]);
        command.args(extra);
        command
    };
    compare(&[]).assert().code(1);
    compare(&["--ignore-noindex-header"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("--ignore-noindex-header"));
}

#[test]
fn recipes_work_with_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    auditroo(&dir)
        .args(["recipes", "copy", "product-pages"])
        .assert()
        .success();
    assert!(dir.path().join("auditroo.toml").exists());
    auditroo(&dir)
        .args([
            "recipes",
            "copy",
            "documentation-site",
            "--config",
            "docs.toml",
        ])
        .assert()
        .success();
    assert!(dir.path().join("docs.toml").exists());
    auditroo(&dir)
        .args([
            "recipes",
            "copy",
            "checkout-and-pricing",
            "--config",
            "x.toml",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would append"));
    assert!(!dir.path().join("x.toml").exists());
    auditroo(&dir)
        .args(["recipes", "copy", "unknown"])
        .assert()
        .code(2);
}

/// A quality command that runs from the folder of a bare `auditroo.toml`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn quality_commands_run_with_an_implicit_config() {
    let server = DocsSite::new().serve(None).await;
    let dir = tempfile::tempdir().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/adapters/lighthouse-pricing.json");
    std::fs::copy(&fixture, dir.path().join("lighthouse.json")).unwrap();
    std::fs::write(
        dir.path().join("auditroo.toml"),
        r#"
        [crawl]
        rate_limit_ms = 0

        [quality]
        enabled = true
        gate = true

        [[quality.template]]
        id = "pricing"
        url = "https://staging.example.test/pricing"

        [[quality.source]]
        id = "lighthouse"
        template = "pricing"
        kind = "lighthouse_command"
        required = true
        argv = ["sh", "-c", "cat lighthouse.json", "{template_url}"]

        [[quality.budget]]
        id = "perf-score"
        source = "lighthouse"
        metric = "performance.score"
        min = 0.85
        severity = "high"
        "#,
    )
    .unwrap();
    auditroo(&dir)
        .args(["check", server.url("/").as_str(), "--quiet"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("perf-score"));
}

#[tokio::test(flavor = "multi_thread")]
async fn log_commands() {
    let production = DemoServer::start(Variant::Production).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    auditroo(&dir)
        .args([
            "snapshot",
            &production.url(),
            "--out",
            "baseline.json",
            "--rate-limit-ms",
            "0",
            "-q",
        ])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("auditroo.toml"),
        r#"
        [crawl]
        headers = [{ name = "x-auditroo-bypass", value = "${MISSING_SECRET}" }]

        [[logs.bot_rule]]
        id = "example-crawler"
        family_id = "example-crawler"
        display_name = "Example Crawler"
        client_type = "generic_automated"
        pattern = "ExampleCrawler/"
        "#,
    )
    .unwrap();
    auditroo(&dir)
        .args(["logs", "bots", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "\"catalog_version\": \"builtin-2026-10\"",
        ))
        .stderr(predicate::str::contains("MISSING_SECRET").not());
    auditroo(&dir)
        .args(["logs", "bots"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Example Crawler"))
        .stdout(predicate::str::contains("OAI-SearchBot"));

    std::fs::write(
        dir.path().join("access.log"),
        "203.0.113.1 - - [10/Oct/2026:13:55:36 +0000] \"GET /brewing-guide/ HTTP/1.1\" 403 1 \"-\" \"Mozilla/5.0 (compatible; GPTBot/1.2)\"\n203.0.113.2 - - [10/Oct/2026:13:55:37 +0000] \"GET / HTTP/1.1\" 200 1 \"-\" \"Mozilla/5.0 (compatible; Googlebot/2.1)\"\n",
    )
    .unwrap();
    let origin = production.url().trim_end_matches('/').to_owned();
    let output = auditroo(&dir)
        .args([
            "logs",
            "analyze",
            "--snapshot",
            "baseline.json",
            "--logs",
            "access.log",
            "--log-format",
            "combined",
            "--log-origin",
            &origin,
            "--json",
            "--out",
            "log-report.json",
            "--fail-on",
            "medium",
        ])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["artifact_kind"], "log_analysis_report");
    assert!(dir.path().join("log-report.json").exists());
    auditroo(&dir)
        .args(["report", "log-report.json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("GPTBot"));
    auditroo(&dir)
        .args([
            "logs",
            "analyze",
            "--snapshot",
            "baseline.json",
            "--logs",
            "access.log",
            "--log-format",
            "combined",
            "--log-origin",
            &origin,
            "--out",
            "auditroo.toml",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("config"));
}

#[tokio::test(flavor = "multi_thread")]
async fn monitor_history_baseline_and_bundle() {
    let production = DemoServer::start(Variant::Production).await.unwrap();
    let preview = DemoServer::start(Variant::Preview).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    auditroo(&dir)
        .args([
            "snapshot",
            &production.url(),
            "--out",
            "baseline.json",
            "--rate-limit-ms",
            "0",
            "-q",
        ])
        .assert()
        .success();
    std::fs::write(
        dir.path().join("access.log"),
        "203.0.113.1 - - [10/Oct/2026:13:55:36 +0000] \"GET / HTTP/1.1\" 200 1 \"-\" \"Googlebot/2.1\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("auditroo.toml"),
        format!(
            r#"
            [crawl]
            rate_limit_ms = 0

            [[logs.source]]
            path = "access.log"
            format = "combined"
            origin = "{origin}"

            [history]
            database = "history.sqlite3"
            artifact_root = "runs"
            baseline_root = "baselines"

            [bundle]
            root = "bundles"

            [[site]]
            id = "client-a"
            start_url = "{start}"
            baseline = {{ mode = "label", label = "last-known-good", bootstrap_path = "baseline.json" }}
            bundle = {{ on = ["gate_failed"], required = false }}

            [[site]]
            id = "broken"
            start_url = "http://127.0.0.1:9/"
            baseline = {{ mode = "label", label = "last-known-good", bootstrap_path = "baseline.json" }}
            "#,
            origin = preview.url().trim_end_matches('/'),
            start = preview.url(),
        ),
    )
    .unwrap();

    auditroo(&dir)
        .args(["monitor", "run", "--site", "client-a"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("FAIL  client-a"))
        .stdout(predicate::str::contains("problems would hurt search"));

    let output = auditroo(&dir)
        .args(["history", "list", "--site", "client-a", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let history: Value = serde_json::from_slice(&output).unwrap();
    let run_id = history["runs"][0]["run_id"].as_str().unwrap().to_owned();
    assert_eq!(history["runs"][0]["audit_result"], "gate_failed");
    let run_dir = dir.path().join("runs").join("client-a").join(&run_id);
    assert!(run_dir.join("report.json").exists());
    assert!(run_dir.join("snapshot.json").exists());
    let bundle = dir.path().join("bundles").join("client-a").join(&run_id);
    for file in [
        "index.html",
        "report.json",
        "findings.csv",
        "manifest.json",
        "log-analysis-report.json",
    ] {
        assert!(bundle.join(file).exists(), "bundle has {file}");
    }

    auditroo(&dir)
        .args(["history", "show", &run_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("failed the gate"))
        .stdout(predicate::str::contains("UTC"));
    auditroo(&dir)
        .args([
            "bundle",
            "latest",
            "--site",
            "client-a",
            "--out",
            "client-a-report",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("index.html"));
    assert!(dir
        .path()
        .join("client-a-report")
        .join("index.html")
        .exists());
    auditroo(&dir)
        .args([
            "bundle",
            "latest",
            "--site",
            "client-a",
            "--out",
            "client-a-report",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists"));

    auditroo(&dir)
        .args([
            "baseline",
            "import",
            "baseline.json",
            "--site",
            "client-a",
            "--label",
            "bad label",
        ])
        .assert()
        .code(2);
    auditroo(&dir)
        .args([
            "baseline",
            "mark",
            "latest",
            "--site",
            "client-a",
            "--label",
            "last-known-good",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("now points at"));

    let output = auditroo(&dir)
        .args(["monitor", "run", "--site", "all", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    let sites = result["site_results"].as_array().unwrap();
    let broken = sites
        .iter()
        .find(|site| site["site_id"] == "broken")
        .unwrap();
    assert_eq!(broken["audit_result"], "operational_error");
    let good = sites
        .iter()
        .find(|site| site["site_id"] == "client-a")
        .unwrap();
    assert_eq!(
        good["audit_result"], "passed",
        "the marked baseline is the preview now"
    );
}
