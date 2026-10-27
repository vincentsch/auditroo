use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    issue_evidence, ParsedMetric, ParsedQualitySource, QualityFailure, TargetVerification,
};

pub(crate) fn parse_nu_html_report(value: &Value) -> Result<ParsedQualitySource, QualityFailure> {
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            QualityFailure::new(
                "invalid_schema",
                "the Nu HTML Checker output has no messages array",
            )
        })?;

    let mut error_count = 0f64;
    let mut warning_count = 0f64;
    let mut error_evidence = Vec::new();
    let mut warning_evidence = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        let message_type = message.get("type").and_then(Value::as_str).unwrap_or("");
        let subtype = message.get("subType").and_then(Value::as_str);
        if message_type == "non-document-error" {
            return Err(QualityFailure::new(
                "tool_runtime_error",
                "the Nu HTML Checker reported a non-document-error",
            ));
        }
        let text = message.get("message").and_then(Value::as_str);
        let source_location =
            nu_source_location(message).unwrap_or_else(|| format!("messages[{}]", index + 1));
        match (message_type, subtype) {
            ("error", _) => {
                error_count += 1.0;
                error_evidence.push(issue_evidence(
                    "html.error_count",
                    text,
                    None,
                    text,
                    Some(source_location),
                ));
            }
            ("info", Some("warning")) => {
                warning_count += 1.0;
                warning_evidence.push(issue_evidence(
                    "html.warning_count",
                    text,
                    None,
                    text,
                    Some(source_location),
                ));
            }
            _ => {}
        }
    }

    let mut metrics = BTreeMap::new();
    metrics.insert(
        "html.error_count".to_owned(),
        ParsedMetric {
            value: error_count,
            unit: "count",
            evidence: error_evidence,
        },
    );
    metrics.insert(
        "html.warning_count".to_owned(),
        ParsedMetric {
            value: warning_count,
            unit: "count",
            evidence: warning_evidence,
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

fn nu_source_location(message: &Value) -> Option<String> {
    let first_line = message.get("firstLine").and_then(Value::as_u64);
    let first_column = message.get("firstColumn").and_then(Value::as_u64);
    let last_line = message.get("lastLine").and_then(Value::as_u64);
    let last_column = message.get("lastColumn").and_then(Value::as_u64);
    match (first_line, first_column, last_line, last_column) {
        (Some(line), Some(column), _, _) => Some(format!("{line}:{column}")),
        (Some(line), None, _, _) => Some(line.to_string()),
        (None, None, Some(line), Some(column)) => Some(format!("{line}:{column}")),
        _ => None,
    }
}
