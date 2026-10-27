# Contributing to auditroo

Thanks for helping. This page explains how to build and test auditroo, and
how to add a rule. For setting up auditroo in another project, see
[AGENTS.md](AGENTS.md).

## Build

You need Rust 1.88 or newer.

```sh
cargo build --locked
cargo run --locked -- demo
```

The `browser` feature adds `--render-js`. It needs Chrome or Chromium at
runtime, and `AUDITROO_CHROME` when the browser is not on PATH:

```sh
cargo build --locked --features browser
```

## Checks before a pull request

CI runs these checks. Run them locally before you open a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo clippy --locked --all-targets --features browser -- -D warnings
cargo test --locked --all-targets --features browser
cargo +1.88.0 check --locked --all-targets
cargo +1.88.0 check --locked --all-targets --features browser
cargo +1.88.0 test --locked --all-targets
cargo audit --file Cargo.lock
```

The Makefile has shortcuts. `make ci` runs all of these checks except the
browser tests. `make browser` runs the tests with real Chrome and fails
when Chrome is missing. Without
`AUDITROO_REQUIRE_BROWSER_TESTS=1`, the browser tests print a note and pass
when no browser is installed.

`cargo audit` comes from `cargo install cargo-audit --locked`.

## Tests

All tests run against local servers. None of them need the internet.

| File | What it covers |
| --- | --- |
| `tests/regressions.rs` | One test per deploy mistake, such as a new noindex or a canonical tag on a staging host. Each test changes a copy of a small docs site and checks the exact finding and severity. |
| `tests/check.rs` | Problems that `check` finds on one crawl. |
| `tests/cli.rs` | The binary itself, for example its exit codes and output formats. |
| `tests/docs.rs` | `docs/commands.md` matches the help text, and `docs/findings.md` covers every rule. |
| The other files | One part each, for example robots.txt parsing or one of the experimental features. |

`tests/support/site.rs` builds the docs site that most tests use. When you
fix a wrong or missing finding, add a regression test to
`tests/regressions.rs`.

## Docs

`docs/commands.md` is generated from the `--help` text. After you change a
command or a flag, update it:

```sh
make docs
```

That is the same as `AUDITROO_UPDATE_DOCS=1 cargo test --locked --test docs`.
Each new rule needs a section in `docs/findings.md`. A test fails until it
has one.

## Adding a rule

1. Add the rule to the `rules!` list in `src/findings.rs`, with its id and
   default severity, and the summary text for one finding and for many.
2. Add its advice to `Rule::advice` in the same file. The first sentence
   must say what to do on its own, because the text output and the job
   summary show only that sentence.
3. Raise it in `src/rules/check.rs` for one crawl, or in
   `src/rules/diff.rs` for a change between two crawls.
4. Add a regression test and a section in `docs/findings.md`.

Messages are read by people in a CI log. Write them as short plain
sentences that start after the URL, for example "now returns 500 (it
returned 200 before)".

## Commit messages

Write one plain sentence that says what the change does, for example "Report
redirect loops on linked URLs".

## Security

Report security problems privately as described in [SECURITY.md](SECURITY.md).
