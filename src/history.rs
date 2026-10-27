//! The monitor history: a SQLite database with one row per run and the
//! baseline labels of each site. The files of each run live on disk next
//! to it.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};

use crate::analysis::LogAnalysisReport;
use crate::external::{annotate_log_finding_targets, ImpactAnnotation};
use crate::findings::{GateDecision, Rule, Severity};
use crate::report::Report;
use crate::snapshot::SnapshotMetadata;
use crate::snapshot::{snapshot_from_str, Snapshot};

pub const HISTORY_SCHEMA_VERSION: u32 = 1;
pub const RUN_SNAPSHOT_FILE: &str = "snapshot.json";
pub const RUN_REPORT_FILE: &str = "report.json";
pub const RUN_LOG_REPORT_FILE: &str = "log-analysis-report.json";

static RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRoots {
    pub run_artifact_root: PathBuf,
    pub baseline_root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedArtifactStorage {
    RunArtifacts,
    Baselines,
}

impl ManagedArtifactStorage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunArtifacts => "run_artifacts",
            Self::Baselines => "baselines",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    pub storage: ManagedArtifactStorage,
    pub kind: String,
    pub schema_version: u16,
    pub role: String,
    pub relative_path: String,
    pub byte_length: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Finished,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
        }
    }

    fn from_str(input: &str) -> Result<Self, HistoryError> {
        match input {
            "running" => Ok(Self::Running),
            "finished" => Ok(Self::Finished),
            _ => Err(HistoryError::InvalidState(format!(
                "unknown run_state {input:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditResult {
    Passed,
    GateFailed,
    OperationalError,
}

impl AuditResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::GateFailed => "gate_failed",
            Self::OperationalError => "operational_error",
        }
    }

    fn from_optional(input: Option<String>) -> Result<Option<Self>, HistoryError> {
        input
            .as_deref()
            .map(|value| match value {
                "passed" => Ok(Self::Passed),
                "gate_failed" => Ok(Self::GateFailed),
                "operational_error" => Ok(Self::OperationalError),
                _ => Err(HistoryError::InvalidState(format!(
                    "unknown audit_result {value:?}"
                ))),
            })
            .transpose()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryResult {
    NotConfigured,
    Pending,
    Succeeded,
    FailedOptional,
    FailedRequired,
}

impl DeliveryResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::FailedOptional => "failed_optional",
            Self::FailedRequired => "failed_required",
        }
    }

    fn from_str(input: &str) -> Result<Self, HistoryError> {
        match input {
            "not_configured" => Ok(Self::NotConfigured),
            "pending" => Ok(Self::Pending),
            "succeeded" => Ok(Self::Succeeded),
            "failed_optional" => Ok(Self::FailedOptional),
            "failed_required" => Ok(Self::FailedRequired),
            _ => Err(HistoryError::InvalidState(format!(
                "unknown delivery_result {input:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactGateSummary {
    #[serde(default = "default_gate_component")]
    pub component: String,
    pub artifact_kind: String,
    pub gate_enabled: bool,
    pub threshold: Option<Severity>,
    pub raw_max_severity: Option<Severity>,
    pub effective_max_severity: Option<Severity>,
    pub decision: Option<GateDecision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggregateGateSummary {
    pub gate_decision: GateDecision,
    pub raw_max_severity: Option<Severity>,
    pub effective_max_severity: Option<Severity>,
    pub per_artifact: Vec<ArtifactGateSummary>,
}

fn default_gate_component() -> String {
    "core".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeverityCount {
    pub provenance: String,
    pub ignored: bool,
    pub severity: Severity,
    pub count: usize,
}

/// One finding of a monitor run, from the report or the log analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindingSummary {
    /// Stable id: provenance, rule, URL and detail.
    pub id: String,
    /// `crawl`, `assertion`, `quality` or `log`.
    pub provenance: String,
    pub site_id: String,
    pub run_id: String,
    /// Rule id for report findings, finding type for log findings.
    pub rule: String,
    pub url: Option<String>,
    /// Log URL group or the finding's shared detail.
    pub group_key: Option<String>,
    pub severity: Option<Severity>,
    /// Severity when the finding counts, `None` when it is ignored.
    pub effective_severity: Option<Severity>,
    pub ignored: bool,
    /// The finding counts toward the run's gate.
    pub gate_relevant: bool,
    pub message: String,
    #[serde(default)]
    pub impact_context: Vec<ImpactAnnotation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub sequence: i64,
    pub run_id: String,
    pub site_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub run_state: RunState,
    pub audit_result: Option<AuditResult>,
    pub delivery_result: DeliveryResult,
    pub config_fingerprint: Option<String>,
    pub baseline_selector: Value,
    pub resolved_baseline: Option<ArtifactDescriptor>,
    pub aggregate_gate: Option<AggregateGateSummary>,
    pub severity_counts: Vec<SeverityCount>,
    pub findings: Vec<FindingSummary>,
    pub operational_error: Option<String>,
    pub delivery_error: Option<String>,
    pub source_artifacts: Vec<ArtifactDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunListItem {
    pub sequence: i64,
    pub run_id: String,
    pub site_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub run_state: RunState,
    pub audit_result: Option<AuditResult>,
    pub delivery_result: DeliveryResult,
    pub effective_max_severity: Option<Severity>,
    pub source_artifact_count: usize,
    pub operational_error: Option<String>,
    pub delivery_error: Option<String>,
}

struct RunRow {
    sequence: i64,
    run_id: String,
    site_id: String,
    started_at: String,
    finished_at: Option<String>,
    run_state: String,
    audit_result: Option<String>,
    delivery_result: String,
    config_fingerprint: Option<String>,
    baseline_selector_json: String,
    resolved_baseline_json: Option<String>,
    aggregate_gate_json: Option<String>,
    severity_counts_json: String,
    findings_json: String,
    operational_error: Option<String>,
    delivery_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFinalization {
    pub run_id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
    pub audit_result: AuditResult,
    pub delivery_result: DeliveryResult,
    pub resolved_baseline: Option<ArtifactDescriptor>,
    pub aggregate_gate: Option<AggregateGateSummary>,
    pub severity_counts: Vec<SeverityCount>,
    pub findings: Vec<FindingSummary>,
    pub operational_error: Option<String>,
    pub source_artifacts: Vec<ArtifactDescriptor>,
    pub baseline_label_update: Option<BaselineLabelUpdate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineLabelUpdate {
    pub site_id: String,
    pub label: String,
    pub descriptor: ArtifactDescriptor,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationAttemptRecord {
    pub run_id: String,
    pub hook_id: String,
    pub hook_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finished_at: OffsetDateTime,
    pub trigger_reasons: Vec<String>,
    pub required: bool,
    pub success: bool,
    pub status: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleOrigin {
    Explicit,
    Automatic,
}

impl BundleOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Automatic => "automatic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleState {
    Pending,
    Completed,
    Failed,
}

impl BundleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleRecord {
    pub bundle_id: String,
    pub run_id: String,
    pub site_id: String,
    pub origin: BundleOrigin,
    pub state: BundleState,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    pub trigger_reason: Option<String>,
    pub required: Option<bool>,
    pub root_locator: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleFileDescriptor {
    pub path: String,
    pub role: String,
    pub artifact_kind: String,
    pub schema_version: Option<u16>,
    pub media_type: String,
    pub byte_length: u64,
    pub sha256: String,
    pub record_count: Option<usize>,
    pub truncation: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    Io(String),
    Sql(String),
    Serialization(String),
    MissingDatabase(String),
    UnsupportedSchemaVersion(u32),
    InvalidState(String),
    InvalidDescriptor(String),
    InvalidArtifact(String),
    NotFound(String),
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(detail) => write!(
                formatter,
                "could not read or write the monitor history: {detail}"
            ),
            Self::Sql(detail) => write!(formatter, "the history database failed: {detail}"),
            Self::Serialization(detail) => {
                write!(formatter, "could not store data in the history: {detail}")
            }
            Self::MissingDatabase(path) => write!(
                formatter,
                "there is no history database at {path} yet. Run auditroo monitor run first"
            ),
            Self::UnsupportedSchemaVersion(version) => write!(
                formatter,
                "the history database has schema version {version}, which this version of auditroo cannot read"
            ),
            Self::InvalidState(detail) => {
                write!(formatter, "the history database is not usable: {detail}")
            }
            Self::InvalidDescriptor(detail) => write!(
                formatter,
                "a file in the monitor history does not match its record: {detail}"
            ),
            Self::InvalidArtifact(detail) => formatter.write_str(detail),
            Self::NotFound(detail) => write!(formatter, "the monitor history has no {detail}"),
        }
    }
}

impl Error for HistoryError {}

impl From<io::Error> for HistoryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<rusqlite::Error> for HistoryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error.to_string())
    }
}

pub struct HistoryStore {
    conn: Connection,
}

impl HistoryStore {
    pub fn open_or_create(path: &Path) -> Result<Self, HistoryError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_existing(path: &Path) -> Result<Self, HistoryError> {
        if !path.exists() {
            return Err(HistoryError::MissingDatabase(path.display().to_string()));
        }
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.check_schema()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), HistoryError> {
        self.conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            ",
        )?;
        let version = self.schema_version()?;
        if version > HISTORY_SCHEMA_VERSION {
            return Err(HistoryError::UnsupportedSchemaVersion(version));
        }
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS runs (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL UNIQUE,
                site_id TEXT NOT NULL,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                run_state TEXT NOT NULL,
                audit_result TEXT,
                delivery_result TEXT NOT NULL,
                config_fingerprint TEXT,
                baseline_selector_json TEXT NOT NULL,
                resolved_baseline_json TEXT,
                aggregate_gate_json TEXT,
                severity_counts_json TEXT NOT NULL,
                findings_json TEXT NOT NULL,
                operational_error TEXT,
                delivery_error TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_runs_site_sequence ON runs(site_id, sequence DESC);
            CREATE TABLE IF NOT EXISTS source_artifacts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                schema_version INTEGER NOT NULL,
                role TEXT NOT NULL,
                relative_path TEXT NOT NULL,
                byte_length INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                descriptor_json TEXT NOT NULL,
                FOREIGN KEY(run_id) REFERENCES runs(run_id)
            );
            CREATE INDEX IF NOT EXISTS idx_source_artifacts_run ON source_artifacts(run_id);
            CREATE TABLE IF NOT EXISTS baseline_labels (
                site_id TEXT NOT NULL,
                label TEXT NOT NULL,
                descriptor_json TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                source TEXT NOT NULL,
                run_id TEXT,
                PRIMARY KEY(site_id, label)
            );
            CREATE TABLE IF NOT EXISTS baseline_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                site_id TEXT NOT NULL,
                label TEXT NOT NULL,
                descriptor_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                source TEXT NOT NULL,
                run_id TEXT
            );
            CREATE TABLE IF NOT EXISTS baseline_imports (
                site_id TEXT NOT NULL,
                selector_key TEXT NOT NULL,
                descriptor_json TEXT NOT NULL,
                imported_at TEXT NOT NULL,
                source TEXT NOT NULL,
                PRIMARY KEY(site_id, selector_key)
            );
            CREATE TABLE IF NOT EXISTS notification_attempts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                hook_id TEXT NOT NULL,
                hook_kind TEXT NOT NULL,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                trigger_reasons_json TEXT NOT NULL,
                required INTEGER NOT NULL,
                success INTEGER NOT NULL,
                status TEXT,
                error TEXT,
                FOREIGN KEY(run_id) REFERENCES runs(run_id)
            );
            CREATE TABLE IF NOT EXISTS bundle_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bundle_id TEXT NOT NULL UNIQUE,
                run_id TEXT NOT NULL,
                site_id TEXT NOT NULL,
                origin TEXT NOT NULL,
                state TEXT NOT NULL,
                created_at TEXT NOT NULL,
                finished_at TEXT,
                trigger_reason TEXT,
                required INTEGER,
                root_locator TEXT NOT NULL,
                error TEXT,
                FOREIGN KEY(run_id) REFERENCES runs(run_id)
            );
            CREATE INDEX IF NOT EXISTS idx_bundle_records_run ON bundle_records(run_id);
            CREATE TABLE IF NOT EXISTS bundle_files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bundle_id TEXT NOT NULL,
                path TEXT NOT NULL,
                role TEXT NOT NULL,
                artifact_kind TEXT NOT NULL,
                schema_version INTEGER,
                media_type TEXT NOT NULL,
                byte_length INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                record_count INTEGER,
                truncation_json TEXT,
                FOREIGN KEY(bundle_id) REFERENCES bundle_records(bundle_id)
            );
            ",
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?1)",
            params![HISTORY_SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    fn check_schema(&self) -> Result<(), HistoryError> {
        let version = self.schema_version()?;
        if version == 0 {
            return Err(HistoryError::InvalidState("it was never set up".to_owned()));
        }
        if version > HISTORY_SCHEMA_VERSION {
            return Err(HistoryError::UnsupportedSchemaVersion(version));
        }
        Ok(())
    }

    fn schema_version(&self) -> Result<u32, HistoryError> {
        let maybe: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        maybe
            .map(|value| {
                value.parse::<u32>().map_err(|_| {
                    HistoryError::InvalidState("its schema_version is not a number".to_owned())
                })
            })
            .transpose()
            .map(|value| value.unwrap_or(0))
    }

    pub fn begin_run(
        &self,
        run_id: &str,
        site_id: &str,
        started_at: OffsetDateTime,
        config_fingerprint: Option<&str>,
        baseline_selector: &Value,
    ) -> Result<(), HistoryError> {
        self.conn.execute(
            "
            INSERT INTO runs(
                run_id, site_id, started_at, run_state, audit_result, delivery_result,
                config_fingerprint, baseline_selector_json, severity_counts_json, findings_json
            )
            VALUES(?1, ?2, ?3, 'running', NULL, 'not_configured', ?4, ?5, '[]', '[]')
            ",
            params![
                run_id,
                site_id,
                timestamp(started_at)?,
                config_fingerprint,
                to_json(baseline_selector)?,
            ],
        )?;
        Ok(())
    }

    pub fn update_config_fingerprint(
        &self,
        run_id: &str,
        config_fingerprint: &str,
    ) -> Result<(), HistoryError> {
        self.conn.execute(
            "UPDATE runs SET config_fingerprint = ?2 WHERE run_id = ?1",
            params![run_id, config_fingerprint],
        )?;
        Ok(())
    }

    pub fn finalize_run(&mut self, finalization: RunFinalization) -> Result<(), HistoryError> {
        let tx = self.conn.transaction()?;
        let current_sequence = tx
            .query_row(
                "SELECT sequence FROM runs WHERE run_id = ?1",
                params![finalization.run_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| {
                HistoryError::InvalidState(format!("unknown run id {:?}", finalization.run_id))
            })?;
        tx.execute(
            "
            UPDATE runs
            SET finished_at = ?2,
                run_state = 'finished',
                audit_result = ?3,
                delivery_result = ?4,
                resolved_baseline_json = ?5,
                aggregate_gate_json = ?6,
                severity_counts_json = ?7,
                findings_json = ?8,
                operational_error = ?9
            WHERE run_id = ?1
            ",
            params![
                finalization.run_id,
                timestamp(finalization.finished_at)?,
                finalization.audit_result.as_str(),
                finalization.delivery_result.as_str(),
                optional_json(&finalization.resolved_baseline)?,
                optional_json(&finalization.aggregate_gate)?,
                to_json(&finalization.severity_counts)?,
                to_json(&finalization.findings)?,
                finalization.operational_error,
            ],
        )?;
        tx.execute(
            "DELETE FROM source_artifacts WHERE run_id = ?1",
            params![finalization.run_id],
        )?;
        for descriptor in &finalization.source_artifacts {
            tx.execute(
                "
                INSERT INTO source_artifacts(
                    run_id, kind, schema_version, role, relative_path, byte_length, sha256, descriptor_json
                )
                VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ",
                params![
                    finalization.run_id,
                    descriptor.kind,
                    descriptor.schema_version,
                    descriptor.role,
                    descriptor.relative_path,
                    descriptor.byte_length,
                    descriptor.sha256,
                    to_json(descriptor)?,
                ],
            )?;
        }
        if let Some(update) = &finalization.baseline_label_update {
            let existing_sequence = tx
                .query_row(
                    "
                    SELECT runs.sequence
                    FROM baseline_labels
                    JOIN runs ON runs.run_id = baseline_labels.run_id
                    WHERE baseline_labels.site_id = ?1 AND baseline_labels.label = ?2
                    ",
                    params![update.site_id, update.label],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if existing_sequence
                .map(|sequence| current_sequence >= sequence)
                .unwrap_or(true)
            {
                tx.execute(
                    "
                    INSERT INTO baseline_labels(site_id, label, descriptor_json, updated_at, source, run_id)
                    VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                    ON CONFLICT(site_id, label)
                    DO UPDATE SET descriptor_json = excluded.descriptor_json,
                                  updated_at = excluded.updated_at,
                                  source = excluded.source,
                                  run_id = excluded.run_id
                    ",
                    params![
                        update.site_id,
                        update.label,
                        to_json(&update.descriptor)?,
                        timestamp(finalization.finished_at)?,
                        update.source,
                        finalization.run_id,
                    ],
                )?;
                tx.execute(
                    "
                    INSERT INTO baseline_events(site_id, label, descriptor_json, created_at, source, run_id)
                    VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                    ",
                    params![
                        update.site_id,
                        update.label,
                        to_json(&update.descriptor)?,
                        timestamp(finalization.finished_at)?,
                        update.source,
                        finalization.run_id,
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn imported_baseline(
        &self,
        site_id: &str,
        selector_key: &str,
    ) -> Result<Option<ArtifactDescriptor>, HistoryError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT descriptor_json FROM baseline_imports WHERE site_id = ?1 AND selector_key = ?2",
                params![site_id, selector_key],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|value| from_json(&value)).transpose()
    }

    pub fn record_imported_baseline(
        &self,
        site_id: &str,
        selector_key: &str,
        descriptor: &ArtifactDescriptor,
        source: &str,
        when: OffsetDateTime,
    ) -> Result<(), HistoryError> {
        self.conn.execute(
            "
            INSERT INTO baseline_imports(site_id, selector_key, descriptor_json, imported_at, source)
            VALUES(?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(site_id, selector_key)
            DO UPDATE SET descriptor_json = baseline_imports.descriptor_json,
                          imported_at = baseline_imports.imported_at,
                          source = baseline_imports.source
            ",
            params![
                site_id,
                selector_key,
                to_json(descriptor)?,
                timestamp(when)?,
                source,
            ],
        )?;
        Ok(())
    }

    pub fn update_delivery(
        &self,
        run_id: &str,
        delivery_result: DeliveryResult,
        delivery_error: Option<&str>,
    ) -> Result<(), HistoryError> {
        self.conn.execute(
            "UPDATE runs SET delivery_result = ?2, delivery_error = ?3 WHERE run_id = ?1",
            params![run_id, delivery_result.as_str(), delivery_error],
        )?;
        Ok(())
    }

    pub fn record_notification(
        &self,
        attempt: &NotificationAttemptRecord,
    ) -> Result<(), HistoryError> {
        self.conn.execute(
            "
            INSERT INTO notification_attempts(
                run_id, hook_id, hook_kind, started_at, finished_at, trigger_reasons_json,
                required, success, status, error
            )
            VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
            params![
                attempt.run_id,
                attempt.hook_id,
                attempt.hook_kind,
                timestamp(attempt.started_at)?,
                timestamp(attempt.finished_at)?,
                to_json(&attempt.trigger_reasons)?,
                bool_int(attempt.required),
                bool_int(attempt.success),
                attempt.status,
                attempt.error,
            ],
        )?;
        Ok(())
    }

    pub fn baseline_label(
        &self,
        site_id: &str,
        label: &str,
    ) -> Result<Option<ArtifactDescriptor>, HistoryError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT descriptor_json FROM baseline_labels WHERE site_id = ?1 AND label = ?2",
                params![site_id, label],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|value| from_json(&value)).transpose()
    }

    pub fn set_baseline_label(
        &mut self,
        site_id: &str,
        label: &str,
        descriptor: &ArtifactDescriptor,
        source: &str,
        run_id: Option<&str>,
        when: OffsetDateTime,
    ) -> Result<(), HistoryError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "
            INSERT INTO baseline_labels(site_id, label, descriptor_json, updated_at, source, run_id)
            VALUES(?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(site_id, label)
            DO UPDATE SET descriptor_json = excluded.descriptor_json,
                          updated_at = excluded.updated_at,
                          source = excluded.source,
                          run_id = excluded.run_id
            ",
            params![
                site_id,
                label,
                to_json(descriptor)?,
                timestamp(when)?,
                source,
                run_id,
            ],
        )?;
        tx.execute(
            "
            INSERT INTO baseline_events(site_id, label, descriptor_json, created_at, source, run_id)
            VALUES(?1, ?2, ?3, ?4, ?5, ?6)
            ",
            params![
                site_id,
                label,
                to_json(descriptor)?,
                timestamp(when)?,
                source,
                run_id,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_runs(&self, site_id: &str, limit: usize) -> Result<Vec<RunListItem>, HistoryError> {
        let mut statement = self.conn.prepare(
            "
            SELECT r.sequence, r.run_id, r.site_id, r.started_at, r.finished_at,
                   r.run_state, r.audit_result, r.delivery_result, r.aggregate_gate_json,
                   r.operational_error, r.delivery_error,
                   (SELECT COUNT(*) FROM source_artifacts a WHERE a.run_id = r.run_id)
            FROM runs r
            WHERE r.site_id = ?1
            ORDER BY r.sequence DESC
            LIMIT ?2
            ",
        )?;
        let rows = statement.query_map(params![site_id, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, i64>(11)?,
            ))
        })?;
        let mut output = Vec::new();
        for row in rows {
            let (
                sequence,
                run_id,
                site_id,
                started_at,
                finished_at,
                run_state,
                audit_result,
                delivery_result,
                aggregate_gate,
                operational_error,
                delivery_error,
                artifact_count,
            ) = row?;
            let aggregate_gate: Option<AggregateGateSummary> = aggregate_gate
                .map(|value| from_json(&value).map(normalize_aggregate_gate_components))
                .transpose()?;
            output.push(RunListItem {
                sequence,
                run_id,
                site_id,
                started_at: parse_timestamp(&started_at)?,
                finished_at: finished_at.as_deref().map(parse_timestamp).transpose()?,
                run_state: RunState::from_str(&run_state)?,
                audit_result: AuditResult::from_optional(audit_result)?,
                delivery_result: DeliveryResult::from_str(&delivery_result)?,
                effective_max_severity: aggregate_gate
                    .as_ref()
                    .and_then(|gate| gate.effective_max_severity),
                source_artifact_count: artifact_count as usize,
                operational_error,
                delivery_error,
            });
        }
        Ok(output)
    }

    pub fn run(&self, run_id: &str) -> Result<RunRecord, HistoryError> {
        let row: Option<RunRow> = self
            .conn
            .query_row(
                "
                SELECT sequence, run_id, site_id, started_at, finished_at, run_state,
                       audit_result, delivery_result, config_fingerprint, baseline_selector_json,
                       resolved_baseline_json, aggregate_gate_json, severity_counts_json,
                       findings_json, operational_error, delivery_error
                FROM runs WHERE run_id = ?1
                ",
                params![run_id],
                |row| {
                    Ok(RunRow {
                        sequence: row.get(0)?,
                        run_id: row.get(1)?,
                        site_id: row.get(2)?,
                        started_at: row.get(3)?,
                        finished_at: row.get(4)?,
                        run_state: row.get(5)?,
                        audit_result: row.get(6)?,
                        delivery_result: row.get(7)?,
                        config_fingerprint: row.get(8)?,
                        baseline_selector_json: row.get(9)?,
                        resolved_baseline_json: row.get(10)?,
                        aggregate_gate_json: row.get(11)?,
                        severity_counts_json: row.get(12)?,
                        findings_json: row.get(13)?,
                        operational_error: row.get(14)?,
                        delivery_error: row.get(15)?,
                    })
                },
            )
            .optional()?;
        let Some(row) = row else {
            return Err(HistoryError::NotFound(format!("run {run_id:?}")));
        };
        let source_artifacts = self.source_artifacts(run_id)?;
        Ok(RunRecord {
            sequence: row.sequence,
            run_id: row.run_id,
            site_id: row.site_id,
            started_at: parse_timestamp(&row.started_at)?,
            finished_at: row
                .finished_at
                .as_deref()
                .map(parse_timestamp)
                .transpose()?,
            run_state: RunState::from_str(&row.run_state)?,
            audit_result: AuditResult::from_optional(row.audit_result)?,
            delivery_result: DeliveryResult::from_str(&row.delivery_result)?,
            config_fingerprint: row.config_fingerprint,
            baseline_selector: from_json(&row.baseline_selector_json)?,
            resolved_baseline: row
                .resolved_baseline_json
                .as_deref()
                .map(from_json)
                .transpose()?,
            aggregate_gate: row
                .aggregate_gate_json
                .as_deref()
                .map(from_json)
                .transpose()?
                .map(normalize_aggregate_gate_components),
            severity_counts: from_json(&row.severity_counts_json)?,
            findings: from_json(&row.findings_json)?,
            operational_error: row.operational_error,
            delivery_error: row.delivery_error,
            source_artifacts,
        })
    }

    pub fn run_for_site(&self, run_id: &str, site_id: &str) -> Result<RunRecord, HistoryError> {
        let run = self.run(run_id)?;
        if run.site_id != site_id {
            return Err(HistoryError::NotFound(format!(
                "run {run_id:?} for site {site_id:?}"
            )));
        }
        Ok(run)
    }

    pub fn latest_mark_candidate(&self, site_id: &str) -> Result<RunRecord, HistoryError> {
        self.latest_candidate(site_id, true)
    }

    pub fn latest_bundle_candidate(&self, site_id: &str) -> Result<RunRecord, HistoryError> {
        self.latest_candidate(site_id, true)
    }

    fn latest_candidate(
        &self,
        site_id: &str,
        require_diff: bool,
    ) -> Result<RunRecord, HistoryError> {
        let mut statement = self.conn.prepare(
            "
            SELECT run_id
            FROM runs
            WHERE site_id = ?1
              AND run_state = 'finished'
              AND audit_result IN ('passed', 'gate_failed')
            ORDER BY sequence DESC
            ",
        )?;
        let ids = statement
            .query_map(params![site_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            let run = self.run(&id)?;
            let has_snapshot = run.source_artifact("snapshot", "current").is_some();
            let has_diff = run.source_artifact("report", "current_report").is_some();
            if has_snapshot && (!require_diff || has_diff) {
                return Ok(run);
            }
        }
        Err(HistoryError::NotFound(format!(
            "finished run with a report for site {site_id:?}"
        )))
    }

    pub fn source_artifacts(&self, run_id: &str) -> Result<Vec<ArtifactDescriptor>, HistoryError> {
        let mut statement = self.conn.prepare(
            "SELECT descriptor_json FROM source_artifacts WHERE run_id = ?1 ORDER BY role, kind, relative_path",
        )?;
        let rows = statement
            .query_map(params![run_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows.iter().map(|row| from_json(row)).collect()
    }

    pub fn insert_bundle_pending(&self, record: &BundleRecord) -> Result<(), HistoryError> {
        self.conn.execute(
            "
            INSERT INTO bundle_records(
                bundle_id, run_id, site_id, origin, state, created_at, finished_at,
                trigger_reason, required, root_locator, error
            )
            VALUES(?1, ?2, ?3, ?4, 'pending', ?5, NULL, ?6, ?7, ?8, NULL)
            ",
            params![
                record.bundle_id,
                record.run_id,
                record.site_id,
                record.origin.as_str(),
                timestamp(record.created_at)?,
                record.trigger_reason,
                record.required.map(bool_int),
                record.root_locator,
            ],
        )?;
        Ok(())
    }

    pub fn complete_bundle(
        &mut self,
        bundle_id: &str,
        finished_at: OffsetDateTime,
        files: &[BundleFileDescriptor],
    ) -> Result<(), HistoryError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE bundle_records SET state = 'completed', finished_at = ?2, error = NULL WHERE bundle_id = ?1",
            params![bundle_id, timestamp(finished_at)?],
        )?;
        tx.execute(
            "DELETE FROM bundle_files WHERE bundle_id = ?1",
            params![bundle_id],
        )?;
        for file in files {
            tx.execute(
                "
                INSERT INTO bundle_files(
                    bundle_id, path, role, artifact_kind, schema_version, media_type,
                    byte_length, sha256, record_count, truncation_json
                )
                VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ",
                params![
                    bundle_id,
                    file.path,
                    file.role,
                    file.artifact_kind,
                    file.schema_version,
                    file.media_type,
                    file.byte_length,
                    file.sha256,
                    file.record_count.map(|value| value as i64),
                    optional_json(&file.truncation)?,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn fail_bundle(
        &self,
        bundle_id: &str,
        finished_at: OffsetDateTime,
        error: &str,
    ) -> Result<(), HistoryError> {
        self.conn.execute(
            "UPDATE bundle_records SET state = 'failed', finished_at = ?2, error = ?3 WHERE bundle_id = ?1",
            params![bundle_id, timestamp(finished_at)?, bounded_line(error, 500)],
        )?;
        Ok(())
    }
}

impl RunRecord {
    pub fn source_artifact(&self, kind: &str, role: &str) -> Option<&ArtifactDescriptor> {
        self.source_artifacts
            .iter()
            .find(|descriptor| descriptor.kind == kind && descriptor.role == role)
    }
}

pub fn new_run_id(now: OffsetDateTime) -> String {
    let counter = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let mut hasher = Sha256::new();
    hasher.update(now.unix_timestamp_nanos().to_string().as_bytes());
    hasher.update(std::process::id().to_string().as_bytes());
    hasher.update(counter.to_string().as_bytes());
    let digest = hasher.finalize();
    format!("{stamp}-{}", hex_prefix(&digest, 4))
}

pub fn new_bundle_id(now: OffsetDateTime) -> String {
    let runish = new_run_id(now);
    format!("bnd_{}_{}", &runish[..16], &runish[17..])
}

pub fn artifact_descriptor_for_file(
    roots: &ArtifactRoots,
    storage: ManagedArtifactStorage,
    relative_path: impl Into<String>,
    kind: impl Into<String>,
    schema_version: u16,
    role: impl Into<String>,
) -> Result<ArtifactDescriptor, HistoryError> {
    let relative_path = relative_path.into();
    let path = descriptor_path(roots, storage, &relative_path)?;
    let bytes = fs::read(&path)?;
    Ok(ArtifactDescriptor {
        storage,
        kind: kind.into(),
        schema_version,
        role: role.into(),
        relative_path,
        byte_length: bytes.len() as u64,
        sha256: sha256_hex(&bytes),
    })
}

pub fn validate_descriptor(
    roots: &ArtifactRoots,
    descriptor: &ArtifactDescriptor,
) -> Result<PathBuf, HistoryError> {
    validate_relative_path(&descriptor.relative_path)?;
    let path = descriptor_path(roots, descriptor.storage, &descriptor.relative_path)?;
    let root = storage_root(roots, descriptor.storage);
    let canonical_root = root.canonicalize().map_err(|error| {
        HistoryError::InvalidDescriptor(format!(
            "the folder {} is not usable: {error}",
            root.display()
        ))
    })?;
    let canonical_path = path.canonicalize().map_err(|error| {
        HistoryError::InvalidDescriptor(format!(
            "{} cannot be read: {error}",
            descriptor.relative_path
        ))
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(HistoryError::InvalidDescriptor(
            "the file is outside the history folder".to_owned(),
        ));
    }
    let bytes = fs::read(&canonical_path)?;
    if bytes.len() as u64 != descriptor.byte_length {
        return Err(HistoryError::InvalidDescriptor(format!(
            "{} has {} bytes, but the history recorded {}",
            descriptor.relative_path,
            bytes.len(),
            descriptor.byte_length
        )));
    }
    let observed = sha256_hex(&bytes);
    if observed != descriptor.sha256 {
        return Err(HistoryError::InvalidDescriptor(format!(
            "the SHA-256 checksum of {} does not match",
            descriptor.relative_path
        )));
    }
    Ok(canonical_path)
}

pub fn read_descriptor_bytes(
    roots: &ArtifactRoots,
    descriptor: &ArtifactDescriptor,
) -> Result<Vec<u8>, HistoryError> {
    let path = validate_descriptor(roots, descriptor)?;
    fs::read(path).map_err(Into::into)
}

pub fn import_snapshot_baseline(
    baseline_root: &Path,
    source_path: &Path,
) -> Result<ArtifactDescriptor, HistoryError> {
    let bytes = fs::read(source_path).map_err(|error| {
        HistoryError::InvalidArtifact(format!(
            "cannot read the baseline snapshot {}: {error}",
            source_path.display()
        ))
    })?;
    let raw = std::str::from_utf8(&bytes).map_err(|error| {
        HistoryError::InvalidArtifact(format!(
            "the baseline snapshot {} is not UTF-8 text: {error}",
            source_path.display()
        ))
    })?;
    snapshot_from_str(raw).map_err(|error| {
        HistoryError::InvalidArtifact(format!(
            "the baseline snapshot {} is not usable: {error}",
            source_path.display()
        ))
    })?;
    let sha = sha256_hex(&bytes);
    fs::create_dir_all(baseline_root)?;
    let file_name = format!("{sha}.json");
    let destination = baseline_root.join(&file_name);
    if !destination.exists() {
        write_new_atomically(&destination, &bytes)?;
    } else {
        let existing = fs::read(&destination)?;
        if existing != bytes {
            return Err(HistoryError::InvalidArtifact(
                "a different baseline with the same SHA-256 checksum is already stored".to_owned(),
            ));
        }
    }
    let roots = ArtifactRoots {
        run_artifact_root: PathBuf::from("."),
        baseline_root: baseline_root.to_path_buf(),
    };
    artifact_descriptor_for_file(
        &roots,
        ManagedArtifactStorage::Baselines,
        file_name,
        "snapshot",
        crate::snapshot::SNAPSHOT_SCHEMA_VERSION,
        "baseline",
    )
}

pub fn write_new_atomically(path: &Path, bytes: &[u8]) -> Result<(), HistoryError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let parent = crate::snapshot::parent_dir(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
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
    let temp_path = temp_path
        .ok_or_else(|| HistoryError::Io("could not create a temporary file".to_owned()))?;
    let mut temp_file = temp_file.expect("temporary file was opened with its path");
    let result = (|| -> Result<(), HistoryError> {
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

pub fn publish_directory_atomically(staged: &Path, final_path: &Path) -> Result<(), HistoryError> {
    if final_path.exists() {
        return Err(HistoryError::Io(format!(
            "{} already exists",
            final_path.display()
        )));
    }
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(staged, final_path)?;
    Ok(())
}

pub fn normalized_findings(
    site_id: &str,
    run_id: &str,
    report: &Report,
    log_report: Option<&LogAnalysisReport>,
) -> Vec<FindingSummary> {
    let mut findings = Vec::new();
    for finding in &report.findings {
        let provenance = match finding.rule {
            Rule::Assertion => "assertion",
            Rule::QualityBudget => "quality",
            _ => "crawl",
        };
        findings.push(FindingSummary {
            id: format!(
                "{provenance}/{}/{}/{}",
                finding.rule.id(),
                percent_component(finding.url.as_deref().unwrap_or("site")),
                percent_component(finding.group.as_deref().unwrap_or(""))
            ),
            provenance: provenance.to_owned(),
            site_id: site_id.to_owned(),
            run_id: run_id.to_owned(),
            rule: finding.rule.id().to_owned(),
            url: finding.url.clone(),
            group_key: finding.group.clone(),
            severity: Some(finding.severity),
            effective_severity: (!finding.ignored).then_some(finding.severity),
            ignored: finding.ignored,
            gate_relevant: report.fails_gate(finding),
            message: finding.message.clone(),
            impact_context: finding.impact.clone(),
        });
    }
    if let Some(log_report) = log_report {
        for finding in &log_report.findings {
            let log_targets = match &finding.normalized_url {
                Some(url) => vec![url.clone()],
                None => log_report
                    .url_activity
                    .iter()
                    .filter(|row| row.group_key == finding.group_key)
                    .map(|row| row.normalized_url.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            };
            findings.push(FindingSummary {
                id: finding.id.clone(),
                provenance: "log".to_owned(),
                site_id: site_id.to_owned(),
                run_id: run_id.to_owned(),
                rule: finding.finding_type.as_str().to_owned(),
                url: finding.normalized_url.clone(),
                group_key: Some(finding.group_key.clone()),
                severity: Some(finding.raw_severity),
                effective_severity: finding.effective_severity,
                ignored: finding.effective_severity.is_none(),
                gate_relevant: log_report.gate.gate_enabled && finding.effective_severity.is_some(),
                message: finding.message.clone(),
                impact_context: annotate_log_finding_targets(
                    report.impact.as_ref(),
                    &log_targets,
                    report
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.start_url.as_str())
                        .unwrap_or(&report.current.start_url),
                    &report.current.start_url,
                    finding.normalized_url.is_none(),
                ),
            });
        }
    }
    sort_finding_summaries(&mut findings);
    findings
}

pub fn aggregate_gate_summary(
    report: &Report,
    log_report: Option<&LogAnalysisReport>,
) -> AggregateGateSummary {
    let mut per_artifact = vec![ArtifactGateSummary {
        component: "core".to_owned(),
        artifact_kind: report.artifact_kind.clone(),
        gate_enabled: true,
        threshold: Some(report.fail_on),
        raw_max_severity: report.findings.iter().map(|finding| finding.severity).max(),
        effective_max_severity: report
            .findings
            .iter()
            .filter(|finding| !finding.ignored)
            .map(|finding| finding.severity)
            .max(),
        decision: Some(if report.passed {
            GateDecision::Passed
        } else {
            GateDecision::Failed
        }),
    }];
    if let Some(log_report) = log_report {
        per_artifact.push(ArtifactGateSummary {
            component: "logs".to_owned(),
            artifact_kind: log_report.artifact_kind.clone(),
            gate_enabled: log_report.gate.gate_enabled,
            threshold: log_report.gate.threshold,
            raw_max_severity: log_report.severity.raw_max_severity,
            effective_max_severity: log_report.severity.effective_max_severity,
            decision: log_report.gate.gate_decision,
        });
    }
    let gate_decision = if per_artifact
        .iter()
        .any(|summary| summary.decision == Some(GateDecision::Failed))
    {
        GateDecision::Failed
    } else {
        GateDecision::Passed
    };
    let raw_max_severity = per_artifact
        .iter()
        .filter_map(|summary| summary.raw_max_severity)
        .max();
    let effective_max_severity = per_artifact
        .iter()
        .filter(|summary| summary.gate_enabled)
        .filter_map(|summary| summary.effective_max_severity)
        .max();
    AggregateGateSummary {
        gate_decision,
        raw_max_severity,
        effective_max_severity,
        per_artifact,
    }
}

pub fn severity_counts(findings: &[FindingSummary]) -> Vec<SeverityCount> {
    let mut counts = BTreeMap::new();
    for finding in findings {
        if let Some(severity) = finding.severity {
            *counts
                .entry((finding.provenance.clone(), finding.ignored, severity))
                .or_insert(0usize) += 1;
        }
    }
    counts
        .into_iter()
        .map(|((provenance, ignored, severity), count)| SeverityCount {
            provenance,
            ignored,
            severity,
            count,
        })
        .collect()
}

pub fn snapshot_metadata(snapshot: &Snapshot) -> SnapshotMetadata {
    SnapshotMetadata {
        start_url: snapshot.start_url.clone(),
        created_at: snapshot.created_at,
        auditroo_version: snapshot.auditroo_version.clone(),
        schema_version: snapshot.schema_version,
    }
}

pub fn bounded_line(input: &str, max_chars: usize) -> String {
    let sanitized = input.replace(['\n', '\r'], " ");
    let mut output = sanitized.chars().take(max_chars).collect::<String>();
    if sanitized.chars().count() > max_chars {
        output.push_str("...");
    }
    output
}

fn sort_finding_summaries(findings: &mut [FindingSummary]) {
    findings.sort_by(|left, right| {
        left.ignored
            .cmp(&right.ignored)
            .then_with(|| severity_sort_key(right.severity).cmp(&severity_sort_key(left.severity)))
            .then_with(|| left.provenance.cmp(&right.provenance))
            .then_with(|| {
                left.url
                    .as_deref()
                    .or(left.group_key.as_deref())
                    .unwrap_or("")
                    .cmp(
                        right
                            .url
                            .as_deref()
                            .or(right.group_key.as_deref())
                            .unwrap_or(""),
                    )
            })
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn severity_sort_key(severity: Option<Severity>) -> usize {
    match severity {
        Some(Severity::Critical) => 5,
        Some(Severity::High) => 4,
        Some(Severity::Medium) => 3,
        Some(Severity::Low) => 2,
        Some(Severity::Info) => 1,
        None => 0,
    }
}

fn percent_component(input: &str) -> String {
    let mut output = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn descriptor_path(
    roots: &ArtifactRoots,
    storage: ManagedArtifactStorage,
    relative_path: &str,
) -> Result<PathBuf, HistoryError> {
    validate_relative_path(relative_path)?;
    Ok(storage_root(roots, storage).join(relative_path))
}

fn storage_root(roots: &ArtifactRoots, storage: ManagedArtifactStorage) -> &Path {
    match storage {
        ManagedArtifactStorage::RunArtifacts => &roots.run_artifact_root,
        ManagedArtifactStorage::Baselines => &roots.baseline_root,
    }
}

fn validate_relative_path(relative_path: &str) -> Result<(), HistoryError> {
    if relative_path.is_empty() {
        return Err(HistoryError::InvalidDescriptor(
            "a file path in the history is empty".to_owned(),
        ));
    }
    let path = Path::new(relative_path);
    if path.is_absolute() {
        return Err(HistoryError::InvalidDescriptor(
            "a file path in the history is absolute".to_owned(),
        ));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(HistoryError::InvalidDescriptor(
                    "a file path in the history points outside its folder".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn to_json<T: Serialize>(value: &T) -> Result<String, HistoryError> {
    serde_json::to_string(value).map_err(|error| HistoryError::Serialization(error.to_string()))
}

fn optional_json<T: Serialize>(value: &Option<T>) -> Result<Option<String>, HistoryError> {
    value.as_ref().map(to_json).transpose()
}

fn from_json<T: DeserializeOwned>(raw: &str) -> Result<T, HistoryError> {
    serde_json::from_str(raw).map_err(|error| HistoryError::Serialization(error.to_string()))
}

fn normalize_aggregate_gate_components(mut gate: AggregateGateSummary) -> AggregateGateSummary {
    for component in &mut gate.per_artifact {
        if component.component == "core" && component.artifact_kind == "log_analysis_report" {
            component.component = "logs".to_owned();
        }
    }
    gate
}

fn timestamp(value: OffsetDateTime) -> Result<String, HistoryError> {
    let utc = value
        .to_offset(UtcOffset::UTC)
        .replace_nanosecond(0)
        .map_err(|error| HistoryError::InvalidState(error.to_string()))?;
    utc.format(&Rfc3339)
        .map_err(|error| HistoryError::InvalidState(error.to_string()))
}

fn parse_timestamp(raw: &str) -> Result<OffsetDateTime, HistoryError> {
    OffsetDateTime::parse(raw, &Rfc3339)
        .map_err(|error| HistoryError::InvalidState(format!("invalid timestamp: {error}")))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_prefix(&digest, digest.len())
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    let mut output = String::with_capacity(len * 2);
    for byte in bytes.iter().take(len) {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn bool_int(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(name: &str) -> ArtifactDescriptor {
        ArtifactDescriptor {
            storage: ManagedArtifactStorage::RunArtifacts,
            kind: "snapshot".to_owned(),
            schema_version: 2,
            role: "current".to_owned(),
            relative_path: format!("site/{name}/snapshot.json"),
            byte_length: 2,
            sha256: format!("{name:0<64}"),
        }
    }

    fn passed_finalization(
        run_id: &str,
        when: OffsetDateTime,
        descriptor: ArtifactDescriptor,
    ) -> RunFinalization {
        RunFinalization {
            run_id: run_id.to_owned(),
            finished_at: when,
            audit_result: AuditResult::Passed,
            delivery_result: DeliveryResult::NotConfigured,
            resolved_baseline: None,
            aggregate_gate: None,
            severity_counts: Vec::new(),
            findings: Vec::new(),
            operational_error: None,
            source_artifacts: vec![descriptor.clone()],
            baseline_label_update: Some(BaselineLabelUpdate {
                site_id: "site".to_owned(),
                label: "last-known-good".to_owned(),
                descriptor,
                source: "auto_promotion".to_owned(),
            }),
        }
    }

    #[test]
    fn automatic_label_promotion_keeps_newer_run_when_older_run_finishes_late() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = dir.path().join("history.sqlite3");
        let mut store = HistoryStore::open_or_create(&db).expect("history opens");
        let started = OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .expect("timestamp normalizes");
        let baseline_selector = serde_json::json!({"mode": "label", "label": "last-known-good"});
        store
            .begin_run("old-run", "site", started, None, &baseline_selector)
            .expect("old run starts");
        store
            .begin_run(
                "new-run",
                "site",
                started + time::Duration::seconds(1),
                None,
                &baseline_selector,
            )
            .expect("new run starts");

        let old_descriptor = descriptor("old");
        let new_descriptor = descriptor("new");
        store
            .finalize_run(passed_finalization(
                "new-run",
                started + time::Duration::seconds(3),
                new_descriptor.clone(),
            ))
            .expect("newer run finalizes");
        store
            .finalize_run(passed_finalization(
                "old-run",
                started + time::Duration::seconds(4),
                old_descriptor,
            ))
            .expect("older run finalizes");

        let promoted = store
            .baseline_label("site", "last-known-good")
            .expect("label query")
            .expect("label exists");
        assert_eq!(promoted, new_descriptor);
    }
}
