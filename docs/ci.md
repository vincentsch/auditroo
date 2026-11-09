# Run auditroo in CI

The usual setup runs `auditroo compare` after each preview deployment. It
compares the preview with production and fails the job when a change would
hurt search. This page shows that setup for GitHub Actions with Vercel, and
how to adapt it to other hosts and CI systems.

## How a CI run passes or fails

auditroo exits with one of four codes:

| Code | Meaning | What CI should do |
| --- | --- | --- |
| 0 | No finding at or above `--fail-on`. | Pass. |
| 1 | At least one finding at or above `--fail-on`. | Fail the check. |
| 2 | Usage or configuration error. | Fail. Fix the command or `auditroo.toml`. |
| 3 | Network or runtime error, for example the preview did not answer, or production answered 403 and gave nothing to compare with. | Fail or retry. |

`--fail-on` is `high` by default. Use `--fail-on critical` to fail only on
the worst problems while you get used to the tool. Findings below the level
still appear in the report.

auditroo can write the same result in several formats at once:

| Flag | Output |
| --- | --- |
| `--format markdown` | Markdown on stdout, for `$GITHUB_STEP_SUMMARY` or a pull request comment |
| `--format github` | GitHub Actions annotations on stdout |
| `--html report.html` | A standalone web page, for example to save as a build artifact |
| `--junit auditroo.xml` | JUnit XML for CI systems that show test reports |
| `--out report.json` | The full report as JSON |

`auditroo report report.json` prints a saved report again in any of these
formats, without crawling.

## GitHub Actions with Vercel

Vercel sends a `deployment_status` event to GitHub when a preview is ready.
This workflow runs on that event and compares the preview with production:

```yaml
# .github/workflows/seo.yml
name: SEO
on: deployment_status

permissions:
  contents: read

jobs:
  auditroo:
    if: >-
      github.event.deployment_status.state == 'success' &&
      !startsWith(github.event.deployment.environment, 'Production')
    runs-on: ubuntu-latest
    steps:
      - uses: vincentsch/auditroo@v0.1.0
        env:
          PREVIEW_URL: ${{ github.event.deployment_status.target_url }}
        with:
          args: compare https://www.example.com "$PREVIEW_URL"
          html: auditroo-report.html
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: auditroo-report
          path: auditroo-report.html
          if-no-files-found: ignore
```

Replace `https://www.example.com` with your production URL. The action
installs the auditroo release that matches the action's version, so
`@v0.1.0` always runs auditroo 0.1.0.

This job has no checkout step, so auditroo runs with its defaults and does
not see an `auditroo.toml`. When you have settings there, such as ignore
rules or crawl limits, check out the commit that was deployed before the
auditroo step:

```yaml
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event.deployment.sha }}
          persist-credentials: false
      - uses: vincentsch/auditroo@v0.1.0
        # ... as above
```

auditroo reads `auditroo.toml` from the folder it runs in, which is the
checkout. Pass `--config path/to/auditroo.toml` in `args` when the file
lives elsewhere.

What you get on each preview:

- a failed check when a finding reaches `--fail-on`
- the report as Markdown in the job summary, with what to do and the rule
  id for each problem
- annotations on the pull request, at most ten errors and ten warnings
- the HTML report as a downloadable artifact

Vercel adds an `X-Robots-Tag: noindex` header to every preview deployment.
That header tells search engines not to index the page. auditroo ignores it
on `*.vercel.app` hosts, so it does not report every page as noindex. It
still checks the robots meta tags in your HTML. If your previews run on your
own domain, add `--ignore-noindex-header` to the arguments.

### Previews behind Vercel Deployment Protection

When your previews need a login, auditroo gets a login page or an error
instead of your site. Vercel's Protection Bypass for Automation gives you a secret for this.
Save it as a repository secret, for example `VERCEL_BYPASS`, and send it as a
header:

```yaml
      - uses: vincentsch/auditroo@v0.1.0
        env:
          PREVIEW_URL: ${{ github.event.deployment_status.target_url }}
          VERCEL_BYPASS: ${{ secrets.VERCEL_BYPASS }}
        with:
          args: >-
            compare https://www.example.com "$PREVIEW_URL"
            --header 'x-vercel-protection-bypass: ${VERCEL_BYPASS}'
```

Use single quotes here. They pass `${VERCEL_BYPASS}` to auditroo as text,
and auditroo reads the value from the environment itself. That way the
secret never appears in a command line.

auditroo sends a `--header` only to the exact hosts of the URLs you pass,
never to other hosts that a page links to or redirects to. In `compare` that
means production also gets the header. That is fine for a bypass token,
because production ignores headers it does not know.

### The action's inputs and outputs

| Input | Default | Meaning |
| --- | --- | --- |
| `args` | required | The auditroo command and its arguments, for example `compare https://www.example.com "$PREVIEW_URL"`. Leave out `--out`; the action writes the report itself. |
| `version` | the action's version | Release to install. `latest` installs the newest release, `path` uses an auditroo that is already on PATH. |
| `fail` | `true` | Fail the step when a finding reaches `--fail-on`. |
| `summary` | `true` | Add the report to the job summary. |
| `annotations` | `true` | Show findings as annotations. |
| `html` | empty | Also write the HTML report to this path. |

| Output | Meaning |
| --- | --- |
| `exit-code` | auditroo's exit code. |
| `passed` | `true` when no finding reached `--fail-on`. |
| `report` | Path of the full JSON report. Empty when the command writes no report, such as `check --help`. |

The action reads `args` as shell words, so you can quote values. Never put
text that someone else controls into `args` directly, such as a branch name
or a pull request title. Pass values like that through `env` and refer to
them as `"$NAME"`, as the examples above do.

With `fail: false` the step does not fail because of findings. It still
fails when auditroo cannot finish, with exit code 2 or 3. Use the `passed`
output to decide what happens next.

## robots.txt and --ignore-robots

Leave `--ignore-robots` out of your CI workflow. In `compare` it applies to
both sites. auditroo would then also crawl the pages of your live site that
its robots.txt blocks for auditroo.

You do not need the flag when a pull request changes robots.txt. Say the
preview's robots.txt blocks `/docs/`, and production's robots.txt allows
it. auditroo reports the new block as `robots_blocked` and still checks the
pages under `/docs/` on the preview, so a broken link there also fails the
check. auditroo always follows a rule that names auditroo itself, such as
`User-agent: auditroo`.

Some URLs can still go unchecked, because production's robots.txt blocks
auditroo from them too. The output then says above the findings that the
result is incomplete, and the notes list those URLs.

## Netlify, Cloudflare Pages and other hosts

auditroo needs the URL of a finished preview. How you get it depends on the
host.

- If your host creates GitHub deployments for its previews, the Vercel
  workflow works as it is. The pull request page then shows an entry such
  as "deployed to Preview" with a link.
- Netlify and Cloudflare Pages also add the noindex header to previews.
  auditroo ignores it on `*.netlify.app` and `*.pages.dev` hosts.
- If you know the preview URL in advance, or an earlier step in the job
  prints it, run the workflow on `pull_request`.
  [When you know the preview URL](#when-you-know-the-preview-url) shows how.
- If you get no preview URL at all, build the site inside the job and
  compare that build with production.
  [Build the site inside the job](#build-the-site-inside-the-job) shows how.

## When you know the preview URL

Some hosts give each pull request or branch its own preview URL, such as
`https://pr-42.preview.example.com`. This workflow runs on each pull
request. It waits until the preview answers, then compares it with
production:

```yaml
# .github/workflows/seo.yml
name: SEO
on: pull_request

permissions:
  contents: read

jobs:
  auditroo:
    runs-on: ubuntu-latest
    env:
      # The preview URL of this pull request. Change it to your host's pattern.
      PREVIEW_URL: https://pr-${{ github.event.pull_request.number }}.preview.example.com
    steps:
      - name: Wait for the preview
        run: |
          for i in $(seq 1 60); do
            curl -fsS -o /dev/null "$PREVIEW_URL" && exit 0
            sleep 10
          done
          echo "The preview at $PREVIEW_URL did not answer within 10 minutes." >&2
          exit 1
      - uses: vincentsch/auditroo@v0.1.0
        with:
          args: compare https://www.example.com "$PREVIEW_URL"
          html: auditroo-report.html
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: auditroo-report
          path: auditroo-report.html
          if-no-files-found: ignore
```

The wait only checks that the preview answers. If your host still serves
the old build until the new one is ready, auditroo may check the old build.
Then use the URL from the deploy step below, or the `deployment_status`
workflow above.

When a step of the same job deploys the preview and writes its URL as a
step output, pass that output instead. Here the deploy step has
`id: deploy` and writes `url=...` to `$GITHUB_OUTPUT`:

```yaml
      - uses: vincentsch/auditroo@v0.1.0
        env:
          PREVIEW_URL: ${{ steps.deploy.outputs.url }}
        with:
          args: compare https://www.example.com "$PREVIEW_URL"
```

If every pull request deploys to the same staging URL, save that URL as a
repository variable, for example `STAGING_URL`. Then set
`PREVIEW_URL: ${{ vars.STAGING_URL }}` in the first workflow.

## Build the site inside the job

For a static site you can build and serve the pull request's version in the
job itself. This works with any host:

```yaml
# .github/workflows/seo.yml
name: SEO
on: pull_request

permissions:
  contents: read

jobs:
  auditroo:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 22
      - run: npm ci && npm run build
      - name: Serve the build
        run: |
          npx --yes serve@14 dist --listen 4321 &
          for i in $(seq 1 30); do curl -fsS -o /dev/null http://localhost:4321/ && break; sleep 1; done
      - uses: vincentsch/auditroo@v0.1.0
        with:
          args: compare https://www.example.com http://localhost:4321
```

The checkout also gives auditroo your `auditroo.toml`, if you have one.

Change `dist` to your build folder. A local server knows nothing about the
redirects and headers that your host adds, so you may get findings about
redirects that only exist on the host. Ignore them in `auditroo.toml`, or
use the real preview when those rules matter.

## Other CI systems

Install the binary with the installer and run auditroo like any test command.
Pin the version so that builds stay repeatable:

```sh
curl -fsSL https://raw.githubusercontent.com/vincentsch/auditroo/main/install.sh \
  | AUDITROO_VERSION=v0.1.0 sh
export PATH="$HOME/.local/bin:$PATH"
auditroo compare "$PRODUCTION_URL" "$PREVIEW_URL" \
  --junit auditroo.xml --html auditroo-report.html
```

In GitLab CI, for example, save `auditroo.xml` as a `junit` report artifact.
GitLab then shows the findings in the merge request.

## Without a production site

Before a launch or a migration there may be no production site to compare
with. Save a snapshot of the old state and compare later builds with it:

```sh
auditroo snapshot https://old.example.com --out baseline.json
auditroo diff https://new.example.com --baseline baseline.json
```

`diff` matches pages by path like `compare`, so the two sites can be on
different hosts. You can commit `baseline.json` to the repository or store
it as a CI artifact.

## Speed

auditroo fetches 8 URLs at the same time and waits 100 ms between two
requests to the same host. That is about 10 pages a second. A site with 500
pages takes about a minute per crawl, and `compare` crawls two sites.

`compare` uses the same `--rate-limit-ms` for both sites. Leave the default
when production has real visitors, or ask your host what it allows. Turn the
wait off with `--rate-limit-ms 0` only for a `check` or `diff` of a preview
or a local build.

To check part of a large site, limit the crawl:

```sh
auditroo compare https://www.example.com "$PREVIEW_URL" --max-pages 2000 --include '/blog/*'
```
