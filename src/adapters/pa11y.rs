use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    issue_evidence, ParsedMetric, ParsedQualitySource, QualityFailure, TargetVerification,
};

pub(crate) fn parse_pa11y_report(value: &Value) -> Result<ParsedQualitySource, QualityFailure> {
    let issues = value.as_array().ok_or_else(|| {
        QualityFailure::new(
            "invalid_schema",
            "the Pa11y output is not a JSON array. Run Pa11y with --reporter json",
        )
    })?;

    let mut error_count = 0f64;
    let mut warning_count = 0f64;
    let mut notice_count = 0f64;
    let mut error_evidence = Vec::new();
    let mut warning_evidence = Vec::new();
    let mut notice_evidence = Vec::new();

    for (index, issue) in issues.iter().enumerate() {
        let issue_type = issue.get("type").and_then(Value::as_str).unwrap_or("");
        let code = issue.get("code").and_then(Value::as_str);
        let selector = issue.get("selector").and_then(Value::as_str);
        let message = issue
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| issue.get("context").and_then(Value::as_str));
        match issue_type {
            "error" => {
                error_count += 1.0;
                error_evidence.push(issue_evidence(
                    "accessibility.error_count",
                    code.or(message),
                    selector,
                    message,
                    Some(format!("issue[{}]", index + 1)),
                ));
            }
            "warning" => {
                warning_count += 1.0;
                warning_evidence.push(issue_evidence(
                    "accessibility.warning_count",
                    code.or(message),
                    selector,
                    message,
                    Some(format!("issue[{}]", index + 1)),
                ));
            }
            "notice" => {
                notice_count += 1.0;
                notice_evidence.push(issue_evidence(
                    "accessibility.notice_count",
                    code.or(message),
                    selector,
                    message,
                    Some(format!("issue[{}]", index + 1)),
                ));
            }
            _ => {}
        }
    }

    let mut metrics = BTreeMap::new();
    metrics.insert(
        "accessibility.error_count".to_owned(),
        ParsedMetric {
            value: error_count,
            unit: "count",
            evidence: error_evidence,
        },
    );
    metrics.insert(
        "accessibility.warning_count".to_owned(),
        ParsedMetric {
            value: warning_count,
            unit: "count",
            evidence: warning_evidence,
        },
    );
    metrics.insert(
        "accessibility.notice_count".to_owned(),
        ParsedMetric {
            value: notice_count,
            unit: "count",
            evidence: notice_evidence,
        },
    );

    Ok(ParsedQualitySource {
        metrics,
        warnings: Vec::new(),
        target_verification: TargetVerification::Unavailable,
        lighthouse_version: None,
        final_displayed_url: None,
    })
}
