//! `check`: problems one crawl shows without a baseline.

mod support;

use auditroo::audit::{crawl, evaluate, CrawlExtras, Evaluation};
use auditroo::config::{default_settings, ResolvedSettings};
use auditroo::findings::{Finding, Rule, Severity};
use auditroo::progress::Progress;
use auditroo::report::{Report, ReportCommand};
use support::site::{DocsSite, Route};

fn settings() -> ResolvedSettings {
    let mut settings = default_settings();
    settings.crawl.rate_limit_ms = 0;
    settings
}

async fn check(site: &DocsSite) -> Report {
    check_with(site, settings()).await
}

async fn check_with(site: &DocsSite, settings: ResolvedSettings) -> Report {
    let server = site.serve(None).await;
    let snapshot = crawl(
        server.url("/"),
        &settings,
        CrawlExtras::default(),
        &mut Progress::quiet(),
        "test",
    )
    .await
    .expect("crawl works");
    evaluate(Evaluation {
        command: ReportCommand::Check,
        baseline: None,
        current: &snapshot,
        settings: &settings,
        notes: Vec::new(),
    })
    .await
    .expect("report evaluates")
}

fn render(report: &Report) -> String {
    auditroo::render::human::render(report, Default::default())
}

#[track_caller]
fn expect(report: &Report, rule: Rule, severity: Severity, path: &str) -> Finding {
    report
        .findings
        .iter()
        .find(|finding| {
            finding.rule == rule
                && finding.severity == severity
                && finding
                    .url
                    .as_deref()
                    .is_some_and(|url| url.ends_with(path))
        })
        .cloned()
        .unwrap_or_else(|| panic!("expected {rule} ({severity}) on {path}\n{}", render(report)))
}

#[tokio::test]
async fn a_healthy_site_has_no_findings() {
    let report = check(&DocsSite::new()).await;
    assert!(report.findings.is_empty(), "{}", render(&report));
    assert!(report.passed);
    let human = render(&report);
    assert!(human.starts_with("PASS"), "{human}");
}

#[tokio::test]
async fn finds_the_common_problems_of_one_crawl() {
    let mut site = DocsSite::new();
    // A link to a page that does not exist.
    site.edit("/", |html| {
        html.replace("</main>", "<a href=\"/docs/does-not-exist/\">x</a></main>")
    });
    // A noindex page that the sitemap still lists.
    site.edit("/docs/faq/", |html| {
        html.replace(
            "</head>",
            "<meta name=\"robots\" content=\"noindex\"></head>",
        )
    });
    // A canonical tag on another host.
    site.edit("/docs/commands/", |html| {
        html.replace(
            "href=\"{{ORIGIN}}/docs/commands/\"",
            "href=\"https://other.example/docs/commands/\"",
        )
    });
    // A canonical tag that names a URL which redirects.
    site.set("/docs/faq", Route::redirect(301, "/docs/faq/"));
    site.edit("/docs/verify/", |html| {
        html.replace(
            "href=\"{{ORIGIN}}/docs/verify/\"",
            "href=\"{{ORIGIN}}/docs/faq\"",
        )
    });
    // No title, the same title twice, no H1 and broken JSON-LD.
    site.edit("/privacy/", |html| {
        html.replace("<title>Privacy | Lantern</title>", "")
    });
    site.edit("/docs/quickstart/", |html| {
        html.replace(
            "<title>Quickstart | Lantern</title>",
            "<title>Capture | Lantern</title>",
        )
    });
    site.edit("/docs/reference/", |html| {
        html.replace("<h1>Reference</h1>", "")
    });
    site.edit("/docs/capture/", |html| {
        html.replacen(
            "<script type=\"application/ld+json\">{",
            "<script type=\"application/ld+json\">{,",
            1,
        )
    });
    // A linked page that redirects twice, and a redirect loop.
    let timestamps = site.routes["/docs/timestamps/"].clone();
    site.set("/docs/timestamps-new/", timestamps);
    site.set("/docs/timestamps/", Route::redirect(301, "/docs/ts-v2/"));
    site.set(
        "/docs/ts-v2/",
        Route::redirect(301, "/docs/timestamps-new/"),
    );
    site.edit("/docs/", |html| {
        html.replace("</main>", "<a href=\"/loop-a/\">loop</a></main>")
    });
    site.set("/loop-a/", Route::redirect(302, "/loop-b/"));
    site.set("/loop-b/", Route::redirect(302, "/loop-a/"));
    // robots.txt blocks a section that the sitemap lists.
    site.edit("/robots.txt", |robots| {
        robots.replace("Allow: /", "Allow: /\nDisallow: /docs/de/")
    });

    let report = check(&site).await;

    let broken = expect(
        &report,
        Rule::BrokenLink,
        Severity::High,
        "/docs/does-not-exist/",
    );
    assert!(broken.message.contains("returns 404"));
    assert_eq!(broken.related.len(), 1);
    expect(
        &report,
        Rule::SitemapUrlNoindex,
        Severity::Medium,
        "/docs/faq/",
    );
    let external = expect(
        &report,
        Rule::CanonicalExternal,
        Severity::High,
        "/docs/commands/",
    );
    assert_eq!(external.group.as_deref(), Some("other.example"));
    let to_redirect = expect(
        &report,
        Rule::CanonicalToRedirect,
        Severity::Medium,
        "/docs/verify/",
    );
    assert!(to_redirect.message.contains("redirects to /docs/faq/"));
    expect(&report, Rule::TitleMissing, Severity::Medium, "/privacy/");
    expect(
        &report,
        Rule::TitleDuplicate,
        Severity::Low,
        "/docs/quickstart/",
    );
    expect(
        &report,
        Rule::TitleDuplicate,
        Severity::Low,
        "/docs/capture/",
    );
    expect(&report, Rule::H1Missing, Severity::Low, "/docs/reference/");
    let invalid = expect(
        &report,
        Rule::StructuredDataInvalid,
        Severity::High,
        "/docs/capture/",
    );
    assert!(invalid.message.contains("not valid JSON"));
    let chain = expect(
        &report,
        Rule::RedirectChain,
        Severity::Low,
        "/docs/timestamps/",
    );
    assert!(chain.message.contains("2 times"));
    let looped = expect(&report, Rule::RedirectLoop, Severity::High, "/loop-a/");
    assert!(looped.message.contains("redirect loop"));
    expect(
        &report,
        Rule::SitemapUrlBlocked,
        Severity::High,
        "/docs/de/abnahme/",
    );
    expect(
        &report,
        Rule::SitemapUrlBlocked,
        Severity::High,
        "/docs/de/fuer-empfaenger/",
    );
    assert!(!report.passed);

    // --fail-on decides the gate.
    let mut critical_only = settings();
    critical_only.gate.fail_on = Severity::Critical;
    let report = check_with(&site, critical_only).await;
    assert!(report.passed, "{}", render(&report));
}

#[tokio::test]
async fn a_noindex_start_page_is_critical() {
    let mut site = DocsSite::new();
    site.edit("/", |html| {
        html.replace(
            "</head>",
            "<meta name=\"robots\" content=\"noindex\"></head>",
        )
    });
    let report = check(&site).await;
    expect(&report, Rule::HomeNoindex, Severity::Critical, "/");
}

#[tokio::test]
async fn a_failing_start_page_is_critical() {
    let mut site = DocsSite::new();
    site.set("/", Route::status(500));
    let report = check(&site).await;
    let finding = expect(&report, Rule::HomeError, Severity::Critical, "/");
    assert!(finding.message.contains("500"));
}

#[tokio::test]
async fn robots_txt_that_blocks_googlebot_from_the_start_page_is_critical() {
    let mut site = DocsSite::new();
    site.edit("/robots.txt", |robots| {
        format!("User-agent: Googlebot\nDisallow: /\n\n{robots}")
    });
    let report = check(&site).await;
    expect(&report, Rule::RobotsBlocksSite, Severity::Critical, "/");
}

#[tokio::test]
async fn a_failing_robots_txt_is_critical() {
    let mut site = DocsSite::new();
    site.set("/robots.txt", Route::status(500));
    let report = check(&site).await;
    let finding = expect(
        &report,
        Rule::RobotsError,
        Severity::Critical,
        "/robots.txt",
    );
    assert!(finding.message.contains("500"));
    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.rule == Rule::SitemapUrlBlocked),
        "a robots.txt error is one finding, not one per sitemap URL"
    );
}

#[tokio::test]
async fn sitemap_pages_without_links_and_missing_sitemaps() {
    let mut site = DocsSite::new();
    site.edit_pages(|html| {
        html.replace(
            "<li><a href=\"/docs/timestamps/\">/docs/timestamps/</a></li>",
            "",
        )
    });
    let report = check(&site).await;
    expect(
        &report,
        Rule::OrphanPage,
        Severity::Low,
        "/docs/timestamps/",
    );

    let mut no_sitemap = DocsSite::new();
    no_sitemap.remove("/sitemap-index.xml");
    no_sitemap.edit("/robots.txt", |robots| {
        robots
            .lines()
            .filter(|line| !line.starts_with("Sitemap:"))
            .collect::<Vec<_>>()
            .join("\n")
    });
    let report = check(&no_sitemap).await;
    assert!(report
        .findings
        .iter()
        .any(|finding| finding.rule == Rule::SitemapMissing && finding.url.is_none()));
    // check has no "before", so a passing run lists findings, not changes.
    let text = render(&report);
    assert!(report.passed, "{text}");
    assert!(text.contains("\nFindings\n"), "{text}");
    assert!(!text.contains("Changes"), "{text}");
}

#[tokio::test]
async fn a_page_whose_canonical_names_another_page_is_not_indexable() {
    let mut site = DocsSite::new();
    site.edit("/docs/package-format/", |html| {
        html.replace(
            "href=\"{{ORIGIN}}/docs/package-format/\"",
            "href=\"{{ORIGIN}}/docs/commands/\"",
        )
    });
    let report = check(&site).await;
    expect(
        &report,
        Rule::SitemapUrlCanonicalised,
        Severity::Low,
        "/docs/package-format/",
    );
}
