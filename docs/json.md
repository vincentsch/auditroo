# The JSON report

`check`, `compare` and `diff` can write their full result as JSON. Read it
when a script or a coding agent needs the result, for example the rule id
of each finding.

```sh
auditroo compare https://www.example.com "$PREVIEW_URL" --out report.json
auditroo compare https://www.example.com "$PREVIEW_URL" --json > report.json
```

`--out` writes the report to a file and still prints the text output.
`--json` prints the report on stdout instead of the text. The GitHub Action
always writes the report, and its `report` output names the file.
`auditroo report report.json` prints a saved report again, as text or in
another format.

`passed` in the report matches the exit code: `true` means exit code 0 and
`false` means exit code 1. When auditroo cannot finish, with exit code 2 or
3, it writes no report.

## An example

This is the report of `auditroo demo --json`, with three of its eight
findings:

```json
{
  "artifact_kind": "report",
  "schema_version": 1,
  "auditroo_version": "0.1.0",
  "command": "compare",
  "created_at": "2026-11-09T10:44:35Z",
  "current": {
    "start_url": "http://127.0.0.1:39089/",
    "crawled_at": "2026-11-09T10:44:34Z",
    "pages": 8,
    "urls": 10,
    "failed": 0,
    "truncated": false
  },
  "baseline": {
    "start_url": "http://127.0.0.1:43413/",
    "crawled_at": "2026-11-09T10:44:34Z",
    "pages": 9,
    "urls": 9,
    "failed": 0,
    "truncated": false
  },
  "fail_on": "high",
  "passed": false,
  "incomplete": false,
  "counts": {
    "critical": 4,
    "high": 3,
    "medium": 1,
    "low": 0,
    "info": 0,
    "ignored": 0
  },
  "findings": [
    {
      "rule": "noindex_added",
      "severity": "critical",
      "url": "http://127.0.0.1:39089/coffee/ethiopia-yirgacheffe/",
      "message": "now has noindex (robots meta tag \"noindex\")",
      "ignored": false
    },
    {
      "rule": "robots_blocked",
      "severity": "critical",
      "url": "http://127.0.0.1:39089/robots.txt",
      "message": "now blocks Googlebot and 9 other crawlers from 1 page that they could crawl before",
      "related": [
        "http://127.0.0.1:39089/brewing-guide/"
      ],
      "related_total": 1,
      "group": "Googlebot and 9 other crawlers",
      "details": [
        "crawlers: Googlebot, Bingbot, OAI-SearchBot (ChatGPT search), ChatGPT-User, PerplexityBot, Claude-SearchBot, GPTBot (OpenAI training), ClaudeBot (Anthropic training), Google-Extended (Gemini training), CCBot (Common Crawl)"
      ],
      "ignored": false
    },
    {
      "rule": "title_changed",
      "severity": "medium",
      "url": "http://127.0.0.1:39089/contact/",
      "message": "has a new title",
      "before": "\"Contact | Harbor Coffee Roasters\"",
      "after": "\"Contact us | Harbor Coffee Roasters\"",
      "group": "\"Contact us | Harbor Coffee Roasters\"",
      "ignored": false
    }
  ]
}
```

auditroo leaves out fields that are empty or 0. Read them with a default,
for example `.notes // []` in jq. `passed` and `incomplete` are always
there, as `true` or `false`. So is `ignored` on every finding.

## The summary fields

| Field | Meaning |
| --- | --- |
| `artifact_kind` | Always `"report"`. A snapshot file has `"snapshot"` here. |
| `schema_version` | The version of this layout, now `1`. `auditroo report` refuses a report with another version. |
| `auditroo_version` | The auditroo version that wrote the report. |
| `command` | The command that made the report: `check`, `compare` or `diff`. |
| `created_at` | When the report was made, in UTC. |
| `current` | The crawl that was checked. In `compare` that is the preview. The next section lists its fields. |
| `baseline` | The crawl it was compared with. In `compare` that is production, and in `diff` the snapshot. Missing for `check`. |
| `fail_on` | The severity that fails the run, from `--fail-on` or `auditroo.toml`. |
| `passed` | `true` when no finding reached `fail_on`. Ignored findings do not count. |
| `incomplete` | `true` when auditroo knows it did not check everything, so a pass does not mean the whole site is fine. The next sections say when. |
| `counts` | The number of findings per severity, such as `"high": 3`. `ignored` counts the findings that matched an ignore rule. They are not counted under their severity. |
| `notes` | Plain sentences about what auditroo could not check, or about what it did for you, such as ignoring a preview's noindex header. |
| `not_compared` | The number of indexable baseline pages that the current crawl did not reach, for example because of `--max-pages`. |
| `suggested_production_url` | In `compare` and `diff`: the host that most canonical tags or sitemap URLs on both sites name, when it is neither of the two sites. auditroo then counts most pages as not indexable, so most rules have nothing to check. Run again with this URL as `--production-url`. |
| `findings` | Every finding, the ignored ones included. The section after next lists the fields. |
| `ignore_rules` | The ignore rules of this run, from `auditroo.toml` and `--ignore`. |
| `quality` | Results of quality budgets, an [experimental feature](experimental.md). |
| `impact` | Traffic numbers from Search Console or analytics exports, an [experimental feature](experimental.md). |

## The crawl fields

`current` and `baseline` have the same fields.

| Field | Meaning |
| --- | --- |
| `start_url` | Where the crawl started. |
| `crawled_at` | When the crawl ran, in UTC. In `diff` the baseline time is when you took the snapshot. |
| `pages` | HTML pages that loaded without a redirect. |
| `urls` | All URLs that answered. This also counts redirects and files such as images. |
| `failed` | URLs that could not be fetched at all, for example because of a timeout. |
| `truncated` | `true` when the crawl stopped at `--max-pages` or `--max-depth`. |
| `robots_denied` | URLs that robots.txt blocks for auditroo, so the crawl did not fetch them. |

In `compare` and `diff`, auditroo fetches a preview page that production's
robots.txt allows, even when the preview's new robots.txt blocks it. Such a
page is not in `robots_denied`. There the list only has URLs that both
files block, and URLs that a new rule for auditroo itself blocks.

## When the result is incomplete

A run can pass although auditroo did not see every page, or although most
rules had nothing to check. The text output then starts with
`PASS (incomplete)` or `FAIL (incomplete)`. A line above the findings says
"This result is incomplete", and the notes say why. In the JSON report,
`incomplete` is `true`. It is `true` when any of these is the case:

- `current.robots_denied` is not empty.
- `current.truncated` is `true`.
- `not_compared` is greater than 0.
- `suggested_production_url` is set.

```sh
jq '.incomplete' report.json
```

## The finding fields

| Field | Meaning |
| --- | --- |
| `rule` | The rule id, such as `noindex_added`. [Findings](findings.md) explains each rule. Ignore rules match it. |
| `severity` | `critical`, `high`, `medium`, `low` or `info`. |
| `url` | The page or file the finding is about. For `robots_blocked` and `robots_error` it is the robots.txt file. Missing for `sitemap_urls_removed` and `sitemap_missing`, which are about the sitemap as a whole. |
| `message` | One sentence for people. When there is a `url`, the sentence goes after it, as in "/about/ now returns 404". |
| `before` | The old value, as text for people, for example the old title. |
| `after` | The new value, as text for people. |
| `related` | Other URLs that belong to the finding, at most 50. For a broken page these are the pages that link to it. For `robots_blocked` they are the pages the new rule blocks. |
| `related_total` | How many related URLs there are, also past the first 50. |
| `group` | A detail that findings share when the output sums them up, such as the host a canonical tag now points to. |
| `details` | More lines for people, such as the crawlers that a robots.txt rule blocks. |
| `follow_on` | Rule ids of other changes on the same page that come from this finding. A page that now returns an error also loses its title, for example. Those changes are not listed as findings of their own. |
| `check_id` | For the rules `assertion` and `quality_budget`, the id of the custom check or budget in `auditroo.toml`. Ignore rules match it as `assertion`. |
| `ignored` | `true` when an ignore rule matched the finding, `false` otherwise. Every finding has it. |
| `impact` | Traffic numbers for the URL, from the experimental impact feature. |

The findings are sorted. Findings that are not ignored come first, and
within each part the most severe come first.

## Reading the report with jq

```sh
# Did the run pass?
jq '.passed' report.json

# Severity, rule and URL of each finding that is not ignored
jq -r '.findings[] | select(.ignored | not) | [.severity, .rule, .url // "-"] | @tsv' report.json

# The URLs that robots.txt kept auditroo from
jq -r '.current.robots_denied // [] | .[]' report.json
```
