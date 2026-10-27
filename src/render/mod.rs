//! Turn a report into text for people: terminal, Markdown, GitHub
//! annotations, JUnit XML and a standalone HTML page.
//!
//! All formats share the same grouping. Findings with the same rule,
//! severity and shared detail form one group; a group of many findings is
//! shown as one line ("23 pages now have noindex") with a list of URLs.

pub mod github;
pub mod html;
pub mod human;
pub mod junit;
pub mod markdown;

use crate::findings::{format_count, Finding, Rule, Severity};
use crate::report::{Report, ReportCommand};
use crate::site::Site;

/// Groups with at least this many findings are shown as one summary line.
pub const GROUP_THRESHOLD: usize = 4;

/// Findings that share rule, severity and detail.
#[derive(Debug)]
pub struct Group<'a> {
    pub rule: Rule,
    pub severity: Severity,
    pub detail: Option<&'a str>,
    pub fails: bool,
    pub ignored: bool,
    pub findings: Vec<&'a Finding>,
}

impl Group<'_> {
    /// Number of pages or URLs the group stands for.
    pub fn weight(&self) -> usize {
        self.findings.iter().map(|finding| finding.weight()).sum()
    }

    /// "3 pages now have noindex".
    pub fn summary(&self) -> String {
        let summary = self.rule.summary(self.weight(), self.detail);
        capitalize(&summary)
    }

    /// How an ignore rule names this group, for example "rule: noindex_added".
    /// Custom checks and quality budgets are ignored by their own id.
    pub fn ignore_key(&self) -> String {
        match self.findings.first() {
            Some(finding) => ignore_key(finding),
            None => format!("rule: {}", self.rule.id()),
        }
    }

    /// Whether the group is shown as one line instead of one line per finding.
    pub fn collapsed(&self) -> bool {
        self.findings.len() >= GROUP_THRESHOLD
    }
}

/// How an ignore rule names one finding, for example "rule: broken_link" or
/// "assertion: product-offer-shape".
pub fn ignore_key(finding: &Finding) -> String {
    match finding.check_id.as_deref() {
        Some(id) => format!("assertion: {id}"),
        None => format!("rule: {}", finding.rule.id()),
    }
}

/// Group a report's findings. The report holds findings in sorted order, so groups
/// come out failing first, then by severity.
pub fn groups(report: &Report) -> Vec<Group<'_>> {
    let mut output: Vec<Group<'_>> = Vec::new();
    for finding in &report.findings {
        let fails = report.fails_gate(finding);
        let detail = finding.group.as_deref();
        match output.last_mut() {
            Some(group)
                if group.rule == finding.rule
                    && group.severity == finding.severity
                    && group.detail == detail
                    && group.ignored == finding.ignored =>
            {
                group.findings.push(finding);
            }
            _ => output.push(Group {
                rule: finding.rule,
                severity: finding.severity,
                detail,
                fails,
                ignored: finding.ignored,
                findings: vec![finding],
            }),
        }
    }
    output
}

/// Search and analytics numbers attached to a finding, for example
/// "traffic: 120 clicks, 5,000 impressions". Only additive metrics are summed.
pub fn impact_line(report: &Report, finding: &Finding) -> Option<String> {
    let impact = report.impact.as_ref()?;
    if finding.impact.is_empty() {
        return None;
    }
    // Numbers from two exports of the same URL do not add up, so show one
    // source: the primary one, or else the first.
    let source = impact
        .primary
        .as_ref()
        .map(|primary| primary.source.as_str())
        .unwrap_or(finding.impact[0].source_id.as_str());
    let mut totals: std::collections::BTreeMap<&str, f64> = std::collections::BTreeMap::new();
    for annotation in finding
        .impact
        .iter()
        .filter(|annotation| annotation.source_id == source)
    {
        let Some(context) = impact
            .url_contexts
            .iter()
            .find(|context| context.context_id == annotation.context_id)
        else {
            continue;
        };
        for (metric, value) in &context.metrics {
            if crate::external::is_additive_impact_metric(metric) {
                *totals.entry(metric.as_str()).or_default() += value;
            }
        }
    }
    let order = [
        "clicks",
        "impressions",
        "sessions",
        "pageviews",
        "conversions",
        "revenue",
    ];
    let parts: Vec<String> = order
        .iter()
        .filter_map(|metric| {
            totals.get(metric).map(|value| {
                let number = if value.fract() == 0.0 {
                    format_count(*value as usize)
                } else {
                    format!("{value:.2}")
                };
                format!("{number} {metric}")
            })
        })
        .collect();
    (!parts.is_empty()).then(|| format!("traffic: {}", parts.join(", ")))
}

/// The site, to show URLs as short paths.
pub fn site_for(report: &Report) -> Site {
    let mut site = Site::new(&report.current.start_url, std::iter::empty());
    if let Some(baseline) = &report.baseline {
        site = site.with_alias(&baseline.start_url);
    }
    site
}

/// One line of the summary at the top: all findings of one rule and
/// severity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryLine {
    pub severity: Severity,
    pub text: String,
    pub fails: bool,
}

/// Summary lines for findings that are not ignored, most severe first.
pub fn summary_lines(report: &Report) -> Vec<SummaryLine> {
    /// Groups of one rule and severity; the detail stays only while all
    /// groups share it.
    struct Merged<'a> {
        rule: Rule,
        severity: Severity,
        fails: bool,
        weight: usize,
        detail: Option<&'a str>,
        same_detail: bool,
    }
    let mut merged: Vec<Merged<'_>> = Vec::new();
    for group in groups(report).into_iter().filter(|group| !group.ignored) {
        match merged.last_mut() {
            Some(line)
                if line.rule == group.rule
                    && line.severity == group.severity
                    && line.fails == group.fails =>
            {
                line.weight += group.weight();
                line.same_detail &= line.detail == group.detail;
            }
            _ => merged.push(Merged {
                rule: group.rule,
                severity: group.severity,
                fails: group.fails,
                weight: group.weight(),
                detail: group.detail,
                same_detail: true,
            }),
        }
    }
    merged
        .into_iter()
        .map(|line| SummaryLine {
            severity: line.severity,
            text: capitalize(
                &line
                    .rule
                    .summary(line.weight, line.detail.filter(|_| line.same_detail)),
            ),
            fails: line.fails,
        })
        .collect()
}

/// The headline: "3 problems would hurt search" or "No SEO regressions found".
pub fn headline(report: &Report) -> String {
    if report.passed {
        return match report.command {
            ReportCommand::Check => "No problems at or above the fail-on level".to_owned(),
            _ => "No SEO regressions found".to_owned(),
        };
    }
    let problems = summary_lines(report)
        .iter()
        .filter(|line| line.fails)
        .count();
    let noun = if problems == 1 { "problem" } else { "problems" };
    match report.command {
        ReportCommand::Check => format!("{} {noun} found", format_count(problems)),
        _ => format!("{} {noun} would hurt search", format_count(problems)),
    }
}

/// A comma list of URLs, cut after `limit` with "and N more".
pub fn url_list(urls: &[String], total: usize, limit: usize, site: &Site) -> String {
    let shown: Vec<String> = urls
        .iter()
        .take(limit)
        .map(|url| site.display(url))
        .collect();
    let rest = total.max(urls.len()).saturating_sub(shown.len());
    let mut text = shown.join(", ");
    if rest > 0 {
        text.push_str(&format!(" and {} more", format_count(rest)));
    }
    text
}

/// Upper-case the first letter, except for file names such as robots.txt.
pub fn capitalize(text: &str) -> String {
    if text
        .split_whitespace()
        .next()
        .is_some_and(|word| word.contains('.'))
    {
        return text.to_owned();
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// "26 Oct 2026, 16:00 UTC".
pub fn format_time(time: time::OffsetDateTime) -> String {
    let format = time::macros::format_description!(
        "[day padding:none] [month repr:short] [year], [hour]:[minute] UTC"
    );
    time.to_offset(time::UtcOffset::UTC)
        .format(&format)
        .unwrap_or_else(|_| time.to_string())
}

/// What was compared with what, in one sentence.
pub fn scope_sentence(report: &Report) -> String {
    let current = &report.current;
    match (&report.baseline, report.command) {
        (Some(baseline), ReportCommand::Compare) => format!(
            "Compared the preview {} ({}) with production {} ({}).",
            current.start_url,
            pages(current.pages),
            baseline.start_url,
            pages(baseline.pages)
        ),
        (Some(baseline), _) => format!(
            "Compared {} ({}) with the baseline of {} from {} ({}).",
            current.start_url,
            pages(current.pages),
            baseline.start_url,
            format_time(baseline.crawled_at),
            pages(baseline.pages)
        ),
        (None, _) => format!("Checked {} ({}).", current.start_url, pages(current.pages)),
    }
}

/// "1 page", "23 pages".
pub fn pages(count: usize) -> String {
    format!(
        "{} page{}",
        format_count(count),
        if count == 1 { "" } else { "s" }
    )
}
