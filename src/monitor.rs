//! Scheduled monitoring. `monitor run` crawls each configured site and
//! compares it with its baseline. Each run is saved in the history, and
//! other commands such as `history list` read it from there.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use url::Url;

use crate::analysis::{analyze_log_sources, write_log_report};
use crate::audit::{
    compare_settings, crawl, ensure_comparable, ensure_crawled, evaluate, start_problem,
    CrawlExtras, Evaluation,
};
use crate::bundle::{generate_bundle, BundleCommandResult, BundleError, BundleRequest};
use crate::config::{
    resolve_monitoring_catalog, resolve_monitoring_site_log_settings,
    resolve_monitoring_site_settings, select_monitoring_sites, validate_monitoring_label,
    DeliveryTrigger, MonitoringBaselineConfig, MonitoringCatalog, MonitoringSite,
    NotificationHookConfig, ResolvedLogSettings, ResolvedSettings,
};
use crate::crawl::normalize_url;
use crate::findings::GateDecision;
use crate::history::{
    aggregate_gate_summary, artifact_descriptor_for_file, bounded_line, import_snapshot_baseline,
    new_run_id, normalized_findings, read_descriptor_bytes, severity_counts, validate_descriptor,
    ArtifactDescriptor, ArtifactRoots, AuditResult, BaselineLabelUpdate, BundleOrigin,
    DeliveryResult, HistoryError, HistoryStore, ManagedArtifactStorage, RunFinalization,
    RunListItem, RunRecord, RUN_LOG_REPORT_FILE, RUN_REPORT_FILE, RUN_SNAPSHOT_FILE,
};
use crate::notify::{attempt_notifications, NotificationContext};
use crate::progress::Progress;
use crate::report::{render_json, write_report, ReportCommand};
use crate::snapshot::{snapshot_from_str, write as write_snapshot, Snapshot};

#[derive(Debug, Clone, Serialize)]
pub struct MonitorRunOutput {
    pub artifact_kind: String,
    pub schema_version: u16,
    pub site_results: Vec<MonitorSiteResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorSiteResult {
    pub site_id: String,
    pub run_id: String,
    pub audit_result: AuditResult,
    pub delivery_result: DeliveryResult,
    pub exit_code: i32,
    pub operational_error: Option<String>,
    /// "3 problems would hurt search" and the summary lines below it.
    pub summary: Vec<String>,
    /// Where the run's report.json was written.
    pub report_path: Option<String>,
    pub bundle: Option<BundleCommandResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryListOutput {
    pub artifact_kind: String,
    pub schema_version: u16,
    pub site_id: String,
    pub runs: Vec<RunListItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BaselineCommandOutput {
    pub artifact_kind: String,
    pub schema_version: u16,
    pub ok: bool,
    pub site_id: String,
    pub label: String,
    pub source: String,
    pub run_id: Option<String>,
    pub descriptor: ArtifactDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorError {
    Config(crate::config::ConfigError),
    History(HistoryError),
    Cli(String),
    Bundle(String),
    Notification(String),
    InvalidBaseline(String),
    Io(String),
    Serialization(String),
}

impl fmt::Display for MonitorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(formatter, "{error}"),
            Self::History(error) => write!(formatter, "{error}"),
            Self::Cli(detail) => formatter.write_str(detail),
            Self::Bundle(detail) => formatter.write_str(detail),
            Self::Notification(detail) => formatter.write_str(detail),
            Self::InvalidBaseline(detail) => {
                write!(formatter, "the baseline cannot be used: {detail}")
            }
            Self::Io(detail) => write!(formatter, "could not write the monitor run: {detail}"),
            Self::Serialization(detail) => {
                write!(formatter, "could not store the monitor run: {detail}")
            }
        }
    }
}

impl Error for MonitorError {}

impl From<crate::config::ConfigError> for MonitorError {
    fn from(error: crate::config::ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<HistoryError> for MonitorError {
    fn from(error: HistoryError) -> Self {
        Self::History(error)
    }
}

impl From<BundleError> for MonitorError {
    fn from(error: BundleError) -> Self {
        Self::Bundle(error.to_string())
    }
}

impl From<std::io::Error> for MonitorError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

pub async fn run_monitor(
    config_path: Option<&Path>,
    site_selector: &str,
) -> Result<MonitorRunOutput, MonitorError> {
    let catalog = resolve_monitoring_catalog(config_path)?;
    let sites = select_monitoring_sites(&catalog, site_selector)?;
    let mut store = HistoryStore::open_or_create(&catalog.history.database)?;
    let roots = roots(&catalog);
    let mut site_results = Vec::new();
    for site in sites {
        site_results.push(run_site(&mut store, &catalog, &roots, &site).await?);
    }
    Ok(MonitorRunOutput {
        artifact_kind: "monitor_run_result".to_owned(),
        schema_version: 1,
        site_results,
    })
}

pub fn history_list(
    config_path: Option<&Path>,
    site_id: &str,
    limit: usize,
) -> Result<HistoryListOutput, MonitorError> {
    let catalog = resolve_monitoring_catalog(config_path)?;
    select_monitoring_sites(&catalog, site_id)?;
    let store = HistoryStore::open_existing(&catalog.history.database)?;
    Ok(HistoryListOutput {
        artifact_kind: "history_list".to_owned(),
        schema_version: 1,
        site_id: site_id.to_owned(),
        runs: store.list_runs(site_id, limit)?,
    })
}

pub fn history_show(config_path: Option<&Path>, run_id: &str) -> Result<RunRecord, MonitorError> {
    let catalog = resolve_monitoring_catalog(config_path)?;
    let store = HistoryStore::open_existing(&catalog.history.database)?;
    store.run(run_id).map_err(Into::into)
}

pub fn baseline_mark(
    config_path: Option<&Path>,
    site_id: &str,
    selector: &str,
    label: &str,
) -> Result<BaselineCommandOutput, MonitorError> {
    validate_monitoring_label(label)?;
    let catalog = resolve_monitoring_catalog(config_path)?;
    let site = select_one_site(&catalog, site_id)?;
    let mut store = HistoryStore::open_existing(&catalog.history.database)?;
    let roots = roots(&catalog);
    let run = if selector == "latest" {
        store.latest_mark_candidate(site_id)?
    } else {
        store.run_for_site(selector, site_id)?
    };
    if !matches!(
        run.audit_result,
        Some(AuditResult::Passed | AuditResult::GateFailed)
    ) {
        return Err(MonitorError::InvalidBaseline(
            "baseline mark needs a finished run that passed or failed the gate".to_owned(),
        ));
    }
    let descriptor = run
        .source_artifact("snapshot", "current")
        .cloned()
        .ok_or_else(|| MonitorError::InvalidBaseline("the run has no snapshot".to_owned()))?;
    validate_descriptor(&roots, &descriptor)?;
    let snapshot = parse_snapshot_descriptor(&roots, &descriptor)?;
    let start = Url::parse(&normalize_url(&Url::parse(&site.start_url).map_err(
        |error| {
            MonitorError::InvalidBaseline(format!("the site's start_url is not valid: {error}"))
        },
    )?))
    .map_err(|error| MonitorError::InvalidBaseline(error.to_string()))?;
    check_baseline_start(&snapshot, &start)?;
    store.set_baseline_label(
        site_id,
        label,
        &descriptor,
        "manual_mark",
        Some(&run.run_id),
        OffsetDateTime::now_utc(),
    )?;
    Ok(BaselineCommandOutput {
        artifact_kind: "baseline_mark_result".to_owned(),
        schema_version: 1,
        ok: true,
        site_id: site_id.to_owned(),
        label: label.to_owned(),
        source: "manual_mark".to_owned(),
        run_id: Some(run.run_id),
        descriptor,
    })
}

pub fn baseline_import(
    config_path: Option<&Path>,
    site_id: &str,
    snapshot_path: &Path,
    label: &str,
) -> Result<BaselineCommandOutput, MonitorError> {
    validate_monitoring_label(label)?;
    let catalog = resolve_monitoring_catalog(config_path)?;
    let site = select_one_site(&catalog, site_id)?;
    let mut store = HistoryStore::open_or_create(&catalog.history.database)?;
    let descriptor = import_snapshot_baseline(&catalog.history.baseline_root, snapshot_path)?;
    let roots = roots(&catalog);
    let snapshot = parse_snapshot_descriptor(&roots, &descriptor)?;
    let start = Url::parse(&normalize_url(&Url::parse(&site.start_url).map_err(
        |error| {
            MonitorError::InvalidBaseline(format!("the site's start_url is not valid: {error}"))
        },
    )?))
    .map_err(|error| MonitorError::InvalidBaseline(error.to_string()))?;
    check_baseline_start(&snapshot, &start)?;
    store.set_baseline_label(
        site_id,
        label,
        &descriptor,
        "manual_import",
        None,
        OffsetDateTime::now_utc(),
    )?;
    Ok(BaselineCommandOutput {
        artifact_kind: "baseline_import_result".to_owned(),
        schema_version: 1,
        ok: true,
        site_id: site_id.to_owned(),
        label: label.to_owned(),
        source: "manual_import".to_owned(),
        run_id: None,
        descriptor,
    })
}

pub fn bundle_from_history(
    config_path: Option<&Path>,
    site_id: &str,
    selector: &str,
    out: &Path,
    display_path: String,
) -> Result<BundleCommandResult, MonitorError> {
    let catalog = resolve_monitoring_catalog(config_path)?;
    select_one_site(&catalog, site_id)?;
    let mut store = HistoryStore::open_existing(&catalog.history.database)?;
    let roots = roots(&catalog);
    let run = if selector == "latest" {
        store.latest_bundle_candidate(site_id)?
    } else {
        store.run_for_site(selector, site_id)?
    };
    generate_bundle(
        &mut store,
        &roots,
        &run,
        BundleRequest {
            site_id: site_id.to_owned(),
            run_id: run.run_id.clone(),
            origin: BundleOrigin::Explicit,
            destination: out.to_path_buf(),
            display_path,
            generated_at: OffsetDateTime::now_utc(),
            trigger_reason: None,
            required: None,
        },
    )
    .map_err(Into::into)
}

pub fn render_monitor_human(output: &MonitorRunOutput) -> String {
    let mut out = String::new();
    for result in &output.site_results {
        let verdict = match result.audit_result {
            AuditResult::Passed => "PASS",
            AuditResult::GateFailed => "FAIL",
            AuditResult::OperationalError => "ERROR",
        };
        out.push_str(&format!(
            "{verdict}  {} (run {})\n",
            result.site_id, result.run_id
        ));
        for line in &result.summary {
            out.push_str(&format!("      {line}\n"));
        }
        if let Some(error) = &result.operational_error {
            out.push_str(&format!("      {error}\n"));
        }
        if let Some(path) = &result.report_path {
            out.push_str(&format!("      report: {path}\n"));
        }
        if let Some(bundle) = &result.bundle {
            out.push_str(&format!(
                "      bundle: {}\n",
                bundle.destination.display_path
            ));
        }
        match result.delivery_result {
            DeliveryResult::FailedRequired | DeliveryResult::FailedOptional => {
                out.push_str(&format!(
                    "      delivery: {}\n",
                    delivery_text(result.delivery_result)
                ));
            }
            _ => {}
        }
    }
    out
}

/// One monitor run, for `auditroo history show`.
pub fn render_history_show_human(run: &RunRecord) -> String {
    let mut out = format!(
        "Run {} of site {}: {}\n",
        run.run_id,
        run.site_id,
        result_text(run.audit_result)
    );
    out.push_str(&format!(
        "  started:  {}\n",
        format_timestamp(run.started_at)
    ));
    if let Some(finished) = run.finished_at {
        out.push_str(&format!("  finished: {}\n", format_timestamp(finished)));
    }
    out.push_str(&format!(
        "  delivery: {}\n",
        delivery_text(run.delivery_result)
    ));
    let counting: Vec<_> = run
        .findings
        .iter()
        .filter(|finding| finding.gate_relevant)
        .collect();
    out.push_str(&format!(
        "  findings: {} ({} fail the gate)\n",
        run.findings.len(),
        counting.len()
    ));
    for finding in counting.iter().take(20) {
        out.push_str(&format!(
            "    {} {} {}\n",
            finding
                .severity
                .map(|severity| severity.as_str())
                .unwrap_or("-"),
            finding.url.as_deref().unwrap_or("site"),
            finding.message
        ));
    }
    if let Some(error) = &run.operational_error {
        out.push_str(&format!("  error: {error}\n"));
    }
    if let Some(error) = &run.delivery_error {
        out.push_str(&format!("  delivery error: {error}\n"));
    }
    out
}

pub fn render_history_list_human(output: &HistoryListOutput) -> String {
    if output.runs.is_empty() {
        return format!("Site {} has no monitor runs yet.\n", output.site_id);
    }
    let mut rendered = format!("Runs of site {}, newest first\n", output.site_id);
    for run in &output.runs {
        let worst = run
            .effective_max_severity
            .map(|severity| format!(", worst finding {severity}"))
            .unwrap_or_default();
        let delivery = match run.delivery_result {
            DeliveryResult::FailedRequired | DeliveryResult::FailedOptional => {
                format!(", delivery {}", delivery_text(run.delivery_result))
            }
            _ => String::new(),
        };
        rendered.push_str(&format!(
            "  {}  {}  {}{worst}{delivery}\n",
            run.run_id,
            format_timestamp(run.started_at),
            result_text(run.audit_result)
        ));
    }
    rendered
}

/// A run result in words, for the text output.
fn result_text(result: Option<AuditResult>) -> &'static str {
    match result {
        Some(AuditResult::Passed) => "passed",
        Some(AuditResult::GateFailed) => "failed the gate",
        Some(AuditResult::OperationalError) => "could not finish",
        None => "still running",
    }
}

/// What happened to the notifications and bundles of a run, in words.
fn delivery_text(result: DeliveryResult) -> &'static str {
    match result {
        DeliveryResult::NotConfigured => "nothing to deliver",
        DeliveryResult::Pending => "pending",
        DeliveryResult::Succeeded => "done",
        DeliveryResult::FailedOptional => "failed (optional)",
        DeliveryResult::FailedRequired => "failed (required)",
    }
}

async fn run_site(
    store: &mut HistoryStore,
    catalog: &MonitoringCatalog,
    roots: &ArtifactRoots,
    site: &MonitoringSite,
) -> Result<MonitorSiteResult, MonitorError> {
    let started_at = OffsetDateTime::now_utc();
    let run_id = new_run_id(started_at);
    let baseline_selector = baseline_selector_json(site);
    store.begin_run(
        &run_id,
        &site.id,
        started_at,
        Some(&initial_config_fingerprint(catalog, site)),
        &baseline_selector,
    )?;

    let audit = run_site_audit(store, catalog, roots, site, &run_id).await;
    let mut result = match audit {
        Ok(result) => result,
        Err(error) => {
            let message = bounded_line(&error.to_string(), 500);
            store.finalize_run(RunFinalization {
                run_id: run_id.clone(),
                finished_at: OffsetDateTime::now_utc(),
                audit_result: AuditResult::OperationalError,
                delivery_result: DeliveryResult::Pending,
                resolved_baseline: None,
                aggregate_gate: None,
                severity_counts: Vec::new(),
                findings: Vec::new(),
                operational_error: Some(message.clone()),
                source_artifacts: Vec::new(),
                baseline_label_update: None,
            })?;
            SiteAuditResult {
                run_id: run_id.clone(),
                audit_result: AuditResult::OperationalError,
                aggregate_gate: None,
                severity_counts: Vec::new(),
                findings: Vec::new(),
                source_artifacts: Vec::new(),
                local_artifact_paths: BTreeMap::new(),
                operational_error: Some(message),
                summary: Vec::new(),
            }
        }
    };

    let mut delivery = DeliveryAccumulator::default();
    let automatic_bundle = attempt_automatic_bundle(store, catalog, roots, site, &result).await;
    let mut bundle_result = None;
    match automatic_bundle {
        Ok(Some(bundle)) => {
            delivery.observe_success();
            bundle_result = Some(bundle);
        }
        Ok(None) => {}
        Err(error) => {
            let required = site
                .bundle
                .as_ref()
                .map(|policy| policy.required)
                .unwrap_or(false);
            delivery.observe_failure(required, error.to_string());
        }
    }

    let notifications =
        attempt_site_notifications(catalog, site, &result, delivery.current()).await?;
    for attempt in &notifications.attempts {
        store.record_notification(attempt)?;
    }
    delivery.observe_delivery(notifications.delivery_result, notifications.error_summary);
    let final_delivery = delivery.final_result();
    store.update_delivery(
        &result.run_id,
        final_delivery,
        delivery.error_summary.as_deref(),
    )?;
    result.source_artifacts.clear();

    let report_path = result.local_artifact_paths.get("current_report").cloned();
    Ok(MonitorSiteResult {
        site_id: site.id.clone(),
        run_id: result.run_id,
        audit_result: result.audit_result,
        delivery_result: final_delivery,
        exit_code: site_exit_code(result.audit_result, final_delivery),
        operational_error: result.operational_error,
        summary: result.summary,
        report_path,
        bundle: bundle_result,
    })
}

struct SiteAuditResult {
    run_id: String,
    audit_result: AuditResult,
    aggregate_gate: Option<crate::history::AggregateGateSummary>,
    severity_counts: Vec<crate::history::SeverityCount>,
    findings: Vec<crate::history::FindingSummary>,
    source_artifacts: Vec<ArtifactDescriptor>,
    local_artifact_paths: BTreeMap<String, String>,
    operational_error: Option<String>,
    summary: Vec<String>,
}

async fn run_site_audit(
    store: &mut HistoryStore,
    catalog: &MonitoringCatalog,
    roots: &ArtifactRoots,
    site: &MonitoringSite,
    run_id: &str,
) -> Result<SiteAuditResult, MonitorError> {
    validate_selected_site(catalog, site)?;
    let settings = resolve_monitoring_site_settings(catalog, site)?;
    let start_url = crate::config::validate_start_url(&site.start_url)?;
    let log_settings = resolve_monitoring_site_log_settings(catalog, site)?;
    let selected_hooks = notification_hooks(catalog, &site.notify);
    store.update_config_fingerprint(
        run_id,
        &resolved_config_fingerprint(
            catalog,
            site,
            &settings,
            log_settings.as_ref(),
            &selected_hooks,
        ),
    )?;
    let baseline = resolve_baseline(store, catalog, roots, site, &start_url)?;
    ensure_comparable(&baseline.snapshot, "the baseline")
        .map_err(|error| MonitorError::Cli(error.to_string()))?;
    let notes = compare_settings(&baseline.snapshot, &settings)
        .map_err(|error| MonitorError::Cli(error.to_string()))?;
    let current = crawl(
        start_url.clone(),
        &settings,
        CrawlExtras::compared_with(&baseline.snapshot, &start_url),
        &mut Progress::quiet(),
        "Crawling",
    )
    .await
    .map_err(|error| MonitorError::Cli(error.to_string()))?;
    // Like on the command line, a crawl that robots.txt stopped for auditroo
    // alone is an error and does not pass.
    ensure_crawled(&current).map_err(|error| MonitorError::Cli(error.to_string()))?;
    let diff_report = evaluate(Evaluation {
        command: ReportCommand::Diff,
        baseline: Some(&baseline.snapshot),
        current: &current,
        settings: &settings,
        notes,
    })
    .await
    .map_err(|error| MonitorError::Cli(error.to_string()))?;
    let log_report = match log_settings {
        Some(log_settings) => Some(
            analyze_log_sources(
                &current,
                &log_settings.sources,
                &log_settings.bot_catalog,
                &log_settings.analysis,
            )
            .map_err(|error| MonitorError::Cli(error.to_string()))?,
        ),
        None => None,
    };

    let stage = staged_run_dir(&catalog.history.artifact_root, &site.id, run_id)?;
    fs::create_dir_all(&stage)?;
    write_snapshot(&current, &stage.join(RUN_SNAPSHOT_FILE))
        .map_err(|error| MonitorError::Cli(error.to_string()))?;
    write_report(&diff_report, &stage.join(RUN_REPORT_FILE))
        .map_err(|error| MonitorError::Cli(error.to_string()))?;
    if let Some(report) = &log_report {
        write_log_report(report, &stage.join(RUN_LOG_REPORT_FILE))
            .map_err(|error| MonitorError::Cli(error.to_string()))?;
    }
    let final_dir = catalog.history.artifact_root.join(&site.id).join(run_id);
    crate::history::publish_directory_atomically(&stage, &final_dir)?;

    let mut artifacts = vec![
        artifact_descriptor_for_file(
            roots,
            ManagedArtifactStorage::RunArtifacts,
            format!("{}/{}/{}", site.id, run_id, RUN_SNAPSHOT_FILE),
            "snapshot",
            current.schema_version,
            "current",
        )?,
        artifact_descriptor_for_file(
            roots,
            ManagedArtifactStorage::RunArtifacts,
            format!("{}/{}/{}", site.id, run_id, RUN_REPORT_FILE),
            "report",
            diff_report.schema_version,
            "current_report",
        )?,
    ];
    if let Some(report) = &log_report {
        artifacts.push(artifact_descriptor_for_file(
            roots,
            ManagedArtifactStorage::RunArtifacts,
            format!("{}/{}/{}", site.id, run_id, RUN_LOG_REPORT_FILE),
            "log_analysis_report",
            report.schema_version,
            "log_analysis",
        )?);
    }

    let gate = aggregate_gate_summary(&diff_report, log_report.as_ref());
    let findings = normalized_findings(&site.id, run_id, &diff_report, log_report.as_ref());
    let counts = severity_counts(&findings);
    let audit_result = match gate.gate_decision {
        GateDecision::Passed => AuditResult::Passed,
        GateDecision::Failed => AuditResult::GateFailed,
    };
    let current_descriptor = artifacts
        .iter()
        .find(|descriptor| descriptor.kind == "snapshot" && descriptor.role == "current")
        .cloned()
        .expect("current snapshot descriptor was just created");
    // Only a crawl that reached a working start page may become the next
    // baseline, whatever the gate says.
    let promotable = start_problem(&current).is_none();
    let baseline_label_update = match (&site.baseline, audit_result) {
        (MonitoringBaselineConfig::Label { label, .. }, AuditResult::Passed) if promotable => {
            Some(BaselineLabelUpdate {
                site_id: site.id.clone(),
                label: label.clone(),
                descriptor: current_descriptor.clone(),
                source: "auto_promotion".to_owned(),
            })
        }
        _ => None,
    };
    store.finalize_run(RunFinalization {
        run_id: run_id.to_owned(),
        finished_at: OffsetDateTime::now_utc(),
        audit_result,
        delivery_result: DeliveryResult::Pending,
        resolved_baseline: Some(baseline.descriptor),
        aggregate_gate: Some(gate.clone()),
        severity_counts: counts.clone(),
        findings: findings.clone(),
        operational_error: None,
        source_artifacts: artifacts.clone(),
        baseline_label_update,
    })?;

    let local_artifact_paths = artifacts
        .iter()
        .map(|descriptor| {
            (
                descriptor.role.clone(),
                catalog
                    .history
                    .artifact_root
                    .join(&descriptor.relative_path)
                    .display()
                    .to_string(),
            )
        })
        .collect();

    let mut summary = vec![crate::render::headline(&diff_report)];
    summary.extend(
        crate::render::summary_lines(&diff_report)
            .into_iter()
            .filter(|line| line.fails)
            .map(|line| format!("- {}", line.text)),
    );
    Ok(SiteAuditResult {
        run_id: run_id.to_owned(),
        audit_result,
        aggregate_gate: Some(gate),
        severity_counts: counts,
        findings,
        source_artifacts: artifacts,
        local_artifact_paths,
        operational_error: None,
        summary,
    })
}

struct ResolvedBaseline {
    descriptor: ArtifactDescriptor,
    snapshot: Snapshot,
}

fn resolve_baseline(
    store: &mut HistoryStore,
    catalog: &MonitoringCatalog,
    roots: &ArtifactRoots,
    site: &MonitoringSite,
    current_start: &Url,
) -> Result<ResolvedBaseline, MonitorError> {
    let descriptor = match &site.baseline {
        MonitoringBaselineConfig::Label {
            label,
            bootstrap_path,
            ..
        } => match store.baseline_label(&site.id, label)? {
            Some(descriptor) => descriptor,
            None => {
                let path = bootstrap_path.as_ref().ok_or_else(|| {
                    MonitorError::InvalidBaseline(format!(
                        "the label {label:?} does not exist yet, and the site has no bootstrap_path"
                    ))
                })?;
                imported_external_baseline(
                    store,
                    catalog,
                    site,
                    &external_baseline_key("bootstrap", label, path),
                    "bootstrap",
                    path,
                )?
            }
        },
        MonitoringBaselineConfig::Fixed { path, .. } => imported_external_baseline(
            store,
            catalog,
            site,
            &external_baseline_key("fixed", "", path),
            "fixed",
            path,
        )?,
    };
    validate_descriptor(roots, &descriptor)?;
    let snapshot = parse_snapshot_descriptor(roots, &descriptor)?;
    check_baseline_start(&snapshot, current_start)?;
    Ok(ResolvedBaseline {
        descriptor,
        snapshot,
    })
}

fn imported_external_baseline(
    store: &mut HistoryStore,
    catalog: &MonitoringCatalog,
    site: &MonitoringSite,
    selector_key: &str,
    source: &str,
    path: &Path,
) -> Result<ArtifactDescriptor, MonitorError> {
    if let Some(descriptor) = store.imported_baseline(&site.id, selector_key)? {
        return Ok(descriptor);
    }
    let descriptor = import_snapshot_baseline(&catalog.history.baseline_root, path)?;
    store.record_imported_baseline(
        &site.id,
        selector_key,
        &descriptor,
        source,
        OffsetDateTime::now_utc(),
    )?;
    Ok(descriptor)
}

fn external_baseline_key(mode: &str, label: &str, path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(mode.as_bytes());
    hasher.update([0]);
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(path.display().to_string().as_bytes());
    format!("{mode}:{}", finish_sha256(hasher))
}

async fn attempt_automatic_bundle(
    store: &mut HistoryStore,
    catalog: &MonitoringCatalog,
    roots: &ArtifactRoots,
    site: &MonitoringSite,
    result: &SiteAuditResult,
) -> Result<Option<BundleCommandResult>, MonitorError> {
    let Some(policy) = &site.bundle else {
        return Ok(None);
    };
    let reason = bundle_trigger_reason(&policy.triggers, result);
    let Some(reason) = reason else {
        return Ok(None);
    };
    let run = store.run_for_site(&result.run_id, &site.id)?;
    let destination = catalog.bundle.root.join(&site.id).join(&result.run_id);
    let display_path = format!("{}/{}/", site.id, result.run_id);
    let bundle = generate_bundle(
        store,
        roots,
        &run,
        BundleRequest {
            site_id: site.id.clone(),
            run_id: result.run_id.clone(),
            origin: BundleOrigin::Automatic,
            destination,
            display_path,
            generated_at: OffsetDateTime::now_utc(),
            trigger_reason: Some(reason),
            required: Some(policy.required),
        },
    )?;
    Ok(Some(bundle))
}

async fn attempt_site_notifications(
    catalog: &MonitoringCatalog,
    site: &MonitoringSite,
    result: &SiteAuditResult,
    delivery_result_before: DeliveryResult,
) -> Result<crate::notify::NotificationRunResult, MonitorError> {
    let selected_hooks = notification_hooks(catalog, &site.notify);
    let context = NotificationContext {
        site_id: site.id.clone(),
        run_id: result.run_id.clone(),
        audit_result: result.audit_result,
        aggregate_gate: result.aggregate_gate.clone(),
        severity_counts: result.severity_counts.clone(),
        source_artifacts: result.source_artifacts.clone(),
        local_artifact_paths: result.local_artifact_paths.clone(),
        findings: result.findings.clone(),
        delivery_result_before,
        summary: result.summary.clone(),
    };
    Ok(attempt_notifications(&selected_hooks, &site.notify, &context).await)
}

fn notification_hooks(
    catalog: &MonitoringCatalog,
    site_hook_ids: &[String],
) -> Vec<NotificationHookConfig> {
    let selected = site_hook_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    catalog
        .notifications
        .iter()
        .filter(|hook| selected.contains(hook.id.as_str()))
        .cloned()
        .collect()
}

fn validate_selected_site(
    catalog: &MonitoringCatalog,
    site: &MonitoringSite,
) -> Result<(), MonitorError> {
    if let Some(error) = &site.semantic_error {
        return Err(MonitorError::Config(
            crate::config::ConfigError::InvalidValue(format!(
                "the site {:?} is not valid: {error}",
                site.id
            )),
        ));
    }
    for hook_id in &site.notify {
        if !catalog.notifications.iter().any(|hook| hook.id == *hook_id) {
            return Err(MonitorError::Config(
                crate::config::ConfigError::InvalidValue(format!(
                    "the site {:?} names the notification hook {:?}, which is not in the config",
                    site.id, hook_id
                )),
            ));
        }
    }
    Ok(())
}

fn bundle_trigger_reason(triggers: &[DeliveryTrigger], result: &SiteAuditResult) -> Option<String> {
    for trigger in triggers {
        match trigger {
            DeliveryTrigger::Passed if result.audit_result == AuditResult::Passed => {
                return Some(trigger.as_str().to_owned());
            }
            DeliveryTrigger::GateFailed if result.audit_result == AuditResult::GateFailed => {
                return Some(trigger.as_str().to_owned());
            }
            DeliveryTrigger::OperationalError
                if result.audit_result == AuditResult::OperationalError =>
            {
                return Some(trigger.as_str().to_owned());
            }
            DeliveryTrigger::MinEffectiveSeverity
                if result
                    .findings
                    .iter()
                    .any(|finding| finding.effective_severity.is_some()) =>
            {
                return Some(trigger.as_str().to_owned());
            }
            _ => {}
        }
    }
    None
}

#[derive(Default)]
struct DeliveryAccumulator {
    attempted: bool,
    required_failed: bool,
    optional_failed: bool,
    error_summary: Option<String>,
}

impl DeliveryAccumulator {
    fn observe_success(&mut self) {
        self.attempted = true;
    }

    fn observe_failure(&mut self, required: bool, error: String) {
        self.attempted = true;
        if required {
            self.required_failed = true;
        } else {
            self.optional_failed = true;
        }
        self.push_error(error);
    }

    fn observe_delivery(&mut self, result: DeliveryResult, error: Option<String>) {
        match result {
            DeliveryResult::NotConfigured => {}
            DeliveryResult::Pending => self.attempted = true,
            DeliveryResult::Succeeded => self.attempted = true,
            DeliveryResult::FailedOptional => {
                self.attempted = true;
                self.optional_failed = true;
            }
            DeliveryResult::FailedRequired => {
                self.attempted = true;
                self.required_failed = true;
            }
        }
        if let Some(error) = error {
            self.push_error(error);
        }
    }

    fn current(&self) -> DeliveryResult {
        self.final_result()
    }

    fn final_result(&self) -> DeliveryResult {
        if !self.attempted {
            DeliveryResult::NotConfigured
        } else if self.required_failed {
            DeliveryResult::FailedRequired
        } else if self.optional_failed {
            DeliveryResult::FailedOptional
        } else {
            DeliveryResult::Succeeded
        }
    }

    fn push_error(&mut self, error: String) {
        let combined = match self.error_summary.take() {
            Some(existing) => format!("{existing}; {error}"),
            None => error,
        };
        self.error_summary = Some(bounded_line(&combined, 500));
    }
}

fn site_exit_code(audit_result: AuditResult, delivery_result: DeliveryResult) -> i32 {
    match audit_result {
        AuditResult::OperationalError => crate::error::EXIT_RUNTIME,
        AuditResult::GateFailed => crate::error::EXIT_GATE_FAILED,
        AuditResult::Passed if delivery_result == DeliveryResult::FailedRequired => {
            crate::error::EXIT_RUNTIME
        }
        AuditResult::Passed => crate::error::EXIT_OK,
    }
}

/// 3 when a site could not be checked or a required delivery failed, 1 when
/// a site failed its gate, 0 otherwise.
pub fn aggregate_exit_code(output: &MonitorRunOutput) -> i32 {
    let results = &output.site_results;
    if results
        .iter()
        .any(|result| result.audit_result == AuditResult::OperationalError)
    {
        crate::error::EXIT_RUNTIME
    } else if results
        .iter()
        .any(|result| result.audit_result == AuditResult::GateFailed)
    {
        crate::error::EXIT_GATE_FAILED
    } else if results
        .iter()
        .any(|result| result.delivery_result == DeliveryResult::FailedRequired)
    {
        crate::error::EXIT_RUNTIME
    } else {
        crate::error::EXIT_OK
    }
}

pub fn render_json_output<T: Serialize>(value: &T) -> Result<String, MonitorError> {
    Ok(render_json(value))
}

fn select_one_site(
    catalog: &MonitoringCatalog,
    site_id: &str,
) -> Result<MonitoringSite, MonitorError> {
    let mut selected = select_monitoring_sites(catalog, site_id)?;
    selected.pop().ok_or_else(|| {
        MonitorError::Config(crate::config::ConfigError::InvalidValue(format!(
            "there is no [[site]] with the id {site_id:?}"
        )))
    })
}

fn parse_snapshot_descriptor(
    roots: &ArtifactRoots,
    descriptor: &ArtifactDescriptor,
) -> Result<Snapshot, MonitorError> {
    let bytes = read_descriptor_bytes(roots, descriptor)?;
    let raw = std::str::from_utf8(&bytes).map_err(|error| {
        MonitorError::InvalidBaseline(format!("the snapshot is not UTF-8 text: {error}"))
    })?;
    snapshot_from_str(raw).map_err(|error| MonitorError::InvalidBaseline(error.to_string()))
}

/// The baseline must start on the same page as the site.
fn check_baseline_start(snapshot: &Snapshot, start: &Url) -> Result<(), MonitorError> {
    let baseline_start = Url::parse(&snapshot.start_url)
        .map_err(|_| MonitorError::InvalidBaseline("its start URL is not valid".to_owned()))?;
    if baseline_start.path() != start.path() || baseline_start.query() != start.query() {
        return Err(MonitorError::InvalidBaseline(format!(
            "it starts at {} but the site's start_url is {}",
            baseline_start.path(),
            start.path()
        )));
    }
    Ok(())
}

fn staged_run_dir(root: &Path, site_id: &str, run_id: &str) -> Result<PathBuf, MonitorError> {
    let parent = root.join(site_id);
    fs::create_dir_all(&parent)?;
    for attempt in 0..1000u32 {
        let candidate = parent.join(format!(".{run_id}.tmp-{}-{attempt}", std::process::id()));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(MonitorError::Io(
        "could not create a temporary folder for the run".to_owned(),
    ))
}

fn roots(catalog: &MonitoringCatalog) -> ArtifactRoots {
    ArtifactRoots {
        run_artifact_root: catalog.history.artifact_root.clone(),
        baseline_root: catalog.history.baseline_root.clone(),
    }
}

fn baseline_selector_json(site: &MonitoringSite) -> Value {
    match &site.baseline {
        MonitoringBaselineConfig::Label {
            label,
            bootstrap_display,
            ..
        } => json!({
            "mode": "label",
            "label": label,
            "bootstrap_path": bootstrap_display
        }),
        MonitoringBaselineConfig::Fixed { display_path, .. } => json!({
            "mode": "fixed",
            "path": display_path
        }),
    }
}

fn initial_config_fingerprint(catalog: &MonitoringCatalog, site: &MonitoringSite) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"initial");
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update(site.id.as_bytes());
    hasher.update(site.start_url.as_bytes());
    hasher.update(format!("{:?}", site.baseline).as_bytes());
    hasher.update(format!("{:?}", site.bundle).as_bytes());
    hasher.update(format!("{:?}", site.notify).as_bytes());
    hasher.update(format!("{:?}", site.config_display).as_bytes());
    hasher.update(
        catalog
            .config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
            .as_bytes(),
    );
    finish_sha256(hasher)
}

fn resolved_config_fingerprint(
    catalog: &MonitoringCatalog,
    site: &MonitoringSite,
    settings: &ResolvedSettings,
    log_settings: Option<&ResolvedLogSettings>,
    selected_hooks: &[NotificationHookConfig],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"resolved");
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update(site.id.as_bytes());
    hasher.update(site.start_url.as_bytes());
    hasher.update(format!("{settings:?}").as_bytes());
    hasher.update(format!("{log_settings:?}").as_bytes());
    hasher.update(format!("{selected_hooks:?}").as_bytes());
    hasher.update(format!("{:?}", site.baseline).as_bytes());
    hasher.update(format!("{:?}", site.bundle).as_bytes());
    hasher.update(format!("{:?}", catalog.history).as_bytes());
    hasher.update(format!("{:?}", catalog.bundle).as_bytes());
    finish_sha256(hasher)
}

fn finish_sha256(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(71);
    hex.push_str("sha256:");
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn format_timestamp(value: OffsetDateTime) -> String {
    crate::render::format_time(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint_for_config(path: &Path, max_pages: usize) -> String {
        fs::write(
            path,
            format!(
                r#"
                [crawl]
                max_pages = {max_pages}

                [[site]]
                id = "client-a"
                start_url = "https://example.test/"
                "#
            ),
        )
        .expect("config writes");
        let catalog = resolve_monitoring_catalog(Some(path)).expect("catalog resolves");
        let site = catalog.sites.first().expect("site exists");
        let settings =
            resolve_monitoring_site_settings(&catalog, site).expect("site settings resolve");
        let log_settings =
            resolve_monitoring_site_log_settings(&catalog, site).expect("log settings resolve");
        let hooks = notification_hooks(&catalog, &site.notify);
        resolved_config_fingerprint(&catalog, site, &settings, log_settings.as_ref(), &hooks)
    }

    #[test]
    fn resolved_config_fingerprint_changes_when_crawl_settings_change_at_same_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("auditroo.toml");
        let first = fingerprint_for_config(&path, 10);
        let second = fingerprint_for_config(&path, 20);

        assert_ne!(first, second);
    }
}
