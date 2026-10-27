//! Quality budgets. auditroo reads results from Lighthouse and similar
//! tools, either from a JSON file or from a command that prints one, and
//! compares their numbers with the budgets in auditroo.toml.

pub mod lighthouse;
pub mod nu_html;
pub mod pa11y;
pub mod pagespeed;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;
use url::Url;

use crate::crawl::normalize_url;
use crate::findings::Severity;
use crate::snapshot::{parse_json_no_duplicates, JsonParseError};

pub const MAX_QUALITY_JSON_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_QUALITY_STDERR_SAMPLE_BYTES: usize = 4 * 1024;
pub const MAX_QUALITY_TEMPLATES: usize = 100;
pub const MAX_QUALITY_SOURCES: usize = 200;
pub const MAX_QUALITY_BUDGETS: usize = 1000;
pub const MAX_QUALITY_WARNING_SAMPLES: usize = 20;
pub const MAX_QUALITY_WARNING_CHARS: usize = 240;
pub const MAX_QUALITY_EVIDENCE: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Lighthouse,
    Pagespeed,
    Pa11y,
    NuHtml,
}

impl AdapterKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lighthouse => "lighthouse",
            Self::Pagespeed => "pagespeed",
            Self::Pa11y => "pa11y",
            Self::NuHtml => "nu_html",
        }
    }
}

impl fmt::Display for AdapterKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualitySourceKind {
    LighthouseJson,
    LighthouseCommand,
    PagespeedJson,
    Pa11yJson,
    Pa11yCommand,
    NuHtmlJson,
    NuHtmlCommand,
}

impl QualitySourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LighthouseJson => "lighthouse_json",
            Self::LighthouseCommand => "lighthouse_command",
            Self::PagespeedJson => "pagespeed_json",
            Self::Pa11yJson => "pa11y_json",
            Self::Pa11yCommand => "pa11y_command",
            Self::NuHtmlJson => "nu_html_json",
            Self::NuHtmlCommand => "nu_html_command",
        }
    }

    pub fn adapter_kind(self) -> AdapterKind {
        match self {
            Self::LighthouseJson | Self::LighthouseCommand => AdapterKind::Lighthouse,
            Self::PagespeedJson => AdapterKind::Pagespeed,
            Self::Pa11yJson | Self::Pa11yCommand => AdapterKind::Pa11y,
            Self::NuHtmlJson | Self::NuHtmlCommand => AdapterKind::NuHtml,
        }
    }

    pub fn is_command(self) -> bool {
        matches!(
            self,
            Self::LighthouseCommand | Self::Pa11yCommand | Self::NuHtmlCommand
        )
    }

    pub fn is_file(self) -> bool {
        !self.is_command()
    }

    pub fn supports_metric(self, metric_id: &str) -> bool {
        match self.adapter_kind() {
            AdapterKind::Lighthouse | AdapterKind::Pagespeed => {
                matches!(
                    metric_id,
                    "performance.score"
                        | "accessibility.score"
                        | "performance.largest_contentful_paint_ms"
                        | "performance.cumulative_layout_shift"
                        | "performance.total_blocking_time_ms"
                        | "performance.speed_index_ms"
                        | "performance.total_byte_weight_bytes"
                )
            }
            AdapterKind::Pa11y => matches!(
                metric_id,
                "accessibility.error_count"
                    | "accessibility.warning_count"
                    | "accessibility.notice_count"
            ),
            AdapterKind::NuHtml => {
                matches!(metric_id, "html.error_count" | "html.warning_count")
            }
        }
    }
}

impl fmt::Display for QualitySourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for QualitySourceKind {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "lighthouse_json" => Ok(Self::LighthouseJson),
            "lighthouse_command" => Ok(Self::LighthouseCommand),
            "pagespeed_json" => Ok(Self::PagespeedJson),
            "pa11y_json" => Ok(Self::Pa11yJson),
            "pa11y_command" => Ok(Self::Pa11yCommand),
            "nu_html_json" => Ok(Self::NuHtmlJson),
            "nu_html_command" => Ok(Self::NuHtmlCommand),
            _ => Err(format!(
                "unknown quality source kind {input:?}. Use lighthouse_json, lighthouse_command, pagespeed_json, pa11y_json, pa11y_command, nu_html_json or nu_html_command"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetVerification {
    Matched,
    Mismatch,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualitySourceState {
    Succeeded,
    SucceededWithWarnings,
    SkippedOptional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetComparison {
    Min,
    Max,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedQualitySettings {
    pub enabled: bool,
    /// Failed budgets count toward the fail-on gate. Without it they are
    /// reported as info.
    pub gate: bool,
    pub templates: Vec<ResolvedQualityTemplate>,
    pub sources: Vec<ResolvedQualitySource>,
    pub budgets: Vec<ResolvedQualityBudget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedQualityTemplate {
    pub id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedQualitySource {
    pub id: String,
    pub template_id: String,
    pub template_url: String,
    pub kind: QualitySourceKind,
    pub adapter_kind: AdapterKind,
    pub required: bool,
    pub path: Option<PathBuf>,
    pub display_path: Option<String>,
    pub argv: Option<Vec<String>>,
    pub timeout_seconds: u64,
    pub accepted_exit_codes: Vec<i32>,
    pub declaring_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedQualityBudget {
    pub id: String,
    pub source_id: String,
    pub metric_id: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub severity: Severity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityEvaluation {
    pub enabled: bool,
    pub gate: bool,
    pub source_summaries: Vec<QualitySourceSummary>,
    pub adapter_findings: Vec<AdapterFinding>,
}

impl QualityEvaluation {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            gate: false,
            source_summaries: Vec::new(),
            adapter_findings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualitySourceSummary {
    pub source_id: String,
    pub template_id: String,
    pub kind: QualitySourceKind,
    pub adapter_kind: AdapterKind,
    pub required: bool,
    pub state: QualitySourceState,
    pub target_verification: TargetVerification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lighthouse_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_displayed_url: Option<String>,
    pub payload_bytes: Option<u64>,
    pub payload_sha256: Option<String>,
    pub budgets_passed: usize,
    pub budgets_failed: usize,
    pub budgets_unevaluated: usize,
    pub warning_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdapterFinding {
    pub id: String,
    pub template_id: String,
    pub url: String,
    pub source_id: String,
    pub budget_id: String,
    pub adapter_kind: AdapterKind,
    pub source_kind: QualitySourceKind,
    pub metric_id: String,
    pub observed: f64,
    pub expected: BudgetExpectation,
    pub severity: Severity,
    pub provenance: String,
    pub evidence: Vec<AdapterEvidence>,
    pub source: AdapterFindingSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetExpectation {
    pub comparison: BudgetComparison,
    pub threshold: f64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterEvidence {
    pub kind: String,
    pub metric_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterFindingSource {
    pub target_verification: TargetVerification,
    pub source_kind: QualitySourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lighthouse_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_displayed_url: Option<String>,
    pub payload_bytes: Option<u64>,
    pub payload_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualitySourceError {
    pub source_id: String,
    pub source_kind: QualitySourceKind,
    pub code: String,
    pub exit_status: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualityError {
    Source(QualitySourceError),
}

impl fmt::Display for QualityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The reason already says when a command timed out, exited
            // with an error or printed too much.
            Self::Source(error) => write!(
                formatter,
                "could not use the quality source {} ({}): {} ({})",
                error.source_id, error.source_kind, error.reason, error.code
            ),
        }
    }
}

impl Error for QualityError {}

#[derive(Debug, Clone)]
pub(crate) struct ParsedQualitySource {
    pub metrics: BTreeMap<String, ParsedMetric>,
    pub warnings: Vec<String>,
    pub target_verification: TargetVerification,
    pub lighthouse_version: Option<String>,
    pub final_displayed_url: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedMetric {
    pub value: f64,
    pub unit: &'static str,
    pub evidence: Vec<AdapterEvidence>,
}

#[derive(Debug, Clone)]
pub(crate) struct QualityFailure {
    pub code: &'static str,
    pub reason: Box<str>,
    pub exit_status: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub target_verification: Option<TargetVerification>,
    pub lighthouse_version: Option<Box<str>>,
    pub final_displayed_url: Option<Box<str>>,
    pub payload_bytes: Option<u64>,
    pub payload_sha256: Option<Box<str>>,
}

impl QualityFailure {
    pub(crate) fn new(code: &'static str, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into().into_boxed_str(),
            exit_status: None,
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
            target_verification: None,
            lighthouse_version: None,
            final_displayed_url: None,
            payload_bytes: None,
            payload_sha256: None,
        }
    }

    fn with_exit_status(mut self, exit_status: Option<i32>) -> Self {
        self.exit_status = exit_status;
        self
    }

    fn with_timeout(mut self) -> Self {
        self.timed_out = true;
        self
    }

    fn with_stdout_truncated(mut self) -> Self {
        self.stdout_truncated = true;
        self
    }

    fn with_stderr_truncated(mut self, stderr_truncated: bool) -> Self {
        self.stderr_truncated = stderr_truncated;
        self
    }

    fn with_payload(mut self, payload_bytes: u64, payload_sha256: String) -> Self {
        self.payload_bytes = Some(payload_bytes);
        self.payload_sha256 = Some(payload_sha256.into_boxed_str());
        self
    }

    pub(crate) fn with_target_verification(mut self, verification: TargetVerification) -> Self {
        self.target_verification = Some(verification);
        self
    }

    pub(crate) fn with_tool_metadata(
        mut self,
        lighthouse_version: Option<String>,
        final_displayed_url: Option<String>,
    ) -> Self {
        self.lighthouse_version =
            lighthouse_version.map(|value| bounded_scalar(&value, 1024).into_boxed_str());
        self.final_displayed_url =
            final_displayed_url.map(|value| bounded_scalar(&value, 1024).into_boxed_str());
        self
    }
}

pub async fn evaluate_quality(
    settings: &ResolvedQualitySettings,
) -> Result<QualityEvaluation, QualityError> {
    if !settings.enabled {
        return Ok(QualityEvaluation::disabled());
    }

    let mut budgets_by_source: BTreeMap<&str, Vec<&ResolvedQualityBudget>> = BTreeMap::new();
    for budget in &settings.budgets {
        budgets_by_source
            .entry(budget.source_id.as_str())
            .or_default()
            .push(budget);
    }

    let mut sources = settings.sources.iter().collect::<Vec<_>>();
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    let mut summaries = Vec::new();
    let mut findings = Vec::new();
    for source in sources {
        let source_budgets = budgets_by_source
            .get(source.id.as_str())
            .cloned()
            .unwrap_or_default();
        match evaluate_source(source, &source_budgets).await {
            Ok((summary, mut source_findings)) => {
                if source.required && summary.budgets_unevaluated > 0 {
                    return Err(QualityError::Source(source_error(
                        source,
                        "unevaluated_required_budget",
                        None,
                        false,
                        false,
                        false,
                        "the result has no value for at least one budget of this source",
                    )));
                }
                summaries.push(summary);
                findings.append(&mut source_findings);
            }
            Err(failure) if source.required => {
                return Err(QualityError::Source(source_error_from_failure(
                    source, failure,
                )));
            }
            Err(failure) => {
                let warning = bounded_warning(&format!("{}: {}", failure.code, failure.reason));
                summaries.push(QualitySourceSummary {
                    source_id: source.id.clone(),
                    template_id: source.template_id.clone(),
                    kind: source.kind,
                    adapter_kind: source.adapter_kind,
                    required: source.required,
                    state: QualitySourceState::SkippedOptional,
                    target_verification: failure
                        .target_verification
                        .unwrap_or(TargetVerification::Unavailable),
                    lighthouse_version: failure.lighthouse_version.map(String::from),
                    final_displayed_url: failure.final_displayed_url.map(String::from),
                    payload_bytes: failure.payload_bytes,
                    payload_sha256: failure.payload_sha256.map(String::from),
                    budgets_passed: 0,
                    budgets_failed: 0,
                    budgets_unevaluated: source_budgets.len(),
                    warning_count: 1,
                    warnings: vec![warning],
                });
            }
        }
    }
    findings.sort_by(adapter_finding_sort);

    Ok(QualityEvaluation {
        enabled: true,
        gate: settings.gate,
        source_summaries: summaries,
        adapter_findings: findings,
    })
}

async fn evaluate_source(
    source: &ResolvedQualitySource,
    budgets: &[&ResolvedQualityBudget],
) -> Result<(QualitySourceSummary, Vec<AdapterFinding>), QualityFailure> {
    let payload = read_source_payload(source).await?;
    let payload_bytes = payload.bytes.len() as u64;
    let payload_sha256 = sha256_hex(&payload.bytes);
    let raw = std::str::from_utf8(&payload.bytes)
        .map_err(|_| QualityFailure::new("invalid_utf8", "the result is not UTF-8 text"))
        .map_err(|failure| failure.with_payload(payload_bytes, payload_sha256.clone()))?;
    let value = parse_json_no_duplicates(raw)
        .map_err(quality_json_error)
        .map_err(|failure| failure.with_payload(payload_bytes, payload_sha256.clone()))?;
    let parsed = parse_payload(source, &value)
        .map_err(|failure| failure.with_payload(payload_bytes, payload_sha256.clone()))?;

    let mut warnings = parsed
        .warnings
        .iter()
        .map(|warning| bounded_warning(warning))
        .collect::<Vec<_>>();
    let mut budgets_passed = 0usize;
    let mut budgets_failed = 0usize;
    let mut budgets_unevaluated = 0usize;
    let mut findings = Vec::new();
    for budget in budgets {
        let Some(metric) = parsed.metrics.get(&budget.metric_id) else {
            budgets_unevaluated += 1;
            push_warning(
                &mut warnings,
                format!(
                    "the result has no value for {}, so the budget {} was not checked",
                    budget.metric_id, budget.id
                ),
            );
            continue;
        };
        let (comparison, threshold, failed) = match (budget.min, budget.max) {
            (Some(min), None) => (BudgetComparison::Min, min, metric.value < min),
            (None, Some(max)) => (BudgetComparison::Max, max, metric.value > max),
            _ => {
                return Err(QualityFailure::new(
                    "invalid_budget",
                    "a budget needs exactly one of min and max",
                ));
            }
        };
        if failed {
            budgets_failed += 1;
            let mut evidence = metric.evidence.clone();
            evidence.sort_by(adapter_evidence_sort);
            evidence.truncate(MAX_QUALITY_EVIDENCE);
            findings.push(AdapterFinding {
                id: format!(
                    "adapter::{}::{}::{}",
                    source.id, source.template_id, budget.id
                ),
                template_id: source.template_id.clone(),
                url: source.template_url.clone(),
                source_id: source.id.clone(),
                budget_id: budget.id.clone(),
                adapter_kind: source.adapter_kind,
                source_kind: source.kind,
                metric_id: budget.metric_id.clone(),
                observed: metric.value,
                expected: BudgetExpectation {
                    comparison,
                    threshold,
                    unit: metric.unit.to_owned(),
                },
                severity: budget.severity,
                provenance: "adapter".to_owned(),
                evidence,
                source: AdapterFindingSource {
                    target_verification: parsed.target_verification,
                    source_kind: source.kind,
                    lighthouse_version: parsed.lighthouse_version.clone(),
                    final_displayed_url: parsed.final_displayed_url.clone(),
                    payload_bytes: Some(payload_bytes),
                    payload_sha256: Some(payload_sha256.clone()),
                },
            });
        } else {
            budgets_passed += 1;
        }
    }

    let warning_count = warnings.len();
    let state = if warning_count == 0 && budgets_unevaluated == 0 {
        QualitySourceState::Succeeded
    } else {
        QualitySourceState::SucceededWithWarnings
    };
    warnings.truncate(MAX_QUALITY_WARNING_SAMPLES);
    Ok((
        QualitySourceSummary {
            source_id: source.id.clone(),
            template_id: source.template_id.clone(),
            kind: source.kind,
            adapter_kind: source.adapter_kind,
            required: source.required,
            state,
            target_verification: parsed.target_verification,
            lighthouse_version: parsed.lighthouse_version,
            final_displayed_url: parsed.final_displayed_url,
            payload_bytes: Some(payload_bytes),
            payload_sha256: Some(payload_sha256),
            budgets_passed,
            budgets_failed,
            budgets_unevaluated,
            warning_count,
            warnings,
        },
        findings,
    ))
}

struct SourcePayload {
    bytes: Vec<u8>,
}

async fn read_source_payload(
    source: &ResolvedQualitySource,
) -> Result<SourcePayload, QualityFailure> {
    if source.kind.is_file() {
        let path = source
            .path
            .as_ref()
            .ok_or_else(|| QualityFailure::new("missing_path", "the source has no path"))?;
        let metadata = fs::metadata(path)
            .map_err(|_| QualityFailure::new("source_unavailable", "the file could not be read"))?;
        if metadata.len() > MAX_QUALITY_JSON_BYTES as u64 {
            return Err(QualityFailure::new(
                "payload_too_large",
                "the file is larger than 25 MiB, the limit for one source",
            ));
        }
        let bytes = fs::read(path)
            .map_err(|_| QualityFailure::new("source_unavailable", "the file could not be read"))?;
        if bytes.len() > MAX_QUALITY_JSON_BYTES {
            return Err(QualityFailure::new(
                "payload_too_large",
                "the file is larger than 25 MiB, the limit for one source",
            ));
        }
        return Ok(SourcePayload { bytes });
    }

    read_command_payload(source).await
}

async fn read_command_payload(
    source: &ResolvedQualitySource,
) -> Result<SourcePayload, QualityFailure> {
    let argv = source
        .argv
        .as_ref()
        .ok_or_else(|| QualityFailure::new("missing_argv", "the source has no argv"))?;
    let expanded = argv
        .iter()
        .map(|arg| arg.replace("{template_url}", &source.template_url))
        .collect::<Vec<_>>();
    let mut command = tokio::process::Command::new(&expanded[0]);
    command
        .args(&expanded[1..])
        .current_dir(&source.declaring_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| QualityFailure::new("command_start_failed", "the command could not start"))?;
    let stdout = child.stdout.take().ok_or_else(|| {
        QualityFailure::new(
            "command_start_failed",
            "could not read the output of the command",
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        QualityFailure::new(
            "command_start_failed",
            "could not read the error output of the command",
        )
    })?;
    let mut stdout_task = tokio::spawn(read_until_limit(stdout, MAX_QUALITY_JSON_BYTES));
    let mut stderr_task = tokio::spawn(read_sampled(stderr, MAX_QUALITY_STDERR_SAMPLE_BYTES));
    let deadline = tokio::time::sleep(Duration::from_secs(source.timeout_seconds));
    tokio::pin!(deadline);
    let mut status = None;
    let mut stdout_output = None;
    let mut stderr_output = None;
    loop {
        if status.is_some() && stdout_output.is_some() && stderr_output.is_some() {
            break;
        }
        tokio::select! {
            result = child.wait(), if status.is_none() => {
                status = Some(result.map_err(|_| {
                    QualityFailure::new("command_wait_failed", "the command did not finish")
                })?);
            }
            result = &mut stdout_task, if stdout_output.is_none() => {
                let output = command_read_output(result, "stdout")?;
                if output.truncated {
                    let _ = child.kill().await;
                    abort_task(&mut stderr_task);
                    return Err(QualityFailure::new(
                        "stdout_too_large",
                        "the command printed more than 25 MiB, the limit for one source",
                    )
                    .with_stdout_truncated());
                }
                stdout_output = Some(output);
            }
            result = &mut stderr_task, if stderr_output.is_none() => {
                stderr_output = Some(command_read_output(result, "stderr")?);
            }
            _ = &mut deadline => {
                let _ = child.kill().await;
                abort_task(&mut stdout_task);
                abort_task(&mut stderr_task);
                return Err(QualityFailure::new(
                    "timeout",
                    format!(
                        "the command did not finish within {} seconds",
                        source.timeout_seconds
                    ),
                )
                .with_timeout());
            }
        }
    }
    let status = status.expect("command status is collected before loop exit");
    let stdout = stdout_output
        .expect("stdout is collected before loop exit")
        .bytes;
    let stderr_truncated = stderr_output
        .expect("stderr is collected before loop exit")
        .truncated;
    if !source
        .accepted_exit_codes
        .contains(&status.code().unwrap_or(255))
    {
        return Err(QualityFailure::new(
            "unaccepted_exit_status",
            format!(
                "the command exited with status {}{}",
                status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_owned()),
                if stderr_truncated {
                    " (its error output was cut off)"
                } else {
                    ""
                }
            ),
        )
        .with_exit_status(status.code())
        .with_stderr_truncated(stderr_truncated));
    }
    Ok(SourcePayload { bytes: stdout })
}

#[derive(Debug)]
struct ReadOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_until_limit<R>(mut reader: R, max_bytes: usize) -> std::io::Result<ReadOutput>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if bytes.len() + read > max_bytes {
            let keep = max_bytes.saturating_sub(bytes.len());
            bytes.extend_from_slice(&buffer[..keep]);
            return Ok(ReadOutput {
                bytes,
                truncated: true,
            });
        } else {
            bytes.extend_from_slice(&buffer[..read]);
        }
    }
    Ok(ReadOutput {
        bytes,
        truncated: false,
    })
}

async fn read_sampled<R>(mut reader: R, max_bytes: usize) -> std::io::Result<ReadOutput>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if bytes.len() + read > max_bytes {
            let keep = max_bytes.saturating_sub(bytes.len());
            bytes.extend_from_slice(&buffer[..keep]);
            truncated = true;
        } else if !truncated {
            bytes.extend_from_slice(&buffer[..read]);
        }
    }
    Ok(ReadOutput { bytes, truncated })
}

fn command_read_output(
    result: Result<std::io::Result<ReadOutput>, tokio::task::JoinError>,
    stream: &str,
) -> Result<ReadOutput, QualityFailure> {
    result
        .map_err(|_| {
            QualityFailure::new(
                "command_output_failed",
                format!("could not read the {stream} of the command"),
            )
        })?
        .map_err(|_| {
            QualityFailure::new(
                "command_output_failed",
                format!("could not read the {stream} of the command"),
            )
        })
}

fn abort_task(task: &mut JoinHandle<std::io::Result<ReadOutput>>) {
    if !task.is_finished() {
        task.abort();
    }
}

fn parse_payload(
    source: &ResolvedQualitySource,
    value: &Value,
) -> Result<ParsedQualitySource, QualityFailure> {
    match source.kind {
        QualitySourceKind::LighthouseJson | QualitySourceKind::LighthouseCommand => {
            lighthouse::parse_lighthouse_result(value, &source.template_url)
        }
        QualitySourceKind::PagespeedJson => {
            pagespeed::parse_pagespeed_response(value, &source.template_url)
        }
        QualitySourceKind::Pa11yJson | QualitySourceKind::Pa11yCommand => {
            pa11y::parse_pa11y_report(value)
        }
        QualitySourceKind::NuHtmlJson | QualitySourceKind::NuHtmlCommand => {
            nu_html::parse_nu_html_report(value)
        }
    }
}

pub(crate) fn verify_reported_url(
    raw_url: Option<&str>,
    template_url: &str,
) -> Result<TargetVerification, QualityFailure> {
    let Some(raw_url) = raw_url else {
        return Ok(TargetVerification::Unavailable);
    };
    let parsed = Url::parse(raw_url).map_err(|_| {
        QualityFailure::new("target_mismatch", "the URL in the result is not a full URL")
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(QualityFailure::new(
            "target_mismatch",
            "the URL in the result is not an http:// or https:// URL",
        ));
    }
    let normalized = normalize_url(&parsed);
    if normalized == template_url {
        Ok(TargetVerification::Matched)
    } else {
        Err(QualityFailure::new(
            "target_mismatch",
            "the result is for another URL than the url of its template",
        )
        .with_target_verification(TargetVerification::Mismatch))
    }
}

pub(crate) fn finite_metric(
    value: Option<f64>,
    metric_id: &str,
    unit: &'static str,
    evidence: Vec<AdapterEvidence>,
) -> Option<(String, ParsedMetric)> {
    let value = value?;
    value.is_finite().then(|| {
        (
            metric_id.to_owned(),
            ParsedMetric {
                value,
                unit,
                evidence,
            },
        )
    })
}

pub(crate) fn metric_evidence(
    metric_id: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> AdapterEvidence {
    AdapterEvidence {
        kind: "metric".to_owned(),
        metric_id: metric_id.to_owned(),
        title: title.map(|value| bounded_scalar(value, 1024)),
        description: description.map(|value| bounded_scalar(value, 1024)),
        selector: None,
        snippet: None,
        source_location: None,
    }
}

pub(crate) fn issue_evidence(
    metric_id: &str,
    title: Option<&str>,
    selector: Option<&str>,
    snippet: Option<&str>,
    source_location: Option<String>,
) -> AdapterEvidence {
    AdapterEvidence {
        kind: "issue".to_owned(),
        metric_id: metric_id.to_owned(),
        title: title.map(|value| bounded_scalar(value, 1024)),
        description: None,
        selector: selector.map(|value| bounded_scalar(value, 1024)),
        snippet: snippet.map(|value| bounded_scalar(value, 1024)),
        source_location: source_location.map(|value| bounded_scalar(&value, 1024)),
    }
}

pub fn normalize_quality_template_url(input: &str) -> Result<String, String> {
    let url = Url::parse(input)
        .map_err(|error| format!("the template url is not a valid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("the template url must be a full http:// or https:// URL".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the template url must not contain a user name or password".to_owned());
    }
    if url.fragment().is_some() {
        return Err("the template url must not contain a # part".to_owned());
    }
    Ok(normalize_url(&url))
}

pub fn metric_unit(metric_id: &str) -> Option<&'static str> {
    match metric_id {
        "performance.score" | "accessibility.score" => Some("ratio"),
        "performance.largest_contentful_paint_ms"
        | "performance.total_blocking_time_ms"
        | "performance.speed_index_ms" => Some("milliseconds"),
        "performance.cumulative_layout_shift" => Some("unitless"),
        "performance.total_byte_weight_bytes" => Some("bytes"),
        "accessibility.error_count"
        | "accessibility.warning_count"
        | "accessibility.notice_count"
        | "html.error_count"
        | "html.warning_count" => Some("count"),
        _ => None,
    }
}

pub(crate) fn number_at(value: &Value, path: &[&str]) -> Option<f64> {
    value_at(value, path)?.as_f64()
}

pub(crate) fn string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    value_at(value, path)?.as_str()
}

pub(crate) fn value_at<'a>(mut value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    for key in path {
        value = value.get(*key)?;
    }
    Some(value)
}

pub(crate) fn push_metric(
    metrics: &mut BTreeMap<String, ParsedMetric>,
    metric: Option<(String, ParsedMetric)>,
) {
    if let Some((id, metric)) = metric {
        metrics.insert(id, metric);
    }
}

pub(crate) fn validate_numeric_unit(
    audit: Option<&Value>,
    metric_id: &str,
    allowed: &[&str],
) -> Result<(), QualityFailure> {
    let Some(audit) = audit else {
        return Ok(());
    };
    if audit.get("numericValue").and_then(Value::as_f64).is_none() {
        return Ok(());
    };
    let Some(unit) = audit.get("numericUnit").and_then(Value::as_str) else {
        return Err(QualityFailure::new(
            "unit_mismatch",
            format!("{metric_id} is missing numericUnit"),
        ));
    };
    if !allowed.contains(&unit) {
        return Err(QualityFailure::new(
            "unit_mismatch",
            format!("{metric_id} has a numericUnit that auditroo does not expect"),
        ));
    }
    Ok(())
}

fn quality_json_error(error: JsonParseError) -> QualityFailure {
    match error {
        JsonParseError::Malformed(detail) => QualityFailure::new("invalid_json", detail),
        JsonParseError::DuplicateKey(key) => QualityFailure::new(
            "duplicate_json_key",
            format!("the JSON has the key {key} twice"),
        ),
    }
}

fn source_error(
    source: &ResolvedQualitySource,
    code: &str,
    exit_status: Option<i32>,
    timed_out: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
    reason: impl Into<String>,
) -> QualitySourceError {
    QualitySourceError {
        source_id: source.id.clone(),
        source_kind: source.kind,
        code: code.to_owned(),
        exit_status,
        timed_out,
        stdout_truncated,
        stderr_truncated,
        reason: bounded_warning(&reason.into()),
    }
}

fn source_error_from_failure(
    source: &ResolvedQualitySource,
    failure: QualityFailure,
) -> QualitySourceError {
    source_error(
        source,
        failure.code,
        failure.exit_status,
        failure.timed_out,
        failure.stdout_truncated,
        failure.stderr_truncated,
        String::from(failure.reason),
    )
}

fn push_warning(warnings: &mut Vec<String>, warning: impl Into<String>) {
    warnings.push(bounded_warning(&warning.into()));
}

pub(crate) fn bounded_scalar(input: &str, max_chars: usize) -> String {
    let sanitized = input.replace(['\n', '\r'], " ");
    let mut output = sanitized.chars().take(max_chars).collect::<String>();
    if sanitized.chars().count() > max_chars {
        output.push_str("...");
    }
    output
}

fn bounded_warning(input: &str) -> String {
    bounded_scalar(input, MAX_QUALITY_WARNING_CHARS)
}

fn adapter_finding_sort(left: &AdapterFinding, right: &AdapterFinding) -> std::cmp::Ordering {
    (
        left.url.as_str(),
        left.template_id.as_str(),
        left.source_id.as_str(),
        left.budget_id.as_str(),
        left.id.as_str(),
    )
        .cmp(&(
            right.url.as_str(),
            right.template_id.as_str(),
            right.source_id.as_str(),
            right.budget_id.as_str(),
            right.id.as_str(),
        ))
}

fn adapter_evidence_sort(left: &AdapterEvidence, right: &AdapterEvidence) -> std::cmp::Ordering {
    (
        left.kind.as_str(),
        left.metric_id.as_str(),
        left.title.as_deref().unwrap_or(""),
        left.source_location.as_deref().unwrap_or(""),
    )
        .cmp(&(
            right.kind.as_str(),
            right.metric_id.as_str(),
            right.title.as_deref().unwrap_or(""),
            right.source_location.as_deref().unwrap_or(""),
        ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

pub fn sorted_unique_i32(values: &[i32]) -> bool {
    let unique = values.iter().collect::<BTreeSet<_>>();
    unique.len() == values.len()
}
