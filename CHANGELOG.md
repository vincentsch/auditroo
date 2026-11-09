# Changelog

All notable changes to auditroo are listed here. Version headings look like
`## 0.1.0 (2026-11-02)`. The release workflow copies the section of a
version into its release notes.

## Unreleased

The first public release. It contains:

### Commands

- `auditroo check <url>` crawls a site and reports the problems that one
  crawl shows, such as broken links and noindex pages in the sitemap.
- `auditroo compare <production> <preview>` crawls both sites and reports
  what got worse on the preview. It matches pages by path. Problems that
  production already has are not reported.
- `auditroo snapshot` and `auditroo diff` do the same with a saved snapshot
  instead of a live production site.
- `auditroo demo` compares a built-in sample site with a broken copy, with
  no network access.
- `auditroo report` prints a saved report again in any output format.
- `auditroo explain <rule>` prints what a rule checks and what to do about
  it, from the same text as `docs/findings.md`. It ends with an ignore rule
  for it that you can paste.

### Findings

- 53 rules, each with a severity and advice. `docs/findings.md`
  explains them.
- Critical findings for the worst deploy mistakes:
  - new noindex on indexable pages
  - pages that start to fail
  - canonical tags that move to another host
  - robots.txt rules that block Googlebot
  - a robots.txt that returns a server error
- robots.txt of every host the crawl visits is stored and checked for search
  engines and the main AI crawlers. auditroo reads it the way Google does.
  Each host has its own file, and `User-agent: Googlebot/2.1` counts for
  Googlebot. A 429 or 5xx answer blocks the whole site.
- When a preview's robots.txt blocks pages that production's robots.txt
  allows, `compare` reports the block and still checks those pages. `diff`
  does the same with the baseline's robots.txt. A rule that names auditroo
  itself is always followed.
- noindex counts when it applies to Google. That is a robots or googlebot
  meta tag, or an `X-Robots-Tag` header without a crawler name or for
  googlebot.
- Only hosts you name count as the site. In `compare` and `diff`, production
  URLs in a preview are read on the preview, so a preview never checks
  production by accident. `--production-url`, or `production_url` in
  `auditroo.toml`, does the same for local builds that contain the live
  site's URLs. A staging host in canonical tags or the sitemap is
  critical.
- A run never passes when auditroo could not see the site. When the start
  page of production or of a baseline fails, that is an error. When all
  pages of a preview redirect to one login page, that is critical.
- When a page breaks, the other changes on that page are counted on one
  finding, which also lists the pages that link to it.
- `--fail-on` sets the severity that fails a run. Ignore rules in
  `auditroo.toml` or `--ignore` accept known findings.
- The noindex header that preview hosts such as Vercel and Netlify add is
  ignored on their preview domains. `--ignore-noindex-header` does the same
  for other hosts.

### Output

- Text output that starts with a summary. Repeated findings are grouped,
  and URLs are shown as paths. Each finding names its rule id, for ignore
  rules. Each finding that fails the run also has a line on what to do.
- When some URLs were not checked, the first line says `PASS (incomplete)`
  or `FAIL (incomplete)`, and a line above the findings says why. The notes
  list the URLs that robots.txt kept auditroo from. The JSON report has
  `incomplete`.
- When both sites of a `compare` or `diff` point their canonical tags or
  sitemap at the same other host, the output says above the findings that
  `--production-url` with that host is probably missing.
- Ignored findings are listed with their rule id and URL, under "Ignored"
  in the text output and in the job summary. In JSON every finding has
  `ignored`, also when it is `false`.
- `--format markdown` for job summaries and pull request comments.
- `--format github` for GitHub Actions annotations.
- `--format json` for the full report as data. `docs/json.md` describes
  its fields.
- `--html` writes a standalone HTML report with advice for each finding.
- `--junit` writes JUnit XML.
- Exit codes that let CI tell a failed check (1) from a usage error (2) and
  a network or runtime error (3).
- Progress goes to stderr, so stdout stays clean for reports.

### CI and installing

- A GitHub Action that installs a release binary and adds the report to the
  job summary and the annotations.
- CI recipes in `docs/ci.md`:
  - Vercel previews
  - Cloudflare Pages previews, which wait for Cloudflare's check on the
    commit
  - preview URLs that you know in advance
  - builds served inside the CI job
- The recipes read `auditroo.toml` from the base branch, so a pull request
  cannot add ignore rules that hide its own problems.
- Release binaries for the three desktop systems on x86_64 and arm64, with
  SHA-256 checksums and build provenance. The Linux binaries are static.
- The GitHub Action installs the auditroo release of its own version unless
  you pass `version`.
- `install.sh` checks the checksum before it installs.
- A Dockerfile.

### Experimental

These work but may change before 1.0. `docs/experimental.md` describes them.

- Custom checks with `[[extract]]` and `[[assert]]`, and ready-made recipes.
- Access log analysis for search engine and AI crawler activity.
- Scheduled monitoring of several sites. It saves a history of runs and can
  send notifications.
- `auditroo bundle` writes a monitor run into a folder that you can send to
  a client.
- Quality budgets for results from tools such as Lighthouse and Pa11y.
- Traffic numbers from Search Console or analytics exports, shown next to
  findings.
- `--render-js` renders pages in headless Chrome, in builds with the
  `browser` feature.
