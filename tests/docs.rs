//! The docs that are generated from the code, or have to cover all of it.

use std::fs;
use std::path::PathBuf;

use auditroo::findings::Rule;

fn docs_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join(name)
}

#[test]
fn command_reference_is_current() {
    let expected = auditroo::cli::command_reference();
    let path = docs_path("commands.md");
    if std::env::var_os("AUDITROO_UPDATE_DOCS").is_some() {
        fs::write(&path, &expected).expect("write docs/commands.md");
        return;
    }
    let actual = fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        actual == expected,
        "docs/commands.md is out of date. Run: AUDITROO_UPDATE_DOCS=1 cargo test --test docs"
    );
}

#[test]
fn every_rule_is_explained_in_the_findings_reference() {
    let findings = fs::read_to_string(docs_path("findings.md")).expect("read docs/findings.md");
    let missing: Vec<&str> = Rule::ALL
        .iter()
        .map(|rule| rule.id())
        .filter(|id| !findings.contains(&format!("### `{id}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/findings.md has no section for: {}",
        missing.join(", ")
    );
}

#[test]
fn the_action_installs_the_matching_release_by_default() {
    let action = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("action.yml"))
        .expect("read action.yml");
    let expected = format!("default: v{}", env!("CARGO_PKG_VERSION"));
    assert!(
        action.contains(&expected),
        "action.yml must install the release of this version by default ({expected}). RELEASE.md lists this step"
    );
}
