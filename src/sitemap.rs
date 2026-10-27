use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Cursor, Read};
use std::time::Duration;

use flate2::read::GzDecoder;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use url::Url;

use crate::crawl::{
    normalize_url, CrawlScope, CrawlSeed, CrawlSeedSource, FetchError, FetchedBytesResponse,
    HttpFetcher, RobotsPolicy,
};
use crate::extract::SeoRecord;
use crate::site::Site;

const STANDARD_SITEMAP_NAMESPACE: &str = "http://www.sitemaps.org/schemas/sitemap/0.9";
const DEFAULT_MAX_SITEMAP_DOCUMENTS: usize = 1_000;
const DEFAULT_MAX_DECLARED_URLS: usize = 1_000_000;
const DEFAULT_MAX_DEPTH: usize = 8;
const DEFAULT_MAX_DECOMPRESSED_BYTES: usize = 52_428_800;
const DETAIL_LIMIT: usize = 240;
const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SitemapDiscoveryStatus {
    Complete,
    Absent,
    Invalid,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SitemapDocumentOutcome {
    ParsedUrlSet,
    ParsedSitemapIndex,
    Missing,
    Invalid,
    Unavailable,
    SkippedLimit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitemapDocumentDiagnostic {
    pub requested_url: String,
    pub final_url: Option<String>,
    pub final_status: Option<u16>,
    pub outcome: SitemapDocumentOutcome,
    pub detail: Option<String>,
    /// Entries with a full URL, for a file that could be parsed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub entries: usize,
    /// Entries whose `<loc>` is missing or not a full http:// or https://
    /// URL. Google ignores them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub invalid_entries: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitemapDiscovery {
    pub status: SitemapDiscoveryStatus,
    pub documents: Vec<SitemapDocumentDiagnostic>,
    /// Every page URL the sitemaps list. Production URLs of a preview are
    /// already mapped to the preview. URLs on other hosts are stored as they
    /// are. auditroo does not crawl them, and `check` reports them.
    pub declared_urls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitemapCoverage {
    pub discovery: SitemapDiscovery,
    pub declared_not_observed: Vec<String>,
    pub declared_observed_not_directly_indexable: Vec<String>,
    pub indexable_not_declared: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitemapDiscoveryConfig {
    pub max_sitemap_documents: usize,
    pub max_declared_urls: usize,
    pub max_depth: usize,
    pub max_decompressed_bytes: usize,
    pub rate_limit_ms: u64,
}

impl Default for SitemapDiscoveryConfig {
    fn default() -> Self {
        Self {
            max_sitemap_documents: DEFAULT_MAX_SITEMAP_DOCUMENTS,
            max_declared_urls: DEFAULT_MAX_DECLARED_URLS,
            max_depth: DEFAULT_MAX_DEPTH,
            max_decompressed_bytes: DEFAULT_MAX_DECOMPRESSED_BYTES,
            rate_limit_ms: 0,
        }
    }
}

pub async fn discover_sitemaps(
    start_url: &Url,
    fetcher: &HttpFetcher,
    robots_policy: &RobotsPolicy,
    scope: &CrawlScope,
    config: SitemapDiscoveryConfig,
) -> SitemapDiscovery {
    discover_sitemaps_from_urls(
        start_url,
        fetcher,
        robots_policy.sitemap_urls(),
        scope,
        config,
    )
    .await
}

pub async fn discover_sitemaps_from_urls(
    start_url: &Url,
    fetcher: &HttpFetcher,
    root_urls: impl IntoIterator<Item = String>,
    scope: &CrawlScope,
    config: SitemapDiscoveryConfig,
) -> SitemapDiscovery {
    let root_urls: Vec<_> = root_urls.into_iter().collect();
    let used_fallback = root_urls.is_empty();
    let mut documents = Vec::new();
    let mut current_level = BTreeMap::new();
    let mapper = SiteMapper { scope };

    if used_fallback {
        let fallback = conventional_sitemap_url(start_url);
        current_level.insert(normalize_url(&fallback), fallback);
    } else {
        for root in root_urls {
            enqueue_root(&root, &mapper, &mut current_level, &mut documents);
        }
    }

    let mut seen_documents = BTreeSet::new();
    let mut attempted_documents = 0usize;
    let mut declared_urls = BTreeSet::new();
    let mut parsed_documents = 0usize;
    let mut partial_issue = false;
    let mut rate_limits = HashMap::new();
    let mut document_limit_skip = LimitSkip::default();

    for depth in 0..=config.max_depth {
        let mut next_level = BTreeMap::new();

        for (requested_url, url) in current_level {
            if !seen_documents.insert(requested_url.clone()) {
                continue;
            }
            if attempted_documents >= config.max_sitemap_documents {
                partial_issue = true;
                document_limit_skip.record(requested_url);
                continue;
            }

            attempted_documents += 1;
            wait_for_host_turn(&url, config.rate_limit_ms, &mut rate_limits).await;

            let processed =
                fetch_and_parse_document(start_url, fetcher, &url, &mapper, &config).await;
            let ProcessedDocument {
                diagnostic,
                declared,
                children,
                parsed,
                partial,
            } = processed;

            if parsed {
                parsed_documents += 1;
            }
            if partial {
                partial_issue = true;
            }

            let skipped_urls = merge_declared_urls(&mut declared_urls, declared, &config);
            if skipped_urls > 0 {
                partial_issue = true;
                documents.push(append_detail(
                    diagnostic,
                    format!(
                        "auditroo reads at most {} URLs from sitemaps, so {skipped_urls} more were not read",
                        crate::findings::format_count(config.max_declared_urls)
                    ),
                ));
            } else {
                documents.push(diagnostic);
            }

            for (child_url, child) in children {
                if !seen_documents.contains(&child_url) {
                    next_level.entry(child_url).or_insert(child);
                }
            }
        }

        if attempted_documents >= config.max_sitemap_documents && !next_level.is_empty() {
            partial_issue = true;
            for child_url in next_level.into_keys() {
                document_limit_skip.record(child_url);
            }
            if let Some(diagnostic) =
                document_limit_skip.into_diagnostic("auditroo reached its limit of sitemap files")
            {
                documents.push(diagnostic);
            }
            return finalize_discovery(
                documents,
                declared_urls,
                parsed_documents,
                partial_issue,
                used_fallback,
            );
        }

        if let Some(diagnostic) =
            document_limit_skip.take_diagnostic("auditroo reached its limit of sitemap files")
        {
            documents.push(diagnostic);
            return finalize_discovery(
                documents,
                declared_urls,
                parsed_documents,
                partial_issue,
                used_fallback,
            );
        }

        if next_level.is_empty() {
            return finalize_discovery(
                documents,
                declared_urls,
                parsed_documents,
                partial_issue,
                used_fallback,
            );
        }

        if depth == config.max_depth {
            partial_issue = true;
            let mut depth_limit_skip = LimitSkip::default();
            for child_url in next_level.into_keys() {
                depth_limit_skip.record(child_url);
            }
            if let Some(diagnostic) =
                depth_limit_skip.into_diagnostic("the sitemap indexes are nested too deep")
            {
                documents.push(diagnostic);
            }
            return finalize_discovery(
                documents,
                declared_urls,
                parsed_documents,
                partial_issue,
                used_fallback,
            );
        }

        current_level = next_level;
    }

    finalize_discovery(
        documents,
        declared_urls,
        parsed_documents,
        partial_issue,
        used_fallback,
    )
}

/// The listed pages to crawl: those on the crawled site.
pub fn sitemap_seeds(discovery: &SitemapDiscovery, scope: &CrawlScope) -> Vec<CrawlSeed> {
    discovery
        .declared_urls
        .iter()
        .filter_map(|url| Url::parse(url).ok())
        .filter(|url| scope.is_same_site(url))
        .map(|url| CrawlSeed {
            url,
            source: CrawlSeedSource::Sitemap,
        })
        .collect()
}

pub fn compute_sitemap_coverage<'a>(
    discovery: SitemapDiscovery,
    records: impl IntoIterator<Item = &'a SeoRecord>,
    site: &Site,
) -> SitemapCoverage {
    let records_by_url: BTreeMap<_, _> = records
        .into_iter()
        .map(|record| (record.url.clone(), record))
        .collect();
    let declared: BTreeSet<_> = discovery.declared_urls.iter().cloned().collect();

    let mut coverage = SitemapCoverage {
        discovery,
        declared_not_observed: Vec::new(),
        declared_observed_not_directly_indexable: Vec::new(),
        indexable_not_declared: Vec::new(),
    };

    match coverage.discovery.status {
        SitemapDiscoveryStatus::Complete | SitemapDiscoveryStatus::Partial => {
            for url in &coverage.discovery.declared_urls {
                match records_by_url.get(url) {
                    None => coverage.declared_not_observed.push(url.clone()),
                    Some(record) if !site.is_indexable(record) => coverage
                        .declared_observed_not_directly_indexable
                        .push(url.clone()),
                    Some(_) => {}
                }
            }
        }
        SitemapDiscoveryStatus::Absent
        | SitemapDiscoveryStatus::Invalid
        | SitemapDiscoveryStatus::Unavailable => {}
    }

    if coverage.discovery.status == SitemapDiscoveryStatus::Complete {
        coverage.indexable_not_declared = records_by_url
            .values()
            .filter(|record| site.is_indexable(record))
            .map(|record| record.url.clone())
            .filter(|url| !declared.contains(url))
            .collect();
    }

    coverage
}

struct ProcessedDocument {
    diagnostic: SitemapDocumentDiagnostic,
    declared: BTreeSet<String>,
    children: BTreeMap<String, Url>,
    parsed: bool,
    partial: bool,
}

/// Decides where sitemap files are read and which listed URLs belong to the
/// site.
///
/// URLs on the crawled host are the site's own. When a preview is crawled,
/// URLs on the production host count as the preview's own and are read on
/// the preview under the same path, so a preview crawl never reads
/// production's sitemap. A sitemap file on any other host is read where it
/// is, because Google accepts sitemaps that robots.txt names on another
/// host. Page URLs on other hosts are not part of the site. They are still
/// stored, so the checks can report them. That is how a staging host in the
/// sitemap shows up.
struct SiteMapper<'a> {
    scope: &'a CrawlScope,
}

impl SiteMapper<'_> {
    fn document(&self, url: Url) -> Url {
        self.scope.localize(url)
    }

    /// The URL to record for a listed page, or `None` when --include or
    /// --exclude leaves it out.
    fn page(&self, url: Url) -> Option<Url> {
        let url = self.scope.localize(url);
        let wanted = if self.scope.is_same_site(&url) {
            self.scope.allows_candidate(&url, 0)
        } else {
            self.scope.allows_path(&url)
        };
        wanted.then_some(url)
    }
}

#[derive(Default)]
struct LimitSkip {
    first_url: Option<String>,
    count: usize,
}

impl LimitSkip {
    fn record(&mut self, requested_url: String) {
        if self.first_url.is_none() {
            self.first_url = Some(requested_url);
        }
        self.count += 1;
    }

    fn take_diagnostic(&mut self, reason: &str) -> Option<SitemapDocumentDiagnostic> {
        let first_url = self.first_url.take()?;
        let count = std::mem::take(&mut self.count);
        Some(limit_diagnostic(first_url, reason, count))
    }

    fn into_diagnostic(self, reason: &str) -> Option<SitemapDocumentDiagnostic> {
        self.first_url
            .map(|first_url| limit_diagnostic(first_url, reason, self.count))
    }
}

fn limit_diagnostic(
    requested_url: String,
    reason: &str,
    count: usize,
) -> SitemapDocumentDiagnostic {
    let files = if count == 1 {
        "1 sitemap file was".to_owned()
    } else {
        format!("{count} sitemap files were")
    };
    diagnostic(
        requested_url,
        None,
        None,
        SitemapDocumentOutcome::SkippedLimit,
        Some(format!("{reason}, so {files} not read")),
    )
}

async fn fetch_and_parse_document(
    start_url: &Url,
    fetcher: &HttpFetcher,
    url: &Url,
    mapper: &SiteMapper<'_>,
    config: &SitemapDiscoveryConfig,
) -> ProcessedDocument {
    let requested_url = normalize_url(url);

    match fetcher.fetch_bytes_with_observation(url, start_url).await {
        Ok(response) => process_response(requested_url, response, mapper, config),
        Err(failure) => ProcessedDocument {
            diagnostic: diagnostic(
                requested_url,
                normalized_observed_url(failure.final_url),
                failure.final_status,
                SitemapDocumentOutcome::Unavailable,
                Some(fetch_error_detail(&failure.error)),
            ),
            declared: BTreeSet::new(),
            children: BTreeMap::new(),
            parsed: false,
            partial: false,
        },
    }
}

fn process_response(
    requested_url: String,
    response: FetchedBytesResponse,
    mapper: &SiteMapper<'_>,
    config: &SitemapDiscoveryConfig,
) -> ProcessedDocument {
    let final_url = Url::parse(&response.final_url)
        .ok()
        .map(|url| normalize_url(&url));
    let status = Some(response.status);

    match response.status {
        200 => match prepare_xml_bytes(&response.bytes, config.max_decompressed_bytes) {
            Ok(bytes) => match parse_sitemap_xml(&bytes) {
                Ok(parsed) => parsed_document(requested_url, final_url, status, parsed, mapper),
                Err(detail) => invalid_document(requested_url, final_url, status, detail),
            },
            Err(BytePreparationError::Invalid(detail)) => {
                invalid_document(requested_url, final_url, status, detail)
            }
            Err(BytePreparationError::Unavailable(detail)) => {
                unavailable_document(requested_url, final_url, status, detail)
            }
        },
        404 | 410 => ProcessedDocument {
            diagnostic: diagnostic(
                requested_url,
                final_url,
                status,
                SitemapDocumentOutcome::Missing,
                None,
            ),
            declared: BTreeSet::new(),
            children: BTreeMap::new(),
            parsed: false,
            partial: false,
        },
        status if (300..400).contains(&status) => unavailable_document(
            requested_url,
            final_url,
            Some(status),
            format!("it answers {status}, but the redirect cannot be followed"),
        ),
        status if matches!(status, 401 | 403 | 429) || (500..600).contains(&status) => {
            unavailable_document(
                requested_url,
                final_url,
                Some(status),
                format!("it returns {status}"),
            )
        }
        status if (400..500).contains(&status) => invalid_document(
            requested_url,
            final_url,
            Some(status),
            format!("it returns {status}"),
        ),
        status => invalid_document(
            requested_url,
            final_url,
            Some(status),
            format!("it returns status {status}"),
        ),
    }
}

fn parsed_document(
    requested_url: String,
    final_url: Option<String>,
    final_status: Option<u16>,
    parsed: ParsedSitemapDocument,
    mapper: &SiteMapper<'_>,
) -> ProcessedDocument {
    let entries = parsed.page_urls.len() + parsed.child_sitemaps.len();
    let invalid_entries = parsed.invalid_page_locations + parsed.invalid_child_locations;
    let declared = parsed
        .page_urls
        .into_iter()
        .filter_map(|url| mapper.page(url))
        .map(|url| normalize_url(&url))
        .collect::<BTreeSet<_>>();
    let children = parsed
        .child_sitemaps
        .into_values()
        .map(|url| mapper.document(url))
        .map(|url| (normalize_url(&url), url))
        .collect::<BTreeMap<_, _>>();
    let partial = parsed.invalid_page_locations > 0 || parsed.invalid_child_locations > 0;
    let detail = invalid_entry_detail(
        parsed.invalid_page_locations,
        parsed.invalid_child_locations,
    );
    let outcome = match parsed.kind {
        SitemapRootKind::UrlSet => SitemapDocumentOutcome::ParsedUrlSet,
        SitemapRootKind::SitemapIndex => SitemapDocumentOutcome::ParsedSitemapIndex,
    };

    let mut diagnostic = diagnostic(requested_url, final_url, final_status, outcome, detail);
    diagnostic.entries = entries;
    diagnostic.invalid_entries = invalid_entries;
    ProcessedDocument {
        diagnostic,
        declared,
        children,
        parsed: true,
        partial,
    }
}

fn invalid_document(
    requested_url: String,
    final_url: Option<String>,
    final_status: Option<u16>,
    detail: impl Into<String>,
) -> ProcessedDocument {
    ProcessedDocument {
        diagnostic: diagnostic(
            requested_url,
            final_url,
            final_status,
            SitemapDocumentOutcome::Invalid,
            Some(detail.into()),
        ),
        declared: BTreeSet::new(),
        children: BTreeMap::new(),
        parsed: false,
        partial: false,
    }
}

fn unavailable_document(
    requested_url: String,
    final_url: Option<String>,
    final_status: Option<u16>,
    detail: impl Into<String>,
) -> ProcessedDocument {
    ProcessedDocument {
        diagnostic: diagnostic(
            requested_url,
            final_url,
            final_status,
            SitemapDocumentOutcome::Unavailable,
            Some(detail.into()),
        ),
        declared: BTreeSet::new(),
        children: BTreeMap::new(),
        parsed: false,
        partial: false,
    }
}

fn merge_declared_urls(
    target: &mut BTreeSet<String>,
    source: BTreeSet<String>,
    config: &SitemapDiscoveryConfig,
) -> usize {
    let mut skipped = 0usize;
    for url in source {
        if target.contains(&url) {
            continue;
        }
        if target.len() >= config.max_declared_urls {
            skipped += 1;
            continue;
        }
        target.insert(url);
    }
    skipped
}

fn finalize_discovery(
    mut documents: Vec<SitemapDocumentDiagnostic>,
    declared_urls: BTreeSet<String>,
    parsed_documents: usize,
    partial_issue: bool,
    used_fallback: bool,
) -> SitemapDiscovery {
    documents.sort_by(|left, right| left.requested_url.cmp(&right.requested_url));
    documents.dedup_by(|left, right| left.requested_url == right.requested_url);

    let status = if parsed_documents > 0 {
        if partial_issue
            || documents.iter().any(|document| {
                matches!(
                    document.outcome,
                    SitemapDocumentOutcome::Invalid
                        | SitemapDocumentOutcome::Missing
                        | SitemapDocumentOutcome::Unavailable
                        | SitemapDocumentOutcome::SkippedLimit
                )
            })
        {
            SitemapDiscoveryStatus::Partial
        } else {
            SitemapDiscoveryStatus::Complete
        }
    } else if used_fallback
        && documents.len() == 1
        && documents[0].outcome == SitemapDocumentOutcome::Missing
    {
        SitemapDiscoveryStatus::Absent
    } else if documents.iter().any(|document| {
        matches!(
            document.outcome,
            SitemapDocumentOutcome::Unavailable | SitemapDocumentOutcome::SkippedLimit
        )
    }) {
        SitemapDiscoveryStatus::Unavailable
    } else {
        SitemapDiscoveryStatus::Invalid
    };

    SitemapDiscovery {
        status,
        documents,
        declared_urls: declared_urls.into_iter().collect(),
    }
}

fn enqueue_root(
    root: &str,
    mapper: &SiteMapper<'_>,
    roots: &mut BTreeMap<String, Url>,
    documents: &mut Vec<SitemapDocumentDiagnostic>,
) {
    match Url::parse(root) {
        Ok(url) if is_http_url(&url) => {
            let url = mapper.document(url);
            roots.entry(normalize_url(&url)).or_insert(url);
        }
        Ok(url) => {
            documents.push(diagnostic(
                diagnostic_url(&url),
                None,
                None,
                SitemapDocumentOutcome::Invalid,
                Some("its URL is not an http:// or https:// URL".to_owned()),
            ));
        }
        Err(error) => {
            documents.push(diagnostic(
                sanitize_detail(root),
                None,
                None,
                SitemapDocumentOutcome::Invalid,
                Some(format!("its URL is not valid ({error})")),
            ));
        }
    }
}

fn conventional_sitemap_url(start_url: &Url) -> Url {
    let mut url = start_url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_path("/sitemap.xml");
    url.set_query(None);
    url.set_fragment(None);
    url
}

async fn wait_for_host_turn(
    url: &Url,
    rate_limit_ms: u64,
    rate_limits: &mut HashMap<String, Instant>,
) {
    if rate_limit_ms == 0 {
        return;
    }

    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
        return;
    };
    let delay = Duration::from_millis(rate_limit_ms);
    let now = Instant::now();
    let next_allowed = rate_limits.get(&host).copied().unwrap_or(now);

    if now < next_allowed {
        tokio::time::sleep(next_allowed - now).await;
    }
    rate_limits.insert(host, Instant::now() + delay);
}

enum BytePreparationError {
    Invalid(String),
    Unavailable(String),
}

fn prepare_xml_bytes(
    bytes: &[u8],
    max_decompressed_bytes: usize,
) -> Result<Vec<u8>, BytePreparationError> {
    if bytes.starts_with(GZIP_MAGIC) {
        let decoder = GzDecoder::new(bytes);
        read_to_limit(decoder, max_decompressed_bytes).map_err(|error| match error {
            ReadLimitError::LimitExceeded => BytePreparationError::Unavailable(format!(
                "it is larger than {max_decompressed_bytes} bytes after unzipping"
            )),
            ReadLimitError::ReadFailed(detail) => {
                BytePreparationError::Invalid(format!("the gzip file is damaged ({detail})"))
            }
        })
    } else if bytes.len() > max_decompressed_bytes {
        Err(BytePreparationError::Unavailable(format!(
            "it is larger than {max_decompressed_bytes} bytes"
        )))
    } else {
        Ok(bytes.to_vec())
    }
}

enum ReadLimitError {
    LimitExceeded,
    ReadFailed(String),
}

fn read_to_limit(mut reader: impl Read, limit: usize) -> Result<Vec<u8>, ReadLimitError> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| ReadLimitError::ReadFailed(error.to_string()))?;
        if read == 0 {
            return Ok(output);
        }
        if read > limit.saturating_sub(output.len()) {
            return Err(ReadLimitError::LimitExceeded);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SitemapRootKind {
    UrlSet,
    SitemapIndex,
}

#[derive(Default)]
struct EntryState {
    loc: Option<String>,
}

struct ParsedSitemapDocument {
    kind: SitemapRootKind,
    page_urls: Vec<Url>,
    child_sitemaps: BTreeMap<String, Url>,
    invalid_page_locations: usize,
    invalid_child_locations: usize,
}

#[derive(Default)]
struct XmlParseState {
    stack: Vec<String>,
    root_kind: Option<SitemapRootKind>,
    page_urls: Vec<Url>,
    child_sitemaps: BTreeMap<String, Url>,
    invalid_page_locations: usize,
    invalid_child_locations: usize,
    page_entry: Option<EntryState>,
    child_entry: Option<EntryState>,
    collecting: Option<LocTarget>,
}

fn parse_sitemap_xml(bytes: &[u8]) -> Result<ParsedSitemapDocument, String> {
    if contains_ascii_case_insensitive(bytes, b"<!doctype")
        || contains_ascii_case_insensitive(bytes, b"<!entity")
    {
        return Err(
            "it has a DOCTYPE or ENTITY declaration, which auditroo does not read".to_owned(),
        );
    }

    let mut reader = Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);

    let mut buffer = Vec::new();
    let mut state = XmlParseState::default();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) => {
                let (prefix, local) = split_xml_name(element.name().as_ref())?;
                if state.stack.is_empty() {
                    if state.root_kind.is_some() {
                        return Err("it has more than one root element".to_owned());
                    }
                    state.root_kind =
                        Some(root_kind_from_element(&element, prefix.as_deref(), &local)?);
                } else {
                    handle_start(&mut state, &local);
                }
                state.stack.push(local);
            }
            Ok(Event::Empty(element)) => {
                let (prefix, local) = split_xml_name(element.name().as_ref())?;
                if state.stack.is_empty() {
                    if state.root_kind.is_some() {
                        return Err("it has more than one root element".to_owned());
                    }
                    state.root_kind =
                        Some(root_kind_from_element(&element, prefix.as_deref(), &local)?);
                } else {
                    handle_empty(&mut state, &local);
                }
            }
            Ok(Event::Text(text)) => {
                let value = decode_xml_text(text.as_ref(), true)?;
                if let Some(target) = state.collecting.as_mut() {
                    target.push_str(&value);
                } else if state.stack.is_empty() && !is_ignorable_xml_boundary_text(&value) {
                    return Err("it has text outside the root element".to_owned());
                }
            }
            Ok(Event::CData(text)) => {
                let value = decode_xml_text(text.as_ref(), false)?;
                if let Some(target) = state.collecting.as_mut() {
                    target.push_str(&value);
                } else if state.stack.is_empty() && !is_ignorable_xml_boundary_text(&value) {
                    return Err("it has CDATA outside the root element".to_owned());
                }
            }
            Ok(Event::End(element)) => {
                let (_, local) = split_xml_name(element.name().as_ref())?;
                let Some(expected) = state.stack.last() else {
                    return Err("it closes an XML element that was never opened".to_owned());
                };
                if expected != &local {
                    return Err("an XML closing tag does not match its opening tag".to_owned());
                }

                handle_end(&mut state, &local);
                state.stack.pop();
            }
            Ok(Event::Eof) => {
                if !state.stack.is_empty() {
                    return Err("the XML ends before all elements are closed".to_owned());
                }
                let Some(kind) = state.root_kind else {
                    return Err("it has no XML elements".to_owned());
                };
                return Ok(ParsedSitemapDocument {
                    kind,
                    page_urls: state.page_urls,
                    child_sitemaps: state.child_sitemaps,
                    invalid_page_locations: state.invalid_page_locations,
                    invalid_child_locations: state.invalid_child_locations,
                });
            }
            Ok(Event::Decl(_))
            | Ok(Event::PI(_))
            | Ok(Event::Comment(_))
            | Ok(Event::DocType(_)) => {}
            Ok(Event::GeneralRef(reference)) => {
                let value = decode_xml_general_reference(reference.as_ref())?;
                if let Some(target) = state.collecting.as_mut() {
                    target.push_str(&value);
                } else if state.stack.is_empty() && !is_ignorable_xml_boundary_text(&value) {
                    return Err("it has text outside the root element".to_owned());
                }
            }
            Err(error) => return Err(format!("the XML is broken ({error})")),
        }
        buffer.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocTarget {
    Page(String),
    Child(String),
}

impl LocTarget {
    fn push_str(&mut self, value: &str) {
        match self {
            LocTarget::Page(text) | LocTarget::Child(text) => text.push_str(value),
        }
    }

    fn into_text(self) -> String {
        match self {
            LocTarget::Page(text) | LocTarget::Child(text) => text,
        }
    }
}

fn handle_start(state: &mut XmlParseState, local: &str) {
    let stack = state.stack.clone();
    match (state.root_kind, stack.as_slice(), local) {
        (Some(SitemapRootKind::UrlSet), [root], "url") if root == "urlset" => {
            state.page_entry = Some(EntryState::default());
        }
        (Some(SitemapRootKind::SitemapIndex), [root], "sitemap") if root == "sitemapindex" => {
            state.child_entry = Some(EntryState::default());
        }
        (Some(SitemapRootKind::UrlSet), [root, entry], "loc")
            if root == "urlset" && entry == "url" =>
        {
            state.collecting = Some(LocTarget::Page(String::new()));
        }
        (Some(SitemapRootKind::SitemapIndex), [root, entry], "loc")
            if root == "sitemapindex" && entry == "sitemap" =>
        {
            state.collecting = Some(LocTarget::Child(String::new()));
        }
        _ => {}
    }
}

fn handle_empty(state: &mut XmlParseState, local: &str) {
    let stack = state.stack.clone();
    match (state.root_kind, stack.as_slice(), local) {
        (Some(SitemapRootKind::UrlSet), [root], "url") if root == "urlset" => {
            state.invalid_page_locations += 1;
        }
        (Some(SitemapRootKind::SitemapIndex), [root], "sitemap") if root == "sitemapindex" => {
            state.invalid_child_locations += 1;
        }
        (Some(SitemapRootKind::UrlSet), [root, entry], "loc")
            if root == "urlset" && entry == "url" =>
        {
            set_entry_loc(
                &mut state.page_entry,
                String::new(),
                &mut state.invalid_page_locations,
            );
        }
        (Some(SitemapRootKind::SitemapIndex), [root, entry], "loc")
            if root == "sitemapindex" && entry == "sitemap" =>
        {
            set_entry_loc(
                &mut state.child_entry,
                String::new(),
                &mut state.invalid_child_locations,
            );
        }
        _ => {}
    }
}

fn handle_end(state: &mut XmlParseState, local: &str) {
    let stack = state.stack.clone();
    match (state.root_kind, stack.as_slice(), local) {
        (Some(SitemapRootKind::UrlSet), [root, entry, loc], "loc")
            if root == "urlset" && entry == "url" && loc == "loc" =>
        {
            if let Some(target) = state.collecting.take() {
                set_entry_loc(
                    &mut state.page_entry,
                    target.into_text(),
                    &mut state.invalid_page_locations,
                );
            }
        }
        (Some(SitemapRootKind::SitemapIndex), [root, entry, loc], "loc")
            if root == "sitemapindex" && entry == "sitemap" && loc == "loc" =>
        {
            if let Some(target) = state.collecting.take() {
                set_entry_loc(
                    &mut state.child_entry,
                    target.into_text(),
                    &mut state.invalid_child_locations,
                );
            }
        }
        (Some(SitemapRootKind::UrlSet), [root, entry], "url")
            if root == "urlset" && entry == "url" =>
        {
            finish_page_entry(
                state.page_entry.take(),
                &mut state.page_urls,
                &mut state.invalid_page_locations,
            );
        }
        (Some(SitemapRootKind::SitemapIndex), [root, entry], "sitemap")
            if root == "sitemapindex" && entry == "sitemap" =>
        {
            finish_child_entry(
                state.child_entry.take(),
                &mut state.child_sitemaps,
                &mut state.invalid_child_locations,
            );
        }
        _ => {}
    }
}

fn set_entry_loc(entry: &mut Option<EntryState>, text: String, invalid_locations: &mut usize) {
    match entry {
        Some(entry) if entry.loc.is_none() => entry.loc = Some(text),
        Some(_) => *invalid_locations += 1,
        None => {}
    }
}

fn finish_page_entry(
    entry: Option<EntryState>,
    page_urls: &mut Vec<Url>,
    invalid_page_locations: &mut usize,
) {
    let Some(loc) = entry.and_then(|entry| entry.loc) else {
        *invalid_page_locations += 1;
        return;
    };
    let loc = loc.trim();
    let Ok(url) = Url::parse(loc) else {
        *invalid_page_locations += 1;
        return;
    };
    if !is_http_url(&url) {
        *invalid_page_locations += 1;
        return;
    }
    page_urls.push(url);
}

fn finish_child_entry(
    entry: Option<EntryState>,
    child_sitemaps: &mut BTreeMap<String, Url>,
    invalid_child_locations: &mut usize,
) {
    let Some(loc) = entry.and_then(|entry| entry.loc) else {
        *invalid_child_locations += 1;
        return;
    };
    let loc = loc.trim();
    let Ok(url) = Url::parse(loc) else {
        *invalid_child_locations += 1;
        return;
    };
    if !is_http_url(&url) {
        *invalid_child_locations += 1;
        return;
    }
    child_sitemaps.entry(normalize_url(&url)).or_insert(url);
}

fn root_kind_from_element(
    element: &BytesStart<'_>,
    prefix: Option<&str>,
    local: &str,
) -> Result<SitemapRootKind, String> {
    let kind = match local {
        "urlset" => SitemapRootKind::UrlSet,
        "sitemapindex" => SitemapRootKind::SitemapIndex,
        _ => {
            return Err(format!(
                "its root element is <{local}>, not <urlset> or <sitemapindex>"
            ))
        }
    };

    let namespace = root_namespace(element, prefix)?;
    match (prefix, namespace.as_deref()) {
        (Some(_), Some(STANDARD_SITEMAP_NAMESPACE)) => Ok(kind),
        (Some(_), _) => Err(wrong_namespace()),
        (None, None | Some(STANDARD_SITEMAP_NAMESPACE)) => Ok(kind),
        (None, Some(_)) => Err(wrong_namespace()),
    }
}

fn root_namespace(
    element: &BytesStart<'_>,
    prefix: Option<&str>,
) -> Result<Option<String>, String> {
    let expected_prefixed = prefix.map(|prefix| format!("xmlns:{prefix}"));

    for attribute in element.attributes().with_checks(false) {
        let attribute = attribute
            .map_err(|error| format!("the root element has a broken attribute ({error})"))?;
        let key = attribute.key.as_ref();
        let matches_namespace = match expected_prefixed.as_deref() {
            Some(expected) => key == expected.as_bytes(),
            None => key == b"xmlns",
        };
        if matches_namespace {
            let value = std::str::from_utf8(attribute.value.as_ref())
                .map_err(|_| "the XML namespace is not valid UTF-8".to_owned())?;
            return Ok(Some(value.to_owned()));
        }
    }

    Ok(None)
}

fn split_xml_name(raw: &[u8]) -> Result<(Option<String>, String), String> {
    let name =
        std::str::from_utf8(raw).map_err(|_| "an XML element name is not valid".to_owned())?;
    match name.split_once(':') {
        Some((prefix, local)) if !prefix.is_empty() && !local.is_empty() => {
            Ok((Some(prefix.to_owned()), local.to_owned()))
        }
        Some(_) => Err("an XML element name is not valid".to_owned()),
        None => Ok((None, name.to_owned())),
    }
}

fn decode_xml_text(bytes: &[u8], unescape: bool) -> Result<String, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "the XML is not valid UTF-8".to_owned())?;
    if unescape {
        unescape_xml_entities(text)
    } else {
        Ok(text.to_owned())
    }
}

fn decode_xml_general_reference(bytes: &[u8]) -> Result<String, String> {
    let entity = std::str::from_utf8(bytes).map_err(|_| "an XML entity is not valid".to_owned())?;
    unescape_xml_entities(&format!("&{entity};"))
}

fn is_ignorable_xml_boundary_text(value: &str) -> bool {
    value
        .trim_matches(|character: char| character.is_whitespace() || character == '\u{feff}')
        .is_empty()
}

fn unescape_xml_entities(input: &str) -> Result<String, String> {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0usize;

    while let Some(relative) = input[cursor..].find('&') {
        let start = cursor + relative;
        output.push_str(&input[cursor..start]);

        let Some(end_relative) = input[start..].find(';') else {
            return Err("an XML entity has no closing ;".to_owned());
        };
        let end = start + end_relative;
        let entity = &input[start + 1..end];
        match entity {
            "amp" => output.push('&'),
            "lt" => output.push('<'),
            "gt" => output.push('>'),
            "apos" => output.push('\''),
            "quot" => output.push('"'),
            value if value.starts_with("#x") || value.starts_with("#X") => {
                let codepoint = u32::from_str_radix(&value[2..], 16)
                    .map_err(|_| "a numeric XML entity is not a valid character".to_owned())?;
                let Some(character) = char::from_u32(codepoint) else {
                    return Err("a numeric XML entity is not a valid character".to_owned());
                };
                output.push(character);
            }
            value if value.starts_with('#') => {
                let codepoint = value[1..]
                    .parse::<u32>()
                    .map_err(|_| "a numeric XML entity is not a valid character".to_owned())?;
                let Some(character) = char::from_u32(codepoint) else {
                    return Err("a numeric XML entity is not a valid character".to_owned());
                };
                output.push(character);
            }
            _ => {
                return Err("it uses its own XML entities, which auditroo does not read".to_owned())
            }
        }
        cursor = end + 1;
    }

    output.push_str(&input[cursor..]);
    Ok(output)
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.to_ascii_lowercase() == *right)
    })
}

fn invalid_entry_detail(invalid_pages: usize, invalid_children: usize) -> Option<String> {
    let mut parts = Vec::new();
    if invalid_pages > 0 {
        parts.push(format!(
            "{invalid_pages} page {} without a full URL in <loc>",
            entries(invalid_pages)
        ));
    }
    if invalid_children > 0 {
        parts.push(format!(
            "{invalid_children} child sitemap {} without a full URL in <loc>",
            entries(invalid_children)
        ));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn entries(count: usize) -> &'static str {
    if count == 1 {
        "entry"
    } else {
        "entries"
    }
}

fn wrong_namespace() -> String {
    format!("its root element is not in the sitemap namespace {STANDARD_SITEMAP_NAMESPACE}")
}

fn append_detail(
    mut diagnostic: SitemapDocumentDiagnostic,
    extra: impl AsRef<str>,
) -> SitemapDocumentDiagnostic {
    let detail = match diagnostic.detail {
        Some(existing) => format!("{existing}; {}", extra.as_ref()),
        None => extra.as_ref().to_owned(),
    };
    diagnostic.detail = Some(sanitize_detail(&detail));
    diagnostic
}

fn diagnostic(
    requested_url: String,
    final_url: Option<String>,
    final_status: Option<u16>,
    outcome: SitemapDocumentOutcome,
    detail: Option<String>,
) -> SitemapDocumentDiagnostic {
    SitemapDocumentDiagnostic {
        requested_url,
        final_url,
        final_status,
        outcome,
        detail: detail.map(|detail| sanitize_detail(&detail)),
        entries: 0,
        invalid_entries: 0,
    }
}

fn diagnostic_url(url: &Url) -> String {
    if is_http_url(url) {
        return normalize_url(url);
    }

    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.set_fragment(None);
    redact_url_userinfo(redacted.as_str())
}

fn sanitize_detail(detail: &str) -> String {
    let collapsed = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    redact_url_userinfo(&collapsed)
        .chars()
        .take(DETAIL_LIMIT)
        .collect()
}

fn redact_url_userinfo(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0usize;

    while let Some(relative_scheme_end) = value[cursor..].find("://") {
        let scheme_end = cursor + relative_scheme_end;
        let authority_start = scheme_end + 3;
        let authority_end = value[authority_start..]
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(character, '/' | '?' | '#' | '<' | '>' | '"' | '\'')
            })
            .map_or(value.len(), |relative| authority_start + relative);

        output.push_str(&value[cursor..authority_start]);
        if let Some(relative_at) = value[authority_start..authority_end].find('@') {
            let at = authority_start + relative_at;
            output.push_str("[REDACTED]@");
            output.push_str(&value[at + 1..authority_end]);
        } else {
            output.push_str(&value[authority_start..authority_end]);
        }
        cursor = authority_end;
    }

    output.push_str(&value[cursor..]);
    output
}

fn fetch_error_detail(error: &FetchError) -> String {
    match error {
        FetchError::BodyTooLarge { limit } => {
            format!("the download is larger than {limit} bytes (--max-body-bytes)")
        }
        _ => error.to_string(),
    }
}

fn normalized_observed_url(value: Option<String>) -> Option<String> {
    value.and_then(|value| Url::parse(&value).ok().map(|url| normalize_url(&url)))
}

fn is_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
}
