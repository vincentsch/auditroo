//! Command-line interface.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use url::Url;

use crate::analysis::{
    analyze_log_sources, log_report_from_str, render_log_human, write_log_report,
};
use crate::audit::{
    compare_settings, crawl, crawl_notes, ensure_comparable, ensure_crawled, evaluate,
    start_problem, CrawlExtras, Evaluation, ForeignOrigin,
};
use crate::config::{
    parse_cli_field_map, resolve_bot_catalog, resolve_log_settings, resolve_settings,
    resolve_settings_without_env_expansion, validate_start_url, LogCliSources,
    LogSettingsOverrides, ResolvedSettings, SettingsOverrides,
};
use crate::error::{CliError, EXIT_OK};
use crate::findings::{format_count, Severity};
use crate::logs::{
    canonical_or_display_path, origin_from_url, source_path_collides_with, LogFormat,
};
use crate::monitor::{
    aggregate_exit_code, baseline_import, baseline_mark, bundle_from_history, history_list,
    history_show, render_history_list_human, render_json_output, render_monitor_human, run_monitor,
};
use crate::progress::Progress;
use crate::recipes::{copy_recipe, find_recipe, render_recipe, render_recipe_list};
use crate::render::{github, html, human, junit, markdown};
use crate::report::{
    artifact_kind, render_json, report_from_str, write_report, Report, ReportCommand,
};
use crate::rules::strip_header_noindex;
use crate::snapshot::{
    check_writable, read as read_snapshot, snapshot_from_str, write as write_snapshot,
    write_atomically, Snapshot,
};

const ROOT_HELP: &str = "\
Examples:
  auditroo demo
  auditroo check https://example.com
  auditroo compare https://example.com https://my-branch.example.vercel.app
  auditroo snapshot https://example.com --out baseline.json
  auditroo diff https://staging.example.com --baseline baseline.json

Exit codes:
  0  no finding at or above --fail-on
  1  findings at or above --fail-on (the check failed)
  2  usage or configuration error; fix the command or auditroo.toml
  3  network or runtime error; a site could not be reached or crawled, or a
     file could not be written

Docs: https://github.com/vincentsch/auditroo/tree/main/docs";

#[derive(Debug, Parser)]
#[command(
    name = "auditroo",
    version,
    about = "Checks your site for SEO mistakes before a change goes live. auditroo fails the build when a change would hurt search, for example a new noindex or a canonical tag on a staging host.",
    after_help = ROOT_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Crawl one site and report its SEO problems.
    #[command(after_help = CHECK_HELP)]
    Check(CheckCommand),
    /// Crawl production and a preview, and report what got worse on the preview.
    #[command(after_help = COMPARE_HELP)]
    Compare(CompareCommand),
    /// Crawl a site and compare it with a snapshot saved earlier.
    #[command(after_help = DIFF_HELP)]
    Diff(DiffCommand),
    /// Crawl a site and save what search engines see to a JSON file.
    #[command(after_help = SNAPSHOT_HELP)]
    Snapshot(SnapshotCommand),
    /// Show a saved report or snapshot again, in any output format.
    #[command(after_help = REPORT_HELP)]
    Report(ReportArgs),
    /// Compare a small built-in site with a broken copy of it. No network needed.
    #[command(after_help = DEMO_HELP)]
    Demo(DemoCommand),
    /// Show what a rule checks and what to do about its findings.
    #[command(after_help = EXPLAIN_HELP)]
    Explain(ExplainCommand),
    /// Experimental: show which pages of a snapshot your custom checks in auditroo.toml cover.
    Assert(AssertCommand),
    /// Experimental: show ready-made custom checks and copy them into auditroo.toml.
    Recipes(RecipesCommand),
    /// Experimental: show which search and AI bots read which pages, from access logs.
    Logs(LogsCommand),
    /// Experimental: check the sites in auditroo.toml on a schedule and save a history of runs.
    Monitor(MonitorCommand),
    /// Experimental: list or show earlier monitor runs.
    History(HistoryCommand),
    /// Experimental: set which run or snapshot the next monitor run compares with.
    Baseline(BaselineCommand),
    /// Experimental: write a folder with the HTML report and data files of a monitor run.
    Bundle(BundleCommand),
}

const CHECK_HELP: &str = "\
Examples:
  auditroo check https://example.com
  auditroo check https://example.com --fail-on medium --html report.html
  auditroo check https://example.com --max-pages 500 --format markdown

check treats the URL as your live site. If you check a preview or a local
build whose canonical tags and sitemap still use the live domain, pass
--production-url with the live site, or compare the build with production.

docs/findings.md explains every finding and what to do about it.";

const COMPARE_HELP: &str = "\
auditroo crawls both sites with the same settings and matches pages by path,
so the preview can be on any host. It also fetches each production page on
the preview, so removed pages show up even when nothing links to them. When
the preview still uses production URLs, for example in canonical tags,
auditroo reads them on the preview under the same path. URLs on any other
host are reported.

When both sites are local copies of your live site, pass the live site with
--production-url. Otherwise most pages do not count as indexable, and the
output says so above the findings.

When the preview's robots.txt blocks pages that production's robots.txt
allows, auditroo reports the block and still checks those pages on the
preview. You do not need --ignore-robots for that.

Examples:
  auditroo compare https://example.com https://my-branch.example.vercel.app
  auditroo compare https://example.com http://localhost:4321
  auditroo compare https://example.com \"$PREVIEW_URL\" --header 'x-vercel-protection-bypass: ${BYPASS}'

The --header value is in single quotes, so the shell does not expand
${BYPASS}. auditroo reads it from the environment itself, and the secret
does not show up in the process list.

docs/ci.md has recipes for GitHub Actions and other CI systems.";

const DIFF_HELP: &str = "\
Save the baseline with `auditroo snapshot` while the site works as it should.
Then run diff after a change.

Examples:
  auditroo snapshot https://example.com --out baseline.json
  auditroo diff https://staging.example.com --baseline baseline.json
  auditroo diff https://example.com --baseline baseline.json --json --out report.json";

const SNAPSHOT_HELP: &str = "\
Examples:
  auditroo snapshot https://example.com --out baseline.json
  auditroo snapshot https://example.com --out baseline.json --max-pages 500

Next: auditroo diff <url> --baseline <file>";

const REPORT_HELP: &str = "\
Reads a report written by check, compare or diff (--out or --json), a snapshot,
or a log analysis report.

Examples:
  auditroo report report.json
  auditroo report report.json --format markdown >> \"$GITHUB_STEP_SUMMARY\"
  auditroo report report.json --html report.html --junit auditroo.xml";

const DEMO_HELP: &str = "\
The demo starts a small web server on 127.0.0.1. It serves a sample site and
a copy of it with typical deploy mistakes, and compares the two. Nothing
leaves your computer.

Examples:
  auditroo demo
  auditroo demo --html demo-report.html";

const EXPLAIN_HELP: &str = "\
The text is the rule's section of docs/findings.md. It ends with an ignore
rule for auditroo.toml that you can paste. Without a rule, explain lists
every rule id.

Examples:
  auditroo explain title_changed
  auditroo explain";

#[derive(Debug, Args)]
struct CheckCommand {
    /// Start URL, for example https://example.com.
    url: String,
    #[command(flatten)]
    analysis: AnalysisArgs,
    #[command(flatten)]
    gate: GateArgs,
    #[command(flatten)]
    output: OutputArgs,
    #[command(flatten)]
    crawl: CrawlArgs,
}

#[derive(Debug, Args)]
struct CompareCommand {
    /// The live site, for example https://example.com.
    production_url: String,
    /// The preview or staging deployment to check.
    preview_url: String,
    #[command(flatten)]
    analysis: AnalysisArgs,
    #[command(flatten)]
    gate: GateArgs,
    #[command(flatten)]
    output: OutputArgs,
    #[command(flatten)]
    crawl: CrawlArgs,
}

#[derive(Debug, Args)]
struct DiffCommand {
    /// Start URL of the site to check.
    url: String,
    /// Snapshot to compare with, written by `auditroo snapshot`.
    #[arg(long, value_name = "FILE")]
    baseline: PathBuf,
    #[command(flatten)]
    analysis: AnalysisArgs,
    #[command(flatten)]
    gate: GateArgs,
    #[command(flatten)]
    output: OutputArgs,
    #[command(flatten)]
    crawl: CrawlArgs,
}

#[derive(Debug, Args)]
struct SnapshotCommand {
    /// Start URL, for example https://example.com.
    url: String,
    /// Where to write the snapshot.
    #[arg(long, value_name = "FILE")]
    out: PathBuf,
    /// Print a JSON summary instead of text.
    #[arg(long)]
    json: bool,
    /// Do not print progress on stderr.
    #[arg(long, short)]
    quiet: bool,
    #[command(flatten)]
    crawl: CrawlArgs,
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// A report, snapshot or log analysis JSON file.
    path: PathBuf,
    #[command(flatten)]
    output: RenderArgs,
}

#[derive(Debug, Args)]
struct DemoCommand {
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Debug, Args)]
struct ExplainCommand {
    /// Rule id, as the output shows it under each finding, for example
    /// title_changed.
    rule: Option<String>,
}

/// Rules that change how findings are evaluated.
#[derive(Debug, Args, Default, Clone)]
struct AnalysisArgs {
    /// Ignore noindex in X-Robots-Tag headers. Preview hosts such as Vercel and
    /// Netlify add one to every preview, so compare and diff ignore it on their
    /// preview domains without this flag. Robots meta tags are still checked.
    #[arg(long)]
    ignore_noindex_header: bool,
}

#[derive(Debug, Args, Default, Clone)]
struct GateArgs {
    /// Exit with code 1 when a finding has this severity or higher:
    /// critical, high, medium, low or info [default: high, or [gate] fail_on]
    #[arg(long, value_name = "SEVERITY")]
    fail_on: Option<Severity>,
    /// Do not count findings that match, written as a TOML table, for example
    /// '{ rule = "title_changed" }' or '{ url = "/promo/*" }'. Repeat for more.
    /// Adds to [gate] ignore in auditroo.toml.
    #[arg(long, value_name = "RULE")]
    ignore: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    /// Text for a terminal or CI log.
    Human,
    /// The full report as JSON.
    Json,
    /// Markdown for $GITHUB_STEP_SUMMARY or a pull request comment.
    Markdown,
    /// GitHub Actions annotations.
    Github,
}

/// Where and how to write a report.
#[derive(Debug, Args, Default, Clone)]
pub(crate) struct RenderArgs {
    /// Output format on stdout [default: human]
    #[arg(long, value_enum, value_name = "FORMAT")]
    format: Option<Format>,
    /// Same as --format json.
    #[arg(long, conflicts_with = "format")]
    json: bool,
    /// Also write a standalone HTML report to this file.
    #[arg(long, value_name = "FILE")]
    html: Option<PathBuf>,
    /// Also write the report as Markdown to this file.
    #[arg(long, value_name = "FILE")]
    markdown: Option<PathBuf>,
    /// Also write the result as JUnit XML to this file.
    #[arg(long, value_name = "FILE")]
    junit: Option<PathBuf>,
}

#[derive(Debug, Args, Default, Clone)]
pub(crate) struct OutputArgs {
    #[command(flatten)]
    render: RenderArgs,
    /// Also write the full report as JSON to this file.
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Do not print progress on stderr.
    #[arg(long, short)]
    quiet: bool,
}

#[derive(Debug, Args, Default, Clone)]
struct CrawlArgs {
    /// Read settings from this file [default: ./auditroo.toml when it exists]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Stop after this many URLs; 0 means no limit [default: 10000]
    #[arg(long, value_name = "N")]
    max_pages: Option<usize>,
    /// Follow links at most this many clicks from the start page; 0 means no limit [default: 0]
    #[arg(long, value_name = "N")]
    max_depth: Option<usize>,
    /// Fetch this many URLs at the same time [default: 8]
    #[arg(long, value_name = "N")]
    concurrency: Option<usize>,
    /// Wait at least this long between two requests to one host. With the
    /// default, a crawl does about 10 pages a second. compare also uses it for
    /// production, so use 0 only when both sites can take the load
    /// [default: 100]
    #[arg(long, value_name = "MS")]
    rate_limit_ms: Option<u64>,
    /// Give up on one request after this long [default: 10000]
    #[arg(long, value_name = "MS")]
    timeout_ms: Option<u64>,
    /// Treat larger responses as failed [default: 10485760]
    #[arg(long, value_name = "BYTES")]
    max_body_bytes: Option<usize>,
    /// Only crawl paths that match, for example '/docs/*'. Repeat for more.
    #[arg(long, value_name = "PATTERN")]
    include: Vec<String>,
    /// Do not crawl paths that match, for example '/search?*'. Repeat for more.
    #[arg(long, value_name = "PATTERN")]
    exclude: Vec<String>,
    /// User-Agent header [default: auditroo/<version> (+https://github.com/vincentsch/auditroo)]
    #[arg(long, value_name = "TEXT")]
    user_agent: Option<String>,
    /// Send this header to the site, for example a firewall or preview bypass
    /// token. auditroo fills in ${NAME} from the environment. The header only
    /// goes to the start URL's host. Repeat for more.
    #[arg(long, value_name = "NAME: VALUE")]
    header: Vec<String>,
    /// Crawl pages that robots.txt blocks for auditroo. Use it only on your
    /// own sites. In compare it applies to production too. A preview does
    /// not need it when only its new robots.txt blocks a page, because
    /// compare and diff crawl every page that production or the baseline
    /// allowed. robots.txt is still checked for Googlebot.
    #[arg(long)]
    ignore_robots: bool,
    /// Render pages in headless Chrome before reading them. Needs a build
    /// with the browser feature.
    #[arg(long)]
    render_js: bool,
    /// Read this sitemap instead of the ones robots.txt names. Repeat for more.
    #[arg(long, value_name = "URL")]
    sitemap: Vec<String>,
    /// The live site, when the crawled URL is a preview or a local build of
    /// it. Production URLs, for example in canonical tags, then count as the
    /// build's own and are read on the build under the same path. Overrides
    /// [crawl] production_url.
    #[arg(long = "production-url", id = "live_site", value_name = "URL")]
    live_site: Option<String>,
}

#[derive(Debug, Args)]
struct AssertCommand {
    /// Show which pages your checks cover, without crawling. This is the only
    /// mode.
    #[arg(long)]
    dry_run: bool,
    /// Config file [default: ./auditroo.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Snapshot to check the coverage against.
    #[arg(long, value_name = "FILE")]
    snapshot: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct RecipesCommand {
    #[command(subcommand)]
    command: RecipeCommand,
}

#[derive(Debug, Subcommand)]
enum RecipeCommand {
    /// List the built-in recipes.
    List,
    /// Print one recipe.
    Show {
        /// Recipe id, from `auditroo recipes list`.
        id: String,
    },
    /// Add one recipe to the end of a config file.
    Copy {
        /// Recipe id, from `auditroo recipes list`.
        id: String,
        /// Config file to add it to. auditroo creates it when it does not exist.
        #[arg(long, value_name = "FILE", default_value = "auditroo.toml")]
        config: PathBuf,
        /// Show what would be written without changing the file.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
struct LogsCommand {
    #[command(subcommand)]
    command: LogsSubcommand,
}

#[derive(Debug, Subcommand)]
enum LogsSubcommand {
    /// Read access logs and report which bots fetched which pages.
    Analyze(LogAnalyzeCommand),
    /// List the bots auditroo recognizes in logs.
    Bots(LogBotsCommand),
}

#[derive(Debug, Args)]
struct LogAnalyzeCommand {
    /// Config file [default: ./auditroo.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Snapshot of the site, to know which logged URLs are real pages.
    #[arg(long, value_name = "FILE")]
    snapshot: PathBuf,
    /// Log file or glob. Repeat for more. Replaces [[logs.source]] in the config.
    #[arg(long = "logs", value_name = "PATH")]
    logs: Vec<String>,
    /// Format of the --logs files.
    #[arg(long = "log-format", value_name = "FORMAT")]
    log_format: Option<LogFormat>,
    /// Origin for log lines that only have a path [default: the snapshot's origin]
    #[arg(long = "log-origin", value_name = "URL")]
    log_origin: Option<String>,
    /// For generic_jsonl logs: auditroo-field=json-key. Repeat for each field.
    #[arg(long = "field-map", value_name = "FIELD=KEY")]
    field_map: Vec<String>,
    /// Print the full analysis as JSON.
    #[arg(long)]
    json: bool,
    /// Also write the full analysis as JSON to this file.
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Exit with code 1 when a log finding reaches --fail-on.
    #[arg(long, conflicts_with = "no_gate")]
    gate: bool,
    /// Never fail because of log findings.
    #[arg(long = "no-gate")]
    no_gate: bool,
    /// Severity that fails the run; implies --gate [default: high]
    #[arg(long = "fail-on", conflicts_with = "no_gate", value_name = "SEVERITY")]
    fail_on: Option<Severity>,
}

#[derive(Debug, Args)]
struct LogBotsCommand {
    /// Config file [default: ./auditroo.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print the list as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MonitorCommand {
    #[command(subcommand)]
    command: MonitorSubcommand,
}

#[derive(Debug, Subcommand)]
enum MonitorSubcommand {
    /// Crawl one configured site, or all of them, and compare each with its baseline.
    Run(MonitorRunCommand),
}

#[derive(Debug, Args)]
struct MonitorRunCommand {
    /// Site id from [[site]] in the config, or `all`.
    #[arg(long)]
    site: String,
    /// Config file [default: ./auditroo.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print the result as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct HistoryCommand {
    #[command(subcommand)]
    command: HistorySubcommand,
}

#[derive(Debug, Subcommand)]
enum HistorySubcommand {
    /// List recent runs of a site.
    List(HistoryListCommand),
    /// Show one run.
    Show(HistoryShowCommand),
}

#[derive(Debug, Args)]
struct HistoryListCommand {
    /// Site id from [[site]] in the config.
    #[arg(long)]
    site: String,
    /// Config file [default: ./auditroo.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Show at most this many runs.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct HistoryShowCommand {
    /// Run id, from `auditroo history list`.
    run_id: String,
    /// Config file [default: ./auditroo.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BaselineCommand {
    #[command(subcommand)]
    command: BaselineSubcommand,
}

#[derive(Debug, Subcommand)]
enum BaselineSubcommand {
    /// Point a baseline label at the snapshot of a monitor run.
    Mark(BaselineMarkCommand),
    /// Copy a snapshot file into the monitor store and point a label at it.
    Import(BaselineImportCommand),
}

#[derive(Debug, Args)]
struct BaselineMarkCommand {
    /// Run id, or `latest`.
    run_selector: String,
    /// Site id from [[site]] in the config.
    #[arg(long)]
    site: String,
    /// Label to set, for example last-known-good.
    #[arg(long)]
    label: String,
    /// Config file [default: ./auditroo.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BaselineImportCommand {
    /// Snapshot file to import.
    snapshot_path: PathBuf,
    /// Site id from [[site]] in the config.
    #[arg(long)]
    site: String,
    /// Label to set, for example last-known-good.
    #[arg(long)]
    label: String,
    /// Config file [default: ./auditroo.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BundleCommand {
    /// Run id, or `latest`.
    run_selector: String,
    /// Site id from [[site]] in the config.
    #[arg(long)]
    site: String,
    /// Folder to create. It must not exist yet.
    #[arg(long, value_name = "DIR")]
    out: PathBuf,
    /// Config file [default: ./auditroo.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

/// Every command with its options as Markdown. docs/commands.md is this
/// text; a test fails when the two differ.
pub fn command_reference() -> String {
    use clap::CommandFactory;
    let mut root = Cli::command();
    root.build();
    let mut out = String::from(
        "# Command reference\n\n\
         <!-- Generated from auditroo --help. Update with: AUDITROO_UPDATE_DOCS=1 cargo test --test docs -->\n\n\
         Most people need three commands:\n\n\
         - `auditroo check <url>` looks at one site.\n\
         - `auditroo compare <production> <preview>` compares a preview with production.\n\
         - `auditroo demo` shows what a failing run looks like.\n\n\
         The [exit codes](#exit-codes) are the same for every command.\n",
    );
    for command in root.get_subcommands() {
        reference_section(command, &mut out);
    }
    out.push_str(
        "\n## Exit codes\n\n\
         | Code | Meaning |\n\
         | --- | --- |\n\
         | 0 | No finding at or above `--fail-on`. |\n\
         | 1 | At least one finding at or above `--fail-on`. The check failed. |\n\
         | 2 | Usage or configuration error. Fix the command or `auditroo.toml`. |\n\
         | 3 | Network or runtime error. A site could not be reached or crawled, or a file could not be written. |\n",
    );
    out
}

fn reference_section(command: &clap::Command, out: &mut String) {
    if command.get_name() == "help" || command.is_hide_set() {
        return;
    }
    let name = command
        .get_bin_name()
        .unwrap_or(command.get_name())
        .to_owned();
    out.push_str(&format!("\n## {name}\n\n"));
    if let Some(about) = command.get_long_about().or(command.get_about()) {
        out.push_str(&format!(
            "{}\n\n",
            sentence(&markdown_text(&about.to_string()))
        ));
    }
    let usage = command.clone().render_usage().to_string();
    let usage = usage.trim().trim_start_matches("Usage:").trim();
    out.push_str(&format!("```text\n{usage}\n```\n"));

    let rows: Vec<(String, String)> = command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set() && !matches!(arg.get_id().as_str(), "help" | "version"))
        .map(|arg| (reference_arg_name(arg), reference_arg_help(arg)))
        .collect();
    if !rows.is_empty() {
        out.push_str("\n| Option | What it does |\n| --- | --- |\n");
        for (name, help) in rows {
            out.push_str(&format!("| {name} | {help} |\n"));
        }
    }
    if let Some(after) = command.get_after_help() {
        out.push_str(&format!("\n```text\n{}\n```\n", after.to_string().trim()));
    }
    for sub in command.get_subcommands() {
        reference_section(sub, out);
    }
}

fn reference_arg_name(arg: &clap::Arg) -> String {
    let values = arg
        .get_value_names()
        .map(|names| {
            names
                .iter()
                .map(|name| format!("<{name}>"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| format!("<{}>", arg.get_id().as_str().to_uppercase()));
    let name = match (arg.get_short(), arg.get_long()) {
        (None, None) => values,
        (short, Some(long)) => {
            let mut name = String::new();
            if let Some(short) = short {
                name.push_str(&format!("-{short}, "));
            }
            name.push_str(&format!("--{long}"));
            if arg.get_action().takes_values() {
                name.push_str(&format!(" {values}"));
            }
            name
        }
        (Some(short), None) => format!("-{short}"),
    };
    format!("`{name}`")
}

fn reference_arg_help(arg: &clap::Arg) -> String {
    let help = arg
        .get_long_help()
        .or(arg.get_help())
        .map(|help| help.to_string())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut help = sentence(&markdown_text(&help));
    let values: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| format!("`{}`", value.get_name()))
        .collect();
    if !values.is_empty() {
        help.push_str(&format!(" Values: {}.", values.join(" ")));
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    if arg.get_action().takes_values() && !defaults.is_empty() {
        help.push_str(&format!(" [default: {}]", defaults.join(" ")));
    }
    help.replace('|', "\\|")
}

/// Help text as Markdown: quoted examples become code, and `<` outside of
/// code is escaped so GitHub does not read it as an HTML tag.
fn markdown_text(text: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    let mut chars = text.chars().peekable();
    let mut previous = ' ';
    while let Some(c) = chars.next() {
        match c {
            '`' => {
                in_code = !in_code;
                out.push(c);
            }
            '\'' if !in_code && (previous == ' ' || previous == '(') => {
                // An opening quote starts an example; its closing quote ends it.
                let example: String = chars.by_ref().take_while(|&c| c != '\'').collect();
                out.push_str(&format!("`{example}`"));
            }
            '<' if !in_code => out.push_str("&lt;"),
            '>' if !in_code => out.push_str("&gt;"),
            _ => out.push(c),
        }
        previous = c;
    }
    out
}

/// End a help line with a period, unless it ends with `[default: ...]`.
fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() || text.ends_with(['.', ']', ')']) {
        text.to_owned()
    } else {
        format!("{text}.")
    }
}

pub async fn run() -> i32 {
    run_with_args(std::env::args_os()).await
}

pub async fn run_with_args<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return if error.use_stderr() {
                crate::error::EXIT_USAGE
            } else {
                EXIT_OK
            };
        }
    };
    match execute(cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("auditroo: {error}");
            error.exit_code()
        }
    }
}

async fn execute(cli: Cli) -> Result<i32, CliError> {
    match cli.command {
        Command::Check(command) => execute_check(command).await,
        Command::Compare(command) => execute_compare(command).await,
        Command::Diff(command) => execute_diff(command).await,
        Command::Snapshot(command) => execute_snapshot(command).await,
        Command::Report(command) => execute_report(command),
        Command::Demo(command) => crate::demo::run(command.output).await,
        Command::Explain(command) => {
            match command.rule {
                Some(rule) => print!("{}", crate::explain::explain(&rule)?),
                None => print!("{}", crate::explain::list()),
            }
            Ok(EXIT_OK)
        }
        Command::Assert(command) => execute_assert(command),
        Command::Recipes(command) => execute_recipes(command),
        Command::Logs(command) => execute_logs(command),
        Command::Monitor(command) => execute_monitor(command).await,
        Command::History(command) => execute_history(command),
        Command::Baseline(command) => execute_baseline(command),
        Command::Bundle(command) => execute_bundle(command),
    }
}

async fn execute_check(command: CheckCommand) -> Result<i32, CliError> {
    let settings = resolve(&command.crawl, &command.gate)?;
    let start = parse_start_url(&command.url)?;
    command.output.check_paths()?;
    let mut progress = Progress::new(command.output.quiet);
    let started = std::time::Instant::now();
    let mut current = crawl(
        start,
        &settings,
        CrawlExtras::default(),
        &mut progress,
        "Crawling",
    )
    .await?;
    let mut notes = Vec::new();
    if command.analysis.ignore_noindex_header && strip_header_noindex(&mut current) > 0 {
        notes.push(IGNORED_HEADER_NOTE.to_owned());
    }
    ensure_crawled(&current)?;
    let report = evaluate(Evaluation {
        command: ReportCommand::Check,
        baseline: None,
        current: &current,
        settings: &settings,
        notes,
    })
    .await?;
    command.output.emit(&report, Some(started.elapsed()))?;
    Ok(report.exit_code())
}

async fn execute_compare(command: CompareCommand) -> Result<i32, CliError> {
    let settings = resolve(&command.crawl, &command.gate)?;
    let production = parse_start_url(&command.production_url)?;
    let preview = parse_start_url(&command.preview_url)?;
    if production.path() != preview.path() {
        return Err(CliError::usage(format!(
            "the two URLs start on different paths ({} and {}). Use the same path, for example both home pages",
            production.path(),
            preview.path()
        )));
    }
    command.output.check_paths()?;
    let mut progress = Progress::new(command.output.quiet);
    let started = std::time::Instant::now();
    let baseline = crawl(
        production,
        &settings,
        CrawlExtras::default(),
        &mut progress,
        "Crawling production",
    )
    .await?;
    ensure_comparable(&baseline, "production")?;
    let mut current = crawl(
        preview.clone(),
        &settings,
        CrawlExtras::compared_with(&baseline, &preview),
        &mut progress,
        "Crawling preview",
    )
    .await?;
    let mut notes = Vec::new();
    handle_preview_noindex(
        &baseline,
        &mut current,
        &preview,
        command.analysis.ignore_noindex_header,
        &mut notes,
    );
    ensure_crawled(&current)?;
    let report = evaluate(Evaluation {
        command: ReportCommand::Compare,
        baseline: Some(&baseline),
        current: &current,
        settings: &settings,
        notes,
    })
    .await?;
    command.output.emit(&report, Some(started.elapsed()))?;
    Ok(report.exit_code())
}

async fn execute_diff(command: DiffCommand) -> Result<i32, CliError> {
    let baseline = read_snapshot(&command.baseline)?;
    let settings = resolve(&command.crawl, &command.gate)?;
    let start = parse_start_url(&command.url)?;
    let baseline_start = Url::parse(&baseline.start_url)
        .map_err(|_| CliError::usage("the start URL in the baseline is not a valid URL"))?;
    if baseline_start.path() != start.path() || baseline_start.query() != start.query() {
        return Err(CliError::usage(format!(
            "the baseline starts at {} but this run starts at {}. Use a start URL with the same path",
            baseline_start.path(),
            start.path()
        )));
    }
    command.output.check_paths()?;
    ensure_comparable(&baseline, "the baseline").map_err(|error| {
        CliError::usage(format!(
            "{}: {error}. Take a new baseline with auditroo snapshot",
            command.baseline.display()
        ))
    })?;
    let mut notes = compare_settings(&baseline, &settings)?;
    let mut progress = Progress::new(command.output.quiet);
    let started = std::time::Instant::now();
    let mut current = crawl(
        start.clone(),
        &settings,
        CrawlExtras::compared_with(&baseline, &start),
        &mut progress,
        "Crawling",
    )
    .await?;
    handle_preview_noindex(
        &baseline,
        &mut current,
        &start,
        command.analysis.ignore_noindex_header,
        &mut notes,
    );
    ensure_crawled(&current)?;
    let report = evaluate(Evaluation {
        command: ReportCommand::Diff,
        baseline: Some(&baseline),
        current: &current,
        settings: &settings,
        notes,
    })
    .await?;
    command.output.emit(&report, Some(started.elapsed()))?;
    Ok(report.exit_code())
}

async fn execute_snapshot(command: SnapshotCommand) -> Result<i32, CliError> {
    let settings = resolve(&command.crawl, &GateArgs::default())?;
    let start = parse_start_url(&command.url)?;
    check_writable(&command.out).map_err(CliError::usage)?;
    let mut progress = Progress::new(command.quiet);
    let snapshot = crawl(
        start,
        &settings,
        CrawlExtras::default(),
        &mut progress,
        "Crawling",
    )
    .await?;
    // A snapshot of a login page or an error is useless as a baseline.
    ensure_crawled(&snapshot)?;
    if let Some(problem) = start_problem(&snapshot) {
        return Err(CliError::runtime(format!(
            "nothing was saved, because {problem}. A baseline needs a site that loads. If a firewall or a login blocks auditroo, docs/own-site.md shows how to let it in"
        )));
    }
    write_snapshot(&snapshot, &command.out).map_err(CliError::runtime)?;
    if command.json {
        print!(
            "{}",
            render_json(&snapshot_summary_json(&snapshot, &command.out))
        );
    } else {
        print!("{}", snapshot_summary(&snapshot, Some(&command.out)));
    }
    Ok(EXIT_OK)
}

fn execute_report(command: ReportArgs) -> Result<i32, CliError> {
    let raw = fs::read_to_string(&command.path).map_err(|error| {
        CliError::usage(format!(
            "{}: {}",
            command.path.display(),
            describe_io(&error)
        ))
    })?;
    let kind = artifact_kind(&raw)
        .map_err(|error| CliError::usage(format!("{}: {error}", command.path.display())))?;
    match kind.as_str() {
        crate::report::REPORT_ARTIFACT_KIND => {
            let report = report_from_str(&raw)
                .map_err(|error| CliError::usage(format!("{}: {error}", command.path.display())))?;
            command.output.into_options().emit(&report, None)?;
        }
        crate::snapshot::SNAPSHOT_ARTIFACT_KIND
        | crate::analysis::LOG_ANALYSIS_REPORT_ARTIFACT_KIND
            if command.output.needs_report() =>
        {
            let what = if kind == crate::snapshot::SNAPSHOT_ARTIFACT_KIND {
                "a snapshot"
            } else {
                "a log analysis report"
            };
            return Err(CliError::usage(format!(
                "{}: this file is {what}, which has no findings to put in a report. Show it as text, or as JSON with --json",
                command.path.display()
            )));
        }
        crate::snapshot::SNAPSHOT_ARTIFACT_KIND => {
            let snapshot = snapshot_from_str(&raw)
                .map_err(|error| CliError::usage(format!("{}: {error}", command.path.display())))?;
            if command.output.wants_json() {
                print!("{}", render_json(&snapshot));
            } else {
                print!("{}", snapshot_summary(&snapshot, None));
            }
        }
        crate::analysis::LOG_ANALYSIS_REPORT_ARTIFACT_KIND => {
            let report = log_report_from_str(&raw)?;
            if command.output.wants_json() {
                print!("{}", render_json(&report));
            } else {
                print!("{}", render_log_human(&report));
            }
        }
        other => {
            return Err(CliError::usage(format!(
                "{}: auditroo cannot show a {other} file",
                command.path.display()
            )))
        }
    }
    Ok(EXIT_OK)
}

const IGNORED_HEADER_NOTE: &str =
    "noindex in X-Robots-Tag headers was ignored (--ignore-noindex-header). Robots meta tags in the HTML were still checked.";

/// Preview hosts that add `X-Robots-Tag: noindex` to every preview.
fn preview_platform(url: &Url) -> Option<&'static str> {
    let host = url.host_str()?.to_ascii_lowercase();
    if host.ends_with(".vercel.app") {
        Some("Vercel")
    } else if host.ends_with(".netlify.app") {
        Some("Netlify")
    } else if host.ends_with(".pages.dev") {
        Some("Cloudflare Pages")
    } else {
        None
    }
}

fn handle_preview_noindex(
    baseline: &Snapshot,
    current: &mut Snapshot,
    current_start: &Url,
    requested: bool,
    notes: &mut Vec<String>,
) {
    let header_noindex = |snapshot: &Snapshot| {
        snapshot
            .records
            .get(&snapshot.start_url)
            .is_some_and(|record| {
                record
                    .indexability
                    .x_robots_tag
                    .iter()
                    .any(|token| token == "noindex" || token == "none")
            })
    };
    if requested {
        if strip_header_noindex(current) > 0 {
            notes.push(IGNORED_HEADER_NOTE.to_owned());
        }
        return;
    }
    if let Some(platform) = preview_platform(current_start) {
        if header_noindex(current) && !header_noindex(baseline) && strip_header_noindex(current) > 0
        {
            notes.push(format!(
                "{platform} adds an X-Robots-Tag: noindex header to preview deployments, so auditroo ignored that header on the preview. Robots meta tags in the HTML were still checked."
            ));
        }
    }
}

fn resolve(crawl: &CrawlArgs, gate: &GateArgs) -> Result<ResolvedSettings, CliError> {
    resolve_settings(crawl.config.as_deref(), crawl.overrides(gate)).map_err(Into::into)
}

/// Parse a start URL, with a hint when the scheme is missing. Errors never
/// repeat a URL that contains a password.
fn parse_start_url(input: &str) -> Result<Url, CliError> {
    match validate_start_url(input) {
        Ok(url) => Ok(url),
        Err(_) if !input.contains("://") => Err(CliError::usage(format!(
            "{input:?} is not a full URL. Add the scheme, for example https://{}/",
            input.trim_end_matches('/')
        ))),
        Err(error) if input.contains('@') => Err(CliError::usage(format!(
            "{error}. Use --header for credentials"
        ))),
        Err(error) => Err(CliError::usage(format!("{input}: {error}"))),
    }
}

impl CrawlArgs {
    fn overrides(&self, gate: &GateArgs) -> SettingsOverrides {
        SettingsOverrides {
            user_agent: self.user_agent.clone(),
            concurrency: self.concurrency,
            rate_limit_ms: self.rate_limit_ms,
            timeout_ms: self.timeout_ms,
            max_body_bytes: self.max_body_bytes,
            max_depth: self.max_depth,
            max_pages: self.max_pages,
            include: (!self.include.is_empty()).then(|| self.include.clone()),
            exclude: (!self.exclude.is_empty()).then(|| self.exclude.clone()),
            respect_robots: self.ignore_robots.then_some(false),
            render_js: self.render_js.then_some(true),
            headers: (!self.header.is_empty()).then(|| self.header.clone()),
            sitemaps: (!self.sitemap.is_empty()).then(|| self.sitemap.clone()),
            production_url: self.live_site.clone(),
            fail_on: gate.fail_on,
            ignore: (!gate.ignore.is_empty()).then(|| gate.ignore.clone()),
        }
    }
}

impl RenderArgs {
    fn format(&self) -> Format {
        if self.json {
            Format::Json
        } else {
            self.format.unwrap_or(Format::Human)
        }
    }

    fn wants_json(&self) -> bool {
        self.format() == Format::Json
    }

    /// Options that only make sense for a findings report.
    fn needs_report(&self) -> bool {
        self.html.is_some()
            || self.markdown.is_some()
            || self.junit.is_some()
            || matches!(self.format(), Format::Markdown | Format::Github)
    }

    fn is_human(&self) -> bool {
        self.format() == Format::Human
    }

    pub(crate) fn into_options(self) -> OutputArgs {
        OutputArgs {
            render: self,
            out: None,
            quiet: false,
        }
    }
}

/// How to print and save a finished report.
impl OutputArgs {
    pub(crate) fn is_human(&self) -> bool {
        self.render.is_human()
    }

    fn check_paths(&self) -> Result<(), CliError> {
        for path in [
            &self.out,
            &self.render.html,
            &self.render.markdown,
            &self.render.junit,
        ]
        .into_iter()
        .flatten()
        {
            check_writable(path).map_err(CliError::usage)?;
        }
        Ok(())
    }

    pub(crate) fn emit(&self, report: &Report, elapsed: Option<Duration>) -> Result<(), CliError> {
        if let Some(path) = &self.out {
            write_report(report, path).map_err(CliError::runtime)?;
        }
        if let Some(path) = &self.render.html {
            write_text(path, &html::render(report))?;
        }
        if let Some(path) = &self.render.markdown {
            write_text(path, &markdown::render(report))?;
        }
        if let Some(path) = &self.render.junit {
            write_text(path, &junit::render(report))?;
        }
        let stdout = match self.render.format() {
            Format::Human => human::render(
                report,
                human::HumanOptions {
                    color: use_color(),
                    elapsed,
                },
            ),
            Format::Json => render_json(report),
            Format::Markdown => markdown::render(report),
            Format::Github => github::render(report),
        };
        print!("{stdout}");
        Ok(())
    }
}

fn write_text(path: &Path, text: &str) -> Result<(), CliError> {
    write_atomically(path, text.as_bytes())
        .map_err(|error| CliError::runtime(format!("{}: {}", path.display(), describe_io(&error))))
}

fn describe_io(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "file not found".to_owned(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        _ => error.to_string(),
    }
}

fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Text summary of a snapshot.
pub fn snapshot_summary(snapshot: &Snapshot, saved_to: Option<&Path>) -> String {
    let mut out = String::new();
    match saved_to {
        Some(path) => out.push_str(&format!(
            "Saved a snapshot of {} to {}\n",
            snapshot.start_url,
            path.display()
        )),
        None => out.push_str(&format!(
            "Snapshot of {}, taken {}\n",
            snapshot.start_url,
            crate::render::format_time(snapshot.created_at)
        )),
    }
    let pages = snapshot
        .pages()
        .filter(|record| record.is_ok() && !record.navigation_changed())
        .count();
    let redirects = snapshot
        .records
        .values()
        .filter(|record| record.navigation_changed())
        .count();
    let errors = snapshot
        .records
        .values()
        .filter(|record| record.is_error() && !record.navigation_changed())
        .count();
    let files = snapshot
        .records
        .values()
        .filter(|record| !record.is_page())
        .count();
    out.push_str(&format!(
        "  {} pages, {} redirects, {} errors, {} other files, {} could not be loaded\n",
        format_count(pages),
        format_count(redirects),
        format_count(errors),
        format_count(files),
        format_count(snapshot.crawl_failures.len())
    ));
    let discovery = &snapshot.sitemap.discovery;
    out.push_str(&format!(
        "  Sitemap: {} ({} URLs)\n",
        match discovery.status {
            crate::sitemap::SitemapDiscoveryStatus::Complete => "read",
            crate::sitemap::SitemapDiscoveryStatus::Partial => "partly read",
            crate::sitemap::SitemapDiscoveryStatus::Absent => "none found",
            crate::sitemap::SitemapDiscoveryStatus::Invalid => "invalid",
            crate::sitemap::SitemapDiscoveryStatus::Unavailable => "could not be read",
        },
        format_count(discovery.declared_urls.len())
    ));
    if let Some(robots) = snapshot.start_robots() {
        out.push_str(&format!(
            "  robots.txt: {}\n",
            match (robots.status, &robots.error) {
                (Some(200), _) => "found".to_owned(),
                (Some(404), _) => "none (404), so everything may be crawled".to_owned(),
                (Some(status), _) => format!("returns {status}"),
                (None, Some(error)) => format!("could not be loaded ({error})"),
                (None, None) => "could not be loaded".to_owned(),
            }
        ));
    }
    for note in crawl_notes(snapshot)
        .into_iter()
        .chain(ForeignOrigin::of(snapshot).map(|foreign| foreign.note()))
    {
        out.push_str(&format!("  Note: {note}\n"));
    }
    if saved_to.is_some() {
        out.push_str(&format!(
            "Next: auditroo diff <url> --baseline {}\n",
            saved_to
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        ));
    }
    out
}

fn snapshot_summary_json(snapshot: &Snapshot, path: &Path) -> serde_json::Value {
    serde_json::json!({
        "saved_to": path.display().to_string(),
        "start_url": snapshot.start_url,
        "created_at": crate::render::format_time(snapshot.created_at),
        "urls": snapshot.records.len(),
        "pages": snapshot.pages().filter(|record| record.is_ok() && !record.navigation_changed()).count(),
        "failed": snapshot.crawl_failures.len(),
        "sitemap_urls": snapshot.sitemap.discovery.declared_urls.len(),
        "robots_status": snapshot.start_robots().and_then(|robots| robots.status),
        "notes": crawl_notes(snapshot),
    })
}

fn execute_assert(command: AssertCommand) -> Result<i32, CliError> {
    if !command.dry_run {
        return Err(CliError::usage(
            "custom checks run as part of check, diff and compare. Use `auditroo assert --dry-run` to see which pages of a snapshot they cover",
        ));
    }
    let settings = resolve_settings_without_env_expansion(
        command.config.as_deref(),
        SettingsOverrides::default(),
    )?;
    let snapshot = command.snapshot.as_deref().map(read_snapshot).transpose()?;
    print!(
        "{}",
        crate::assertions::coverage_summary(
            &settings.extractors,
            &settings.assertions,
            snapshot.as_ref()
        )
    );
    Ok(EXIT_OK)
}

fn execute_recipes(command: RecipesCommand) -> Result<i32, CliError> {
    match command.command {
        RecipeCommand::List => print!("{}", render_recipe_list()),
        RecipeCommand::Show { id } => print!("{}", render_recipe(find_recipe(&id)?)),
        RecipeCommand::Copy {
            id,
            config,
            dry_run,
        } => {
            let recipe = find_recipe(&id)?;
            if dry_run {
                print!(
                    "Would append recipe {id} to {}:\n\n{}",
                    config.display(),
                    render_recipe(recipe)
                );
            } else {
                copy_recipe(&id, &config)?;
                println!(
                    "Added recipe {id} to {}. Replace the PLACEHOLDER_ values, then run `auditroo assert --dry-run --config {} --snapshot <snapshot.json>`.",
                    config.display(),
                    config.display()
                );
            }
        }
    }
    Ok(EXIT_OK)
}

fn execute_logs(command: LogsCommand) -> Result<i32, CliError> {
    match command.command {
        LogsSubcommand::Analyze(command) => execute_log_analyze(command),
        LogsSubcommand::Bots(command) => {
            let catalog = resolve_bot_catalog(command.config.as_deref())?;
            if command.json {
                print!("{}", render_json(&catalog));
            } else {
                print!("{}", crate::bots::render_catalog_human(&catalog));
            }
            Ok(EXIT_OK)
        }
    }
}

fn execute_log_analyze(command: LogAnalyzeCommand) -> Result<i32, CliError> {
    let snapshot = read_snapshot(&command.snapshot)?;
    let cli_sources = cli_log_sources(&command, &snapshot)?;
    let gate_override = if command.no_gate {
        Some(false)
    } else if command.gate || command.fail_on.is_some() {
        Some(true)
    } else {
        None
    };
    let settings = resolve_log_settings(
        command.config.as_deref(),
        LogSettingsOverrides {
            cli_sources,
            gate: gate_override,
            fail_on: command.fail_on,
        },
    )?;
    if let Some(out) = &command.out {
        validate_log_output_path(out, &command, &settings.sources)?;
    }
    let report = analyze_log_sources(
        &snapshot,
        &settings.sources,
        &settings.bot_catalog,
        &settings.analysis,
    )?;
    if let Some(out) = &command.out {
        write_log_report(&report, out).map_err(CliError::runtime)?;
    }
    if command.json {
        print!("{}", render_json(&report));
    } else {
        print!("{}", render_log_human(&report));
    }
    Ok(match report.gate.gate_decision {
        Some(crate::findings::GateDecision::Failed) => crate::error::EXIT_GATE_FAILED,
        _ => EXIT_OK,
    })
}

fn cli_log_sources(
    command: &LogAnalyzeCommand,
    snapshot: &Snapshot,
) -> Result<Option<LogCliSources>, CliError> {
    if command.logs.is_empty() {
        if command.log_format.is_some()
            || command.log_origin.is_some()
            || !command.field_map.is_empty()
        {
            return Err(CliError::usage(
                "--log-format, --log-origin and --field-map need at least one --logs path",
            ));
        }
        return Ok(None);
    }

    let format = command.log_format.ok_or_else(|| {
        CliError::usage("--log-format is required with --logs (common, combined, cloudflare_jsonl, cloudflare_csv or generic_jsonl)")
    })?;
    let field_map = if command.field_map.is_empty() {
        None
    } else {
        Some(parse_cli_field_map(&command.field_map)?)
    };
    if format == LogFormat::GenericJsonl && field_map.is_none() {
        return Err(CliError::usage(
            "--log-format generic_jsonl needs --field-map entries, for example --field-map timestamp=ts",
        ));
    }
    if format != LogFormat::GenericJsonl && field_map.is_some() {
        return Err(CliError::usage(
            "--field-map only works with --log-format generic_jsonl",
        ));
    }

    let origin = match &command.log_origin {
        Some(origin) => origin.clone(),
        None => {
            let start = Url::parse(&snapshot.start_url).map_err(|error| {
                CliError::usage(format!("the snapshot start URL is invalid: {error}"))
            })?;
            origin_from_url(&start)
        }
    };

    Ok(Some(LogCliSources {
        paths: command.logs.clone(),
        format,
        origin,
        field_map,
    }))
}

fn validate_log_output_path(
    out: &Path,
    command: &LogAnalyzeCommand,
    sources: &crate::logs::ResolvedLogSources,
) -> Result<(), CliError> {
    let same = |left: &Path, right: &Path| -> Result<bool, CliError> {
        let left = canonical_or_display_path(left).map_err(CliError::usage)?;
        let right = canonical_or_display_path(right).map_err(CliError::usage)?;
        Ok(left == right)
    };
    if same(out, &command.snapshot)? {
        return Err(CliError::usage("--out must not be the --snapshot file"));
    }
    let config = command.config.clone().or_else(|| {
        Path::new("auditroo.toml")
            .exists()
            .then(|| PathBuf::from("auditroo.toml"))
    });
    if let Some(config) = config {
        if same(out, &config)? {
            return Err(CliError::usage("--out must not be the config file"));
        }
    }
    if source_path_collides_with(out, sources) {
        return Err(CliError::usage("--out must not be one of the log files"));
    }
    Ok(())
}

async fn execute_monitor(command: MonitorCommand) -> Result<i32, CliError> {
    match command.command {
        MonitorSubcommand::Run(command) => {
            let output = run_monitor(command.config.as_deref(), &command.site).await?;
            let exit_code = aggregate_exit_code(&output);
            if command.json {
                print!("{}", render_json_output(&output)?);
            } else {
                print!("{}", render_monitor_human(&output));
            }
            Ok(exit_code)
        }
    }
}

fn execute_history(command: HistoryCommand) -> Result<i32, CliError> {
    match command.command {
        HistorySubcommand::List(command) => {
            let output = history_list(command.config.as_deref(), &command.site, command.limit)?;
            if command.json {
                print!("{}", render_json_output(&output)?);
            } else {
                print!("{}", render_history_list_human(&output));
            }
        }
        HistorySubcommand::Show(command) => {
            let run = history_show(command.config.as_deref(), &command.run_id)?;
            if command.json {
                print!("{}", render_json_output(&run)?);
            } else {
                print!("{}", crate::monitor::render_history_show_human(&run));
            }
        }
    }
    Ok(EXIT_OK)
}

fn execute_baseline(command: BaselineCommand) -> Result<i32, CliError> {
    match command.command {
        BaselineSubcommand::Mark(command) => {
            let output = baseline_mark(
                command.config.as_deref(),
                &command.site,
                &command.run_selector,
                &command.label,
            )?;
            if command.json {
                print!("{}", render_json_output(&output)?);
            } else {
                println!(
                    "Baseline {} of site {} now points at {}{}",
                    output.label,
                    output.site_id,
                    output.descriptor.relative_path,
                    output
                        .run_id
                        .as_deref()
                        .map(|run_id| format!(" (run {run_id})"))
                        .unwrap_or_default()
                );
            }
        }
        BaselineSubcommand::Import(command) => {
            let output = baseline_import(
                command.config.as_deref(),
                &command.site,
                &command.snapshot_path,
                &command.label,
            )?;
            if command.json {
                print!("{}", render_json_output(&output)?);
            } else {
                println!(
                    "Imported baseline {} for site {} as {}",
                    output.label, output.site_id, output.descriptor.relative_path
                );
            }
        }
    }
    Ok(EXIT_OK)
}

fn execute_bundle(command: BundleCommand) -> Result<i32, CliError> {
    let display_path = command.out.display().to_string();
    if command.out.exists() {
        return Err(CliError::usage(format!(
            "{display_path} already exists. Choose a new folder for the bundle"
        )));
    }
    let output = bundle_from_history(
        command.config.as_deref(),
        &command.site,
        &command.run_selector,
        &command.out,
        display_path.clone(),
    )?;
    if command.json {
        print!("{}", render_json_output(&output)?);
    } else {
        println!(
            "Wrote bundle {} for run {} to {} ({} files plus manifest.json). Open {}/index.html to see the report.",
            output.bundle_id,
            output.run_id,
            display_path,
            output.files.len(),
            display_path.trim_end_matches('/')
        );
    }
    Ok(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_definitions_are_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn start_url_without_scheme_suggests_one() {
        let error = parse_start_url("example.com").unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("https://example.com/"));
    }
}
