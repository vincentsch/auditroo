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
explains each rule id and what to do about it. The text output shows the
rule id under each finding that fails the run, for example
`rule: canonical_external`. For findings below the fail-on level, add
`--out report.json` and read the `rule` field of each finding. Do not change
the site to fix findings unless the user asks you to.

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
- Anything else with a static build: build and serve the site in the job,
  then run `compare` against `localhost`.
- Other CI systems: install with `install.sh` and run `auditroo compare`
  like a test command.

When you write the workflow:

- Use the GitHub Action `vincentsch/auditroo` with the latest release tag,
  for example `vincentsch/auditroo@v0.1.0`. Check the releases page for the
  current tag. The action installs the auditroo release of the same version.
- When the project has an `auditroo.toml`, add `actions/checkout` before the
  auditroo step, so the job reads it. `docs/ci.md` shows how.
- Pass values from the event, such as the preview URL, through `env` and
  refer to them as `"$PREVIEW_URL"` in `args`. Never put `${{ ... }}`
  expressions straight into `args`.
- Never write a token into a file. If the preview needs one, tell the user
  which repository secret to create and refer to it as `${NAME}` inside
  single quotes in `--header`, as `docs/ci.md` shows.
- Use the default `--fail-on high` unless the user wants something else.
  `compare` only reports what the change makes worse, so existing problems
  on production do not fail pull requests.

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

Never ignore a whole rule such as `noindex_added` or `robots_blocked` for
every URL without the user's clear agreement.

## 6. Check your work and report

If you can, run the same comparison locally once, against a preview URL or
a local build:

```sh
auditroo compare https://www.example.com http://localhost:4321
```

Use the default crawl speed, because `compare` also crawls production.

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
