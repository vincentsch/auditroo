//! `auditroo logs analyze`: matches access log requests with the pages of a
//! snapshot and reports what search engines and AI crawlers got from the site.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use url::Url;

use crate::assertions::validate_rule_id;
use crate::bots::{BotCatalog, ClientType};
use crate::crawl::{normalize_url, RedirectHop, ScopePattern};
use crate::extract::SeoRecord;
use crate::findings::GateDecision;
use crate::findings::Severity;
use crate::logs::{
    import_log_sources, origin_from_url, LogImportError, LogImportSummary, NormalizedLogEntry,
    ObservedTimeRange, ResolvedLogSources,
};
use crate::snapshot::SnapshotMetadata;
use crate::snapshot::{parse_json_no_duplicates, CrawlSkipReason, JsonParseError, Snapshot};

pub const LOG_ANALYSIS_REPORT_ARTIFACT_KIND: &str = "log_analysis_report";
pub const LOG_ANALYSIS_REPORT_SCHEMA_VERSION: u16 = 1;
pub const LOG_FINDING_PROVENANCE: &str = "log";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogDataState {
    NoSources,
    Empty,
    NoValidEntries,
    Partial,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbsenceSuppressionReason {
    NotEnabled,
    NoSources,
    Empty,
    NoValidEntries,
    PartialImport,
    NoSearchAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogJoinState {
    Record,
    CrawlFailure,
    CrawlSkipped,
    SitemapOnly,
    LogOnly,
    OffSnapshotOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFindingType {
    SitemapUrlNoObservedSearchHits,
    CrawlableUrlNoObservedSearchHits,
    BotHitOrphanUrl,
    #[serde(rename = "bot_4xx_or_5xx")]
    Bot4xxOr5xx,
    BotRedirectChainWaste,
    AiBotBlocked,
    AiBotAccessDiffersFromSearchBot,
}

impl LogFindingType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SitemapUrlNoObservedSearchHits => "sitemap_url_no_observed_search_hits",
            Self::CrawlableUrlNoObservedSearchHits => "crawlable_url_no_observed_search_hits",
            Self::BotHitOrphanUrl => "bot_hit_orphan_url",
            Self::Bot4xxOr5xx => "bot_4xx_or_5xx",
            Self::BotRedirectChainWaste => "bot_redirect_chain_waste",
            Self::AiBotBlocked => "ai_bot_blocked",
            Self::AiBotAccessDiffersFromSearchBot => "ai_bot_access_differs_from_search_bot",
        }
    }

    pub fn all() -> [Self; 7] {
        [
            Self::SitemapUrlNoObservedSearchHits,
            Self::CrawlableUrlNoObservedSearchHits,
            Self::BotHitOrphanUrl,
            Self::Bot4xxOr5xx,
            Self::BotRedirectChainWaste,
            Self::AiBotBlocked,
            Self::AiBotAccessDiffersFromSearchBot,
        ]
    }
}

impl fmt::Display for LogFindingType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LogFindingType {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "sitemap_url_no_observed_search_hits" => Ok(Self::SitemapUrlNoObservedSearchHits),
            "crawlable_url_no_observed_search_hits" => Ok(Self::CrawlableUrlNoObservedSearchHits),
            "bot_hit_orphan_url" => Ok(Self::BotHitOrphanUrl),
            "bot_4xx_or_5xx" => Ok(Self::Bot4xxOr5xx),
            "bot_redirect_chain_waste" => Ok(Self::BotRedirectChainWaste),
            "ai_bot_blocked" => Ok(Self::AiBotBlocked),
            "ai_bot_access_differs_from_search_bot" => Ok(Self::AiBotAccessDiffersFromSearchBot),
            _ => Err(format!("unknown log finding type {input:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlGroupRule {
    pub id: String,
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportUrlGroup {
    pub group_key: String,
    pub display_path: String,
    pub source: UrlGroupSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlGroupSource {
    Config,
    Automatic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogIgnoreRule {
    pub log_finding: Option<LogFindingType>,
    pub url: Option<String>,
    pub url_group: Option<String>,
    pub bot_family: Option<String>,
    pub client_type: Option<ClientType>,
    pub status: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogAnalysisSettings {
    pub gate: bool,
    pub absence_findings: bool,
    pub groups: Vec<UrlGroupRule>,
    pub severity_overrides: BTreeMap<LogFindingType, Severity>,
    pub ignore_rules: Vec<LogIgnoreRule>,
    pub fail_on: Severity,
}

impl Default for LogAnalysisSettings {
    fn default() -> Self {
        Self {
            gate: false,
            absence_findings: false,
            groups: Vec::new(),
            severity_overrides: BTreeMap::new(),
            ignore_rules: Vec::new(),
            fail_on: Severity::High,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogActivitySample {
    pub source_id: String,
    pub record_number: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp_utc: OffsetDateTime,
    pub method: String,
    pub status: u16,
    pub raw_user_agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlActivityRow {
    pub normalized_url: String,
    pub group_key: String,
    pub display_path: String,
    pub join_state: LogJoinState,
    pub crawl_skip_reasons: Vec<CrawlSkipReason>,
    pub client_type: ClientType,
    pub bot_family: Option<String>,
    pub method: String,
    pub status: u16,
    pub request_count: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen_utc: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_utc: OffsetDateTime,
    pub samples: Vec<LogActivitySample>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupActivityRow {
    pub group_key: String,
    pub display_path: String,
    pub client_type: ClientType,
    pub bot_family: Option<String>,
    pub method: String,
    pub status_class: String,
    pub request_count: u64,
    pub unique_url_count: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen_utc: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_utc: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogFinding {
    pub id: String,
    pub finding_type: LogFindingType,
    pub normalized_url: Option<String>,
    pub group_key: String,
    pub display_path: String,
    pub join_state: Option<LogJoinState>,
    pub crawl_skip_reasons: Vec<CrawlSkipReason>,
    pub bot_family: Option<String>,
    pub client_type: Option<ClientType>,
    pub status: Option<String>,
    pub exact_status_counts: BTreeMap<u16, u64>,
    pub request_count: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub first_seen_utc: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub last_seen_utc: Option<OffsetDateTime>,
    pub related_context: BTreeMap<String, Value>,
    pub evidence: Vec<LogActivitySample>,
    pub message: String,
    pub raw_severity: Severity,
    pub matched_ignore_rule_indices: Vec<usize>,
    pub effective_severity: Option<Severity>,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogAbsenceSummary {
    pub absence_findings_enabled: bool,
    pub absence_findings_eligible: bool,
    pub absence_suppression_reasons: Vec<AbsenceSuppressionReason>,
    pub absence_assumption: bool,
    pub observed_time_range: Option<ObservedTimeRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSeveritySummary {
    pub raw_max_severity: Option<Severity>,
    pub effective_max_severity: Option<Severity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogGateSummary {
    pub gate_enabled: bool,
    pub threshold: Option<Severity>,
    pub gate_decision: Option<GateDecision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogAnalysisReport {
    pub artifact_kind: String,
    pub schema_version: u16,
    pub auditroo_version: String,
    pub snapshot: SnapshotMetadata,
    pub data_state: LogDataState,
    pub import_summary: LogImportSummary,
    pub bot_catalog: BotCatalog,
    pub absence: LogAbsenceSummary,
    pub groups: Vec<ReportUrlGroup>,
    pub applied_ignore_rules: Vec<LogIgnoreRule>,
    pub url_activity: Vec<UrlActivityRow>,
    pub group_activity: Vec<GroupActivityRow>,
    pub findings: Vec<LogFinding>,
    pub severity: LogSeveritySummary,
    pub gate: LogGateSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogAnalysisError {
    Import(LogImportError),
    InvalidConfig(String),
    MalformedJson(String),
    DuplicateJsonKey(String),
    WrongArtifactKind(String),
    UnsupportedSchemaVersion(u64),
    InvalidReport(String),
    Io(String),
}

impl fmt::Display for LogAnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Import(error) => write!(formatter, "{error}"),
            Self::InvalidConfig(detail) => {
                write!(formatter, "the log settings are not valid: {detail}")
            }
            Self::MalformedJson(detail) => write!(formatter, "not valid JSON: {detail}"),
            Self::DuplicateJsonKey(key) => write!(formatter, "the JSON has the key {key} twice"),
            Self::WrongArtifactKind(kind) => write!(
                formatter,
                "this is not a log analysis report (artifact_kind {kind})"
            ),
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "this log analysis report has schema version {version}, which this version of auditroo cannot read"
                )
            }
            Self::InvalidReport(detail) => {
                write!(formatter, "the log analysis report is not valid: {detail}")
            }
            Self::Io(detail) => write!(formatter, "could not write the log report: {detail}"),
        }
    }
}

impl Error for LogAnalysisError {}

impl From<LogImportError> for LogAnalysisError {
    fn from(error: LogImportError) -> Self {
        Self::Import(error)
    }
}

impl From<io::Error> for LogAnalysisError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

pub fn analyze_log_sources(
    snapshot: &Snapshot,
    sources: &ResolvedLogSources,
    catalog: &BotCatalog,
    settings: &LogAnalysisSettings,
) -> Result<LogAnalysisReport, LogAnalysisError> {
    validate_log_analysis_settings(settings)?;
    let mut builder = LogAnalysisBuilder::new(snapshot, settings)?;
    let summary = import_log_sources(sources, catalog, |entry| builder.observe(entry))?;
    builder.finish(summary, catalog.clone())
}

pub fn log_report_from_str(raw: &str) -> Result<LogAnalysisReport, LogAnalysisError> {
    let value = parse_json_no_duplicates(raw).map_err(log_json_error)?;
    let object = value.as_object().ok_or_else(|| {
        LogAnalysisError::MalformedJson("top-level JSON must be an object".to_owned())
    })?;
    let artifact_kind = object
        .get("artifact_kind")
        .and_then(Value::as_str)
        .unwrap_or("<missing or non-string>");
    if artifact_kind != LOG_ANALYSIS_REPORT_ARTIFACT_KIND {
        return Err(LogAnalysisError::WrongArtifactKind(
            artifact_kind.to_owned(),
        ));
    }
    let schema_version = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            LogAnalysisError::MalformedJson("schema_version must be a number".to_owned())
        })?;
    if schema_version != u64::from(LOG_ANALYSIS_REPORT_SCHEMA_VERSION) {
        return Err(LogAnalysisError::UnsupportedSchemaVersion(schema_version));
    }
    serde_json::from_value(value)
        .map_err(|error| LogAnalysisError::MalformedJson(error.to_string()))
}

pub fn write_log_report(report: &LogAnalysisReport, path: &Path) -> Result<(), LogAnalysisError> {
    let serialized = format!(
        "{}\n",
        serde_json::to_string_pretty(report)
            .map_err(|error| LogAnalysisError::InvalidReport(error.to_string()))?
    );
    write_atomically(path, serialized.as_bytes())
}

/// Text for people: who fetched the site, what the AI crawlers got, then the
/// findings. The full URL and group tables stay in the JSON report.
pub fn render_log_human(report: &LogAnalysisReport) -> String {
    let summary = &report.import_summary;
    let mut out = format!(
        "Access logs: {} from {} file{}",
        requests(summary.parsed_entries),
        summary.matched_file_count,
        if summary.matched_file_count == 1 {
            ""
        } else {
            "s"
        }
    );
    if let Some(range) = &summary.time_range {
        out.push_str(&format!(
            ", {} to {}",
            crate::render::format_time(range.first_seen_utc),
            crate::render::format_time(range.last_seen_utc)
        ));
    }
    out.push_str(".\n");
    let skipped = summary.parse_error_count + summary.normalization_error_count;
    if skipped > 0 {
        out.push_str(&format!(
            "{} line{} could not be read and {} left out.\n",
            thousands(skipped),
            if skipped == 1 { "" } else { "s" },
            if skipped == 1 { "was" } else { "were" }
        ));
    }

    let families = family_stats(report);
    let sections = [
        (ClientType::Search, "Search engines"),
        (ClientType::AiBot, "AI crawlers and assistants"),
        (ClientType::SocialPreview, "Link previews"),
        (ClientType::UptimeMonitoring, "Uptime monitors"),
        (ClientType::GenericAutomated, "Other bots"),
        (ClientType::BrowserLike, "Browsers"),
        (ClientType::Unknown, "Unknown clients"),
    ];
    out.push_str("\nWho fetched your pages\n");
    for (client_type, title) in sections {
        let total = summary
            .counts_by_client_type
            .get(&client_type)
            .copied()
            .unwrap_or(0);
        if total == 0 {
            continue;
        }
        out.push_str(&format!("  {title}: {}\n", requests(total)));
        if matches!(
            client_type,
            ClientType::Search | ClientType::AiBot | ClientType::SocialPreview
        ) {
            for family in families
                .iter()
                .filter(|family| family.client_type == client_type)
            {
                out.push_str(&format!("    {}\n", family.describe()));
            }
        }
    }

    let ai: Vec<&FamilyStats> = families
        .iter()
        .filter(|family| family.client_type == ClientType::AiBot)
        .collect();
    let everywhere: Vec<&str> = ai
        .iter()
        .filter(|family| family.blocked == family.requests)
        .map(|family| family.name.as_str())
        .collect();
    // Blocks per site section come from the ai_bot_blocked findings, so this
    // summary and the findings below always agree.
    let mut by_section: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for finding in report
        .findings
        .iter()
        .filter(|finding| finding.finding_type == LogFindingType::AiBotBlocked)
    {
        let name = finding
            .bot_family
            .as_deref()
            .map(|family| family_display_name(report, family))
            .unwrap_or_else(|| "AI crawlers".to_owned());
        by_section
            .entry(finding.display_path.clone())
            .or_default()
            .push(name);
    }
    if !ai.is_empty() {
        out.push('\n');
        if !everywhere.is_empty() {
            out.push_str(&format!(
                "These AI crawlers were blocked on every request, so they cannot read your pages: {}.\n",
                everywhere.join(", ")
            ));
        }
        if by_section.is_empty() {
            out.push_str("No AI crawler request got a blocked response.\n");
        } else {
            out.push_str("AI crawlers got blocked responses here:\n");
            for (section, names) in &by_section {
                out.push_str(&format!("  {section}: {}\n", names.join(", ")));
            }
        }
    }

    let mut active: Vec<&LogFinding> = report
        .findings
        .iter()
        .filter(|finding| finding.effective_severity.is_some())
        .collect();
    // Most severe first; site-wide crawler findings before per-URL rows.
    active.sort_by_key(|finding| {
        (
            std::cmp::Reverse(finding.effective_severity),
            !matches!(
                finding.finding_type,
                LogFindingType::AiBotBlocked | LogFindingType::AiBotAccessDiffersFromSearchBot
            ),
        )
    });
    out.push_str("\nFindings\n");
    if active.is_empty() {
        out.push_str("  none\n");
    }
    for finding in active.iter().take(30) {
        out.push_str(&format!(
            "  {:<8}{}\n",
            finding
                .effective_severity
                .map(|severity| severity.as_str())
                .unwrap_or("-"),
            human_log_finding(finding, report)
        ));
    }
    if active.len() > 30 {
        out.push_str(&format!("  and {} more\n", active.len() - 30));
    }
    let ignored = report.findings.len() - active.len();
    if ignored > 0 {
        out.push_str(&format!("  {ignored} more matched an ignore rule\n"));
    }
    if let Some(decision) = report.gate.gate_decision {
        out.push_str(&format!(
            "\nLog gate: {} (fails on {} and above).\n",
            gate_name(decision),
            report
                .gate
                .threshold
                .map(|severity| severity.as_str())
                .unwrap_or("high")
        ));
    }
    out.push_str(&format!(
        "\nThe full tables have {} URL rows and {} group rows. Use --json or --out to get them.\n",
        report.url_activity.len(),
        report.group_activity.len()
    ));
    out
}

struct FamilyStats {
    name: String,
    client_type: ClientType,
    requests: u64,
    urls: usize,
    ok: u64,
    blocked: u64,
    errors: u64,
}

impl FamilyStats {
    fn describe(&self) -> String {
        let mut text = format!(
            "{}: {} to {} URL{}",
            self.name,
            requests(self.requests),
            self.urls,
            if self.urls == 1 { "" } else { "s" }
        );
        if self.blocked == self.requests {
            text.push_str(", all blocked (401, 403 or 429)");
        } else if self.blocked > 0 {
            text.push_str(&format!(", {} blocked", thousands(self.blocked)));
        }
        if self.errors > 0 {
            text.push_str(&format!(
                ", {} error{}",
                thousands(self.errors),
                if self.errors == 1 { "" } else { "s" }
            ));
        }
        if self.ok == self.requests {
            text.push_str(", all answered");
        }
        text
    }
}

fn family_stats(report: &LogAnalysisReport) -> Vec<FamilyStats> {
    let mut by_family: BTreeMap<&str, (FamilyStats, BTreeSet<&str>)> = BTreeMap::new();
    for row in &report.url_activity {
        let Some(family) = row.bot_family.as_deref() else {
            continue;
        };
        let (stats, urls) = by_family.entry(family).or_insert_with(|| {
            let name = report
                .bot_catalog
                .rules
                .iter()
                .find(|rule| rule.family_id == family)
                .map(|rule| rule.display_name.clone())
                .unwrap_or_else(|| family.to_owned());
            let stats = FamilyStats {
                name,
                client_type: row.client_type,
                requests: 0,
                urls: 0,
                ok: 0,
                blocked: 0,
                errors: 0,
            };
            (stats, BTreeSet::new())
        });
        stats.requests += row.request_count;
        urls.insert(row.normalized_url.as_str());
        match row.status {
            401 | 403 | 429 => stats.blocked += row.request_count,
            status if status >= 400 => stats.errors += row.request_count,
            _ => stats.ok += row.request_count,
        }
    }
    let mut stats: Vec<FamilyStats> = by_family
        .into_values()
        .map(|(mut stats, urls)| {
            stats.urls = urls.len();
            stats
        })
        .collect();
    stats.sort_by(|left, right| {
        right
            .requests
            .cmp(&left.requests)
            .then(left.name.cmp(&right.name))
    });
    stats
}

fn thousands(value: u64) -> String {
    crate::findings::format_count(value as usize)
}

/// "1 request", "1,200 requests".
fn requests(count: u64) -> String {
    format!(
        "{} request{}",
        thousands(count),
        if count == 1 { "" } else { "s" }
    )
}

pub fn validate_url_group_rule(rule: &UrlGroupRule) -> Result<(), LogAnalysisError> {
    if !validate_rule_id(&rule.id) || rule.id.contains(['.', '_']) {
        return Err(LogAnalysisError::InvalidConfig(
            "a [[logs.group]] id must match ^[a-z][a-z0-9-]{0,63}$".to_owned(),
        ));
    }
    if !rule.prefix.starts_with('/') || rule.prefix.contains('?') || rule.prefix.contains('#') {
        return Err(LogAnalysisError::InvalidConfig(
            "a [[logs.group]] prefix must be a path that starts with /, without ? or #".to_owned(),
        ));
    }
    Ok(())
}

pub fn validate_log_ignore_rule(rule: &LogIgnoreRule) -> Result<(), LogAnalysisError> {
    if rule.log_finding.is_none()
        && rule.url.is_none()
        && rule.url_group.is_none()
        && rule.bot_family.is_none()
        && rule.client_type.is_none()
        && rule.status.is_none()
    {
        return Err(LogAnalysisError::InvalidConfig(
            "a log ignore rule needs at least one field besides reason, such as url or log_finding"
                .to_owned(),
        ));
    }
    if let Some(url) = &rule.url {
        validate_ignore_url(url)?;
    }
    if let Some(group) = &rule.url_group {
        if group.is_empty() {
            return Err(LogAnalysisError::InvalidConfig(
                "url_group in a log ignore rule must not be empty".to_owned(),
            ));
        }
    }
    if let Some(family) = &rule.bot_family {
        if !validate_rule_id(family) || family.contains(['.', '_']) {
            return Err(LogAnalysisError::InvalidConfig(
                "bot_family in a log ignore rule must match ^[a-z][a-z0-9-]{0,63}$".to_owned(),
            ));
        }
    }
    if let Some(status) = &rule.status {
        validate_status_selector(status)?;
    }
    if let Some(reason) = &rule.reason {
        if reason.chars().count() > 240 || reason.contains('\n') || reason.contains('\r') {
            return Err(LogAnalysisError::InvalidConfig(
                "reason in a log ignore rule must be one line of at most 240 characters".to_owned(),
            ));
        }
    }
    Ok(())
}

pub fn validate_log_analysis_settings(
    settings: &LogAnalysisSettings,
) -> Result<(), LogAnalysisError> {
    let mut prefixes = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for group in &settings.groups {
        validate_url_group_rule(group)?;
        if !ids.insert(group.id.clone()) {
            return Err(LogAnalysisError::InvalidConfig(format!(
                "two [[logs.group]] entries use the id {:?}",
                group.id
            )));
        }
        if !prefixes.insert(group.prefix.clone()) {
            return Err(LogAnalysisError::InvalidConfig(format!(
                "two [[logs.group]] entries use the prefix {:?}",
                group.prefix
            )));
        }
    }
    for rule in &settings.ignore_rules {
        validate_log_ignore_rule(rule)?;
    }
    Ok(())
}

struct LogAnalysisBuilder<'a> {
    snapshot: &'a Snapshot,
    settings: &'a LogAnalysisSettings,
    start_url: Url,
    declared_sitemap_urls: BTreeSet<String>,
    orphan_urls: BTreeSet<String>,
    url_activity: BTreeMap<UrlActivityKey, UrlActivityAccumulator>,
    group_activity: BTreeMap<GroupActivityKey, GroupActivityAccumulator>,
    groups_used: BTreeMap<String, ReportUrlGroup>,
    finding_accumulators: BTreeMap<FindingKey, FindingAccumulator>,
    search_attention_urls: BTreeSet<String>,
    search_attention_count: u64,
    search_group_url_stats: BTreeMap<(String, String), AccessStats>,
    ai_group_url_stats: BTreeMap<(String, String, String), AccessStats>,
    ai_group_family_stats: BTreeMap<(String, String), AiFamilyStats>,
}

impl<'a> LogAnalysisBuilder<'a> {
    fn new(
        snapshot: &'a Snapshot,
        settings: &'a LogAnalysisSettings,
    ) -> Result<Self, LogAnalysisError> {
        let start_url = Url::parse(&snapshot.start_url).map_err(|error| {
            LogAnalysisError::InvalidReport(format!(
                "the snapshot has an invalid start URL: {error}"
            ))
        })?;
        Ok(Self {
            snapshot,
            settings,
            start_url,
            declared_sitemap_urls: snapshot
                .sitemap
                .discovery
                .declared_urls
                .iter()
                .cloned()
                .collect(),
            orphan_urls: snapshot.orphans.iter().cloned().collect(),
            url_activity: BTreeMap::new(),
            group_activity: BTreeMap::new(),
            groups_used: BTreeMap::new(),
            finding_accumulators: BTreeMap::new(),
            search_attention_urls: BTreeSet::new(),
            search_attention_count: 0,
            search_group_url_stats: BTreeMap::new(),
            ai_group_url_stats: BTreeMap::new(),
            ai_group_family_stats: BTreeMap::new(),
        })
    }

    fn observe(&mut self, entry: NormalizedLogEntry) {
        let parsed_url = Url::parse(&entry.normalized_url).ok();
        let group = self.group_for(parsed_url.as_ref(), &entry);
        self.groups_used
            .entry(group.group_key.clone())
            .or_insert_with(|| group.report.clone());
        let join = self.join_for(&entry.normalized_url);
        let crawl_skip_reasons = self
            .snapshot
            .crawl_observation
            .skipped_candidates
            .get(&entry.normalized_url)
            .cloned()
            .unwrap_or_default();
        let sample = sample_from_entry(&entry);

        let url_key = UrlActivityKey {
            normalized_url: entry.normalized_url.clone(),
            client_type: entry.client_type,
            bot_family: entry.bot_family.clone(),
            method: entry.method.clone(),
            status: entry.status,
        };
        self.url_activity
            .entry(url_key)
            .or_insert_with(|| {
                UrlActivityAccumulator::new(&entry, &group, join, &crawl_skip_reasons)
            })
            .observe(&entry, &sample);

        let group_key = GroupActivityKey {
            group_key: group.group_key.clone(),
            client_type: entry.client_type,
            bot_family: entry.bot_family.clone(),
            method: entry.method.clone(),
            status_class: status_class(entry.status),
        };
        self.group_activity
            .entry(group_key)
            .or_insert_with(|| GroupActivityAccumulator::new(&group))
            .observe(&entry);

        let is_attention_method = matches!(entry.method.as_str(), "GET" | "HEAD");
        if is_attention_method && entry.client_type == ClientType::Search {
            self.search_attention_count += 1;
            self.search_attention_urls
                .insert(entry.normalized_url.clone());
            self.search_group_url_stats
                .entry((group.group_key.clone(), entry.normalized_url.clone()))
                .or_default()
                .observe(&entry, &sample);
        }

        if entry.client_type == ClientType::AiBot && is_attention_method {
            if let Some(family) = &entry.bot_family {
                self.ai_group_url_stats
                    .entry((
                        group.group_key.clone(),
                        family.clone(),
                        entry.normalized_url.clone(),
                    ))
                    .or_default()
                    .observe(&entry, &sample);
                self.ai_group_family_stats
                    .entry((group.group_key.clone(), family.clone()))
                    .or_insert_with(|| AiFamilyStats::new(&group))
                    .observe(&entry, &sample);
            }
        }

        if entry.client_type.is_classified_bot() {
            self.observe_positive_findings(&entry, &sample, &group, join, &crawl_skip_reasons);
        }
    }

    fn observe_positive_findings(
        &mut self,
        entry: &NormalizedLogEntry,
        sample: &LogActivitySample,
        group: &ResolvedGroup,
        join: LogJoinState,
        crawl_skip_reasons: &[CrawlSkipReason],
    ) {
        let Some(family) = &entry.bot_family else {
            return;
        };
        let is_attention_method = matches!(entry.method.as_str(), "GET" | "HEAD");
        if is_attention_method && self.orphan_urls.contains(&entry.normalized_url) {
            self.accumulate_finding(
                FindingSeed {
                    key: FindingKey::url(
                        LogFindingType::BotHitOrphanUrl,
                        &entry.normalized_url,
                        group,
                        family,
                        entry.client_type,
                        None,
                    ),
                    join_state: join,
                    crawl_skip_reasons: crawl_skip_reasons.to_vec(),
                    raw_severity: Severity::Medium,
                    message: "a bot fetched a URL that no crawled page links to".to_owned(),
                    context: json!({"orphan": true}),
                },
                entry,
                sample,
            );
        }

        if (400..=599).contains(&entry.status) {
            let class = status_class(entry.status);
            let severity = if class == "5xx" {
                Severity::High
            } else {
                Severity::Medium
            };
            self.accumulate_finding(
                FindingSeed {
                    key: FindingKey::url(
                        LogFindingType::Bot4xxOr5xx,
                        &entry.normalized_url,
                        group,
                        family,
                        entry.client_type,
                        Some(class),
                    ),
                    join_state: join,
                    crawl_skip_reasons: crawl_skip_reasons.to_vec(),
                    raw_severity: severity,
                    message: "a bot got an error response".to_owned(),
                    context: json!({"status_class": status_class(entry.status)}),
                },
                entry,
                sample,
            );
        }

        if is_attention_method {
            if let Some(record) = self.snapshot.records.get(&entry.normalized_url) {
                let looped = redirect_loop(record);
                if record.redirect_chain.len() >= 2 || looped {
                    let severity = if looped {
                        Severity::High
                    } else {
                        Severity::Medium
                    };
                    self.accumulate_finding(
                        FindingSeed {
                            key: FindingKey::url(
                                LogFindingType::BotRedirectChainWaste,
                                &entry.normalized_url,
                                group,
                                family,
                                entry.client_type,
                                None,
                            ),
                            join_state: join,
                            crawl_skip_reasons: crawl_skip_reasons.to_vec(),
                            raw_severity: severity,
                            message: "a bot fetched a URL that redirects more than once or loops"
                                .to_owned(),
                            context: json!({
                                "redirect_hops": record.redirect_chain.len(),
                                "redirect_loop": looped
                            }),
                        },
                        entry,
                        sample,
                    );
                }
            }
        }
    }

    fn accumulate_finding(
        &mut self,
        seed: FindingSeed,
        entry: &NormalizedLogEntry,
        sample: &LogActivitySample,
    ) {
        self.finding_accumulators
            .entry(seed.key.clone())
            .or_insert_with(|| FindingAccumulator::new(seed))
            .observe(entry, sample);
    }

    fn finish(
        mut self,
        import_summary: LogImportSummary,
        bot_catalog: BotCatalog,
    ) -> Result<LogAnalysisReport, LogAnalysisError> {
        let data_state = data_state(&import_summary);
        let absence = absence_summary(
            self.settings.absence_findings,
            data_state,
            import_summary.time_range.clone(),
            self.search_attention_count,
        );
        let snapshot_metadata = snapshot_metadata(self.snapshot);
        let mut findings = std::mem::take(&mut self.finding_accumulators)
            .into_values()
            .map(FindingAccumulator::into_finding)
            .collect::<Vec<_>>();
        findings.extend(self.ai_blocked_findings());
        findings.extend(self.ai_access_diff_findings());
        if absence.absence_findings_eligible {
            findings.extend(self.absence_findings());
        }
        apply_severity_overrides(&mut findings, &self.settings.severity_overrides);
        apply_log_ignores(&mut findings, &self.settings.ignore_rules)?;
        sort_findings(&mut findings);

        let raw_max_severity = findings.iter().map(|finding| finding.raw_severity).max();
        let effective_max_severity = findings
            .iter()
            .filter_map(|finding| finding.effective_severity)
            .max();
        let gate_decision = if self.settings.gate {
            Some(gate_decision(effective_max_severity, self.settings.fail_on))
        } else {
            None
        };

        let mut groups = std::mem::take(&mut self.groups_used)
            .into_values()
            .collect::<Vec<_>>();
        groups.sort_by(|left, right| left.group_key.cmp(&right.group_key));

        let mut url_activity = std::mem::take(&mut self.url_activity)
            .into_values()
            .map(UrlActivityAccumulator::into_row)
            .collect::<Vec<_>>();
        sort_url_activity(&mut url_activity);
        let mut group_activity = std::mem::take(&mut self.group_activity)
            .into_values()
            .map(GroupActivityAccumulator::into_row)
            .collect::<Vec<_>>();
        sort_group_activity(&mut group_activity);

        let report = LogAnalysisReport {
            artifact_kind: LOG_ANALYSIS_REPORT_ARTIFACT_KIND.to_owned(),
            schema_version: LOG_ANALYSIS_REPORT_SCHEMA_VERSION,
            auditroo_version: env!("CARGO_PKG_VERSION").to_owned(),
            snapshot: snapshot_metadata,
            data_state,
            import_summary,
            bot_catalog,
            absence,
            groups,
            applied_ignore_rules: self.settings.ignore_rules.clone(),
            url_activity,
            group_activity,
            findings,
            severity: LogSeveritySummary {
                raw_max_severity,
                effective_max_severity,
            },
            gate: LogGateSummary {
                gate_enabled: self.settings.gate,
                threshold: self.settings.gate.then_some(self.settings.fail_on),
                gate_decision,
            },
        };
        Ok(report)
    }

    fn group_for(&self, parsed_url: Option<&Url>, entry: &NormalizedLogEntry) -> ResolvedGroup {
        let display_path = parsed_url
            .map(first_segment_path)
            .unwrap_or_else(|| entry.observed_path.clone());
        if let Some(url) = parsed_url {
            let mut configured = self
                .settings
                .groups
                .iter()
                .filter(|group| segment_prefix_matches(url.path(), &group.prefix))
                .collect::<Vec<_>>();
            configured.sort_by(|left, right| {
                right
                    .prefix
                    .len()
                    .cmp(&left.prefix.len())
                    .then_with(|| left.id.cmp(&right.id))
            });
            if let Some(group) = configured.first() {
                return ResolvedGroup {
                    group_key: group.id.clone(),
                    display_path: group.prefix.clone(),
                    report: ReportUrlGroup {
                        group_key: group.id.clone(),
                        display_path: group.prefix.clone(),
                        source: UrlGroupSource::Config,
                    },
                };
            }
        }

        let same_origin =
            parsed_url.is_some_and(|url| same_snapshot_boundary(&self.start_url, url));
        let group_key = if same_origin {
            format!("path:{display_path}")
        } else {
            format!("origin:{}|path:{display_path}", entry.observed_origin)
        };
        // A section shows a trailing slash when its URLs have one, so
        // "/coffee/" in the text matches the URLs "/coffee/" and
        // "/coffee/house-blend/". The first URL of a section decides, so every
        // row of one section shows the same path.
        let display_path = match self.groups_used.get(&group_key) {
            Some(group) => group.display_path.clone(),
            None => parsed_url.map(section_display_path).unwrap_or(display_path),
        };
        ResolvedGroup {
            group_key: group_key.clone(),
            display_path: display_path.clone(),
            report: ReportUrlGroup {
                group_key,
                display_path,
                source: UrlGroupSource::Automatic,
            },
        }
    }
    fn join_for(&self, normalized_url: &str) -> LogJoinState {
        if self.snapshot.records.contains_key(normalized_url) {
            return LogJoinState::Record;
        }
        if self.snapshot.crawl_failures.contains_key(normalized_url) {
            return LogJoinState::CrawlFailure;
        }
        if self
            .snapshot
            .crawl_observation
            .skipped_candidates
            .contains_key(normalized_url)
        {
            return LogJoinState::CrawlSkipped;
        }
        if self.declared_sitemap_urls.contains(normalized_url) {
            return LogJoinState::SitemapOnly;
        }
        if Url::parse(normalized_url)
            .ok()
            .is_some_and(|url| same_snapshot_boundary(&self.start_url, &url))
        {
            LogJoinState::LogOnly
        } else {
            LogJoinState::OffSnapshotOrigin
        }
    }

    fn ai_blocked_findings(&self) -> Vec<LogFinding> {
        let mut findings = Vec::new();
        for ((group_key, family), stats) in &self.ai_group_family_stats {
            if stats.blocked_count == 0 {
                continue;
            }
            let ratio = stats.blocked_count as f64 / stats.eligible_count.max(1) as f64;
            let mut related = BTreeMap::new();
            related.insert("blocked_count".to_owned(), json!(stats.blocked_count));
            related.insert("eligible_count".to_owned(), json!(stats.eligible_count));
            related.insert("blocked_ratio".to_owned(), json!(ratio));
            findings.push(log_finding(LogFindingParts {
                finding_type: LogFindingType::AiBotBlocked,
                normalized_url: None,
                group_key: group_key.clone(),
                display_path: stats.display_path.clone(),
                join_state: None,
                crawl_skip_reasons: Vec::new(),
                bot_family: Some(family.clone()),
                client_type: Some(ClientType::AiBot),
                status: None,
                exact_status_counts: stats.exact_status_counts.clone(),
                request_count: stats.eligible_count,
                first_seen_utc: stats.first_seen,
                last_seen_utc: stats.last_seen,
                related_context: related,
                evidence: stats.samples.clone(),
                message: if stats.blocked_count == stats.eligible_count {
                    "every request from this AI bot got a blocked response (401, 403 or 429)"
                        .to_owned()
                } else {
                    "some requests from this AI bot got a blocked response (401, 403 or 429)"
                        .to_owned()
                },
                raw_severity: Severity::Medium,
            }));
        }
        findings
    }

    fn ai_access_diff_findings(&self) -> Vec<LogFinding> {
        let mut by_group_family: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        for (group_key, family, url) in self.ai_group_url_stats.keys() {
            if self
                .search_group_url_stats
                .contains_key(&(group_key.clone(), url.clone()))
            {
                by_group_family
                    .entry((group_key.clone(), family.clone()))
                    .or_default()
                    .insert(url.clone());
            }
        }

        let mut findings = Vec::new();
        for ((group_key, family), shared_urls) in by_group_family {
            if shared_urls.is_empty() {
                continue;
            }
            let search_all_accessible = shared_urls.iter().all(|url| {
                self.search_group_url_stats
                    .get(&(group_key.clone(), url.clone()))
                    .is_some_and(AccessStats::has_accessible_status)
            });
            let ai_all_blocked = shared_urls.iter().all(|url| {
                self.ai_group_url_stats
                    .get(&(group_key.clone(), family.clone(), url.clone()))
                    .is_some_and(AccessStats::all_statuses_blocked)
            });
            if !search_all_accessible || !ai_all_blocked {
                continue;
            }
            let Some(stats) = self
                .ai_group_family_stats
                .get(&(group_key.clone(), family.clone()))
            else {
                continue;
            };
            let mut ai_shared = AccessStats::default();
            let mut search_shared = AccessStats::default();
            for url in &shared_urls {
                if let Some(stats) =
                    self.ai_group_url_stats
                        .get(&(group_key.clone(), family.clone(), url.clone()))
                {
                    ai_shared.merge(stats);
                }
                if let Some(stats) = self
                    .search_group_url_stats
                    .get(&(group_key.clone(), url.clone()))
                {
                    search_shared.merge(stats);
                }
            }
            let mut related = BTreeMap::new();
            related.insert("shared_url_count".to_owned(), json!(shared_urls.len()));
            related.insert(
                "shared_urls".to_owned(),
                Value::Array(shared_urls.iter().cloned().map(Value::String).collect()),
            );
            related.insert(
                "ai_request_count".to_owned(),
                json!(ai_shared.request_count),
            );
            related.insert(
                "ai_exact_status_counts".to_owned(),
                json!(ai_shared.exact_status_counts),
            );
            related.insert(
                "search_request_count".to_owned(),
                json!(search_shared.request_count),
            );
            related.insert(
                "search_exact_status_counts".to_owned(),
                json!(search_shared.exact_status_counts),
            );
            related.insert("search_samples".to_owned(), json!(search_shared.samples));
            findings.push(log_finding(LogFindingParts {
                finding_type: LogFindingType::AiBotAccessDiffersFromSearchBot,
                normalized_url: None,
                group_key: group_key.clone(),
                display_path: stats.display_path.clone(),
                join_state: None,
                crawl_skip_reasons: Vec::new(),
                bot_family: Some(family),
                client_type: Some(ClientType::AiBot),
                status: None,
                exact_status_counts: ai_shared.exact_status_counts,
                request_count: ai_shared.request_count,
                first_seen_utc: ai_shared.first_seen,
                last_seen_utc: ai_shared.last_seen,
                related_context: related,
                evidence: ai_shared.samples,
                message: "this AI bot was blocked on URLs that search engine bots could fetch"
                    .to_owned(),
                raw_severity: Severity::Medium,
            }));
        }
        findings
    }

    fn absence_findings(&mut self) -> Vec<LogFinding> {
        let mut findings = Vec::new();
        let mut sitemap_absent = BTreeSet::new();
        for url in &self.snapshot.sitemap.discovery.declared_urls {
            if self.search_attention_urls.contains(url) {
                continue;
            }
            sitemap_absent.insert(url.clone());
            let group = self.group_for(Url::parse(url).ok().as_ref(), &synthetic_entry(url));
            self.groups_used
                .entry(group.group_key.clone())
                .or_insert_with(|| group.report.clone());
            findings.push(absence_finding(
                LogFindingType::SitemapUrlNoObservedSearchHits,
                url,
                &group,
                self.join_for(url),
                self.snapshot
                    .crawl_observation
                    .skipped_candidates
                    .get(url)
                    .cloned()
                    .unwrap_or_default(),
                "the sitemap lists this URL, but no search engine bot fetched it",
            ));
        }
        for (url, record) in &self.snapshot.records {
            if sitemap_absent.contains(url)
                || self.declared_sitemap_urls.contains(url)
                || self.search_attention_urls.contains(url)
                || !(200..=299).contains(&record.http_status)
                || !record.redirect_chain.is_empty()
            {
                continue;
            }
            let group = self.group_for(Url::parse(url).ok().as_ref(), &synthetic_entry(url));
            self.groups_used
                .entry(group.group_key.clone())
                .or_insert_with(|| group.report.clone());
            findings.push(absence_finding(
                LogFindingType::CrawlableUrlNoObservedSearchHits,
                url,
                &group,
                LogJoinState::Record,
                Vec::new(),
                "this URL works, but no search engine bot fetched it",
            ));
        }
        findings
    }
}

#[derive(Debug, Clone)]
struct ResolvedGroup {
    group_key: String,
    display_path: String,
    report: ReportUrlGroup,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct UrlActivityKey {
    normalized_url: String,
    client_type: ClientType,
    bot_family: Option<String>,
    method: String,
    status: u16,
}

#[derive(Debug, Clone)]
struct UrlActivityAccumulator {
    normalized_url: String,
    group_key: String,
    display_path: String,
    join_state: LogJoinState,
    crawl_skip_reasons: Vec<CrawlSkipReason>,
    client_type: ClientType,
    bot_family: Option<String>,
    method: String,
    status: u16,
    request_count: u64,
    first_seen_utc: OffsetDateTime,
    last_seen_utc: OffsetDateTime,
    samples: Vec<LogActivitySample>,
}

impl UrlActivityAccumulator {
    fn new(
        entry: &NormalizedLogEntry,
        group: &ResolvedGroup,
        join_state: LogJoinState,
        crawl_skip_reasons: &[CrawlSkipReason],
    ) -> Self {
        Self {
            normalized_url: entry.normalized_url.clone(),
            group_key: group.group_key.clone(),
            display_path: group.display_path.clone(),
            join_state,
            crawl_skip_reasons: crawl_skip_reasons.to_vec(),
            client_type: entry.client_type,
            bot_family: entry.bot_family.clone(),
            method: entry.method.clone(),
            status: entry.status,
            request_count: 0,
            first_seen_utc: entry.timestamp_utc,
            last_seen_utc: entry.timestamp_utc,
            samples: Vec::new(),
        }
    }

    fn observe(&mut self, entry: &NormalizedLogEntry, sample: &LogActivitySample) {
        self.request_count += 1;
        if entry.timestamp_utc < self.first_seen_utc {
            self.first_seen_utc = entry.timestamp_utc;
        }
        if entry.timestamp_utc > self.last_seen_utc {
            self.last_seen_utc = entry.timestamp_utc;
        }
        if self.samples.len() < 5 {
            self.samples.push(sample.clone());
        }
    }

    fn into_row(self) -> UrlActivityRow {
        UrlActivityRow {
            normalized_url: self.normalized_url,
            group_key: self.group_key,
            display_path: self.display_path,
            join_state: self.join_state,
            crawl_skip_reasons: self.crawl_skip_reasons,
            client_type: self.client_type,
            bot_family: self.bot_family,
            method: self.method,
            status: self.status,
            request_count: self.request_count,
            first_seen_utc: self.first_seen_utc,
            last_seen_utc: self.last_seen_utc,
            samples: self.samples,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupActivityKey {
    group_key: String,
    client_type: ClientType,
    bot_family: Option<String>,
    method: String,
    status_class: String,
}

#[derive(Debug, Clone)]
struct GroupActivityAccumulator {
    group_key: String,
    display_path: String,
    client_type: Option<ClientType>,
    bot_family: Option<String>,
    method: Option<String>,
    status_class: Option<String>,
    request_count: u64,
    unique_urls: BTreeSet<String>,
    first_seen_utc: Option<OffsetDateTime>,
    last_seen_utc: Option<OffsetDateTime>,
}

impl GroupActivityAccumulator {
    fn new(group: &ResolvedGroup) -> Self {
        Self {
            group_key: group.group_key.clone(),
            display_path: group.display_path.clone(),
            client_type: None,
            bot_family: None,
            method: None,
            status_class: None,
            request_count: 0,
            unique_urls: BTreeSet::new(),
            first_seen_utc: None,
            last_seen_utc: None,
        }
    }

    fn observe(&mut self, entry: &NormalizedLogEntry) {
        self.client_type = Some(entry.client_type);
        self.bot_family = entry.bot_family.clone();
        self.method = Some(entry.method.clone());
        self.status_class = Some(status_class(entry.status));
        self.request_count += 1;
        self.unique_urls.insert(entry.normalized_url.clone());
        self.first_seen_utc = Some(
            self.first_seen_utc
                .map(|current| current.min(entry.timestamp_utc))
                .unwrap_or(entry.timestamp_utc),
        );
        self.last_seen_utc = Some(
            self.last_seen_utc
                .map(|current| current.max(entry.timestamp_utc))
                .unwrap_or(entry.timestamp_utc),
        );
    }

    fn into_row(self) -> GroupActivityRow {
        GroupActivityRow {
            group_key: self.group_key,
            display_path: self.display_path,
            client_type: self
                .client_type
                .expect("group activity observed client_type"),
            bot_family: self.bot_family,
            method: self.method.expect("group activity observed method"),
            status_class: self
                .status_class
                .expect("group activity observed status_class"),
            request_count: self.request_count,
            unique_url_count: self.unique_urls.len(),
            first_seen_utc: self
                .first_seen_utc
                .expect("group activity observed first timestamp"),
            last_seen_utc: self
                .last_seen_utc
                .expect("group activity observed last timestamp"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FindingKey {
    finding_type: LogFindingType,
    normalized_url: Option<String>,
    group_key: String,
    display_path: String,
    bot_family: Option<String>,
    client_type: Option<ClientType>,
    status: Option<String>,
}

impl FindingKey {
    fn url(
        finding_type: LogFindingType,
        normalized_url: &str,
        group: &ResolvedGroup,
        bot_family: &str,
        client_type: ClientType,
        status: Option<String>,
    ) -> Self {
        Self {
            finding_type,
            normalized_url: Some(normalized_url.to_owned()),
            group_key: group.group_key.clone(),
            display_path: group.display_path.clone(),
            bot_family: Some(bot_family.to_owned()),
            client_type: Some(client_type),
            status,
        }
    }
}

#[derive(Debug, Clone)]
struct FindingSeed {
    key: FindingKey,
    join_state: LogJoinState,
    crawl_skip_reasons: Vec<CrawlSkipReason>,
    raw_severity: Severity,
    message: String,
    context: Value,
}

#[derive(Debug, Clone)]
struct FindingAccumulator {
    key: FindingKey,
    join_state: LogJoinState,
    crawl_skip_reasons: Vec<CrawlSkipReason>,
    exact_status_counts: BTreeMap<u16, u64>,
    request_count: u64,
    first_seen: Option<OffsetDateTime>,
    last_seen: Option<OffsetDateTime>,
    related_context: BTreeMap<String, Value>,
    evidence: Vec<LogActivitySample>,
    message: String,
    raw_severity: Severity,
}

impl FindingAccumulator {
    fn new(seed: FindingSeed) -> Self {
        let mut related_context = BTreeMap::new();
        related_context.insert("context".to_owned(), seed.context);
        Self {
            key: seed.key,
            join_state: seed.join_state,
            crawl_skip_reasons: seed.crawl_skip_reasons,
            exact_status_counts: BTreeMap::new(),
            request_count: 0,
            first_seen: None,
            last_seen: None,
            related_context,
            evidence: Vec::new(),
            message: seed.message,
            raw_severity: seed.raw_severity,
        }
    }

    fn observe(&mut self, entry: &NormalizedLogEntry, sample: &LogActivitySample) {
        self.request_count += 1;
        *self.exact_status_counts.entry(entry.status).or_insert(0) += 1;
        self.first_seen = Some(
            self.first_seen
                .map(|current| current.min(entry.timestamp_utc))
                .unwrap_or(entry.timestamp_utc),
        );
        self.last_seen = Some(
            self.last_seen
                .map(|current| current.max(entry.timestamp_utc))
                .unwrap_or(entry.timestamp_utc),
        );
        if self.evidence.len() < 5 {
            self.evidence.push(sample.clone());
        }
    }

    fn into_finding(self) -> LogFinding {
        log_finding(LogFindingParts {
            finding_type: self.key.finding_type,
            normalized_url: self.key.normalized_url,
            group_key: self.key.group_key,
            display_path: self.key.display_path,
            join_state: Some(self.join_state),
            crawl_skip_reasons: self.crawl_skip_reasons,
            bot_family: self.key.bot_family,
            client_type: self.key.client_type,
            status: self.key.status,
            exact_status_counts: self.exact_status_counts,
            request_count: self.request_count,
            first_seen_utc: self.first_seen,
            last_seen_utc: self.last_seen,
            related_context: self.related_context,
            evidence: self.evidence,
            message: self.message,
            raw_severity: self.raw_severity,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct AccessStats {
    request_count: u64,
    exact_status_counts: BTreeMap<u16, u64>,
    first_seen: Option<OffsetDateTime>,
    last_seen: Option<OffsetDateTime>,
    samples: Vec<LogActivitySample>,
}

impl AccessStats {
    fn observe(&mut self, entry: &NormalizedLogEntry, sample: &LogActivitySample) {
        self.request_count += 1;
        *self.exact_status_counts.entry(entry.status).or_insert(0) += 1;
        self.first_seen = Some(
            self.first_seen
                .map(|current| current.min(entry.timestamp_utc))
                .unwrap_or(entry.timestamp_utc),
        );
        self.last_seen = Some(
            self.last_seen
                .map(|current| current.max(entry.timestamp_utc))
                .unwrap_or(entry.timestamp_utc),
        );
        if self.samples.len() < 5 {
            self.samples.push(sample.clone());
        }
    }

    fn merge(&mut self, other: &Self) {
        self.request_count += other.request_count;
        for (status, count) in &other.exact_status_counts {
            *self.exact_status_counts.entry(*status).or_insert(0) += count;
        }
        if let Some(first_seen) = other.first_seen {
            self.first_seen = Some(
                self.first_seen
                    .map(|current| current.min(first_seen))
                    .unwrap_or(first_seen),
            );
        }
        if let Some(last_seen) = other.last_seen {
            self.last_seen = Some(
                self.last_seen
                    .map(|current| current.max(last_seen))
                    .unwrap_or(last_seen),
            );
        }
        self.samples.extend(other.samples.iter().cloned());
        self.samples.sort_by(sample_order);
        self.samples.truncate(5);
    }

    fn has_accessible_status(&self) -> bool {
        self.exact_status_counts
            .keys()
            .any(|status| (200..=399).contains(status))
    }

    fn all_statuses_blocked(&self) -> bool {
        !self.exact_status_counts.is_empty()
            && self
                .exact_status_counts
                .keys()
                .all(|status| is_blocked_status(*status))
    }
}

#[derive(Debug, Clone)]
struct AiFamilyStats {
    display_path: String,
    eligible_count: u64,
    blocked_count: u64,
    exact_status_counts: BTreeMap<u16, u64>,
    first_seen: Option<OffsetDateTime>,
    last_seen: Option<OffsetDateTime>,
    samples: Vec<LogActivitySample>,
}

impl AiFamilyStats {
    fn new(group: &ResolvedGroup) -> Self {
        Self {
            display_path: group.display_path.clone(),
            eligible_count: 0,
            blocked_count: 0,
            exact_status_counts: BTreeMap::new(),
            first_seen: None,
            last_seen: None,
            samples: Vec::new(),
        }
    }

    fn observe(&mut self, entry: &NormalizedLogEntry, sample: &LogActivitySample) {
        self.eligible_count += 1;
        if is_blocked_status(entry.status) {
            self.blocked_count += 1;
        }
        *self.exact_status_counts.entry(entry.status).or_insert(0) += 1;
        self.first_seen = Some(
            self.first_seen
                .map(|current| current.min(entry.timestamp_utc))
                .unwrap_or(entry.timestamp_utc),
        );
        self.last_seen = Some(
            self.last_seen
                .map(|current| current.max(entry.timestamp_utc))
                .unwrap_or(entry.timestamp_utc),
        );
        if self.samples.len() < 5 {
            self.samples.push(sample.clone());
        }
    }
}

struct LogFindingParts {
    finding_type: LogFindingType,
    normalized_url: Option<String>,
    group_key: String,
    display_path: String,
    join_state: Option<LogJoinState>,
    crawl_skip_reasons: Vec<CrawlSkipReason>,
    bot_family: Option<String>,
    client_type: Option<ClientType>,
    status: Option<String>,
    exact_status_counts: BTreeMap<u16, u64>,
    request_count: u64,
    first_seen_utc: Option<OffsetDateTime>,
    last_seen_utc: Option<OffsetDateTime>,
    related_context: BTreeMap<String, Value>,
    evidence: Vec<LogActivitySample>,
    message: String,
    raw_severity: Severity,
}

fn log_finding(parts: LogFindingParts) -> LogFinding {
    let id = stable_finding_id(
        parts.finding_type,
        parts.normalized_url.as_deref(),
        &parts.group_key,
        parts.bot_family.as_deref(),
        parts.client_type.map(ClientType::as_str),
        parts.status.as_deref(),
    );
    LogFinding {
        id,
        finding_type: parts.finding_type,
        normalized_url: parts.normalized_url,
        group_key: parts.group_key,
        display_path: parts.display_path,
        join_state: parts.join_state,
        crawl_skip_reasons: parts.crawl_skip_reasons,
        bot_family: parts.bot_family,
        client_type: parts.client_type,
        status: parts.status,
        exact_status_counts: parts.exact_status_counts,
        request_count: parts.request_count,
        first_seen_utc: parts.first_seen_utc,
        last_seen_utc: parts.last_seen_utc,
        related_context: parts.related_context,
        evidence: parts.evidence,
        message: parts.message,
        raw_severity: parts.raw_severity,
        matched_ignore_rule_indices: Vec::new(),
        effective_severity: Some(parts.raw_severity),
        provenance: LOG_FINDING_PROVENANCE.to_owned(),
    }
}

fn absence_finding(
    finding_type: LogFindingType,
    normalized_url: &str,
    group: &ResolvedGroup,
    join_state: LogJoinState,
    crawl_skip_reasons: Vec<CrawlSkipReason>,
    message: &str,
) -> LogFinding {
    let mut context = BTreeMap::new();
    context.insert("absence_assumption".to_owned(), Value::Bool(true));
    log_finding(LogFindingParts {
        finding_type,
        normalized_url: Some(normalized_url.to_owned()),
        group_key: group.group_key.clone(),
        display_path: group.display_path.clone(),
        join_state: Some(join_state),
        crawl_skip_reasons,
        bot_family: None,
        client_type: Some(ClientType::Search),
        status: None,
        exact_status_counts: BTreeMap::new(),
        request_count: 0,
        first_seen_utc: None,
        last_seen_utc: None,
        related_context: context,
        evidence: Vec::new(),
        message: message.to_owned(),
        raw_severity: Severity::Info,
    })
}

fn synthetic_entry(normalized_url: &str) -> NormalizedLogEntry {
    let parsed = Url::parse(normalized_url).expect("snapshot URL parses");
    NormalizedLogEntry {
        timestamp_utc: OffsetDateTime::UNIX_EPOCH,
        method: "GET".to_owned(),
        normalized_url: normalized_url.to_owned(),
        observed_origin: origin_from_url(&parsed),
        observed_host: None,
        observed_path: parsed.path().to_owned(),
        observed_raw_query: parsed.query().map(str::to_owned),
        raw_request_target: parsed.path().to_owned(),
        status: 0,
        origin_status: None,
        referer: None,
        bytes: None,
        cache_status: None,
        raw_user_agent: None,
        source_id: "synthetic".to_owned(),
        record_number: 0,
        matched_rule_id: None,
        bot_family: None,
        client_type: ClientType::Unknown,
    }
}

fn apply_severity_overrides(
    findings: &mut [LogFinding],
    overrides: &BTreeMap<LogFindingType, Severity>,
) {
    for finding in findings {
        if let Some(severity) = overrides.get(&finding.finding_type).copied() {
            finding.raw_severity = severity;
            finding.effective_severity = Some(severity);
        }
    }
}

fn apply_log_ignores(
    findings: &mut [LogFinding],
    rules: &[LogIgnoreRule],
) -> Result<(), LogAnalysisError> {
    for finding in findings {
        let matches = matching_log_ignore_indices(finding, rules)?;
        finding.matched_ignore_rule_indices = matches;
        if finding.matched_ignore_rule_indices.is_empty() {
            finding.effective_severity = Some(finding.raw_severity);
        } else {
            finding.effective_severity = None;
        }
    }
    Ok(())
}

fn matching_log_ignore_indices(
    finding: &LogFinding,
    rules: &[LogIgnoreRule],
) -> Result<Vec<usize>, LogAnalysisError> {
    let mut matches = Vec::new();
    for (index, rule) in rules.iter().enumerate() {
        validate_log_ignore_rule(rule)?;
        if log_ignore_matches(finding, rule)? {
            matches.push(index);
        }
    }
    Ok(matches)
}

fn log_ignore_matches(
    finding: &LogFinding,
    rule: &LogIgnoreRule,
) -> Result<bool, LogAnalysisError> {
    if rule
        .log_finding
        .is_some_and(|finding_type| finding_type != finding.finding_type)
    {
        return Ok(false);
    }
    if let Some(pattern) = &rule.url {
        let Some(url) = &finding.normalized_url else {
            return Ok(false);
        };
        if !url_pattern_matches(pattern, url)? {
            return Ok(false);
        }
    }
    if rule
        .url_group
        .as_ref()
        .is_some_and(|group| group != &finding.group_key)
    {
        return Ok(false);
    }
    if rule
        .bot_family
        .as_ref()
        .is_some_and(|family| finding.bot_family.as_ref() != Some(family))
    {
        return Ok(false);
    }
    if rule
        .client_type
        .is_some_and(|client_type| finding.client_type != Some(client_type))
    {
        return Ok(false);
    }
    if let Some(status) = &rule.status {
        let Some(finding_status) = &finding.status else {
            if finding
                .exact_status_counts
                .keys()
                .any(|code| status_matches(status, *code))
            {
                return Ok(true);
            }
            return Ok(false);
        };
        if status.ends_with("xx") {
            return Ok(status == finding_status);
        }
        let code = status.parse::<u16>().map_err(|_| {
            LogAnalysisError::InvalidReport(format!(
                "status {status:?} in a log ignore rule is not a status code"
            ))
        })?;
        if !finding.exact_status_counts.contains_key(&code) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn url_pattern_matches(pattern: &str, url: &str) -> Result<bool, LogAnalysisError> {
    validate_ignore_url(pattern)?;
    let parsed = Url::parse(url).map_err(|error| {
        LogAnalysisError::InvalidReport(format!("a log finding has an invalid URL: {error}"))
    })?;
    if pattern.starts_with('/') {
        Ok(ScopePattern::compile(pattern).matches(&path_and_query(&parsed)))
    } else {
        Ok(ScopePattern::compile(pattern).matches(&normalize_url(&parsed)))
    }
}

fn validate_ignore_url(url: &str) -> Result<(), LogAnalysisError> {
    if url.is_empty() {
        return Err(LogAnalysisError::InvalidConfig(
            "url in a log ignore rule must not be empty".to_owned(),
        ));
    }
    if url.starts_with('/') {
        return Ok(());
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let parsed = Url::parse(url).map_err(|error| {
            LogAnalysisError::InvalidConfig(format!(
                "url {url:?} in a log ignore rule is not a valid URL: {error}"
            ))
        })?;
        let normalized = normalize_url(&parsed);
        if normalized != url {
            return Err(LogAnalysisError::InvalidConfig(format!(
                "write the url {url:?} in this log ignore rule as {normalized:?}"
            )));
        }
        return Ok(());
    }
    Err(LogAnalysisError::InvalidConfig(
        "url in a log ignore rule must be a path that starts with /, or a full http:// or https:// URL"
            .to_owned(),
    ))
}

fn validate_status_selector(status: &str) -> Result<(), LogAnalysisError> {
    if matches!(status, "1xx" | "2xx" | "3xx" | "4xx" | "5xx") {
        return Ok(());
    }
    let code = status.parse::<u16>().map_err(|_| {
        LogAnalysisError::InvalidConfig(
            "status in a log ignore rule must be a status code such as 404 or a class such as 4xx"
                .to_owned(),
        )
    })?;
    if !(100..=599).contains(&code) {
        return Err(LogAnalysisError::InvalidConfig(
            "status in a log ignore rule must be between 100 and 599".to_owned(),
        ));
    }
    Ok(())
}

fn status_matches(selector: &str, status: u16) -> bool {
    if selector.ends_with("xx") {
        selector == status_class(status)
    } else {
        selector
            .parse::<u16>()
            .map(|selected| selected == status)
            .unwrap_or(false)
    }
}

fn data_state(summary: &LogImportSummary) -> LogDataState {
    if summary.selected_logical_source_count == 0 {
        LogDataState::NoSources
    } else if summary.total_records == summary.blank_records {
        LogDataState::Empty
    } else if summary.parsed_entries == 0 {
        LogDataState::NoValidEntries
    } else if summary.parse_error_count > 0 || summary.normalization_error_count > 0 {
        LogDataState::Partial
    } else {
        LogDataState::Ready
    }
}

fn absence_summary(
    enabled: bool,
    data_state: LogDataState,
    observed_time_range: Option<ObservedTimeRange>,
    search_attention_count: u64,
) -> LogAbsenceSummary {
    let mut reasons = Vec::new();
    if !enabled {
        reasons.push(AbsenceSuppressionReason::NotEnabled);
    } else {
        match data_state {
            LogDataState::NoSources => reasons.push(AbsenceSuppressionReason::NoSources),
            LogDataState::Empty => reasons.push(AbsenceSuppressionReason::Empty),
            LogDataState::NoValidEntries => reasons.push(AbsenceSuppressionReason::NoValidEntries),
            LogDataState::Partial => reasons.push(AbsenceSuppressionReason::PartialImport),
            LogDataState::Ready if search_attention_count == 0 => {
                reasons.push(AbsenceSuppressionReason::NoSearchAttention)
            }
            LogDataState::Ready => {}
        }
    }
    LogAbsenceSummary {
        absence_findings_enabled: enabled,
        absence_findings_eligible: enabled && reasons.is_empty(),
        absence_suppression_reasons: reasons,
        absence_assumption: enabled,
        observed_time_range,
    }
}

fn sample_from_entry(entry: &NormalizedLogEntry) -> LogActivitySample {
    LogActivitySample {
        source_id: entry.source_id.clone(),
        record_number: entry.record_number,
        timestamp_utc: entry.timestamp_utc,
        method: entry.method.clone(),
        status: entry.status,
        raw_user_agent: entry.raw_user_agent.clone(),
    }
}

fn sample_order(left: &LogActivitySample, right: &LogActivitySample) -> Ordering {
    (
        left.source_id.as_str(),
        left.record_number,
        left.timestamp_utc,
    )
        .cmp(&(
            right.source_id.as_str(),
            right.record_number,
            right.timestamp_utc,
        ))
}

fn status_class(status: u16) -> String {
    format!("{}xx", status / 100)
}

fn is_blocked_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 429)
}

fn first_segment_path(url: &Url) -> String {
    let path = url.path();
    let segment = path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    if segment.is_empty() {
        "/".to_owned()
    } else {
        format!("/{segment}")
    }
}

/// The first path segment as people read it: "/coffee/" for
/// "/coffee/house-blend/", "/coffee" for "/coffee".
fn section_display_path(url: &Url) -> String {
    let section = first_segment_path(url);
    if section != "/" && url.path().len() > section.len() {
        format!("{section}/")
    } else {
        section
    }
}

fn segment_prefix_matches(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn same_snapshot_boundary(start: &Url, candidate: &Url) -> bool {
    if start.scheme() != candidate.scheme() || start.port() != candidate.port() {
        return false;
    }
    let Some(start_host) = start.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    let Some(candidate_host) = candidate.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    strip_www(&start_host) == strip_www(&candidate_host)
}

fn strip_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

fn redirect_loop(record: &SeoRecord) -> bool {
    let mut seen = BTreeSet::new();
    if let Ok(url) = Url::parse(&record.url) {
        seen.insert(normalize_url(&url));
    }
    for RedirectHop { to, .. } in &record.redirect_chain {
        let normalized = Url::parse(to)
            .ok()
            .map(|url| normalize_url(&url))
            .unwrap_or_else(|| to.clone());
        if !seen.insert(normalized) {
            return true;
        }
    }
    false
}

fn path_and_query(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    }
}

fn stable_finding_id(
    finding_type: LogFindingType,
    url: Option<&str>,
    group: &str,
    family: Option<&str>,
    client: Option<&str>,
    status: Option<&str>,
) -> String {
    format!(
        "log/v1/{}|url={}|group={}|family={}|client={}|status={}",
        finding_type.as_str(),
        percent_encode(url.unwrap_or_default()),
        percent_encode(group),
        percent_encode(family.unwrap_or_default()),
        percent_encode(client.unwrap_or_default()),
        percent_encode(status.unwrap_or_default())
    )
}

fn percent_encode(input: &str) -> String {
    let mut output = String::new();
    for byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(*byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn sort_url_activity(rows: &mut [UrlActivityRow]) {
    rows.sort_by(|left, right| {
        (
            left.normalized_url.as_str(),
            left.client_type,
            BotFamilySort(left.bot_family.as_deref()),
            left.method.as_str(),
            left.status,
        )
            .cmp(&(
                right.normalized_url.as_str(),
                right.client_type,
                BotFamilySort(right.bot_family.as_deref()),
                right.method.as_str(),
                right.status,
            ))
    });
}

fn sort_group_activity(rows: &mut [GroupActivityRow]) {
    rows.sort_by(|left, right| {
        (
            left.group_key.as_str(),
            left.client_type,
            BotFamilySort(left.bot_family.as_deref()),
            left.method.as_str(),
            left.status_class.as_str(),
        )
            .cmp(&(
                right.group_key.as_str(),
                right.client_type,
                BotFamilySort(right.bot_family.as_deref()),
                right.method.as_str(),
                right.status_class.as_str(),
            ))
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BotFamilySort<'a>(Option<&'a str>);

impl Ord for BotFamilySort<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.0, other.0) {
            (Some(left), Some(right)) => left.cmp(right),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
}

impl PartialOrd for BotFamilySort<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn sort_findings(findings: &mut [LogFinding]) {
    findings.sort_by(finding_order);
}

fn finding_order(left: &LogFinding, right: &LogFinding) -> Ordering {
    let left_ignored = left.effective_severity.is_none();
    let right_ignored = right.effective_severity.is_none();
    left_ignored
        .cmp(&right_ignored)
        .then_with(|| {
            if !left_ignored && !right_ignored {
                right.effective_severity.cmp(&left.effective_severity)
            } else {
                right.raw_severity.cmp(&left.raw_severity)
            }
        })
        .then_with(|| left.finding_type.cmp(&right.finding_type))
        .then_with(|| {
            left.normalized_url
                .as_deref()
                .unwrap_or(&left.group_key)
                .cmp(right.normalized_url.as_deref().unwrap_or(&right.group_key))
        })
        .then_with(|| left.bot_family.cmp(&right.bot_family))
        .then_with(|| left.status.cmp(&right.status))
        .then_with(|| left.id.cmp(&right.id))
}

fn snapshot_metadata(snapshot: &Snapshot) -> SnapshotMetadata {
    SnapshotMetadata {
        start_url: snapshot.start_url.clone(),
        created_at: snapshot.created_at,
        auditroo_version: snapshot.auditroo_version.clone(),
        schema_version: snapshot.schema_version,
    }
}

fn gate_decision(effective: Option<Severity>, threshold: Severity) -> GateDecision {
    match effective {
        Some(severity) if severity >= threshold => GateDecision::Failed,
        _ => GateDecision::Passed,
    }
}

fn log_json_error(error: JsonParseError) -> LogAnalysisError {
    match error {
        JsonParseError::Malformed(detail) => LogAnalysisError::MalformedJson(detail),
        JsonParseError::DuplicateKey(key) => LogAnalysisError::DuplicateJsonKey(key),
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), LogAnalysisError> {
    let parent = crate::snapshot::parent_dir(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("log-report.json");
    let mut temp_path = None;
    let mut temp_file = None;
    for attempt in 0..1000u32 {
        let candidate = parent.join(format!(".{file_name}.tmp-{}-{attempt}", std::process::id()));
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp_path = Some(candidate);
                temp_file = Some(file);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let temp_path = temp_path.ok_or_else(|| {
        LogAnalysisError::Io("could not create a temporary file next to the report".to_owned())
    })?;
    let mut temp_file = temp_file.expect("temporary file was opened with its path");
    let result = (|| -> Result<(), LogAnalysisError> {
        temp_file.write_all(bytes)?;
        temp_file.flush()?;
        temp_file.sync_all()?;
        drop(temp_file);
        fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn gate_name(decision: GateDecision) -> &'static str {
    match decision {
        GateDecision::Passed => "passed",
        GateDecision::Failed => "failed",
    }
}

fn family_display_name(report: &LogAnalysisReport, family: &str) -> String {
    report
        .bot_catalog
        .rules
        .iter()
        .find(|rule| rule.family_id == family)
        .map(|rule| rule.display_name.clone())
        .unwrap_or_else(|| family.to_owned())
}

fn human_log_finding(finding: &LogFinding, report: &LogAnalysisReport) -> String {
    let place = finding
        .normalized_url
        .as_deref()
        .and_then(|url| url::Url::parse(url).ok())
        .map(|url| crate::crawl::url::path_and_query(&url))
        .unwrap_or_else(|| finding.display_path.clone());
    let who = finding
        .bot_family
        .as_deref()
        .map(|family| family_display_name(report, family))
        .unwrap_or_else(|| "bots".to_owned());
    format!(
        "{place}: {} ({who}, {} request{}, {})",
        finding.message,
        thousands(finding.request_count),
        if finding.request_count == 1 { "" } else { "s" },
        finding.finding_type.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bots::BotCatalog;

    #[test]
    fn finding_type_names_match_in_json_and_text() {
        for finding_type in LogFindingType::all() {
            let json = serde_json::to_string(&finding_type).unwrap();
            assert_eq!(json, format!("\"{}\"", finding_type.as_str()));
            assert_eq!(
                finding_type.as_str().parse::<LogFindingType>(),
                Ok(finding_type)
            );
        }
    }
    use crate::crawl::RobotsFile;
    use crate::extract::{Headings, Indexability, SeoRecord};
    use crate::graph::LinkGraph;
    use crate::logs::{resolve_log_sources, LogFormat, LogSourceDefinition};
    use crate::sitemap::{SitemapDiscovery, SitemapDiscoveryStatus};
    use crate::snapshot::{assemble, CrawlObservation, SnapshotInput};

    fn snapshot() -> Snapshot {
        let start = "https://example.test/";
        let mut records = Vec::new();
        for (url, redirects) in [
            (start, Vec::new()),
            ("https://example.test/docs", Vec::new()),
            ("https://example.test/orphan", Vec::new()),
            (
                "https://example.test/redirected",
                vec![
                    RedirectHop {
                        from: "https://example.test/redirected".to_owned(),
                        status: 301,
                        to: "https://example.test/hop".to_owned(),
                    },
                    RedirectHop {
                        from: "https://example.test/hop".to_owned(),
                        status: 302,
                        to: "https://example.test/final".to_owned(),
                    },
                ],
            ),
        ] {
            records.push(SeoRecord {
                url: url.to_owned(),
                final_url: url.to_owned(),
                content_type: Some("text/html".to_owned()),
                indexability: Indexability {
                    robots_meta: Vec::new(),
                    googlebot_meta: Vec::new(),
                    x_robots_tag: Vec::new(),
                    is_indexable: true,
                },
                canonical: None,
                title: None,
                meta_description: None,
                headings: Headings::default(),
                structured_data: Vec::new(),
                hreflang: Vec::new(),
                http_status: 200,
                redirect_chain: redirects,
                rendered: false,
            });
        }
        let graph: LinkGraph = [
            (
                start.to_owned(),
                vec!["https://example.test/docs".to_owned()],
            ),
            ("https://example.test/docs".to_owned(), Vec::new()),
            ("https://example.test/orphan".to_owned(), Vec::new()),
            ("https://example.test/redirected".to_owned(), Vec::new()),
        ]
        .into_iter()
        .collect();
        assemble(
            SnapshotInput {
                start_url: start.to_owned(),
                production_urls: Vec::new(),
                crawl_observation: CrawlObservation::default(),
                robots: [(
                    "https://example.test".to_owned(),
                    RobotsFile {
                        url: "https://example.test/robots.txt".to_owned(),
                        status: Some(404),
                        body: None,
                        error: None,
                    },
                )]
                .into_iter()
                .collect(),
                records,
                sitemap: SitemapDiscovery {
                    status: SitemapDiscoveryStatus::Complete,
                    documents: Vec::new(),
                    declared_urls: vec![
                        start.to_owned(),
                        "https://example.test/missing-sitemap".to_owned(),
                    ],
                },
                link_graph: graph,
                crawl_failures: Vec::new(),
            },
            OffsetDateTime::UNIX_EPOCH,
            None,
        )
        .expect("snapshot assembles")
    }

    fn analyze_combined_log(contents: &str, settings: LogAnalysisSettings) -> LogAnalysisReport {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("access.log");
        fs::write(&path, contents).expect("log writes");
        let sources = resolve_log_sources(&[LogSourceDefinition {
            id: Some("edge".to_owned()),
            path,
            display_path: "access.log".to_owned(),
            format: LogFormat::Combined,
            origin: "https://example.test".to_owned(),
            field_map: None,
        }])
        .expect("sources resolve");
        analyze_log_sources(&snapshot(), &sources, &BotCatalog::builtin(), &settings)
            .expect("analysis succeeds")
    }

    #[test]
    fn produces_positive_and_absence_findings_from_imported_logs() {
        let report = analyze_combined_log(
            "\
203.0.113.1 - - [10/Oct/2000:13:55:36 -0700] \"GET / HTTP/1.1\" 200 1 \"-\" \"Googlebot/2.1\"\n\
203.0.113.1 - - [10/Oct/2000:13:56:36 -0700] \"GET /orphan HTTP/1.1\" 403 1 \"-\" \"GPTBot/1.0\"\n\
203.0.113.1 - - [10/Oct/2000:13:57:36 -0700] \"GET /redirected HTTP/1.1\" 200 1 \"-\" \"GPTBot/1.0\"\n",
            LogAnalysisSettings {
                gate: true,
                absence_findings: true,
                fail_on: Severity::Medium,
                ..LogAnalysisSettings::default()
            },
        );

        assert_eq!(report.data_state, LogDataState::Ready);
        assert_eq!(report.gate.gate_decision, Some(GateDecision::Failed));
        assert!(report.findings.iter().any(|finding| {
            finding.finding_type == LogFindingType::AiBotBlocked
                && finding.group_key == "path:/orphan"
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.finding_type == LogFindingType::SitemapUrlNoObservedSearchHits
        }));
        let json = serde_json::to_string_pretty(&report).expect("json renders");
        assert_eq!(
            log_report_from_str(&(json + "\n"))
                .expect("report parses")
                .artifact_kind,
            LOG_ANALYSIS_REPORT_ARTIFACT_KIND
        );
    }

    #[test]
    fn human_report_starts_with_who_fetched_the_site() {
        let report = analyze_combined_log(
            "\
203.0.113.1 - - [10/Oct/2000:13:55:36 -0700] \"GET / HTTP/1.1\" 200 1 \"-\" \"Googlebot/2.1\"\n\
203.0.113.1 - - [10/Oct/2000:13:56:36 -0700] \"GET /docs HTTP/1.1\" 403 1 \"-\" \"GPTBot/1.0\"\n",
            LogAnalysisSettings::default(),
        );

        let rendered = render_log_human(&report);

        assert!(
            rendered.starts_with("Access logs: 2 requests from 1 file"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Search engines: 1 request\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Googlebot: 1 request to 1 URL, all answered"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "blocked on every request, so they cannot read your pages: GPTBot (OpenAI training)"
            ),
            "{rendered}"
        );
        assert!(rendered.contains("/docs: "), "{rendered}");
        assert!(!rendered.contains("group=path"), "{rendered}");
    }

    #[test]
    fn summary_names_ai_crawlers_blocked_on_part_of_the_site() {
        // GPTBot reads the home page but gets a 403 on /docs. It is not
        // blocked on every request, but the summary must still show /docs.
        let report = analyze_combined_log(
            "\
203.0.113.1 - - [10/Oct/2000:13:55:36 -0700] \"GET / HTTP/1.1\" 200 1 \"-\" \"GPTBot/1.0\"\n\
203.0.113.1 - - [10/Oct/2000:13:56:36 -0700] \"GET /docs HTTP/1.1\" 403 1 \"-\" \"GPTBot/1.0\"\n\
203.0.113.1 - - [10/Oct/2000:13:57:36 -0700] \"GET /docs HTTP/1.1\" 404 1 \"-\" \"Googlebot/2.1\"\n",
            LogAnalysisSettings::default(),
        );
        assert!(report
            .findings
            .iter()
            .any(|finding| finding.finding_type == LogFindingType::AiBotBlocked));
        let rendered = render_log_human(&report);
        assert!(!rendered.contains("No AI crawler request"), "{rendered}");
        assert!(
            rendered.contains("AI crawlers got blocked responses here:\n  /docs: GPTBot"),
            "{rendered}"
        );
        let findings = rendered.split("\nFindings\n").nth(1).unwrap();
        assert!(
            findings.lines().next().unwrap().contains("ai_bot_blocked"),
            "{rendered}"
        );
    }

    #[test]
    fn sections_keep_the_trailing_slash_and_one_error_is_singular() {
        // The URLs have a trailing slash, so the section is "/coffee/". A
        // "/coffee" next to the URL "/coffee/" would read like another URL.
        let report = analyze_combined_log(
            "\
203.0.113.1 - - [10/Oct/2000:13:55:36 -0700] \"GET /coffee/ HTTP/1.1\" 403 1 \"-\" \"GPTBot/1.0\"\n\
203.0.113.1 - - [10/Oct/2000:13:56:36 -0700] \"GET /coffee/house-blend/ HTTP/1.1\" 403 1 \"-\" \"GPTBot/1.0\"\n\
203.0.113.1 - - [10/Oct/2000:13:57:36 -0700] \"GET /coffee/ HTTP/1.1\" 200 1 \"-\" \"Googlebot/2.1\"\n\
203.0.113.1 - - [10/Oct/2000:13:58:36 -0700] \"GET /gone/ HTTP/1.1\" 404 1 \"-\" \"Googlebot/2.1\"\n",
            LogAnalysisSettings::default(),
        );
        let rendered = render_log_human(&report);
        assert!(
            rendered.contains("AI crawlers got blocked responses here:\n  /coffee/: GPTBot"),
            "{rendered}"
        );
        assert!(!rendered.contains("/coffee:"), "{rendered}");
        assert!(rendered.contains("2 URLs, 1 error\n"), "{rendered}");
        assert!(!rendered.contains("1 errors"), "{rendered}");
        // The grouping itself does not change: one section for both URLs.
        let blocked: Vec<&LogFinding> = report
            .findings
            .iter()
            .filter(|finding| finding.finding_type == LogFindingType::AiBotBlocked)
            .collect();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].group_key, "path:/coffee");
        assert_eq!(blocked[0].display_path, "/coffee/");
    }

    #[test]
    fn ai_search_access_diff_uses_only_shared_url_evidence() {
        let report = analyze_combined_log(
            "\
203.0.113.1 - - [10/Oct/2000:13:55:36 -0700] \"GET /docs HTTP/1.1\" 200 1 \"-\" \"Googlebot/2.1\"\n\
203.0.113.1 - - [10/Oct/2000:13:56:36 -0700] \"GET /docs HTTP/1.1\" 403 1 \"-\" \"GPTBot/1.0\"\n\
203.0.113.1 - - [10/Oct/2000:13:57:36 -0700] \"GET /docs/ai-only HTTP/1.1\" 429 1 \"-\" \"GPTBot/1.0\"\n",
            LogAnalysisSettings::default(),
        );

        let finding = report
            .findings
            .iter()
            .find(|finding| finding.finding_type == LogFindingType::AiBotAccessDiffersFromSearchBot)
            .expect("diff finding exists");
        assert_eq!(finding.request_count, 1);
        assert_eq!(
            finding.exact_status_counts,
            [(403u16, 1u64)].into_iter().collect()
        );
        assert_eq!(
            finding
                .evidence
                .iter()
                .map(|sample| sample.record_number)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(finding.related_context["shared_url_count"], json!(1));
        assert_eq!(
            finding.related_context["shared_urls"],
            json!(["https://example.test/docs"])
        );
        assert_eq!(finding.related_context["search_request_count"], json!(1));
        assert_eq!(
            finding.related_context["search_exact_status_counts"]["200"],
            json!(1)
        );
        assert!(finding.related_context["ai_exact_status_counts"]
            .get("429")
            .is_none());
    }
}
