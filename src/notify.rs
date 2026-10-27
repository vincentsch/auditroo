//! Notifications after a monitor run. A command hook gets a JSON summary on
//! stdin. A webhook gets it as a POST request, without local file paths.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

use crate::config::{NotificationHookConfig, NotificationHookKind, NotificationTrigger};
use crate::history::{
    bounded_line, ArtifactDescriptor, ArtifactGateSummary, AuditResult, DeliveryResult,
    FindingSummary, NotificationAttemptRecord, SeverityCount,
};

const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_TOP_FINDINGS: usize = 20;

#[derive(Debug, Clone)]
pub struct NotificationContext {
    pub site_id: String,
    pub run_id: String,
    pub audit_result: AuditResult,
    pub aggregate_gate: Option<crate::history::AggregateGateSummary>,
    pub severity_counts: Vec<SeverityCount>,
    pub source_artifacts: Vec<ArtifactDescriptor>,
    pub local_artifact_paths: BTreeMap<String, String>,
    pub findings: Vec<FindingSummary>,
    pub delivery_result_before: DeliveryResult,
    /// Headline and summary lines of the report, for the `text` field.
    pub summary: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NotificationRunResult {
    pub attempts: Vec<NotificationAttemptRecord>,
    pub delivery_result: DeliveryResult,
    pub error_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationError {
    Payload(String),
}

impl fmt::Display for NotificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payload(detail) => {
                write!(formatter, "could not build the notification: {detail}")
            }
        }
    }
}

impl Error for NotificationError {}

#[derive(Debug, Serialize)]
struct NotificationPayload<'a> {
    /// A plain-text summary that chat tools can show as the message.
    text: String,
    artifact_kind: &'static str,
    schema_version: u16,
    auditroo_version: &'static str,
    site_id: &'a str,
    run_id: &'a str,
    audit_result: AuditResult,
    trigger_reasons: &'a [String],
    aggregate_gate: Option<&'a crate::history::AggregateGateSummary>,
    per_artifact_gate: Vec<&'a ArtifactGateSummary>,
    severity_counts: &'a [SeverityCount],
    source_artifacts: &'a [ArtifactDescriptor],
    #[serde(skip_serializing_if = "Option::is_none")]
    local_artifact_paths: Option<&'a BTreeMap<String, String>>,
    delivery_result_before: DeliveryResult,
    top_findings: Vec<NotificationFindingProjection<'a>>,
}

#[derive(Debug, Serialize)]
struct NotificationFindingProjection<'a> {
    id: &'a str,
    provenance: &'a str,
    rule: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    severity: Option<crate::findings::Severity>,
    gate_relevant: bool,
    message: &'a str,
}

impl<'a> From<&'a FindingSummary> for NotificationFindingProjection<'a> {
    fn from(finding: &'a FindingSummary) -> Self {
        Self {
            id: &finding.id,
            provenance: &finding.provenance,
            rule: &finding.rule,
            url: finding.url.as_deref(),
            severity: finding.severity,
            gate_relevant: finding.gate_relevant,
            message: &finding.message,
        }
    }
}

pub async fn attempt_notifications(
    hooks: &[NotificationHookConfig],
    site_hook_ids: &[String],
    context: &NotificationContext,
) -> NotificationRunResult {
    let selected = site_hook_ids.iter().collect::<BTreeSet<_>>();
    let mut attempts = Vec::new();
    let mut attempted = false;
    let mut required_failed = false;
    let mut optional_failed = false;
    let mut errors = Vec::new();

    for hook in hooks {
        if !selected.contains(&hook.id) {
            continue;
        }
        let trigger_reasons = trigger_reasons(hook, context);
        if trigger_reasons.is_empty() {
            continue;
        }
        attempted = true;
        let attempt = attempt_one(hook, &trigger_reasons, context).await;
        if !attempt.success {
            if hook.required {
                required_failed = true;
            } else {
                optional_failed = true;
            }
            if let Some(error) = &attempt.error {
                errors.push(format!("{}: {}", hook.id, error));
            }
        }
        attempts.push(attempt);
    }

    let delivery_result = if !attempted {
        DeliveryResult::NotConfigured
    } else if required_failed {
        DeliveryResult::FailedRequired
    } else if optional_failed {
        DeliveryResult::FailedOptional
    } else {
        DeliveryResult::Succeeded
    };
    let error_summary = (!errors.is_empty()).then(|| bounded_line(&errors.join("; "), 500));
    NotificationRunResult {
        attempts,
        delivery_result,
        error_summary,
    }
}

pub fn trigger_reasons(
    hook: &NotificationHookConfig,
    context: &NotificationContext,
) -> Vec<String> {
    let mut reasons = Vec::new();
    for trigger in &hook.triggers {
        match trigger {
            NotificationTrigger::GateFailed if context.audit_result == AuditResult::GateFailed => {
                reasons.push(trigger.as_str().to_owned());
            }
            NotificationTrigger::OperationalError
                if context.audit_result == AuditResult::OperationalError =>
            {
                reasons.push(trigger.as_str().to_owned());
            }
            NotificationTrigger::MinEffectiveSeverity => {
                if let Some(min) = hook.min_effective_severity {
                    if context
                        .findings
                        .iter()
                        .filter_map(|finding| finding.effective_severity)
                        .any(|severity| severity >= min)
                    {
                        reasons.push(format!("min_effective_severity:{min}"));
                    }
                }
            }
            _ => {}
        }
    }
    reasons
}

async fn attempt_one(
    hook: &NotificationHookConfig,
    trigger_reasons: &[String],
    context: &NotificationContext,
) -> NotificationAttemptRecord {
    let started_at = OffsetDateTime::now_utc();
    let include_local_paths = matches!(hook.kind, NotificationHookKind::Command { .. });
    let result = match payload_bytes(context, trigger_reasons, include_local_paths) {
        Ok(payload) => match &hook.kind {
            NotificationHookKind::Command { argv } => {
                run_command_hook(argv, hook.timeout_seconds, &payload).await
            }
            NotificationHookKind::Webhook { url, secret_env } => {
                run_webhook_hook(url, secret_env.as_deref(), hook.timeout_seconds, &payload).await
            }
        },
        Err(error) => Err(error.to_string()),
    };
    let finished_at = OffsetDateTime::now_utc();
    let (success, status, error) = match result {
        Ok(status) => (true, Some(status), None),
        Err(error) => (false, None, Some(bounded_line(&error, 500))),
    };
    NotificationAttemptRecord {
        run_id: context.run_id.clone(),
        hook_id: hook.id.clone(),
        hook_kind: hook_kind_name(&hook.kind).to_owned(),
        started_at,
        finished_at,
        trigger_reasons: trigger_reasons.to_vec(),
        required: hook.required,
        success,
        status,
        error,
    }
}

fn payload_bytes(
    context: &NotificationContext,
    trigger_reasons: &[String],
    include_local_paths: bool,
) -> Result<Vec<u8>, NotificationError> {
    let per_artifact_gate = context
        .aggregate_gate
        .as_ref()
        .map(|gate| gate.per_artifact.iter().collect::<Vec<_>>())
        .unwrap_or_default();
    let top_findings = context
        .findings
        .iter()
        .filter(|finding| finding.effective_severity.is_some())
        .take(MAX_TOP_FINDINGS)
        .map(NotificationFindingProjection::from)
        .collect::<Vec<_>>();
    let mut text = format!(
        "auditroo: site {} {}",
        context.site_id,
        match context.audit_result {
            AuditResult::Passed => "passed",
            AuditResult::GateFailed => "failed",
            AuditResult::OperationalError => "could not be checked",
        }
    );
    for line in &context.summary {
        text.push('\n');
        text.push_str(line);
    }
    let payload = NotificationPayload {
        text,
        artifact_kind: "notification_payload",
        schema_version: 2,
        auditroo_version: env!("CARGO_PKG_VERSION"),
        site_id: &context.site_id,
        run_id: &context.run_id,
        audit_result: context.audit_result,
        trigger_reasons,
        aggregate_gate: context.aggregate_gate.as_ref(),
        per_artifact_gate,
        severity_counts: &context.severity_counts,
        source_artifacts: &context.source_artifacts,
        local_artifact_paths: include_local_paths.then_some(&context.local_artifact_paths),
        delivery_result_before: context.delivery_result_before,
        top_findings,
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|error| NotificationError::Payload(error.to_string()))?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(NotificationError::Payload(
            "it is larger than 64 KiB".to_owned(),
        ));
    }
    Ok(bytes)
}

async fn run_command_hook(
    argv: &[String],
    timeout_seconds: u64,
    payload: &[u8],
) -> Result<String, String> {
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start the command: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(payload)
            .await
            .map_err(|error| format!("could not send the summary to the command: {error}"))?;
    }
    let status =
        match tokio::time::timeout(Duration::from_secs(timeout_seconds), child.wait()).await {
            Ok(result) => result.map_err(|error| format!("the command did not finish: {error}"))?,
            Err(_) => {
                let _ = child.kill().await;
                return Err(format!(
                    "the command did not finish within {timeout_seconds} seconds"
                ));
            }
        };
    if status.success() {
        Ok(format!("exit_status:{}", status.code().unwrap_or_default()))
    } else {
        Err(format!(
            "the command exited with status {}",
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_owned())
        ))
    }
}

async fn run_webhook_hook(
    url: &str,
    secret_env: Option<&str>,
    timeout_seconds: u64,
    payload: &[u8],
) -> Result<String, String> {
    let secret = match secret_env {
        Some(name) => Some(std::env::var(name).map_err(|_| {
            format!("the environment variable {name} for the webhook token is not set or not valid Unicode")
        })?),
        None => None,
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(timeout_seconds))
        .build()
        .map_err(|error| format!("could not set up the webhook request: {error}"))?;
    let mut request = client
        .post(url)
        .header("content-type", "application/json")
        .body(payload.to_vec());
    if let Some(secret) = secret {
        request = request.bearer_auth(secret);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("the webhook request failed: {error}"))?;
    let status = response.status();
    if status.is_success() {
        Ok(format!("http_status:{}", status.as_u16()))
    } else {
        Err(format!(
            "the webhook answered with status {}",
            status.as_u16()
        ))
    }
}

fn hook_kind_name(kind: &NotificationHookKind) -> &'static str {
    match kind {
        NotificationHookKind::Command { .. } => "command",
        NotificationHookKind::Webhook { .. } => "webhook",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_hook(id: &str) -> NotificationHookConfig {
        NotificationHookConfig {
            id: id.to_owned(),
            kind: NotificationHookKind::Command {
                argv: vec!["/bin/true".to_owned()],
            },
            required: false,
            triggers: vec![NotificationTrigger::GateFailed],
            min_effective_severity: None,
            timeout_seconds: 1,
        }
    }

    fn oversized_context() -> NotificationContext {
        let long_message = "x".repeat(6_000);
        let findings = (0..20)
            .map(|index| FindingSummary {
                id: format!("crawl/title_changed/{index}"),
                provenance: "crawl".to_owned(),
                site_id: "site".to_owned(),
                run_id: "run".to_owned(),
                rule: "title_changed".to_owned(),
                url: Some(format!("https://example.test/{index}")),
                group_key: None,
                severity: Some(crate::findings::Severity::High),
                effective_severity: Some(crate::findings::Severity::High),
                ignored: false,
                gate_relevant: true,
                message: long_message.clone(),
                impact_context: Vec::new(),
            })
            .collect();
        NotificationContext {
            site_id: "site".to_owned(),
            run_id: "run".to_owned(),
            audit_result: AuditResult::GateFailed,
            aggregate_gate: None,
            severity_counts: Vec::new(),
            source_artifacts: Vec::new(),
            local_artifact_paths: BTreeMap::new(),
            findings,
            delivery_result_before: DeliveryResult::Pending,
            summary: Vec::new(),
        }
    }

    #[tokio::test]
    async fn oversized_payload_is_recorded_and_later_hooks_continue() {
        let hooks = vec![command_hook("first"), command_hook("second")];
        let site_hook_ids = vec!["first".to_owned(), "second".to_owned()];
        let result = attempt_notifications(&hooks, &site_hook_ids, &oversized_context()).await;

        assert_eq!(result.delivery_result, DeliveryResult::FailedOptional);
        assert_eq!(result.attempts.len(), 2);
        assert!(result.attempts.iter().all(|attempt| !attempt.success));
        assert!(result.attempts.iter().all(|attempt| attempt
            .error
            .as_deref()
            .is_some_and(|error| error.contains("could not build the notification"))));
        assert!(result
            .error_summary
            .as_deref()
            .is_some_and(|error| error.contains("could not build the notification")));
    }
}
