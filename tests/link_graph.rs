use auditroo::crawl::{CrawlItem, FetchedResponse, RedirectHop, ResponseHeaders};
use auditroo::extract::{Indexability, SeoRecord};
use auditroo::graph::{build_link_graph, orphans, LinkGraph};
use auditroo::site::Site;

fn item(url: &str, discovered_links: &[&str]) -> CrawlItem {
    CrawlItem {
        url: url.to_owned(),
        depth: 0,
        response: FetchedResponse {
            final_url: url.to_owned(),
            redirects: Vec::new(),
            status: 200,
            headers: ResponseHeaders::new(),
            content_type: Some("text/html".to_owned()),
            body: String::new(),
            rendered: false,
        },
        discovered_links: discovered_links
            .iter()
            .map(|link| (*link).to_owned())
            .collect(),
    }
}

fn by_url(records: &[&SeoRecord]) -> std::collections::BTreeMap<String, SeoRecord> {
    records
        .iter()
        .map(|record| (record.url.clone(), (*record).clone()))
        .collect()
}

fn record(url: &str, status: u16, indexable: bool, redirected: bool) -> SeoRecord {
    SeoRecord {
        url: url.to_owned(),
        final_url: if redirected {
            format!("{url}-final")
        } else {
            url.to_owned()
        },
        content_type: Some("text/html".to_owned()),
        indexability: Indexability {
            robots_meta: Vec::new(),
            googlebot_meta: Vec::new(),
            x_robots_tag: Vec::new(),
            is_indexable: indexable,
        },
        canonical: None,
        title: None,
        meta_description: None,
        headings: Default::default(),
        structured_data: Vec::new(),
        hreflang: Vec::new(),
        http_status: status,
        redirect_chain: redirected
            .then(|| RedirectHop {
                from: url.to_owned(),
                status: 301,
                to: format!("{url}-final"),
            })
            .into_iter()
            .collect(),
        rendered: false,
    }
}

#[test]
fn accumulates_graph_and_finds_orphans_from_compact_observations() {
    let home_item = item(
        "https://example.test/",
        &[
            "https://example.test/b",
            "https://example.test/a",
            "https://example.test/b",
            "https://example.test/absent",
        ],
    );
    let a_item = item("https://example.test/a", &[]);
    let b_item = item("https://example.test/b", &["https://example.test/b"]);
    let orphan_item = item("https://example.test/orphan", &[]);
    let self_only_item = item(
        "https://example.test/self-only",
        &["https://example.test/self-only"],
    );
    let noindex_source_item = item(
        "https://example.test/noindex-source",
        &["https://example.test/b"],
    );
    let not_found_item = item(
        "https://example.test/not-found",
        &["https://example.test/a"],
    );
    let redirected_item = item("https://example.test/old", &["https://example.test/a"]);

    let home_record = record("https://example.test/", 200, true, false);
    let a_record = record("https://example.test/a", 200, true, false);
    let b_record = record("https://example.test/b", 200, true, false);
    let orphan_record = record("https://example.test/orphan", 200, true, false);
    let self_only_record = record("https://example.test/self-only", 200, true, false);
    let noindex_source_record = record("https://example.test/noindex-source", 200, false, false);
    let not_found_record = record("https://example.test/not-found", 404, false, false);
    let redirected_record = record("https://example.test/old", 200, true, true);

    let graph = build_link_graph([
        (&orphan_item, &orphan_record),
        (&home_item, &home_record),
        (&redirected_item, &redirected_record),
        (&b_item, &b_record),
        (&not_found_item, &not_found_record),
        (&a_item, &a_record),
        (&self_only_item, &self_only_record),
        (&noindex_source_item, &noindex_source_record),
    ]);

    let expected_graph: LinkGraph = [
        (
            "https://example.test/".to_owned(),
            vec![
                "https://example.test/a".to_owned(),
                "https://example.test/absent".to_owned(),
                "https://example.test/b".to_owned(),
            ],
        ),
        ("https://example.test/a".to_owned(), Vec::new()),
        (
            "https://example.test/b".to_owned(),
            vec!["https://example.test/b".to_owned()],
        ),
        (
            "https://example.test/noindex-source".to_owned(),
            vec!["https://example.test/b".to_owned()],
        ),
        ("https://example.test/not-found".to_owned(), Vec::new()),
        ("https://example.test/old".to_owned(), Vec::new()),
        // A redirect's links belong to the page it ended on.
        (
            "https://example.test/old-final".to_owned(),
            vec!["https://example.test/a".to_owned()],
        ),
        ("https://example.test/orphan".to_owned(), Vec::new()),
        (
            "https://example.test/self-only".to_owned(),
            vec!["https://example.test/self-only".to_owned()],
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(graph, expected_graph);

    let records = [
        &home_record,
        &a_record,
        &b_record,
        &orphan_record,
        &self_only_record,
        &noindex_source_record,
        &not_found_record,
        &redirected_record,
    ];
    let site = Site::new("https://example.test/", std::iter::empty());
    assert_eq!(
        orphans(&by_url(&records), &graph, "https://example.test/", &site),
        vec![
            "https://example.test/orphan".to_owned(),
            "https://example.test/self-only".to_owned(),
        ]
    );

    let reshuffled = build_link_graph([
        (&noindex_source_item, &noindex_source_record),
        (&self_only_item, &self_only_record),
        (&a_item, &a_record),
        (&not_found_item, &not_found_record),
        (&b_item, &b_record),
        (&redirected_item, &redirected_record),
        (&home_item, &home_record),
        (&orphan_item, &orphan_record),
    ]);
    assert_eq!(reshuffled, graph);
}

#[test]
fn a_redirected_record_links_from_its_final_url() {
    let start_item = item(
        "https://example.test/",
        &["https://example.test/client-navigation"],
    );
    let changed_item = item(
        "https://example.test/client-navigation",
        &["https://example.test/target"],
    );
    let start_record = record("https://example.test/", 200, true, false);
    let mut changed_record = record("https://example.test/client-navigation", 200, true, false);
    changed_record.final_url = "https://example.test/client-navigation-state".to_owned();

    let graph = build_link_graph([
        (&start_item, &start_record),
        (&changed_item, &changed_record),
    ]);

    assert_eq!(
        graph.get("https://example.test/client-navigation"),
        Some(&Vec::new())
    );
    assert_eq!(
        graph.get("https://example.test/client-navigation-state"),
        Some(&vec!["https://example.test/target".to_owned()])
    );
    let site = Site::new("https://example.test/", std::iter::empty());
    assert!(orphans(
        &by_url(&[&start_record, &changed_record]),
        &graph,
        "https://example.test/",
        &site
    )
    .is_empty());
}

#[test]
fn pages_that_only_link_to_each_other_are_orphans() {
    let home_item = item("https://example.test/", &["https://example.test/a"]);
    let a_item = item("https://example.test/a", &["https://example.test/"]);
    let x_item = item("https://example.test/x", &["https://example.test/y"]);
    let y_item = item("https://example.test/y", &["https://example.test/x"]);
    let home = record("https://example.test/", 200, true, false);
    let a = record("https://example.test/a", 200, true, false);
    let x = record("https://example.test/x", 200, true, false);
    let y = record("https://example.test/y", 200, true, false);
    let graph = build_link_graph([
        (&home_item, &home),
        (&a_item, &a),
        (&x_item, &x),
        (&y_item, &y),
    ]);
    let site = Site::new("https://example.test/", std::iter::empty());
    assert_eq!(
        orphans(
            &by_url(&[&home, &a, &x, &y]),
            &graph,
            "https://example.test/",
            &site
        ),
        vec![
            "https://example.test/x".to_owned(),
            "https://example.test/y".to_owned(),
        ]
    );
}
