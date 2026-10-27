//! JUnit XML, for CI systems that show test results.
//!
//! Each group of findings is one test case. Groups at or above the fail-on
//! level fail; groups below it are marked as skipped, so they are visible
//! without counting as passed tests.

use super::human::{detail_lines, sentence};
use super::{groups, site_for};
use crate::report::Report;

pub fn render(report: &Report) -> String {
    let site = site_for(report);
    let groups = groups(report);
    let failures = groups.iter().filter(|group| group.fails).count();
    let skipped = groups.iter().filter(|group| !group.fails).count();
    let tests = groups.len().max(1);
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<testsuite name=\"auditroo {}\" tests=\"{tests}\" failures=\"{failures}\" skipped=\"{skipped}\" errors=\"0\">\n",
        report.command.as_str()
    ));
    if groups.is_empty() {
        xml.push_str(&format!(
            "  <testcase classname=\"auditroo\" name=\"{}\" />\n",
            escape(&super::headline(report))
        ));
    }
    for group in &groups {
        let mut body = String::new();
        for finding in &group.findings {
            body.push_str(&sentence(finding, &site));
            body.push('\n');
            for line in detail_lines(finding, &site) {
                body.push_str("    ");
                body.push_str(&line);
                body.push('\n');
            }
        }
        xml.push_str(&format!(
            "  <testcase classname=\"auditroo.{}\" name=\"{}\">\n",
            group.rule.id(),
            escape(&format!("{}: {}", group.severity, group.summary()))
        ));
        if group.fails {
            xml.push_str(&format!(
                "    <failure message=\"{}\">{}</failure>\n",
                escape(&group.summary()),
                escape(&body)
            ));
        } else {
            let reason = if group.ignored {
                "ignored by an ignore rule".to_owned()
            } else {
                format!("below the fail-on level ({})", report.fail_on)
            };
            xml.push_str(&format!(
                "    <skipped message=\"{}\" />\n",
                escape(&reason)
            ));
            xml.push_str(&format!("    <system-out>{}</system-out>\n", escape(&body)));
        }
        xml.push_str("  </testcase>\n");
    }
    xml.push_str("</testsuite>\n");
    xml
}

fn escape(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            '\t' | '\n' | '\r' => ch,
            ch if (ch as u32) < 0x20 => '\u{fffd}',
            ch => ch,
        })
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
