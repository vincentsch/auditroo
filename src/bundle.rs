//! A bundle is a folder with the HTML report and the data of one monitor
//! run, to send to a client or attach to a ticket. It holds:
//!
//! - `index.html`, the same report as `--html`
//! - `report.json` and `snapshot.json`
//! - `findings.csv`
//! - the log analysis, when the run had one
//! - `manifest.json` with the SHA-256 hash of each file

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::analysis::log_report_from_str;
use crate::history::{
    new_bundle_id, normalized_findings, read_descriptor_bytes, validate_descriptor,
    write_new_atomically, ArtifactRoots, BundleFileDescriptor, BundleOrigin, BundleRecord,
    BundleState, FindingSummary, HistoryError, HistoryStore, RunRecord, RUN_LOG_REPORT_FILE,
    RUN_REPORT_FILE, RUN_SNAPSHOT_FILE,
};
use crate::report::report_from_str;

pub const BUNDLE_MANIFEST_ARTIFACT_KIND: &str = "bundle_manifest";
pub const BUNDLE_COMMAND_RESULT_ARTIFACT_KIND: &str = "bundle_command_result";
pub const BUNDLE_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone)]
pub struct BundleRequest {
    pub site_id: String,
    pub run_id: String,
    pub origin: BundleOrigin,
    pub destination: PathBuf,
    pub display_path: String,
    pub generated_at: OffsetDateTime,
    pub trigger_reason: Option<String>,
    pub required: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BundleCommandResult {
    pub artifact_kind: String,
    pub schema_version: u16,
    pub ok: bool,
    pub site_id: String,
    pub run_id: String,
    pub bundle_id: String,
    pub origin: BundleOrigin,
    pub destination: BundleDestination,
    pub files: Vec<BundleResultFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BundleDestination {
    pub kind: String,
    pub display_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BundleResultFile {
    pub path: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleError {
    History(HistoryError),
    Io(String),
    InvalidSource(String),
    InvalidDestination(String),
}

impl fmt::Display for BundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::History(error) => write!(formatter, "{error}"),
            Self::Io(detail) => write!(formatter, "could not write the bundle: {detail}"),
            Self::InvalidSource(detail) => {
                write!(formatter, "the run's files are not usable: {detail}")
            }
            Self::InvalidDestination(detail) => write!(formatter, "{detail}"),
        }
    }
}

impl Error for BundleError {}

impl From<HistoryError> for BundleError {
    fn from(error: HistoryError) -> Self {
        Self::History(error)
    }
}

impl From<io::Error> for BundleError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Debug, Serialize)]
struct BundleManifest<'a> {
    artifact_kind: &'static str,
    schema_version: u16,
    auditroo_version: &'static str,
    site_id: &'a str,
    run_id: &'a str,
    bundle_id: &'a str,
    #[serde(with = "time::serde::rfc3339")]
    generated_at: OffsetDateTime,
    files: &'a [BundleFileDescriptor],
}

pub fn generate_bundle(
    store: &mut HistoryStore,
    roots: &ArtifactRoots,
    run: &RunRecord,
    request: BundleRequest,
) -> Result<BundleCommandResult, BundleError> {
    if request.destination.exists() {
        return Err(BundleError::InvalidDestination(format!(
            "{} already exists. Choose a new folder for the bundle",
            request.destination.display()
        )));
    }
    let bundle_id = new_bundle_id(request.generated_at);
    store.insert_bundle_pending(&BundleRecord {
        bundle_id: bundle_id.clone(),
        run_id: run.run_id.clone(),
        site_id: run.site_id.clone(),
        origin: request.origin,
        state: BundleState::Pending,
        created_at: request.generated_at,
        finished_at: None,
        trigger_reason: request.trigger_reason.clone(),
        required: request.required,
        root_locator: request.destination.display().to_string(),
        error: None,
    })?;

    match write_bundle(roots, run, &request, &bundle_id) {
        Ok(files) => {
            store.complete_bundle(&bundle_id, OffsetDateTime::now_utc(), &files)?;
            Ok(BundleCommandResult {
                artifact_kind: BUNDLE_COMMAND_RESULT_ARTIFACT_KIND.to_owned(),
                schema_version: BUNDLE_SCHEMA_VERSION,
                ok: true,
                site_id: request.site_id,
                run_id: request.run_id,
                bundle_id,
                origin: request.origin,
                destination: BundleDestination {
                    kind: match request.origin {
                        BundleOrigin::Explicit => "explicit",
                        BundleOrigin::Automatic => "automatic",
                    }
                    .to_owned(),
                    display_path: request.display_path,
                },
                files: files
                    .iter()
                    .map(|file| BundleResultFile {
                        path: file.path.clone(),
                        role: file.role.clone(),
                    })
                    .collect(),
            })
        }
        Err(error) => {
            let _ = store.fail_bundle(&bundle_id, OffsetDateTime::now_utc(), &error.to_string());
            Err(error)
        }
    }
}

fn write_bundle(
    roots: &ArtifactRoots,
    run: &RunRecord,
    request: &BundleRequest,
    bundle_id: &str,
) -> Result<Vec<BundleFileDescriptor>, BundleError> {
    let read = |kind: &str, role: &str| -> Result<Option<Vec<u8>>, BundleError> {
        match run.source_artifact(kind, role) {
            Some(descriptor) => {
                validate_descriptor(roots, descriptor)?;
                Ok(Some(read_descriptor_bytes(roots, descriptor)?))
            }
            None => Ok(None),
        }
    };
    let report_bytes = read("report", "current_report")?
        .ok_or_else(|| BundleError::InvalidSource("the run has no report".to_owned()))?;
    let snapshot_bytes = read("snapshot", "current")?
        .ok_or_else(|| BundleError::InvalidSource("the run has no snapshot".to_owned()))?;
    let log_bytes = read("log_analysis_report", "log_analysis")?;

    let report = report_from_str(&String::from_utf8_lossy(&report_bytes))
        .map_err(|error| BundleError::InvalidSource(error.to_string()))?;
    let log_report = match &log_bytes {
        Some(bytes) => Some(
            log_report_from_str(&String::from_utf8_lossy(bytes))
                .map_err(|error| BundleError::InvalidSource(error.to_string()))?,
        ),
        None => None,
    };
    let findings = normalized_findings(&run.site_id, &run.run_id, &report, log_report.as_ref());

    let stage = staging_dir(&request.destination)?;
    fs::create_dir_all(&stage)?;
    let result = (|| {
        let mut files = Vec::new();
        let mut add = |path: &str, role: &str, media_type: &str, bytes: &[u8]| {
            write_new_atomically(&stage.join(path), bytes)?;
            files.push(BundleFileDescriptor {
                path: path.to_owned(),
                role: role.to_owned(),
                artifact_kind: role.to_owned(),
                schema_version: None,
                media_type: media_type.to_owned(),
                byte_length: bytes.len() as u64,
                sha256: sha256_hex(bytes),
                record_count: None,
                truncation: None,
            });
            Ok::<(), BundleError>(())
        };
        let html = crate::render::html::render_with_logs(&report, log_report.as_ref());
        add("index.html", "html_report", "text/html", html.as_bytes())?;
        add(RUN_REPORT_FILE, "report", "application/json", &report_bytes)?;
        add(
            RUN_SNAPSHOT_FILE,
            "snapshot",
            "application/json",
            &snapshot_bytes,
        )?;
        add(
            "findings.csv",
            "findings_csv",
            "text/csv",
            findings_csv(&findings)?.as_bytes(),
        )?;
        if let Some(bytes) = &log_bytes {
            add(
                RUN_LOG_REPORT_FILE,
                "log_analysis_report",
                "application/json",
                bytes,
            )?;
        }
        let manifest = serde_json::to_string_pretty(&BundleManifest {
            artifact_kind: BUNDLE_MANIFEST_ARTIFACT_KIND,
            schema_version: BUNDLE_SCHEMA_VERSION,
            auditroo_version: env!("CARGO_PKG_VERSION"),
            site_id: &run.site_id,
            run_id: &run.run_id,
            bundle_id,
            generated_at: request
                .generated_at
                .replace_nanosecond(0)
                .unwrap_or(request.generated_at),
            files: &files,
        })
        .map_err(|error| BundleError::Io(error.to_string()))?;
        write_new_atomically(
            &stage.join("manifest.json"),
            format!("{manifest}\n").as_bytes(),
        )?;
        Ok(files)
    })();
    match result {
        Ok(files) => {
            if request.destination.exists() {
                let _ = fs::remove_dir_all(&stage);
                return Err(BundleError::InvalidDestination(format!(
                    "{} already exists",
                    request.destination.display()
                )));
            }
            fs::rename(&stage, &request.destination)?;
            Ok(files)
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&stage);
            Err(error)
        }
    }
}

fn findings_csv(findings: &[FindingSummary]) -> Result<String, BundleError> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    let csv_error = |error: csv::Error| BundleError::Io(error.to_string());
    writer
        .write_record([
            "severity",
            "fails_gate",
            "ignored",
            "rule",
            "url",
            "message",
            "source",
        ])
        .map_err(csv_error)?;
    for finding in findings {
        writer
            .write_record([
                finding
                    .severity
                    .map(|severity| severity.as_str())
                    .unwrap_or(""),
                if finding.gate_relevant { "yes" } else { "no" },
                if finding.ignored { "yes" } else { "no" },
                &finding.rule,
                &formula_safe(finding.url.as_deref().unwrap_or("")),
                &formula_safe(&finding.message),
                &finding.provenance,
            ])
            .map_err(csv_error)?;
    }
    let bytes = writer
        .into_inner()
        .map_err(|error| BundleError::Io(error.to_string()))?;
    String::from_utf8(bytes).map_err(|error| BundleError::Io(error.to_string()))
}

/// Spreadsheet apps treat a cell that starts with = + - or @ as a formula,
/// so those values get a leading quote.
fn formula_safe(value: &str) -> String {
    if value.starts_with(['=', '+', '-', '@']) {
        format!("'{value}")
    } else {
        value.to_owned()
    }
}

fn staging_dir(destination: &Path) -> Result<PathBuf, BundleError> {
    let parent = crate::snapshot::parent_dir(destination);
    fs::create_dir_all(parent)?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("bundle");
    (0..1000u32)
        .map(|attempt| parent.join(format!(".{name}.tmp-{}-{attempt}", std::process::id())))
        .find(|candidate| !candidate.exists())
        .ok_or_else(|| BundleError::Io("could not create a temporary folder".to_owned()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
