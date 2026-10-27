# Findings

Every finding has a rule id and a severity. The id, such as
`noindex_added`, appears in JSON and HTML reports and in ignore rules. The
severity decides whether the run fails.

This page explains each rule and what to do about it. The text output and
the job summary show the first sentence of that advice for each finding
that fails the run, together with the rule id (`rule: noindex_added`). The
HTML report shows the full advice next to each finding.

## What the SEO terms mean

| Term | What it is |
| --- | --- |
| noindex | A robots meta tag or an `X-Robots-Tag` header that tells search engines to leave a page out of their results. |
| Canonical tag | A `<link rel="canonical">` in the page head. It names the URL that search engines should show for the page. |
| robots.txt | The file at `/robots.txt` that tells crawlers which pages they may fetch. |
| Sitemap | An XML file that lists the pages of a site for search engines. |
| hreflang | `<link rel="alternate" hreflang="...">` links that point to the other language versions of a page. |
| JSON-LD | Structured data in a `<script type="application/ld+json">` block. Google uses it for rich results, such as product prices. |
| Indexable | A page that Google may show in its search results. The next section says when auditroo counts a page as indexable. |

## How severity works

There are five levels: `critical`, `high`, `medium`, `low` and `info`. A
run fails when at least one finding is at or above `--fail-on`, which is
`high` unless you change it.

Each rule has a default severity. Some rules pick a lower level when the page
mattered less. A page that already had noindex, for example, gets `high`
instead of `critical` when it breaks. The sections below say when that
happens.

Many rules only look at indexable pages. auditroo counts a page as
indexable when all of these are true:

- It is an HTML page that loads with a 2xx status, without a redirect.
- It has no `noindex` that applies to Google. auditroo looks in two places:
  - the `robots` and `googlebot` meta tags
  - `X-Robots-Tag` headers without a crawler name, or for `googlebot`

  A header for another crawler, such as `X-Robots-Tag: bingbot: noindex`,
  does not count, because it does not apply to Google.
- Its canonical tag names the page itself, or it has none.

## Which hosts count as the site

auditroo only trusts hosts that you name. In `check` that is the host of the
start URL and its `www` twin. In `compare` it is production and the preview.
In `diff` it is the baseline's host and the host you crawl now. Every
command also trusts the live site you name with `--production-url`, or with
`production_url` in the `[crawl]` section of `auditroo.toml`.

A preview build usually contains production URLs, for example in its
canonical tags and its sitemap. When auditroo knows production, it reads
those URLs on the preview, under the same path. The preview's own pages are
fetched, never production's. When you compare two local builds that both
contain the live site's URLs, pass that site with `--production-url`.

Every other host counts as foreign. A canonical tag or a sitemap entry on a
staging host is reported, even when the whole build names that host. A
sitemap file on another host is read where it is, because Google accepts a
sitemap on another host when robots.txt names it. The pages it lists still
have to be on the site.

## Where findings come from

`compare` and `diff` look at two crawls. They report what changed between
them, and they also report problems that `check` finds on the new crawl but
not on the old one. A problem that production already has does not fail
your pull request.

`check` looks at one crawl and reports every problem it sees.

When a page breaks, its title and headings also change, because auditroo now
sees an error page. auditroo reports the broken page once. The other changes
on that page are counted on the same finding instead of being listed one by
one.

When a finding is expected, add an ignore rule. For example, this ignores
title changes on the blog:

```toml
# auditroo.toml
[gate]
ignore = [
  { rule = "title_changed", url = "/blog/*" },
]
```

Ignored findings stay in the report, marked as ignored. They do not fail
the run. [Configuration](configuration.md) has more on ignore rules.

## Indexing

### `noindex_added`

**Critical.** A page that Google could index now has a `noindex` that applies
to Google, in a meta tag or an `X-Robots-Tag` header. Google drops a noindex page from its
results the next time it crawls it.

What to do: remove the noindex, unless the page should leave Google. A
common cause is a staging setting that made it into production, such as a
CMS option or a header rule on the host.

### `noindex_removed`

**Medium.** A page had `noindex` before and can now be indexed.

What to do: check that this was planned. Thank-you pages and internal search
results usually have noindex on purpose.

### `home_noindex`

**Critical.** The start page has `noindex`. Found by `check`.

What to do: remove it, unless the whole site should stay out of search.

## Pages that stopped working

### `page_broken`

**Critical**, or **high** when the page was not indexable before. A page that
loaded before now returns an error status, such as 404 or 500. You also get
it when a redirect that worked before now ends on an error. That case is
always critical.

What to do: fix the page. If it was removed on purpose, redirect its URL to
the page that replaces it and update the links that point to it. The
finding lists the pages that link to the URL.

### `file_broken`

**High.** A file such as an image or a PDF loaded before and now returns an
error status.

What to do: restore the file, or change the pages that use it.

### `page_not_html`

**Critical**, or **high** when the page was not indexable before. A URL that
served an HTML page now answers with something else, for example JSON or a
download. Google drops the page from search. A routing rule or a wrong
`Content-Type` header is the usual cause.

What to do: serve the HTML page at this URL again.

### `home_error`

**Critical.** The start page does not load as a page of the site. Found by
`check`. These cases count:

- it returns an error status
- it is stuck in a redirect loop
- it redirects to another site, such as a login service
- it is not an HTML page

In `compare` and `diff`, the start page gets its own finding for the same
problem, such as `page_broken`, and that finding fails the run.

What to do: fix the start page. Google reaches most of a site through it.

### `page_redirected`

**Medium** when a page that loaded directly now redirects to another page.
**High** when the target has noindex or is not an HTML page, or when three
or more pages now redirect to the same URL with a temporary redirect, such
as 302 or 307. **Critical** when half or more of the site's indexable pages
now redirect to the same URL. That pattern usually means a login page or a
catch-all rule, for example a preview behind a password. The summary then
names the target, for example "23 pages now redirect to /login/".

What to do: if the page moved on purpose, use a permanent redirect (301 or
308) and link to the new URL. When many pages go to a login page, let
auditroo through the login, as [Crawling your own site](own-site.md)
shows.

### `redirect_loop`

**Critical** in `compare` and `diff`, when a page that worked now loops,
or **high** when that page was not indexable before. **High** in `check`,
for a linked URL that loops. The redirects lead back to
a URL they already passed, so no page ever loads. The finding shows the loop,
for example `/a/ -> /b/ -> /a/`.

What to do: fix the redirect rules so that the URL ends on a page that loads.

### `redirect_changed`

**High** when a redirect now ends at a different URL. **Medium** when it
ends at the same URL but takes more steps.

What to do: check that the redirect still goes where you want, in as few
steps as possible.

### `fetch_failed`

**High**, or **medium** when the page was not indexable before. auditroo
could not load a URL that worked in the old crawl. Timeouts and connection
errors cause this, and so do responses larger than `--max-body-bytes`.

What to do: check that the server answers this URL. Timeouts can be
temporary, so run the check again before you look further.

### `page_added`

**Info.** A page exists that the old crawl did not have. The site's 404 page
doesn't count, for example `/404.html`, or a page whose canonical tag names
`/404.html`.

What to do: nothing. Make sure new pages are linked from the site and listed
in the sitemap.

### `page_fixed`

**Info.** A URL that failed before works again.

What to do: nothing.

## Links and redirects

### `broken_link`

**High**, or **medium** when the target could not be loaded at all, for
example after a timeout. An internal link points to a URL that returns an
error. The finding lists the pages with the link.

What to do: fix the target page, or change the links to a page that works.

### `redirect_chain`

**Low.** An internal link points to a URL that redirects two or more times
before it reaches a page.

What to do: link to the final URL directly, and point old URLs straight at
their final target.

### `orphaned`

**High.** An indexable page could be reached by following links from the
start page before, and cannot now. It still works. Either no crawled page
links to it any more, or only pages that are cut off themselves do, such as
a section whose entry link was removed. Google finds pages mainly through
links.

What to do: link to the page again from a related page or the navigation.
The finding lists the pages that linked to it before, or the cut-off pages
that still do.

### `orphan_page`

**Low.** The sitemap lists the page, but it cannot be reached by following
links from the start page. Found by `check`. auditroo only reports this when
the crawl was complete, so a `--max-pages` limit does not cause false
alarms.

What to do: link to the page from the site, or remove it from the sitemap if
it is no longer needed.

## Canonical tags

### `canonical_external`

**High** in `check`. In `compare` and `diff` it is **critical** when the
canonical tag newly names another host, or **high** when the page has
noindex anyway. The canonical tag names a URL on another host, for example
a staging server. Google may then show the other host in search results, or
neither.

Some sites point canonical tags to another domain on purpose, for example
for articles that first appeared on another site. That is why `check`
reports it as high, and the message shows the ignore rule that accepts it:

```toml
[gate]
ignore = [
  { rule = "canonical_external", url = "/syndicated/*" },
]
```

`check` treats the start URL as the live site. A local build or a preview
whose canonical tags name production gets this finding in `check`. Pass the
live site with `--production-url` or `production_url` in `[crawl]`, or
compare the build with production (`auditroo compare`), so production URLs
count as the build's own. In `compare` and `diff`, when a canonical tag
only names another path on a host it already named before, the finding is
`canonical_changed` instead.

What to do: point the canonical tag at the page's own URL. A wrong base URL
setting in the build is the usual cause.

### `canonical_changed`

**High**, or **medium** when the canonical tag now names the page itself.
The canonical tag names a different URL than before. When it names another
page, Google indexes that page instead. The message says when the new
target redirects or returns an error, and when it has noindex.

What to do: make sure the canonical tag names the URL you want in search
results.

### `canonical_removed`

**Medium.** The page had a canonical tag and has none now.

What to do: add it back. Without one, Google picks the URL it thinks is
best, which can be a version with tracking parameters.

### `canonical_to_redirect`

**Medium.** The canonical tag names a URL that redirects. Found by `check`,
and by `compare` and `diff` when the preview has it and production does not.
Canonical tags and hreflang links that name production are followed on the
preview, so this also works for preview builds.

What to do: use the final URL in the canonical tag.

### `canonical_to_error`

**High.** The canonical tag names a URL that returns an error. Found by
`check`.

What to do: point the canonical tag at a page that loads.

### `canonical_to_noindex`

**Medium.** The canonical tag names a page that has noindex. The two
signals say opposite things. Found by `check`.

What to do: remove the noindex on the target, or change the canonical tag.

## Titles, descriptions and headings

The rules that compare two crawls only look at pages that load in both.

### `title_changed`

**Medium.** The page has a different `<title>`. The title is the headline of
the search result. When many pages get the same new title, the report
groups them, which usually points to a template change.

What to do: check that the new title still describes the page.

### `title_removed`

**Medium.** The page had a `<title>` and has none now.

What to do: add the title back.

### `title_missing`

**Medium.** An indexable page has no `<title>`. Found by `check`.

What to do: add a title that describes the page.

### `title_duplicate`

**Low.** Two or more indexable pages have the same title. Found by `check`.

What to do: give each page its own title, so search results can tell them
apart.

### `description_changed`

**Low.** The meta description changed. Google often shows it under the
title.

What to do: check that it still describes the page.

### `description_removed`

**Low.** The page lost its meta description. Google then picks a text
snippet from the page.

What to do: add it back if you wrote it on purpose.

### `h1_changed`

**Low.** The main `<h1>` heading changed.

What to do: check that it still matches what the page is about.

### `h1_removed`

**Medium.** The page had an `<h1>` and has none now. This often happens when
a template change turns the heading into a `<div>`.

What to do: add a main heading back.

### `h1_missing`

**Low.** An indexable page has no `<h1>`. Found by `check`.

What to do: add a main heading that says what the page is about.

### `headings_changed`

**Info.** The H2 or H3 headings changed. That is usually a content edit.

What to do: nothing, unless it was not planned.

## Structured data and languages

### `structured_data_removed`

**Medium.** The page had JSON-LD structured data and has none now. Rich
results depend on it, for example product prices or breadcrumbs.

What to do: add the JSON-LD back if the page should still get rich results.

### `structured_data_changed`

**Medium** when the page lost a schema.org type, **low** when it only gained
one. auditroo collects the types of every top-level JSON-LD node. Nodes in
`@graph` count, and an `@type` with several types counts each of them. It
does not validate the schema itself.

What to do: check that the page still has the types it should have. A lost
`Product` or `BreadcrumbList` usually means a lost rich result.

### `structured_data_invalid`

**High.** A JSON-LD block on the page is not valid JSON. The message has the
parser's error, for example a trailing comma. Google ignores a block it
cannot parse.

What to do: fix the JSON syntax. A template that writes values without
escaping them is a common cause.

### `hreflang_changed`

**Medium.** The page's hreflang links to its language versions changed.

What to do: check that every language version still lists all versions,
including itself.

### `hreflang_no_return`

**Medium.** The page lists another page as a language version, and that page
does not link back. Google ignores hreflang pairs that do not link both ways.
Found by `check`.

What to do: add the return link on the other page.

### `hreflang_broken`

**Medium.** The page lists a language version that redirects or returns an
error. Found by `check`.

What to do: point the hreflang link at the final URL of a page that loads.

## robots.txt

auditroo stores the robots.txt of every host the crawl visits, and checks
each page against its own host's file, the way Google does. A `www` host and
the bare domain have separate files. The `User-agent` lines are read the way
Google reads them, so `User-agent: Googlebot/2.1` and `User-agent: Googlebot*`
both mean Googlebot.

It checks these crawlers:

| Crawler | Used for | Severity when newly blocked |
| --- | --- | --- |
| Googlebot | Google Search | Critical |
| Bingbot | Bing, and search engines that use its index | High |
| OAI-SearchBot and ChatGPT-User | ChatGPT search | Medium |
| PerplexityBot | Perplexity | Medium |
| Claude-SearchBot | Claude search | Medium |
| GPTBot and ClaudeBot | AI training at OpenAI and Anthropic | Low |
| Google-Extended | Gemini training | Low |
| CCBot | Common Crawl | Low |

Many sites block AI training crawlers on purpose. That is why a change for
them does not fail a run by default.

### `robots_blocked`

**Critical** when Googlebot is affected. Otherwise the severity of the most
important crawler in the table above. robots.txt now blocks a crawler from
indexable pages that it could crawl before. Crawlers with the same blocked
pages share one finding. A new `Disallow: /` shows up as every page being
blocked.

What to do: remove or narrow the `Disallow` rule. Blocked pages drop out of
search results over time.

### `robots_error`

**Critical.** robots.txt returns a 5xx or 429 status, or the request fails.
Google then stops crawling the whole site until robots.txt works again. A 404 or another 4xx status is fine. It means
there are no rules.

What to do: make robots.txt load again with status 200, or 404 if you have
none.

### `robots_blocks_site`

**Critical.** robots.txt blocks Googlebot from the start page. Found by
`check`.

What to do: remove the rule that blocks it, unless the site should stay out
of Google.

## Sitemaps

### `sitemap_urls_removed`

**High.** Pages that the sitemap listed before are no longer in it, although
they still work. This is one finding for the whole site, with the list of
URLs. It often means the sitemap generator lost part of the site.

What to do: add the pages back, or find out why the generator no longer
sees them.

### `sitemap_broken`

**High.** A sitemap file returns an error or is not a valid sitemap. That
includes the sitemaps that a sitemap index file points to. Google cannot
read the URLs in the file until it is fixed. `check` also reports entries
whose `<loc>` is not a full `https://` or `http://` URL, such as a relative
path. Google ignores those. That finding is **high** when no entry of the
file is usable, and **medium** when some are.

What to do: make the file load with status 200 and valid XML, with a full
URL in every `<loc>`.

### `sitemap_missing`

**Low.** auditroo found no sitemap at `/sitemap.xml`, and robots.txt names
none. Found by `check`.

What to do: add a sitemap, or name it in robots.txt with a `Sitemap:` line.
If it lives somewhere else, pass it with `--sitemap`.

### `sitemap_url_error`

**High.** The sitemap lists a URL that returns an error or could not be
loaded. Found by `check`.

What to do: fix the page, or remove its URL from the sitemap.

### `sitemap_url_redirect`

**Low.** The sitemap lists a URL that redirects. Found by `check`. When half
or more of the listed URLs redirect to the same page, the finding is
**critical**. When three or more go there with a temporary redirect, it is
**high**. That pattern usually means a login page or a catch-all rule, the
same as for `page_redirected`.

What to do: list the final URL instead. When many URLs go to a login page,
let auditroo through the login, as [Crawling your own site](own-site.md)
shows.

### `sitemap_url_noindex`

**Medium.** The sitemap lists a page that has noindex. The two say opposite
things. Found by `check`.

What to do: remove the noindex, or remove the page from the sitemap.

### `sitemap_url_blocked`

**High.** The sitemap lists a URL that robots.txt blocks for Googlebot.
Found by `check`.

What to do: allow the URL in robots.txt, or remove it from the sitemap.

### `sitemap_url_external`

**High** in `check`, **critical** in `compare` and `diff` when the sitemap
did not list that host before. The sitemap lists URLs on another host, for
example a staging server. Google ignores them for this site. When the
sitemap and the canonical tags both name a staging host, the build usually
has the wrong site URL.

What to do: list only the site's own URLs. Check the site URL setting of the
build, often called `site` or `BASE_URL`.

### `sitemap_url_canonicalised`

**Low.** The sitemap lists a page whose canonical tag names another URL.
Found by `check`.

What to do: list the URL that the canonical tag names instead.

## Your own checks

### `assertion`

The severity comes from your `auditroo.toml`. A custom check in an
`[[assert]]` block failed. The message names the check and what it found.
[Experimental features](experimental.md) explains custom checks.

What to do: read the check's description in your config. It says what was
expected.

### `quality_budget`

The severity comes from your `auditroo.toml`. A result from a quality tool,
for example a Lighthouse score, is outside the budget you set.
[Experimental features](experimental.md) explains quality budgets.

What to do: compare the measured value with the budget in the report.
