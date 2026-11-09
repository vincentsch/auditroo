//! Markdown for GitHub job summaries and pull request comments.

use super::human::{detail_lines, example_lines, sentence};
use super::{groups, headline, incomplete_line, scope_sentence, site_for, Group};
use crate::findings::Severity;
use crate::report::Report;

pub fn render(report: &Report) -> String {
    let site = site_for(report);
    let groups = groups(report);
    let mut out = String::new();

    out.push_str(&format!("### auditroo: {}\n\n", headline(report)));
    if report.passed {
        out.push_str("> [!NOTE]\n");
        out.push_str(&format!(
            "> {} Nothing reached the fail-on level ({}).\n\n",
            scope_sentence(report),
            report.fail_on
        ));
    } else {
        out.push_str("> [!CAUTION]\n");
        out.push_str(&format!(
            "> {} These findings are at or above the fail-on level ({}).\n\n",
            scope_sentence(report),
            report.fail_on
        ));
    }
    if let Some(line) = incomplete_line(report) {
        out.push_str(&format!("> [!WARNING]\n> {}\n\n", escape_inline(&line)));
    }

    let failing_groups: Vec<&Group<'_>> = groups.iter().filter(|group| group.fails).collect();
    if !failing_groups.is_empty() {
        out.push_str(
            "| Severity | Problem | Where | What to do | Rule |\n| --- | --- | --- | --- | --- |\n",
        );
        for group in &failing_groups {
            out.push_str(&format!(
                "| {} | {} | {} | {} | `{}` |\n",
                group.severity.title(),
                escape_cell(&group.summary()),
                escape_cell(&where_cell(group, &site)),
                escape_cell(&escape_inline(group.rule.short_advice())),
                group.ignore_key()
            ));
        }
        out.push('\n');
    }

    let detail_count = report
        .findings
        .iter()
        .filter(|finding| !finding.ignored)
        .count();
    if detail_count > 0 {
        out.push_str(&format!(
            "<details>\n<summary>All {} finding{}</summary>\n\n",
            detail_count,
            if detail_count == 1 { "" } else { "s" }
        ));
        for severity in Severity::ALL_DESCENDING {
            let in_severity: Vec<&Group<'_>> = groups
                .iter()
                .filter(|group| !group.ignored && group.severity == severity)
                .collect();
            if in_severity.is_empty() {
                continue;
            }
            out.push_str(&format!("#### {}\n\n", severity.title()));
            for group in in_severity {
                if group.collapsed() {
                    out.push_str(&format!(
                        "- {}: {}\n",
                        group.summary(),
                        where_cell(group, &site)
                    ));
                    for line in example_lines(group, &site) {
                        out.push_str(&format!("  - {}\n", escape_inline(line.trim())));
                    }
                    continue;
                }
                for finding in &group.findings {
                    out.push_str(&format!(
                        "- {}\n",
                        code_urls(
                            &sentence(finding, &site),
                            finding.url.as_deref().map(|url| site.display(url))
                        )
                    ));
                    for line in detail_lines(finding, &site) {
                        out.push_str(&format!("  - {}\n", escape_inline(&line)));
                    }
                }
            }
            out.push('\n');
        }
        out.push_str("</details>\n\n");
    }

    if !report.notes.is_empty() {
        for note in &report.notes {
            out.push_str(&format!("- {}\n", escape_inline(note)));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "<sub>auditroo {} · fails on {} and above</sub>\n",
        report.auditroo_version, report.fail_on
    ));
    out
}

fn where_cell(group: &Group<'_>, site: &crate::site::Site) -> String {
    let mut urls: Vec<String> = group
        .findings
        .iter()
        .filter_map(|finding| finding.url.clone())
        .collect();
    let mut total = urls.len();
    // A site-wide finding is about its related pages, such as the pages a
    // robots.txt rule blocks, more than about robots.txt itself.
    if urls.is_empty() || group.rule.is_site_wide() {
        if let Some(finding) = group
            .findings
            .first()
            .filter(|finding| !finding.related.is_empty())
        {
            urls = finding.related.clone();
            total = finding.related_total;
        }
    }
    if urls.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = urls
        .iter()
        .take(5)
        .map(|url| format!("`{}`", site.display(url)))
        .collect();
    let rest = total.saturating_sub(shown.len());
    let mut text = shown.join(", ");
    if rest > 0 {
        text.push_str(&format!(" and {rest} more"));
    }
    text
}

/// Put the leading URL in backticks so Markdown does not turn it into
/// formatting.
fn code_urls(sentence: &str, shown_url: Option<String>) -> String {
    match shown_url {
        Some(url) if sentence.starts_with(&url) => {
            format!("`{}`{}", url, escape_inline(&sentence[url.len()..]))
        }
        _ => escape_inline(sentence),
    }
}

fn escape_inline(text: &str) -> String {
    text.replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('*', "\\*")
        .replace('_', "\\_")
}

fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}
