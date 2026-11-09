[Changing auditroo itself? Read CONTRIBUTING.md](CONTRIBUTING.md)

# Set up auditroo for a project

auditroo is a command-line tool that crawls a website and reports SEO
problems. Its main use is a pull request check that compares a preview
deployment with production. Follow these steps when a user asks you to add
it to their project. Also follow the project's own rules. Make the change
small and easy to undo.

The setup prompt is:

```text
Add auditroo SEO checks to this project.
Use https://github.com/vincentsch/auditroo: read its AGENTS.md and follow
the steps. Run a first check on our production site and show me what it
finds. Then add the pull request check to our CI.
```

## 1. Find the site and how it is deployed

Read the project's README and its build and CI config files. Find out:

- The production URL. Good places to look:
  - the site URL setting of the framework, such as `site` in `astro.config.*`
  - `homepage` in `package.json`
  - a `CNAME` file
  - the README

  Do not print secrets from `.env` files while you look.
- Where previews are deployed:
  - `vercel.json` or a `.vercel` folder means Vercel
  - `netlify.toml` means Netlify
  - `wrangler.toml` means Cloudflare
- Which CI system runs, for example `.github/workflows/` or `.gitlab-ci.yml`.
- How the site is built, and into which folder, in case you need to build it
  in CI.

If you cannot find the production URL, ask the user. Tell the user what you
found and which files you plan to add before you change anything.

## 2. Install auditroo

Run `auditroo --version`. Only install when the shell says the command does
not exist. If it exists but fails, report the error instead of installing
over it.

```sh
curl -fsSL https://raw.githubusercontent.com/vincentsch/auditroo/main/install.sh | sh
```

The installer puts the binary in `~/.local/bin`, or in `INSTALL_DIR` when
that is set. It checks the download's SHA-256 checksum. If the command is
not found afterwards, add the install folder to PATH for this session only.
Do not edit shell profiles unless the user asks.

On Windows without Git Bash, download `auditroo-windows-amd64.exe` and
`checksums.txt` from the latest release and compare the hash with
`Get-FileHash -Algorithm SHA256` before you run it.

Run `auditroo demo` once. It needs no network and shows what a failing
result looks like.

## 3. Run a first check on production

```sh
auditroo check https://www.example.com --max-pages 500
```

Use the default crawl speed on production. Only crawl sites that belong to
the user.

Read the output and summarize it for the user in plain words. Start with
critical and high findings. `docs/findings.md` in the auditroo repository
explains each rule id and what to do about it, and `auditroo explain
<rule>` prints the same text. The text output shows the rule id under each
finding, for example `rule: canonical_external`. If you
want the result as data, add `--out report.json`. `docs/json.md` describes
its fields. Do not change the site to fix findings unless the user asks you
to.

When the first line says `PASS (incomplete)` or `FAIL (incomplete)`,
auditroo could not check everything. In the JSON report, `incomplete` is
`true`. The line above the findings and the notes at the end say why. Tell
the user, so a passing check is not read as a clean site.

The exit code tells you how the run went:

| Code | Meaning |
| --- | --- |
| 0 | No finding at or above `--fail-on` (default `high`). |
| 1 | Findings at or above `--fail-on`. The check worked and found problems. |
| 2 | The command or `auditroo.toml` is wrong. The message names the problem. |
| 3 | The site could not be crawled, for example a timeout or a refused connection. |

Many `403` responses or a challenge page mean that a firewall blocks the
crawler. Explain that to the user and point them to `docs/own-site.md`. Ask
before you add a bypass token or change a firewall rule.

## 4. Add the pull request check

Pick the recipe from `docs/ci.md` that fits the project:

- Vercel with GitHub: a workflow on `deployment_status` that runs
  `compare` against the preview URL from the event.
- A host that creates GitHub deployments for its previews: the same
  workflow.
- Cloudflare Pages without GitHub deployments: the "Cloudflare Pages"
  workflow. It waits for Cloudflare's check on the pull request's commit.
  It needs the Pages project name and the name of that check. If the GitHub
  CLI is logged in, `gh api repos/OWNER/REPO/commits/main/check-runs --jq
  '.check_runs[].name'` lists the check names of the latest commit on
  `main`. Otherwise ask the user to copy the name from a commit's checks on
  GitHub. With a wrong name, the step stops after 5 minutes and prints the
  check names it found on the commit.
- A preview URL that you can build from the pull request number or the
  branch, or a deploy step in the same job that prints the URL: a workflow
  on `pull_request`, as in "When you know the preview URL".
- Anything else with a static build: build and serve the site in the job,
  then run `compare` against `localhost`.
- Other CI systems: install with `install.sh` and run `auditroo compare`
  like a test command.

When you write the workflow:

- Use the GitHub Action `vincentsch/auditroo` with the latest release tag,
  for example `vincentsch/auditroo@v0.1.0`. Check the releases page for the
  current tag. The action installs the auditroo release of the same version.
- Check out the base branch before the auditroo step, not the pull
  request, as the recipes in `docs/ci.md` do. auditroo then reads the
  `auditroo.toml` that was already reviewed. With the pull request's own
  copy, a pull request could add ignore rules that hide its own problems.
  "Ignore rules and pull requests" in `docs/ci.md` explains it. Tell the
  user that changes to the workflow file and `auditroo.toml` need a review.
- Pass values from the event, such as the preview URL, through `env` and
  refer to them as `"$PREVIEW_URL"` in `args`. Never put `${{ ... }}`
  expressions straight into `args`.
- Never write a token into a file. If the preview needs one, tell the user
  which repository secret to create and refer to it as `${NAME}` inside
  single quotes in `--header`, as `docs/ci.md` shows.
- Use the default `--fail-on high` unless the user wants something else.
  `compare` only reports what the change makes worse, so existing problems
  on production do not fail pull requests.
- Do not add `--ignore-robots`. In `compare` it also applies to
  production. auditroo checks pages that only the preview's robots.txt
  blocks without it.

## 5. Ignore findings that the user expects

If the user says a finding is expected, add an ignore rule to
`auditroo.toml` in the project root, with a comment that says why. Make the
rule as narrow as you can, with both a `rule` and a `url`:

```toml
[gate]
ignore = [
  # The blog titles are edited by the marketing team.
  { rule = "title_changed", url = "/blog/*" },
]
```

A `url` without `*`, such as `"/docs/"`, matches only that exact path and
query. "URL patterns" in `docs/configuration.md` has the details.

Never ignore a whole rule such as `noindex_added` or `robots_blocked` for
every URL without the user's clear agreement.

Tell the user two things about the new rule:

- With the base branch checkout from step 4, the rule counts once it is
  merged. Until then the pull request check still sees the finding.
- A medium or low finding, such as `title_changed`, does not fail a run
  with the default `--fail-on high`. The rule then only marks the finding
  as expected. `auditroo explain <rule>` shows a rule's severity and an
  ignore rule for it.

The text output lists ignored findings under "Ignored", so you can check
that the rule matched.

## 6. Check your work and report

If you can, run the same comparison locally once, against a preview URL or
a local build:

```sh
auditroo compare https://www.example.com http://localhost:4321
```

Use the default crawl speed, because `compare` also crawls production.
When you compare two local copies of the site, and the output says "This
result is incomplete, because both sites point their canonical tags or
sitemap at ...", run it again with the `--production-url` it names.

Then tell the user:

- what the first check on production found, with the most important items
  first
- which files you added or changed
- which secrets they still need to create, if any
- how to undo the setup: delete the workflow file and `auditroo.toml`

## Rules

- Crawl only the user's own sites, and use the default speed on
  production.
- Never print or commit tokens and passwords.
- Do not change SEO settings such as robots.txt or canonical tags to make a
  check pass. Report the finding and let the user decide.
- Do not install other tools or change deploy settings without asking.
