# Quickstart

This page shows how to install auditroo and run a first check on your own
site.

## Install

On Linux and macOS, and on Windows in Git Bash:

```sh
curl -fsSL https://raw.githubusercontent.com/vincentsch/auditroo/main/install.sh | sh
```

The installer downloads the release binary for your system and checks its
SHA-256 checksum against `checksums.txt` from the same release. It installs
to `~/.local/bin`. If that folder is not on your PATH, the installer prints
the command that adds it.

You can change two things with environment variables:

| Variable | Effect |
| --- | --- |
| `INSTALL_DIR` | Install somewhere else, for example `INSTALL_DIR=/usr/local/bin`. |
| `AUDITROO_VERSION` | Install a fixed release, for example `AUDITROO_VERSION=v0.1.0`. Without it you get the latest release. |

To see that it works:

```sh
auditroo --version
```

### Other ways to install

Every release has these binaries, plus a `checksums.txt` with their SHA-256
sums:

| System | File |
| --- | --- |
| Linux x86_64 | `auditroo-linux-amd64` |
| Linux arm64 | `auditroo-linux-arm64` |
| macOS Intel | `auditroo-darwin-amd64` |
| macOS Apple silicon | `auditroo-darwin-arm64` |
| Windows x86_64 | `auditroo-windows-amd64.exe` |
| Windows arm64 | `auditroo-windows-arm64.exe` |

The Linux binaries are statically linked, so they run on any distribution,
including Alpine. On Windows without Git Bash, download the `.exe` and
compare its hash in PowerShell with `Get-FileHash -Algorithm SHA256` before
you run it.

To build from source you need Rust 1.88 or newer:

```sh
cargo install --locked --git https://github.com/vincentsch/auditroo
```

To run it in Docker, build the image from a checkout:

```sh
docker build -t auditroo .
docker run --rm auditroo check https://www.example.com
```

## Run the demo

```sh
auditroo demo
```

The demo serves a small coffee shop site on `127.0.0.1`, together with a
preview of it. The preview has seven typical deploy mistakes and one title
change that is fine. Then the demo compares the preview with production, the
same way `auditroo compare` does with real sites. Nothing leaves your
computer.

The output starts with a summary of what would hurt search. Then it lists
the findings by severity, and the ones below the fail-on level come last.
Under each finding that fails the run, a line says what to do. Each finding
also has a line that names its rule, for example `rule: noindex_added`. You
need that name when you want to ignore a finding.
Add `--html demo.html` to write the same report as a web page.

The demo itself exits with code 0, so you can run it anywhere. A real
`compare` with these findings exits with code 1 and fails your build.

## Check your own site

```sh
auditroo check https://www.example.com
```

`check` looks at one site on its own, without anything to compare with. It
crawls the site from the start page. It also reads robots.txt, the file that
tells crawlers which pages they may fetch, and the sitemaps, the XML files
that list your pages for search engines. Then it reports the problems that
one crawl can show:

- internal links to pages that return errors or redirect several times
- sitemap URLs that are broken or blocked by robots.txt
- sitemap URLs that redirect or have noindex
- canonical tags that point to another host or to a page that does not work
- JSON-LD that is not valid JSON
- pages without a title or without an H1 heading
- a start page with noindex, or robots.txt rules that block Googlebot

A canonical tag tells Google which URL to show for a page. noindex tells
Google to leave a page out of its search results. JSON-LD is the structured
data that Google reads for rich results, such as product prices.

On a larger site, start with a limit:

```sh
auditroo check https://www.example.com --max-pages 500
```

auditroo waits 100 ms between two requests to the same host, which is about
10 pages a second. On a preview or a local build you can turn the wait off
with `--rate-limit-ms 0`. `compare` uses the same setting for production, so
leave the default there unless your production server can take more.
[Crawling your own site](own-site.md) explains how to let auditroo through a
firewall or bot protection.

The first `check` of a site often finds old problems. Fix the ones that
matter, and ignore the rest in `auditroo.toml` so they do not fail every
build. [Configuration](configuration.md) shows how.

## Compare a preview with production

```sh
auditroo compare https://www.example.com https://my-branch.example.vercel.app
```

`compare` crawls both sites with the same settings and matches pages by
path. It reports what got worse on the preview:

- pages that now have noindex
- pages that now return errors or redirect
- canonical tags that changed
- titles and headings that changed or went missing
- JSON-LD that changed or went missing
- robots.txt rules that block pages Google could crawl before
- URLs that left the sitemap
- pages that can no longer be reached through links from the start page
- new problems that `check` would find on the preview but not on production

Problems that production already has are not reported, so the result is
only about the change you are testing.

The preview can be on any host, including `http://localhost:4321` while you
develop. If the preview still uses the production domain, for example in
its sitemap or canonical tags, auditroo reads those URLs on the preview,
under the same path. URLs on any other host, such as a staging server, are
reported.

## Compare with a saved snapshot

When you have no production site to compare with, for example before a
migration, save a snapshot first:

```sh
auditroo snapshot https://www.example.com --out baseline.json
# ... deploy the change ...
auditroo diff https://www.example.com --baseline baseline.json
```

`diff` reports the same changes as `compare`. The snapshot is a JSON file
with what auditroo read from each URL, such as the status code and the
canonical tag. It does not contain the full HTML.

## Next steps

- [CI recipes](ci.md) to run `compare` on every pull request
- [Findings](findings.md) for what each finding means and what to do
- [Commands](commands.md) for every flag
