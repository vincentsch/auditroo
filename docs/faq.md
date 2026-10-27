# FAQ

## What is auditroo for?

It catches SEO mistakes in a deploy before they go live. You run it on a
preview of a change and it compares the preview with production. The tests
in your repository check that your code works. auditroo checks that search
engines can still find and index your pages.

## Is it a full SEO audit tool?

No. Audit tools crawl a site and list everything that could be better.
auditroo looks at what a change breaks. `check` also reports problems on a
single crawl, but only technical problems that affect whether Google can
find and index your pages. Examples are broken links, or a canonical tag
that names the wrong URL. The canonical tag tells Google which URL to show
for a page. auditroo does not judge your content or your keywords.

## Does auditroo send data anywhere?

No. It sends requests to the sites you name and to the sitemaps they point
to, plus the webhooks you set up for monitoring. It has no telemetry and
needs no account.

## Can I check a site that is not mine?

auditroo names itself in its User-Agent header. It also follows robots.txt,
the file where a site tells crawlers which pages they may fetch. By default
it fetches about 10 pages a second. Still, crawl only sites you are allowed
to crawl.

## Why does compare not report problems that production already has?

A pull request should only fail for what it breaks. Run `auditroo check` on
production to see the old problems, and fix them separately.

## Why did the demo exit with code 0 when it found problems?

So that you can run it anywhere without a red error. A real `compare` with
the same findings exits with code 1.

## What does "Below the fail-on level" mean?

These findings are real, but their severity is lower than `--fail-on`, so
they do not fail the run. With the default `--fail-on high`, a new title is
reported there.

## Does it check page speed or accessibility?

Not by default. The experimental [quality budgets](experimental.md#quality-budgets)
can read Lighthouse or Pa11y results and fail a run when a score drops below
a budget.

## Does it work with coding agents?

Yes. The output is plain text that an agent can read, and `--json` gives the
full report as data. [AGENTS.md](../AGENTS.md) has setup steps for an agent
to follow, and the README has a prompt to start it.

## Does it work on Windows?

auditroo is built for Windows on x86_64 and arm64. Download
`auditroo-windows-amd64.exe` or `auditroo-windows-arm64.exe` from a release,
or run the installer in Git Bash.

## What license does it use?

MIT. See [LICENSE](../LICENSE).
