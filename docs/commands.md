# Command reference

<!-- Generated from auditroo --help. Update with: AUDITROO_UPDATE_DOCS=1 cargo test --test docs -->

Most people need three commands:

- `auditroo check <url>` looks at one site.
- `auditroo compare <production> <preview>` compares a preview with production.
- `auditroo demo` shows what a failing run looks like.

The [exit codes](#exit-codes) are the same for every command.

## auditroo check

Crawl one site and report its SEO problems.

```text
auditroo check [OPTIONS] <URL>
```

| Option | What it does |
| --- | --- |
| `<URL>` | Start URL, for example https://example.com. |
| `--ignore-noindex-header` | Ignore noindex in X-Robots-Tag headers. Preview hosts such as Vercel and Netlify add one to every preview, so compare and diff ignore it on their preview domains without this flag. Robots meta tags are still checked. |
| `--fail-on <SEVERITY>` | Exit with code 1 when a finding has this severity or higher: critical, high, medium, low or info [default: high, or [gate] fail_on] |
| `--ignore <RULE>` | Do not count findings that match, written as a TOML table, for example `{ rule = "title_changed" }` or `{ url = "/promo/*" }`. Repeat for more. Adds to [gate] ignore in auditroo.toml. |
| `--format <FORMAT>` | Output format on stdout [default: human] Values: `human` `json` `markdown` `github`. |
| `--json` | Same as --format json. |
| `--html <FILE>` | Also write a standalone HTML report to this file. |
| `--markdown <FILE>` | Also write the report as Markdown to this file. |
| `--junit <FILE>` | Also write the result as JUnit XML to this file. |
| `--out <FILE>` | Also write the full report as JSON to this file. |
| `-q, --quiet` | Do not print progress on stderr. |
| `--config <FILE>` | Read settings from this file [default: ./auditroo.toml when it exists] |
| `--max-pages <N>` | Stop after this many URLs; 0 means no limit [default: 10000] |
| `--max-depth <N>` | Follow links at most this many clicks from the start page; 0 means no limit [default: 0] |
| `--concurrency <N>` | Fetch this many URLs at the same time [default: 8] |
| `--rate-limit-ms <MS>` | Wait at least this long between two requests to one host. With the default, a crawl does about 10 pages a second. compare also uses it for production, so use 0 only when both sites can take the load [default: 100] |
| `--timeout-ms <MS>` | Give up on one request after this long [default: 10000] |
| `--max-body-bytes <BYTES>` | Treat larger responses as failed [default: 10485760] |
| `--include <PATTERN>` | Only crawl paths that match, for example `/docs/*`. Repeat for more. |
| `--exclude <PATTERN>` | Do not crawl paths that match, for example `/search?*`. Repeat for more. |
| `--user-agent <TEXT>` | User-Agent header [default: auditroo/&lt;version&gt; (+https://github.com/vincentsch/auditroo)] |
| `--header <NAME: VALUE>` | Send this header to the site, for example a firewall or preview bypass token. auditroo fills in ${NAME} from the environment. The header only goes to the start URL's host. Repeat for more. |
| `--ignore-robots` | Crawl pages that robots.txt blocks for auditroo. Use it only on your own sites. In compare it applies to production too. A preview does not need it when only its new robots.txt blocks a page, because compare and diff crawl every page that production or the baseline allowed. robots.txt is still checked for Googlebot. |
| `--render-js` | Render pages in headless Chrome before reading them. Needs a build with the browser feature. |
| `--sitemap <URL>` | Read this sitemap instead of the ones robots.txt names. Repeat for more. |
| `--production-url <URL>` | The live site, when the crawled URL is a preview or a local build of it. Production URLs, for example in canonical tags, then count as the build's own and are read on the build under the same path. Overrides [crawl] production_url. |

```text
Examples:
  auditroo check https://example.com
  auditroo check https://example.com --fail-on medium --html report.html
  auditroo check https://example.com --max-pages 500 --format markdown

check treats the URL as your live site. If you check a preview or a local
build whose canonical tags and sitemap still use the live domain, pass
--production-url with the live site, or compare the build with production.

docs/findings.md explains every finding and what to do about it.
```

## auditroo compare

Crawl production and a preview, and report what got worse on the preview.

```text
auditroo compare [OPTIONS] <PRODUCTION_URL> <PREVIEW_URL>
```

| Option | What it does |
| --- | --- |
| `<PRODUCTION_URL>` | The live site, for example https://example.com. |
| `<PREVIEW_URL>` | The preview or staging deployment to check. |
| `--ignore-noindex-header` | Ignore noindex in X-Robots-Tag headers. Preview hosts such as Vercel and Netlify add one to every preview, so compare and diff ignore it on their preview domains without this flag. Robots meta tags are still checked. |
| `--fail-on <SEVERITY>` | Exit with code 1 when a finding has this severity or higher: critical, high, medium, low or info [default: high, or [gate] fail_on] |
| `--ignore <RULE>` | Do not count findings that match, written as a TOML table, for example `{ rule = "title_changed" }` or `{ url = "/promo/*" }`. Repeat for more. Adds to [gate] ignore in auditroo.toml. |
| `--format <FORMAT>` | Output format on stdout [default: human] Values: `human` `json` `markdown` `github`. |
| `--json` | Same as --format json. |
| `--html <FILE>` | Also write a standalone HTML report to this file. |
| `--markdown <FILE>` | Also write the report as Markdown to this file. |
| `--junit <FILE>` | Also write the result as JUnit XML to this file. |
| `--out <FILE>` | Also write the full report as JSON to this file. |
| `-q, --quiet` | Do not print progress on stderr. |
| `--config <FILE>` | Read settings from this file [default: ./auditroo.toml when it exists] |
| `--max-pages <N>` | Stop after this many URLs; 0 means no limit [default: 10000] |
| `--max-depth <N>` | Follow links at most this many clicks from the start page; 0 means no limit [default: 0] |
| `--concurrency <N>` | Fetch this many URLs at the same time [default: 8] |
| `--rate-limit-ms <MS>` | Wait at least this long between two requests to one host. With the default, a crawl does about 10 pages a second. compare also uses it for production, so use 0 only when both sites can take the load [default: 100] |
| `--timeout-ms <MS>` | Give up on one request after this long [default: 10000] |
| `--max-body-bytes <BYTES>` | Treat larger responses as failed [default: 10485760] |
| `--include <PATTERN>` | Only crawl paths that match, for example `/docs/*`. Repeat for more. |
| `--exclude <PATTERN>` | Do not crawl paths that match, for example `/search?*`. Repeat for more. |
| `--user-agent <TEXT>` | User-Agent header [default: auditroo/&lt;version&gt; (+https://github.com/vincentsch/auditroo)] |
| `--header <NAME: VALUE>` | Send this header to the site, for example a firewall or preview bypass token. auditroo fills in ${NAME} from the environment. The header only goes to the start URL's host. Repeat for more. |
| `--ignore-robots` | Crawl pages that robots.txt blocks for auditroo. Use it only on your own sites. In compare it applies to production too. A preview does not need it when only its new robots.txt blocks a page, because compare and diff crawl every page that production or the baseline allowed. robots.txt is still checked for Googlebot. |
| `--render-js` | Render pages in headless Chrome before reading them. Needs a build with the browser feature. |
| `--sitemap <URL>` | Read this sitemap instead of the ones robots.txt names. Repeat for more. |
| `--production-url <URL>` | The live site, when the crawled URL is a preview or a local build of it. Production URLs, for example in canonical tags, then count as the build's own and are read on the build under the same path. Overrides [crawl] production_url. |

```text
auditroo crawls both sites with the same settings and matches pages by path,
so the preview can be on any host. It also fetches each production page on
the preview, so removed pages show up even when nothing links to them. When
the preview still uses production URLs, for example in canonical tags,
auditroo reads them on the preview under the same path. URLs on any other
host are reported.

When both sites are local copies of your live site, pass the live site with
--production-url, as in the last example. Otherwise most pages do not count
as indexable. The first line then says (incomplete), and a line above the
findings names the flag.

When the preview's robots.txt blocks pages that production's robots.txt
allows, auditroo reports the block and still checks those pages on the
preview. You do not need --ignore-robots for that.

Examples:
  auditroo compare https://example.com https://my-branch.example.vercel.app
  auditroo compare https://example.com http://localhost:4321
  auditroo compare https://example.com "$PREVIEW_URL" --header 'x-vercel-protection-bypass: ${BYPASS}'
  auditroo compare http://localhost:4000 http://localhost:4001 --production-url https://example.com

The --header value is in single quotes, so the shell does not expand
${BYPASS}. auditroo reads it from the environment itself, and the secret
does not show up in the process list.

docs/ci.md has recipes for GitHub Actions and other CI systems.
```

## auditroo diff

Crawl a site and compare it with a snapshot saved earlier.

```text
auditroo diff [OPTIONS] --baseline <FILE> <URL>
```

| Option | What it does |
| --- | --- |
| `<URL>` | Start URL of the site to check. |
| `--baseline <FILE>` | Snapshot to compare with, written by `auditroo snapshot`. |
| `--ignore-noindex-header` | Ignore noindex in X-Robots-Tag headers. Preview hosts such as Vercel and Netlify add one to every preview, so compare and diff ignore it on their preview domains without this flag. Robots meta tags are still checked. |
| `--fail-on <SEVERITY>` | Exit with code 1 when a finding has this severity or higher: critical, high, medium, low or info [default: high, or [gate] fail_on] |
| `--ignore <RULE>` | Do not count findings that match, written as a TOML table, for example `{ rule = "title_changed" }` or `{ url = "/promo/*" }`. Repeat for more. Adds to [gate] ignore in auditroo.toml. |
| `--format <FORMAT>` | Output format on stdout [default: human] Values: `human` `json` `markdown` `github`. |
| `--json` | Same as --format json. |
| `--html <FILE>` | Also write a standalone HTML report to this file. |
| `--markdown <FILE>` | Also write the report as Markdown to this file. |
| `--junit <FILE>` | Also write the result as JUnit XML to this file. |
| `--out <FILE>` | Also write the full report as JSON to this file. |
| `-q, --quiet` | Do not print progress on stderr. |
| `--config <FILE>` | Read settings from this file [default: ./auditroo.toml when it exists] |
| `--max-pages <N>` | Stop after this many URLs; 0 means no limit [default: 10000] |
| `--max-depth <N>` | Follow links at most this many clicks from the start page; 0 means no limit [default: 0] |
| `--concurrency <N>` | Fetch this many URLs at the same time [default: 8] |
| `--rate-limit-ms <MS>` | Wait at least this long between two requests to one host. With the default, a crawl does about 10 pages a second. compare also uses it for production, so use 0 only when both sites can take the load [default: 100] |
| `--timeout-ms <MS>` | Give up on one request after this long [default: 10000] |
| `--max-body-bytes <BYTES>` | Treat larger responses as failed [default: 10485760] |
| `--include <PATTERN>` | Only crawl paths that match, for example `/docs/*`. Repeat for more. |
| `--exclude <PATTERN>` | Do not crawl paths that match, for example `/search?*`. Repeat for more. |
| `--user-agent <TEXT>` | User-Agent header [default: auditroo/&lt;version&gt; (+https://github.com/vincentsch/auditroo)] |
| `--header <NAME: VALUE>` | Send this header to the site, for example a firewall or preview bypass token. auditroo fills in ${NAME} from the environment. The header only goes to the start URL's host. Repeat for more. |
| `--ignore-robots` | Crawl pages that robots.txt blocks for auditroo. Use it only on your own sites. In compare it applies to production too. A preview does not need it when only its new robots.txt blocks a page, because compare and diff crawl every page that production or the baseline allowed. robots.txt is still checked for Googlebot. |
| `--render-js` | Render pages in headless Chrome before reading them. Needs a build with the browser feature. |
| `--sitemap <URL>` | Read this sitemap instead of the ones robots.txt names. Repeat for more. |
| `--production-url <URL>` | The live site, when the crawled URL is a preview or a local build of it. Production URLs, for example in canonical tags, then count as the build's own and are read on the build under the same path. Overrides [crawl] production_url. |

```text
Save the baseline with `auditroo snapshot` while the site works as it should.
Then run diff after a change.

Examples:
  auditroo snapshot https://example.com --out baseline.json
  auditroo diff https://staging.example.com --baseline baseline.json
  auditroo diff https://example.com --baseline baseline.json --json --out report.json
```

## auditroo snapshot

Crawl a site and save what search engines see to a JSON file.

```text
auditroo snapshot [OPTIONS] --out <FILE> <URL>
```

| Option | What it does |
| --- | --- |
| `<URL>` | Start URL, for example https://example.com. |
| `--out <FILE>` | Where to write the snapshot. |
| `--json` | Print a JSON summary instead of text. |
| `-q, --quiet` | Do not print progress on stderr. |
| `--config <FILE>` | Read settings from this file [default: ./auditroo.toml when it exists] |
| `--max-pages <N>` | Stop after this many URLs; 0 means no limit [default: 10000] |
| `--max-depth <N>` | Follow links at most this many clicks from the start page; 0 means no limit [default: 0] |
| `--concurrency <N>` | Fetch this many URLs at the same time [default: 8] |
| `--rate-limit-ms <MS>` | Wait at least this long between two requests to one host. With the default, a crawl does about 10 pages a second. compare also uses it for production, so use 0 only when both sites can take the load [default: 100] |
| `--timeout-ms <MS>` | Give up on one request after this long [default: 10000] |
| `--max-body-bytes <BYTES>` | Treat larger responses as failed [default: 10485760] |
| `--include <PATTERN>` | Only crawl paths that match, for example `/docs/*`. Repeat for more. |
| `--exclude <PATTERN>` | Do not crawl paths that match, for example `/search?*`. Repeat for more. |
| `--user-agent <TEXT>` | User-Agent header [default: auditroo/&lt;version&gt; (+https://github.com/vincentsch/auditroo)] |
| `--header <NAME: VALUE>` | Send this header to the site, for example a firewall or preview bypass token. auditroo fills in ${NAME} from the environment. The header only goes to the start URL's host. Repeat for more. |
| `--ignore-robots` | Crawl pages that robots.txt blocks for auditroo. Use it only on your own sites. In compare it applies to production too. A preview does not need it when only its new robots.txt blocks a page, because compare and diff crawl every page that production or the baseline allowed. robots.txt is still checked for Googlebot. |
| `--render-js` | Render pages in headless Chrome before reading them. Needs a build with the browser feature. |
| `--sitemap <URL>` | Read this sitemap instead of the ones robots.txt names. Repeat for more. |
| `--production-url <URL>` | The live site, when the crawled URL is a preview or a local build of it. Production URLs, for example in canonical tags, then count as the build's own and are read on the build under the same path. Overrides [crawl] production_url. |

```text
Examples:
  auditroo snapshot https://example.com --out baseline.json
  auditroo snapshot https://example.com --out baseline.json --max-pages 500

Next: auditroo diff <url> --baseline <file>
```

## auditroo report

Show a saved report or snapshot again, in any output format.

```text
auditroo report [OPTIONS] <PATH>
```

| Option | What it does |
| --- | --- |
| `<PATH>` | A report, snapshot or log analysis JSON file. |
| `--format <FORMAT>` | Output format on stdout [default: human] Values: `human` `json` `markdown` `github`. |
| `--json` | Same as --format json. |
| `--html <FILE>` | Also write a standalone HTML report to this file. |
| `--markdown <FILE>` | Also write the report as Markdown to this file. |
| `--junit <FILE>` | Also write the result as JUnit XML to this file. |

```text
Reads a report written by check, compare or diff (--out or --json), a snapshot,
or a log analysis report.

Examples:
  auditroo report report.json
  auditroo report report.json --format markdown >> "$GITHUB_STEP_SUMMARY"
  auditroo report report.json --html report.html --junit auditroo.xml
```

## auditroo demo

Compare a small built-in site with a broken copy of it. No network needed.

```text
auditroo demo [OPTIONS]
```

| Option | What it does |
| --- | --- |
| `--format <FORMAT>` | Output format on stdout [default: human] Values: `human` `json` `markdown` `github`. |
| `--json` | Same as --format json. |
| `--html <FILE>` | Also write a standalone HTML report to this file. |
| `--markdown <FILE>` | Also write the report as Markdown to this file. |
| `--junit <FILE>` | Also write the result as JUnit XML to this file. |
| `--out <FILE>` | Also write the full report as JSON to this file. |
| `-q, --quiet` | Do not print progress on stderr. |

```text
The demo starts a small web server on 127.0.0.1. It serves a sample site and
a copy of it with typical deploy mistakes, and compares the two. Nothing
leaves your computer.

Examples:
  auditroo demo
  auditroo demo --html demo-report.html
```

## auditroo explain

Show what a rule checks and what to do about its findings.

```text
auditroo explain [RULE]
```

| Option | What it does |
| --- | --- |
| `<RULE>` | Rule id, as the output shows it under each finding, for example title_changed. |

```text
The text is the rule's section of docs/findings.md. It ends with an ignore
rule for auditroo.toml that you can paste. Without a rule, explain lists
every rule id.

Examples:
  auditroo explain title_changed
  auditroo explain
```

## auditroo assert

Experimental: show which pages of a snapshot your custom checks in auditroo.toml cover.

```text
auditroo assert [OPTIONS]
```

| Option | What it does |
| --- | --- |
| `--dry-run` | Show which pages your checks cover, without crawling. This is the only mode. |
| `--config <FILE>` | Config file [default: ./auditroo.toml] |
| `--snapshot <FILE>` | Snapshot to check the coverage against. |

## auditroo recipes

Experimental: show ready-made custom checks and copy them into auditroo.toml.

```text
auditroo recipes <COMMAND>
```

## auditroo recipes list

List the built-in recipes.

```text
auditroo recipes list
```

## auditroo recipes show

Print one recipe.

```text
auditroo recipes show <ID>
```

| Option | What it does |
| --- | --- |
| `<ID>` | Recipe id, from `auditroo recipes list`. |

## auditroo recipes copy

Add one recipe to the end of a config file.

```text
auditroo recipes copy [OPTIONS] <ID>
```

| Option | What it does |
| --- | --- |
| `<ID>` | Recipe id, from `auditroo recipes list`. |
| `--config <FILE>` | Config file to add it to. auditroo creates it when it does not exist. [default: auditroo.toml] |
| `--dry-run` | Show what would be written without changing the file. |

## auditroo logs

Experimental: show which search and AI bots read which pages, from access logs.

```text
auditroo logs <COMMAND>
```

## auditroo logs analyze

Read access logs and report which bots fetched which pages.

```text
auditroo logs analyze [OPTIONS] --snapshot <FILE>
```

| Option | What it does |
| --- | --- |
| `--config <FILE>` | Config file [default: ./auditroo.toml] |
| `--snapshot <FILE>` | Snapshot of the site, to know which logged URLs are real pages. |
| `--logs <PATH>` | Log file or glob. Repeat for more. Replaces [[logs.source]] in the config. |
| `--log-format <FORMAT>` | Format of the --logs files. |
| `--log-origin <URL>` | Origin for log lines that only have a path [default: the snapshot's origin] |
| `--field-map <FIELD=KEY>` | For generic_jsonl logs: auditroo-field=json-key. Repeat for each field. |
| `--json` | Print the full analysis as JSON. |
| `--out <FILE>` | Also write the full analysis as JSON to this file. |
| `--gate` | Exit with code 1 when a log finding reaches --fail-on. |
| `--no-gate` | Never fail because of log findings. |
| `--fail-on <SEVERITY>` | Severity that fails the run; implies --gate [default: high] |

## auditroo logs bots

List the bots auditroo recognizes in logs.

```text
auditroo logs bots [OPTIONS]
```

| Option | What it does |
| --- | --- |
| `--config <FILE>` | Config file [default: ./auditroo.toml] |
| `--json` | Print the list as JSON. |

## auditroo monitor

Experimental: check the sites in auditroo.toml on a schedule and save a history of runs.

```text
auditroo monitor <COMMAND>
```

## auditroo monitor run

Crawl one configured site, or all of them, and compare each with its baseline.

```text
auditroo monitor run [OPTIONS] --site <SITE>
```

| Option | What it does |
| --- | --- |
| `--site <SITE>` | Site id from [[site]] in the config, or `all`. |
| `--config <FILE>` | Config file [default: ./auditroo.toml] |
| `--json` | Print the result as JSON. |

## auditroo history

Experimental: list or show earlier monitor runs.

```text
auditroo history <COMMAND>
```

## auditroo history list

List recent runs of a site.

```text
auditroo history list [OPTIONS] --site <SITE>
```

| Option | What it does |
| --- | --- |
| `--site <SITE>` | Site id from [[site]] in the config. |
| `--config <FILE>` | Config file [default: ./auditroo.toml] |
| `--limit <LIMIT>` | Show at most this many runs. [default: 20] |
| `--json` | Print JSON. |

## auditroo history show

Show one run.

```text
auditroo history show [OPTIONS] <RUN_ID>
```

| Option | What it does |
| --- | --- |
| `<RUN_ID>` | Run id, from `auditroo history list`. |
| `--config <FILE>` | Config file [default: ./auditroo.toml] |
| `--json` | Print JSON. |

## auditroo baseline

Experimental: set which run or snapshot the next monitor run compares with.

```text
auditroo baseline <COMMAND>
```

## auditroo baseline mark

Point a baseline label at the snapshot of a monitor run.

```text
auditroo baseline mark [OPTIONS] --site <SITE> --label <LABEL> <RUN_SELECTOR>
```

| Option | What it does |
| --- | --- |
| `<RUN_SELECTOR>` | Run id, or `latest`. |
| `--site <SITE>` | Site id from [[site]] in the config. |
| `--label <LABEL>` | Label to set, for example last-known-good. |
| `--config <FILE>` | Config file [default: ./auditroo.toml] |
| `--json` | Print JSON. |

## auditroo baseline import

Copy a snapshot file into the monitor store and point a label at it.

```text
auditroo baseline import [OPTIONS] --site <SITE> --label <LABEL> <SNAPSHOT_PATH>
```

| Option | What it does |
| --- | --- |
| `<SNAPSHOT_PATH>` | Snapshot file to import. |
| `--site <SITE>` | Site id from [[site]] in the config. |
| `--label <LABEL>` | Label to set, for example last-known-good. |
| `--config <FILE>` | Config file [default: ./auditroo.toml] |
| `--json` | Print JSON. |

## auditroo bundle

Experimental: write a folder with the HTML report and data files of a monitor run.

```text
auditroo bundle [OPTIONS] --site <SITE> --out <DIR> <RUN_SELECTOR>
```

| Option | What it does |
| --- | --- |
| `<RUN_SELECTOR>` | Run id, or `latest`. |
| `--site <SITE>` | Site id from [[site]] in the config. |
| `--out <DIR>` | Folder to create. It must not exist yet. |
| `--config <FILE>` | Config file [default: ./auditroo.toml] |
| `--json` | Print JSON. |

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | No finding at or above `--fail-on`. |
| 1 | At least one finding at or above `--fail-on`. The check failed. |
| 2 | Usage or configuration error. Fix the command or `auditroo.toml`. |
| 3 | Network or runtime error. A site could not be reached or crawled, or a file could not be written. |
