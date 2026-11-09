//! The regression matrix: one test per kind of deploy mistake, and one per
//! normal preview setup that must not fail.
//!
//! Each test serves the docs site from `support::site`, breaks one thing, and
//! compares the crawl with a baseline crawl of the unbroken site, the same
//! way `auditroo diff` does. A few cases run `check` on one crawl instead.
//! The tests check which findings come out, their severity and whether the
//! default gate (fail on high) fails.

mod support;

use std::collections::BTreeSet;

use auditroo::audit::{crawl, ensure_comparable, evaluate, CrawlExtras, Evaluation};
use auditroo::config::{default_settings, ResolvedSettings};
use auditroo::findings::{Finding, Rule, Severity};
use auditroo::progress::Progress;
use auditroo::report::{Report, ReportCommand};
use auditroo::snapshot::Snapshot;
use support::site::{DocsSite, Route};
use support::FixtureSite;
use tokio::sync::OnceCell;

fn settings() -> ResolvedSettings {
    let mut settings = default_settings();
    settings.crawl.rate_limit_ms = 0;
    settings
}

async fn crawl_site(server: &FixtureSite, baseline: Option<&Snapshot>) -> Snapshot {
    let start = server.url("/");
    let extras = baseline
        .map(|baseline| CrawlExtras::compared_with(baseline, &start))
        .unwrap_or_default();
    crawl(start, &settings(), extras, &mut Progress::quiet(), "test")
        .await
        .expect("crawl works")
}

/// Compare a crawl of `current` with `baseline`, like `auditroo compare`.
async fn diff_with(baseline: &Snapshot, current: &FixtureSite) -> Report {
    let current = crawl_site(current, Some(baseline)).await;
    evaluate(Evaluation {
        command: ReportCommand::Compare,
        baseline: Some(baseline),
        current: &current,
        settings: &settings(),
        notes: Vec::new(),
    })
    .await
    .expect("report evaluates")
}

/// `auditroo check` on one site.
async fn check_site(server: &FixtureSite) -> Report {
    let snapshot = crawl_site(server, None).await;
    evaluate(Evaluation {
        command: ReportCommand::Check,
        baseline: None,
        current: &snapshot,
        settings: &settings(),
        notes: Vec::new(),
    })
    .await
    .expect("report evaluates")
}

fn origin_of(server: &FixtureSite) -> String {
    server.url("/").as_str().trim_end_matches('/').to_owned()
}

/// Only info findings, and the gate passes.
#[track_caller]
fn expect_clean(report: &Report) {
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.severity == Severity::Info),
        "{}",
        render(report)
    );
    assert!(report.passed);
}

/// The unbroken site, crawled once for all tests.
async fn baseline() -> &'static Snapshot {
    static BASELINE: OnceCell<Snapshot> = OnceCell::const_new();
    BASELINE
        .get_or_init(|| async {
            let server = DocsSite::new().serve(None).await;
            crawl_site(&server, None).await
        })
        .await
}

/// Serve `site`, crawl it and compare it with the baseline.
async fn compare(site: DocsSite) -> Report {
    compare_with_origin(site, None).await
}

async fn compare_with_origin(site: DocsSite, origin: Option<&str>) -> Report {
    let baseline = baseline().await;
    let server = site.serve(origin).await;
    let current = crawl_site(&server, Some(baseline)).await;
    evaluate(Evaluation {
        command: ReportCommand::Diff,
        baseline: Some(baseline),
        current: &current,
        settings: &settings(),
        notes: Vec::new(),
    })
    .await
    .expect("report evaluates")
}

fn render(report: &Report) -> String {
    auditroo::render::human::render(report, Default::default())
}

fn findings(report: &Report, rule: Rule) -> Vec<&Finding> {
    report
        .findings
        .iter()
        .filter(|finding| finding.rule == rule)
        .collect()
}

#[track_caller]
fn expect(report: &Report, rule: Rule, severity: Severity, path: Option<&str>) -> Finding {
    let found = report.findings.iter().find(|finding| {
        finding.rule == rule
            && finding.severity == severity
            && match path {
                Some(path) => finding
                    .url
                    .as_deref()
                    .is_some_and(|url| url.ends_with(path) && url.split('/').count() > 2),
                None => true,
            }
    });
    match found {
        Some(finding) => finding.clone(),
        None => panic!(
            "expected {rule} ({severity}) on {path:?}\n{}",
            render(report)
        ),
    }
}

/// Rules of findings that fail the gate.
fn failing(report: &Report) -> BTreeSet<Rule> {
    report.failing().map(|finding| finding.rule).collect()
}

#[track_caller]
fn expect_failing(report: &Report, rules: &[Rule]) {
    let expected: BTreeSet<Rule> = rules.iter().copied().collect();
    assert_eq!(
        failing(report),
        expected,
        "unexpected failing findings\n{}",
        render(report)
    );
    assert_eq!(report.passed, rules.is_empty());
}

#[tokio::test]
async fn an_unchanged_site_has_no_findings() {
    let report = compare(DocsSite::new()).await;
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.severity == Severity::Info),
        "{}",
        render(&report)
    );
    assert!(report.passed);
}

#[tokio::test]
async fn meta_noindex_on_one_page() {
    let mut site = DocsSite::new();
    site.edit("/docs/quickstart/", |html| {
        html.replace(
            "</head>",
            "<meta name=\"robots\" content=\"noindex, follow\"></head>",
        )
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::NoindexAdded,
        Severity::Critical,
        Some("/docs/quickstart/"),
    );
    assert!(finding.message.contains("robots meta tag"));
    expect_failing(&report, &[Rule::NoindexAdded]);
}

#[tokio::test]
async fn x_robots_tag_noindex_header() {
    let mut site = DocsSite::new();
    site.header("/docs/faq/", "x-robots-tag", "noindex");
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::NoindexAdded,
        Severity::Critical,
        Some("/docs/faq/"),
    );
    assert!(finding.message.contains("X-Robots-Tag"));
    expect_failing(&report, &[Rule::NoindexAdded]);
}

#[tokio::test]
async fn noindex_on_every_page_is_one_group() {
    let mut site = DocsSite::new();
    site.edit_pages(|html| {
        html.replace(
            "</head>",
            "<meta name=\"robots\" content=\"noindex\"></head>",
        )
    });
    let report = compare(site).await;
    assert!(findings(&report, Rule::NoindexAdded).len() >= 20);
    expect_failing(&report, &[Rule::NoindexAdded]);
    let human = render(&report);
    assert!(human.contains("pages now have noindex"), "{human}");
    assert!(human.contains("and "), "{human}");
}

#[tokio::test]
async fn canonical_to_a_staging_host() {
    let mut site = DocsSite::new();
    site.edit("/docs/for-recipients/", |html| {
        html.replace(
            "href=\"{{ORIGIN}}/docs/for-recipients/\">",
            "href=\"https://staging.lantern.test/docs/for-recipients/\">",
        )
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::CanonicalExternal,
        Severity::Critical,
        Some("/docs/for-recipients/"),
    );
    assert_eq!(finding.group.as_deref(), Some("staging.lantern.test"));
    assert_eq!(
        finding.after.as_deref(),
        Some("https://staging.lantern.test/docs/for-recipients/")
    );
    expect_failing(&report, &[Rule::CanonicalExternal]);
}

#[tokio::test]
async fn every_canonical_to_a_staging_host_is_one_group() {
    let mut site = DocsSite::new();
    site.edit_pages(|html| {
        html.replace(
            "rel=\"canonical\" href=\"{{ORIGIN}}",
            "rel=\"canonical\" href=\"https://staging.lantern.test",
        )
    });
    let report = compare(site).await;
    assert!(findings(&report, Rule::CanonicalExternal).len() >= 20);
    expect_failing(&report, &[Rule::CanonicalExternal]);
    let human = render(&report);
    assert!(
        human.contains("canonical tags point to another host (staging.lantern.test)"),
        "{human}"
    );
}

#[tokio::test]
async fn canonical_removed() {
    let mut site = DocsSite::new();
    site.edit("/docs/faq/", |html| {
        html.replace(
            "<link rel=\"canonical\" href=\"{{ORIGIN}}/docs/faq/\">\n",
            "",
        )
    });
    let report = compare(site).await;
    expect(
        &report,
        Rule::CanonicalRemoved,
        Severity::Medium,
        Some("/docs/faq/"),
    );
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn seven_urls_dropped_from_the_sitemap() {
    let mut site = DocsSite::new();
    site.edit("/sitemap-0.xml", |xml| {
        xml.lines()
            .filter(|line| !line.contains("/docs/reference/lantern_"))
            .collect::<Vec<_>>()
            .join("\n")
    });
    let report = compare(site).await;
    let finding = expect(&report, Rule::SitemapUrlsRemoved, Severity::High, None);
    assert_eq!(finding.related_total, 7);
    expect_failing(&report, &[Rule::SitemapUrlsRemoved]);
}

#[tokio::test]
async fn child_sitemap_returns_404() {
    let mut site = DocsSite::new();
    site.remove("/sitemap-0.xml");
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::SitemapBroken,
        Severity::High,
        Some("/sitemap-0.xml"),
    );
    assert!(finding.message.contains("404"), "{}", finding.message);
    assert!(finding.related_total >= 20, "{}", render(&report));
    assert_eq!(
        findings(&report, Rule::SitemapBroken).len(),
        1,
        "{}",
        render(&report)
    );
    expect_failing(&report, &[Rule::SitemapBroken]);
}

#[tokio::test]
async fn links_renamed_to_a_missing_page() {
    let mut site = DocsSite::new();
    site.edit_pages(|html| html.replace("href=\"/docs/timestamps/", "href=\"/docs/timestamp/"));
    let report = compare(site).await;
    let broken = expect(
        &report,
        Rule::BrokenLink,
        Severity::High,
        Some("/docs/timestamp/"),
    );
    assert!(broken.message.contains("404"));
    assert!(!broken.related.is_empty());
    expect(
        &report,
        Rule::Orphaned,
        Severity::High,
        Some("/docs/timestamps/"),
    );
    expect_failing(&report, &[Rule::BrokenLink, Rule::Orphaned]);
}

#[tokio::test]
async fn page_loses_all_its_links() {
    let mut site = DocsSite::new();
    site.edit_pages(|html| {
        html.replace(
            "<li><a href=\"/docs/timestamps/\">/docs/timestamps/</a></li>",
            "",
        )
    });
    let report = compare(site).await;
    let orphan = expect(
        &report,
        Rule::Orphaned,
        Severity::High,
        Some("/docs/timestamps/"),
    );
    assert!(!orphan.related.is_empty(), "lists where the links were");
    expect_failing(&report, &[Rule::Orphaned]);
}

#[tokio::test]
async fn redirect_chain_on_a_linked_page() {
    let mut site = DocsSite::new();
    let page = site.routes["/docs/verify/"].clone();
    site.set("/docs/verify-new/", page);
    site.set("/docs/verify/", Route::redirect(301, "/docs/verify-v2/"));
    site.set(
        "/docs/verify-v2/",
        Route::redirect(302, "/docs/verify-new/"),
    );
    let report = compare(site).await;
    let moved = expect(
        &report,
        Rule::PageRedirected,
        Severity::Medium,
        Some("/docs/verify/"),
    );
    assert!(moved.message.contains("/docs/verify-new/"));
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn redirect_loop_is_named_a_loop() {
    let mut site = DocsSite::new();
    site.set("/docs/verify/", Route::redirect(301, "/docs/verify-a/"));
    site.set("/docs/verify-a/", Route::redirect(301, "/docs/verify/"));
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::RedirectLoop,
        Severity::Critical,
        Some("/docs/verify/"),
    );
    assert!(finding.message.contains("redirect loop"));
    assert!(finding.message.contains("/docs/verify-a/"));
    // No title, H1 or JSON-LD changes from the loop's empty response.
    assert!(report
        .findings
        .iter()
        .filter(|other| other.url == finding.url)
        .all(|other| other.rule == Rule::RedirectLoop));
    expect_failing(&report, &[Rule::RedirectLoop]);
}

#[tokio::test]
async fn json_ld_removed() {
    let mut site = DocsSite::new();
    site.edit("/docs/capture/", |html| {
        let start = html.find("<script type=\"application/ld+json\">").unwrap();
        let end = html[start..].find("</script>").unwrap() + start + "</script>".len();
        format!("{}{}", &html[..start], &html[end..])
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::StructuredDataRemoved,
        Severity::Medium,
        Some("/docs/capture/"),
    );
    assert!(finding.message.contains("TechArticle"));
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn json_ld_made_invalid_is_called_invalid() {
    let mut site = DocsSite::new();
    site.edit("/docs/capture/", |html| {
        html.replacen(
            "<script type=\"application/ld+json\">{",
            "<script type=\"application/ld+json\">{,",
            1,
        )
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::StructuredDataInvalid,
        Severity::High,
        Some("/docs/capture/"),
    );
    assert!(finding.message.contains("not valid JSON"));
    expect_failing(&report, &[Rule::StructuredDataInvalid]);
}

#[tokio::test]
async fn title_changed() {
    let mut site = DocsSite::new();
    site.edit("/docs/commands/", |html| {
        html.replace(
            "<title>Commands | Lantern</title>",
            "<title>Lantern</title>",
        )
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::TitleChanged,
        Severity::Medium,
        Some("/docs/commands/"),
    );
    assert_eq!(finding.before.as_deref(), Some("\"Commands | Lantern\""));
    assert_eq!(finding.after.as_deref(), Some("\"Lantern\""));
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn the_same_title_on_every_page_is_one_group() {
    let mut site = DocsSite::new();
    site.edit_pages(|html| {
        let start = html.find("<title>").unwrap();
        let end = html.find("</title>").unwrap();
        format!("{}<title>Lantern{}", &html[..start], &html[end..])
    });
    let report = compare(site).await;
    assert!(findings(&report, Rule::TitleChanged).len() >= 20);
    let human = render(&report);
    assert!(human.contains("titles changed (\"Lantern\")"), "{human}");
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn meta_description_removed() {
    let mut site = DocsSite::new();
    site.edit("/privacy/", |html| {
        html.replace(
            "<meta name=\"description\" content=\"What Lantern stores.\">\n",
            "",
        )
    });
    let report = compare(site).await;
    expect(
        &report,
        Rule::DescriptionRemoved,
        Severity::Low,
        Some("/privacy/"),
    );
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn h1_removed() {
    let mut site = DocsSite::new();
    site.edit("/docs/reference/", |html| {
        html.replace("<h1>Reference</h1>", "<div>Reference</div>")
    });
    let report = compare(site).await;
    expect(
        &report,
        Rule::H1Removed,
        Severity::Medium,
        Some("/docs/reference/"),
    );
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn hreflang_return_link_removed() {
    let mut site = DocsSite::new();
    site.edit("/docs/de/fuer-empfaenger/", |html| {
        let start = html.find("<link rel=\"alternate\"").unwrap();
        let end = html[start..].find('\n').unwrap() + start + 1;
        format!("{}{}", &html[..start], &html[end..])
    });
    let report = compare(site).await;
    expect(
        &report,
        Rule::HreflangChanged,
        Severity::Medium,
        Some("/docs/de/fuer-empfaenger/"),
    );
    let no_return = expect(
        &report,
        Rule::HreflangNoReturn,
        Severity::Medium,
        Some("/docs/for-recipients/"),
    );
    assert!(no_return.message.contains("does not link back"));
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn hreflang_points_to_a_missing_page() {
    let mut site = DocsSite::new();
    site.edit("/docs/for-recipients/", |html| {
        html.replace(
            "hreflang=\"de\" href=\"{{ORIGIN}}/docs/de/fuer-empfaenger/\"",
            "hreflang=\"de\" href=\"{{ORIGIN}}/docs/de/missing/\"",
        )
    });
    let report = compare(site).await;
    let broken = expect(
        &report,
        Rule::HreflangBroken,
        Severity::Medium,
        Some("/docs/for-recipients/"),
    );
    assert!(broken.message.contains("404"), "{}", broken.message);
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn robots_txt_blocks_a_section() {
    // The new rule also blocks auditroo, which production's robots.txt let
    // in. The pages behind the rule are still checked, so a problem there
    // shows up without --ignore-robots.
    let mut site = DocsSite::new();
    site.edit("/robots.txt", |robots| {
        robots.replace("Allow: /", "Allow: /\nDisallow: /docs/de/")
    });
    site.edit("/docs/de/abnahme/", |html| {
        html.replace(
            "</head>",
            "<meta name=\"robots\" content=\"noindex\"></head>",
        )
    });
    site.edit("/docs/de/fuer-empfaenger/", |html| {
        html.replace("</main>", "<a href=\"/docs/de/neu/\">Neu</a></main>")
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::RobotsBlocked,
        Severity::Critical,
        Some("/robots.txt"),
    );
    assert!(finding.message.contains("Googlebot"));
    assert_eq!(finding.related_total, 2);
    expect(
        &report,
        Rule::NoindexAdded,
        Severity::Critical,
        Some("/docs/de/abnahme/"),
    );
    expect(
        &report,
        Rule::BrokenLink,
        Severity::High,
        Some("/docs/de/neu/"),
    );
    expect_failing(
        &report,
        &[Rule::RobotsBlocked, Rule::NoindexAdded, Rule::BrokenLink],
    );
    assert!(report.current.robots_denied.is_empty());
    assert!(!report.incomplete(), "{:?}", report.notes);
}

#[tokio::test]
async fn urls_that_robots_txt_still_blocks_are_listed() {
    // Both sides block auditroo from the German docs, so neither crawl
    // checks them. The report says which URLs, before the findings.
    let mut site = DocsSite::new();
    site.edit("/robots.txt", |robots| {
        robots.replace("Allow: /", "Allow: /\nDisallow: /docs/de/")
    });
    let base = site.serve(None).await;
    let baseline = crawl_site(&base, None).await;
    site.edit("/docs/faq/", |html| {
        html.replace(
            "</head>",
            "<meta name=\"robots\" content=\"noindex\"></head>",
        )
    });
    let preview = site.serve(None).await;
    let report = diff_with(&baseline, &preview).await;
    expect_failing(&report, &[Rule::NoindexAdded]);

    let denied: Vec<&str> = report
        .current
        .robots_denied
        .iter()
        .map(|url| url.strip_prefix(&origin_of(&preview)).unwrap())
        .collect();
    assert_eq!(denied, ["/docs/de/abnahme/", "/docs/de/fuer-empfaenger/"]);
    let note = report
        .notes
        .iter()
        .find(|note| note.starts_with("robots.txt blocks auditroo"))
        .expect("a note about the blocked URLs");
    assert!(
        note.contains("not checked: /docs/de/abnahme/, /docs/de/fuer-empfaenger/."),
        "{note}"
    );

    let text = render(&report);
    let warning = text
        .find("This result is incomplete, because robots.txt blocks auditroo from 2 URLs.")
        .expect("the incomplete line");
    let first_finding = text.find("Critical").expect("the findings");
    assert!(warning < first_finding, "{text}");
    let markdown = auditroo::render::markdown::render(&report);
    assert!(
        markdown.contains("> [!WARNING]\n> This result is incomplete"),
        "{markdown}"
    );
    let html = auditroo::render::html::render(&report);
    assert!(html.contains("<p class=\"incomplete\">This result is incomplete"));
    let json: serde_json::Value =
        serde_json::from_str(&auditroo::report::render_json(&report)).unwrap();
    assert_eq!(
        json["current"]["robots_denied"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test]
async fn robots_txt_disallows_everything() {
    let mut site = DocsSite::new();
    site.edit("/robots.txt", |robots| {
        robots.replace("Allow: /", "Disallow: /")
    });
    let report = compare(site).await;
    let finding = expect(&report, Rule::RobotsBlocked, Severity::Critical, None);
    assert!(finding.related_total >= 20);
    expect_failing(&report, &[Rule::RobotsBlocked]);
}

#[tokio::test]
async fn robots_txt_blocks_only_googlebot() {
    let mut site = DocsSite::new();
    site.edit("/robots.txt", |robots| {
        format!("User-agent: Googlebot\nDisallow: /\n\n{robots}")
    });
    let report = compare(site).await;
    let finding = expect(&report, Rule::RobotsBlocked, Severity::Critical, None);
    assert_eq!(finding.group.as_deref(), Some("Googlebot"));
    assert!(finding.related_total >= 20);
    expect_failing(&report, &[Rule::RobotsBlocked]);
}

#[tokio::test]
async fn robots_txt_starts_failing() {
    let mut site = DocsSite::new();
    site.set("/robots.txt", Route::status(503));
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::RobotsError,
        Severity::Critical,
        Some("/robots.txt"),
    );
    assert!(finding.message.contains("503"));
    expect_failing(&report, &[Rule::RobotsError]);
    assert!(findings(&report, Rule::SitemapMissing).is_empty());
}

#[tokio::test]
async fn page_returns_500_is_one_finding() {
    let mut site = DocsSite::new();
    site.set("/docs/reference/lantern_demo/", Route::status(500));
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::PageBroken,
        Severity::Critical,
        Some("/docs/reference/lantern_demo/"),
    );
    assert!(finding.message.contains("500"));
    assert!(
        !finding.related.is_empty(),
        "lists the pages that link to it"
    );
    let about_page: Vec<_> = report
        .findings
        .iter()
        .filter(|other| other.url == finding.url)
        .collect();
    assert_eq!(about_page.len(), 1, "{}", render(&report));
    expect_failing(&report, &[Rule::PageBroken]);
}

#[tokio::test]
async fn deleted_page_is_one_finding() {
    let mut site = DocsSite::new();
    site.remove("/docs/faq/");
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::PageBroken,
        Severity::Critical,
        Some("/docs/faq/"),
    );
    assert!(finding.message.contains("404"));
    expect_failing(&report, &[Rule::PageBroken]);
}

#[tokio::test]
async fn one_new_broken_internal_link() {
    let mut site = DocsSite::new();
    site.edit("/", |html| {
        html.replace(
            "</main>",
            "<a href=\"/docs/does-not-exist/\">New link</a></main>",
        )
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::BrokenLink,
        Severity::High,
        Some("/docs/does-not-exist/"),
    );
    assert_eq!(finding.related.len(), 1);
    expect_failing(&report, &[Rule::BrokenLink]);
}

#[tokio::test]
async fn the_404_page_is_not_a_new_page() {
    // Static site builders often give the 404 page a canonical tag that
    // names /404.html, and the host serves that file with 200. A new broken
    // link leads the crawl to the 404 page and from there to /404.html.
    let mut site = DocsSite::new();
    site.not_found = support::site::not_found_page().replace(
        "</title>",
        "</title><link rel=\"canonical\" href=\"{{ORIGIN}}/404.html\">",
    );
    site.set("/404.html", Route::html(site.not_found.clone()));
    site.edit("/", |html| {
        html.replace(
            "</main>",
            "<a href=\"/docs/does-not-exist/\">New link</a></main>",
        )
    });
    let report = compare(site).await;
    expect(
        &report,
        Rule::BrokenLink,
        Severity::High,
        Some("/docs/does-not-exist/"),
    );
    assert!(
        findings(&report, Rule::PageAdded).is_empty(),
        "{}",
        render(&report)
    );
}

#[tokio::test]
async fn canonical_points_to_a_redirect() {
    let mut site = DocsSite::new();
    site.set("/docs/faq", Route::redirect(301, "/docs/faq/"));
    site.edit("/docs/faq/", |html| {
        html.replace(
            "<link rel=\"canonical\" href=\"{{ORIGIN}}/docs/faq/\">",
            "<link rel=\"canonical\" href=\"{{ORIGIN}}/docs/faq\">",
        )
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::CanonicalChanged,
        Severity::High,
        Some("/docs/faq/"),
    );
    assert_eq!(finding.after.as_deref(), Some("/docs/faq"));
    assert!(
        finding.message.contains("which redirects to /docs/faq/"),
        "{}",
        finding.message
    );
    assert!(findings(&report, Rule::CanonicalToRedirect).is_empty());
    expect_failing(&report, &[Rule::CanonicalChanged]);
}

#[tokio::test]
async fn links_through_a_redirect_do_not_orphan_the_page() {
    let mut site = DocsSite::new();
    site.set("/docs/faq", Route::redirect(301, "/docs/faq/"));
    site.edit_pages(|html| html.replace("href=\"/docs/faq/\"", "href=\"/docs/faq\""));
    let report = compare(site).await;
    assert!(
        findings(&report, Rule::Orphaned).is_empty(),
        "{}",
        render(&report)
    );
    expect_failing(&report, &[]);
}

#[tokio::test]
async fn preview_build_with_production_urls_has_no_findings() {
    let baseline = baseline().await;
    let production_origin = baseline.start_url.trim_end_matches('/').to_owned();
    // The same build on another host, still carrying the production URLs in
    // canonicals, hreflang, robots.txt and the sitemap.
    let report = compare_with_origin(DocsSite::new(), Some(&production_origin)).await;
    assert!(
        report
            .findings
            .iter()
            .all(|finding| finding.severity == Severity::Info),
        "{}",
        render(&report)
    );
    assert!(report.passed);
}

#[tokio::test]
async fn images_are_not_counted_as_pages() {
    let baseline = baseline().await;
    let image = baseline
        .records
        .values()
        .find(|record| record.url.ends_with("/images/diagram.png"))
        .expect("the linked image was fetched");
    assert!(!image.is_page());
    assert!(!baseline.site().is_indexable(image));
    assert!(!baseline
        .sitemap
        .indexable_not_declared
        .iter()
        .any(|url| url.ends_with(".png")));
}

// Origins: only production and the crawled host count as the site.

/// Production served with its own URLs, and a preview that carries them.
async fn production_and_preview(production: DocsSite, preview: DocsSite) -> Report {
    let production = production.serve(None).await;
    let baseline = crawl_site(&production, None).await;
    let preview = preview.serve(Some(&origin_of(&production))).await;
    diff_with(&baseline, &preview).await
}

/// The docs site without any sitemap.
fn without_sitemap() -> DocsSite {
    let mut site = DocsSite::new();
    site.remove("/sitemap-index.xml");
    site.remove("/sitemap-0.xml");
    site.edit("/robots.txt", |robots| {
        robots.replace("\nSitemap: {{ORIGIN}}/sitemap-index.xml\n", "")
    });
    site
}

#[tokio::test]
async fn a_build_with_a_staging_site_url_everywhere_is_critical() {
    // The classic wrong base URL: canonicals, hreflang, robots.txt and the
    // sitemap all name the staging host, and that host serves a sitemap.
    let staging = DocsSite::new().serve(None).await;
    let preview = DocsSite::new().serve(Some(&origin_of(&staging))).await;
    let report = diff_with(baseline().await, &preview).await;
    let canonical = expect(
        &report,
        Rule::CanonicalExternal,
        Severity::Critical,
        Some("/docs/faq/"),
    );
    let staging_host = format!("127.0.0.1:{}", staging.url("/").port().unwrap());
    assert_eq!(canonical.group.as_deref(), Some(staging_host.as_str()));
    expect(&report, Rule::SitemapUrlExternal, Severity::Critical, None);
    assert!(!report.passed);
    assert!(
        staging
            .requests()
            .await
            .iter()
            .any(|request| request.path == "/sitemap-index.xml"),
        "the sitemap that robots.txt names on another host was read there"
    );
}

#[tokio::test]
async fn a_sitemap_on_a_staging_host_is_critical_on_its_own() {
    let staging = DocsSite::new().serve(None).await;
    let staging_origin = origin_of(&staging);
    let mut site = DocsSite::new();
    site.edit("/robots.txt", |robots| {
        robots.replace("{{ORIGIN}}", &staging_origin)
    });
    let report = compare(site).await;
    let finding = expect(&report, Rule::SitemapUrlExternal, Severity::Critical, None);
    assert!(
        finding.message.contains("another host"),
        "{}",
        finding.message
    );
    assert!(findings(&report, Rule::CanonicalExternal).is_empty());
    // The pages are reported once, as listed on another host, not again as
    // removed from the sitemap.
    assert!(
        findings(&report, Rule::SitemapUrlsRemoved).is_empty(),
        "{}",
        render(&report)
    );
    assert!(!report.passed);
}

#[tokio::test]
async fn a_sitemap_moved_to_an_unreachable_host_is_critical() {
    // The staging host of a wrong site URL is often not reachable from CI.
    let mut site = DocsSite::new();
    site.edit("/robots.txt", |robots| {
        robots.replace("{{ORIGIN}}", "http://127.0.0.1:9")
    });
    let report = compare(site).await;
    let finding = expect(&report, Rule::SitemapBroken, Severity::Critical, None);
    assert!(
        finding.message.contains("another host"),
        "{}",
        finding.message
    );
    assert!(!report.passed);
}

#[tokio::test]
async fn an_identical_preview_without_a_sitemap_passes() {
    let report = production_and_preview(without_sitemap(), without_sitemap()).await;
    expect_clean(&report);
}

#[tokio::test]
async fn a_preview_with_production_urls_and_a_failing_robots_txt_reports_only_robots() {
    let mut preview = DocsSite::new();
    preview.set("/robots.txt", Route::status(503));
    let report = production_and_preview(DocsSite::new(), preview).await;
    expect_failing(&report, &[Rule::RobotsError]);
    assert!(findings(&report, Rule::CanonicalExternal).is_empty());
}

#[tokio::test]
async fn production_links_in_a_preview_are_followed_on_the_preview() {
    // The navigation links to the page with an absolute production URL.
    let mut preview = DocsSite::new();
    preview.edit_pages(|html| {
        html.replace(
            "href=\"/docs/timestamps/\"",
            "href=\"{{ORIGIN}}/docs/timestamps/\"",
        )
    });
    let report = production_and_preview(DocsSite::new(), preview).await;
    expect_clean(&report);
}

#[tokio::test]
async fn hreflang_to_a_missing_page_on_a_preview_with_production_urls() {
    let mut preview = DocsSite::new();
    preview.edit("/docs/for-recipients/", |html| {
        html.replace(
            "hreflang=\"de\" href=\"{{ORIGIN}}/docs/de/fuer-empfaenger/\"",
            "hreflang=\"de\" href=\"{{ORIGIN}}/docs/de/missing/\"",
        )
    });
    let report = production_and_preview(DocsSite::new(), preview).await;
    let broken = expect(
        &report,
        Rule::HreflangBroken,
        Severity::Medium,
        Some("/docs/for-recipients/"),
    );
    assert!(broken.message.contains("404"), "{}", broken.message);
}

#[tokio::test]
async fn canonical_to_a_redirect_on_a_preview_with_production_urls() {
    let mut preview = DocsSite::new();
    preview.set("/docs/faq", Route::redirect(301, "/docs/faq/"));
    preview.edit("/docs/faq/", |html| {
        html.replace(
            "<link rel=\"canonical\" href=\"{{ORIGIN}}/docs/faq/\">",
            "<link rel=\"canonical\" href=\"{{ORIGIN}}/docs/faq\">",
        )
    });
    let report = production_and_preview(DocsSite::new(), preview).await;
    let finding = expect(
        &report,
        Rule::CanonicalChanged,
        Severity::High,
        Some("/docs/faq/"),
    );
    assert!(
        finding.message.contains("which redirects to /docs/faq/"),
        "{}",
        finding.message
    );
}

#[tokio::test]
async fn a_sitemap_on_another_host_is_read_where_it_lives() {
    // robots.txt names a sitemap on a second server, which lists the
    // site's own URLs. Google accepts that, so auditroo reads it there.
    let site = DocsSite::new().serve(None).await;
    let sitemaps = FixtureSite::spawn().await;
    let body = DocsSite::new().routes["/sitemap-0.xml"]
        .body
        .replace("{{ORIGIN}}", &origin_of(&site));
    sitemaps
        .set("/lantern-sitemap.xml", 200, "application/xml", &body)
        .await;
    let robots = format!(
        "User-agent: *\nAllow: /\n\nSitemap: {}/lantern-sitemap.xml\n",
        origin_of(&sitemaps)
    );
    site.set("/robots.txt", 200, "text/plain", &robots).await;
    let report = check_site(&site).await;
    expect_clean(&report);
    assert!(sitemaps
        .requests()
        .await
        .iter()
        .any(|request| request.path == "/lantern-sitemap.xml"));
    assert!(report
        .notes
        .iter()
        .any(|note| note.contains("auditroo fetched it there")));
}

#[tokio::test]
async fn a_sitemap_on_another_host_that_fails_is_not_called_fetched() {
    // Two local builds of a site often name the live sitemap in robots.txt.
    // When auditroo can't load it, the note must say so.
    let site = DocsSite::new().serve(None).await;
    let sitemaps = FixtureSite::spawn().await;
    let robots = format!(
        "User-agent: *\nAllow: /\n\nSitemap: {}/lantern-sitemap.xml\n",
        origin_of(&sitemaps)
    );
    site.set("/robots.txt", 200, "text/plain", &robots).await;
    let report = check_site(&site).await;
    let note = report
        .notes
        .iter()
        .find(|note| note.starts_with("A sitemap is on"))
        .unwrap_or_else(|| panic!("no sitemap note: {:?}", report.notes));
    assert_eq!(
        note,
        &format!(
            "A sitemap is on {}, but auditroo could not read it there: it returns 404.",
            origin_of(&sitemaps)
        )
    );
}

// Crawls that found nothing useful never pass.

fn redirect_everything(site: &mut DocsSite, prefix: &str, target: &str) {
    let paths: Vec<String> = site
        .routes
        .iter()
        .filter(|(path, route)| {
            path.starts_with(prefix)
                && route
                    .headers
                    .iter()
                    .any(|(_, value)| value.starts_with("text/html"))
        })
        .map(|(path, _)| path.clone())
        .collect();
    // Login walls usually add the page they came from, so every page
    // redirects to a slightly different URL of the same login page.
    for path in paths {
        site.set(
            &path,
            Route::redirect(302, &format!("{target}?next={path}")),
        );
    }
}

fn login_page() -> Route {
    Route::html(
        "<!doctype html><html><head><title>Sign in</title></head><body><h1>Sign in</h1><form></form></body></html>".to_owned(),
    )
}

#[tokio::test]
async fn every_page_redirecting_to_a_login_page_is_critical() {
    let mut site = DocsSite::new();
    redirect_everything(&mut site, "/", "/login/");
    site.set("/login/", login_page());
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::PageRedirected,
        Severity::Critical,
        Some("/docs/"),
    );
    assert_eq!(finding.group.as_deref(), Some("/login/"));
    assert!(!report.passed);
    let human = render(&report);
    assert!(human.contains("pages now redirect to /login/"), "{human}");
}

#[tokio::test]
async fn one_section_redirecting_to_a_login_page_fails() {
    let mut site = DocsSite::new();
    redirect_everything(&mut site, "/docs/reference/", "/login/");
    site.set("/login/", login_page());
    let report = compare(site).await;
    expect(
        &report,
        Rule::PageRedirected,
        Severity::High,
        Some("/docs/reference/lantern_demo/"),
    );
    assert!(!report.passed);
}

#[tokio::test]
async fn production_behind_a_firewall_cannot_be_compared_with() {
    let mut site = DocsSite::new();
    let paths: Vec<String> = site.routes.keys().cloned().collect();
    for path in paths {
        site.set(&path, Route::status(403));
    }
    let server = site.serve(None).await;
    let production = crawl_site(&server, None).await;
    let error = ensure_comparable(&production, "production").unwrap_err();
    assert_eq!(error.exit_code(), 3);
    assert!(error.to_string().contains("returns 403"), "{error}");
}

#[tokio::test]
async fn a_preview_behind_basic_auth_is_critical() {
    let mut site = DocsSite::new();
    let paths: Vec<String> = site.routes.keys().cloned().collect();
    for path in paths {
        site.set(&path, Route::status(401));
    }
    let report = compare(site).await;
    expect(
        &report,
        Rule::PageBroken,
        Severity::Critical,
        Some("/docs/"),
    );
    assert!(!report.passed);
    // The image is a file, and the summary says so.
    expect(
        &report,
        Rule::FileBroken,
        Severity::High,
        Some("/images/diagram.png"),
    );
    let human = render(&report);
    assert!(human.contains("1 file now returns an error"), "{human}");
}

#[tokio::test]
async fn a_start_page_redirect_loop_fails_check() {
    let mut site = without_sitemap();
    site.set("/", Route::redirect(302, "/loop/"));
    site.set("/loop/", Route::redirect(302, "/"));
    let server = site.serve(None).await;
    let report = check_site(&server).await;
    let finding = expect(&report, Rule::HomeError, Severity::Critical, Some("/"));
    assert!(
        finding.message.contains("redirect loop"),
        "{}",
        finding.message
    );
    assert!(!report.passed);
}

#[tokio::test]
async fn a_start_page_that_redirects_to_another_site_is_critical() {
    let elsewhere = FixtureSite::spawn().await;
    elsewhere
        .set("/", 200, "text/html", "<!doctype html><title>Login</title>")
        .await;
    let mut site = DocsSite::new();
    site.set("/", Route::redirect(302, elsewhere.url("/").as_str()));
    let report = compare(site).await;
    let on_start_page = report.findings.iter().find(|finding| {
        finding.severity == Severity::Critical
            && finding
                .url
                .as_deref()
                .and_then(|url| url::Url::parse(url).ok())
                .is_some_and(|url| url.path() == "/")
    });
    assert!(on_start_page.is_some(), "{}", render(&report));
    assert!(!report.passed);
}

// Google's indexing rules.

#[tokio::test]
async fn googlebot_meta_noindex() {
    let mut site = DocsSite::new();
    site.edit("/docs/quickstart/", |html| {
        html.replace(
            "</head>",
            "<meta name=\"googlebot\" content=\"noindex\"></head>",
        )
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::NoindexAdded,
        Severity::Critical,
        Some("/docs/quickstart/"),
    );
    assert!(
        finding.message.contains("googlebot meta tag"),
        "{}",
        finding.message
    );
    expect_failing(&report, &[Rule::NoindexAdded]);
}

#[tokio::test]
async fn x_robots_tag_for_googlebot() {
    let mut site = DocsSite::new();
    site.header("/docs/faq/", "x-robots-tag", "googlebot: noindex");
    let report = compare(site).await;
    expect(
        &report,
        Rule::NoindexAdded,
        Severity::Critical,
        Some("/docs/faq/"),
    );
    expect_failing(&report, &[Rule::NoindexAdded]);
}

#[tokio::test]
async fn x_robots_tag_for_another_crawler_is_not_a_noindex() {
    let mut site = DocsSite::new();
    site.header("/docs/faq/", "x-robots-tag", "bingbot: noindex");
    let report = compare(site).await;
    expect_clean(&report);
}

#[tokio::test]
async fn robots_txt_googlebot_groups_with_a_version_or_a_wildcard() {
    for agent in ["Googlebot/2.1", "Googlebot*"] {
        let mut site = DocsSite::new();
        site.edit("/robots.txt", |robots| {
            format!("User-agent: {agent}\nDisallow: /docs/\n\n{robots}")
        });
        let report = compare(site).await;
        let finding = expect(&report, Rule::RobotsBlocked, Severity::Critical, None);
        assert_eq!(finding.group.as_deref(), Some("Googlebot"), "{agent}");
    }
}

#[tokio::test]
async fn robots_txt_429_is_not_an_allow_all() {
    let mut site = DocsSite::new();
    site.set("/robots.txt", Route::status(429));
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::RobotsError,
        Severity::Critical,
        Some("/robots.txt"),
    );
    assert!(finding.message.contains("429"));
    expect_failing(&report, &[Rule::RobotsError]);
}

// Other deploy mistakes.

#[tokio::test]
async fn an_indexable_page_that_turns_into_json() {
    let mut site = DocsSite::new();
    site.set(
        "/docs/faq/",
        Route {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: "{}".into(),
        },
    );
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::PageNotHtml,
        Severity::Critical,
        Some("/docs/faq/"),
    );
    assert!(finding.message.contains("application/json"));
    assert!(!report.passed);
}

#[tokio::test]
async fn links_on_a_redirected_start_page_are_checked() {
    let mut site = DocsSite::new();
    let home = site.routes["/"]
        .body
        .replace("</main>", "<a href=\"/docs/missing/\">New</a></main>");
    site.set("/en/", Route::html(home));
    site.set("/", Route::redirect(301, "/en/"));
    let server = site.serve(None).await;
    let report = check_site(&server).await;
    let finding = expect(
        &report,
        Rule::BrokenLink,
        Severity::High,
        Some("/docs/missing/"),
    );
    assert!(
        finding.related.iter().any(|url| url.ends_with("/en/")),
        "{:?}",
        finding.related
    );
}

#[tokio::test]
async fn sitemap_entries_without_a_full_url() {
    let mut site = DocsSite::new();
    site.edit("/sitemap-0.xml", |xml| {
        xml.replace("<loc>{{ORIGIN}}", "<loc>")
    });
    let server = site.serve(None).await;
    let report = check_site(&server).await;
    let finding = expect(
        &report,
        Rule::SitemapBroken,
        Severity::High,
        Some("/sitemap-0.xml"),
    );
    assert!(
        finding.message.contains("relative path"),
        "{}",
        finding.message
    );
}

#[tokio::test]
async fn a_json_ld_type_removed_from_a_graph() {
    let graph = r#"{"@context":"https://schema.org","@graph":[{"@type":"WebPage"},{"@type":"Product","name":"Lantern"}]}"#;
    let mut production = DocsSite::new();
    production.edit("/docs/faq/", |html| {
        html.replace(
            "</head>",
            &format!("<script type=\"application/ld+json\">{graph}</script></head>"),
        )
    });
    let mut preview = production.clone();
    preview.edit("/docs/faq/", |html| {
        html.replace(r#",{"@type":"Product","name":"Lantern"}"#, "")
    });
    let report = production_and_preview(production, preview).await;
    let finding = expect(
        &report,
        Rule::StructuredDataChanged,
        Severity::Medium,
        Some("/docs/faq/"),
    );
    assert!(finding.message.contains("Product"), "{}", finding.message);
}

#[tokio::test]
async fn pages_cut_off_as_a_group_are_orphaned() {
    // The reference pages still link to each other, but nothing that the
    // start page reaches links to them any more.
    let mut site = DocsSite::new();
    site.edit("/docs/", |html| {
        html.replace(
            "<li><a href=\"/docs/reference/\">/docs/reference/</a></li>",
            "",
        )
    });
    let report = compare(site).await;
    let finding = expect(
        &report,
        Rule::Orphaned,
        Severity::High,
        Some("/docs/reference/"),
    );
    assert!(finding.message.contains("reached"), "{}", finding.message);
    expect(
        &report,
        Rule::Orphaned,
        Severity::High,
        Some("/docs/reference/lantern_demo/"),
    );
}

#[test]
fn each_page_is_checked_against_its_own_hosts_robots_txt() {
    // The bare domain and the www host have separate robots.txt files, like
    // Google reads them. Only the www file starts to block Googlebot.
    use auditroo::crawl::RobotsFile;
    let file = |origin: &str, body: &str| RobotsFile {
        url: format!("{origin}/robots.txt"),
        status: Some(200),
        body: Some(body.to_owned()),
        error: None,
    };
    let mut baseline = support::snapshot_of(
        "https://example.com/",
        vec![
            support::page_record("https://example.com/"),
            support::page_record("https://www.example.com/blog/"),
        ],
    );
    for origin in ["https://example.com", "https://www.example.com"] {
        baseline
            .robots
            .insert(origin.to_owned(), file(origin, "User-agent: *\nAllow: /\n"));
    }
    let mut current = baseline.clone();
    current.robots.insert(
        "https://www.example.com".to_owned(),
        file(
            "https://www.example.com",
            "User-agent: Googlebot\nDisallow: /\n",
        ),
    );
    let outcome = auditroo::rules::diff::diff(&baseline, &current);
    let blocked: Vec<&Finding> = outcome
        .findings
        .iter()
        .filter(|finding| finding.rule == Rule::RobotsBlocked)
        .collect();
    assert_eq!(blocked.len(), 1, "{:#?}", outcome.findings);
    assert_eq!(blocked[0].severity, Severity::Critical);
    assert_eq!(blocked[0].related, vec!["https://www.example.com/blog/"]);
}

// The same host before and after, as `auditroo diff` against a snapshot of
// the same site sees it.

async fn same_host(site: DocsSite) -> Report {
    let server = DocsSite::new().serve(None).await;
    let baseline = crawl_site(&server, None).await;
    site.replace_on(&server, None).await;
    diff_with(&baseline, &server).await
}

#[tokio::test]
async fn an_unchanged_site_on_the_same_host_has_no_findings() {
    expect_clean(&same_host(DocsSite::new()).await);
}

#[tokio::test]
async fn robots_txt_changes_on_the_same_host() {
    let mut site = DocsSite::new();
    site.edit("/robots.txt", |robots| {
        format!("User-agent: Googlebot/2.1\nDisallow: /docs/\n\n{robots}")
    });
    let report = same_host(site).await;
    let finding = expect(&report, Rule::RobotsBlocked, Severity::Critical, None);
    assert_eq!(finding.group.as_deref(), Some("Googlebot"));
    expect_failing(&report, &[Rule::RobotsBlocked]);
}

#[tokio::test]
async fn noindex_and_broken_pages_on_the_same_host() {
    let mut site = DocsSite::new();
    site.header("/docs/faq/", "x-robots-tag", "googlebot: noindex");
    site.set("/docs/verify/", Route::status(500));
    let report = same_host(site).await;
    expect(
        &report,
        Rule::NoindexAdded,
        Severity::Critical,
        Some("/docs/faq/"),
    );
    expect(
        &report,
        Rule::PageBroken,
        Severity::Critical,
        Some("/docs/verify/"),
    );
}

#[tokio::test]
async fn a_staging_canonical_on_the_same_host() {
    let mut site = DocsSite::new();
    site.edit_pages(|html| {
        html.replace(
            "rel=\"canonical\" href=\"{{ORIGIN}}",
            "rel=\"canonical\" href=\"https://staging.lantern.test",
        )
    });
    let report = same_host(site).await;
    expect(
        &report,
        Rule::CanonicalExternal,
        Severity::Critical,
        Some("/docs/faq/"),
    );
    expect_failing(&report, &[Rule::CanonicalExternal]);
}

#[tokio::test]
async fn check_of_a_preview_trusts_production_only_when_told() {
    let production = DocsSite::new().serve(None).await;
    let preview = DocsSite::new().serve(Some(&origin_of(&production))).await;

    let told = crawl(
        preview.url("/"),
        &settings(),
        CrawlExtras {
            production: vec![production.url("/")],
            ..CrawlExtras::default()
        },
        &mut Progress::quiet(),
        "test",
    )
    .await
    .unwrap();
    let report = evaluate(Evaluation {
        command: ReportCommand::Check,
        baseline: None,
        current: &told,
        settings: &settings(),
        notes: Vec::new(),
    })
    .await
    .unwrap();
    assert!(report.findings.is_empty(), "{}", render(&report));

    // Without --production-url the preview is checked as a live site of its
    // own, and production is one more host. In check that is only high,
    // because some sites point canonical tags elsewhere on purpose.
    let report = check_site(&preview).await;
    let finding = expect(
        &report,
        Rule::CanonicalExternal,
        Severity::High,
        Some("/docs/faq/"),
    );
    assert!(
        finding
            .message
            .contains("{ rule = \"canonical_external\", url = \"/docs/faq/\" }"),
        "{}",
        finding.message
    );
    assert!(!report.passed);
}

#[tokio::test]
async fn check_of_a_site_behind_a_login_fails() {
    let mut site = DocsSite::new();
    redirect_everything(&mut site, "/", "/login/");
    site.set("/login/", login_page());
    let server = site.serve(None).await;
    let report = check_site(&server).await;
    let finding = expect(
        &report,
        Rule::SitemapUrlRedirect,
        Severity::Critical,
        Some("/docs/"),
    );
    assert_eq!(finding.group.as_deref(), Some("/login/"));
    assert!(!report.passed);

    let mut site = DocsSite::new();
    redirect_everything(&mut site, "/docs/reference/", "/login/");
    site.set("/login/", login_page());
    let server = site.serve(None).await;
    let report = check_site(&server).await;
    expect(
        &report,
        Rule::SitemapUrlRedirect,
        Severity::High,
        Some("/docs/reference/"),
    );
    assert!(!report.passed);
}

#[tokio::test]
async fn two_local_builds_of_one_site() {
    // Both builds carry the URLs of a live site that is not crawled, like a
    // build of main compared with a build of the pull request.
    let live = DocsSite::new().serve(None).await;
    let live_origin = origin_of(&live);
    let base = DocsSite::new().serve(Some(&live_origin)).await;
    let mut changed = DocsSite::new();
    changed.edit("/robots.txt", |robots| {
        robots.replace("Allow: /", "Allow: /\nDisallow: /docs/de/")
    });
    changed.edit("/docs/faq/", |html| {
        html.replace(
            "<link rel=\"canonical\" href=\"{{ORIGIN}}/docs/faq/\">",
            "<link rel=\"canonical\" href=\"{{ORIGIN}}/docs/faq\">",
        )
    });
    let head = changed.serve(Some(&live_origin)).await;

    // Told which site the builds stand in for, they compare like production
    // and a preview.
    let told = |server: &FixtureSite, baseline: Option<&Snapshot>| {
        let start = server.url("/");
        let mut extras = baseline
            .map(|baseline| CrawlExtras::compared_with(baseline, &start))
            .unwrap_or_default();
        extras.production.push(live.url("/"));
        async move {
            crawl(start, &settings(), extras, &mut Progress::quiet(), "test")
                .await
                .unwrap()
        }
    };
    let baseline = told(&base, None).await;
    let current = told(&head, Some(&baseline)).await;
    let report = evaluate(Evaluation {
        command: ReportCommand::Diff,
        baseline: Some(&baseline),
        current: &current,
        settings: &settings(),
        notes: Vec::new(),
    })
    .await
    .unwrap();
    expect(&report, Rule::RobotsBlocked, Severity::Critical, None);
    expect(
        &report,
        Rule::CanonicalChanged,
        Severity::High,
        Some("/docs/faq/"),
    );

    // Not told, every page names another host as canonical. A new path on
    // that host is a changed canonical tag, not a new host, and a note says
    // how to tell auditroo about the live site.
    let baseline = crawl_site(&base, None).await;
    let report = diff_with(&baseline, &head).await;
    expect(
        &report,
        Rule::CanonicalChanged,
        Severity::High,
        Some("/docs/faq/"),
    );
    assert!(
        findings(&report, Rule::CanonicalExternal).is_empty(),
        "{}",
        render(&report)
    );
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("--production-url")),
        "{:?}",
        report.notes
    );
}

#[tokio::test]
async fn production_url_from_auditroo_toml_counts_like_the_flag() {
    let production = DocsSite::new().serve(None).await;
    let preview = DocsSite::new().serve(Some(&origin_of(&production))).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auditroo.toml");
    std::fs::write(
        &path,
        format!(
            "[crawl]\nrate_limit_ms = 0\nproduction_url = \"{}\"\n",
            production.url("/")
        ),
    )
    .unwrap();
    let settings = auditroo::config::resolve_settings(Some(&path), Default::default()).unwrap();
    let snapshot = crawl(
        preview.url("/"),
        &settings,
        CrawlExtras::default(),
        &mut Progress::quiet(),
        "test",
    )
    .await
    .unwrap();
    let report = evaluate(Evaluation {
        command: ReportCommand::Check,
        baseline: None,
        current: &snapshot,
        settings: &settings,
        notes: Vec::new(),
    })
    .await
    .unwrap();
    assert!(report.findings.is_empty(), "{}", render(&report));
}
