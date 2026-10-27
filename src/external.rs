//! Traffic data. auditroo reads Search Console and analytics CSV exports and
//! shows the traffic of each URL next to its findings. The numbers never
//! change a severity or whether a run fails.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{Date, Month};
use url::Url;

use crate::crawl::normalize_url;
use crate::logs::{normalize_origin, origin_from_url};
use crate::snapshot::Snapshot;

pub const MAX_IMPACT_SOURCE_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_IMPACT_TOTAL_BYTES: usize = 500 * 1024 * 1024;
pub const MAX_IMPACT_SOURCE_ROWS: u64 = 1_000_000;
pub const MAX_IMPACT_TOTAL_ROWS: u64 = 2_000_000;
pub const MAX_IMPACT_SOURCE_URLS: usize = 250_000;
pub const MAX_IMPACT_TOTAL_CONTEXTS: usize = 500_000;
pub const MAX_IMPACT_SOURCES: usize = 20;
pub const MAX_IMPACT_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_IMPACT_CELL_BYTES: usize = 16 * 1024;
pub const MAX_IMPACT_WARNING_SAMPLES: usize = 20;
pub const MAX_IMPACT_WARNING_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactSourceKind {
    SearchConsoleCsv,
    AnalyticsCsv,
}

impl ImpactSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SearchConsoleCsv => "search_console_csv",
            Self::AnalyticsCsv => "analytics_csv",
        }
    }
}

impl fmt::Display for ImpactSourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ImpactSourceKind {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "search_console_csv" => Ok(Self::SearchConsoleCsv),
            "analytics_csv" => Ok(Self::AnalyticsCsv),
            _ => Err(format!(
                "unknown impact source kind {input:?}. Use search_console_csv or analytics_csv"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactSort {
    Stable,
    SeverityThenImpact,
}

impl ImpactSort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::SeverityThenImpact => "severity_then_impact",
        }
    }
}

impl FromStr for ImpactSort {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "stable" => Ok(Self::Stable),
            "severity_then_impact" => Ok(Self::SeverityThenImpact),
            _ => Err(format!(
                "unknown impact sort {input:?}. Use stable or severity_then_impact"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactSourceState {
    Succeeded,
    SkippedOptional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpactJoinKind {
    Exact,
    OriginRebased,
    Canonical,
    UrlGroup,
}

impl ImpactJoinKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::OriginRebased => "origin_rebased",
            Self::Canonical => "canonical",
            Self::UrlGroup => "url_group",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactPrimaryMetric {
    pub source: String,
    pub metric: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImpactSettings {
    pub enabled: bool,
    pub sort: ImpactSort,
    pub primary: Option<ImpactPrimaryMetric>,
    pub sources: Vec<ResolvedImpactSource>,
}

impl Default for ResolvedImpactSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            sort: ImpactSort::Stable,
            primary: None,
            sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImpactSource {
    pub id: String,
    pub kind: ImpactSourceKind,
    pub path: PathBuf,
    pub display_path: String,
    pub required: bool,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub origin: Option<String>,
    pub currency: Option<String>,
    pub field_map: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImpactReport {
    pub enabled: bool,
    pub sort: ImpactSort,
    pub primary: Option<ImpactPrimaryMetric>,
    pub source_summaries: Vec<ImpactSourceSummary>,
    pub import_counts: ImpactImportCounts,
    pub warnings: Vec<String>,
    pub url_contexts: Vec<ExternalUrlContext>,
}

impl ImpactReport {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            sort: ImpactSort::Stable,
            primary: None,
            source_summaries: Vec::new(),
            import_counts: ImpactImportCounts::default(),
            warnings: Vec::new(),
            url_contexts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactImportCounts {
    pub sources: usize,
    pub url_contexts: usize,
    pub annotations: usize,
    pub unmatched_contexts: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImpactSourceSummary {
    pub source_id: String,
    pub kind: ImpactSourceKind,
    pub required: bool,
    pub state: ImpactSourceState,
    pub payload_bytes: Option<u64>,
    pub payload_sha256: Option<String>,
    pub rows: u64,
    pub usable_rows: u64,
    pub unique_urls: usize,
    pub duplicate_aggregate_count: u64,
    pub invalid_rows: u64,
    pub warning_count: usize,
    pub warnings: Vec<String>,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub currency: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalUrlContext {
    pub context_id: String,
    pub source_id: String,
    pub kind: ImpactSourceKind,
    pub normalized_url: String,
    pub metrics: BTreeMap<String, f64>,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub currency: Option<String>,
    pub provenance: String,
    pub raw_url_samples: Vec<String>,
    pub raw_url_omitted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactAnnotation {
    pub context_id: String,
    pub source_id: String,
    pub join_kind: ImpactJoinKind,
    pub target: String,
    pub matched_url: String,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactSourceError {
    pub source_id: String,
    pub source_kind: ImpactSourceKind,
    pub code: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImpactError {
    Source(ImpactSourceError),
    InvalidReport(String),
    Diff(String),
}

impl fmt::Display for ImpactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(
                formatter,
                "could not use the traffic data source {} ({}): {} ({})",
                error.source_id, error.source_kind, error.reason, error.code
            ),
            Self::InvalidReport(detail) | Self::Diff(detail) => write!(
                formatter,
                "could not match the traffic data to the findings: {detail}"
            ),
        }
    }
}

impl Error for ImpactError {}

struct ImportedImpactSource {
    summary: ImpactSourceSummary,
    contexts: Vec<ExternalUrlContext>,
}

struct FailedImpactSource {
    error: ImpactSourceError,
    summary: Box<ImpactSourceSummary>,
}

pub fn import_impact(settings: &ResolvedImpactSettings) -> Result<ImpactReport, ImpactError> {
    if !settings.enabled {
        return Ok(ImpactReport::disabled());
    }

    let mut sources = settings.sources.iter().collect::<Vec<_>>();
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    let mut source_summaries = Vec::new();
    let mut contexts = Vec::new();
    let mut total_bytes = 0usize;
    let mut total_rows = 0u64;
    for source in sources {
        match import_source(source) {
            Ok(mut imported) => {
                charge_import_totals(&mut total_bytes, &mut total_rows, &imported.summary, source)?;
                source_summaries.push(imported.summary);
                contexts.append(&mut imported.contexts);
                if contexts.len() > MAX_IMPACT_TOTAL_CONTEXTS {
                    return Err(ImpactError::Source(source_error(
                        source,
                        "total_contexts_too_large",
                        "the traffic data has more than 500,000 URLs in total, the limit for one run",
                    )));
                }
            }
            Err(failed) => {
                charge_import_totals(
                    &mut total_bytes,
                    &mut total_rows,
                    failed.summary.as_ref(),
                    source,
                )?;
                if source.required {
                    return Err(ImpactError::Source(failed.error));
                }
                source_summaries.push(*failed.summary);
            }
        }
    }
    contexts.sort_by(|left, right| {
        (left.source_id.as_str(), left.normalized_url.as_str())
            .cmp(&(right.source_id.as_str(), right.normalized_url.as_str()))
    });

    Ok(ImpactReport {
        enabled: true,
        sort: settings.sort,
        primary: settings.primary.clone(),
        import_counts: ImpactImportCounts {
            sources: source_summaries.len(),
            url_contexts: contexts.len(),
            annotations: 0,
            unmatched_contexts: contexts.len(),
        },
        source_summaries,
        warnings: Vec::new(),
        url_contexts: contexts,
    })
}

fn charge_import_totals(
    total_bytes: &mut usize,
    total_rows: &mut u64,
    summary: &ImpactSourceSummary,
    source: &ResolvedImpactSource,
) -> Result<(), ImpactError> {
    *total_bytes = total_bytes.saturating_add(summary.payload_bytes.unwrap_or(0) as usize);
    *total_rows = total_rows.saturating_add(summary.rows);
    if *total_bytes > MAX_IMPACT_TOTAL_BYTES {
        return Err(ImpactError::Source(source_error(
            source,
            "total_payload_too_large",
            "the traffic data files are larger than 500 MiB in total, the limit for one run",
        )));
    }
    if *total_rows > MAX_IMPACT_TOTAL_ROWS {
        return Err(ImpactError::Source(source_error(
            source,
            "total_rows_too_large",
            "the traffic data files have more than 2,000,000 rows in total, the limit for one run",
        )));
    }
    Ok(())
}

/// Import the configured Search Console or analytics exports and attach
/// their numbers to the findings of `report`. Impact data never changes a
/// finding's severity or the gate.
pub fn apply_impact(
    report: &mut crate::report::Report,
    baseline: &Snapshot,
    current: &Snapshot,
    settings: &ResolvedImpactSettings,
) -> Result<(), ImpactError> {
    let mut impact = import_impact(settings)?;
    if !impact.enabled {
        return Ok(());
    }

    let indexes = ImpactIndexes::new(&impact, baseline, current)?;
    let mut matched_contexts = BTreeSet::new();
    let mut annotation_count = 0usize;
    let mut warnings = Vec::new();
    for finding in &mut report.findings {
        let targets = match &finding.url {
            Some(url) => vec![url.clone()],
            None => finding.related.clone(),
        };
        finding.impact = annotations_for_targets(
            &targets,
            &indexes,
            baseline,
            current,
            &mut matched_contexts,
            &mut warnings,
        )?;
        annotation_count += finding.impact.len();
    }
    warnings.truncate(MAX_IMPACT_WARNING_SAMPLES);
    impact.import_counts.annotations = annotation_count;
    impact.import_counts.unmatched_contexts = impact
        .url_contexts
        .len()
        .saturating_sub(matched_contexts.len());
    impact.warnings = warnings;
    report.impact = Some(impact);
    Ok(())
}

pub fn annotate_log_finding_targets(
    impact: Option<&ImpactReport>,
    targets: &[String],
    baseline_start_url: &str,
    current_start_url: &str,
    group_only: bool,
) -> Vec<ImpactAnnotation> {
    let Some(impact) = impact.filter(|impact| impact.enabled) else {
        return Vec::new();
    };
    let mut exact = BTreeMap::<String, Vec<usize>>::new();
    let mut identity = BTreeMap::<String, Vec<usize>>::new();
    for (index, context) in impact.url_contexts.iter().enumerate() {
        exact
            .entry(context.normalized_url.clone())
            .or_default()
            .push(index);
        for start in [baseline_start_url, current_start_url] {
            if let Ok(id) = origin_rebased_identity(start, &context.normalized_url) {
                identity.entry(id).or_default().push(index);
            }
        }
    }
    for values in identity.values_mut() {
        values.sort_unstable();
        values.dedup();
    }
    let exact_kind = if group_only {
        ImpactJoinKind::UrlGroup
    } else {
        ImpactJoinKind::Exact
    };
    let rebased_kind = if group_only {
        ImpactJoinKind::UrlGroup
    } else {
        ImpactJoinKind::OriginRebased
    };
    let mut annotations = Vec::new();
    let mut seen = BTreeSet::new();
    for target in targets {
        let target = match normalize_target(target) {
            Ok(target) => target,
            Err(_) => continue,
        };
        let mut target_annotations = Vec::new();
        let mut resolved_sources = BTreeSet::new();
        let exact_annotations = log_annotations_for_matches(
            impact,
            exact.get(&target),
            &target,
            exact_kind,
            &resolved_sources,
        );
        add_tier_annotations(
            &mut target_annotations,
            &mut resolved_sources,
            exact_annotations,
        );

        let mut identity_matches = Vec::new();
        for start in [current_start_url, baseline_start_url] {
            if let Ok(id) = origin_rebased_identity(start, &target) {
                if let Some(matches) = identity.get(&id) {
                    identity_matches.extend(matches.iter().copied());
                }
            }
        }
        identity_matches.sort_unstable();
        identity_matches.dedup();
        let rebased_annotations = log_annotations_for_matches(
            impact,
            Some(&identity_matches),
            &target,
            rebased_kind,
            &resolved_sources,
        );
        add_tier_annotations(
            &mut target_annotations,
            &mut resolved_sources,
            rebased_annotations,
        );

        for annotation in target_annotations {
            let key = (annotation.context_id.clone(), annotation.target.clone());
            if seen.insert(key) {
                annotations.push(annotation);
            }
        }
    }
    annotations.sort_by(annotation_sort);
    annotations
}

fn log_annotations_for_matches(
    impact: &ImpactReport,
    matches: Option<&Vec<usize>>,
    target: &str,
    join_kind: ImpactJoinKind,
    resolved_sources: &BTreeSet<String>,
) -> TierAnnotations {
    let Some(matches) = matches else {
        return TierAnnotations::default();
    };
    let mut by_source = BTreeMap::<&str, Vec<usize>>::new();
    for index in matches {
        let context = &impact.url_contexts[*index];
        if resolved_sources.contains(context.source_id.as_str()) {
            continue;
        }
        by_source
            .entry(context.source_id.as_str())
            .or_default()
            .push(*index);
    }
    let mut output = TierAnnotations::default();
    for (source_id, source_matches) in by_source {
        match source_matches.as_slice() {
            [index] => {
                let context = &impact.url_contexts[*index];
                output.annotations.push(ImpactAnnotation {
                    context_id: context.context_id.clone(),
                    source_id: context.source_id.clone(),
                    join_kind,
                    target: target.to_owned(),
                    matched_url: context.normalized_url.clone(),
                    provenance: "external_context".to_owned(),
                });
            }
            [] => {}
            _ => {
                output.blocked_sources.insert(source_id.to_owned());
            }
        }
    }
    output
}

pub fn parse_impact_date(input: &str) -> Result<String, String> {
    let parts = input.split('-').collect::<Vec<_>>();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return Err("period_start and period_end must be dates in the form YYYY-MM-DD".to_owned());
    }
    let year = parts[0]
        .parse::<i32>()
        .map_err(|_| "the period date has an invalid year".to_owned())?;
    let month_number = parts[1]
        .parse::<u8>()
        .map_err(|_| "the period date has an invalid month".to_owned())?;
    let day = parts[2]
        .parse::<u8>()
        .map_err(|_| "the period date has an invalid day".to_owned())?;
    let month = Month::try_from(month_number)
        .map_err(|_| "the period date has an invalid month".to_owned())?;
    Date::from_calendar_date(year, month, day)
        .map_err(|_| "the period date does not exist".to_owned())?;
    Ok(input.to_owned())
}

pub fn is_additive_impact_metric(metric: &str) -> bool {
    matches!(
        metric,
        "clicks" | "impressions" | "sessions" | "pageviews" | "conversions" | "revenue"
    )
}

pub fn is_supported_impact_metric(kind: ImpactSourceKind, metric: &str) -> bool {
    match kind {
        ImpactSourceKind::SearchConsoleCsv => {
            matches!(metric, "clicks" | "impressions" | "ctr" | "position")
        }
        ImpactSourceKind::AnalyticsCsv => {
            matches!(metric, "sessions" | "pageviews" | "conversions" | "revenue")
        }
    }
}

fn import_source(
    source: &ResolvedImpactSource,
) -> Result<ImportedImpactSource, FailedImpactSource> {
    let metadata = fs::metadata(&source.path).map_err(|_| {
        failed_import(
            source,
            source_error(
                source,
                "source_unavailable",
                "the CSV file could not be read",
            ),
            ImportStats::default(),
        )
    })?;
    if metadata.len() > MAX_IMPACT_SOURCE_BYTES as u64 {
        return Err(failed_import(
            source,
            source_error(
                source,
                "payload_too_large",
                "the CSV file is larger than 100 MiB, the limit for one source",
            ),
            ImportStats::default(),
        ));
    }
    let bytes = fs::read(&source.path).map_err(|_| {
        failed_import(
            source,
            source_error(
                source,
                "source_unavailable",
                "the CSV file could not be read",
            ),
            ImportStats::default(),
        )
    })?;
    if bytes.len() > MAX_IMPACT_SOURCE_BYTES {
        return Err(failed_import(
            source,
            source_error(
                source,
                "payload_too_large",
                "the CSV file is larger than 100 MiB, the limit for one source",
            ),
            ImportStats {
                payload_bytes: Some(bytes.len() as u64),
                ..ImportStats::default()
            },
        ));
    }
    let payload_sha256 = sha256_hex(&bytes);
    let payload_bytes = Some(bytes.len() as u64);
    let payload_sha256 = Some(payload_sha256);
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(bytes.as_slice());
    let headers = reader
        .headers()
        .map_err(|_| {
            failed_import(
                source,
                source_error(
                    source,
                    "invalid_csv",
                    "the CSV header row could not be read",
                ),
                ImportStats {
                    payload_bytes,
                    payload_sha256: payload_sha256.clone(),
                    ..ImportStats::default()
                },
            )
        })?
        .clone();
    let headers = normalized_headers(source, &headers).map_err(|error| {
        failed_import(
            source,
            error,
            ImportStats {
                payload_bytes,
                payload_sha256: payload_sha256.clone(),
                ..ImportStats::default()
            },
        )
    })?;
    let mapping = resolve_column_mapping(source, &headers).map_err(|error| {
        failed_import(
            source,
            error,
            ImportStats {
                payload_bytes,
                payload_sha256: payload_sha256.clone(),
                ..ImportStats::default()
            },
        )
    })?;

    let mut warnings = Vec::new();
    let mut rows = 0u64;
    let mut usable_rows = 0u64;
    let mut invalid_rows = 0u64;
    let mut aggregates: BTreeMap<String, UrlAggregate> = BTreeMap::new();
    for result in reader.records() {
        rows += 1;
        if rows > MAX_IMPACT_SOURCE_ROWS {
            return Err(failed_import(
                source,
                source_error(
                    source,
                    "rows_too_large",
                    "the CSV file has more than 1,000,000 rows, the limit for one source",
                ),
                ImportStats {
                    payload_bytes,
                    payload_sha256: payload_sha256.clone(),
                    rows,
                    usable_rows,
                    unique_urls: aggregates.len(),
                    duplicate_aggregate_count: usable_rows.saturating_sub(aggregates.len() as u64),
                    invalid_rows,
                    warning_count: warnings.len(),
                    warnings,
                },
            ));
        }
        let record = match result {
            Ok(record) => record,
            Err(_) => {
                invalid_rows += 1;
                push_warning(&mut warnings, format!("row {rows}: not valid CSV"));
                continue;
            }
        };
        if record.iter().any(|cell| cell.len() > MAX_IMPACT_CELL_BYTES) {
            invalid_rows += 1;
            push_warning(
                &mut warnings,
                format!("row {rows}: a cell is longer than 16 KiB"),
            );
            continue;
        }
        let record_bytes = record.iter().map(str::len).sum::<usize>() + record.len();
        if record_bytes > MAX_IMPACT_RECORD_BYTES {
            invalid_rows += 1;
            push_warning(
                &mut warnings,
                format!("row {rows}: the row is longer than 64 KiB"),
            );
            continue;
        }
        match parse_record(source, &mapping, &record, rows, &mut warnings) {
            Some(row) => {
                usable_rows += 1;
                let aggregate = aggregates.entry(row.normalized_url.clone()).or_default();
                aggregate.observe(row);
                if aggregates.len() > MAX_IMPACT_SOURCE_URLS {
                    return Err(failed_import(
                        source,
                        source_error(
                            source,
                            "unique_urls_too_large",
                            "the source has more than 250,000 different URLs, the limit for one source",
                        ),
                        ImportStats {
                            payload_bytes,
                            payload_sha256: payload_sha256.clone(),
                            rows,
                            usable_rows,
                            unique_urls: aggregates.len(),
                            duplicate_aggregate_count: usable_rows
                                .saturating_sub(aggregates.len() as u64),
                            invalid_rows,
                            warning_count: warnings.len(),
                            warnings,
                        },
                    ));
                }
            }
            None => invalid_rows += 1,
        }
    }
    if usable_rows == 0 {
        return Err(failed_import(
            source,
            source_error(
                source,
                "no_usable_rows",
                "the file has no rows that auditroo can use",
            ),
            ImportStats {
                payload_bytes,
                payload_sha256: payload_sha256.clone(),
                rows,
                usable_rows,
                unique_urls: aggregates.len(),
                duplicate_aggregate_count: 0,
                invalid_rows,
                warning_count: warnings.len(),
                warnings,
            },
        ));
    }
    let unique_urls = aggregates.len();
    let duplicate_aggregate_count = usable_rows.saturating_sub(unique_urls as u64);
    let contexts = aggregates
        .into_iter()
        .map(|(normalized_url, aggregate)| aggregate.into_context(source, normalized_url))
        .collect::<Vec<_>>();
    let warning_count = warnings.len();
    warnings.truncate(MAX_IMPACT_WARNING_SAMPLES);
    Ok(ImportedImpactSource {
        summary: impact_source_summary(
            source,
            ImpactSourceState::Succeeded,
            ImportStats {
                payload_bytes,
                payload_sha256,
                rows,
                usable_rows,
                unique_urls,
                duplicate_aggregate_count,
                invalid_rows,
                warning_count,
                warnings,
            },
        ),
        contexts,
    })
}

#[derive(Default)]
struct ImportStats {
    payload_bytes: Option<u64>,
    payload_sha256: Option<String>,
    rows: u64,
    usable_rows: u64,
    unique_urls: usize,
    duplicate_aggregate_count: u64,
    invalid_rows: u64,
    warning_count: usize,
    warnings: Vec<String>,
}

fn failed_import(
    source: &ResolvedImpactSource,
    error: ImpactSourceError,
    mut stats: ImportStats,
) -> FailedImpactSource {
    let mut warnings = Vec::new();
    push_warning(&mut warnings, format!("{}: {}", error.code, error.reason));
    warnings.extend(stats.warnings);
    stats.warning_count = warnings.len();
    warnings.truncate(MAX_IMPACT_WARNING_SAMPLES);
    stats.warnings = warnings;
    FailedImpactSource {
        error,
        summary: Box::new(impact_source_summary(
            source,
            ImpactSourceState::SkippedOptional,
            stats,
        )),
    }
}

fn impact_source_summary(
    source: &ResolvedImpactSource,
    state: ImpactSourceState,
    stats: ImportStats,
) -> ImpactSourceSummary {
    ImpactSourceSummary {
        source_id: source.id.clone(),
        kind: source.kind,
        required: source.required,
        state,
        payload_bytes: stats.payload_bytes,
        payload_sha256: stats.payload_sha256,
        rows: stats.rows,
        usable_rows: stats.usable_rows,
        unique_urls: stats.unique_urls,
        duplicate_aggregate_count: stats.duplicate_aggregate_count,
        invalid_rows: stats.invalid_rows,
        warning_count: stats.warning_count,
        warnings: stats.warnings,
        period_start: source.period_start.clone(),
        period_end: source.period_end.clone(),
        currency: source.currency.clone(),
    }
}

#[derive(Debug, Clone)]
struct ColumnMapping {
    url: usize,
    metrics: BTreeMap<String, usize>,
    required_metrics: BTreeSet<String>,
}

fn normalized_headers(
    source: &ResolvedImpactSource,
    headers: &csv::StringRecord,
) -> Result<Vec<String>, ImpactSourceError> {
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for (index, header) in headers.iter().enumerate() {
        let header = if index == 0 {
            header.trim_start_matches('\u{feff}')
        } else {
            header
        };
        if !seen.insert(header.to_owned()) {
            return Err(source_error(
                source,
                "duplicate_header",
                "a column name appears twice in the CSV header row",
            ));
        }
        output.push(header.to_owned());
    }
    Ok(output)
}

fn resolve_column_mapping(
    source: &ResolvedImpactSource,
    headers: &[String],
) -> Result<ColumnMapping, ImpactSourceError> {
    let field_map = if source.field_map.is_empty() {
        default_field_map(source, headers)?
    } else {
        explicit_field_map(source)?
    };
    let mut used_columns = BTreeSet::new();
    let mut url = None;
    let mut metrics = BTreeMap::new();
    for (field, column) in field_map {
        let index = headers
            .iter()
            .position(|header| header == &column)
            .ok_or_else(|| {
                source_error(
                    source,
                    "missing_header",
                    format!("the CSV has no column {column:?}"),
                )
            })?;
        if !used_columns.insert(column.clone()) {
            return Err(source_error(
                source,
                "duplicate_field_column",
                "two fields in field_map use the same CSV column",
            ));
        }
        if field == "url" {
            url = Some(index);
        } else {
            metrics.insert(field, index);
        }
    }
    let url =
        url.ok_or_else(|| source_error(source, "missing_header", "the CSV has no URL column"))?;
    if metrics.is_empty() {
        return Err(source_error(
            source,
            "missing_metric",
            "field_map must name at least one metric column",
        ));
    }
    let required_metrics = required_row_metrics(source.kind, &metrics);
    Ok(ColumnMapping {
        url,
        metrics,
        required_metrics,
    })
}

fn required_row_metrics(
    kind: ImpactSourceKind,
    metrics: &BTreeMap<String, usize>,
) -> BTreeSet<String> {
    match kind {
        ImpactSourceKind::SearchConsoleCsv => ["clicks", "impressions"]
            .into_iter()
            .filter(|metric| metrics.contains_key(*metric))
            .map(str::to_owned)
            .collect(),
        ImpactSourceKind::AnalyticsCsv => BTreeSet::new(),
    }
}

fn default_field_map(
    source: &ResolvedImpactSource,
    headers: &[String],
) -> Result<BTreeMap<String, String>, ImpactSourceError> {
    if source.kind != ImpactSourceKind::SearchConsoleCsv {
        return Err(source_error(
            source,
            "missing_field_map",
            "an analytics_csv source needs a field_map",
        ));
    }
    let aliases: [(&str, &[&str]); 5] = [
        ("url", &["page", "url", "Page", "URL", "Top pages"]),
        ("clicks", &["clicks", "Clicks"]),
        ("impressions", &["impressions", "Impressions"]),
        ("ctr", &["ctr", "CTR"]),
        ("position", &["position", "Position", "Average position"]),
    ];
    let mut output = BTreeMap::new();
    for (field, field_aliases) in aliases {
        let matches = field_aliases
            .iter()
            .filter(|alias| headers.iter().any(|header| header == **alias))
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(ImpactSourceError {
                source_id: source.id.clone(),
                source_kind: source.kind,
                code: "ambiguous_header".to_owned(),
                reason: format!("more than one column could be the Search Console {field} column"),
            });
        }
        if let Some(alias) = matches.first() {
            output.insert(field.to_owned(), (*alias).to_string());
        } else if matches!(field, "url" | "clicks" | "impressions") {
            return Err(ImpactSourceError {
                source_id: source.id.clone(),
                source_kind: source.kind,
                code: "missing_header".to_owned(),
                reason: format!("the Search Console export has no {field} column"),
            });
        }
    }
    Ok(output)
}

fn explicit_field_map(
    source: &ResolvedImpactSource,
) -> Result<BTreeMap<String, String>, ImpactSourceError> {
    for field in source.field_map.keys() {
        let allowed = match source.kind {
            ImpactSourceKind::SearchConsoleCsv => {
                matches!(
                    field.as_str(),
                    "url" | "clicks" | "impressions" | "ctr" | "position"
                )
            }
            ImpactSourceKind::AnalyticsCsv => matches!(
                field.as_str(),
                "url" | "sessions" | "pageviews" | "conversions" | "revenue"
            ),
        };
        if !allowed {
            return Err(source_error(
                source,
                "unknown_field_map_key",
                format!("unknown impact field_map key {field:?}"),
            ));
        }
    }
    if !source.field_map.contains_key("url") {
        return Err(source_error(
            source,
            "missing_field_map",
            "field_map needs a url column",
        ));
    }
    if source
        .field_map
        .keys()
        .filter(|key| key.as_str() != "url")
        .count()
        == 0
    {
        return Err(source_error(
            source,
            "missing_metric",
            "field_map must name at least one metric column",
        ));
    }
    Ok(source.field_map.clone())
}

#[derive(Debug, Clone)]
struct ImpactRow {
    normalized_url: String,
    raw_url: String,
    metrics: BTreeMap<String, f64>,
}

fn parse_record(
    source: &ResolvedImpactSource,
    mapping: &ColumnMapping,
    record: &csv::StringRecord,
    record_number: u64,
    warnings: &mut Vec<String>,
) -> Option<ImpactRow> {
    let raw_url = record.get(mapping.url).unwrap_or("").trim().to_owned();
    let normalized_url = match normalize_row_url(source, &raw_url) {
        Ok(url) => url,
        Err(reason) => {
            push_warning(warnings, format!("row {record_number}: {reason}"));
            return None;
        }
    };
    let mut metrics = BTreeMap::new();
    for (metric, index) in &mapping.metrics {
        let raw = record.get(*index).unwrap_or("").trim();
        if raw.is_empty() {
            if mapping.required_metrics.contains(metric) {
                push_warning(
                    warnings,
                    format!("row {record_number}: the {metric} value is missing"),
                );
                return None;
            }
            continue;
        }
        match parse_metric_value(metric, raw) {
            Ok(value) => {
                metrics.insert(metric.clone(), value);
            }
            Err(reason) if mapping.required_metrics.contains(metric) => {
                push_warning(
                    warnings,
                    format!("row {record_number}: the {metric} value is not valid: {reason}"),
                );
                return None;
            }
            Err(reason) => {
                push_warning(
                    warnings,
                    format!("row {record_number}: metric {metric} ignored: {reason}"),
                );
            }
        }
    }
    if metrics.is_empty() {
        push_warning(
            warnings,
            format!("row {record_number}: no metric value that auditroo can use"),
        );
        return None;
    }
    Some(ImpactRow {
        normalized_url,
        raw_url,
        metrics,
    })
}

fn normalize_row_url(source: &ResolvedImpactSource, raw: &str) -> Result<String, String> {
    let url = match Url::parse(raw) {
        Ok(url) => url,
        Err(_) if raw.starts_with('/') => {
            let origin = source
                .origin
                .as_ref()
                .ok_or_else(|| "the URL is relative, and the source has no origin".to_owned())?;
            Url::parse(&(origin.clone() + "/"))
                .map_err(|_| "source origin is invalid".to_owned())?
                .join(raw)
                .map_err(|_| {
                    "the relative URL could not be added to the source origin".to_owned()
                })?
        }
        Err(error) => return Err(format!("not a valid URL: {error}")),
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("the URL must be a full http:// or https:// URL".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the URL must not contain a user name or password".to_owned());
    }
    if source.kind == ImpactSourceKind::AnalyticsCsv {
        let declared = source
            .origin
            .as_ref()
            .ok_or_else(|| "an analytics_csv source needs an origin".to_owned())?;
        let observed = origin_from_url(&url);
        if &observed != declared {
            return Err("the URL is not on the origin of the source".to_owned());
        }
    }
    Ok(normalize_url(&url))
}

fn parse_metric_value(metric: &str, raw: &str) -> Result<f64, String> {
    let value = if metric == "ctr" {
        if let Some(percent) = raw.strip_suffix('%') {
            parse_finite(percent.trim())? / 100.0
        } else {
            parse_finite(raw)?
        }
    } else {
        parse_finite(raw)?
    };
    match metric {
        "ctr" if !(0.0..=1.0).contains(&value) => Err("CTR must be between 0 and 1".to_owned()),
        "position" if value <= 0.0 => Err("position must be positive".to_owned()),
        "revenue" => Ok(value),
        "clicks" | "impressions" | "sessions" | "pageviews" | "conversions" if value < 0.0 => {
            Err("the value must not be negative".to_owned())
        }
        _ => Ok(value),
    }
}

fn parse_finite(raw: &str) -> Result<f64, String> {
    let value = raw
        .parse::<f64>()
        .map_err(|_| "the value must be a number".to_owned())?;
    if !value.is_finite() {
        return Err("the value must be a finite number".to_owned());
    }
    Ok(value)
}

#[derive(Default)]
struct UrlAggregate {
    metrics: BTreeMap<String, f64>,
    ctr_values: Vec<f64>,
    position_values: Vec<f64>,
    position_weighted_numerator: f64,
    position_weighted_denominator: f64,
    raw_url_samples: Vec<String>,
    raw_url_omitted: usize,
}

impl UrlAggregate {
    fn observe(&mut self, row: ImpactRow) {
        if self.raw_url_samples.len() < 5 {
            self.raw_url_samples.push(row.raw_url);
        } else {
            self.raw_url_omitted += 1;
        }
        let impressions = row.metrics.get("impressions").copied().unwrap_or(0.0);
        for (metric, value) in row.metrics {
            match metric.as_str() {
                "ctr" => self.ctr_values.push(value),
                "position" => {
                    self.position_values.push(value);
                    if impressions > 0.0 {
                        self.position_weighted_numerator += value * impressions;
                        self.position_weighted_denominator += impressions;
                    }
                }
                _ => *self.metrics.entry(metric).or_insert(0.0) += value,
            }
        }
    }

    fn into_context(
        self,
        source: &ResolvedImpactSource,
        normalized_url: String,
    ) -> ExternalUrlContext {
        let mut metrics = self.metrics;
        let derived_ctr = match (
            metrics.get("clicks").copied(),
            metrics.get("impressions").copied(),
        ) {
            (Some(clicks), Some(impressions)) if impressions > 0.0 => Some(clicks / impressions),
            _ if !self.ctr_values.is_empty() => Some(mean(&self.ctr_values)),
            _ => None,
        };
        if let Some(ctr) = derived_ctr {
            metrics.insert("ctr".to_owned(), ctr);
        }
        if !self.position_values.is_empty() {
            let position = if self.position_weighted_denominator > 0.0 {
                self.position_weighted_numerator / self.position_weighted_denominator
            } else {
                mean(&self.position_values)
            };
            metrics.insert("position".to_owned(), position);
        }
        ExternalUrlContext {
            context_id: context_id(&source.id, &normalized_url),
            source_id: source.id.clone(),
            kind: source.kind,
            normalized_url,
            metrics,
            period_start: source.period_start.clone(),
            period_end: source.period_end.clone(),
            currency: source.currency.clone(),
            provenance: "external_context".to_owned(),
            raw_url_samples: self.raw_url_samples,
            raw_url_omitted: self.raw_url_omitted,
        }
    }
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

struct ImpactIndexes<'a> {
    impact: &'a ImpactReport,
    exact: BTreeMap<String, Vec<usize>>,
    identity: BTreeMap<String, Vec<usize>>,
    baseline_records: BTreeMap<String, &'a crate::extract::SeoRecord>,
    current_records: BTreeMap<String, &'a crate::extract::SeoRecord>,
}

impl<'a> ImpactIndexes<'a> {
    fn new(
        impact: &'a ImpactReport,
        baseline: &'a Snapshot,
        current: &'a Snapshot,
    ) -> Result<Self, ImpactError> {
        let mut exact: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut identity: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, context) in impact.url_contexts.iter().enumerate() {
            exact
                .entry(context.normalized_url.clone())
                .or_default()
                .push(index);
            for start in [&baseline.start_url, &current.start_url] {
                let id = origin_rebased_identity(start, &context.normalized_url)?;
                identity.entry(id).or_default().push(index);
            }
        }
        for values in identity.values_mut() {
            values.sort_unstable();
            values.dedup();
        }
        Ok(Self {
            impact,
            exact,
            identity,
            baseline_records: record_identity_index(baseline)?,
            current_records: record_identity_index(current)?,
        })
    }
}

fn annotations_for_targets(
    targets: &[String],
    indexes: &ImpactIndexes<'_>,
    baseline: &Snapshot,
    current: &Snapshot,
    matched_contexts: &mut BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> Result<Vec<ImpactAnnotation>, ImpactError> {
    let mut annotations = Vec::new();
    let mut seen = BTreeSet::new();
    for target in targets.iter().collect::<BTreeSet<_>>() {
        for annotation in annotations_for_target(target, indexes, baseline, current, warnings)? {
            let key = (annotation.context_id.clone(), annotation.target.clone());
            if seen.insert(key) {
                matched_contexts.insert(annotation.context_id.clone());
                annotations.push(annotation);
            }
        }
    }
    annotations.sort_by(annotation_sort);
    Ok(annotations)
}

fn annotations_for_target(
    target: &str,
    indexes: &ImpactIndexes<'_>,
    baseline: &Snapshot,
    current: &Snapshot,
    warnings: &mut Vec<String>,
) -> Result<Vec<ImpactAnnotation>, ImpactError> {
    let target = normalize_target(target)?;
    let mut annotations = Vec::new();
    let mut resolved_sources = BTreeSet::new();
    let exact_annotations = annotations_for_matches(
        indexes.exact.get(&target),
        indexes,
        ImpactJoinKind::Exact,
        &target,
        warnings,
        &resolved_sources,
    );
    add_tier_annotations(&mut annotations, &mut resolved_sources, exact_annotations);

    let mut identity_matches = Vec::new();
    for start in [&current.start_url, &baseline.start_url] {
        let id = origin_rebased_identity(start, &target)?;
        if let Some(matches) = indexes.identity.get(&id) {
            identity_matches.extend(matches.iter().copied());
        }
    }
    identity_matches.sort_unstable();
    identity_matches.dedup();
    let rebased_annotations = annotations_for_matches(
        Some(&identity_matches),
        indexes,
        ImpactJoinKind::OriginRebased,
        &target,
        warnings,
        &resolved_sources,
    );
    add_tier_annotations(&mut annotations, &mut resolved_sources, rebased_annotations);

    if let Some(canonical) = canonical_fallback_target(&target, indexes, baseline, current)? {
        let canonical_exact = annotations_for_matches(
            indexes.exact.get(&canonical),
            indexes,
            ImpactJoinKind::Canonical,
            &target,
            warnings,
            &resolved_sources,
        );
        add_tier_annotations(&mut annotations, &mut resolved_sources, canonical_exact);

        let mut canonical_matches = Vec::new();
        for start in [&current.start_url, &baseline.start_url] {
            let id = origin_rebased_identity(start, &canonical)?;
            if let Some(matches) = indexes.identity.get(&id) {
                canonical_matches.extend(matches.iter().copied());
            }
        }
        canonical_matches.sort_unstable();
        canonical_matches.dedup();
        let canonical_rebased = annotations_for_matches(
            Some(&canonical_matches),
            indexes,
            ImpactJoinKind::Canonical,
            &target,
            warnings,
            &resolved_sources,
        );
        add_tier_annotations(&mut annotations, &mut resolved_sources, canonical_rebased);
    }
    Ok(annotations)
}

#[derive(Default)]
struct TierAnnotations {
    annotations: Vec<ImpactAnnotation>,
    blocked_sources: BTreeSet<String>,
}

fn add_tier_annotations(
    output: &mut Vec<ImpactAnnotation>,
    resolved_sources: &mut BTreeSet<String>,
    tier: TierAnnotations,
) {
    for annotation in tier.annotations {
        if resolved_sources.insert(annotation.source_id.clone()) {
            output.push(annotation);
        }
    }
    resolved_sources.extend(tier.blocked_sources);
}

fn annotations_for_matches(
    matches: Option<&Vec<usize>>,
    indexes: &ImpactIndexes<'_>,
    join_kind: ImpactJoinKind,
    target: &str,
    warnings: &mut Vec<String>,
    resolved_sources: &BTreeSet<String>,
) -> TierAnnotations {
    let Some(matches) = matches else {
        return TierAnnotations::default();
    };
    let mut by_source: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for index in matches {
        let context = &indexes.impact.url_contexts[*index];
        if resolved_sources.contains(context.source_id.as_str()) {
            continue;
        }
        by_source
            .entry(context.source_id.as_str())
            .or_default()
            .push(*index);
    }
    let mut output = TierAnnotations::default();
    for (source_id, source_matches) in by_source {
        match source_matches.as_slice() {
            [] => {}
            [index] => output
                .annotations
                .push(annotation(indexes, *index, join_kind, target)),
            _ => {
                output.blocked_sources.insert(source_id.to_owned());
                push_warning(
                    warnings,
                    format!(
                        "more than one row of source {} matches {} ({} match), so none was used",
                        source_id,
                        target,
                        join_kind.as_str()
                    ),
                );
            }
        }
    }
    output
}

fn annotation(
    indexes: &ImpactIndexes<'_>,
    index: usize,
    join_kind: ImpactJoinKind,
    target: &str,
) -> ImpactAnnotation {
    let context = &indexes.impact.url_contexts[index];
    ImpactAnnotation {
        context_id: context.context_id.clone(),
        source_id: context.source_id.clone(),
        join_kind,
        target: target.to_owned(),
        matched_url: context.normalized_url.clone(),
        provenance: "external_context".to_owned(),
    }
}

fn canonical_fallback_target(
    target: &str,
    indexes: &ImpactIndexes<'_>,
    baseline: &Snapshot,
    current: &Snapshot,
) -> Result<Option<String>, ImpactError> {
    let baseline_record = find_record(target, &indexes.baseline_records, baseline)?;
    let current_record = find_record(target, &indexes.current_records, current)?;
    let canonical = match (
        baseline_record.and_then(|record| record.canonical.as_deref()),
        current_record.and_then(|record| record.canonical.as_deref()),
    ) {
        (None, None) => return Ok(None),
        (Some(left), Some(right)) => {
            let left_id = pair_identity(left, baseline, current)?;
            let right_id = pair_identity(right, baseline, current)?;
            if left_id != right_id {
                return Ok(None);
            }
            left
        }
        (Some(value), None) | (None, Some(value)) => value,
    };
    let parsed = Url::parse(canonical).map_err(|_| {
        ImpactError::InvalidReport("canonical fallback URL is not a valid URL".to_owned())
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Ok(None);
    }
    if !url_in_site_boundary(canonical, baseline, current)? {
        return Ok(None);
    }
    Ok(Some(normalize_url(&parsed)))
}

fn find_record<'a>(
    target: &str,
    records: &'a BTreeMap<String, &'a crate::extract::SeoRecord>,
    snapshot: &Snapshot,
) -> Result<Option<&'a crate::extract::SeoRecord>, ImpactError> {
    let id = origin_rebased_identity(&snapshot.start_url, target)?;
    Ok(records.get(&id).copied())
}

fn record_identity_index(
    snapshot: &Snapshot,
) -> Result<BTreeMap<String, &crate::extract::SeoRecord>, ImpactError> {
    let mut output = BTreeMap::new();
    for record in snapshot.records.values() {
        let id = origin_rebased_identity(&snapshot.start_url, &record.url)?;
        if output.insert(id, record).is_some() {
            return Err(ImpactError::Diff(
                "two pages of the snapshot have the same path".to_owned(),
            ));
        }
    }
    Ok(output)
}

fn pair_identity(
    raw_url: &str,
    baseline: &Snapshot,
    current: &Snapshot,
) -> Result<String, ImpactError> {
    let current_id = origin_rebased_identity(&current.start_url, raw_url)?;
    if current_id.starts_with("site:") {
        return Ok(current_id);
    }
    origin_rebased_identity(&baseline.start_url, raw_url)
}

fn url_in_site_boundary(
    raw_url: &str,
    baseline: &Snapshot,
    current: &Snapshot,
) -> Result<bool, ImpactError> {
    let current_id = origin_rebased_identity(&current.start_url, raw_url)?;
    let baseline_id = origin_rebased_identity(&baseline.start_url, raw_url)?;
    Ok(current_id.starts_with("site:") || baseline_id.starts_with("site:"))
}

fn normalize_target(target: &str) -> Result<String, ImpactError> {
    let url = Url::parse(target).map_err(|error| {
        ImpactError::InvalidReport(format!("a finding has an invalid URL: {error}"))
    })?;
    Ok(normalize_url(&url))
}

fn context_id(source_id: &str, normalized_url: &str) -> String {
    let digest = Sha256::digest(normalized_url.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("external_context::{source_id}::{hex}")
}

fn source_error(
    source: &ResolvedImpactSource,
    code: &str,
    reason: impl Into<String>,
) -> ImpactSourceError {
    ImpactSourceError {
        source_id: source.id.clone(),
        source_kind: source.kind,
        code: code.to_owned(),
        reason: bounded_warning(&reason.into()),
    }
}

/// The host-independent key of `raw_url` relative to a crawl's start URL.
fn origin_rebased_identity(start_url: &str, raw_url: &str) -> Result<String, ImpactError> {
    Url::parse(raw_url)
        .map_err(|error| ImpactError::Diff(format!("{raw_url} is not a URL: {error}")))?;
    Ok(crate::site::Site::new(start_url, std::iter::empty()).key(raw_url))
}

fn push_warning(warnings: &mut Vec<String>, warning: impl Into<String>) {
    if warnings.len() < MAX_IMPACT_WARNING_SAMPLES {
        warnings.push(bounded_warning(&warning.into()));
    }
}

fn bounded_warning(input: &str) -> String {
    let sanitized = input.replace(['\n', '\r'], " ");
    let mut output = sanitized
        .chars()
        .take(MAX_IMPACT_WARNING_CHARS)
        .collect::<String>();
    if sanitized.chars().count() > MAX_IMPACT_WARNING_CHARS {
        output.push_str("...");
    }
    output
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn annotation_sort(left: &ImpactAnnotation, right: &ImpactAnnotation) -> std::cmp::Ordering {
    (
        left.source_id.as_str(),
        left.target.as_str(),
        left.context_id.as_str(),
    )
        .cmp(&(
            right.source_id.as_str(),
            right.target.as_str(),
            right.context_id.as_str(),
        ))
}

pub fn validate_impact_origin(input: &str) -> Result<String, String> {
    normalize_origin(input).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> ResolvedImpactSource {
        ResolvedImpactSource {
            id: "search".to_owned(),
            kind: ImpactSourceKind::SearchConsoleCsv,
            path: PathBuf::from("search.csv"),
            display_path: "search.csv".to_owned(),
            required: false,
            period_start: None,
            period_end: None,
            origin: None,
            currency: None,
            field_map: BTreeMap::new(),
        }
    }

    fn summary(rows: u64, payload_bytes: u64) -> ImpactSourceSummary {
        ImpactSourceSummary {
            source_id: "search".to_owned(),
            kind: ImpactSourceKind::SearchConsoleCsv,
            required: false,
            state: ImpactSourceState::SkippedOptional,
            payload_bytes: Some(payload_bytes),
            payload_sha256: Some("0".repeat(64)),
            rows,
            usable_rows: 0,
            unique_urls: 0,
            duplicate_aggregate_count: 0,
            invalid_rows: rows,
            warning_count: 1,
            warnings: vec!["no_usable_rows: source contained no usable data rows".to_owned()],
            period_start: None,
            period_end: None,
            currency: None,
        }
    }

    #[test]
    fn failed_source_summaries_count_against_global_row_limit() {
        let source = source();
        let mut total_bytes = 0usize;
        let mut total_rows = MAX_IMPACT_TOTAL_ROWS;

        let error =
            charge_import_totals(&mut total_bytes, &mut total_rows, &summary(1, 0), &source)
                .expect_err("row limit fails");

        match error {
            ImpactError::Source(error) => assert_eq!(error.code, "total_rows_too_large"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn failed_source_summaries_count_against_global_byte_limit() {
        let source = source();
        let mut total_bytes = MAX_IMPACT_TOTAL_BYTES;
        let mut total_rows = 0u64;

        let error =
            charge_import_totals(&mut total_bytes, &mut total_rows, &summary(0, 1), &source)
                .expect_err("byte limit fails");

        match error {
            ImpactError::Source(error) => assert_eq!(error.code, "total_payload_too_large"),
            other => panic!("unexpected error: {other}"),
        }
    }
}
