use serde_json::Value;

use super::{lighthouse, ParsedQualitySource, QualityFailure};

pub(crate) fn parse_pagespeed_response(
    value: &Value,
    template_url: &str,
) -> Result<ParsedQualitySource, QualityFailure> {
    let lighthouse_result = value.get("lighthouseResult").ok_or_else(|| {
        QualityFailure::new(
            "invalid_schema",
            "the PageSpeed response has no lighthouseResult",
        )
    })?;
    lighthouse::parse_lighthouse_result(lighthouse_result, template_url)
}
