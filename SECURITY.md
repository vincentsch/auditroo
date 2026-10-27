# Reporting a security issue

Use [GitHub private vulnerability reporting](https://github.com/vincentsch/auditroo/security/advisories/new)
for security reports. Do not put exploit details or private data such as
tokens in public issues.

Include the auditroo version and your operating system. Then describe the
steps that reproduce the problem. Use example domains and fake tokens in
examples.

## What auditroo protects

auditroo crawls websites and writes reports. These are the places where a
mistake could leak data or harm a system.

### Custom headers

Values from `--header` and `[crawl] headers` often hold bypass tokens.
auditroo sends them only to the exact host of the start URL. It drops them
when a redirect goes to another host or from HTTPS to HTTP. With
`--render-js`, the browser also sends them only to that host. `${NAME}`
values are read from the environment, so tokens need not appear in files or
command lines. Error messages do not repeat a start URL that contains a
password.

### Reports and snapshots

They contain page data such as URLs and titles, plus any values that custom
checks extract. Log reports contain samples of User-Agent strings. Treat
these files as private when the site is private.

### HTML reports

The report is a single file without scripts. Its content security policy
blocks scripts and remote loads, and page data in the report is escaped.

### Commands from the config

Monitoring hooks and quality sources can run local commands named in
`auditroo.toml`. They run without a shell, with a timeout. Only use config
files you trust.

### The installer

`install.sh` checks the SHA-256 checksum of the download before it replaces
a binary. Release binaries also have build provenance attestations from
GitHub. These are signed records that show that GitHub Actions built the
binary from this repository.

## Network access

auditroo has no telemetry. It connects to the sites you crawl and to the
sitemaps they point to. With monitoring, it also calls the webhooks you set
up.

## Supported versions

Security fixes go into the latest release.
