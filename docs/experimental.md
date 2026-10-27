# Experimental features

The features on this page work and have tests, but their commands and
config keys may still change before version 1.0. The pull request check
described in the rest of the docs does not need any of them.

## Custom checks

Custom checks let you test things that are specific to your site. An
`[[extract]]` block reads a value from each page. An `[[assert]]` block
says what that value must look like. A failed assertion is a finding with
the rule id `assertion` and the severity you choose.

This example checks that each product page has a Product item in its
JSON-LD, with a price and an availability. JSON-LD is the structured data
that Google reads for rich results, such as product prices.

```toml
[[extract]]
id = "product-nodes"
kind = "json_ld"
include = ["/products/*"]
schema_type = "Product"

[[assert]]
id = "product-offer-shape"
extractor = "product-nodes"
include = ["/products/*"]
operator = "json_shape"
required = [
  { pointer = "/offers/price" },
  { pointer = "/offers/availability", type = "string" },
]
severity = "high"
```

auditroo comes with a few ready-made sets of checks, called recipes. Use
these commands to see them and to copy one into your config:

```sh
auditroo recipes list
auditroo recipes show product-pages
auditroo recipes copy product-pages --config auditroo.toml
```

Some recipes contain `PLACEHOLDER_` values, such as your production host.
Replace them before you run any command with that config. auditroo stops
with exit code 2 while one is left.

To see which pages your checks would cover, run them against a saved
snapshot. This does not crawl:

```sh
auditroo snapshot https://www.example.com --out snapshot.json
auditroo assert --dry-run --config auditroo.toml --snapshot snapshot.json
```

To accept a failing check on some pages, ignore it by its id:

```toml
[gate]
ignore = [
  { assertion = "product-offer-shape", url = "/products/legacy-*" },
]
```

Extracted values are stored in snapshots and reports. Do not extract
anything you would not want in a CI artifact.

## Access log analysis

`auditroo logs analyze` reads your server or CDN access logs and shows which
search engines and AI crawlers fetched which pages. It matches the log lines
with a snapshot, so it can tell which requests reached real pages and which
ones got a redirect or an error.

```sh
auditroo snapshot https://www.example.com --out snapshot.json
auditroo logs analyze --snapshot snapshot.json \
  --logs 'logs/*.jsonl' --log-format cloudflare_jsonl \
  --log-origin https://www.example.com
```

The supported formats are:

| Format | Source |
| --- | --- |
| `common` | Common Log Format |
| `combined` | Combined Log Format, as nginx and Apache write it |
| `cloudflare_jsonl` | Cloudflare Logpush, one JSON object per line |
| `cloudflare_csv` | Cloudflare logs exported as CSV |
| `generic_jsonl` | Any JSON lines; you map the fields with `--field-map` |

`auditroo logs bots` lists the crawlers auditroo recognizes and the text it
looks for in the User-Agent. A User-Agent can be faked, so treat the
result as what clients claim to be.

Log sources can also live in `auditroo.toml`:

```toml
[[logs.source]]
id = "edge"
path = "logs/*.jsonl"
format = "cloudflare_jsonl"
origin = "https://www.example.com"

[[logs.group]]
id = "guides"
prefix = "/guides"
```

Log reports contain samples of User-Agent strings and requested URLs. Treat
them as private.

## Scheduled monitoring

`auditroo monitor run` is for people who look after several sites, such as
agencies. It crawls each site from the config and compares it with that
site's baseline. It saves each run in a local SQLite database. Run it from
cron or a scheduled CI job.

```toml
[history]
database = ".auditroo/history.sqlite3"
artifact_root = ".auditroo/runs"
baseline_root = ".auditroo/baselines"

[[site]]
id = "client-a"
start_url = "https://client-a.example/"
baseline = { mode = "label", label = "last-known-good", bootstrap_path = "baseline.json" }
```

```sh
auditroo monitor run --site all
auditroo history list --site client-a
auditroo history show <run-id>
```

With `mode = "label"`, the site is compared with the snapshot that has the
label. The first run uses `bootstrap_path` until the label exists. A run
that passes moves the label to its own snapshot. To set the label by hand:

```sh
auditroo baseline mark latest --site client-a --label last-known-good
auditroo baseline import baseline.json --site client-a --label last-known-good
```

With `mode = "fixed"` and `path = "baseline.json"`, every run compares with
the same snapshot.

`auditroo bundle` writes one run into a folder you can send to a client:

```sh
auditroo bundle latest --site client-a --out client-a-report/
```

The folder has these files:

- the HTML report as `index.html`
- the JSON report and the snapshot
- `findings.csv`
- `manifest.json` with checksums of the other files

### Notifications

A `[[notify]]` block runs a local command or calls a webhook after a run.
The command gets a short JSON summary on stdin. It runs without a shell.

```toml
[[notify]]
id = "ops"
kind = "command"
triggers = ["gate_failed", "operational_error"]
argv = ["./scripts/notify"]

[[site]]
id = "client-a"
start_url = "https://client-a.example/"
baseline = { mode = "label", label = "last-known-good", bootstrap_path = "baseline.json" }
notify = ["ops"]
```

A webhook uses `kind = "webhook"` and a `url`. When the webhook needs a
token, `secret_env` names the environment variable that holds it.

## Quality budgets

auditroo can read results from quality tools such as Lighthouse and Pa11y,
and compare them with budgets you set. Point it at a few template pages,
such as your pricing page:

```toml
[quality]
enabled = true
gate = true

[[quality.template]]
id = "pricing"
url = "https://www.example.com/pricing"

[[quality.source]]
id = "pricing-lighthouse"
template = "pricing"
kind = "lighthouse_json"
path = "quality/lighthouse-pricing.json"

[[quality.budget]]
id = "perf-score"
source = "pricing-lighthouse"
metric = "performance.score"
min = 0.85
severity = "high"
```

A source either reads a JSON file that you made earlier, or runs the tool
itself. These are the `kind` values:

| Tool | Read a JSON file | Run the tool |
| --- | --- | --- |
| Lighthouse | `lighthouse_json` | `lighthouse_command` |
| PageSpeed Insights | `pagespeed_json` | |
| Pa11y | `pa11y_json` | `pa11y_command` |
| Nu HTML Checker | `nu_html_json` | `nu_html_command` |

A command source runs `argv`, with `{template_url}` standing for the page
URL:

```toml
[[quality.source]]
id = "pricing-pa11y"
template = "pricing"
kind = "pa11y_command"
required = false
argv = ["pa11y", "--reporter", "json", "{template_url}"]
accepted_exit_codes = [0, 2]
timeout_seconds = 120
```

When a source cannot be read, the run stops with an error. With
`required = false`, auditroo notes the problem instead and does not check
that source's budgets. A budget that is not met becomes a `quality_budget`
finding. With `gate = false`, those findings are only information and never
fail a run.

## Traffic data

auditroo can read a Search Console or analytics export and show the traffic
of each URL next to its findings. That helps you fix the pages that matter
most first. The numbers never change whether a run fails.

```toml
[impact]
enabled = true
primary = { source = "gsc", metric = "impressions" }

[[impact.source]]
id = "gsc"
kind = "search_console_csv"
path = "exports/search-console-pages.csv"
period_start = "2026-07-01"
period_end = "2026-07-31"
```

For an analytics export, use `kind = "analytics_csv"`. It needs an `origin`
and a `field_map` that names the columns:

```toml
[[impact.source]]
id = "analytics"
kind = "analytics_csv"
path = "exports/analytics-pages.csv"
origin = "https://www.example.com"
field_map = { url = "Landing page", sessions = "Sessions", conversions = "Conversions" }
```

These exports can contain revenue and traffic numbers. Reports with this
data are as private as the exports themselves.
