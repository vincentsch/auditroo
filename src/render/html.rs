//! A standalone HTML report: one file with inline styles and no scripts,
//! good to attach to a pull request or send to a client.

use std::sync::OnceLock;

use regex::Regex;

use super::{
    format_time, groups, headline, impact_line, incomplete_line, scope_sentence, site_for,
    summary_lines, verdict, Group,
};
use crate::analysis::LogAnalysisReport;
use crate::findings::{format_count, Finding, Severity};
use crate::report::Report;
use crate::site::Site;

/// URLs listed per group before "and N more".
const URL_LIMIT: usize = 200;

pub fn render(report: &Report) -> String {
    render_with_logs(report, None)
}

/// The report, with an access log section when a log analysis is given.
pub fn render_with_logs(report: &Report, logs: Option<&LogAnalysisReport>) -> String {
    let site = site_for(report);
    let groups = groups(report);
    let host = url::Url::parse(&report.current.start_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| report.current.start_url.clone());
    let mut html = String::with_capacity(32 * 1024);
    html.push_str(&format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:\">\n<title>auditroo report for {}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n<main>\n",
        escape(&host)
    ));

    let verdict_class = if report.passed { "pass" } else { "fail" };
    html.push_str(&format!(
        "<header class=\"top\">\n<p class=\"brand\">auditroo {}</p>\n<p class=\"verdict {verdict_class}\">{}</p>\n<h1>{}</h1>\n<p class=\"scope\">{}</p>\n<dl class=\"meta\">\n<div><dt>Fails on</dt><dd>{} and above</dd></div>\n<div><dt>Pages checked</dt><dd>{}</dd></div>\n<div><dt>Report created</dt><dd>{}</dd></div>\n</dl>\n</header>\n",
        escape(report.command.as_str()),
        verdict(report),
        escape(&headline(report)),
        prose(&scope_sentence(report)),
        report.fail_on,
        format_count(report.current.pages),
        escape(&format_time(report.created_at)),
    ));
    if let Some(line) = incomplete_line(report) {
        html.push_str(&format!("<p class=\"incomplete\">{}</p>\n", prose(&line)));
    }

    html.push_str("<section class=\"tiles\" aria-label=\"Findings by severity\">\n");
    for severity in Severity::ALL_DESCENDING {
        let count = report.counts.get(severity);
        let failing = severity >= report.fail_on && count > 0;
        html.push_str(&format!(
            "<div class=\"tile sev-{}{}\"><span class=\"count\">{}</span><span class=\"label\">{}</span></div>\n",
            severity.as_str(),
            if failing { " failing" } else { "" },
            format_count(count),
            severity.title()
        ));
    }
    html.push_str("</section>\n");

    let failing_lines: Vec<_> = summary_lines(report)
        .into_iter()
        .filter(|line| line.fails)
        .collect();
    if !failing_lines.is_empty() {
        html.push_str("<section class=\"summary\">\n<h2>What would hurt search</h2>\n<ul>\n");
        for line in failing_lines {
            html.push_str(&format!(
                "<li><span class=\"pill sev-{}\">{}</span> {}</li>\n",
                line.severity.as_str(),
                line.severity.title(),
                prose(&line.text)
            ));
        }
        html.push_str("</ul>\n</section>\n");
    }

    let visible: Vec<&Group<'_>> = groups.iter().filter(|group| !group.ignored).collect();
    if visible.is_empty() {
        html.push_str("<section class=\"empty\"><p>No findings. Everything auditroo checks looks fine.</p></section>\n");
    } else {
        html.push_str("<section class=\"findings\">\n<h2>Findings</h2>\n");
        for severity in Severity::ALL_DESCENDING {
            let in_severity: Vec<&&Group<'_>> = visible
                .iter()
                .filter(|group| group.severity == severity)
                .collect();
            if in_severity.is_empty() {
                continue;
            }
            html.push_str(&format!(
                "<h3 class=\"severity-heading sev-{}\">{}</h3>\n",
                severity.as_str(),
                severity.title()
            ));
            for group in in_severity {
                render_group(&mut html, group, report, &site);
            }
        }
        html.push_str("</section>\n");
    }

    if !report.notes.is_empty() {
        html.push_str("<section class=\"notes\">\n<h2>Notes</h2>\n<ul>\n");
        for note in &report.notes {
            html.push_str(&format!("<li>{}</li>\n", prose(note)));
        }
        html.push_str("</ul>\n</section>\n");
    }

    let ignored: Vec<&Finding> = report
        .findings
        .iter()
        .filter(|finding| finding.ignored)
        .collect();
    if !ignored.is_empty() {
        html.push_str(&format!(
            "<section class=\"ignored\">\n<details>\n<summary>{} finding{} matched an ignore rule</summary>\n<ul>\n",
            ignored.len(),
            if ignored.len() == 1 { "" } else { "s" }
        ));
        for finding in ignored {
            html.push_str(&format!(
                "<li>{} <code>{}</code></li>\n",
                sentence_html(finding, &site),
                finding.rule.id()
            ));
        }
        html.push_str("</ul>\n</details>\n</section>\n");
    }

    if let Some(logs) = logs {
        html.push_str(&format!(
            "<section class=\"logs\">\n<h2>Access logs</h2>\n<pre>{}</pre>\n</section>\n",
            escape(&crate::analysis::render_log_human(logs))
        ));
    }

    html.push_str(&format!(
        "<footer>Made with auditroo {}. <a href=\"https://github.com/vincentsch/auditroo/blob/main/docs/findings.md\">What each finding means</a>.</footer>\n</main>\n</body>\n</html>\n",
        escape(&report.auditroo_version)
    ));
    html
}

fn render_group(html: &mut String, group: &Group<'_>, report: &Report, site: &Site) {
    let severity = group.severity.as_str();
    html.push_str(&format!(
        "<article class=\"group sev-{severity}\">\n<header>\n<span class=\"pill sev-{severity}\">{}</span>\n<h4>{}</h4>\n{}</header>\n<p class=\"advice\">{}</p>\n",
        group.severity.title(),
        escape(&group.summary()),
        if group.fails {
            "<span class=\"gate\">fails the check</span>\n"
        } else {
            ""
        },
        prose(group.rule.advice())
    ));
    if group.collapsed() {
        let urls: Vec<&str> = group
            .findings
            .iter()
            .filter_map(|finding| finding.url.as_deref())
            .collect();
        html.push_str(&format!(
            "<details open>\n<summary>{} URLs</summary>\n",
            format_count(urls.len())
        ));
        push_url_list(html, urls.iter().copied(), urls.len(), site);
        html.push_str("</details>\n");
        if let Some(example) = group
            .findings
            .first()
            .filter(|finding| finding.url.is_some())
        {
            html.push_str("<p class=\"detail\">For example:</p>\n");
            render_finding(html, example, report, site);
        }
    } else {
        for finding in &group.findings {
            render_finding(html, finding, report, site);
        }
    }
    html.push_str(&format!(
        "<p class=\"rule-id\">Rule <code>{}</code></p>\n</article>\n",
        group.rule.id()
    ));
}

fn render_finding(html: &mut String, finding: &Finding, report: &Report, site: &Site) {
    html.push_str("<div class=\"finding\">\n");
    html.push_str(&format!(
        "<p class=\"what\">{}</p>\n",
        sentence_html(finding, site)
    ));
    if finding.before.is_some() || finding.after.is_some() {
        html.push_str("<table class=\"change\">\n");
        if let Some(before) = &finding.before {
            html.push_str(&format!(
                "<tr class=\"before\"><th>Before</th><td>{}</td></tr>\n",
                escape(before)
            ));
        }
        if let Some(after) = &finding.after {
            html.push_str(&format!(
                "<tr class=\"after\"><th>After</th><td>{}</td></tr>\n",
                escape(after)
            ));
        }
        html.push_str("</table>\n");
    }
    for detail in &finding.details {
        html.push_str(&format!("<p class=\"detail\">{}</p>\n", prose(detail)));
    }
    if let Some(line) = impact_line(report, finding) {
        html.push_str(&format!("<p class=\"detail\">{}</p>\n", prose(&line)));
    }
    if !finding.related.is_empty() {
        let total = finding.related_total.max(finding.related.len());
        html.push_str(&format!(
            "<details>\n<summary>{} {} URL{}</summary>\n",
            capitalize_label(finding.rule.related_label()),
            format_count(total),
            if total == 1 { "" } else { "s" }
        ));
        push_url_list(
            html,
            finding.related.iter().map(String::as_str),
            total,
            site,
        );
        html.push_str("</details>\n");
    }
    if !finding.follow_on.is_empty() {
        let count = finding.follow_on.len();
        html.push_str(&format!(
            "<p class=\"detail\">{count} other finding{} on this page come{} from this one.</p>\n",
            if count == 1 { "" } else { "s" },
            if count == 1 { "s" } else { "" }
        ));
    }
    html.push_str("</div>\n");
}

fn push_url_list<'a>(
    html: &mut String,
    urls: impl Iterator<Item = &'a str>,
    total: usize,
    site: &Site,
) {
    html.push_str("<ul class=\"urls\">\n");
    let mut shown = 0;
    for url in urls.take(URL_LIMIT) {
        html.push_str(&format!("<li>{}</li>\n", link(url, site)));
        shown += 1;
    }
    if total > shown {
        html.push_str(&format!(
            "<li class=\"more\">and {} more in the JSON report</li>\n",
            format_count(total - shown)
        ));
    }
    html.push_str("</ul>\n");
}

fn sentence_html(finding: &Finding, site: &Site) -> String {
    match &finding.url {
        Some(url) => format!("{} {}", link(url, site), prose(&finding.message)),
        None => prose(&super::capitalize(&finding.message)),
    }
}

/// A link for http(s) URLs, plain text for anything else.
fn link(url: &str, site: &Site) -> String {
    let text = escape(&site.display(url));
    if url.starts_with("http://") || url.starts_with("https://") {
        format!(
            "<a class=\"url\" href=\"{}\" rel=\"noopener noreferrer\">{text}</a>",
            escape(url)
        )
    } else {
        format!("<span class=\"url\">{text}</span>")
    }
}

fn capitalize_label(label: &str) -> String {
    super::capitalize(label)
}

/// Escaped text in which flags such as --production-url stay on one line.
/// A browser may otherwise break the line at a hyphen inside the flag.
fn prose(text: &str) -> String {
    static FLAG: OnceLock<Regex> = OnceLock::new();
    let flag = FLAG.get_or_init(|| {
        Regex::new(r"(^|[\s(])(--[a-z][a-z0-9-]*[a-z0-9])").expect("the flag pattern is valid")
    });
    flag.replace_all(&escape(text), "$1<span class=\"flag\">$2</span>")
        .into_owned()
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const STYLE: &str = r#"
:root {
  --bg: #f6f6f3; --card: #ffffff; --text: #1d2125; --muted: #5d6874; --line: #e2e5e9;
  --critical: #b42318; --high: #c2410c; --medium: #a16207; --low: #1d4ed8; --info: #5d6874;
  --pass: #15803d; --before: #fdecea; --after: #e8f5ec; --code: #f0f1f3;
  color-scheme: light dark;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #101317; --card: #171b21; --text: #e5e8eb; --muted: #9aa4ae; --line: #2a3038;
    --critical: #f97066; --high: #fb923c; --medium: #facc15; --low: #7aa2ff; --info: #9aa4ae;
    --pass: #4ade80; --before: #3a1d1d; --after: #173322; --code: #232931;
  }
  .pill, .verdict { color: #101317 !important; }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--text);
  font: 15px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif; }
main { max-width: 980px; margin: 0 auto; padding: 32px 20px 48px; }
a { color: inherit; }
code { font: 13px/1.4 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: var(--code); padding: 1px 5px; border-radius: 4px; }
h1 { font-size: 30px; line-height: 1.2; margin: 8px 0; letter-spacing: -0.01em; }
h2 { font-size: 19px; margin: 36px 0 12px; }
h4 { font-size: 16px; margin: 0; }
.top { background: var(--card); border: 1px solid var(--line); border-radius: 14px; padding: 24px 28px; }
.brand { margin: 0; color: var(--muted); font-size: 13px; letter-spacing: 0.06em; text-transform: uppercase; }
.verdict { display: inline-block; margin: 14px 0 0; padding: 3px 12px; border-radius: 999px; font-weight: 700; font-size: 13px; letter-spacing: 0.08em; color: #fff; }
.verdict.fail { background: var(--critical); }
.verdict.pass { background: var(--pass); }
.scope { color: var(--muted); margin: 6px 0 16px; overflow-wrap: anywhere; }
.meta { display: flex; flex-wrap: wrap; gap: 8px 28px; margin: 0; }
.meta div { min-width: 120px; }
.meta dt { color: var(--muted); font-size: 12px; text-transform: uppercase; letter-spacing: 0.05em; }
.meta dd { margin: 0; font-weight: 600; }
.tiles { display: grid; grid-template-columns: repeat(5, minmax(0, 1fr)); gap: 10px; margin-top: 16px; }
.tile { background: var(--card); border: 1px solid var(--line); border-radius: 12px; padding: 12px 14px; }
.tile .count { display: block; font-size: 26px; font-weight: 700; line-height: 1.1; }
.tile .label { color: var(--muted); font-size: 13px; }
.tile.failing { border-width: 2px; }
.tile.sev-critical.failing { border-color: var(--critical); }
.tile.sev-high.failing { border-color: var(--high); }
.tile.sev-medium.failing { border-color: var(--medium); }
.tile.sev-low.failing { border-color: var(--low); }
.summary ul, .notes ul { padding-left: 0; list-style: none; }
.summary li { margin: 6px 0; }
.notes li { margin: 6px 0; padding-left: 14px; border-left: 3px solid var(--line); }
.pill { display: inline-block; min-width: 64px; text-align: center; padding: 1px 8px; border-radius: 999px; font-size: 12px; font-weight: 700; color: #fff; background: var(--info); }
.pill.sev-critical { background: var(--critical); }
.pill.sev-high { background: var(--high); }
.pill.sev-medium { background: var(--medium); color: #1d2125; }
.pill.sev-low { background: var(--low); }
.severity-heading { font-size: 14px; text-transform: uppercase; letter-spacing: 0.08em; margin: 28px 0 10px; }
.severity-heading.sev-critical { color: var(--critical); }
.severity-heading.sev-high { color: var(--high); }
.severity-heading.sev-medium { color: var(--medium); }
.severity-heading.sev-low { color: var(--low); }
.severity-heading.sev-info { color: var(--info); }
.group { background: var(--card); border: 1px solid var(--line); border-left-width: 5px; border-radius: 12px; padding: 16px 20px; margin: 12px 0; break-inside: avoid; }
.group.sev-critical { border-left-color: var(--critical); }
.group.sev-high { border-left-color: var(--high); }
.group.sev-medium { border-left-color: var(--medium); }
.group.sev-low { border-left-color: var(--low); }
.group.sev-info { border-left-color: var(--info); }
.group > header { display: flex; flex-wrap: wrap; align-items: center; gap: 10px; }
.gate { margin-left: auto; font-size: 12px; font-weight: 700; color: var(--critical); text-transform: uppercase; letter-spacing: 0.05em; }
.advice { color: var(--muted); margin: 8px 0 4px; }
.flag { white-space: nowrap; }
.finding { border-top: 1px solid var(--line); margin-top: 12px; padding-top: 12px; }
.what { margin: 0 0 6px; overflow-wrap: anywhere; }
.url { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 13.5px; font-weight: 600; }
.change { border-collapse: collapse; width: 100%; margin: 6px 0; font-size: 14px; }
.change th { width: 70px; text-align: left; vertical-align: top; color: var(--muted); font-weight: 600; padding: 4px 8px; }
.change td { padding: 4px 8px; overflow-wrap: anywhere; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 13px; }
.change .before td { background: var(--before); }
.change .after td { background: var(--after); }
.detail { margin: 4px 0; color: var(--muted); font-size: 14px; overflow-wrap: anywhere; }
details { margin: 6px 0; }
summary { cursor: pointer; color: var(--muted); font-size: 14px; }
.urls { margin: 6px 0; padding-left: 20px; columns: 2; column-gap: 28px; }
.urls li { break-inside: avoid; overflow-wrap: anywhere; }
.urls .more { list-style: none; color: var(--muted); }
.rule-id { margin: 10px 0 0; font-size: 12px; color: var(--muted); }
.incomplete { margin: 14px 0 0; padding: 10px 14px; border: 1px solid var(--line); border-left: 4px solid var(--medium); border-radius: 10px; background: var(--card); }
.empty { background: var(--card); border: 1px solid var(--line); border-radius: 12px; padding: 18px 22px; margin-top: 20px; }
.logs pre { background: var(--card); border: 1px solid var(--line); border-radius: 12px; padding: 16px; overflow-x: auto; font-size: 13px; }
footer { margin-top: 40px; color: var(--muted); font-size: 13px; }
@media (max-width: 720px) {
  .tiles { grid-template-columns: repeat(3, minmax(0, 1fr)); }
  .urls { columns: 1; }
  h1 { font-size: 24px; }
}
@media print {
  body { background: #fff; }
  details { display: block; }
  .group, .top, .tile { border-color: #ccc; }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_stay_on_one_line() {
        assert_eq!(
            prose("pass --production-url with the live site (--max-pages), not a--b"),
            "pass <span class=\"flag\">--production-url</span> with the live site (<span class=\"flag\">--max-pages</span>), not a--b"
        );
        assert_eq!(
            prose("--quiet & more"),
            "<span class=\"flag\">--quiet</span> &amp; more"
        );
        assert!(STYLE.contains(".flag { white-space: nowrap; }"));
    }
}
