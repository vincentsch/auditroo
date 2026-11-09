//! The report of a `check`, `diff` or `compare` run.
//!
//! A report holds the findings, the gate result and a short summary of each
//! crawl. It is what `--json` prints and what `auditroo report` renders again
//! as text, Markdown, HTML or JUnit.

use std::error::Error;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use url::Url;

use crate::adapters::QualitySourceSummary;
use crate::config::{IgnoreRule, ResolvedGateSettings};
use crate::crawl::url::path_and_query;
use crate::crawl::{normalize_url, ScopePattern};
use crate::external::ImpactReport;
use crate::findings::{Finding, Rule, Severity};
use crate::snapshot::{write_atomically, Snapshot};

pub const REPORT_ARTIFACT_KIND: &str = "report";
pub const REPORT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportCommand {
    Check,
    Diff,
    Compare,
}

impl ReportCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Diff => "diff",
            Self::Compare => "compare",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub artifact_kind: String,
    pub schema_version: u16,
    pub auditroo_version: String,
    pub command: ReportCommand,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// The crawl that was checked: the preview in `compare`.
    pub current: CrawlSummary,
    /// The crawl it was compared with: production in `compare`, the
    /// baseline file in `diff`. Missing for `check`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<CrawlSummary>,
    pub fail_on: Severity,
    pub passed: bool,
    pub counts: SeverityCounts,
    /// Plain sentences about what could not be compared, or about what
    /// auditroo did for the user, such as ignoring a preview's noindex
    /// header.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Indexable baseline pages that the current crawl did not reach, for
    /// example because of --max-pages or --include.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub not_compared: usize,
    /// In compare and diff: the host that both crawls name in most of their
    /// canonical tags or sitemap URLs, when it is neither of the two sites.
    /// Most rules then have little to check, until auditroo runs again with
    /// this URL as --production-url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_production_url: Option<String>,
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_rules: Vec<IgnoreRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<Vec<QualitySourceSummary>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<ImpactReport>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlSummary {
    pub start_url: String,
    #[serde(with = "time::serde::rfc3339")]
    pub crawled_at: OffsetDateTime,
    /// HTML pages that loaded.
    pub pages: usize,
    /// Every URL with a response, including redirects, errors and files.
    pub urls: usize,
    /// URLs that could not be fetched at all.
    pub failed: usize,
    /// The crawl stopped at --max-pages or --max-depth.
    pub truncated: bool,
    /// URLs that robots.txt blocks for auditroo, so the crawl did not fetch
    /// them. In compare and diff the current crawl still fetches pages that
    /// the earlier robots.txt allowed, unless a new rule names auditroo.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub robots_denied: Vec<String>,
}

impl CrawlSummary {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self {
            start_url: snapshot.start_url.clone(),
            crawled_at: snapshot.created_at,
            pages: snapshot
                .pages()
                .filter(|record| record.is_ok() && !record.navigation_changed())
                .count(),
            urls: snapshot.records.len(),
            failed: snapshot.crawl_failures.len(),
            truncated: snapshot.truncated(),
            robots_denied: snapshot.robots_denied().map(str::to_owned).collect(),
        }
    }
}

/// Findings per severity, without ignored ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityCounts {
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub info: usize,
    pub ignored: usize,
}

impl SeverityCounts {
    pub fn get(&self, severity: Severity) -> usize {
        match severity {
            Severity::Critical => self.critical,
            Severity::High => self.high,
            Severity::Medium => self.medium,
            Severity::Low => self.low,
            Severity::Info => self.info,
        }
    }

    fn add(&mut self, severity: Severity) {
        match severity {
            Severity::Critical => self.critical += 1,
            Severity::High => self.high += 1,
            Severity::Medium => self.medium += 1,
            Severity::Low => self.low += 1,
            Severity::Info => self.info += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    Io { path: String, detail: String },
    Invalid(String),
}

impl fmt::Display for ReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, detail } => write!(formatter, "{path}: {detail}"),
            Self::Invalid(detail) => formatter.write_str(detail),
        }
    }
}

impl Error for ReportError {}

/// Everything needed to build a report.
pub struct ReportInput<'a> {
    pub command: ReportCommand,
    pub current: &'a Snapshot,
    pub baseline: Option<&'a Snapshot>,
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
    pub gate: &'a ResolvedGateSettings,
}

impl Report {
    pub fn new(input: ReportInput<'_>) -> Self {
        let mut report = Report {
            artifact_kind: REPORT_ARTIFACT_KIND.to_owned(),
            schema_version: REPORT_SCHEMA_VERSION,
            auditroo_version: env!("CARGO_PKG_VERSION").to_owned(),
            command: input.command,
            created_at: OffsetDateTime::now_utc()
                .replace_nanosecond(0)
                .expect("zero nanoseconds is valid"),
            current: CrawlSummary::from_snapshot(input.current),
            baseline: input.baseline.map(CrawlSummary::from_snapshot),
            fail_on: input.gate.fail_on,
            passed: true,
            counts: SeverityCounts::default(),
            notes: input.notes,
            not_compared: 0,
            suggested_production_url: None,
            findings: input.findings,
            ignore_rules: input.gate.ignore.clone(),
            quality: None,
            impact: None,
        };
        report.finish();
        report
    }

    /// Apply ignore rules, sort, and decide the gate. Call again after
    /// adding findings.
    pub fn finish(&mut self) {
        for finding in &mut self.findings {
            finding.ignored = self
                .ignore_rules
                .iter()
                .any(|rule| ignore_matches(rule, finding));
        }
        let traffic = self.traffic_by_finding();
        let mut indexed: Vec<(f64, Finding)> = self
            .findings
            .drain(..)
            .enumerate()
            .map(|(index, finding)| (traffic.get(index).copied().unwrap_or(0.0), finding))
            .collect();
        indexed.sort_by(|(left_traffic, left), (right_traffic, right)| {
            left.ignored
                .cmp(&right.ignored)
                .then(right.severity.cmp(&left.severity))
                .then(left.rule.cmp(&right.rule))
                .then(right_traffic.total_cmp(left_traffic))
                .then(left.group.cmp(&right.group))
                .then(left.url.cmp(&right.url))
        });
        self.findings = indexed.into_iter().map(|(_, finding)| finding).collect();
        let mut counts = SeverityCounts::default();
        for finding in &self.findings {
            if finding.ignored {
                counts.ignored += 1;
            } else {
                counts.add(finding.severity);
            }
        }
        self.counts = counts;
        self.passed = !self.findings.iter().any(|finding| self.fails_gate(finding));
    }

    /// The primary impact metric of each finding, when `[impact]` asks to
    /// sort by it. Pages with more traffic come first within a rule.
    fn traffic_by_finding(&self) -> Vec<f64> {
        let Some(impact) = self.impact.as_ref().filter(|impact| {
            impact.enabled && impact.sort == crate::external::ImpactSort::SeverityThenImpact
        }) else {
            return Vec::new();
        };
        let Some(primary) = &impact.primary else {
            return Vec::new();
        };
        self.findings
            .iter()
            .map(|finding| {
                finding
                    .impact
                    .iter()
                    .filter_map(|annotation| {
                        impact
                            .url_contexts
                            .iter()
                            .find(|context| {
                                context.context_id == annotation.context_id
                                    && context.source_id == primary.source
                            })
                            .and_then(|context| context.metrics.get(&primary.metric))
                    })
                    .sum()
            })
            .collect()
    }

    /// Whether this finding makes the run fail.
    pub fn fails_gate(&self, finding: &Finding) -> bool {
        !finding.ignored && finding.severity >= self.fail_on
    }

    pub fn failing(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(move |finding| self.fails_gate(finding))
    }

    /// Whether some URLs were not checked, or most rules had nothing to
    /// check, so a pass does not mean the whole site is fine.
    pub fn incomplete(&self) -> bool {
        !self.current.robots_denied.is_empty()
            || self.current.truncated
            || self.not_compared > 0
            || self.suggested_production_url.is_some()
    }

    pub fn exit_code(&self) -> i32 {
        if self.passed {
            0
        } else {
            1
        }
    }
}

fn ignore_matches(rule: &IgnoreRule, finding: &Finding) -> bool {
    if let Some(id) = &rule.rule {
        if Rule::from_id(id) != Some(finding.rule) {
            return false;
        }
    }
    if let Some(assertion) = &rule.assertion {
        if finding.check_id.as_deref() != Some(assertion.as_str()) {
            return false;
        }
    }
    match (&rule.url, &finding.url) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(pattern), Some(url)) => url_matches(pattern, url),
    }
}

/// Match an ignore `url` pattern: a path pattern such as `/promo/*`, or a
/// full URL pattern. `*` matches any text.
pub fn url_matches(pattern: &str, url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    if pattern.starts_with('/') {
        ScopePattern::compile(pattern).matches(&path_and_query(&parsed))
    } else {
        ScopePattern::compile(pattern).matches(&normalize_url(&parsed))
    }
}

pub fn render_json<T: Serialize>(value: &T) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(value).expect("report values serialize")
    )
}

pub fn write_report(report: &Report, path: &Path) -> Result<(), ReportError> {
    write_atomically(path, render_json(report).as_bytes()).map_err(|error| ReportError::Io {
        path: path.display().to_string(),
        detail: error.to_string(),
    })
}

pub fn report_from_str(raw: &str) -> Result<Report, ReportError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| ReportError::Invalid(format!("not valid JSON: {error}")))?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if version != u64::from(REPORT_SCHEMA_VERSION) {
        return Err(ReportError::Invalid(format!(
            "this report uses format version {version}, but this auditroo reads version {REPORT_SCHEMA_VERSION}. Run the command again to get a new report"
        )));
    }
    serde_json::from_value(value)
        .map_err(|error| ReportError::Invalid(format!("not a valid report: {error}")))
}

/// The `artifact_kind` of a JSON file, to pick a renderer.
pub fn artifact_kind(raw: &str) -> Result<String, ReportError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| ReportError::Invalid(format!("not valid JSON: {error}")))?;
    value
        .get("artifact_kind")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            ReportError::Invalid(
                "this JSON file has no artifact_kind, so it was not written by auditroo".to_owned(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IgnoreRuleSource;

    #[test]
    fn ignore_rules_match_rule_url_and_assertion() {
        let finding = Finding::new(
            Rule::TitleChanged,
            Some("https://example.test/promo/a?x=1"),
            "has a new title",
        );
        let rule = |url: Option<&str>, id: Option<&str>| IgnoreRule {
            url: url.map(str::to_owned),
            rule: id.map(str::to_owned),
            assertion: None,
            source: IgnoreRuleSource::Cli,
        };
        assert!(ignore_matches(&rule(Some("/promo/*"), None), &finding));
        assert!(ignore_matches(&rule(None, Some("title_changed")), &finding));
        assert!(!ignore_matches(
            &rule(None, Some("noindex_added")),
            &finding
        ));
        assert!(!ignore_matches(
            &rule(Some("/other/*"), Some("title_changed")),
            &finding
        ));
        assert!(ignore_matches(
            &rule(Some("https://example.test/promo/*"), None),
            &finding
        ));
    }
}
