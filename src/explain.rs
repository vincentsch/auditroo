//! `auditroo explain`: what a rule checks and what to do about it, printed
//! from the rule's section in docs/findings.md, so the terminal and the docs
//! say the same thing.

use crate::error::CliError;
use crate::findings::Rule;

const FINDINGS: &str = include_str!("../docs/findings.md");
const DOCS_URL: &str = "https://github.com/vincentsch/auditroo/blob/main/docs/findings.md";

/// The section of docs/findings.md for one rule, without its heading.
pub fn section(rule: Rule) -> Option<String> {
    let heading = format!("### `{}`", rule.id());
    let mut lines = FINDINGS.lines().skip_while(|line| *line != heading);
    lines.next()?;
    let mut in_code = false;
    let body: Vec<&str> = lines
        .take_while(|line| {
            if line.starts_with("```") {
                in_code = !in_code;
            }
            in_code || !line.starts_with('#')
        })
        .collect();
    Some(body.join("\n").trim().to_owned())
}

/// The text for `auditroo explain <rule>`. `name` may also be written the
/// way the output shows it, as "rule: title_changed".
pub fn explain(name: &str) -> Result<String, CliError> {
    let id = name.trim();
    let id = id.strip_prefix("rule:").unwrap_or(id).trim();
    let rule = Rule::from_id(id).ok_or_else(|| {
        CliError::usage(format!(
            "there is no rule {id:?}. Run auditroo explain without a rule to list them"
        ))
    })?;
    let section = section(rule).unwrap_or_default();
    let mut out = format!("{}\n\n{}\n\n", rule.id(), plain(&section));
    out.push_str(&ignore_rule(rule));
    out.push_str("A run fails on high and critical findings, unless you change --fail-on.\n");
    out.push_str(&format!("More: {DOCS_URL}#{}\n", rule.id()));
    Ok(out)
}

/// A ready ignore rule for the rule, because that is what you need next
/// when a finding is expected.
fn ignore_rule(rule: Rule) -> String {
    let id = rule.id();
    let (table, help) = match rule {
        Rule::Assertion | Rule::QualityBudget => (
            "{ assertion = \"your-check-id\" }".to_owned(),
            "Use the id that the output shows after \"assertion:\". To try the\nrule first, pass it on the command line:",
        ),
        // These findings are about the whole site or its robots.txt, so a
        // page URL would not match them.
        Rule::SitemapMissing
        | Rule::SitemapUrlsRemoved
        | Rule::RobotsBlocked
        | Rule::RobotsError
        | Rule::RobotsBlocksSite => (
            format!("{{ rule = \"{id}\" }}"),
            "To try the rule first, pass it on the command line:",
        ),
        _ => (
            format!("{{ rule = \"{id}\", url = \"/docs/\" }}"),
            "Set url to the path that the output shows. \"/docs/\" matches only that\npage, and \"/docs/*\" every page under it. Without url, the rule covers\nevery page. To try the rule first, pass it on the command line:",
        ),
    };
    format!(
        "To accept these findings on purpose, add this line to the ignore list\nunder [gate] in auditroo.toml:\n\n    {table},\n\n{help}\n\n    --ignore '{table}'\n\n"
    )
}

/// The text for `auditroo explain` without a rule: every rule id, under
/// the headings of docs/findings.md.
pub fn list() -> String {
    let mut out = String::from("Rules, as docs/findings.md groups them:\n");
    let mut heading = None;
    for line in FINDINGS.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            heading = Some(title);
        } else if let Some(id) = line
            .strip_prefix("### `")
            .and_then(|rest| rest.strip_suffix('`'))
        {
            if let Some(title) = heading.take() {
                out.push_str(&format!("\n{title}\n"));
            }
            out.push_str(&format!("  {id}\n"));
        }
    }
    out.push_str("\nRun auditroo explain <rule> to see what one of them means.\n");
    out
}

/// Markdown as plain text for a terminal: no bold or backticks, links as
/// their text, and code blocks indented.
fn plain(markdown: &str) -> String {
    let mut out = Vec::new();
    let mut in_code = false;
    for line in markdown.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push(format!("    {line}"));
            continue;
        }
        let mut text = line.replace("**", "").replace('`', "");
        while let Some(start) = text.find('[') {
            let Some(middle) = text[start..].find("](").map(|index| start + index) else {
                break;
            };
            let Some(end) = text[middle..].find(')').map(|index| middle + index) else {
                break;
            };
            text = format!(
                "{}{}{}",
                &text[..start],
                &text[start + 1..middle],
                &text[end + 1..]
            );
        }
        out.push(text);
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_has_text() {
        for rule in Rule::ALL {
            let text = section(*rule).unwrap_or_default();
            assert!(text.contains("What to do"), "{}: {text:?}", rule.id());
        }
    }

    #[test]
    fn every_rule_prints_an_ignore_rule_that_works() {
        for rule in Rule::ALL {
            let text = ignore_rule(*rule);
            let table = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("--ignore '"))
                .and_then(|rest| rest.strip_suffix('\''))
                .unwrap_or_else(|| panic!("{}: {text}", rule.id()));
            assert!(text.contains(&format!("    {table},\n")), "{text}");
            let parsed = crate::config::parse_cli_ignore_rule(table)
                .unwrap_or_else(|error| panic!("{table}: {error}"));
            match rule {
                Rule::Assertion | Rule::QualityBudget => {
                    assert_eq!(parsed.assertion.as_deref(), Some("your-check-id"))
                }
                _ => assert_eq!(parsed.rule.as_deref(), Some(rule.id())),
            }
        }
    }

    #[test]
    fn a_section_ends_at_the_next_heading_but_not_inside_code() {
        let text = section(Rule::CanonicalExternal).unwrap();
        assert!(text.starts_with("**High** in `check`."), "{text}");
        assert!(text.contains("{ rule = \"canonical_external\", url = \"/syndicated/*\" },"));
        assert!(text.ends_with("A wrong base URL\nsetting in the build is the usual cause."));
        assert!(!text.contains("canonical_changed`\n"));
    }

    #[test]
    fn markdown_becomes_plain_text() {
        assert_eq!(
            plain("**High.** See [the guide](own-site.md) for `check`.\n```toml\n[gate]\n```"),
            "High. See the guide for check.\n    [gate]"
        );
    }
}
