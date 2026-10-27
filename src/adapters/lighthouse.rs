use std::collections::BTreeMap;

use serde_json::Value;

use super::{
    finite_metric, metric_evidence, number_at, push_metric, string_at, validate_numeric_unit,
    verify_reported_url, ParsedQualitySource, QualityFailure,
};

pub(crate) fn parse_lighthouse_result(
    value: &Value,
    template_url: &str,
) -> Result<ParsedQualitySource, QualityFailure> {
    let lighthouse_version = string_at(value, &["lighthouseVersion"]).map(str::to_owned);
    let final_displayed_url = string_at(value, &["finalDisplayedUrl"]).map(str::to_owned);
    reject_runtime_error(value).map_err(|failure| {
        failure.with_tool_metadata(lighthouse_version.clone(), final_displayed_url.clone())
    })?;
    validate_numeric_units(value).map_err(|failure| {
        failure.with_tool_metadata(lighthouse_version.clone(), final_displayed_url.clone())
    })?;
    let target_verification = verify_reported_url(
        string_at(value, &["requestedUrl"]).or_else(|| string_at(value, &["finalDisplayedUrl"])),
        template_url,
    )
    .map_err(|failure| {
        failure.with_tool_metadata(lighthouse_version.clone(), final_displayed_url.clone())
    })?;

    let mut metrics = BTreeMap::new();
    push_metric(
        &mut metrics,
        category_score_metric(
            value,
            "performance",
            "performance.score",
            "Performance score",
        )?,
    );
    push_metric(
        &mut metrics,
        category_score_metric(
            value,
            "accessibility",
            "accessibility.score",
            "Accessibility score",
        )?,
    );
    push_metric(
        &mut metrics,
        audit_metric(
            value,
            "largest-contentful-paint",
            "performance.largest_contentful_paint_ms",
            "milliseconds",
        ),
    );
    push_metric(
        &mut metrics,
        audit_metric(
            value,
            "cumulative-layout-shift",
            "performance.cumulative_layout_shift",
            "unitless",
        ),
    );
    push_metric(
        &mut metrics,
        audit_metric(
            value,
            "total-blocking-time",
            "performance.total_blocking_time_ms",
            "milliseconds",
        ),
    );
    push_metric(
        &mut metrics,
        audit_metric(
            value,
            "speed-index",
            "performance.speed_index_ms",
            "milliseconds",
        ),
    );
    push_metric(
        &mut metrics,
        audit_metric(
            value,
            "total-byte-weight",
            "performance.total_byte_weight_bytes",
            "bytes",
        ),
    );

    Ok(ParsedQualitySource {
        metrics,
        warnings: Vec::new(),
        target_verification,
        lighthouse_version,
        final_displayed_url,
    })
}

fn reject_runtime_error(value: &Value) -> Result<(), QualityFailure> {
    match value.get("runtimeError") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(object)) if object.is_empty() => Ok(()),
        Some(_) => Err(QualityFailure::new(
            "tool_runtime_error",
            "Lighthouse reported a runtimeError",
        )),
    }
}

fn validate_numeric_units(value: &Value) -> Result<(), QualityFailure> {
    validate_numeric_unit(
        value
            .get("audits")
            .and_then(|audits| audits.get("largest-contentful-paint")),
        "performance.largest_contentful_paint_ms",
        &["millisecond", "milliseconds"],
    )?;
    validate_numeric_unit(
        value
            .get("audits")
            .and_then(|audits| audits.get("cumulative-layout-shift")),
        "performance.cumulative_layout_shift",
        &["unitless"],
    )?;
    validate_numeric_unit(
        value
            .get("audits")
            .and_then(|audits| audits.get("total-blocking-time")),
        "performance.total_blocking_time_ms",
        &["millisecond", "milliseconds"],
    )?;
    validate_numeric_unit(
        value
            .get("audits")
            .and_then(|audits| audits.get("speed-index")),
        "performance.speed_index_ms",
        &["millisecond", "milliseconds"],
    )?;
    validate_numeric_unit(
        value
            .get("audits")
            .and_then(|audits| audits.get("total-byte-weight")),
        "performance.total_byte_weight_bytes",
        &["byte", "bytes"],
    )?;
    Ok(())
}

fn category_evidence(
    metric_id: &str,
    value: &Value,
    category_id: &str,
    fallback_title: &'static str,
) -> super::AdapterEvidence {
    let category = value
        .get("categories")
        .and_then(|categories| categories.get(category_id));
    metric_evidence(
        metric_id,
        category
            .and_then(|category| category.get("title"))
            .and_then(Value::as_str)
            .or(Some(fallback_title)),
        category
            .and_then(|category| category.get("description"))
            .and_then(Value::as_str),
    )
}

fn category_score_metric(
    value: &Value,
    category_id: &str,
    metric_id: &str,
    fallback_title: &'static str,
) -> Result<Option<(String, super::ParsedMetric)>, QualityFailure> {
    let Some(score) = number_at(value, &["categories", category_id, "score"]) else {
        return Ok(None);
    };
    if !(0.0..=1.0).contains(&score) {
        return Err(QualityFailure::new(
            "invalid_metric",
            format!("{metric_id} must be between 0 and 1"),
        ));
    }
    Ok(finite_metric(
        Some(score),
        metric_id,
        "ratio",
        vec![category_evidence(
            metric_id,
            value,
            category_id,
            fallback_title,
        )],
    ))
}

fn audit_metric(
    value: &Value,
    audit_id: &str,
    metric_id: &str,
    unit: &'static str,
) -> Option<(String, super::ParsedMetric)> {
    let audit = value.get("audits").and_then(|audits| audits.get(audit_id));
    finite_metric(
        audit
            .and_then(|audit| audit.get("numericValue"))
            .and_then(Value::as_f64),
        metric_id,
        unit,
        vec![metric_evidence(
            metric_id,
            audit
                .and_then(|audit| audit.get("title"))
                .and_then(Value::as_str),
            audit
                .and_then(|audit| audit.get("description"))
                .and_then(Value::as_str),
        )],
    )
}
