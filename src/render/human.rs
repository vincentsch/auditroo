//! Terminal output: a summary first, then the findings, then notes.

use std::time::Duration;

use super::{
    capitalize, groups, headline, incomplete_line, scope_sentence, site_for, summary_lines,
    url_list, Group,
};
use crate::findings::{Finding, Severity};
use crate::report::{Report, ReportCommand};
use crate::site::Site;

#[derive(Debug, Clone, Copy, Default)]
pub struct HumanOptions {
    /// Use ANSI colors.
    pub color: bool,
    /// How long the crawl took, for the last line.
    pub elapsed: Option<Duration>,
}

struct Style {
    color: bool,
}

impl Style {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn severity(&self, severity: Severity, text: &str) -> String {
        let code = match severity {
            Severity::Critical => "1;31",
            Severity::High => "31",
            Severity::Medium => "33",
            Severity::Low | Severity::Info => "2",
        };
        self.paint(code, text)
    }
}

pub fn render(report: &Report, options: HumanOptions) -> String {
    let style = Style {
        color: options.color,
    };
    let site = site_for(report);
    let groups = groups(report);
    let mut out = String::new();

    let verdict = if report.passed {
        style.paint("1;32", "PASS")
    } else {
        style.paint("1;31", "FAIL")
    };
    out.push_str(&format!("{verdict}  {}", headline(report)));
    if !report.passed {
        out.push_str(&format!(" (fails on {} and above)", report.fail_on));
    }
    out.push('\n');
    for line in summary_lines(report).iter().filter(|line| line.fails) {
        out.push_str(&format!("  - {}\n", line.text));
    }
    if let Some(line) = incomplete_line(report) {
        out.push('\n');
        out.push_str(&style.paint("33", &line));
        out.push('\n');
    }

    for severity in Severity::ALL_DESCENDING {
        let failing: Vec<&Group<'_>> = groups
            .iter()
            .filter(|group| group.fails && group.severity == severity)
            .collect();
        if failing.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&style.severity(severity, severity.title()));
        out.push('\n');
        for group in failing {
            render_group(&mut out, group, &site, report, &style);
        }
    }

    let passing: Vec<&Group<'_>> = groups
        .iter()
        .filter(|group| !group.fails && !group.ignored)
        .collect();
    if !passing.is_empty() {
        out.push('\n');
        let title = if report.passed && report.command == ReportCommand::Check {
            "Findings".to_owned()
        } else if report.passed {
            "Changes and findings".to_owned()
        } else {
            format!("Below the fail-on level ({})", report.fail_on)
        };
        out.push_str(&style.paint("1", &title));
        out.push('\n');
        for group in passing {
            let label = format!("{:<6}", group.severity.as_str());
            if group.severity == Severity::Info {
                out.push_str(&format!(
                    "  {}  {}\n",
                    style.severity(group.severity, &label),
                    group.summary()
                ));
            } else if group.collapsed() || group.findings.len() > 1 {
                out.push_str(&format!(
                    "  {}  {}\n",
                    style.severity(group.severity, &label),
                    group.summary()
                ));
                let urls: Vec<String> = group
                    .findings
                    .iter()
                    .filter_map(|finding| finding.url.clone())
                    .collect();
                if !urls.is_empty() {
                    out.push_str(&format!(
                        "          {}\n",
                        url_list(&urls, urls.len(), 6, &site)
                    ));
                }
                for line in example_lines(group, &site) {
                    out.push_str(&format!("          {line}\n"));
                }
            } else {
                let finding = group.findings[0];
                out.push_str(&format!(
                    "  {}  {}\n",
                    style.severity(group.severity, &label),
                    sentence(finding, &site)
                ));
                for line in detail_lines(finding, &site)
                    .into_iter()
                    .chain(super::impact_line(report, finding))
                {
                    out.push_str(&format!("          {line}\n"));
                }
            }
            // The rule id is what an ignore rule needs.
            out.push_str(&format!(
                "          {}\n",
                style.paint("2", &group.ignore_key())
            ));
        }
    }

    let ignored = report.counts.ignored;
    if ignored > 0 {
        out.push_str(&format!(
            "\n{ignored} finding{} matched an ignore rule and {} not counted.\n",
            if ignored == 1 { "" } else { "s" },
            if ignored == 1 { "is" } else { "are" }
        ));
    }

    if !report.notes.is_empty() {
        out.push('\n');
        out.push_str(&style.paint("1", "Notes"));
        out.push('\n');
        for note in &report.notes {
            out.push_str(&format!("  - {note}\n"));
        }
    }

    out.push('\n');
    out.push_str(&scope_sentence(report));
    if let Some(elapsed) = options.elapsed {
        out.push_str(&format!(" Took {:.1} s.", elapsed.as_secs_f64()));
    }
    out.push('\n');
    if report.findings.is_empty() && report.command != ReportCommand::Check {
        if report.incomplete() {
            out.push_str(
                "Nothing changed on the pages both crawls reached. Some pages were not compared. The notes say why.\n",
            );
        } else {
            out.push_str("Nothing changed that auditroo checks.\n");
        }
    }
    out
}

/// A failing group. Each finding, or the collapsed group as a whole, ends with
/// what to do and the rule id, so the CI log says how to fix a problem or how
/// to accept it.
fn render_group(out: &mut String, group: &Group<'_>, site: &Site, report: &Report, style: &Style) {
    let advice = |key: String| {
        format!(
            "      what to do: {}\n      {}\n",
            group.rule.short_advice(),
            style.paint("2", &key)
        )
    };
    if group.collapsed() {
        out.push_str(&format!("  {}\n", group.summary()));
        let urls: Vec<String> = group
            .findings
            .iter()
            .filter_map(|finding| finding.url.clone())
            .collect();
        if !urls.is_empty() {
            out.push_str(&format!("      {}\n", url_list(&urls, urls.len(), 8, site)));
        }
        for line in example_lines(group, site) {
            out.push_str(&format!("      {line}\n"));
        }
        out.push_str(&advice(group.ignore_key()));
        return;
    }
    for finding in &group.findings {
        out.push_str(&format!("  {}\n", sentence(finding, site)));
        for line in detail_lines(finding, site)
            .into_iter()
            .chain(super::impact_line(report, finding))
        {
            out.push_str(&format!("      {line}\n"));
        }
        out.push_str(&advice(super::ignore_key(finding)));
    }
}

/// One finding of a collapsed group with its before and after values, so the
/// reader sees what changed without opening the full report.
pub fn example_lines(group: &Group<'_>, site: &Site) -> Vec<String> {
    let Some(finding) = group.findings.first() else {
        return Vec::new();
    };
    if finding.url.is_none() {
        return Vec::new();
    }
    let mut lines = vec![format!("for example, {}", sentence(finding, site))];
    match (&finding.before, &finding.after) {
        (Some(before), Some(after)) => {
            lines.push(format!("  before: {before}"));
            lines.push(format!("  after:  {after}"));
        }
        (Some(before), None) => lines.push(format!("  before: {before}")),
        (None, Some(after)) => lines.push(format!("  after:  {after}")),
        (None, None) => {}
    }
    lines.extend(finding.details.iter().map(|detail| format!("  {detail}")));
    lines
}

/// "/docs/ now has noindex (robots meta tag "noindex")".
pub fn sentence(finding: &Finding, site: &Site) -> String {
    match &finding.url {
        Some(url) => format!("{} {}", site.display(url), finding.message),
        None => capitalize(&finding.message),
    }
}

/// Before and after values, related URLs and follow-on changes.
pub fn detail_lines(finding: &Finding, site: &Site) -> Vec<String> {
    let mut lines = Vec::new();
    match (&finding.before, &finding.after) {
        (Some(before), Some(after)) => {
            lines.push(format!("before: {before}"));
            lines.push(format!("after:  {after}"));
        }
        (Some(before), None) => lines.push(format!("before: {before}")),
        (None, Some(after)) => lines.push(format!("after:  {after}")),
        (None, None) => {}
    }
    lines.extend(finding.details.iter().cloned());
    if !finding.related.is_empty() {
        lines.push(format!(
            "{}: {}",
            finding.rule.related_label(),
            url_list(&finding.related, finding.related_total, 5, site)
        ));
    }
    if !finding.follow_on.is_empty() {
        let count = finding.follow_on.len();
        lines.push(format!(
            "{count} other finding{} on this page come{} from this one and {} not listed",
            if count == 1 { "" } else { "s" },
            if count == 1 { "s" } else { "" },
            if count == 1 { "is" } else { "are" }
        ));
    }
    lines
}
