use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Number, Value};
use url::Url;

use crate::adapters::{
    metric_unit, normalize_quality_template_url, sorted_unique_i32, QualitySourceKind,
    ResolvedQualityBudget, ResolvedQualitySettings, ResolvedQualitySource, ResolvedQualityTemplate,
    MAX_QUALITY_BUDGETS, MAX_QUALITY_SOURCES, MAX_QUALITY_TEMPLATES,
};
use crate::analysis::{
    validate_log_analysis_settings, LogAnalysisSettings, LogFindingType, LogIgnoreRule,
    UrlGroupRule,
};
use crate::assertions::{
    builtin_target_from_str, valid_json_pointer, validate_rule_id, AssertionCondition,
    AssertionOperator, AssertionTarget, BuiltinTarget, ExtractorDefinition, JsonPrimitiveType,
    JsonShapeRequirement, RegexSource, ResolvedAssertion, ResolvedExtractor, RuleScope,
    TargetShape, UrlPart, DEFAULT_EVIDENCE_CHARS, DEFAULT_MAX_SCALAR_CHARS, DEFAULT_MAX_VALUES,
    MAX_EVIDENCE_CHARS_LIMIT, MAX_SCALAR_CHARS_LIMIT, MAX_VALUES_LIMIT,
};
use crate::bots::{BotCatalog, BotCatalogError, BotRuleConfig, ClientType};
use crate::crawl::normalize_url;
use crate::external::{
    is_additive_impact_metric, is_supported_impact_metric, parse_impact_date,
    validate_impact_origin, ImpactPrimaryMetric, ImpactSort, ImpactSourceKind,
    ResolvedImpactSettings, ResolvedImpactSource, MAX_IMPACT_SOURCES,
};
use crate::findings::{Rule, Severity};
use crate::logs::{
    normalize_origin, resolve_log_sources, GenericJsonFieldMap, LogFormat, LogImportError,
    LogSourceDefinition, ResolvedLogSources,
};

const DEFAULT_CONFIG_PATH: &str = "auditroo.toml";
const DEFAULT_CONCURRENCY: usize = 8;
const DEFAULT_RATE_LIMIT_MS: u64 = 100;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_MAX_BODY_BYTES: usize = 10_485_760;
const DEFAULT_MAX_DEPTH: usize = 0;
const DEFAULT_MAX_PAGES: usize = 10_000;
const DEFAULT_RESPECT_ROBOTS: bool = true;
const DEFAULT_RENDER_JS: bool = false;
const DEFAULT_FAIL_ON: Severity = Severity::High;

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSettings {
    pub crawl: ResolvedCrawlSettings,
    pub gate: ResolvedGateSettings,
    pub extractors: Vec<ResolvedExtractor>,
    pub assertions: Vec<ResolvedAssertion>,
    pub quality: ResolvedQualitySettings,
    pub impact: ResolvedImpactSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCrawlSettings {
    pub user_agent: Option<String>,
    pub concurrency: usize,
    pub rate_limit_ms: u64,
    pub timeout_ms: u64,
    pub max_body_bytes: usize,
    pub max_depth: usize,
    pub max_pages: usize,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub respect_robots: bool,
    pub render_js: bool,
    pub headers: Vec<ResolvedHeader>,
    /// Sitemap URLs to read instead of the ones robots.txt names.
    pub sitemaps: Vec<String>,
    /// The live site that crawled previews and local builds stand in for.
    /// Its URLs count as the crawled site's own.
    pub production_url: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedHeader {
    pub name: String,
    pub value: String,
}

impl fmt::Debug for ResolvedHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedHeader")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedGateSettings {
    pub fail_on: Severity,
    pub ignore: Vec<IgnoreRule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLogSettings {
    pub sources: ResolvedLogSources,
    pub bot_catalog: BotCatalog,
    pub analysis: LogAnalysisSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitoringCatalog {
    pub config_path: Option<PathBuf>,
    pub config_dir: PathBuf,
    pub history: MonitoringHistoryConfig,
    pub bundle: MonitoringBundleConfig,
    pub sites: Vec<MonitoringSite>,
    pub notifications: Vec<NotificationHookConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitoringHistoryConfig {
    pub database: PathBuf,
    pub artifact_root: PathBuf,
    pub baseline_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitoringBundleConfig {
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitoringSite {
    pub id: String,
    pub start_url: String,
    pub config: Option<PathBuf>,
    pub config_display: Option<String>,
    pub notify: Vec<String>,
    pub baseline: MonitoringBaselineConfig,
    pub bundle: Option<MonitoringSiteBundleConfig>,
    pub semantic_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitoringBaselineConfig {
    Label {
        label: String,
        bootstrap_path: Option<PathBuf>,
        bootstrap_display: Option<String>,
    },
    Fixed {
        path: PathBuf,
        display_path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitoringSiteBundleConfig {
    pub triggers: Vec<DeliveryTrigger>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationHookConfig {
    pub id: String,
    pub kind: NotificationHookKind,
    pub required: bool,
    pub triggers: Vec<NotificationTrigger>,
    pub min_effective_severity: Option<Severity>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationHookKind {
    Command {
        argv: Vec<String>,
    },
    Webhook {
        url: String,
        secret_env: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationTrigger {
    GateFailed,
    OperationalError,
    MinEffectiveSeverity,
}

impl NotificationTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GateFailed => "gate_failed",
            Self::OperationalError => "operational_error",
            Self::MinEffectiveSeverity => "min_effective_severity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryTrigger {
    Passed,
    GateFailed,
    OperationalError,
    MinEffectiveSeverity,
}

impl DeliveryTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::GateFailed => "gate_failed",
            Self::OperationalError => "operational_error",
            Self::MinEffectiveSeverity => "min_effective_severity",
        }
    }
}

/// A `[gate] ignore` entry or an `--ignore` flag. Every part that is set
/// must match for a finding to be ignored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct IgnoreRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion: Option<String>,
    pub source: IgnoreRuleSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoreRuleSource {
    Config,
    Cli,
}

impl IgnoreRuleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Cli => "cli",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsOverrides {
    pub user_agent: Option<String>,
    pub concurrency: Option<usize>,
    pub rate_limit_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub max_body_bytes: Option<usize>,
    pub max_depth: Option<usize>,
    pub max_pages: Option<usize>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub respect_robots: Option<bool>,
    pub render_js: Option<bool>,
    pub headers: Option<Vec<String>>,
    pub sitemaps: Option<Vec<String>>,
    pub production_url: Option<String>,
    pub fail_on: Option<Severity>,
    pub ignore: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogSettingsOverrides {
    pub cli_sources: Option<LogCliSources>,
    pub gate: Option<bool>,
    pub fail_on: Option<Severity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogCliSources {
    pub paths: Vec<String>,
    pub format: LogFormat,
    pub origin: String,
    pub field_map: Option<GenericJsonFieldMap>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Io(String),
    MissingExplicitConfig(String),
    MalformedToml(String),
    InvalidValue(String),
    InvalidHeader(String),
    MissingEnvironmentVariable(String),
    NonUnicodeEnvironmentVariable(String),
    MalformedInterpolation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(detail) => write!(formatter, "{detail}"),
            Self::MissingExplicitConfig(path) => {
                write!(formatter, "{path}: the config file does not exist")
            }
            Self::MalformedToml(detail) => write!(formatter, "{detail}"),
            Self::InvalidValue(detail) => write!(formatter, "{detail}"),
            Self::InvalidHeader(detail) => write!(formatter, "{detail}"),
            Self::MissingEnvironmentVariable(name) => {
                write!(
                    formatter,
                    "a header uses ${{{name}}}, but the environment variable {name} is not set"
                )
            }
            Self::NonUnicodeEnvironmentVariable(name) => {
                write!(
                    formatter,
                    "the environment variable {name} is not valid Unicode"
                )
            }
            Self::MalformedInterpolation(detail) => {
                write!(
                    formatter,
                    "a header value has a broken ${{...}} part: {detail}"
                )
            }
        }
    }
}

impl Error for ConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentError {
    Missing,
    NotUnicode,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    crawl: Option<FileCrawl>,
    gate: Option<FileGate>,
    logs: Option<FileLogs>,
    quality: Option<FileQuality>,
    impact: Option<FileImpact>,
    history: Option<FileHistory>,
    bundle: Option<FileBundle>,
    #[serde(default, rename = "extract")]
    extractors: Vec<FileExtractor>,
    #[serde(default, rename = "assert")]
    assertions: Vec<FileAssertion>,
    #[serde(default, rename = "site")]
    sites: Vec<FileMonitorSite>,
    #[serde(default, rename = "notify")]
    notifications: Vec<FileNotify>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileCrawl {
    user_agent: Option<String>,
    concurrency: Option<usize>,
    rate_limit_ms: Option<u64>,
    timeout_ms: Option<u64>,
    max_body_bytes: Option<usize>,
    max_depth: Option<usize>,
    max_pages: Option<usize>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    respect_robots: Option<bool>,
    render_js: Option<bool>,
    headers: Option<Vec<FileHeader>>,
    sitemaps: Option<Vec<String>>,
    production_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileHeader {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileGate {
    fail_on: Option<Severity>,
    ignore: Option<Vec<FileIgnoreRule>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIgnoreRule {
    url: Option<String>,
    rule: Option<String>,
    assertion: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileExtractor {
    id: String,
    kind: String,
    description: Option<String>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    max_values: Option<usize>,
    max_scalar_chars: Option<usize>,
    evidence_chars: Option<usize>,
    selector: Option<String>,
    attr: Option<String>,
    source: Option<String>,
    pattern: Option<String>,
    group: Option<usize>,
    schema_type: Option<String>,
    pointer: Option<String>,
    name: Option<String>,
    part: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAssertion {
    id: String,
    field: Option<String>,
    extractor: Option<String>,
    operator: String,
    severity: Severity,
    description: Option<String>,
    message: Option<String>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    expected: Option<toml::Value>,
    pattern: Option<String>,
    min: Option<toml::Value>,
    max: Option<toml::Value>,
    allowed_hosts: Option<Vec<String>>,
    required: Option<Vec<FileJsonShapeRequirement>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileJsonShapeRequirement {
    pointer: String,
    #[serde(rename = "type")]
    value_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileLogs {
    gate: Option<bool>,
    absence_findings: Option<bool>,
    #[serde(default, rename = "source")]
    sources: Vec<FileLogSource>,
    #[serde(default, rename = "bot_rule")]
    bot_rules: Vec<FileBotRule>,
    #[serde(default, rename = "group")]
    groups: Vec<FileLogGroup>,
    severity: Option<BTreeMap<String, Severity>>,
    ignore: Option<Vec<FileLogIgnoreRule>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileQuality {
    enabled: Option<bool>,
    gate: Option<bool>,
    #[serde(default, rename = "template")]
    templates: Vec<FileQualityTemplate>,
    #[serde(default, rename = "source")]
    sources: Vec<FileQualitySource>,
    #[serde(default, rename = "budget")]
    budgets: Vec<FileQualityBudget>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileQualityTemplate {
    id: String,
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileQualitySource {
    id: String,
    template: String,
    kind: String,
    required: Option<bool>,
    path: Option<String>,
    argv: Option<Vec<String>>,
    timeout_seconds: Option<u64>,
    accepted_exit_codes: Option<Vec<i32>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileQualityBudget {
    id: String,
    source: String,
    metric: String,
    min: Option<toml::Value>,
    max: Option<toml::Value>,
    severity: Severity,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileImpact {
    enabled: Option<bool>,
    sort: Option<String>,
    primary: Option<FileImpactPrimary>,
    #[serde(default, rename = "source")]
    sources: Vec<FileImpactSource>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileImpactPrimary {
    source: String,
    metric: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileImpactSource {
    id: String,
    kind: String,
    path: String,
    required: Option<bool>,
    period_start: Option<String>,
    period_end: Option<String>,
    origin: Option<String>,
    currency: Option<String>,
    field_map: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileHistory {
    database: Option<String>,
    artifact_root: Option<String>,
    baseline_root: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileBundle {
    root: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileMonitorSite {
    id: String,
    start_url: String,
    config: Option<String>,
    #[serde(default)]
    notify: Vec<String>,
    baseline: Option<FileBaseline>,
    bundle: Option<FileSiteBundle>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBaseline {
    mode: String,
    label: Option<String>,
    bootstrap_path: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSiteBundle {
    #[serde(default, rename = "on")]
    triggers: Vec<DeliveryTrigger>,
    required: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileNotify {
    id: String,
    kind: String,
    required: Option<bool>,
    #[serde(default)]
    triggers: Vec<NotificationTrigger>,
    min_effective_severity: Option<Severity>,
    timeout_seconds: Option<u64>,
    argv: Option<Vec<String>>,
    url: Option<String>,
    secret_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLogSource {
    id: Option<String>,
    path: String,
    format: LogFormat,
    origin: String,
    field_map: Option<GenericJsonFieldMap>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBotRule {
    id: String,
    pattern: String,
    family_id: String,
    display_name: String,
    client_type: ClientType,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLogGroup {
    id: String,
    prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLogIgnoreRule {
    log_finding: Option<String>,
    url: Option<String>,
    url_group: Option<String>,
    bot_family: Option<String>,
    client_type: Option<ClientType>,
    status: Option<String>,
    reason: Option<String>,
    field: Option<String>,
    assertion: Option<String>,
    extractor: Option<String>,
}

pub fn resolve_settings(
    config_path: Option<&Path>,
    overrides: SettingsOverrides,
) -> Result<ResolvedSettings, ConfigError> {
    resolve_settings_with_env(config_path, overrides, process_env)
}

pub fn resolve_log_settings(
    config_path: Option<&Path>,
    overrides: LogSettingsOverrides,
) -> Result<ResolvedLogSettings, ConfigError> {
    let (file, config_dir) = load_file_config_with_dir(config_path)?;
    let logs = file.logs.unwrap_or_default();
    let analysis = log_analysis_settings_from_file(
        &logs,
        file.gate.as_ref().and_then(|gate| gate.fail_on),
        overrides.gate,
        overrides.fail_on,
    )?;
    let bot_catalog = bot_catalog_from_file_rules(logs.bot_rules)?;
    let source_definitions = match overrides.cli_sources {
        Some(cli) => log_source_definitions_from_cli(cli)?,
        None => log_source_definitions_from_file(logs.sources, &config_dir)?,
    };
    let sources = if source_definitions.is_empty() {
        ResolvedLogSources::default()
    } else {
        resolve_log_sources(&source_definitions).map_err(log_import_config_error)?
    };

    Ok(ResolvedLogSettings {
        sources,
        bot_catalog,
        analysis,
    })
}

pub fn resolve_bot_catalog(config_path: Option<&Path>) -> Result<BotCatalog, ConfigError> {
    let file = load_file_config(config_path)?;
    bot_catalog_from_file_rules(file.logs.unwrap_or_default().bot_rules)
}

pub fn resolve_monitoring_catalog(
    config_path: Option<&Path>,
) -> Result<MonitoringCatalog, ConfigError> {
    let (file, config_dir, loaded_path) = load_monitoring_file_config(config_path)?;
    monitoring_catalog_from_file(file, config_dir, loaded_path)
}

pub fn select_monitoring_sites(
    catalog: &MonitoringCatalog,
    selector: &str,
) -> Result<Vec<MonitoringSite>, ConfigError> {
    let mut sites = if selector == "all" {
        catalog.sites.clone()
    } else {
        vec![catalog
            .sites
            .iter()
            .find(|site| site.id == selector)
            .cloned()
            .ok_or_else(|| {
                ConfigError::InvalidValue(format!("no [[site]] has the id {selector:?}"))
            })?]
    };
    sites.sort_by(|left, right| left.id.cmp(&right.id));

    Ok(sites)
}

pub fn validate_monitoring_label(label: &str) -> Result<(), ConfigError> {
    validate_baseline_label(label)
}

pub fn resolve_monitoring_site_settings(
    catalog: &MonitoringCatalog,
    site: &MonitoringSite,
) -> Result<ResolvedSettings, ConfigError> {
    match &site.config {
        Some(path) => resolve_settings(Some(path), SettingsOverrides::default()),
        None => resolve_settings(catalog.config_path.as_deref(), SettingsOverrides::default()),
    }
}

pub fn resolve_monitoring_site_log_settings(
    catalog: &MonitoringCatalog,
    site: &MonitoringSite,
) -> Result<Option<ResolvedLogSettings>, ConfigError> {
    let path = site.config.as_deref().or(catalog.config_path.as_deref());
    if !config_file_has_logs(path)? {
        return Ok(None);
    }
    resolve_log_settings(path, LogSettingsOverrides::default()).map(Some)
}

pub fn parse_cli_field_map(pairs: &[String]) -> Result<GenericJsonFieldMap, ConfigError> {
    let mut values = BTreeMap::new();
    for pair in pairs {
        let (field, key) = pair.split_once('=').ok_or_else(|| {
            ConfigError::InvalidValue(
                "write --field-map as auditroo-field=json-key, for example timestamp=ts".to_owned(),
            )
        })?;
        if field.is_empty() || key.is_empty() {
            return Err(ConfigError::InvalidValue(
                "field-map entries must not contain empty field names or JSON keys".to_owned(),
            ));
        }
        if values.insert(field.to_owned(), key.to_owned()).is_some() {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate field-map field {field:?}"
            )));
        }
    }

    let required = |values: &mut BTreeMap<String, String>, key: &str| {
        values.remove(key).ok_or_else(|| {
            ConfigError::InvalidValue(format!("--field-map needs an entry for {key}"))
        })
    };
    let optional = |values: &mut BTreeMap<String, String>, key: &str| values.remove(key);
    let map = GenericJsonFieldMap {
        timestamp: required(&mut values, "timestamp")?,
        method: required(&mut values, "method")?,
        status: required(&mut values, "status")?,
        url: optional(&mut values, "url"),
        path: optional(&mut values, "path"),
        user_agent: optional(&mut values, "user_agent"),
        referer: optional(&mut values, "referer"),
        host: optional(&mut values, "host"),
        bytes: optional(&mut values, "bytes"),
        origin_status: optional(&mut values, "origin_status"),
        cache_status: optional(&mut values, "cache_status"),
    };
    if let Some((unknown, _)) = values.into_iter().next() {
        return Err(ConfigError::InvalidValue(format!(
            "unknown generic_jsonl field-map field {unknown:?}"
        )));
    }
    crate::logs::validate_generic_field_map(&map).map_err(log_import_config_error)?;
    Ok(map)
}

pub fn resolve_settings_without_env_expansion(
    config_path: Option<&Path>,
    overrides: SettingsOverrides,
) -> Result<ResolvedSettings, ConfigError> {
    resolve_settings_with_env(config_path, overrides, |_| {
        Ok("AUDITROO_ENV_PLACEHOLDER".to_owned())
    })
}

pub fn resolve_settings_with_env<F>(
    config_path: Option<&Path>,
    overrides: SettingsOverrides,
    mut env: F,
) -> Result<ResolvedSettings, ConfigError>
where
    F: FnMut(&str) -> Result<String, EnvironmentError>,
{
    let (file, config_dir) = load_file_config_with_dir(config_path)?;
    let mut settings = default_settings();
    apply_file_config(&mut settings, file, &config_dir)?;
    apply_overrides(&mut settings, overrides)?;

    for header in &mut settings.crawl.headers {
        header.value = expand_header_value(&header.value, &mut env)?;
    }

    validate_resolved_settings(&settings)?;
    Ok(settings)
}

pub fn validate_config_structure_for_copy(raw: &str) -> Result<(), ConfigError> {
    let file: FileConfig = toml::from_str(raw)
        .map_err(|error| ConfigError::MalformedToml(safe_toml_error(error, raw)))?;
    reject_duplicate_rule_ids_for_copy(&file)?;
    Ok(())
}

pub fn parse_header_line(line: &str) -> Result<ResolvedHeader, ConfigError> {
    let (name, value) = line.split_once(':').ok_or_else(|| {
        ConfigError::InvalidHeader(
            "a --header value has no colon. Write it as 'name: value'".to_owned(),
        )
    })?;
    let name = name.trim();
    if name.is_empty() {
        return Err(ConfigError::InvalidHeader(
            "a --header value has no name before the colon".to_owned(),
        ));
    }
    let value = value.trim();
    validate_header(name, value)?;
    Ok(ResolvedHeader {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

/// Parse one `--ignore` value, written as a TOML inline table such as
/// `{ url = "/promo/*", rule = "title_changed" }`.
pub fn parse_cli_ignore_rule(raw: &str) -> Result<IgnoreRule, ConfigError> {
    let wrapped = format!("rule = {raw}");
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wrapper {
        rule: FileIgnoreRule,
    }

    let wrapper: Wrapper = toml::from_str(&wrapped).map_err(|error| {
        ConfigError::InvalidValue(format!(
            "--ignore {raw}: {}. Example: --ignore '{{ url = \"/promo/*\", rule = \"title_changed\" }}'",
            error.message().trim()
        ))
    })?;
    file_ignore_rule(wrapper.rule, IgnoreRuleSource::Cli)
}

pub fn validate_ignore_rule_parts(
    url: Option<&str>,
    rule: Option<&str>,
    assertion: Option<&str>,
) -> Result<(), ConfigError> {
    if url.is_none() && rule.is_none() && assertion.is_none() {
        return Err(ConfigError::InvalidValue(
            "an ignore rule needs url, rule or assertion".to_owned(),
        ));
    }
    if let Some(url) = url {
        validate_ignore_url(url)?;
    }
    if let Some(rule) = rule {
        if Rule::from_id(rule).is_none() {
            return Err(ConfigError::InvalidValue(format!(
                "unknown rule {rule:?} in an ignore rule. docs/findings.md lists the rule ids"
            )));
        }
    }
    if let Some(assertion) = assertion {
        if !validate_rule_id(assertion) {
            return Err(ConfigError::InvalidValue(format!(
                "ignore rule assertion {assertion:?} is not a valid id"
            )));
        }
    }
    Ok(())
}

pub fn validate_start_url(input: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(input).map_err(|error| {
        ConfigError::InvalidValue(format!(
            "the start URL is not a full http:// or https:// URL ({error})"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::InvalidValue(
            "the start URL must start with http:// or https://".to_owned(),
        ));
    }
    if url.host_str().is_none() {
        return Err(ConfigError::InvalidValue(
            "the start URL has no host name".to_owned(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::InvalidValue(
            "the start URL must not contain a user name or password".to_owned(),
        ));
    }
    let normalized = normalize_url(&url);
    Url::parse(&normalized)
        .map_err(|error| ConfigError::InvalidValue(format!("the start URL is not valid: {error}")))
}

fn load_file_config(config_path: Option<&Path>) -> Result<FileConfig, ConfigError> {
    load_file_config_with_dir(config_path).map(|(file, _)| file)
}

fn load_file_config_with_dir(
    config_path: Option<&Path>,
) -> Result<(FileConfig, PathBuf), ConfigError> {
    let path = config_path.unwrap_or_else(|| Path::new(DEFAULT_CONFIG_PATH));
    if !path.exists() {
        if config_path.is_some() {
            return Err(ConfigError::MissingExplicitConfig(
                path.display().to_string(),
            ));
        }
        return Ok((FileConfig::default(), PathBuf::from(".")));
    }

    let raw = fs::read_to_string(path)
        .map_err(|error| ConfigError::Io(format!("{}: {error}", path.display())))?;
    let file = toml::from_str(&raw).map_err(|error| {
        ConfigError::MalformedToml(format!(
            "{}: {}",
            path.display(),
            safe_toml_error(error, &raw)
        ))
    })?;
    reject_recipe_placeholders(&raw, path)?;
    let config_dir = crate::snapshot::parent_dir(path).to_path_buf();
    Ok((file, config_dir))
}

/// Recipes contain values such as `PLACEHOLDER_PRODUCTION_HOST` that the
/// user has to replace. A check with a placeholder left in it would test the
/// wrong thing, for example a staging pattern that never matches.
fn reject_recipe_placeholders(raw: &str, path: &Path) -> Result<(), ConfigError> {
    fn find(value: &toml::Value) -> Option<String> {
        match value {
            toml::Value::String(text) => {
                let start = text.find("PLACEHOLDER_")?;
                let name: String = text[start..]
                    .chars()
                    .take_while(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || *ch == '_')
                    .collect();
                Some(name)
            }
            toml::Value::Array(items) => items.iter().find_map(find),
            toml::Value::Table(table) => table.values().find_map(find),
            _ => None,
        }
    }
    let Ok(value) = raw.parse::<toml::Value>() else {
        return Ok(());
    };
    match find(&value) {
        Some(name) => Err(ConfigError::InvalidValue(format!(
            "{}: replace {name} with your own value. Recipes leave these placeholders for you to fill in",
            path.display()
        ))),
        None => Ok(()),
    }
}

fn load_monitoring_file_config(
    config_path: Option<&Path>,
) -> Result<(FileConfig, PathBuf, Option<PathBuf>), ConfigError> {
    let path = config_path.unwrap_or_else(|| Path::new(DEFAULT_CONFIG_PATH));
    if !path.exists() {
        if config_path.is_some() {
            return Err(ConfigError::MissingExplicitConfig(
                path.display().to_string(),
            ));
        }
        return Ok((FileConfig::default(), PathBuf::from("."), None));
    }

    let raw = fs::read_to_string(path)
        .map_err(|error| ConfigError::Io(format!("{}: {error}", path.display())))?;
    let file = toml::from_str(&raw).map_err(|error| {
        ConfigError::MalformedToml(format!(
            "{}: {}",
            path.display(),
            safe_toml_error(error, &raw)
        ))
    })?;
    reject_recipe_placeholders(&raw, path)?;
    let config_dir = crate::snapshot::parent_dir(path).to_path_buf();
    Ok((file, config_dir, Some(path.to_path_buf())))
}

fn config_file_has_logs(config_path: Option<&Path>) -> Result<bool, ConfigError> {
    load_file_config(config_path).map(|file| file.logs.is_some())
}

fn monitoring_catalog_from_file(
    file: FileConfig,
    config_dir: PathBuf,
    config_path: Option<PathBuf>,
) -> Result<MonitoringCatalog, ConfigError> {
    let history_file = file.history.unwrap_or_default();
    let history = MonitoringHistoryConfig {
        database: resolve_declared_path(
            &config_dir,
            history_file
                .database
                .as_deref()
                .unwrap_or(".auditroo/history.sqlite3"),
        ),
        artifact_root: resolve_declared_path(
            &config_dir,
            history_file
                .artifact_root
                .as_deref()
                .unwrap_or(".auditroo/runs"),
        ),
        baseline_root: resolve_declared_path(
            &config_dir,
            history_file
                .baseline_root
                .as_deref()
                .unwrap_or(".auditroo/baselines"),
        ),
    };
    let bundle_file = file.bundle.unwrap_or_default();
    let bundle = MonitoringBundleConfig {
        root: resolve_declared_path(
            &config_dir,
            bundle_file.root.as_deref().unwrap_or(".auditroo/bundles"),
        ),
    };

    let mut seen_sites = BTreeSet::new();
    let mut sites = Vec::new();
    for site in file.sites {
        validate_site_id(&site.id, "site id")?;
        if !seen_sites.insert(site.id.clone()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate site id {:?}",
                site.id
            )));
        }
        let mut semantic_errors = Vec::new();
        if let Err(error) = validate_start_url(&site.start_url) {
            semantic_errors.push(error.to_string());
        }
        let config = site
            .config
            .as_ref()
            .map(|path| resolve_declared_path(&config_dir, path));
        for hook_id in &site.notify {
            if let Err(error) = validate_site_id(hook_id, "notification hook id") {
                semantic_errors.push(error.to_string());
            }
        }
        let notify = match sorted_unique(site.notify, "site notification hook") {
            Ok(notify) => notify,
            Err(error) => {
                semantic_errors.push(error.to_string());
                Vec::new()
            }
        };
        let baseline = match monitoring_baseline(site.baseline, &config_dir) {
            Ok(baseline) => baseline,
            Err(error) => {
                semantic_errors.push(error.to_string());
                default_monitoring_baseline()
            }
        };
        let bundle_policy = match site.bundle.map(monitoring_site_bundle).transpose() {
            Ok(bundle_policy) => bundle_policy,
            Err(error) => {
                semantic_errors.push(error.to_string());
                None
            }
        };
        sites.push(MonitoringSite {
            id: site.id,
            start_url: site.start_url,
            config,
            config_display: site.config,
            notify,
            baseline,
            bundle: bundle_policy,
            semantic_error: (!semantic_errors.is_empty()).then(|| semantic_errors.join("; ")),
        });
    }
    sites.sort_by(|left, right| left.id.cmp(&right.id));

    let mut seen_hooks = BTreeSet::new();
    let mut notifications = Vec::new();
    for hook in file.notifications {
        validate_site_id(&hook.id, "notification hook id")?;
        if !seen_hooks.insert(hook.id.clone()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate notification hook id {:?}",
                hook.id
            )));
        }
        notifications.push(notification_hook(hook)?);
    }

    Ok(MonitoringCatalog {
        config_path,
        config_dir,
        history,
        bundle,
        sites,
        notifications,
    })
}

fn resolve_declared_path(config_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}

fn monitoring_baseline(
    raw: Option<FileBaseline>,
    config_dir: &Path,
) -> Result<MonitoringBaselineConfig, ConfigError> {
    let Some(raw) = raw else {
        return Ok(default_monitoring_baseline());
    };
    match raw.mode.as_str() {
        "label" => {
            if raw.path.is_some() {
                return Err(ConfigError::InvalidValue(
                    "a baseline with mode = \"label\" cannot have a path".to_owned(),
                ));
            }
            let label = raw.label.unwrap_or_else(|| "last-known-good".to_owned());
            validate_baseline_label(&label)?;
            let bootstrap_path = raw
                .bootstrap_path
                .as_ref()
                .map(|path| resolve_declared_path(config_dir, path));
            Ok(MonitoringBaselineConfig::Label {
                label,
                bootstrap_path,
                bootstrap_display: raw.bootstrap_path,
            })
        }
        "fixed" => {
            if raw.label.is_some() || raw.bootstrap_path.is_some() {
                return Err(ConfigError::InvalidValue(
                    "a baseline with mode = \"fixed\" cannot have label or bootstrap_path"
                        .to_owned(),
                ));
            }
            let path = raw.path.ok_or_else(|| {
                ConfigError::InvalidValue(
                    "a baseline with mode = \"fixed\" needs a path".to_owned(),
                )
            })?;
            Ok(MonitoringBaselineConfig::Fixed {
                path: resolve_declared_path(config_dir, &path),
                display_path: path,
            })
        }
        _ => Err(ConfigError::InvalidValue(
            "baseline mode must be label or fixed".to_owned(),
        )),
    }
}

fn default_monitoring_baseline() -> MonitoringBaselineConfig {
    MonitoringBaselineConfig::Label {
        label: "last-known-good".to_owned(),
        bootstrap_path: None,
        bootstrap_display: None,
    }
}

fn monitoring_site_bundle(raw: FileSiteBundle) -> Result<MonitoringSiteBundleConfig, ConfigError> {
    if raw.triggers.is_empty() {
        return Err(ConfigError::InvalidValue(
            "a site's bundle needs at least one trigger in on".to_owned(),
        ));
    }
    Ok(MonitoringSiteBundleConfig {
        triggers: sorted_unique_copy(raw.triggers, "bundle trigger")?,
        required: raw.required.unwrap_or(false),
    })
}

fn notification_hook(raw: FileNotify) -> Result<NotificationHookConfig, ConfigError> {
    if raw.triggers.is_empty() {
        return Err(ConfigError::InvalidValue(
            "a notification hook needs at least one trigger".to_owned(),
        ));
    }
    if raw
        .triggers
        .contains(&NotificationTrigger::MinEffectiveSeverity)
        && raw.min_effective_severity.is_none()
    {
        return Err(ConfigError::InvalidValue(
            "the min_effective_severity trigger needs a min_effective_severity value".to_owned(),
        ));
    }
    let triggers = sorted_unique_copy(raw.triggers, "notification trigger")?;
    let timeout_seconds = raw.timeout_seconds.unwrap_or(20);
    if timeout_seconds == 0 || timeout_seconds > 300 {
        return Err(ConfigError::InvalidValue(
            "notification timeout_seconds must be between 1 and 300".to_owned(),
        ));
    }
    let kind = match raw.kind.as_str() {
        "command" => {
            let argv = raw
                .argv
                .ok_or_else(|| ConfigError::InvalidValue("a command hook needs argv".to_owned()))?;
            if argv.is_empty() || argv.iter().any(|arg| arg.is_empty()) {
                return Err(ConfigError::InvalidValue(
                    "the argv of a command hook must not be empty or contain empty strings"
                        .to_owned(),
                ));
            }
            if raw.url.is_some() || raw.secret_env.is_some() {
                return Err(ConfigError::InvalidValue(
                    "a command hook cannot have url or secret_env".to_owned(),
                ));
            }
            NotificationHookKind::Command { argv }
        }
        "webhook" => {
            let url = raw.url.ok_or_else(|| {
                ConfigError::InvalidValue("a webhook hook needs a url".to_owned())
            })?;
            let parsed = Url::parse(&url).map_err(|error| {
                ConfigError::InvalidValue(format!("the webhook url is not a valid URL ({error})"))
            })?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err(ConfigError::InvalidValue(
                    "the webhook url must be a full http:// or https:// URL".to_owned(),
                ));
            }
            if raw.argv.is_some() {
                return Err(ConfigError::InvalidValue(
                    "a webhook hook cannot have argv".to_owned(),
                ));
            }
            if let Some(secret_env) = &raw.secret_env {
                if !valid_env_name(secret_env) {
                    return Err(ConfigError::InvalidValue(
                        "webhook secret_env must be a valid environment variable name".to_owned(),
                    ));
                }
            }
            NotificationHookKind::Webhook {
                url,
                secret_env: raw.secret_env,
            }
        }
        _ => {
            return Err(ConfigError::InvalidValue(
                "notification hook kind must be command or webhook".to_owned(),
            ));
        }
    };
    Ok(NotificationHookConfig {
        id: raw.id,
        kind,
        required: raw.required.unwrap_or(false),
        triggers,
        min_effective_severity: raw.min_effective_severity,
        timeout_seconds,
    })
}

fn sorted_unique(values: Vec<String>, label: &str) -> Result<Vec<String>, ConfigError> {
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for value in values {
        if !seen.insert(value.clone()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate {label} {value:?}"
            )));
        }
        output.push(value);
    }
    output.sort();
    Ok(output)
}

fn sorted_unique_copy<T>(mut values: Vec<T>, label: &str) -> Result<Vec<T>, ConfigError>
where
    T: Ord + Copy + fmt::Debug,
{
    values.sort();
    for pair in values.windows(2) {
        if pair[0] == pair[1] {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate {label} {:?}",
                pair[0]
            )));
        }
    }
    Ok(values)
}

fn validate_site_id(id: &str, label: &str) -> Result<(), ConfigError> {
    if id.len() > 64 {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must be at most 64 characters"
        )));
    }
    let mut bytes = id.bytes();
    let Some(first) = bytes.next() else {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must not be empty"
        )));
    };
    if !first.is_ascii_lowercase() {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must match ^[a-z][a-z0-9-]{{0,63}}$"
        )));
    }
    if !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-') {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must match ^[a-z][a-z0-9-]{{0,63}}$"
        )));
    }
    Ok(())
}

fn validate_baseline_label(label: &str) -> Result<(), ConfigError> {
    if label.len() > 64 {
        return Err(ConfigError::InvalidValue(
            "baseline label must be at most 64 characters".to_owned(),
        ));
    }
    let mut bytes = label.bytes();
    let Some(first) = bytes.next() else {
        return Err(ConfigError::InvalidValue(
            "baseline label must not be empty".to_owned(),
        ));
    };
    if !(first.is_ascii_alphanumeric()) {
        return Err(ConfigError::InvalidValue(
            "baseline label must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$".to_owned(),
        ));
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) {
        return Err(ConfigError::InvalidValue(
            "baseline label must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$".to_owned(),
        ));
    }
    Ok(())
}

fn safe_toml_error(error: toml::de::Error, raw: &str) -> String {
    let mut detail = error.message().trim().to_owned();
    if let Some(span) = error.span() {
        let (line, column) = line_column(raw, span.start);
        detail.push_str(&format!(" at line {line}, column {column}"));
    }
    detail
}

fn line_column(raw: &str, offset: usize) -> (usize, usize) {
    let prefix = &raw[..offset.min(raw.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map(|(_, tail)| tail.chars().count() + 1)
        .unwrap_or_else(|| prefix.chars().count() + 1);
    (line, column)
}

/// The built-in settings, as if there were no auditroo.toml.
pub fn default_settings() -> ResolvedSettings {
    ResolvedSettings {
        crawl: ResolvedCrawlSettings {
            user_agent: None,
            concurrency: DEFAULT_CONCURRENCY,
            rate_limit_ms: DEFAULT_RATE_LIMIT_MS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_depth: DEFAULT_MAX_DEPTH,
            max_pages: DEFAULT_MAX_PAGES,
            include: Vec::new(),
            exclude: Vec::new(),
            respect_robots: DEFAULT_RESPECT_ROBOTS,
            render_js: DEFAULT_RENDER_JS,
            headers: Vec::new(),
            sitemaps: Vec::new(),
            production_url: None,
        },
        gate: ResolvedGateSettings {
            fail_on: DEFAULT_FAIL_ON,
            ignore: Vec::new(),
        },
        extractors: Vec::new(),
        assertions: Vec::new(),
        quality: ResolvedQualitySettings::default(),
        impact: ResolvedImpactSettings::default(),
    }
}

fn apply_file_config(
    settings: &mut ResolvedSettings,
    file: FileConfig,
    config_dir: &Path,
) -> Result<(), ConfigError> {
    if let Some(crawl) = file.crawl {
        if let Some(user_agent) = crawl.user_agent {
            settings.crawl.user_agent = (!user_agent.is_empty()).then_some(user_agent);
        }
        if let Some(concurrency) = crawl.concurrency {
            settings.crawl.concurrency = concurrency;
        }
        if let Some(rate_limit_ms) = crawl.rate_limit_ms {
            settings.crawl.rate_limit_ms = rate_limit_ms;
        }
        if let Some(timeout_ms) = crawl.timeout_ms {
            settings.crawl.timeout_ms = timeout_ms;
        }
        if let Some(max_body_bytes) = crawl.max_body_bytes {
            settings.crawl.max_body_bytes = max_body_bytes;
        }
        if let Some(max_depth) = crawl.max_depth {
            settings.crawl.max_depth = max_depth;
        }
        if let Some(max_pages) = crawl.max_pages {
            settings.crawl.max_pages = max_pages;
        }
        if let Some(include) = crawl.include {
            settings.crawl.include = include;
        }
        if let Some(exclude) = crawl.exclude {
            settings.crawl.exclude = exclude;
        }
        if let Some(respect_robots) = crawl.respect_robots {
            settings.crawl.respect_robots = respect_robots;
        }
        if let Some(render_js) = crawl.render_js {
            settings.crawl.render_js = render_js;
        }
        if let Some(headers) = crawl.headers {
            settings.crawl.headers = headers
                .into_iter()
                .map(file_header)
                .collect::<Result<Vec<_>, _>>()?;
        }
        if let Some(sitemaps) = crawl.sitemaps {
            settings.crawl.sitemaps = sitemaps;
        }
        if let Some(production_url) = crawl.production_url {
            settings.crawl.production_url = Some(production_url);
        }
    }

    if let Some(gate) = file.gate {
        if let Some(fail_on) = gate.fail_on {
            settings.gate.fail_on = fail_on;
        }
        if let Some(ignore) = gate.ignore {
            settings.gate.ignore = ignore
                .into_iter()
                .map(|rule| file_ignore_rule(rule, IgnoreRuleSource::Config))
                .collect::<Result<Vec<_>, _>>()?;
        }
    }

    settings.extractors = resolve_extractors(file.extractors, &settings.crawl.headers)?;
    settings.assertions = resolve_assertions(file.assertions, &settings.extractors)?;
    settings.quality = resolve_quality_settings(file.quality, config_dir)?;
    settings.impact = resolve_impact_settings(file.impact, config_dir)?;

    Ok(())
}

fn apply_overrides(
    settings: &mut ResolvedSettings,
    overrides: SettingsOverrides,
) -> Result<(), ConfigError> {
    if let Some(user_agent) = overrides.user_agent {
        settings.crawl.user_agent = (!user_agent.is_empty()).then_some(user_agent);
    }
    if let Some(concurrency) = overrides.concurrency {
        settings.crawl.concurrency = concurrency;
    }
    if let Some(rate_limit_ms) = overrides.rate_limit_ms {
        settings.crawl.rate_limit_ms = rate_limit_ms;
    }
    if let Some(timeout_ms) = overrides.timeout_ms {
        settings.crawl.timeout_ms = timeout_ms;
    }
    if let Some(max_body_bytes) = overrides.max_body_bytes {
        settings.crawl.max_body_bytes = max_body_bytes;
    }
    if let Some(max_depth) = overrides.max_depth {
        settings.crawl.max_depth = max_depth;
    }
    if let Some(max_pages) = overrides.max_pages {
        settings.crawl.max_pages = max_pages;
    }
    if let Some(include) = overrides.include {
        settings.crawl.include = include;
    }
    if let Some(exclude) = overrides.exclude {
        settings.crawl.exclude = exclude;
    }
    if let Some(respect_robots) = overrides.respect_robots {
        settings.crawl.respect_robots = respect_robots;
    }
    if let Some(render_js) = overrides.render_js {
        settings.crawl.render_js = render_js;
    }
    if let Some(headers) = overrides.headers {
        settings.crawl.headers = headers
            .iter()
            .map(|line| parse_header_line(line))
            .collect::<Result<Vec<_>, _>>()?;
    }
    if let Some(sitemaps) = overrides.sitemaps {
        settings.crawl.sitemaps = sitemaps;
    }
    if let Some(production_url) = overrides.production_url {
        settings.crawl.production_url = Some(production_url);
    }
    if let Some(fail_on) = overrides.fail_on {
        settings.gate.fail_on = fail_on;
    }
    // --ignore adds to the rules from auditroo.toml.
    if let Some(ignore) = overrides.ignore {
        for raw in &ignore {
            settings.gate.ignore.push(parse_cli_ignore_rule(raw)?);
        }
    }
    Ok(())
}

fn bot_catalog_from_file_rules(rules: Vec<FileBotRule>) -> Result<BotCatalog, ConfigError> {
    let rules = rules
        .into_iter()
        .map(|rule| BotRuleConfig {
            id: rule.id,
            family_id: rule.family_id,
            display_name: rule.display_name,
            client_type: rule.client_type,
            pattern: rule.pattern,
        })
        .collect::<Vec<_>>();
    BotCatalog::with_config_rules(rules).map_err(bot_catalog_config_error)
}

fn log_analysis_settings_from_file(
    logs: &FileLogs,
    gate_fail_on: Option<Severity>,
    gate_override: Option<bool>,
    fail_on_override: Option<Severity>,
) -> Result<LogAnalysisSettings, ConfigError> {
    let mut severity_overrides = BTreeMap::new();
    if let Some(severity) = &logs.severity {
        for (key, value) in severity {
            let finding_type = LogFindingType::from_str(key).map_err(|_| {
                ConfigError::InvalidValue(format!("unknown log severity key {key:?}"))
            })?;
            severity_overrides.insert(finding_type, *value);
        }
    }

    let settings = LogAnalysisSettings {
        gate: gate_override.unwrap_or(logs.gate.unwrap_or(false) || fail_on_override.is_some()),
        absence_findings: logs.absence_findings.unwrap_or(false),
        groups: logs
            .groups
            .iter()
            .map(|group| UrlGroupRule {
                id: group.id.clone(),
                prefix: group.prefix.clone(),
            })
            .collect(),
        severity_overrides,
        ignore_rules: logs
            .ignore
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(file_log_ignore_rule)
            .collect::<Result<Vec<_>, _>>()?,
        fail_on: fail_on_override.or(gate_fail_on).unwrap_or(DEFAULT_FAIL_ON),
    };
    validate_log_analysis_settings(&settings)
        .map_err(|error| ConfigError::InvalidValue(error.to_string()))?;
    Ok(settings)
}

fn log_source_definitions_from_file(
    sources: Vec<FileLogSource>,
    config_dir: &Path,
) -> Result<Vec<LogSourceDefinition>, ConfigError> {
    let mut seen_ids = BTreeSet::new();
    sources
        .into_iter()
        .map(|source| {
            if let Some(id) = &source.id {
                validate_config_id(id, "log source id")?;
                if !seen_ids.insert(id.clone()) {
                    return Err(ConfigError::InvalidValue(format!(
                        "duplicate log source id {id:?}"
                    )));
                }
            }
            normalize_origin(&source.origin).map_err(log_import_config_error)?;
            let path = PathBuf::from(&source.path);
            let resolved_path = if path.is_absolute() {
                path
            } else {
                config_dir.join(path)
            };
            Ok(LogSourceDefinition {
                id: source.id,
                path: resolved_path,
                display_path: source.path,
                format: source.format,
                origin: source.origin,
                field_map: source.field_map,
            })
        })
        .collect()
}

fn log_source_definitions_from_cli(
    cli: LogCliSources,
) -> Result<Vec<LogSourceDefinition>, ConfigError> {
    if cli.paths.is_empty() {
        return Err(ConfigError::InvalidValue(
            "add at least one --logs path".to_owned(),
        ));
    }
    normalize_origin(&cli.origin).map_err(log_import_config_error)?;
    match (cli.format, &cli.field_map) {
        (LogFormat::GenericJsonl, None) => {
            return Err(ConfigError::InvalidValue(
                "--log-format generic_jsonl needs --field-map".to_owned(),
            ));
        }
        (LogFormat::GenericJsonl, Some(map)) => {
            crate::logs::validate_generic_field_map(map).map_err(log_import_config_error)?;
        }
        (_, Some(_)) => {
            return Err(ConfigError::InvalidValue(
                "--field-map only works with --log-format generic_jsonl".to_owned(),
            ));
        }
        (_, None) => {}
    }
    Ok(cli
        .paths
        .into_iter()
        .map(|path| LogSourceDefinition {
            id: None,
            path: PathBuf::from(&path),
            display_path: path,
            format: cli.format,
            origin: cli.origin.clone(),
            field_map: cli.field_map.clone(),
        })
        .collect())
}

fn file_log_ignore_rule(rule: FileLogIgnoreRule) -> Result<LogIgnoreRule, ConfigError> {
    if rule.field.is_some() || rule.assertion.is_some() || rule.extractor.is_some() {
        return Err(ConfigError::InvalidValue(
            "ignore rules in [logs] cannot use field, assertion or extractor".to_owned(),
        ));
    }
    let log_finding = rule
        .log_finding
        .as_deref()
        .map(LogFindingType::from_str)
        .transpose()
        .map_err(|_| {
            ConfigError::InvalidValue("unknown log_finding in a [logs] ignore rule".to_owned())
        })?;
    let resolved = LogIgnoreRule {
        log_finding,
        url: rule.url,
        url_group: rule.url_group,
        bot_family: rule.bot_family,
        client_type: rule.client_type,
        status: rule.status,
        reason: rule.reason,
    };
    crate::analysis::validate_log_ignore_rule(&resolved)
        .map_err(|error| ConfigError::InvalidValue(error.to_string()))?;
    Ok(resolved)
}

fn bot_catalog_config_error(error: BotCatalogError) -> ConfigError {
    ConfigError::InvalidValue(error.to_string())
}

fn log_import_config_error(error: LogImportError) -> ConfigError {
    ConfigError::InvalidValue(error.to_string())
}

fn resolve_quality_settings(
    raw: Option<FileQuality>,
    config_dir: &Path,
) -> Result<ResolvedQualitySettings, ConfigError> {
    let Some(raw) = raw else {
        return Ok(ResolvedQualitySettings::default());
    };
    if raw.templates.len() > MAX_QUALITY_TEMPLATES {
        return Err(ConfigError::InvalidValue(format!(
            "auditroo.toml can have at most {MAX_QUALITY_TEMPLATES} quality templates"
        )));
    }
    if raw.sources.len() > MAX_QUALITY_SOURCES {
        return Err(ConfigError::InvalidValue(format!(
            "auditroo.toml can have at most {MAX_QUALITY_SOURCES} quality sources"
        )));
    }
    if raw.budgets.len() > MAX_QUALITY_BUDGETS {
        return Err(ConfigError::InvalidValue(format!(
            "auditroo.toml can have at most {MAX_QUALITY_BUDGETS} quality budgets"
        )));
    }

    let mut seen_templates = BTreeSet::new();
    let mut templates = Vec::new();
    let mut template_urls = BTreeMap::new();
    for template in raw.templates {
        validate_config_id(&template.id, "quality template id")?;
        if !seen_templates.insert(template.id.clone()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate quality template id {:?}",
                template.id
            )));
        }
        let url =
            normalize_quality_template_url(&template.url).map_err(ConfigError::InvalidValue)?;
        template_urls.insert(template.id.clone(), url.clone());
        templates.push(ResolvedQualityTemplate {
            id: template.id,
            url,
        });
    }

    let mut seen_sources = BTreeSet::new();
    let mut sources = Vec::new();
    let mut source_kinds = BTreeMap::new();
    for source in raw.sources {
        validate_config_id(&source.id, "quality source id")?;
        if !seen_sources.insert(source.id.clone()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate quality source id {:?}",
                source.id
            )));
        }
        validate_config_id(&source.template, "quality source template id")?;
        let template_url = template_urls
            .get(&source.template)
            .cloned()
            .ok_or_else(|| {
                ConfigError::InvalidValue(format!(
                    "quality source {:?} names the template {:?}, which does not exist",
                    source.id, source.template
                ))
            })?;
        let kind = QualitySourceKind::from_str(&source.kind)
            .map_err(|error| ConfigError::InvalidValue(error.to_string()))?;
        let path = match (kind.is_file(), source.path.as_ref()) {
            (true, Some(path)) => Some(resolve_declared_path(config_dir, path)),
            (true, None) => {
                return Err(ConfigError::InvalidValue(
                    "a quality source that reads a file needs a path".to_owned(),
                ));
            }
            (false, Some(_)) => {
                return Err(ConfigError::InvalidValue(
                    "a quality source that runs a command cannot have a path".to_owned(),
                ));
            }
            (false, None) => None,
        };
        let argv = match (kind.is_command(), source.argv) {
            (true, Some(argv)) if !argv.is_empty() && argv.iter().all(|arg| !arg.is_empty()) => {
                if !argv.iter().any(|arg| arg.contains("{template_url}")) {
                    return Err(ConfigError::InvalidValue(
                        "the argv of a quality command must contain {template_url}, which stands for the page URL"
                            .to_owned(),
                    ));
                }
                Some(argv)
            }
            (true, _) => {
                return Err(ConfigError::InvalidValue(
                    "a quality source that runs a command needs an argv without empty strings"
                        .to_owned(),
                ));
            }
            (false, Some(_)) => {
                return Err(ConfigError::InvalidValue(
                    "a quality source that reads a file cannot have argv".to_owned(),
                ));
            }
            (false, None) => None,
        };
        if kind.is_file()
            && (source.timeout_seconds.is_some() || source.accepted_exit_codes.is_some())
        {
            return Err(ConfigError::InvalidValue(
                "a quality source that reads a file cannot have timeout_seconds or accepted_exit_codes"
                    .to_owned(),
            ));
        }
        let timeout_seconds = source.timeout_seconds.unwrap_or(60);
        if !(1..=600).contains(&timeout_seconds) {
            return Err(ConfigError::InvalidValue(
                "quality timeout_seconds must be between 1 and 600".to_owned(),
            ));
        }
        let mut accepted_exit_codes = source.accepted_exit_codes.unwrap_or_else(|| vec![0]);
        accepted_exit_codes.sort_unstable();
        if !sorted_unique_i32(&accepted_exit_codes)
            || accepted_exit_codes
                .iter()
                .any(|code| !(0..=255).contains(code))
        {
            return Err(ConfigError::InvalidValue(
                "accepted_exit_codes must be different numbers from 0 to 255".to_owned(),
            ));
        }
        source_kinds.insert(source.id.clone(), kind);
        sources.push(ResolvedQualitySource {
            id: source.id,
            template_id: source.template,
            template_url,
            kind,
            adapter_kind: kind.adapter_kind(),
            // A source fails the run when it cannot be read, unless it says
            // `required = false`.
            required: source.required.unwrap_or(true),
            path,
            display_path: source.path,
            argv,
            timeout_seconds,
            accepted_exit_codes,
            declaring_dir: config_dir.to_path_buf(),
        });
    }

    let mut seen_budgets = BTreeSet::new();
    let mut budgets = Vec::new();
    for budget in raw.budgets {
        validate_config_id(&budget.id, "quality budget id")?;
        if !seen_budgets.insert(budget.id.clone()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate quality budget id {:?}",
                budget.id
            )));
        }
        validate_config_id(&budget.source, "quality budget source id")?;
        let source_kind = source_kinds.get(&budget.source).copied().ok_or_else(|| {
            ConfigError::InvalidValue(format!(
                "quality budget {:?} names the source {:?}, which does not exist",
                budget.id, budget.source
            ))
        })?;
        if metric_unit(&budget.metric).is_none() {
            return Err(ConfigError::InvalidValue(format!(
                "unknown quality metric {:?}",
                budget.metric
            )));
        }
        if !source_kind.supports_metric(&budget.metric) {
            return Err(ConfigError::InvalidValue(format!(
                "a source of kind {} has no metric {:?}",
                source_kind, budget.metric
            )));
        }
        let min = budget
            .min
            .as_ref()
            .map(|value| parse_numeric_bound(Some(value), "quality budget min"))
            .transpose()?;
        let max = budget
            .max
            .as_ref()
            .map(|value| parse_numeric_bound(Some(value), "quality budget max"))
            .transpose()?;
        if min.is_some() == max.is_some() {
            return Err(ConfigError::InvalidValue(
                "a quality budget needs exactly one of min and max".to_owned(),
            ));
        }
        budgets.push(ResolvedQualityBudget {
            id: budget.id,
            source_id: budget.source,
            metric_id: budget.metric,
            min,
            max,
            severity: budget.severity,
        });
    }
    templates.sort_by(|left, right| left.id.cmp(&right.id));
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    budgets.sort_by(|left, right| left.id.cmp(&right.id));

    let enabled = raw.enabled.unwrap_or(false);
    if enabled && (sources.is_empty() || budgets.is_empty()) {
        return Err(ConfigError::InvalidValue(
            "[quality] is enabled, so it needs at least one source and one budget".to_owned(),
        ));
    }
    if enabled && templates.is_empty() {
        return Err(ConfigError::InvalidValue(
            "[quality] is enabled, so it needs at least one template".to_owned(),
        ));
    }
    Ok(ResolvedQualitySettings {
        enabled,
        gate: raw.gate.unwrap_or(false),
        templates,
        sources,
        budgets,
    })
}

fn resolve_impact_settings(
    raw: Option<FileImpact>,
    config_dir: &Path,
) -> Result<ResolvedImpactSettings, ConfigError> {
    let Some(raw) = raw else {
        return Ok(ResolvedImpactSettings::default());
    };
    if raw.sources.len() > MAX_IMPACT_SOURCES {
        return Err(ConfigError::InvalidValue(format!(
            "auditroo.toml can have at most {MAX_IMPACT_SOURCES} impact sources"
        )));
    }
    let sort = raw
        .sort
        .as_deref()
        .map(ImpactSort::from_str)
        .transpose()
        .map_err(|error| ConfigError::InvalidValue(error.to_string()))?
        .unwrap_or(ImpactSort::Stable);

    let mut seen_sources = BTreeSet::new();
    let mut sources = Vec::new();
    let mut source_kinds = BTreeMap::new();
    for source in raw.sources {
        validate_config_id(&source.id, "impact source id")?;
        if !seen_sources.insert(source.id.clone()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate impact source id {:?}",
                source.id
            )));
        }
        let kind = ImpactSourceKind::from_str(&source.kind)
            .map_err(|error| ConfigError::InvalidValue(error.to_string()))?;
        let (period_start, period_end) = match (source.period_start, source.period_end) {
            (Some(start), Some(end)) => {
                let start = parse_impact_date(&start).map_err(ConfigError::InvalidValue)?;
                let end = parse_impact_date(&end).map_err(ConfigError::InvalidValue)?;
                if start > end {
                    return Err(ConfigError::InvalidValue(
                        "impact period_start must be on or before period_end".to_owned(),
                    ));
                }
                (Some(start), Some(end))
            }
            (None, None) => (None, None),
            _ => {
                return Err(ConfigError::InvalidValue(
                    "set both period_start and period_end, or neither".to_owned(),
                ));
            }
        };
        let origin = match (kind, source.origin.as_deref()) {
            (ImpactSourceKind::AnalyticsCsv, Some(origin)) => {
                Some(validate_impact_origin(origin).map_err(ConfigError::InvalidValue)?)
            }
            (ImpactSourceKind::AnalyticsCsv, None) => {
                return Err(ConfigError::InvalidValue(
                    "an analytics_csv source needs an origin".to_owned(),
                ));
            }
            (ImpactSourceKind::SearchConsoleCsv, Some(origin)) => {
                Some(validate_impact_origin(origin).map_err(ConfigError::InvalidValue)?)
            }
            (ImpactSourceKind::SearchConsoleCsv, None) => None,
        };
        let field_map = source.field_map.unwrap_or_default();
        validate_impact_field_map(kind, &field_map)?;
        if field_map.contains_key("revenue") {
            let currency = source.currency.as_deref().ok_or_else(|| {
                ConfigError::InvalidValue(
                    "an impact source that maps revenue needs a currency".to_owned(),
                )
            })?;
            validate_currency(currency)?;
        } else if source.currency.is_some() && kind != ImpactSourceKind::AnalyticsCsv {
            return Err(ConfigError::InvalidValue(
                "currency only works for analytics_csv sources that map revenue".to_owned(),
            ));
        }
        source_kinds.insert(source.id.clone(), kind);
        sources.push(ResolvedImpactSource {
            id: source.id,
            kind,
            path: resolve_declared_path(config_dir, &source.path),
            display_path: source.path,
            // A source fails the run when it cannot be read, unless it says
            // `required = false`.
            required: source.required.unwrap_or(true),
            period_start,
            period_end,
            origin,
            currency: source.currency,
            field_map,
        });
    }
    sources.sort_by(|left, right| left.id.cmp(&right.id));

    let primary = raw
        .primary
        .map(|primary| {
            validate_config_id(&primary.source, "impact primary source id")?;
            if !is_additive_impact_metric(&primary.metric) {
                return Err(ConfigError::InvalidValue(
                    "the impact primary metric must be one that adds up across URLs, such as clicks or sessions"
                        .to_owned(),
                ));
            }
            let kind = source_kinds.get(&primary.source).copied().ok_or_else(|| {
                ConfigError::InvalidValue(format!(
                    "impact primary names the source {:?}, which does not exist",
                    primary.source
                ))
            })?;
            if !is_supported_impact_metric(kind, &primary.metric) {
                return Err(ConfigError::InvalidValue(format!(
                    "a source of kind {} has no metric {:?}",
                    kind, primary.metric
                )));
            }
            Ok(ImpactPrimaryMetric {
                source: primary.source,
                metric: primary.metric,
            })
        })
        .transpose()?;
    if sort == ImpactSort::SeverityThenImpact && primary.is_none() {
        return Err(ConfigError::InvalidValue(
            "sort = \"severity_then_impact\" needs an impact primary metric".to_owned(),
        ));
    }
    let enabled = raw.enabled.unwrap_or(false);
    if enabled && sources.is_empty() {
        return Err(ConfigError::InvalidValue(
            "[impact] is enabled, so it needs at least one source".to_owned(),
        ));
    }
    Ok(ResolvedImpactSettings {
        enabled,
        sort,
        primary,
        sources,
    })
}

fn validate_impact_field_map(
    kind: ImpactSourceKind,
    field_map: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    if field_map.is_empty() {
        if kind == ImpactSourceKind::AnalyticsCsv {
            return Err(ConfigError::InvalidValue(
                "an analytics_csv source needs a field_map".to_owned(),
            ));
        }
        return Ok(());
    }
    if !field_map.contains_key("url") {
        return Err(ConfigError::InvalidValue(
            "the impact field_map needs a url column".to_owned(),
        ));
    }
    if field_map.keys().filter(|key| key.as_str() != "url").count() == 0 {
        return Err(ConfigError::InvalidValue(
            "the impact field_map needs at least one metric column".to_owned(),
        ));
    }
    let mut columns = BTreeSet::new();
    for (field, column) in field_map {
        let allowed = match kind {
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
            return Err(ConfigError::InvalidValue(format!(
                "unknown impact field_map key {field:?}"
            )));
        }
        if column.is_empty() || column.chars().count() > 1024 {
            return Err(ConfigError::InvalidValue(
                "impact field_map columns must be non-empty and at most 1024 characters".to_owned(),
            ));
        }
        if !columns.insert(column) {
            return Err(ConfigError::InvalidValue(
                "impact field_map columns must be unique".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_currency(currency: &str) -> Result<(), ConfigError> {
    if currency.len() == 3 && currency.bytes().all(|byte| byte.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(ConfigError::InvalidValue(
            "the impact currency must be an uppercase three-letter code such as EUR or USD"
                .to_owned(),
        ))
    }
}

fn file_header(header: FileHeader) -> Result<ResolvedHeader, ConfigError> {
    if header.name.trim() != header.name {
        return Err(ConfigError::InvalidHeader(format!(
            "the header name {:?} in [crawl] headers has spaces around it",
            header.name
        )));
    }
    validate_header(&header.name, &header.value)?;
    Ok(ResolvedHeader {
        name: header.name,
        value: header.value,
    })
}

fn validate_header(name: &str, value: &str) -> Result<(), ConfigError> {
    HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
        ConfigError::InvalidHeader(format!("{name:?} is not a valid HTTP header name"))
    })?;
    HeaderValue::from_str(value).map_err(|_| {
        ConfigError::InvalidHeader(format!(
            "the value of the header {name:?} contains a line break or another control character"
        ))
    })?;
    Ok(())
}

fn file_ignore_rule(
    rule: FileIgnoreRule,
    source: IgnoreRuleSource,
) -> Result<IgnoreRule, ConfigError> {
    validate_ignore_rule_parts(
        rule.url.as_deref(),
        rule.rule.as_deref(),
        rule.assertion.as_deref(),
    )?;
    Ok(IgnoreRule {
        url: rule.url,
        rule: rule.rule,
        assertion: rule.assertion,
        source,
    })
}

fn validate_ignore_url(url: &str) -> Result<(), ConfigError> {
    if url.is_empty() {
        return Err(ConfigError::InvalidValue(
            "an ignore rule has an empty url".to_owned(),
        ));
    }
    if url.starts_with('/') {
        return Ok(());
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let parsed = Url::parse(url).map_err(|error| {
            ConfigError::InvalidValue(format!(
                "the ignore rule url {url:?} is not a valid URL ({error})"
            ))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(ConfigError::InvalidValue(format!(
                "the ignore rule url {url:?} has no host name"
            )));
        }
        let written = normalize_url(&parsed);
        if written != url {
            return Err(ConfigError::InvalidValue(format!(
                "write the ignore rule url {url:?} as {written:?}, the way auditroo shows URLs"
            )));
        }
        return Ok(());
    }

    Err(ConfigError::InvalidValue(format!(
        "the ignore rule url {url:?} must be a path that starts with /, such as \"/blog/*\", or a full http:// or https:// URL"
    )))
}

fn resolve_extractors(
    raw_extractors: Vec<FileExtractor>,
    request_headers: &[ResolvedHeader],
) -> Result<Vec<ResolvedExtractor>, ConfigError> {
    if raw_extractors.len() > 256 {
        return Err(ConfigError::InvalidValue(
            "auditroo.toml can have at most 256 extractors".to_owned(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut extractors = Vec::new();
    for raw in raw_extractors {
        validate_config_id(&raw.id, "extractor id")?;
        if !seen.insert(raw.id.clone()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate extractor id {:?}",
                raw.id
            )));
        }
        let description = validate_optional_text(raw.description.clone(), "extractor description")?;
        let scope = resolve_scope(raw.include.clone(), raw.exclude.clone())?;
        let max_values = bounded_usize(
            raw.max_values.unwrap_or(DEFAULT_MAX_VALUES),
            1,
            MAX_VALUES_LIMIT,
            "extractor max_values",
        )?;
        let max_scalar_chars = bounded_usize(
            raw.max_scalar_chars.unwrap_or(DEFAULT_MAX_SCALAR_CHARS),
            1,
            MAX_SCALAR_CHARS_LIMIT,
            "extractor max_scalar_chars",
        )?;
        let evidence_chars = bounded_usize(
            raw.evidence_chars.unwrap_or(DEFAULT_EVIDENCE_CHARS),
            1,
            MAX_EVIDENCE_CHARS_LIMIT,
            "extractor evidence_chars",
        )?;
        let definition = resolve_extractor_definition(&raw, request_headers)?;
        extractors.push(
            ResolvedExtractor::new(
                raw.id,
                description,
                scope,
                max_values,
                max_scalar_chars,
                evidence_chars,
                definition,
            )
            .map_err(ConfigError::InvalidValue)?,
        );
    }
    extractors.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(extractors)
}

fn resolve_extractor_definition(
    raw: &FileExtractor,
    request_headers: &[ResolvedHeader],
) -> Result<ExtractorDefinition, ConfigError> {
    match raw.kind.as_str() {
        "css_text" => {
            require_only_extractor_fields(raw, &["selector"])?;
            let selector = required_bounded_string(raw.selector.as_deref(), "selector")?;
            validate_selector(&selector)?;
            Ok(ExtractorDefinition::CssText { selector })
        }
        "css_attr" => {
            require_only_extractor_fields(raw, &["selector", "attr"])?;
            let selector = required_bounded_string(raw.selector.as_deref(), "selector")?;
            validate_selector(&selector)?;
            let attr = required_bounded_string(raw.attr.as_deref(), "attr")?;
            Ok(ExtractorDefinition::CssAttr { selector, attr })
        }
        "css_count" => {
            require_only_extractor_fields(raw, &["selector"])?;
            let selector = required_bounded_string(raw.selector.as_deref(), "selector")?;
            validate_selector(&selector)?;
            Ok(ExtractorDefinition::CssCount { selector })
        }
        "regex" => {
            require_only_extractor_fields(raw, &["source", "pattern", "group"])?;
            let source = match required_bounded_string(raw.source.as_deref(), "source")?.as_str() {
                "body" => RegexSource::Body,
                "dom_text" => RegexSource::DomText,
                _ => {
                    return Err(ConfigError::InvalidValue(
                        "regex extractor source must be body or dom_text".to_owned(),
                    ));
                }
            };
            let pattern = required_bounded_string(raw.pattern.as_deref(), "pattern")?;
            let regex = regex::Regex::new(&pattern).map_err(|_| {
                ConfigError::InvalidValue("the extractor pattern is not a valid regex".to_owned())
            })?;
            let group = raw.group.unwrap_or(0);
            if group >= regex.captures_len() {
                return Err(ConfigError::InvalidValue(format!(
                    "the regex has no capture group {group}"
                )));
            }
            Ok(ExtractorDefinition::Regex {
                source,
                pattern,
                group,
            })
        }
        "json_ld" => {
            require_only_extractor_fields(raw, &["schema_type"])?;
            Ok(ExtractorDefinition::JsonLd {
                schema_type: optional_bounded_string(raw.schema_type.as_deref(), "schema_type")?,
            })
        }
        "json_pointer" => {
            require_only_extractor_fields(raw, &["pointer", "schema_type"])?;
            let pointer = required_bounded_string(raw.pointer.as_deref(), "pointer")?;
            if !valid_json_pointer(&pointer) {
                return Err(ConfigError::InvalidValue(
                    "the json_pointer pointer must be a JSON pointer that starts with /, such as /offers/price"
                        .to_owned(),
                ));
            }
            Ok(ExtractorDefinition::JsonPointer {
                pointer,
                schema_type: optional_bounded_string(raw.schema_type.as_deref(), "schema_type")?,
            })
        }
        "header" => {
            require_only_extractor_fields(raw, &["name"])?;
            let name = required_bounded_string(raw.name.as_deref(), "name")?;
            validate_response_header_extractor_name(&name, request_headers)?;
            Ok(ExtractorDefinition::Header {
                name: name.to_ascii_lowercase(),
            })
        }
        "url_part" => {
            require_only_extractor_fields(raw, &["part"])?;
            let part = match required_bounded_string(raw.part.as_deref(), "part")?.as_str() {
                "scheme" => UrlPart::Scheme,
                "host" => UrlPart::Host,
                "path" => UrlPart::Path,
                "query" => UrlPart::Query,
                "extension" => UrlPart::Extension,
                "normalized_url" => UrlPart::NormalizedUrl,
                _ => {
                    return Err(ConfigError::InvalidValue(
                        "part must be scheme, host, path, query, extension or normalized_url"
                            .to_owned(),
                    ));
                }
            };
            Ok(ExtractorDefinition::UrlPart { part })
        }
        other => Err(ConfigError::InvalidValue(format!(
            "unknown extractor kind {other:?}"
        ))),
    }
}

fn require_only_extractor_fields(raw: &FileExtractor, allowed: &[&str]) -> Result<(), ConfigError> {
    let checks = [
        ("selector", raw.selector.is_some()),
        ("attr", raw.attr.is_some()),
        ("source", raw.source.is_some()),
        ("pattern", raw.pattern.is_some()),
        ("group", raw.group.is_some()),
        ("schema_type", raw.schema_type.is_some()),
        ("pointer", raw.pointer.is_some()),
        ("name", raw.name.is_some()),
        ("part", raw.part.is_some()),
    ];
    for (field, present) in checks {
        if present && !allowed.contains(&field) {
            return Err(ConfigError::InvalidValue(format!(
                "field {field:?} is not valid for extractor kind {:?}",
                raw.kind
            )));
        }
    }
    Ok(())
}

fn resolve_assertions(
    raw_assertions: Vec<FileAssertion>,
    extractors: &[ResolvedExtractor],
) -> Result<Vec<ResolvedAssertion>, ConfigError> {
    if raw_assertions.len() > 512 {
        return Err(ConfigError::InvalidValue(
            "auditroo.toml can have at most 512 assertions".to_owned(),
        ));
    }
    let extractor_map = extractors
        .iter()
        .cloned()
        .map(|extractor| (extractor.id.clone(), extractor))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    let mut assertions = Vec::new();
    for raw in raw_assertions {
        validate_config_id(&raw.id, "assertion id")?;
        if !seen.insert(raw.id.clone()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate assertion id {:?}",
                raw.id
            )));
        }
        let description = validate_optional_text(raw.description.clone(), "assertion description")?;
        let message = validate_optional_text(raw.message.clone(), "assertion message")?;
        let scope = resolve_scope(raw.include.clone(), raw.exclude.clone())?;
        let target = resolve_assertion_target(&raw, &extractor_map)?;
        let operator = parse_operator(&raw.operator)?;
        let condition = resolve_assertion_condition(&raw, operator)?;
        let shape = match &target {
            AssertionTarget::Field(field) => crate::assertions::builtin_target_shape(field)
                .expect("known built-in target has a shape"),
            AssertionTarget::Extractor(id) => extractor_map
                .get(id)
                .map(ResolvedExtractor::output_shape)
                .ok_or_else(|| ConfigError::InvalidValue(format!("unknown extractor id {id:?}")))?,
        };
        validate_operator_compatibility(operator, &condition, &target, shape)?;
        assertions.push(ResolvedAssertion {
            id: raw.id,
            target,
            operator,
            severity: raw.severity,
            scope,
            description,
            message,
            condition,
        });
    }
    assertions.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(assertions)
}

fn resolve_assertion_target(
    raw: &FileAssertion,
    extractors: &std::collections::BTreeMap<String, ResolvedExtractor>,
) -> Result<AssertionTarget, ConfigError> {
    match (&raw.field, &raw.extractor) {
        (Some(_), Some(_)) => Err(ConfigError::InvalidValue(
            "an assertion can have field or extractor, but not both".to_owned(),
        )),
        (None, None) => Err(ConfigError::InvalidValue(
            "an assertion needs a field or an extractor".to_owned(),
        )),
        (Some(field), None) => {
            let field = bounded_string(field, "field")?;
            let target = builtin_target_from_str(&field).ok_or_else(|| {
                ConfigError::InvalidValue(format!("unknown assertion field target {field:?}"))
            })?;
            Ok(AssertionTarget::Field(target))
        }
        (None, Some(extractor)) => {
            validate_config_id(extractor, "assertion extractor target")?;
            if !extractors.contains_key(extractor) {
                return Err(ConfigError::InvalidValue(format!(
                    "unknown assertion extractor target {extractor:?}"
                )));
            }
            Ok(AssertionTarget::Extractor(extractor.clone()))
        }
    }
}

fn parse_operator(input: &str) -> Result<AssertionOperator, ConfigError> {
    Ok(match input {
        "exists" => AssertionOperator::Exists,
        "missing" => AssertionOperator::Missing,
        "equals" => AssertionOperator::Equals,
        "not_equals" => AssertionOperator::NotEquals,
        "contains" => AssertionOperator::Contains,
        "not_contains" => AssertionOperator::NotContains,
        "matches" => AssertionOperator::Matches,
        "not_matches" => AssertionOperator::NotMatches,
        "count_min" => AssertionOperator::CountMin,
        "count_max" => AssertionOperator::CountMax,
        "count_between" => AssertionOperator::CountBetween,
        "numeric_min" => AssertionOperator::NumericMin,
        "numeric_max" => AssertionOperator::NumericMax,
        "numeric_between" => AssertionOperator::NumericBetween,
        "url_same_host" => AssertionOperator::UrlSameHost,
        "url_allowed_host" => AssertionOperator::UrlAllowedHost,
        "json_shape" => AssertionOperator::JsonShape,
        other => {
            return Err(ConfigError::InvalidValue(format!(
                "unknown assertion operator {other:?}"
            )));
        }
    })
}

fn resolve_assertion_condition(
    raw: &FileAssertion,
    operator: AssertionOperator,
) -> Result<AssertionCondition, ConfigError> {
    match operator {
        AssertionOperator::Exists | AssertionOperator::Missing | AssertionOperator::UrlSameHost => {
            require_only_assertion_keys(raw, &[])?;
            Ok(AssertionCondition::None)
        }
        AssertionOperator::Equals
        | AssertionOperator::NotEquals
        | AssertionOperator::Contains
        | AssertionOperator::NotContains => {
            require_only_assertion_keys(raw, &["expected"])?;
            Ok(AssertionCondition::Expected(toml_value_to_json(
                raw.expected.as_ref().ok_or_else(|| {
                    ConfigError::InvalidValue(format!(
                        "the operator {} needs an expected value",
                        operator.as_str()
                    ))
                })?,
            )?))
        }
        AssertionOperator::Matches | AssertionOperator::NotMatches => {
            require_only_assertion_keys(raw, &["pattern"])?;
            let pattern = required_bounded_string(raw.pattern.as_deref(), "pattern")?;
            regex::Regex::new(&pattern).map_err(|_| {
                ConfigError::InvalidValue("the assertion pattern is not a valid regex".to_owned())
            })?;
            Ok(AssertionCondition::Pattern(pattern))
        }
        AssertionOperator::CountMin => {
            require_only_assertion_keys(raw, &["min"])?;
            Ok(AssertionCondition::CountMin {
                min: parse_count_bound(raw.min.as_ref(), "min")?,
            })
        }
        AssertionOperator::CountMax => {
            require_only_assertion_keys(raw, &["max"])?;
            Ok(AssertionCondition::CountMax {
                max: parse_count_bound(raw.max.as_ref(), "max")?,
            })
        }
        AssertionOperator::CountBetween => {
            require_only_assertion_keys(raw, &["min", "max"])?;
            let min = parse_count_bound(raw.min.as_ref(), "min")?;
            let max = parse_count_bound(raw.max.as_ref(), "max")?;
            if min > max {
                return Err(ConfigError::InvalidValue(
                    "count_between min must be <= max".to_owned(),
                ));
            }
            Ok(AssertionCondition::CountBetween { min, max })
        }
        AssertionOperator::NumericMin => {
            require_only_assertion_keys(raw, &["min"])?;
            Ok(AssertionCondition::NumericMin {
                min: parse_numeric_bound(raw.min.as_ref(), "min")?,
            })
        }
        AssertionOperator::NumericMax => {
            require_only_assertion_keys(raw, &["max"])?;
            Ok(AssertionCondition::NumericMax {
                max: parse_numeric_bound(raw.max.as_ref(), "max")?,
            })
        }
        AssertionOperator::NumericBetween => {
            require_only_assertion_keys(raw, &["min", "max"])?;
            let min = parse_numeric_bound(raw.min.as_ref(), "min")?;
            let max = parse_numeric_bound(raw.max.as_ref(), "max")?;
            if min > max {
                return Err(ConfigError::InvalidValue(
                    "numeric_between min must be <= max".to_owned(),
                ));
            }
            Ok(AssertionCondition::NumericBetween { min, max })
        }
        AssertionOperator::UrlAllowedHost => {
            require_only_assertion_keys(raw, &["allowed_hosts"])?;
            let hosts = raw
                .allowed_hosts
                .clone()
                .ok_or_else(|| ConfigError::InvalidValue("allowed_hosts is required".to_owned()))?;
            if hosts.is_empty() {
                return Err(ConfigError::InvalidValue(
                    "allowed_hosts must not be empty".to_owned(),
                ));
            }
            for host in &hosts {
                validate_allowed_host(host)?;
            }
            Ok(AssertionCondition::AllowedHosts(hosts))
        }
        AssertionOperator::JsonShape => {
            require_only_assertion_keys(raw, &["required"])?;
            let required = raw.required.as_ref().ok_or_else(|| {
                ConfigError::InvalidValue("json_shape needs a required list".to_owned())
            })?;
            if required.is_empty() {
                return Err(ConfigError::InvalidValue(
                    "json_shape required list must not be empty".to_owned(),
                ));
            }
            Ok(AssertionCondition::JsonShape {
                required: required
                    .iter()
                    .map(resolve_json_shape_requirement)
                    .collect::<Result<Vec<_>, _>>()?,
            })
        }
    }
}

fn require_only_assertion_keys(raw: &FileAssertion, allowed: &[&str]) -> Result<(), ConfigError> {
    let checks = [
        ("expected", raw.expected.is_some()),
        ("pattern", raw.pattern.is_some()),
        ("min", raw.min.is_some()),
        ("max", raw.max.is_some()),
        ("allowed_hosts", raw.allowed_hosts.is_some()),
        ("required", raw.required.is_some()),
    ];
    for (field, present) in checks {
        if present && !allowed.contains(&field) {
            return Err(ConfigError::InvalidValue(format!(
                "field {field:?} is not valid for assertion operator {:?}",
                raw.operator
            )));
        }
    }
    Ok(())
}

fn resolve_json_shape_requirement(
    raw: &FileJsonShapeRequirement,
) -> Result<JsonShapeRequirement, ConfigError> {
    let pointer = bounded_string(&raw.pointer, "json_shape pointer")?;
    if !valid_json_pointer(&pointer) {
        return Err(ConfigError::InvalidValue(
            "a json_shape pointer must be a JSON pointer that starts with /, such as /offers/price"
                .to_owned(),
        ));
    }
    let value_type = match raw.value_type.as_deref() {
        None => None,
        Some("string") => Some(JsonPrimitiveType::String),
        Some("number") => Some(JsonPrimitiveType::Number),
        Some("boolean") => Some(JsonPrimitiveType::Boolean),
        Some(_) => {
            return Err(ConfigError::InvalidValue(
                "json_shape type must be string, number or boolean".to_owned(),
            ));
        }
    };
    Ok(JsonShapeRequirement {
        pointer,
        value_type,
    })
}

fn validate_operator_compatibility(
    operator: AssertionOperator,
    condition: &AssertionCondition,
    target: &AssertionTarget,
    shape: TargetShape,
) -> Result<(), ConfigError> {
    if matches!(
        operator,
        AssertionOperator::Exists | AssertionOperator::Missing
    ) {
        return Ok(());
    }
    let compatible = match shape {
        TargetShape::OptionalString { url_valued } => {
            matches!(
                operator,
                AssertionOperator::Equals
                    | AssertionOperator::NotEquals
                    | AssertionOperator::Contains
                    | AssertionOperator::NotContains
                    | AssertionOperator::Matches
                    | AssertionOperator::NotMatches
            ) || (url_valued
                && matches!(
                    operator,
                    AssertionOperator::UrlSameHost | AssertionOperator::UrlAllowedHost
                ))
        }
        TargetShape::Boolean => matches!(
            operator,
            AssertionOperator::Equals | AssertionOperator::NotEquals
        ),
        TargetShape::Integer => matches!(
            operator,
            AssertionOperator::Equals
                | AssertionOperator::NotEquals
                | AssertionOperator::NumericMin
                | AssertionOperator::NumericMax
                | AssertionOperator::NumericBetween
        ),
        TargetShape::Count => matches!(
            operator,
            AssertionOperator::Equals
                | AssertionOperator::NotEquals
                | AssertionOperator::NumericMin
                | AssertionOperator::NumericMax
                | AssertionOperator::NumericBetween
                | AssertionOperator::CountMin
                | AssertionOperator::CountMax
                | AssertionOperator::CountBetween
        ),
        TargetShape::StringList { url_valued } => {
            matches!(
                operator,
                AssertionOperator::Equals
                    | AssertionOperator::NotEquals
                    | AssertionOperator::Contains
                    | AssertionOperator::NotContains
                    | AssertionOperator::Matches
                    | AssertionOperator::NotMatches
                    | AssertionOperator::CountMin
                    | AssertionOperator::CountMax
                    | AssertionOperator::CountBetween
            ) || (url_valued && operator == AssertionOperator::UrlAllowedHost)
        }
        TargetShape::JsonList => matches!(
            operator,
            AssertionOperator::Equals
                | AssertionOperator::NotEquals
                | AssertionOperator::Contains
                | AssertionOperator::NotContains
                | AssertionOperator::CountMin
                | AssertionOperator::CountMax
                | AssertionOperator::CountBetween
        ),
        TargetShape::JsonObjectList => matches!(
            operator,
            AssertionOperator::JsonShape
                | AssertionOperator::CountMin
                | AssertionOperator::CountMax
                | AssertionOperator::CountBetween
        ),
        TargetShape::RedirectChain => matches!(
            operator,
            AssertionOperator::CountMin
                | AssertionOperator::CountMax
                | AssertionOperator::CountBetween
                | AssertionOperator::UrlSameHost
                | AssertionOperator::UrlAllowedHost
        ),
    };
    if !compatible {
        return Err(ConfigError::InvalidValue(format!(
            "the operator {} does not work with this field or extractor",
            operator.as_str()
        )));
    }
    if operator == AssertionOperator::UrlSameHost
        && matches!(target, AssertionTarget::Field(BuiltinTarget::FinalUrl))
    {
        return Err(ConfigError::InvalidValue(
            "url_same_host is not valid for final_url".to_owned(),
        ));
    }
    if operator == AssertionOperator::JsonShape
        && !matches!(condition, AssertionCondition::JsonShape { .. })
    {
        return Err(ConfigError::InvalidValue(
            "json_shape assertion is missing shape requirements".to_owned(),
        ));
    }
    Ok(())
}

fn resolve_scope(
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
) -> Result<RuleScope, ConfigError> {
    if include.as_ref().is_some_and(Vec::is_empty) {
        return Err(ConfigError::InvalidValue(
            "include must not be an empty list".to_owned(),
        ));
    }
    if exclude.as_ref().is_some_and(Vec::is_empty) {
        return Err(ConfigError::InvalidValue(
            "exclude must not be an empty list".to_owned(),
        ));
    }
    let include = include.unwrap_or_else(|| vec!["/*".to_owned()]);
    let exclude = exclude.unwrap_or_default();
    validate_scope_patterns(&include, "include")?;
    validate_scope_patterns(&exclude, "exclude")?;
    Ok(RuleScope { include, exclude })
}

fn validate_scope_patterns(patterns: &[String], label: &str) -> Result<(), ConfigError> {
    for pattern in patterns {
        if pattern.is_empty() || !pattern.starts_with('/') {
            return Err(ConfigError::InvalidValue(format!(
                "{label} patterns in [[extract]] and [[assert]] must start with /"
            )));
        }
        if pattern.starts_with("http://") || pattern.starts_with("https://") {
            return Err(ConfigError::InvalidValue(
                "scope patterns must be path/query patterns, not absolute URLs".to_owned(),
            ));
        }
        bounded_string(pattern, "scope pattern")?;
    }
    Ok(())
}

fn validate_config_id(id: &str, label: &str) -> Result<(), ConfigError> {
    if validate_rule_id(id) {
        Ok(())
    } else {
        Err(ConfigError::InvalidValue(format!(
            "{label} {id:?} must start with a lowercase letter and use only a-z, 0-9, ., _ and -, at most 64 characters"
        )))
    }
}

fn validate_optional_text(
    value: Option<String>,
    label: &str,
) -> Result<Option<String>, ConfigError> {
    value
        .map(|value| bounded_single_line(value, label))
        .transpose()
}

fn bounded_single_line(value: String, label: &str) -> Result<String, ConfigError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must be a single line"
        )));
    }
    if value.chars().count() > 1024 {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must be at most 1024 characters"
        )));
    }
    Ok(value)
}

fn required_bounded_string(value: Option<&str>, label: &str) -> Result<String, ConfigError> {
    let value = value.ok_or_else(|| ConfigError::InvalidValue(format!("{label} is required")))?;
    bounded_string(value, label)
}

fn optional_bounded_string(
    value: Option<&str>,
    label: &str,
) -> Result<Option<String>, ConfigError> {
    value.map(|value| bounded_string(value, label)).transpose()
}

fn bounded_string(value: &str, label: &str) -> Result<String, ConfigError> {
    if value.chars().count() > 1024 {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must be at most 1024 characters"
        )));
    }
    Ok(value.to_owned())
}

fn bounded_usize(value: usize, min: usize, max: usize, label: &str) -> Result<usize, ConfigError> {
    if value < min || value > max {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

fn validate_selector(selector: &str) -> Result<(), ConfigError> {
    scraper::Selector::parse(selector)
        .map(|_| ())
        .map_err(|_| ConfigError::InvalidValue(format!("{selector:?} is not a valid CSS selector")))
}

fn validate_response_header_extractor_name(
    name: &str,
    request_headers: &[ResolvedHeader],
) -> Result<(), ConfigError> {
    HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
        ConfigError::InvalidHeader(format!(
            "the header extractor name {name:?} is not a valid HTTP header name ({error})"
        ))
    })?;
    let normalized = name.to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "set-cookie" | "cookie" | "authorization" | "proxy-authorization"
    ) {
        return Err(ConfigError::InvalidHeader(format!(
            "a header extractor cannot read {normalized}, because that header can hold secrets"
        )));
    }
    if request_headers
        .iter()
        .any(|header| header.name.eq_ignore_ascii_case(name))
    {
        return Err(ConfigError::InvalidHeader(format!(
            "a header extractor cannot read {name:?}, because auditroo sends that header itself"
        )));
    }
    Ok(())
}

fn validate_header_extractor_safety(
    extractors: &[ResolvedExtractor],
    request_headers: &[ResolvedHeader],
) -> Result<(), ConfigError> {
    for extractor in extractors {
        if let ExtractorDefinition::Header { name } = &extractor.definition {
            validate_response_header_extractor_name(name, request_headers)?;
        }
    }
    Ok(())
}

fn parse_count_bound(value: Option<&toml::Value>, label: &str) -> Result<usize, ConfigError> {
    let value = value.ok_or_else(|| ConfigError::InvalidValue(format!("{label} is required")))?;
    let Some(integer) = value.as_integer() else {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must be a non-negative integer"
        )));
    };
    usize::try_from(integer)
        .map_err(|_| ConfigError::InvalidValue(format!("{label} must be a non-negative integer")))
}

fn parse_numeric_bound(value: Option<&toml::Value>, label: &str) -> Result<f64, ConfigError> {
    let value = value.ok_or_else(|| ConfigError::InvalidValue(format!("{label} is required")))?;
    let number = match value {
        toml::Value::Integer(value) => *value as f64,
        toml::Value::Float(value) => *value,
        _ => {
            return Err(ConfigError::InvalidValue(format!(
                "{label} must be a number"
            )));
        }
    };
    if !number.is_finite() {
        return Err(ConfigError::InvalidValue(format!(
            "{label} must be a finite number"
        )));
    }
    Ok(number)
}

fn toml_value_to_json(value: &toml::Value) -> Result<Value, ConfigError> {
    Ok(match value {
        toml::Value::String(value) => Value::String(value.clone()),
        toml::Value::Integer(value) => Value::Number(Number::from(*value)),
        toml::Value::Float(value) => {
            Number::from_f64(*value).map(Value::Number).ok_or_else(|| {
                ConfigError::InvalidValue("expected must be a finite number".to_owned())
            })?
        }
        toml::Value::Boolean(value) => Value::Bool(*value),
        toml::Value::Array(items) => Value::Array(
            items
                .iter()
                .map(toml_value_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(key, value)| Ok((key.clone(), toml_value_to_json(value)?)))
                .collect::<Result<serde_json::Map<_, _>, ConfigError>>()?,
        ),
        toml::Value::Datetime(_) => {
            return Err(ConfigError::InvalidValue(
                "assertions cannot compare TOML dates and times".to_owned(),
            ));
        }
    })
}

fn validate_allowed_host(host: &str) -> Result<(), ConfigError> {
    if host.is_empty()
        || host.chars().any(|ch| {
            !(ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || matches!(ch, '.' | '-' | '[' | ']' | ':'))
        })
    {
        return Err(ConfigError::InvalidValue(
            "allowed_hosts must list host names in lowercase, such as www.example.com".to_owned(),
        ));
    }
    let parsed = Url::parse(&format!("http://{host}/")).map_err(|_| {
        ConfigError::InvalidValue(format!(
            "{host:?} in allowed_hosts is not a valid host name"
        ))
    })?;
    let written = parsed.host_str().map(str::to_ascii_lowercase);
    if written.as_deref() != Some(host) {
        return Err(ConfigError::InvalidValue(format!(
            "write {host:?} in allowed_hosts as {:?}",
            written.unwrap_or_default()
        )));
    }
    Ok(())
}

fn validate_live_ignore_references(
    rules: &[IgnoreRule],
    assertions: &[ResolvedAssertion],
    quality: &ResolvedQualitySettings,
) -> Result<(), ConfigError> {
    let known = assertions
        .iter()
        .map(|assertion| assertion.id.as_str())
        .chain(quality.budgets.iter().map(|budget| budget.id.as_str()))
        .collect::<BTreeSet<_>>();
    for rule in rules {
        if let Some(assertion_id) = &rule.assertion {
            if !known.contains(assertion_id.as_str()) {
                return Err(ConfigError::InvalidValue(format!(
                    "ignore rule names assertion {assertion_id:?}, but no [[assert]] or quality budget has that id"
                )));
            }
        }
    }
    Ok(())
}

fn reject_duplicate_rule_ids_for_copy(file: &FileConfig) -> Result<(), ConfigError> {
    if let Some(crawl) = &file.crawl {
        if let Some(headers) = &crawl.headers {
            for header in headers {
                file_header(header.clone())?;
                validate_interpolation_syntax(&header.value)?;
            }
        }
    }
    if let Some(gate) = &file.gate {
        if let Some(ignore) = &gate.ignore {
            for rule in ignore {
                validate_ignore_rule_parts(
                    rule.url.as_deref(),
                    rule.rule.as_deref(),
                    rule.assertion.as_deref(),
                )?;
            }
        }
    }
    let mut extractors = BTreeSet::new();
    for extractor in &file.extractors {
        if !extractors.insert(extractor.id.as_str()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate extractor id {:?}",
                extractor.id
            )));
        }
    }
    let mut assertions = BTreeSet::new();
    for assertion in &file.assertions {
        if !assertions.insert(assertion.id.as_str()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate assertion id {:?}",
                assertion.id
            )));
        }
    }
    Ok(())
}

fn validate_interpolation_syntax(value: &str) -> Result<(), ConfigError> {
    fn placeholder_env(_: &str) -> Result<String, EnvironmentError> {
        Ok(String::new())
    }
    let mut noop = placeholder_env;
    expand_header_value(value, &mut noop).map(|_| ())
}

fn validate_resolved_settings(settings: &ResolvedSettings) -> Result<(), ConfigError> {
    if settings.crawl.concurrency == 0 {
        return Err(ConfigError::InvalidValue(
            "concurrency must be at least 1".to_owned(),
        ));
    }
    if settings.crawl.timeout_ms == 0 {
        return Err(ConfigError::InvalidValue(
            "timeout_ms must be at least 1".to_owned(),
        ));
    }
    if settings.crawl.max_body_bytes == 0 {
        return Err(ConfigError::InvalidValue(
            "max_body_bytes must be at least 1".to_owned(),
        ));
    }

    if let Some(production_url) = &settings.crawl.production_url {
        validate_start_url(production_url).map_err(|_| {
            ConfigError::InvalidValue(format!(
                "production_url {production_url:?} is not a full URL. Write it with http:// or https://, for example \"https://www.example.com\""
            ))
        })?;
    }
    for (label, patterns) in [
        ("include", &settings.crawl.include),
        ("exclude", &settings.crawl.exclude),
    ] {
        for pattern in patterns.iter() {
            if !pattern.starts_with('/') && !pattern.starts_with('*') {
                return Err(ConfigError::InvalidValue(format!(
                    "the {label} pattern {pattern:?} never matches, because patterns are matched against the path and query. Start it with / or *, for example {:?}",
                    format!("/{pattern}")
                )));
            }
        }
    }
    for header in &settings.crawl.headers {
        validate_header(&header.name, &header.value)?;
    }
    for rule in &settings.gate.ignore {
        validate_ignore_rule_parts(
            rule.url.as_deref(),
            rule.rule.as_deref(),
            rule.assertion.as_deref(),
        )?;
    }
    validate_header_extractor_safety(&settings.extractors, &settings.crawl.headers)?;
    validate_live_ignore_references(
        &settings.gate.ignore,
        &settings.assertions,
        &settings.quality,
    )?;

    Ok(())
}

fn expand_header_value<F>(value: &str, env: &mut F) -> Result<String, ConfigError>
where
    F: FnMut(&str) -> Result<String, EnvironmentError>,
{
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('$') {
        output.push_str(&rest[..index]);
        let after_dollar = &rest[index + 1..];
        if !after_dollar.starts_with('{') {
            output.push('$');
            rest = after_dollar;
            continue;
        }

        let after_brace = &after_dollar[1..];
        let Some(end) = after_brace.find('}') else {
            return Err(ConfigError::MalformedInterpolation(
                "a ${ has no closing }".to_owned(),
            ));
        };
        let name = &after_brace[..end];
        if name.is_empty() {
            return Err(ConfigError::MalformedInterpolation(
                "${} has no variable name".to_owned(),
            ));
        }
        if !valid_env_name(name) {
            return Err(ConfigError::MalformedInterpolation(format!(
                "{name:?} is not a valid environment variable name. Use letters, digits and _, and start with a letter or _"
            )));
        }
        match env(name) {
            Ok(value) => output.push_str(&value),
            Err(EnvironmentError::Missing) => {
                return Err(ConfigError::MissingEnvironmentVariable(name.to_owned()));
            }
            Err(EnvironmentError::NotUnicode) => {
                return Err(ConfigError::NonUnicodeEnvironmentVariable(name.to_owned()));
            }
        }
        rest = &after_brace[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !matches!(first, b'A'..=b'Z' | b'a'..=b'z' | b'_') {
        return false;
    }
    bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn process_env(name: &str) -> Result<String, EnvironmentError> {
    let value = std::env::var_os(name).ok_or(EnvironmentError::Missing)?;
    value
        .into_string()
        .map_err(|_| EnvironmentError::NotUnicode)
}

pub fn canonical_observation_patterns(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl FromStr for IgnoreRuleSource {
    type Err = ConfigError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "config" => Ok(Self::Config),
            "cli" => Ok(Self::Cli),
            _ => Err(ConfigError::InvalidValue(format!(
                "invalid ignore rule source {input:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn env(vars: &[(&str, &str)]) -> impl FnMut(&str) -> Result<String, EnvironmentError> {
        let map: BTreeMap<String, String> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| map.get(name).cloned().ok_or(EnvironmentError::Missing)
    }

    fn write_config(raw: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("auditroo.toml");
        fs::write(&path, raw).expect("config writes");
        (dir, path)
    }

    #[test]
    fn default_settings_are_resolved_without_default_file() {
        let settings = resolve_settings_with_env(
            Some(Path::new("/definitely/missing/auditroo.toml")),
            SettingsOverrides::default(),
            env(&[]),
        )
        .expect_err("explicit missing config fails");
        assert!(matches!(settings, ConfigError::MissingExplicitConfig(_)));

        let settings =
            resolve_settings_with_env(None, SettingsOverrides::default(), env(&[])).unwrap();
        assert_eq!(settings.crawl.concurrency, 8);
        assert_eq!(settings.crawl.max_pages, 10_000);
        assert_eq!(settings.gate.fail_on, Severity::High);
    }

    #[test]
    fn strict_config_cli_overrides_and_env_expansion_work() {
        let (_dir, path) = write_config(
            r#"
            [crawl]
            user_agent = "from-file"
            concurrency = 4
            headers = [{ name = "x-auditroo-bypass", value = "${AUDITROO_SECRET}" }]
            include = ["/docs/*"]

            [gate]
            fail_on = "critical"
            ignore = [{ url = "/old/*", rule = "title_changed" }]
            "#,
        );

        let settings = resolve_settings_with_env(
            Some(&path),
            SettingsOverrides {
                concurrency: Some(2),
                include: Some(vec!["/new/*".to_owned()]),
                headers: Some(vec!["x-cli: ${CLI_SECRET}:tail".to_owned()]),
                fail_on: Some(Severity::Medium),
                ..SettingsOverrides::default()
            },
            env(&[("AUDITROO_SECRET", "file-secret"), ("CLI_SECRET", "cli")]),
        )
        .expect("config resolves");

        assert_eq!(settings.crawl.user_agent.as_deref(), Some("from-file"));
        assert_eq!(settings.crawl.concurrency, 2);
        assert_eq!(settings.crawl.include, ["/new/*"]);
        assert_eq!(settings.crawl.headers[0].name, "x-cli");
        assert_eq!(settings.crawl.headers[0].value, "cli:tail");
        assert_eq!(settings.gate.fail_on, Severity::Medium);
        assert_eq!(settings.gate.ignore.len(), 1);
    }

    #[test]
    fn production_url_comes_from_the_file_and_the_flag_wins() {
        let (_dir, path) = write_config("[crawl]\nproduction_url = \"https://www.example.com\"\n");
        let from_file =
            resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
                .expect("config resolves");
        assert_eq!(
            from_file.crawl.production_url.as_deref(),
            Some("https://www.example.com")
        );
        let from_flag = resolve_settings_with_env(
            Some(&path),
            SettingsOverrides {
                production_url: Some("https://live.example.org/".to_owned()),
                ..SettingsOverrides::default()
            },
            env(&[]),
        )
        .expect("config resolves");
        assert_eq!(
            from_flag.crawl.production_url.as_deref(),
            Some("https://live.example.org/")
        );
        let (_dir, bad) = write_config("[crawl]\nproduction_url = \"www.example.com\"\n");
        let error = resolve_settings_with_env(Some(&bad), SettingsOverrides::default(), env(&[]))
            .unwrap_err();
        assert!(error.to_string().contains("production_url"), "{error}");
    }

    #[test]
    fn a_recipe_placeholder_left_in_the_config_stops_the_run() {
        // The host placeholder is replaced, the staging pattern is not.
        let (_dir, path) = write_config(
            "# Replace PLACEHOLDER_STAGING_HOST_PATTERN below.\n\
             [[extract]]\n\
             id = \"staging-host\"\n\
             kind = \"regex\"\n\
             source = \"body\"\n\
             pattern = \"PLACEHOLDER_STAGING_HOST_PATTERN\"\n\
             [[assert]]\n\
             id = \"no-staging-host\"\n\
             extractor = \"staging-host\"\n\
             operator = \"missing\"\n\
             severity = \"critical\"\n",
        );
        let error = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("replace PLACEHOLDER_STAGING_HOST_PATTERN with your own value"),
            "{error}"
        );
        // The same word in a comment is fine.
        let (_dir, path) = write_config("# Replace PLACEHOLDER_X once.\n[crawl]\nmax_pages = 5\n");
        resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
            .expect("a comment is not a value");
    }

    #[test]
    fn cli_ignore_rules_add_to_config_rules() {
        let (_dir, path) = write_config("[gate]\nignore = [{ rule = \"title_changed\" }]\n");
        let settings = resolve_settings_with_env(
            Some(&path),
            SettingsOverrides {
                ignore: Some(vec!["{ url = \"/promo/*\" }".to_owned()]),
                ..SettingsOverrides::default()
            },
            env(&[]),
        )
        .expect("config resolves");
        assert_eq!(settings.gate.ignore.len(), 2);
        assert_eq!(settings.gate.ignore[0].source, IgnoreRuleSource::Config);
        assert_eq!(settings.gate.ignore[1].source, IgnoreRuleSource::Cli);
    }

    #[test]
    fn rejects_unknown_fields_zero_values_bad_headers_and_bad_interpolation() {
        let (_dir, unknown) = write_config("[crawl]\nunknown = true\n");
        assert!(matches!(
            resolve_settings_with_env(Some(&unknown), SettingsOverrides::default(), env(&[])),
            Err(ConfigError::MalformedToml(_))
        ));

        let (_dir, zero) = write_config("[crawl]\nconcurrency = 0\n");
        assert!(matches!(
            resolve_settings_with_env(Some(&zero), SettingsOverrides::default(), env(&[])),
            Err(ConfigError::InvalidValue(_))
        ));

        assert!(matches!(
            parse_header_line("missing-colon"),
            Err(ConfigError::InvalidHeader(_))
        ));

        let (_dir, interpolation) =
            write_config("[crawl]\nheaders = [{ name = \"x-test\", value = \"${}\" }]\n");
        assert!(matches!(
            resolve_settings_with_env(Some(&interpolation), SettingsOverrides::default(), env(&[])),
            Err(ConfigError::MalformedInterpolation(_))
        ));
    }

    #[test]
    fn toml_errors_name_the_file_and_show_the_users_own_value() {
        let (_dir, path) = write_config(
            r#"
            [crawl]
            headers = [{ name = "x-auditroo-bypass", value = "literal-secret" }]
            concurrency = "4"
            "#,
        );

        let error = resolve_settings_with_env(Some(&path), SettingsOverrides::default(), env(&[]))
            .expect_err("a string concurrency fails");
        let message = error.to_string();
        assert!(message.contains("auditroo.toml"), "{message}");
        assert!(message.contains("line 4"), "{message}");
        assert!(message.contains("\"4\""), "{message}");
        assert!(!message.contains("[REDACTED]"), "{message}");
        assert!(!message.contains("literal-secret"), "{message}");
    }

    #[test]
    fn ignore_rules_validate_known_fields_and_matchable_urls() {
        let rule = parse_cli_ignore_rule("{ url = \"/promo/*\", rule = \"title_changed\" }")
            .expect("rule parses");
        assert_eq!(rule.url.as_deref(), Some("/promo/*"));
        assert_eq!(rule.source, IgnoreRuleSource::Cli);

        let unknown = parse_cli_ignore_rule("{ rule = \"missing_rule\" }").unwrap_err();
        assert!(unknown.to_string().contains("unknown rule"), "{unknown}");
        let bad_toml = parse_cli_ignore_rule("{ rule = }").unwrap_err().to_string();
        assert!(bad_toml.starts_with("--ignore"), "{bad_toml}");
        assert!(!bad_toml.contains("auditroo.toml"), "{bad_toml}");
        assert!(parse_cli_ignore_rule("{ url = \"promo/*\" }").is_err());
        assert!(parse_cli_ignore_rule("{ url = \"HTTPS://example.test/*\" }").is_err());
        assert!(parse_cli_ignore_rule("{}").is_err());
    }

    #[test]
    fn start_url_rejects_credentials_and_non_http() {
        assert!(validate_start_url("https://example.test/a#frag").is_ok());
        assert!(validate_start_url("https://user:pass@example.test/").is_err());
        assert!(validate_start_url("mailto:seo@example.test").is_err());
    }

    #[test]
    fn header_debug_redacts_values() {
        let header = ResolvedHeader {
            name: "x-secret".to_owned(),
            value: "top-secret-value".to_owned(),
        };
        let debug = format!("{header:?}");
        assert!(debug.contains("x-secret"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("top-secret-value"));
    }
}
