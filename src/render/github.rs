//! GitHub Actions workflow commands, so findings show up as annotations on
//! the run and the pull request.

use super::human::sentence;
use super::{groups, site_for};
use crate::findings::Severity;
use crate::report::Report;

/// GitHub shows at most ten annotations of each kind per step.
const MAX_ANNOTATIONS: usize = 10;

pub fn render(report: &Report) -> String {
    let site = site_for(report);
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for group in groups(report) {
        if group.ignored || group.severity < Severity::Medium && !group.fails {
            continue;
        }
        let lines: Vec<String> = group
            .findings
            .iter()
            .take(20)
            .map(|finding| sentence(finding, &site))
            .collect();
        let mut message = lines.join("\n");
        if group.findings.len() > lines.len() {
            message.push_str(&format!(
                "\nand {} more",
                group.findings.len() - lines.len()
            ));
        }
        let title = format!("auditroo: {}", group.summary());
        let command = format!(
            "::{} title={}::{}",
            if group.fails { "error" } else { "warning" },
            escape_property(&title),
            escape_data(&message)
        );
        if group.fails {
            errors.push(command);
        } else {
            warnings.push(command);
        }
    }
    errors.truncate(MAX_ANNOTATIONS);
    warnings.truncate(MAX_ANNOTATIONS);
    let mut out = String::new();
    for line in errors.into_iter().chain(warnings) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn escape_data(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn escape_property(text: &str) -> String {
    escape_data(text).replace(':', "%3A").replace(',', "%2C")
}
