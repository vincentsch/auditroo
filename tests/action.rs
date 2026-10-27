//! Runs the main step of the GitHub Action (action.yml) with bash and the
//! auditroo binary of this build, the way a GitHub runner does.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The shell script of the "Run auditroo" step.
fn run_step() -> String {
    let action = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("action.yml"))
        .expect("read action.yml");
    let step = action
        .split("- name: Run auditroo")
        .nth(1)
        .expect("action.yml has a Run auditroo step");
    let script = step
        .split("      run: |\n")
        .nth(1)
        .expect("the step has a run block");
    script
        .lines()
        .take_while(|line| line.is_empty() || line.starts_with("        "))
        .map(|line| line.get(8..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

struct Run {
    status: Option<i32>,
    stdout: String,
    outputs: String,
    report: PathBuf,
    _dir: tempfile::TempDir,
}

fn run_action(args: &str) -> Run {
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = Path::new(env!("CARGO_BIN_EXE_auditroo"))
        .parent()
        .expect("binary folder")
        .to_owned();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let outputs = dir.path().join("outputs");
    let report = dir.path().join("report.json");
    // GitHub runs a bash step with these options.
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-eo",
            "pipefail",
            "-c",
            &run_step(),
        ])
        .current_dir(dir.path())
        .env("PATH", path)
        .env("AUDITROO_ARGS", args)
        .env("AUDITROO_HTML", "")
        .env("AUDITROO_SUMMARY", "true")
        .env("AUDITROO_ANNOTATIONS", "true")
        .env("AUDITROO_FAIL", "true")
        .env("REPORT", &report)
        .env("GITHUB_OUTPUT", &outputs)
        .env("GITHUB_STEP_SUMMARY", dir.path().join("summary.md"))
        .output()
        .expect("bash runs");
    Run {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        outputs: fs::read_to_string(&outputs).unwrap_or_default(),
        report,
        _dir: dir,
    }
}

#[test]
fn a_command_without_a_report_that_exits_0_is_not_an_error() {
    let run = run_action("check --help");
    assert_eq!(run.status, Some(0), "{}", run.stdout);
    assert!(!run.stdout.contains("::error"), "{}", run.stdout);
    assert!(run.outputs.contains("exit-code=0\n"), "{}", run.outputs);
    assert!(run.outputs.contains("passed=true\n"), "{}", run.outputs);
    assert!(!run.outputs.contains("report="), "{}", run.outputs);
}

#[test]
fn a_usage_error_fails_the_step_with_an_annotation() {
    let run = run_action("check");
    assert_eq!(run.status, Some(2), "{}", run.stdout);
    assert!(
        run.stdout.contains("::error title=auditroo::"),
        "{}",
        run.stdout
    );
    assert!(run.outputs.contains("passed=false\n"), "{}", run.outputs);
    assert!(!run.outputs.contains("report="), "{}", run.outputs);
}

#[test]
fn the_demo_writes_a_report_with_annotations() {
    // The demo exits 0 although it shows failing findings.
    let run = run_action("demo");
    assert_eq!(run.status, Some(0), "{}", run.stdout);
    assert!(run.report.exists());
    assert!(
        run.outputs
            .contains(&format!("report={}\n", run.report.display())),
        "{}",
        run.outputs
    );
    assert!(
        run.stdout
            .contains("::error title=auditroo%3A 1 page now has noindex::"),
        "{}",
        run.stdout
    );
}
