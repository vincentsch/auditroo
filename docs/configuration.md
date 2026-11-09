# Configuration

auditroo works without a config file. When you want the same settings on
every run, put them in `auditroo.toml`.

auditroo reads `./auditroo.toml` from the folder you run it in, when that
file exists. Use `--config path/to/file.toml` to read another file. A file
you name with `--config` must exist. An unknown key or a wrong value stops
the run with exit code 2 and a message that names the key.

Command-line flags win over the file. Flags that you can repeat, such as
`--include` or `--header`, replace the list from the file. `--ignore` is
different. It adds to the ignore rules from the file.

## A complete example

```toml
[crawl]
max_pages = 5000
rate_limit_ms = 0
exclude = ["/search*", "/cart*"]
headers = [
  { name = "x-crawler-token", value = "${AUDITROO_TOKEN}" },
]

[gate]
fail_on = "high"
ignore = [
  { rule = "title_changed", url = "/blog/*" },
  { url = "/legacy/*" },
]
```

## `[crawl]`

| Key | Default | Meaning |
| --- | --- | --- |
| `max_pages` | `10000` | Stop after this many URLs. `0` means no limit. |
| `max_depth` | `0` | Follow links at most this many clicks from the start page. `0` means no limit. |
| `concurrency` | `8` | Fetch this many URLs at the same time. |
| `rate_limit_ms` | `100` | Wait at least this long between two requests to one host. |
| `timeout_ms` | `10000` | Give up on one request after this long. |
| `max_body_bytes` | `10485760` | Treat larger responses as failed. |
| `include` | `[]` | Only crawl paths that match one of these patterns. |
| `exclude` | `[]` | Do not crawl paths that match one of these patterns. |
| `sitemaps` | `[]` | Read these sitemap URLs instead of the ones robots.txt names. A sitemap is the XML file that lists your pages for search engines, and robots.txt is the file with the rules for crawlers. |
| `production_url` | none | The live site, when you crawl a preview or a local build of it, for example `"https://www.example.com"`. Its URLs count as the crawled site's own, for example in the sitemap or in canonical tags (the tag that names the URL Google should show for a page). `--production-url` overrides it. In `compare` it applies to both sites. When the production side is the live site itself, as in CI, it changes nothing there. |
| `user_agent` | `auditroo/<version> (+https://github.com/vincentsch/auditroo)` | The User-Agent header. |
| `headers` | `[]` | Extra request headers, as `{ name = "...", value = "..." }`. |
| `respect_robots` | `true` | Set to `false` to crawl pages that robots.txt blocks for auditroo. Same as `--ignore-robots`. In `compare` it applies to production too. |
| `render_js` | `false` | Render pages in headless Chrome. See [JavaScript sites](javascript.md). |

Patterns match the path and query of a URL, so they start with `/` or `*`.
A `*` stands for any text, including `/`. Some examples:

- `/blog/*` matches every page under `/blog/`.
- `/search*` matches `/search?q=shoes`.
- `*?sort=*` matches every URL with a `sort` parameter.

A pattern such as `blog/*` would never match, so auditroo stops with exit
code 2 and suggests `/blog/*`.

### Secrets in headers

A header value can name an environment variable as `${NAME}`. auditroo
reads the variable when it starts. When the variable is not set, the run
stops with exit code 2. That way tokens are not in the file or in your shell
history. `${NAME}` works the same way in `--header` on the command
line, as long as you quote it with single quotes so that your shell leaves
it alone.

auditroo sends these headers only to the host of the start URL. They are
dropped when a redirect leaves that host, or when it switches from HTTPS to
HTTP.

## `[gate]`

| Key | Default | Meaning |
| --- | --- | --- |
| `fail_on` | `"high"` | The lowest severity that fails a run: `critical`, `high`, `medium`, `low` or `info`. |
| `ignore` | `[]` | Findings that should not fail a run. |

### Ignore rules

An ignore rule has up to three parts. A finding is ignored when every part
you set matches:

| Part | Matches |
| --- | --- |
| `rule` | The rule id, such as `title_changed`. The output shows it under each finding that fails the run, and [Findings](findings.md) lists them all. |
| `url` | The finding's URL. A pattern that starts with `/` matches the path and query. You can also give a full URL such as `https://www.example.com/promo/*`. |
| `assertion` | The id of one of your own checks. |

Some examples:

```toml
[gate]
ignore = [
  # Titles on the blog change all the time.
  { rule = "title_changed", url = "/blog/*" },
  # Everything under /legacy/ is going away.
  { url = "/legacy/*" },
  # We block AI training crawlers on purpose.
  { rule = "robots_blocked" },
]
```

The last rule ignores every `robots_blocked` finding, including one for
Googlebot. Use it only when you are sure.

The first rule changes nothing with the default settings. `title_changed`
is a medium finding, and a run fails only on high and critical findings
unless you change `fail_on`. Its ignore rule starts to matter with
`fail_on = "medium"` or lower. Run `auditroo explain title_changed` to see
the severity of a rule and what it checks.

On the command line, write the rule as a TOML table:

```sh
auditroo compare https://www.example.com "$PREVIEW_URL" \
  --ignore '{ rule = "title_changed", url = "/blog/*" }'
```

Ignored findings do not fail a run, but auditroo still shows them. The
text output lists them under "Ignored" with their rule id and URL. The
other report formats mark them as ignored, and in the JSON report every
finding has `"ignored": true` or `false`. Only the GitHub annotations leave
them out. An ignore rule that names an unknown rule id
stops the run with exit code 2, so a typo does not hide findings by
accident.

In a pull request check, read `auditroo.toml` from the base branch.
Otherwise a pull request can add ignore rules that hide its own problems.
[Ignore rules and pull requests](ci.md#ignore-rules-and-pull-requests)
shows how.

## Other sections

`auditroo.toml` can also hold custom checks (`[[extract]]` and `[[assert]]`)
and settings for log analysis and scheduled monitoring. These features may
still change. [Experimental features](experimental.md) describes them.
