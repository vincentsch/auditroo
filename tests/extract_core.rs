mod support;

use auditroo::crawl::{
    normalize_url, CrawlItem, FetchedResponse, Fetcher, HttpFetcher, HttpFetcherConfig,
    RedirectHop, ResponseHeaders,
};
use auditroo::extract::{
    extract, Headings, HreflangAlternate, Indexability, JsonLdBlock, SeoRecord,
};
use support::TestServer;

fn crawl_item(
    requested_url: &str,
    final_url: &str,
    status: u16,
    headers: ResponseHeaders,
    body: &str,
) -> CrawlItem {
    CrawlItem {
        url: requested_url.to_owned(),
        depth: 0,
        response: FetchedResponse {
            final_url: final_url.to_owned(),
            redirects: Vec::new(),
            status,
            headers,
            content_type: Some("text/html".to_owned()),
            body: body.to_owned(),
            rendered: false,
        },
        discovered_links: Vec::new(),
    }
}

fn plain_item(status: u16, headers: ResponseHeaders, body: &str) -> CrawlItem {
    crawl_item(
        "https://example.test/page",
        "https://example.test/page",
        status,
        headers,
        body,
    )
}

#[test]
fn extracts_core_fields_and_preserves_crawl_response_data() {
    let mut item = crawl_item(
        "https://example.test/requested",
        "https://example.test/final",
        200,
        ResponseHeaders::from_pairs([("X-Robots-Tag", "nofollow")]),
        r#"
        <!doctype html>
        <html>
          <head>
            <base href="/docs/">
            <title>
              Example   Title
            </title>
            <meta name="robots" content="noindex, follow">
            <meta name="ROBOTS" content=" max-snippet:50 ">
            <meta name="description" content="  A compact description.  ">
            <link rel="canonical" href="guide">
          </head>
          <body>
            <h1> Main
              Heading </h1>
            <h2>First <span>Subheading</span></h2>
            <h2>Second   Subheading</h2>
            <h3> Detail
              Heading </h3>
          </body>
        </html>
        "#,
    );
    item.response.redirects = vec![RedirectHop {
        from: "https://example.test/requested".to_owned(),
        status: 301,
        to: "https://example.test/final".to_owned(),
    }];
    item.response.rendered = true;

    let record = extract(&item);

    assert_eq!(record.url, "https://example.test/requested");
    assert_eq!(record.final_url, "https://example.test/final");
    assert_eq!(record.http_status, 200);
    assert_eq!(record.redirect_chain, item.response.redirects);
    assert!(record.rendered);
    assert_eq!(
        record.indexability,
        Indexability {
            robots_meta: vec![
                "noindex".to_owned(),
                "follow".to_owned(),
                "max-snippet:50".to_owned(),
            ],
            googlebot_meta: Vec::new(),
            x_robots_tag: vec!["nofollow".to_owned()],
            is_indexable: false,
        }
    );
    assert_eq!(
        record.canonical.as_deref(),
        Some("https://example.test/docs/guide")
    );
    assert_eq!(record.title.as_deref(), Some("Example Title"));
    assert_eq!(
        record.meta_description.as_deref(),
        Some("A compact description.")
    );
    assert_eq!(
        record.headings,
        Headings {
            h1: vec!["Main Heading".to_owned()],
            h2: vec![
                "First Subheading".to_owned(),
                "Second Subheading".to_owned()
            ],
            h3: vec!["Detail Heading".to_owned()],
        }
    );
    assert!(record.structured_data.is_empty());
    assert!(record.hreflang.is_empty());
}

#[test]
fn derives_indexability_from_status_and_robots_tokens() {
    let cases = [
        (
            "indexable 200",
            plain_item(200, ResponseHeaders::new(), ""),
            true,
            Vec::<String>::new(),
            Vec::<String>::new(),
        ),
        (
            "meta noindex",
            plain_item(
                200,
                ResponseHeaders::new(),
                r#"<meta name="robots" content="index, NOINDEX, follow">"#,
            ),
            false,
            vec![
                "index".to_owned(),
                "noindex".to_owned(),
                "follow".to_owned(),
            ],
            Vec::<String>::new(),
        ),
        (
            "x robots noindex",
            plain_item(
                200,
                ResponseHeaders::from_pairs([
                    ("X-Robots-Tag", "NOFOLLOW"),
                    ("x-robots-tag", "NOINDEX, archive"),
                ]),
                "",
            ),
            false,
            Vec::<String>::new(),
            vec![
                "nofollow".to_owned(),
                "noindex".to_owned(),
                "archive".to_owned(),
            ],
        ),
        (
            "non 200",
            plain_item(404, ResponseHeaders::new(), ""),
            false,
            Vec::<String>::new(),
            Vec::<String>::new(),
        ),
        (
            "non 200 with noindex",
            plain_item(
                500,
                ResponseHeaders::from_pairs([("X-Robots-Tag", "noindex")]),
                r#"<meta name="robots" content="noindex">"#,
            ),
            false,
            vec!["noindex".to_owned()],
            vec!["noindex".to_owned()],
        ),
    ];

    for (name, item, expected_indexable, expected_meta, expected_header) in cases {
        let record = extract(&item);
        assert_eq!(
            record.indexability.is_indexable, expected_indexable,
            "{name}"
        );
        assert_eq!(record.indexability.robots_meta, expected_meta, "{name}");
        assert_eq!(record.indexability.x_robots_tag, expected_header, "{name}");
    }
}

#[test]
fn resolves_canonical_against_document_base_or_final_url() {
    let no_base = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"<link rel="canonical" href="../canonical">"#,
    ));
    assert_eq!(
        no_base.canonical.as_deref(),
        Some("https://example.test/canonical")
    );

    let valid_base = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"
        <base href="https://cdn.example.test/base/">
        <link rel="alternate canonical" href="page?x=1#frag">
        "#,
    ));
    assert_eq!(
        valid_base.canonical.as_deref(),
        Some("https://cdn.example.test/base/page?x=1#frag")
    );

    let fallback_base = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"
        <base href="http://[::1">
        <base href="mailto:seo@example.test">
        <link rel="CANONICAL" href="/fallback">
        "#,
    ));
    assert_eq!(
        fallback_base.canonical.as_deref(),
        Some("https://example.test/fallback")
    );

    let cross_host = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"<link rel="canonical" href="https://other.example/canonical">"#,
    ));
    assert_eq!(
        cross_host.canonical.as_deref(),
        Some("https://other.example/canonical")
    );

    let absent = extract(&plain_item(200, ResponseHeaders::new(), ""));
    assert_eq!(absent.canonical, None);
}

#[test]
fn extracts_json_ld_schema_type_from_top_level_object() {
    let record = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"<script type="application/ld+json">{"@context":"https://schema.org","@type":"Product","name":"Widget"}</script>"#,
    ));

    assert_eq!(
        record.structured_data,
        vec![JsonLdBlock {
            raw: r#"{"@context":"https://schema.org","@type":"Product","name":"Widget"}"#
                .to_owned(),
            valid_json: true,
            types: vec!["Product".to_owned()],
            error: None,
        }]
    );
}

#[test]
fn captures_invalid_json_ld_raw_with_no_schema_type() {
    let record = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"<script type="application/ld+json">{"@type":"Product"</script>"#,
    ));

    assert_eq!(record.structured_data.len(), 1);
    let block = &record.structured_data[0];
    assert_eq!(block.raw, r#"{"@type":"Product""#);
    assert!(!block.valid_json);
    assert!(block.types.is_empty());
    assert!(block
        .error
        .as_deref()
        .is_some_and(|error| error.contains("EOF while parsing")));
}

#[test]
fn googlebot_rules_count_and_rules_for_other_crawlers_do_not() {
    let meta = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"<meta name="googlebot" content="noindex"><meta name="bingbot" content="nofollow">"#,
    ));
    assert!(!meta.indexability.is_indexable);
    assert!(meta.has_noindex());
    assert_eq!(meta.indexability.googlebot_meta, vec!["noindex"]);
    assert!(meta.indexability.robots_meta.is_empty());

    let scoped = extract(&plain_item(
        200,
        ResponseHeaders::from_pairs([("X-Robots-Tag", "googlebot: noindex")]),
        "",
    ));
    assert!(!scoped.indexability.is_indexable);
    assert_eq!(scoped.indexability.x_robots_tag, vec!["noindex"]);

    let other = extract(&plain_item(
        200,
        ResponseHeaders::from_pairs([
            ("X-Robots-Tag", "otherbot: noindex, nofollow"),
            (
                "X-Robots-Tag",
                "unavailable_after: 25 Jun 2030 15:00:00 PST",
            ),
        ]),
        "",
    ));
    assert!(other.indexability.is_indexable);
    assert_eq!(
        other.indexability.x_robots_tag,
        vec!["unavailable_after: 25 jun 2030 15:00:00 pst"]
    );

    let mixed = extract(&plain_item(
        200,
        ResponseHeaders::from_pairs([(
            "X-Robots-Tag",
            "bingbot: noindex, googlebot: nofollow, noindex",
        )]),
        "",
    ));
    assert_eq!(mixed.indexability.x_robots_tag, vec!["nofollow", "noindex"]);
    assert!(!mixed.indexability.is_indexable);
}

#[test]
fn extracts_every_json_ld_type_from_graph_lists_and_type_arrays() {
    let record = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"<script type="application/ld+json">{"@context":"https://schema.org","@graph":[{"@type":"WebPage","author":{"@type":"Person"}},{"@type":["Product","Thing"]},{"@type":"WebPage"}]}</script>
        <script type="application/ld+json">[{"@type":"Organization"},{"@type":"WebSite"}]</script>"#,
    ));

    assert_eq!(record.structured_data.len(), 2);
    assert!(record.structured_data[0].valid_json);
    assert_eq!(
        record.structured_data[0].types,
        vec!["Product", "Thing", "WebPage"]
    );
    assert_eq!(
        record.structured_data[1].types,
        vec!["Organization", "WebSite"]
    );
}

#[test]
fn captures_multiple_json_ld_blocks_and_array_schema_type() {
    let record = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"
        <script type="APPLICATION/LD+JSON; charset=utf-8">[{"@type":"BreadcrumbList"}]</script>
        <script type="application/ld+json">{"@type":"FAQPage"}</script>
        <script type="application/json">{"@type":"Ignored"}</script>
        "#,
    ));

    assert_eq!(
        record
            .structured_data
            .iter()
            .map(|block| block.types.clone())
            .collect::<Vec<_>>(),
        vec![vec!["BreadcrumbList"], vec!["FAQPage"]]
    );
    assert!(record.structured_data.iter().all(|block| block.valid_json));
}

#[test]
fn extracts_hreflang_alternates_against_document_base() {
    let record = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"
        <base href="/localized/">
        <link rel="alternate" hreflang=" EN-US " href="en/page">
        <link rel="ALTERNATE stylesheet" hreflang="X-DEFAULT" href="/">
        <link rel="alternate" href="/rss.xml">
        <link rel="alternate" hreflang="fr" href="http://[::1">
        "#,
    ));

    assert_eq!(
        record.hreflang,
        vec![
            HreflangAlternate {
                hreflang: "en-us".to_owned(),
                href: "https://example.test/localized/en/page".to_owned(),
            },
            HreflangAlternate {
                hreflang: "x-default".to_owned(),
                href: "https://example.test/".to_owned(),
            },
        ]
    );
}

#[test]
fn resolves_hreflang_against_final_url_when_no_base_is_valid() {
    let record = extract(&crawl_item(
        "https://example.test/requested",
        "https://example.test/products/widget/index.html",
        200,
        ResponseHeaders::new(),
        r#"
        <base href="mailto:seo@example.test">
        <link rel="alternate" hreflang="de" href="../de/widget">
        "#,
    ));

    assert_eq!(
        record.hreflang,
        vec![HreflangAlternate {
            hreflang: "de".to_owned(),
            href: "https://example.test/products/de/widget".to_owned(),
        }]
    );
}

#[test]
fn missing_title_and_description_yield_none_and_empty_heading_vectors() {
    let record = extract(&plain_item(
        200,
        ResponseHeaders::new(),
        r#"<html><body><p>No metadata here.</p></body></html>"#,
    ));

    assert_eq!(record.title, None);
    assert_eq!(record.meta_description, None);
    assert_eq!(record.headings, Headings::default());
}

#[test]
fn seo_record_round_trips_through_serde() {
    let record = SeoRecord {
        url: "https://example.test/source".to_owned(),
        final_url: "https://example.test/final".to_owned(),
        content_type: Some("text/html".to_owned()),
        indexability: Indexability {
            robots_meta: vec!["index".to_owned()],
            googlebot_meta: Vec::new(),
            x_robots_tag: vec!["follow".to_owned()],
            is_indexable: true,
        },
        canonical: Some("https://example.test/canonical".to_owned()),
        title: Some("Title".to_owned()),
        meta_description: Some("Description".to_owned()),
        headings: Headings {
            h1: vec!["H1".to_owned()],
            h2: vec!["H2".to_owned()],
            h3: vec!["H3".to_owned()],
        },
        structured_data: vec![JsonLdBlock {
            raw: r#"{"@type":"Product"}"#.to_owned(),
            valid_json: true,
            types: vec!["Product".to_owned()],
            error: None,
        }],
        hreflang: vec![HreflangAlternate {
            hreflang: "x-default".to_owned(),
            href: "https://example.test/".to_owned(),
        }],
        http_status: 200,
        redirect_chain: vec![RedirectHop {
            from: "https://example.test/source".to_owned(),
            status: 302,
            to: "https://example.test/final".to_owned(),
        }],
        rendered: false,
    };

    let encoded = serde_json::to_string(&record).expect("record serializes");
    let decoded: SeoRecord = serde_json::from_str(&encoded).expect("record deserializes");

    assert_eq!(decoded, record);
}

#[tokio::test]
async fn extracts_record_from_local_http_fetch_response() {
    let server = TestServer::spawn().await;
    let fetcher = HttpFetcher::new(HttpFetcherConfig::default()).expect("fetcher builds");
    let requested = server.url("/chain/a");
    let response = fetcher.fetch(&requested).await.expect("fetch succeeds");
    let item = CrawlItem {
        url: normalize_url(&requested),
        depth: 0,
        response,
        discovered_links: Vec::new(),
    };

    let record = extract(&item);
    let requests = server.requests().await;

    assert_eq!(record.url, normalize_url(&requested));
    assert_eq!(record.http_status, 200);
    assert!(record.final_url.ends_with("/chain/d"));
    assert_eq!(record.redirect_chain.len(), 3);
    assert!(!record.rendered);
    assert_eq!(server.addr().port(), requested.port().expect("port exists"));
    assert_eq!(requests.first().expect("request recorded").path, "/chain/a");
    assert!(requests
        .first()
        .expect("request recorded")
        .header_value("user-agent")
        .is_some());
}
