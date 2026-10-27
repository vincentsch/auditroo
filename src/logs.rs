//! Reads access logs. Each line becomes one request with its URL, its
//! status and the bot that sent it. `LogFormat` lists the formats.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};
use url::Url;

use crate::bots::{BotCatalog, BotClassification, ClientType};
use crate::crawl::normalize_url;
use crate::snapshot::{parse_json_no_duplicates, JsonParseError};

pub const MAX_RAW_RECORD_BYTES: usize = 1_048_576;
pub const MAX_SCALAR_FIELD_BYTES: usize = 16_384;
pub const MAX_IMPORT_ERROR_SAMPLES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    Common,
    Combined,
    CloudflareJsonl,
    CloudflareCsv,
    GenericJsonl,
}

impl LogFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Common => "common",
            Self::Combined => "combined",
            Self::CloudflareJsonl => "cloudflare_jsonl",
            Self::CloudflareCsv => "cloudflare_csv",
            Self::GenericJsonl => "generic_jsonl",
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "common" => Ok(Self::Common),
            "combined" => Ok(Self::Combined),
            "cloudflare_jsonl" => Ok(Self::CloudflareJsonl),
            "cloudflare_csv" => Ok(Self::CloudflareCsv),
            "generic_jsonl" => Ok(Self::GenericJsonl),
            _ => Err(
                "use common, combined, cloudflare_jsonl, cloudflare_csv or generic_jsonl"
                    .to_owned(),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenericJsonFieldMap {
    pub timestamp: String,
    pub method: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSourceDefinition {
    pub id: Option<String>,
    pub path: PathBuf,
    pub display_path: String,
    pub format: LogFormat,
    pub origin: String,
    pub field_map: Option<GenericJsonFieldMap>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLogSource {
    pub source_id: String,
    pub path: PathBuf,
    pub display_path: String,
    pub format: LogFormat,
    pub origin: Url,
    pub normalized_origin: String,
    pub field_map: Option<GenericJsonFieldMap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedLogSources {
    pub configured_source_count: usize,
    pub selected_logical_source_count: usize,
    pub matched_file_count: usize,
    pub files: Vec<ResolvedLogSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedLogEntry {
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp_utc: OffsetDateTime,
    pub method: String,
    pub normalized_url: String,
    pub observed_origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_host: Option<String>,
    pub observed_path: String,
    pub observed_raw_query: Option<String>,
    pub raw_request_target: String,
    pub status: u16,
    pub origin_status: Option<u16>,
    pub referer: Option<String>,
    pub bytes: Option<u64>,
    pub cache_status: Option<String>,
    pub raw_user_agent: Option<String>,
    pub source_id: String,
    pub record_number: u64,
    pub matched_rule_id: Option<String>,
    pub bot_family: Option<String>,
    pub client_type: ClientType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportErrorSample {
    pub source_id: String,
    pub record_number: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedTimeRange {
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen_utc: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_utc: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotFamilyCount {
    pub family_id: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogImportSummary {
    pub configured_source_count: usize,
    pub selected_logical_source_count: usize,
    pub matched_file_count: usize,
    pub source_files_read: Vec<String>,
    pub total_records: u64,
    pub blank_records: u64,
    pub parsed_entries: u64,
    pub parse_error_count: u64,
    pub normalization_error_count: u64,
    pub error_samples: Vec<ImportErrorSample>,
    pub time_range: Option<ObservedTimeRange>,
    pub counts_by_client_type: BTreeMap<ClientType, u64>,
    pub bot_family_counts: BTreeMap<String, u64>,
    pub top_bot_families: Vec<BotFamilyCount>,
    pub exact_status_counts: BTreeMap<u16, u64>,
    pub bot_catalog_version: String,
    pub bot_catalog_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogImportError {
    Io(String),
    Source(String),
    InvalidInput(String),
}

impl fmt::Display for LogImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(detail) | Self::Source(detail) | Self::InvalidInput(detail) => {
                formatter.write_str(detail)
            }
        }
    }
}

impl Error for LogImportError {}

impl From<std::io::Error> for LogImportError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(format!("could not read a log file: {error}"))
    }
}

pub fn resolve_log_sources(
    definitions: &[LogSourceDefinition],
) -> Result<ResolvedLogSources, LogImportError> {
    let mut candidates = Vec::new();
    for (index, definition) in definitions.iter().enumerate() {
        validate_source_definition(definition)?;
        let base_id = definition
            .id
            .clone()
            .unwrap_or_else(|| format!("source-{}", index + 1));
        let matches = expand_source_matches(definition)?;
        for (match_index, path) in matches.iter().enumerate() {
            let source_id = if matches.len() > 1 {
                format!("{}#{}", base_id, match_index + 1)
            } else {
                base_id.clone()
            };
            let canonical = fs::canonicalize(path).map_err(|error| {
                LogImportError::Source(format!(
                    "could not open the log file {}: {error}",
                    definition.display_path
                ))
            })?;
            let normalized_origin = normalize_origin(&definition.origin)
                .map_err(|error| LogImportError::Source(error.to_string()))?;
            let origin = Url::parse(&(normalized_origin.clone() + "/"))
                .map_err(|error| LogImportError::Source(error.to_string()))?;
            candidates.push(SourceCandidate {
                source_id,
                path: path.clone(),
                canonical,
                display_path: definition.display_path.clone(),
                format: definition.format,
                normalized_origin,
                origin,
                field_map: definition.field_map.clone(),
            });
        }
    }

    let mut by_canonical: BTreeMap<PathBuf, SourceCandidate> = BTreeMap::new();
    for candidate in candidates {
        match by_canonical.get(&candidate.canonical) {
            Some(existing) if existing.effective_definition_matches(&candidate) => {}
            Some(existing) => {
                return Err(LogImportError::Source(format!(
                    "the log sources {} and {} read the same file with different settings",
                    candidate.source_id, existing.source_id
                )));
            }
            None => {
                by_canonical.insert(candidate.canonical.clone(), candidate);
            }
        }
    }

    let mut files = by_canonical
        .into_values()
        .map(SourceCandidate::into_resolved)
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        (
            left.source_id.as_str(),
            left.display_path.as_str(),
            left.path.as_os_str(),
        )
            .cmp(&(
                right.source_id.as_str(),
                right.display_path.as_str(),
                right.path.as_os_str(),
            ))
    });

    Ok(ResolvedLogSources {
        configured_source_count: definitions.len(),
        selected_logical_source_count: files.len(),
        matched_file_count: files.len(),
        files,
    })
}

pub fn normalize_origin(input: &str) -> Result<String, LogImportError> {
    let url = Url::parse(input).map_err(|error| {
        LogImportError::InvalidInput(format!(
            "the log origin {input:?} is not a full http:// or https:// URL: {error}"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(LogImportError::InvalidInput(
            "the log origin must be a full http:// or https:// URL with a host".to_owned(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(LogImportError::InvalidInput(
            "the log origin must be only the scheme and host, for example https://www.example.com"
                .to_owned(),
        ));
    }
    Ok(origin_from_url(&url))
}

pub fn origin_from_url(url: &Url) -> String {
    let mut out = String::new();
    out.push_str(url.scheme());
    out.push_str("://");
    if let Some(host) = url.host_str() {
        let host = host.to_ascii_lowercase();
        if host.contains(':') && !host.starts_with('[') {
            out.push('[');
            out.push_str(&host);
            out.push(']');
        } else {
            out.push_str(&host);
        }
    }
    if let Some(port) = url.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out
}

pub fn import_log_sources<F>(
    sources: &ResolvedLogSources,
    catalog: &BotCatalog,
    mut consumer: F,
) -> Result<LogImportSummary, LogImportError>
where
    F: FnMut(NormalizedLogEntry),
{
    let mut summary = LogImportSummary::new(sources, catalog);
    for source in &sources.files {
        summary.source_files_read.push(source.source_id.clone());
        match source.format {
            LogFormat::Common | LogFormat::Combined => {
                import_text_access_log(source, catalog, &mut summary, &mut consumer)?
            }
            LogFormat::CloudflareJsonl | LogFormat::GenericJsonl => {
                import_jsonl_log(source, catalog, &mut summary, &mut consumer)?
            }
            LogFormat::CloudflareCsv => {
                import_cloudflare_csv(source, catalog, &mut summary, &mut consumer)?
            }
        }
    }
    summary.finalize();
    Ok(summary)
}

impl LogImportSummary {
    fn new(sources: &ResolvedLogSources, catalog: &BotCatalog) -> Self {
        Self {
            configured_source_count: sources.configured_source_count,
            selected_logical_source_count: sources.selected_logical_source_count,
            matched_file_count: sources.matched_file_count,
            source_files_read: Vec::new(),
            total_records: 0,
            blank_records: 0,
            parsed_entries: 0,
            parse_error_count: 0,
            normalization_error_count: 0,
            error_samples: Vec::new(),
            time_range: None,
            counts_by_client_type: BTreeMap::new(),
            bot_family_counts: BTreeMap::new(),
            top_bot_families: Vec::new(),
            exact_status_counts: BTreeMap::new(),
            bot_catalog_version: catalog.catalog_version.clone(),
            bot_catalog_fingerprint: catalog.effective_fingerprint.clone(),
        }
    }

    fn record_entry(&mut self, entry: &NormalizedLogEntry) {
        self.parsed_entries += 1;
        *self
            .counts_by_client_type
            .entry(entry.client_type)
            .or_insert(0) += 1;
        if let Some(family) = &entry.bot_family {
            *self.bot_family_counts.entry(family.clone()).or_insert(0) += 1;
        }
        *self.exact_status_counts.entry(entry.status).or_insert(0) += 1;
        match &mut self.time_range {
            None => {
                self.time_range = Some(ObservedTimeRange {
                    first_seen_utc: entry.timestamp_utc,
                    last_seen_utc: entry.timestamp_utc,
                });
            }
            Some(range) => {
                if entry.timestamp_utc < range.first_seen_utc {
                    range.first_seen_utc = entry.timestamp_utc;
                }
                if entry.timestamp_utc > range.last_seen_utc {
                    range.last_seen_utc = entry.timestamp_utc;
                }
            }
        }
    }

    fn record_error(
        &mut self,
        source_id: &str,
        record_number: u64,
        kind: RecordErrorKind,
        reason: impl Into<String>,
    ) {
        match kind {
            RecordErrorKind::Parse => self.parse_error_count += 1,
            RecordErrorKind::Normalization => self.normalization_error_count += 1,
        }
        if self.error_samples.len() < MAX_IMPORT_ERROR_SAMPLES {
            self.error_samples.push(ImportErrorSample {
                source_id: source_id.to_owned(),
                record_number,
                reason: sanitize_reason(&reason.into()),
            });
        }
    }

    fn finalize(&mut self) {
        let mut top = self
            .bot_family_counts
            .iter()
            .map(|(family_id, count)| BotFamilyCount {
                family_id: family_id.clone(),
                count: *count,
            })
            .collect::<Vec<_>>();
        top.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.family_id.cmp(&right.family_id))
        });
        top.truncate(20);
        self.top_bot_families = top;
        self.source_files_read.sort();
        self.source_files_read.dedup();
    }
}

#[derive(Debug, Clone)]
struct SourceCandidate {
    source_id: String,
    path: PathBuf,
    canonical: PathBuf,
    display_path: String,
    format: LogFormat,
    normalized_origin: String,
    origin: Url,
    field_map: Option<GenericJsonFieldMap>,
}

impl SourceCandidate {
    fn effective_definition_matches(&self, other: &Self) -> bool {
        self.format == other.format
            && self.normalized_origin == other.normalized_origin
            && self.field_map == other.field_map
    }

    fn into_resolved(self) -> ResolvedLogSource {
        ResolvedLogSource {
            source_id: self.source_id,
            path: self.path,
            display_path: self.display_path,
            format: self.format,
            origin: self.origin,
            normalized_origin: self.normalized_origin,
            field_map: self.field_map,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum RecordErrorKind {
    Parse,
    Normalization,
}

#[derive(Debug, Clone)]
struct RawLogRecord {
    timestamp_utc: OffsetDateTime,
    method: String,
    raw_request_target: String,
    status: u16,
    origin_status: Option<u16>,
    observed_host: Option<String>,
    referer: Option<String>,
    bytes: Option<u64>,
    cache_status: Option<String>,
    raw_user_agent: Option<String>,
}

fn validate_source_definition(definition: &LogSourceDefinition) -> Result<(), LogImportError> {
    normalize_origin(&definition.origin)?;
    match (definition.format, &definition.field_map) {
        (LogFormat::GenericJsonl, Some(map)) => validate_generic_field_map(map)?,
        (LogFormat::GenericJsonl, None) => {
            return Err(LogImportError::Source(
                "a generic_jsonl log source needs a field_map".to_owned(),
            ));
        }
        (_, Some(_)) => {
            return Err(LogImportError::Source(
                "field_map only works with generic_jsonl log sources".to_owned(),
            ));
        }
        (_, None) => {}
    }
    Ok(())
}

pub fn validate_generic_field_map(map: &GenericJsonFieldMap) -> Result<(), LogImportError> {
    if map.url.is_some() == map.path.is_some() {
        return Err(LogImportError::InvalidInput(
            "the generic_jsonl field_map must map exactly one of url and path".to_owned(),
        ));
    }
    let mut names = BTreeSet::new();
    for (label, value) in [
        ("timestamp", Some(&map.timestamp)),
        ("method", Some(&map.method)),
        ("status", Some(&map.status)),
        ("url", map.url.as_ref()),
        ("path", map.path.as_ref()),
        ("user_agent", map.user_agent.as_ref()),
        ("referer", map.referer.as_ref()),
        ("host", map.host.as_ref()),
        ("bytes", map.bytes.as_ref()),
        ("origin_status", map.origin_status.as_ref()),
        ("cache_status", map.cache_status.as_ref()),
    ] {
        if let Some(value) = value {
            validate_scalar(value, &format!("field_map {label}"))?;
            if !names.insert(value.clone()) {
                return Err(LogImportError::InvalidInput(format!(
                    "the generic_jsonl field_map maps two fields to the JSON key {value:?}"
                )));
            }
        }
    }
    Ok(())
}

fn expand_source_matches(definition: &LogSourceDefinition) -> Result<Vec<PathBuf>, LogImportError> {
    let pattern = definition.path.to_string_lossy().to_string();
    let mut paths = Vec::new();
    let globbed = glob::glob(&pattern).map_err(|error| {
        LogImportError::Source(format!("the log path is not a valid glob pattern: {error}"))
    })?;
    for entry in globbed {
        match entry {
            Ok(path) if path.is_file() => paths.push(path),
            Ok(_) => {}
            Err(error) => {
                return Err(LogImportError::Source(format!(
                    "could not read the files that match {}: {error}",
                    definition.display_path
                )));
            }
        }
    }
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Err(LogImportError::Source(format!(
            "no log files match {}",
            definition.display_path
        )));
    }
    Ok(paths)
}

fn import_text_access_log<F>(
    source: &ResolvedLogSource,
    catalog: &BotCatalog,
    summary: &mut LogImportSummary,
    consumer: &mut F,
) -> Result<(), LogImportError>
where
    F: FnMut(NormalizedLogEntry),
{
    let file = fs::File::open(&source.path).map_err(|error| {
        LogImportError::Io(format!(
            "could not open the log source {}: {error}",
            source.source_id
        ))
    })?;
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    let mut record_number = 0u64;
    let mut first = true;

    loop {
        raw.clear();
        let Some(record_exceeded_limit) = read_bounded_line(&mut reader, &mut raw)? else {
            break;
        };
        record_number += 1;
        summary.total_records += 1;
        if record_exceeded_limit {
            summary.record_error(
                &source.source_id,
                record_number,
                RecordErrorKind::Parse,
                "line is longer than 1 MiB",
            );
            continue;
        }
        let mut line = match String::from_utf8(raw.clone()) {
            Ok(line) => line,
            Err(_) => {
                summary.record_error(
                    &source.source_id,
                    record_number,
                    RecordErrorKind::Parse,
                    "line is not valid UTF-8",
                );
                continue;
            }
        };
        strip_line_ending(&mut line);
        if first {
            line = line.strip_prefix('\u{feff}').unwrap_or(&line).to_owned();
            first = false;
        }
        if line.trim().is_empty() {
            summary.blank_records += 1;
            continue;
        }

        match parse_access_log_line(&line, source.format) {
            Ok(record) => {
                consume_raw_record(source, catalog, summary, consumer, record_number, record)
            }
            Err(reason) => summary.record_error(
                &source.source_id,
                record_number,
                RecordErrorKind::Parse,
                reason,
            ),
        }
    }
    Ok(())
}

fn import_jsonl_log<F>(
    source: &ResolvedLogSource,
    catalog: &BotCatalog,
    summary: &mut LogImportSummary,
    consumer: &mut F,
) -> Result<(), LogImportError>
where
    F: FnMut(NormalizedLogEntry),
{
    let file = fs::File::open(&source.path).map_err(|error| {
        LogImportError::Io(format!(
            "could not open the log source {}: {error}",
            source.source_id
        ))
    })?;
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    let mut record_number = 0u64;
    let mut first = true;

    loop {
        raw.clear();
        let Some(record_exceeded_limit) = read_bounded_line(&mut reader, &mut raw)? else {
            break;
        };
        record_number += 1;
        summary.total_records += 1;
        if record_exceeded_limit {
            summary.record_error(
                &source.source_id,
                record_number,
                RecordErrorKind::Parse,
                "line is longer than 1 MiB",
            );
            continue;
        }
        let mut line = match String::from_utf8(raw.clone()) {
            Ok(line) => line,
            Err(_) => {
                summary.record_error(
                    &source.source_id,
                    record_number,
                    RecordErrorKind::Parse,
                    "line is not valid UTF-8",
                );
                continue;
            }
        };
        strip_line_ending(&mut line);
        if first {
            line = line.strip_prefix('\u{feff}').unwrap_or(&line).to_owned();
            first = false;
        }
        if line.trim().is_empty() {
            summary.blank_records += 1;
            continue;
        }

        let parsed = match parse_json_no_duplicates(&line) {
            Ok(Value::Object(object)) => match source.format {
                LogFormat::CloudflareJsonl => parse_cloudflare_object(&object),
                LogFormat::GenericJsonl => parse_generic_object(
                    &object,
                    source
                        .field_map
                        .as_ref()
                        .expect("generic source has a field map"),
                ),
                _ => unreachable!("jsonl import called only for JSONL formats"),
            },
            Ok(_) => Err("line is not a JSON object".to_owned()),
            Err(JsonParseError::DuplicateKey(key)) => {
                Err(format!("the JSON has the key {key:?} twice"))
            }
            Err(JsonParseError::Malformed(detail)) => Err(format!("not valid JSON: {detail}")),
        };

        match parsed {
            Ok(record) => {
                consume_raw_record(source, catalog, summary, consumer, record_number, record)
            }
            Err(reason) => summary.record_error(
                &source.source_id,
                record_number,
                RecordErrorKind::Parse,
                reason,
            ),
        }
    }
    Ok(())
}

fn import_cloudflare_csv<F>(
    source: &ResolvedLogSource,
    catalog: &BotCatalog,
    summary: &mut LogImportSummary,
    consumer: &mut F,
) -> Result<(), LogImportError>
where
    F: FnMut(NormalizedLogEntry),
{
    let file = fs::File::open(&source.path).map_err(|error| {
        LogImportError::Io(format!(
            "could not open the log source {}: {error}",
            source.source_id
        ))
    })?;
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    let Some(headers_exceeded_limit) = read_bounded_csv_record(&mut reader, &mut raw)? else {
        return Err(LogImportError::Source(format!(
            "the CSV log source {} has no header row",
            source.source_id
        )));
    };
    if headers_exceeded_limit {
        return Err(LogImportError::Source(format!(
            "the header row of the CSV log source {} is longer than 1 MiB",
            source.source_id
        )));
    }
    let headers_record = parse_single_csv_record(&raw).map_err(|error| {
        LogImportError::Source(format!(
            "the header row of the CSV log source {} is not valid CSV: {error}",
            source.source_id
        ))
    })?;
    let headers = headers_record
        .iter()
        .enumerate()
        .map(|(index, value)| {
            if index == 0 {
                value.strip_prefix('\u{feff}').unwrap_or(value).to_owned()
            } else {
                value.to_owned()
            }
        })
        .collect::<Vec<_>>();
    validate_csv_headers(&headers)?;
    let header_index = headers
        .iter()
        .enumerate()
        .map(|(index, header)| (header.clone(), index))
        .collect::<BTreeMap<_, _>>();

    let mut record_number = 0u64;
    loop {
        raw.clear();
        let Some(record_exceeded_limit) = read_bounded_csv_record(&mut reader, &mut raw)? else {
            break;
        };
        record_number += 1;
        summary.total_records += 1;
        if record_exceeded_limit {
            summary.record_error(
                &source.source_id,
                record_number,
                RecordErrorKind::Parse,
                "row is longer than 1 MiB",
            );
            continue;
        }
        let record = match parse_single_csv_record(&raw) {
            Ok(record) => record,
            Err(error) => {
                summary.record_error(
                    &source.source_id,
                    record_number,
                    RecordErrorKind::Parse,
                    format!("row is not valid CSV: {error}"),
                );
                continue;
            }
        };
        if record.iter().all(|field| field.trim().is_empty()) {
            summary.blank_records += 1;
            continue;
        }
        match parse_cloudflare_csv_record(&header_index, &record) {
            Ok(raw) => consume_raw_record(source, catalog, summary, consumer, record_number, raw),
            Err(reason) => summary.record_error(
                &source.source_id,
                record_number,
                RecordErrorKind::Parse,
                reason,
            ),
        }
    }
    Ok(())
}

fn consume_raw_record<F>(
    source: &ResolvedLogSource,
    catalog: &BotCatalog,
    summary: &mut LogImportSummary,
    consumer: &mut F,
    record_number: u64,
    raw: RawLogRecord,
) where
    F: FnMut(NormalizedLogEntry),
{
    match normalize_raw_record(source, catalog, record_number, raw) {
        Ok(entry) => {
            summary.record_entry(&entry);
            consumer(entry);
        }
        Err(reason) => summary.record_error(
            &source.source_id,
            record_number,
            RecordErrorKind::Normalization,
            reason,
        ),
    }
}

fn normalize_raw_record(
    source: &ResolvedLogSource,
    catalog: &BotCatalog,
    record_number: u64,
    raw: RawLogRecord,
) -> Result<NormalizedLogEntry, String> {
    let parsed = if raw.raw_request_target.starts_with("http://")
        || raw.raw_request_target.starts_with("https://")
    {
        Url::parse(&raw.raw_request_target)
            .map_err(|error| format!("request URL is invalid: {error}"))?
    } else {
        source
            .origin
            .join(&raw.raw_request_target)
            .map_err(|error| format!("relative request target is invalid: {error}"))?
    };
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("the request URL is not a full http:// or https:// URL".to_owned());
    }

    let normalized_url = normalize_url(&parsed);
    let classification = catalog.classify(raw.raw_user_agent.as_deref());
    let BotClassification {
        matched_rule_id,
        bot_family,
        client_type,
    } = classification;
    Ok(NormalizedLogEntry {
        timestamp_utc: raw.timestamp_utc,
        method: raw.method,
        normalized_url,
        observed_origin: origin_from_url(&parsed),
        observed_host: raw.observed_host,
        observed_path: parsed.path().to_owned(),
        observed_raw_query: parsed.query().map(str::to_owned),
        raw_request_target: raw.raw_request_target,
        status: raw.status,
        origin_status: raw.origin_status,
        referer: raw.referer,
        bytes: raw.bytes,
        cache_status: raw.cache_status,
        raw_user_agent: raw.raw_user_agent,
        source_id: source.source_id.clone(),
        record_number,
        matched_rule_id,
        bot_family,
        client_type,
    })
}

fn parse_access_log_line(line: &str, format: LogFormat) -> Result<RawLogRecord, String> {
    let captures = access_log_regex()
        .captures(line)
        .ok_or_else(|| "line is not in the common or combined log format".to_owned())?;
    let combined_fields_present = captures.get(8).is_some() || captures.get(9).is_some();
    match (format, combined_fields_present) {
        (LogFormat::Common, true) => {
            return Err(
                "line has referer and User-Agent fields, so the source needs the combined format"
                    .to_owned(),
            );
        }
        (LogFormat::Combined, false) => {
            return Err(
                "line has no referer and User-Agent fields, so the source needs the common format"
                    .to_owned(),
            );
        }
        _ => {}
    }

    let request = unescape_quoted(captures.get(5).expect("request capture").as_str())?;
    if request == "-" {
        return Err("line has no request".to_owned());
    }
    let (method, target) = parse_request_line(&request)?;
    let status = parse_status(captures.get(6).expect("status capture").as_str())?;
    let bytes = parse_optional_u64(captures.get(7).expect("bytes capture").as_str(), "bytes")?;
    let referer = captures
        .get(8)
        .map(|value| unescape_quoted(value.as_str()))
        .transpose()?
        .and_then(dash_to_none);
    let raw_user_agent = captures
        .get(9)
        .map(|value| unescape_quoted(value.as_str()))
        .transpose()?
        .and_then(dash_to_none);

    Ok(RawLogRecord {
        timestamp_utc: parse_apache_timestamp(captures.get(4).expect("time capture").as_str())?,
        method,
        raw_request_target: target,
        status,
        origin_status: None,
        observed_host: None,
        referer,
        bytes,
        cache_status: None,
        raw_user_agent,
    })
}

fn access_log_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r#"^(\S+) (\S+) (\S+) \[([^\]]+)\] "((?:\\.|[^"\\])*)" ([0-9]{3}) (\S+)(?: "((?:\\.|[^"\\])*)" "((?:\\.|[^"\\])*)")?$"#,
        )
        .expect("access log regex compiles")
    })
}

fn parse_cloudflare_object(object: &Map<String, Value>) -> Result<RawLogRecord, String> {
    Ok(RawLogRecord {
        timestamp_utc: parse_timestamp_value(required_value(object, "EdgeStartTimestamp")?)?,
        method: parse_method(&required_string(object, "ClientRequestMethod")?)?,
        raw_request_target: required_string(object, "ClientRequestURI")?,
        status: parse_status_value(required_value(object, "EdgeResponseStatus")?)?,
        origin_status: optional_u16_value(
            object.get("OriginResponseStatus"),
            "OriginResponseStatus",
        )?,
        observed_host: optional_string(object.get("ClientRequestHost"), "ClientRequestHost")?,
        referer: optional_string(object.get("ClientRequestReferer"), "ClientRequestReferer")?,
        bytes: optional_u64_value(object.get("EdgeResponseBytes"), "EdgeResponseBytes")?,
        cache_status: optional_string(object.get("CacheCacheStatus"), "CacheCacheStatus")?,
        raw_user_agent: optional_string(
            object.get("ClientRequestUserAgent"),
            "ClientRequestUserAgent",
        )?,
    })
}

fn parse_generic_object(
    object: &Map<String, Value>,
    field_map: &GenericJsonFieldMap,
) -> Result<RawLogRecord, String> {
    let target_key = field_map
        .url
        .as_ref()
        .or(field_map.path.as_ref())
        .expect("field_map has one URL key");
    Ok(RawLogRecord {
        timestamp_utc: parse_timestamp_value(required_value(object, &field_map.timestamp)?)?,
        method: parse_method(&required_string(object, &field_map.method)?)?,
        raw_request_target: required_string(object, target_key)?,
        status: parse_status_value(required_value(object, &field_map.status)?)?,
        origin_status: field_map
            .origin_status
            .as_ref()
            .map(|key| optional_u16_value(object.get(key), key))
            .transpose()?
            .flatten(),
        observed_host: field_map
            .host
            .as_ref()
            .map(|key| optional_string(object.get(key), key))
            .transpose()?
            .flatten(),
        referer: field_map
            .referer
            .as_ref()
            .map(|key| optional_string(object.get(key), key))
            .transpose()?
            .flatten(),
        bytes: field_map
            .bytes
            .as_ref()
            .map(|key| optional_u64_value(object.get(key), key))
            .transpose()?
            .flatten(),
        cache_status: field_map
            .cache_status
            .as_ref()
            .map(|key| optional_string(object.get(key), key))
            .transpose()?
            .flatten(),
        raw_user_agent: field_map
            .user_agent
            .as_ref()
            .map(|key| optional_string(object.get(key), key))
            .transpose()?
            .flatten(),
    })
}

fn parse_cloudflare_csv_record(
    header_index: &BTreeMap<String, usize>,
    record: &csv::StringRecord,
) -> Result<RawLogRecord, String> {
    Ok(RawLogRecord {
        timestamp_utc: parse_timestamp_str(csv_required(
            header_index,
            record,
            "EdgeStartTimestamp",
        )?)?,
        method: parse_method(csv_required(header_index, record, "ClientRequestMethod")?)?,
        raw_request_target: bounded_string(
            csv_required(header_index, record, "ClientRequestURI")?,
            "ClientRequestURI",
        )?,
        status: parse_status(csv_required(header_index, record, "EdgeResponseStatus")?)?,
        origin_status: csv_optional(header_index, record, "OriginResponseStatus")
            .map(parse_status)
            .transpose()?,
        observed_host: csv_optional(header_index, record, "ClientRequestHost")
            .map(|value| bounded_string(value, "ClientRequestHost"))
            .transpose()?
            .and_then(dash_to_none),
        referer: csv_optional(header_index, record, "ClientRequestReferer")
            .map(|value| bounded_string(value, "ClientRequestReferer"))
            .transpose()?
            .and_then(dash_to_none),
        bytes: csv_optional(header_index, record, "EdgeResponseBytes")
            .map(|value| parse_u64(value, "EdgeResponseBytes"))
            .transpose()?,
        cache_status: csv_optional(header_index, record, "CacheCacheStatus")
            .map(|value| bounded_string(value, "CacheCacheStatus"))
            .transpose()?
            .and_then(dash_to_none),
        raw_user_agent: csv_optional(header_index, record, "ClientRequestUserAgent")
            .map(|value| bounded_string(value, "ClientRequestUserAgent"))
            .transpose()?
            .and_then(dash_to_none),
    })
}

fn validate_csv_headers(headers: &[String]) -> Result<(), LogImportError> {
    let mut seen = BTreeSet::new();
    for header in headers {
        validate_scalar(header, "CSV header")?;
        if !seen.insert(header.clone()) {
            return Err(LogImportError::Source(format!(
                "the CSV header {header:?} appears twice"
            )));
        }
    }
    for required in [
        "EdgeStartTimestamp",
        "ClientRequestMethod",
        "ClientRequestURI",
        "EdgeResponseStatus",
    ] {
        if !seen.contains(required) {
            return Err(LogImportError::Source(format!(
                "the CSV logs have no {required} column"
            )));
        }
    }
    Ok(())
}

fn required_value<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a Value, String> {
    object
        .get(key)
        .ok_or_else(|| format!("missing required field {key}"))
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    match required_value(object, key)? {
        Value::String(value) => bounded_string(value, key),
        _ => Err(format!("{key} must be a string")),
    }
}

fn optional_string(value: Option<&Value>, key: &str) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => bounded_string(value, key).map(dash_to_none),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn parse_timestamp_value(value: &Value) -> Result<OffsetDateTime, String> {
    match value {
        Value::String(value) => parse_timestamp_str(value),
        Value::Number(number) if number.is_u64() => parse_decimal_timestamp(&number.to_string()),
        _ => Err("timestamp must be an RFC 3339 date or a Unix timestamp".to_owned()),
    }
}

fn parse_timestamp_str(value: &str) -> Result<OffsetDateTime, String> {
    validate_scalar(value, "timestamp").map_err(|error| error.to_string())?;
    if value.chars().all(|ch| ch.is_ascii_digit()) {
        parse_decimal_timestamp(value)
    } else {
        OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
            .map(|value| value.to_offset(UtcOffset::UTC))
            .map_err(|error| format!("timestamp is not RFC3339: {error}"))
    }
}

fn parse_decimal_timestamp(value: &str) -> Result<OffsetDateTime, String> {
    let (divisor, multiplier) = match value.len() {
        10 => (1u64, 0u64),
        13 => (1_000, 1_000_000),
        16 => (1_000_000, 1_000),
        19 => (1_000_000_000, 1),
        _ => {
            return Err("a Unix timestamp must have 10, 13, 16 or 19 digits".to_owned());
        }
    };
    let parsed = value
        .parse::<u64>()
        .map_err(|_| "the Unix timestamp is too large".to_owned())?;
    let seconds = if divisor == 1 {
        parsed
    } else {
        parsed / divisor
    };
    let nanos = if divisor == 1 {
        0
    } else {
        (parsed % divisor) * multiplier
    };
    OffsetDateTime::from_unix_timestamp(
        seconds
            .try_into()
            .map_err(|_| "the Unix timestamp is too large".to_owned())?,
    )
    .map(|value| value + Duration::nanoseconds(nanos as i64))
    .map(|value| value.to_offset(UtcOffset::UTC))
    .map_err(|error| format!("timestamp is outside the supported range: {error}"))
}

fn parse_apache_timestamp(value: &str) -> Result<OffsetDateTime, String> {
    let (date_part, offset_part) = value
        .rsplit_once(' ')
        .ok_or_else(|| "access log timestamp must include timezone offset".to_owned())?;
    let pieces = date_part.split(['/', ':']).collect::<Vec<_>>();
    if pieces.len() != 6 {
        return Err("access log timestamp has invalid shape".to_owned());
    }
    let day = pieces[0]
        .parse::<u8>()
        .map_err(|_| "timestamp day is invalid".to_owned())?;
    let month = match pieces[1] {
        "Jan" => Month::January,
        "Feb" => Month::February,
        "Mar" => Month::March,
        "Apr" => Month::April,
        "May" => Month::May,
        "Jun" => Month::June,
        "Jul" => Month::July,
        "Aug" => Month::August,
        "Sep" => Month::September,
        "Oct" => Month::October,
        "Nov" => Month::November,
        "Dec" => Month::December,
        _ => return Err("timestamp month is invalid".to_owned()),
    };
    let year = pieces[2]
        .parse::<i32>()
        .map_err(|_| "timestamp year is invalid".to_owned())?;
    let hour = pieces[3]
        .parse::<u8>()
        .map_err(|_| "timestamp hour is invalid".to_owned())?;
    let minute = pieces[4]
        .parse::<u8>()
        .map_err(|_| "timestamp minute is invalid".to_owned())?;
    let second = pieces[5]
        .parse::<u8>()
        .map_err(|_| "timestamp second is invalid".to_owned())?;
    if offset_part.len() != 5 || !matches!(&offset_part[..1], "+" | "-") {
        return Err("timestamp timezone offset must be +/-HHMM".to_owned());
    }
    let offset_hour = offset_part[1..3]
        .parse::<i8>()
        .map_err(|_| "timestamp offset hour is invalid".to_owned())?;
    let offset_minute = offset_part[3..5]
        .parse::<i8>()
        .map_err(|_| "timestamp offset minute is invalid".to_owned())?;
    let sign = if offset_part.starts_with('-') { -1 } else { 1 };
    let offset = UtcOffset::from_hms(sign * offset_hour, sign * offset_minute, 0)
        .map_err(|error| format!("timestamp offset is invalid: {error}"))?;
    let date = Date::from_calendar_date(year, month, day)
        .map_err(|error| format!("timestamp date is invalid: {error}"))?;
    let time = Time::from_hms(hour, minute, second)
        .map_err(|error| format!("timestamp time is invalid: {error}"))?;
    Ok(PrimitiveDateTime::new(date, time)
        .assume_offset(offset)
        .to_offset(UtcOffset::UTC))
}

fn parse_request_line(request: &str) -> Result<(String, String), String> {
    let mut pieces = request.split_ascii_whitespace();
    let method = pieces
        .next()
        .ok_or_else(|| "request line is missing method".to_owned())?;
    let target = pieces
        .next()
        .ok_or_else(|| "request line is missing request target".to_owned())?;
    let method = parse_method(method)?;
    let target = bounded_string(target, "request target")?;
    if let Some(version) = pieces.next() {
        if !version.starts_with("HTTP/") {
            return Err("the request line has an invalid HTTP version".to_owned());
        }
    }
    if pieces.next().is_some() {
        return Err("request line has too many fields".to_owned());
    }
    Ok((method, target))
}

fn parse_method(input: &str) -> Result<String, String> {
    validate_scalar(input, "method").map_err(|error| error.to_string())?;
    if input.is_empty() || !input.bytes().all(is_http_token_byte) {
        return Err("method is not a valid HTTP method".to_owned());
    }
    Ok(input.to_ascii_uppercase())
}

fn is_http_token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
            | b'0'..=b'9'
            | b'a'..=b'z'
            | b'A'..=b'Z'
    )
}

fn parse_status_value(value: &Value) -> Result<u16, String> {
    match value {
        Value::Number(number) if number.is_u64() => parse_status(&number.to_string()),
        Value::String(value) => parse_status(value),
        _ => Err("status must be a number".to_owned()),
    }
}

fn optional_u16_value(value: Option<&Value>, key: &str) -> Result<Option<u16>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => parse_status_value(value)
            .map(Some)
            .map_err(|error| format!("{key}: {error}")),
    }
}

fn parse_status(input: &str) -> Result<u16, String> {
    validate_scalar(input, "status").map_err(|error| error.to_string())?;
    if !input.chars().all(|ch| ch.is_ascii_digit()) {
        return Err("status must be a number".to_owned());
    }
    let status = input
        .parse::<u16>()
        .map_err(|_| "status is too large".to_owned())?;
    if !(100..=599).contains(&status) {
        return Err("status must be between 100 and 599".to_owned());
    }
    Ok(status)
}

fn optional_u64_value(value: Option<&Value>, key: &str) -> Result<Option<u64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) if number.is_u64() => Ok(number.as_u64()),
        Some(Value::String(value)) => parse_u64(value, key).map(Some),
        Some(_) => Err(format!("{key} must be a number")),
    }
}

fn parse_optional_u64(input: &str, label: &str) -> Result<Option<u64>, String> {
    if input == "-" {
        Ok(None)
    } else {
        parse_u64(input, label).map(Some)
    }
}

fn parse_u64(input: &str, label: &str) -> Result<u64, String> {
    validate_scalar(input, label).map_err(|error| error.to_string())?;
    if input.is_empty() || !input.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("{label} must be a number"));
    }
    input
        .parse::<u64>()
        .map_err(|_| format!("{label} is too large"))
}

fn csv_required<'a>(
    header_index: &BTreeMap<String, usize>,
    record: &'a csv::StringRecord,
    key: &str,
) -> Result<&'a str, String> {
    let index = header_index
        .get(key)
        .ok_or_else(|| format!("the CSV logs have no {key} column"))?;
    record
        .get(*index)
        .ok_or_else(|| format!("row has no {key} field"))
}

fn csv_optional<'a>(
    header_index: &BTreeMap<String, usize>,
    record: &'a csv::StringRecord,
    key: &str,
) -> Option<&'a str> {
    header_index
        .get(key)
        .and_then(|index| record.get(*index))
        .filter(|value| !value.is_empty())
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> Result<Option<bool>, LogImportError> {
    let bytes = {
        let mut limited = reader.take((MAX_RAW_RECORD_BYTES + 1) as u64);
        limited.read_until(b'\n', buffer)?
    };
    if bytes == 0 {
        return Ok(None);
    }
    if buffer.len() > MAX_RAW_RECORD_BYTES {
        if !buffer.ends_with(b"\n") {
            discard_until_line_end(reader)?;
        }
        return Ok(Some(true));
    }
    Ok(Some(false))
}

fn read_bounded_csv_record<R: BufRead>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> Result<Option<bool>, LogImportError> {
    let mut in_quotes = false;
    let mut at_field_start = true;
    let mut after_quote = false;
    let mut exceeded_limit = false;
    let mut saw_byte = false;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(saw_byte.then_some(exceeded_limit));
        }

        let mut consumed = 0usize;
        for &byte in available {
            consumed += 1;
            saw_byte = true;
            if !exceeded_limit {
                buffer.push(byte);
                if buffer.len() > MAX_RAW_RECORD_BYTES {
                    exceeded_limit = true;
                    buffer.clear();
                }
            }

            let record_ended = if in_quotes {
                if after_quote {
                    match byte {
                        b'"' => {
                            after_quote = false;
                            false
                        }
                        b',' => {
                            in_quotes = false;
                            after_quote = false;
                            at_field_start = true;
                            false
                        }
                        b'\n' => {
                            in_quotes = false;
                            after_quote = false;
                            true
                        }
                        _ => {
                            in_quotes = false;
                            after_quote = false;
                            at_field_start = false;
                            false
                        }
                    }
                } else if byte == b'"' {
                    after_quote = true;
                    false
                } else {
                    false
                }
            } else {
                match byte {
                    b'"' if at_field_start => {
                        in_quotes = true;
                        at_field_start = false;
                        false
                    }
                    b',' => {
                        at_field_start = true;
                        false
                    }
                    b'\n' => true,
                    _ => {
                        at_field_start = false;
                        false
                    }
                }
            };

            if record_ended {
                reader.consume(consumed);
                return Ok(Some(exceeded_limit));
            }
        }
        reader.consume(consumed);
    }
}

fn discard_until_line_end<R: BufRead>(reader: &mut R) -> Result<(), LogImportError> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        if let Some(index) = available.iter().position(|byte| *byte == b'\n') {
            reader.consume(index + 1);
            return Ok(());
        }
        let len = available.len();
        reader.consume(len);
    }
}

fn parse_single_csv_record(raw: &[u8]) -> Result<csv::StringRecord, csv::Error> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(raw);
    let mut record = csv::StringRecord::new();
    reader.read_record(&mut record)?;
    Ok(record)
}

fn bounded_string(input: &str, label: &str) -> Result<String, String> {
    validate_scalar(input, label).map_err(|error| error.to_string())?;
    Ok(input.to_owned())
}

fn validate_scalar(input: &str, label: &str) -> Result<(), LogImportError> {
    if input.len() > MAX_SCALAR_FIELD_BYTES {
        return Err(LogImportError::InvalidInput(format!(
            "{label} is longer than 16 KiB"
        )));
    }
    Ok(())
}

fn dash_to_none(input: String) -> Option<String> {
    (input != "-").then_some(input)
}

fn unescape_quoted(input: &str) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => return Err("a quoted field ends with a lone backslash".to_owned()),
        }
    }
    bounded_string(&out, "quoted field")
}

fn strip_line_ending(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

fn sanitize_reason(input: &str) -> String {
    let mut value = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() > 240 {
        value = value.chars().take(240).collect();
    }
    value
}

pub fn source_path_collides_with(path: &Path, sources: &ResolvedLogSources) -> bool {
    let Ok(output) = canonical_or_display_path(path) else {
        return false;
    };
    sources.files.iter().any(|source| {
        canonical_or_display_path(&source.path)
            .map(|candidate| candidate == output)
            .unwrap_or(false)
    })
}

pub fn canonical_or_display_path(path: &Path) -> Result<PathBuf, LogImportError> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(_) => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let parent = fs::canonicalize(parent).map_err(|error| {
                LogImportError::Io(format!(
                    "could not find the folder of {}: {error}",
                    path.display()
                ))
            })?;
            Ok(parent.join(path.file_name().unwrap_or_default()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bots::BotCatalog;

    #[test]
    fn parses_combined_rows_and_discards_source_ip() {
        let record = parse_access_log_line(
            r#"203.0.113.1 - - [10/Oct/2000:13:55:36 -0700] "GET /docs?q=1 HTTP/1.1" 200 123 "https://ref.example/" "Googlebot/2.1""#,
            LogFormat::Combined,
        )
        .expect("row parses");
        assert_eq!(record.method, "GET");
        assert_eq!(record.raw_request_target, "/docs?q=1");
        assert_eq!(record.status, 200);
        assert_eq!(record.bytes, Some(123));
        assert_eq!(record.raw_user_agent.as_deref(), Some("Googlebot/2.1"));
        assert_eq!(
            record.timestamp_utc.to_string(),
            "2000-10-10 20:55:36.0 +00:00:00"
        );
    }

    #[test]
    fn reconstructs_relative_urls_and_classifies_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log_path = dir.path().join("access.log");
        fs::write(
            &log_path,
            r#"203.0.113.1 - - [10/Oct/2000:13:55:36 -0700] "GET /docs?q=1 HTTP/1.1" 200 123 "-" "GPTBot/1.0""#,
        )
        .expect("log writes");
        let sources = resolve_log_sources(&[LogSourceDefinition {
            id: Some("edge".to_owned()),
            path: log_path.clone(),
            display_path: log_path.display().to_string(),
            format: LogFormat::Combined,
            origin: "https://example.test".to_owned(),
            field_map: None,
        }])
        .expect("sources resolve");
        let mut entries = Vec::new();
        let summary = import_log_sources(&sources, &BotCatalog::builtin(), |entry| {
            entries.push(entry);
        })
        .expect("import succeeds");

        assert_eq!(summary.parsed_entries, 1);
        assert_eq!(entries[0].normalized_url, "https://example.test/docs?q=1");
        assert_eq!(entries[0].client_type, ClientType::AiBot);
        assert_eq!(entries[0].bot_family.as_deref(), Some("gptbot"));
    }
}
