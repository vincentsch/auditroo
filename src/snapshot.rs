//! The snapshot: everything one crawl saw, stored as stable JSON.
//!
//! A snapshot is the input for `diff`, `check`, `report` and log analysis.
//! Lists and maps are sorted so two crawls of an unchanged site produce the
//! same file apart from `created_at`.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use time::{OffsetDateTime, UtcOffset};
use url::Url;

use crate::assertions::{canonical_custom_extraction, CustomExtraction};
use crate::crawl::{normalize_url, CrawlFetchFailure, FetchError, RobotsFile};
use crate::extract::SeoRecord;
use crate::graph::{orphans as compute_orphans, LinkGraph};
use crate::site::Site;
use crate::sitemap::{compute_sitemap_coverage, SitemapCoverage, SitemapDiscovery};

pub const SNAPSHOT_ARTIFACT_KIND: &str = "snapshot";
pub const SNAPSHOT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub artifact_kind: String,
    pub schema_version: u16,
    pub auditroo_version: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub start_url: String,
    /// The live sites this crawl stands in for, when it crawled a preview or
    /// a local build. URLs on those hosts count as this site's own wherever
    /// they appear.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub production_urls: Vec<String>,
    pub crawl_observation: CrawlObservation,
    /// robots.txt of every origin the crawl visited, keyed by origin such as
    /// `https://www.example.com`.
    pub robots: BTreeMap<String, RobotsFile>,
    pub records: BTreeMap<String, SeoRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_extraction: Option<CustomExtraction>,
    pub sitemap: SitemapCoverage,
    pub orphans: Vec<String>,
    pub link_graph: LinkGraph,
    pub crawl_failures: BTreeMap<String, CrawlFailureSummary>,
}

/// Who took a snapshot and when, kept in reports that point at it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    pub start_url: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub auditroo_version: String,
    pub schema_version: u16,
}

impl SnapshotMetadata {
    pub fn of(snapshot: &Snapshot) -> Self {
        Self {
            start_url: snapshot.start_url.clone(),
            created_at: snapshot.created_at,
            auditroo_version: snapshot.auditroo_version.clone(),
            schema_version: snapshot.schema_version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInput {
    pub start_url: String,
    pub production_urls: Vec<String>,
    pub crawl_observation: CrawlObservation,
    pub robots: BTreeMap<String, RobotsFile>,
    pub records: Vec<SeoRecord>,
    pub sitemap: SitemapDiscovery,
    pub link_graph: LinkGraph,
    pub crawl_failures: Vec<CrawlFetchFailure>,
}

/// The crawl settings that change what a crawl can see, plus what it could
/// not see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CrawlObservation {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_depth: usize,
    pub max_pages: usize,
    pub respect_robots: bool,
    pub render_js: bool,
    pub global_truncation_reasons: Vec<CrawlTruncationReason>,
    pub skipped_candidates: BTreeMap<String, Vec<CrawlSkipReason>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrawlTruncationReason {
    MaxPagesReached,
    MaxDepthReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrawlSkipReason {
    RobotsDenied,
    DepthLimit,
    PageBudget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlFailureSummary {
    pub depth: usize,
    pub kind: CrawlFailureKind,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrawlFailureKind {
    Request,
    Timeout,
    MalformedRedirect,
    BodyTooLarge,
    BackendUnavailable,
    Other,
}

impl CrawlFailureKind {
    /// A short phrase for people, for example "timed out".
    pub fn describe(self) -> &'static str {
        match self {
            Self::Request => "the request failed",
            Self::Timeout => "the request timed out",
            Self::MalformedRedirect => "it redirects to an invalid URL",
            Self::BodyTooLarge => "the page is larger than --max-body-bytes",
            Self::BackendUnavailable => "the browser stopped working",
            Self::Other => "auditroo could not fetch it",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    Io { path: String, detail: String },
    Malformed(String),
    WrongArtifactKind(String),
    UnsupportedSchemaVersion(u64),
    Invalid(String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, detail } => write!(formatter, "{path}: {detail}"),
            Self::Malformed(detail) => write!(formatter, "the snapshot is not valid JSON: {detail}"),
            Self::WrongArtifactKind(kind) => write!(
                formatter,
                "this file is a {kind}, not a snapshot. Create one with `auditroo snapshot <url> --out <file>`"
            ),
            Self::UnsupportedSchemaVersion(version) => write!(
                formatter,
                "this snapshot uses format version {version}, but this auditroo reads version {SNAPSHOT_SCHEMA_VERSION}. Take a new snapshot"
            ),
            Self::Invalid(detail) => write!(formatter, "the snapshot is invalid: {detail}"),
        }
    }
}

impl Error for SnapshotError {}

impl SnapshotError {
    pub fn io(path: &Path, error: &io::Error) -> Self {
        let detail = match error.kind() {
            io::ErrorKind::NotFound => "file not found".to_owned(),
            io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
            _ => error.to_string(),
        };
        Self::Io {
            path: path.display().to_string(),
            detail,
        }
    }
}

/// Build a snapshot from crawl results. Sitemap coverage and orphan pages
/// are worked out here.
pub fn assemble(
    input: SnapshotInput,
    created_at: OffsetDateTime,
    custom_extraction: Option<CustomExtraction>,
) -> Result<Snapshot, SnapshotError> {
    let start_url = normalize_start_url(&input.start_url)?;
    let crawl_observation = canonicalize_observation(input.crawl_observation);
    let records = build_record_map(input.records)?;
    let mut crawl_failures = BTreeMap::new();
    for failure in &input.crawl_failures {
        let (url, summary) = summarize_failure(failure);
        if !records.contains_key(&url) {
            crawl_failures.insert(url, summary);
        }
    }
    let link_graph = input
        .link_graph
        .into_iter()
        .map(|(url, neighbors)| (url, sorted_deduped(neighbors)))
        .collect::<LinkGraph>();
    let mut discovery = input.sitemap;
    discovery.declared_urls = sorted_deduped(discovery.declared_urls);
    discovery
        .documents
        .sort_by(|left, right| left.requested_url.cmp(&right.requested_url));
    let production_urls = sorted_deduped(input.production_urls);
    let site = Site::new(&start_url, &production_urls);
    let sitemap = compute_sitemap_coverage(discovery, records.values(), &site);
    let orphans = compute_orphans(&records, &link_graph, &start_url, &site);
    let custom_extraction = custom_extraction
        .map(canonical_custom_extraction)
        .transpose()
        .map_err(|error| SnapshotError::Invalid(error.to_string()))?;

    let snapshot = Snapshot {
        artifact_kind: SNAPSHOT_ARTIFACT_KIND.to_owned(),
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        auditroo_version: env!("CARGO_PKG_VERSION").to_owned(),
        created_at: whole_second_utc(created_at),
        start_url,
        production_urls,
        crawl_observation,
        robots: input.robots,
        records,
        custom_extraction,
        sitemap,
        orphans,
        link_graph,
        crawl_failures,
    };
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

pub fn summarize_failure(failure: &CrawlFetchFailure) -> (String, CrawlFailureSummary) {
    let url = Url::parse(&failure.url)
        .ok()
        .map(|url| normalize_url(&url))
        .unwrap_or_else(|| failure.url.clone());

    let (kind, detail) = match &failure.error {
        FetchError::Request { detail } => (CrawlFailureKind::Request, Some(detail.clone())),
        FetchError::Timeout => (CrawlFailureKind::Timeout, None),
        FetchError::MalformedRedirect { detail } => {
            (CrawlFailureKind::MalformedRedirect, Some(detail.clone()))
        }
        FetchError::BodyTooLarge { .. } => (CrawlFailureKind::BodyTooLarge, None),
        FetchError::BackendUnavailable { reason } => (
            CrawlFailureKind::BackendUnavailable,
            Some(reason.describe().to_owned()),
        ),
        FetchError::InvalidHeader { .. }
        | FetchError::UserAgentHeaderNotAllowed { .. }
        | FetchError::InvalidUserAgent { .. }
        | FetchError::ClientBuild { .. } => (CrawlFailureKind::Other, None),
    };

    (
        url,
        CrawlFailureSummary {
            depth: failure.depth,
            kind,
            detail: detail.map(|value| bounded_line(&value, 200)),
        },
    )
}

/// Write the snapshot as pretty JSON. The file is replaced in one step, so a
/// failed write never leaves half a snapshot behind.
pub fn write(snapshot: &Snapshot, path: &Path) -> Result<(), SnapshotError> {
    let serialized = format!(
        "{}\n",
        serde_json::to_string_pretty(snapshot)
            .map_err(|error| SnapshotError::Invalid(error.to_string()))?
    );
    write_atomically(path, serialized.as_bytes()).map_err(|error| SnapshotError::io(path, &error))
}

pub fn read(path: &Path) -> Result<Snapshot, SnapshotError> {
    let raw = fs::read_to_string(path).map_err(|error| SnapshotError::io(path, &error))?;
    snapshot_from_str(&raw).map_err(|error| match error {
        SnapshotError::Io { .. } => error,
        other => SnapshotError::Io {
            path: path.display().to_string(),
            detail: other.to_string(),
        },
    })
}

pub fn snapshot_from_str(raw: &str) -> Result<Snapshot, SnapshotError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|error| SnapshotError::Malformed(error.to_string()))?;
    let kind = value
        .get("artifact_kind")
        .and_then(Value::as_str)
        .unwrap_or("JSON file of another kind");
    if kind != SNAPSHOT_ARTIFACT_KIND {
        return Err(SnapshotError::WrongArtifactKind(kind.to_owned()));
    }
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if version != u64::from(SNAPSHOT_SCHEMA_VERSION) {
        return Err(SnapshotError::UnsupportedSchemaVersion(version));
    }
    let snapshot: Snapshot = serde_json::from_value(value)
        .map_err(|error| SnapshotError::Malformed(error.to_string()))?;
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

/// Checks that the rest of the program relies on.
pub(crate) fn validate_snapshot(snapshot: &Snapshot) -> Result<(), SnapshotError> {
    if snapshot.artifact_kind != SNAPSHOT_ARTIFACT_KIND {
        return Err(SnapshotError::WrongArtifactKind(
            snapshot.artifact_kind.clone(),
        ));
    }
    if snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return Err(SnapshotError::UnsupportedSchemaVersion(
            snapshot.schema_version.into(),
        ));
    }
    let start_url = normalize_start_url(&snapshot.start_url)?;
    if start_url != snapshot.start_url {
        return Err(SnapshotError::Invalid(format!(
            "the start URL {} should be written as {start_url}",
            snapshot.start_url
        )));
    }
    if !snapshot.records.contains_key(&snapshot.start_url) && !snapshot.start_blocked_by_robots() {
        return Err(SnapshotError::Invalid(
            "it has no record for the start URL".to_owned(),
        ));
    }
    for (key, record) in &snapshot.records {
        if &record.url != key {
            return Err(SnapshotError::Invalid(format!(
                "the record under {key} is for another URL, {}",
                record.url
            )));
        }
    }
    Ok(())
}

impl Snapshot {
    /// robots.txt blocked auditroo from the start page, so nothing was crawled.
    pub fn start_blocked_by_robots(&self) -> bool {
        self.crawl_observation
            .skipped_candidates
            .get(&self.start_url)
            .is_some_and(|reasons| reasons.contains(&CrawlSkipReason::RobotsDenied))
    }

    /// URLs that robots.txt kept auditroo from, sorted.
    pub fn robots_denied(&self) -> impl Iterator<Item = &str> {
        self.crawl_observation
            .skipped_candidates
            .iter()
            .filter(|(_, reasons)| reasons.contains(&CrawlSkipReason::RobotsDenied))
            .map(|(url, _)| url.as_str())
    }

    /// The crawl stopped early because of --max-pages or --max-depth.
    pub fn truncated(&self) -> bool {
        !self.crawl_observation.global_truncation_reasons.is_empty()
    }

    /// The site: the crawled host, plus production when this crawl stood
    /// in for it.
    pub fn site(&self) -> Site {
        Site::new(&self.start_url, &self.production_urls)
    }

    /// The robots.txt file that applies to `url`, when the crawl loaded it.
    pub fn robots_file(&self, url: &str) -> Option<&RobotsFile> {
        let url = Url::parse(url).ok()?;
        self.robots.get(&crate::crawl::robots::origin_key(&url))
    }

    /// The start origin's robots.txt.
    pub fn start_robots(&self) -> Option<&RobotsFile> {
        self.robots_file(&self.start_url)
    }

    /// HTML pages, without images, PDFs and other files.
    pub fn pages(&self) -> impl Iterator<Item = &SeoRecord> {
        self.records.values().filter(|record| record.is_page())
    }
}

pub(crate) fn parse_json_no_duplicates(raw: &str) -> Result<Value, JsonParseError> {
    serde_json::from_str::<CheckedValue>(raw)
        .map(|checked| checked.0)
        .map_err(|error| {
            let message = error.to_string();
            if let Some(key) = message.strip_prefix("duplicate JSON object key: ") {
                let key = key.split(" at line ").next().unwrap_or(key).to_owned();
                JsonParseError::DuplicateKey(key)
            } else {
                JsonParseError::Malformed(message)
            }
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JsonParseError {
    Malformed(String),
    DuplicateKey(String),
}

fn build_record_map(records: Vec<SeoRecord>) -> Result<BTreeMap<String, SeoRecord>, SnapshotError> {
    let mut output = BTreeMap::new();
    for record in records {
        let key = record.url.clone();
        if output.insert(key.clone(), record).is_some() {
            return Err(SnapshotError::Invalid(format!(
                "the crawl produced two records for {key}"
            )));
        }
    }
    Ok(output)
}

fn canonicalize_observation(mut observation: CrawlObservation) -> CrawlObservation {
    observation.include = sorted_deduped(observation.include);
    observation.exclude = sorted_deduped(observation.exclude);
    observation.global_truncation_reasons = sorted_deduped(observation.global_truncation_reasons);
    observation.skipped_candidates = observation
        .skipped_candidates
        .into_iter()
        .map(|(url, reasons)| (url, sorted_deduped(reasons)))
        .collect();
    observation
}

fn sorted_deduped<T>(items: Vec<T>) -> Vec<T>
where
    T: Ord,
{
    let set: BTreeSet<_> = items.into_iter().collect();
    set.into_iter().collect()
}

fn whole_second_utc(created_at: OffsetDateTime) -> OffsetDateTime {
    created_at
        .to_offset(UtcOffset::UTC)
        .replace_nanosecond(0)
        .expect("zero nanoseconds is valid")
}

fn normalize_start_url(input: &str) -> Result<String, SnapshotError> {
    let url = Url::parse(input)
        .map_err(|_| SnapshotError::Invalid(format!("the start URL {input} is not a URL")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(SnapshotError::Invalid(format!(
            "the start URL {input} is not an http:// or https:// URL"
        )));
    }
    Ok(normalize_url(&url))
}

fn bounded_line(input: &str, limit: usize) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

/// The folder that contains `path`. A bare file name such as
/// `auditroo.toml` lives in the current folder, not in "".
pub fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Replace `path` with `bytes` through a temporary file in the same folder.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = parent_dir(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auditroo");

    let mut attempt = 0u32;
    let (temp_path, mut temp_file) = loop {
        let candidate = parent.join(format!(".{file_name}.tmp-{}-{attempt}", std::process::id()));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 1000 => {
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    };

    let result = (|| {
        temp_file.write_all(bytes)?;
        temp_file.flush()?;
        temp_file.sync_all()?;
        drop(temp_file);
        fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// Check that a file can be created at `path` before a long crawl starts.
pub fn check_writable(path: &Path) -> Result<(), String> {
    let parent = parent_dir(path);
    if !parent.is_dir() {
        return Err(format!(
            "{}: the folder {} does not exist",
            path.display(),
            parent.display()
        ));
    }
    if path.is_dir() {
        return Err(format!("{}: this is a folder, not a file", path.display()));
    }
    Ok(())
}

struct CheckedValue(Value);

impl<'de> Deserialize<'de> for CheckedValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(CheckedValueVisitor)
    }
}

struct CheckedValueVisitor;

impl<'de> Visitor<'de> for CheckedValueVisitor {
    type Value = CheckedValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any valid JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(CheckedValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(CheckedValue(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(CheckedValue(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(CheckedValue)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(CheckedValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(CheckedValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(CheckedValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(CheckedValue(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        CheckedValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<CheckedValue>()? {
            values.push(value.0);
        }
        Ok(CheckedValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!(
                    "duplicate JSON object key: {key}"
                )));
            }
            let value = map.next_value::<CheckedValue>()?;
            object.insert(key, value.0);
        }
        Ok(CheckedValue(Value::Object(object)))
    }
}
