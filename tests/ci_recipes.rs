//! Runs the shell step of the Cloudflare Pages recipe in docs/ci.md with
//! bash, the way a GitHub runner does, against a stand-in for the GitHub
//! CLI. The recipe is code that people copy, so it gets a test like the
//! action itself.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// The script of the step that waits for Cloudflare's check.
fn wait_step() -> String {
    let docs = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/ci.md"))
        .expect("read docs/ci.md");
    let step = docs
        .split("- name: Wait for Cloudflare's build of this commit")
        .nth(1)
        .expect("docs/ci.md has the Cloudflare wait step");
    let script = step
        .split("        run: |\n")
        .nth(1)
        .expect("the step has a run block");
    script
        .lines()
        .take_while(|line| line.is_empty() || line.starts_with("          "))
        .map(|line| line.get(10..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

struct Run {
    status: Option<i32>,
    stderr: String,
    outputs: String,
    calls: Vec<String>,
}

/// Run the step. `answers` are the check runs that `gh api` returns, one
/// per call; the last one repeats.
fn run_wait_step(answers: &[&str]) -> Option<Run> {
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("jq is not installed, so the Cloudflare recipe is not tested");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for (index, answer) in answers.iter().enumerate() {
        fs::write(
            dir.path().join(format!("answer-{}.json", index + 1)),
            answer,
        )
        .unwrap();
    }
    let script = |name: &str, body: &str| {
        let path = bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    };
    // gh api: log the arguments, then print the next answer through the
    // --jq filter, as the real gh does.
    script(
        "gh",
        &format!(
            r#"#!/bin/sh
dir='{dir}'
n=$(( $(cat "$dir/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$dir/count"
echo "$*" >> "$dir/calls"
file="$dir/answer-$n.json"
[ -f "$file" ] || file="$dir/answer-{last}.json"
while [ $# -gt 0 ]; do [ "$1" = --jq ] && filter=$2; shift; done
jq -c "$filter" "$file"
"#,
            dir = dir.path().display(),
            last = answers.len()
        ),
    );
    // With these two, the 15 minute wait is five quick rounds.
    script("sleep", "#!/bin/sh\nexit 0\n");
    script("seq", "#!/bin/sh\necho 1 2 3 4 5\n");
    let outputs = dir.path().join("outputs");
    let output = Command::new("bash")
        .args(["-e", "-c", &wait_step()])
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("GITHUB_REPOSITORY", "brightlake/site")
        .env("GITHUB_OUTPUT", &outputs)
        .env("SHA", "4f9c2e1")
        .env("BRANCH", "Feature/New_Pricing")
        .env("CHECK_NAME", "Cloudflare Pages")
        .env("PAGES_PROJECT", "brightlake")
        .env("GH_TOKEN", "test")
        .output()
        .expect("bash runs");
    Some(Run {
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        outputs: fs::read_to_string(&outputs).unwrap_or_default(),
        calls: fs::read_to_string(dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect(),
    })
}

fn check_run(status: &str, conclusion: &str, summary: &str) -> String {
    serde_json::json!({
        "total_count": 1,
        "check_runs": [{
            "name": "Cloudflare Pages",
            "status": status,
            "conclusion": if conclusion.is_empty() { serde_json::Value::Null } else { conclusion.into() },
            "output": { "title": "Deployed", "summary": summary, "text": null },
        }],
    })
    .to_string()
}

#[test]
fn the_step_waits_for_the_check_and_takes_the_address_from_it() {
    let running = check_run("in_progress", "", "");
    let done = check_run(
        "completed",
        "success",
        "Preview URL: https://3f2a9c1d.brightlake.pages.dev\nBranch preview URL: https://feature-new-pricing.brightlake.pages.dev",
    );
    let Some(run) = run_wait_step(&[&running, &running, &done]) else {
        return;
    };
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    assert_eq!(run.outputs, "url=https://3f2a9c1d.brightlake.pages.dev\n");
    assert_eq!(run.calls.len(), 3, "{:?}", run.calls);
    assert!(
        run.calls[0].contains("repos/brightlake/site/commits/4f9c2e1/check-runs")
            && run.calls[0].contains("check_name=Cloudflare Pages"),
        "{:?}",
        run.calls
    );
}

#[test]
fn without_an_address_in_the_check_the_step_builds_the_branch_alias() {
    let done = check_run("completed", "success", "Build finished.");
    let Some(run) = run_wait_step(&[&done]) else {
        return;
    };
    assert_eq!(run.status, Some(0), "{}", run.stderr);
    assert_eq!(
        run.outputs,
        "url=https://feature-new-pricing.brightlake.pages.dev\n"
    );
}

#[test]
fn a_failed_build_fails_the_step() {
    let failed = check_run("completed", "failure", "");
    let Some(run) = run_wait_step(&[&failed]) else {
        return;
    };
    assert_eq!(run.status, Some(1));
    assert!(
        run.stderr
            .contains("Cloudflare's build of 4f9c2e1 ended with \"failure\"."),
        "{}",
        run.stderr
    );
    assert!(run.outputs.is_empty());
}

#[test]
fn no_check_at_all_fails_the_step_after_the_wait() {
    let none = r#"{"total_count":0,"check_runs":[]}"#;
    let Some(run) = run_wait_step(&[none]) else {
        return;
    };
    assert_eq!(run.status, Some(1));
    assert!(
        run.stderr
            .contains("No finished check named \"Cloudflare Pages\" on 4f9c2e1 after 15 minutes."),
        "{}",
        run.stderr
    );
    assert_eq!(run.calls.len(), 5);
}
