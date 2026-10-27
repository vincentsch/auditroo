use std::collections::BTreeMap;
use std::fs;

use auditroo::crawl::{CrawlFetchFailure, FetchError, RobotsFile};
use auditroo::extract::{Headings, Indexability, SeoRecord};
use auditroo::sitemap::{SitemapDiscovery, SitemapDiscoveryStatus};
use auditroo::snapshot::{
    assemble, check_writable, read, snapshot_from_str, summarize_failure, write, CrawlFailureKind,
    CrawlObservation, CrawlSkipReason, SnapshotInput,
};
use time::OffsetDateTime;

fn created_at() -> OffsetDateTime {
    OffsetDateTime::parse(
        "2026-08-18T09:00:00.5Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("test timestamp parses")
}

fn record(url: &str) -> SeoRecord {
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
        title: Some(format!("Title for {url}")),
        meta_description: None,
        headings: Headings::default(),
        structured_data: Vec::new(),
        hreflang: Vec::new(),
        http_status: 200,
        redirect_chain: Vec::new(),
        rendered: false,
    }
}

fn robots() -> RobotsFile {
    RobotsFile {
        url: "https://example.test/robots.txt".to_owned(),
        status: Some(200),
        body: Some("User-agent: *\nAllow: /\n".to_owned()),
        error: None,
    }
}

fn input(records: Vec<SeoRecord>, links: &[(&str, &[&str])], declared: &[&str]) -> SnapshotInput {
    SnapshotInput {
        start_url: "https://example.test/".to_owned(),
        production_urls: Vec::new(),
        crawl_observation: CrawlObservation {
            respect_robots: true,
            ..CrawlObservation::default()
        },
        robots: [("https://example.test".to_owned(), robots())]
            .into_iter()
            .collect(),
        records,
        sitemap: SitemapDiscovery {
            status: SitemapDiscoveryStatus::Complete,
            documents: Vec::new(),
            declared_urls: declared.iter().map(|url| (*url).to_owned()).collect(),
        },
        link_graph: links
            .iter()
            .map(|(source, targets)| {
                (
                    (*source).to_owned(),
                    targets.iter().map(|target| (*target).to_owned()).collect(),
                )
            })
            .collect(),
        crawl_failures: Vec::new(),
    }
}

#[test]
fn snapshots_are_stable_whatever_order_the_crawl_found_things() {
    let start = "https://example.test/";
    let about = "https://example.test/about";
    let orphan = "https://example.test/orphan";
    let first = assemble(
        input(
            vec![record(start), record(about), record(orphan)],
            &[(start, &[about, about]), (about, &[]), (orphan, &[])],
            &[orphan, start, about, start],
        ),
        created_at(),
        None,
    )
    .expect("snapshot assembles");
    let second = assemble(
        input(
            vec![record(orphan), record(about), record(start)],
            &[(orphan, &[]), (about, &[]), (start, &[about])],
            &[about, orphan, start],
        ),
        created_at(),
        None,
    )
    .expect("snapshot assembles");

    assert_eq!(
        serde_json::to_string_pretty(&first).unwrap(),
        serde_json::to_string_pretty(&second).unwrap()
    );
    assert_eq!(first.orphans, vec![orphan.to_owned()]);
    assert_eq!(first.created_at.nanosecond(), 0);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.json");
    write(&first, &path).expect("snapshot writes");
    let raw = fs::read_to_string(&path).unwrap();
    assert!(raw.ends_with('\n'));
    assert!(raw.contains("\"artifact_kind\": \"snapshot\""));
    assert!(raw.contains("\"robots\""));
    assert_eq!(read(&path).expect("snapshot reads"), first);
}

#[test]
fn read_errors_name_the_file_and_the_problem() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.json");
    let error = read(&missing).unwrap_err().to_string();
    assert!(error.contains("missing.json"), "{error}");
    assert!(error.contains("file not found"), "{error}");

    let report = dir.path().join("report.json");
    fs::write(&report, r#"{"artifact_kind":"report","schema_version":1}"#).unwrap();
    let error = read(&report).unwrap_err().to_string();
    assert!(error.contains("report.json"), "{error}");
    assert!(error.contains("not a snapshot"), "{error}");

    let old = snapshot_from_str(r#"{"artifact_kind":"snapshot","schema_version":7}"#)
        .unwrap_err()
        .to_string();
    assert!(old.contains("version 7"), "{old}");
    assert!(snapshot_from_str("not json").is_err());
}

#[test]
fn output_paths_are_checked_before_a_crawl() {
    let dir = tempfile::tempdir().unwrap();
    assert!(check_writable(&dir.path().join("baseline.json")).is_ok());
    let error = check_writable(&dir.path().join("missing").join("baseline.json")).unwrap_err();
    assert!(error.contains("does not exist"), "{error}");
    assert!(check_writable(dir.path()).is_err());
    assert!(check_writable(std::path::Path::new("baseline.json")).is_ok());
}

#[test]
fn a_failed_write_leaves_the_old_file_alone() {
    let snapshot = assemble(
        input(vec![record("https://example.test/")], &[], &[]),
        created_at(),
        None,
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.json");
    fs::write(&path, "old contents\n").unwrap();
    assert!(write(&snapshot, &dir.path().join("missing").join("snapshot.json")).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "old contents\n");
}

#[test]
fn failures_keep_their_cause() {
    let (url, summary) = summarize_failure(&CrawlFetchFailure {
        url: "https://EXAMPLE.test/a#top".to_owned(),
        depth: 2,
        error: FetchError::Request {
            detail: "the connection was refused. Is the server running on that port?".to_owned(),
        },
    });
    assert_eq!(url, "https://example.test/a");
    assert_eq!(summary.kind, CrawlFailureKind::Request);
    assert!(summary.detail.unwrap().contains("refused"));

    let (_, timeout) = summarize_failure(&CrawlFetchFailure {
        url: "https://example.test/slow".to_owned(),
        depth: 1,
        error: FetchError::Timeout,
    });
    assert_eq!(timeout.kind, CrawlFailureKind::Timeout);
}

#[test]
fn a_crawl_that_robots_txt_stopped_is_a_valid_snapshot() {
    let mut blocked = input(Vec::new(), &[], &[]);
    blocked.crawl_observation.skipped_candidates = BTreeMap::from([(
        "https://example.test/".to_owned(),
        vec![CrawlSkipReason::RobotsDenied],
    )]);
    let snapshot = assemble(blocked, created_at(), None).expect("blocked snapshot assembles");
    assert!(snapshot.start_blocked_by_robots());
    assert!(snapshot.records.is_empty());

    let empty = input(Vec::new(), &[], &[]);
    assert!(assemble(empty, created_at(), None).is_err());
}
