# Troubleshooting

## Every page on my preview has noindex

Preview hosts add an `X-Robots-Tag: noindex` header to their previews. That
header tells search engines to leave a page out of their results, so that
previews do not show up in Google. `compare` and `diff` ignore it on
`*.vercel.app`, `*.netlify.app` and `*.pages.dev` hosts. When your previews
run on your own domain, or when you run `check` on a preview, pass
`--ignore-noindex-header`. auditroo then still checks robots meta tags in
the HTML, which is where a real mistake would be.

## auditroo checked only one page

Run `auditroo check` with the same start URL and read the first findings.
The usual causes are:

- robots.txt, the file with the rules for crawlers, blocks auditroo. If it
  blocks only auditroo and not Googlebot, auditroo stops with exit code 2
  and says so. On your own site, add `--ignore-robots`. `compare` and `diff`
  still crawl the pages that production's robots.txt, or the baseline's,
  allowed.
- The start page redirects to another host, for example from `example.com`
  to `shop.example.com`. Start the crawl at the final URL. auditroo follows
  links to the `www` twin of the start host, but not to other subdomains.
- The links only exist after JavaScript runs. See [JavaScript sites](javascript.md).
- A firewall answers with `403` or a challenge page. See
  [Crawling your own site](own-site.md).

## "cannot be used for the comparison"

auditroo stops when the site it compares with gives it nothing to compare.
Its start page returns an error such as `403`, or it never ends on a page
of the site, for example because it redirects to a login service.

- In `compare`, that is production, and the exit code is 3.
- In `diff`, that is the baseline file. The exit code is 2, and you need to
  take a new snapshot.
- In `monitor`, the run reports it as an error for that site.

`snapshot` refuses to save such a crawl for the same reason. A firewall or a
login is the usual cause. [Crawling your own site](own-site.md) shows how to
let auditroo through.

## check reports canonical tags on another host for a local build

`check` treats the start URL as the live site. A local build or a preview
usually contains production URLs in its canonical tags and its sitemap, so
`check` reports them as another host. Tell auditroo which live site the
build belongs to:

```sh
auditroo check http://localhost:4321 --production-url https://www.example.com
```

URLs on the production host are then read on the build, under the same
path. Better still, compare the build with production, which also shows what
changed:

```sh
auditroo compare https://www.example.com http://localhost:4321
```

Either way, a host that is neither production nor the build, such as a
staging server, is still reported.

## Two local builds show almost nothing

When you compare two builds that both contain the live site's URLs, for
example a build of `main` and a build of a pull request, every canonical tag
names a host that auditroo does not know. It then treats no page as one that
Google may index, so most rules have nothing to look at. The first line
says `FAIL (incomplete)` or `PASS (incomplete)`, and a line above the
findings names the host to pass:

```text
This result is incomplete, because both sites point their canonical tags or sitemap at https://www.example.com, which is neither of them. If both are copies of that site, run again with --production-url https://www.example.com.
```

The JSON report has the host in `suggested_production_url`. Name the live
site, and both builds count its URLs as their own. Set `production_url` in
the `[crawl]` section of `auditroo.toml`, or pass the flag:

```sh
auditroo compare http://localhost:4000 http://localhost:4001 --production-url https://www.example.com
auditroo snapshot http://localhost:4000 --production-url https://www.example.com --out main.json
auditroo diff http://localhost:4001 --baseline main.json --production-url https://www.example.com
```

In `compare` the setting applies to both builds. When production is the
live site itself, it changes nothing on that side.

## Many pages return 403 or 429

A firewall or rate limiter is blocking auditroo. Let it through with a
secret header as described in [Crawling your own site](own-site.md). For
`429`, crawl slower with a higher `--rate-limit-ms` or a lower
`--concurrency`.

## compare reports pages as removed that exist on the preview

`compare` fetches every production URL on the preview by its path. When the
preview does serve those paths, check these settings:

- Both crawls stop at `--max-pages`. On a site with more URLs than that,
  the two crawls can see different parts of the site. Raise the limit or
  narrow the crawl with `--include`.
- The preview needs a header or login that production does not. A `401`
  shows up as broken pages. When every page redirects to a login page, the
  redirects are reported, as critical when half or more of the site goes
  there. [Crawling your own site](own-site.md) shows how to let auditroo
  in.

## The crawl never ends

Calendars and search pages can create endless URLs. Leave them out:

```sh
auditroo check https://www.example.com --exclude '/search*' --exclude '/*?*'
```

`/*?*` leaves out every URL with a query string. The default limit of
10,000 URLs stops the crawl in any case. When a crawl hits the limit,
auditroo says so in the report. It then does not report pages that no link
leads to, because it cannot know about links on pages it did not see.

## Timeouts

auditroo gives up on a request after 10 seconds. Slow pages show up as
`fetch_failed` or as `broken_link` with a timeout message. Raise the limit
with `--timeout-ms 30000`, or lower `--concurrency` so the server gets fewer
requests at once.

## Exit code 2

Something is wrong with the command or `auditroo.toml`. The message names
the flag or key. Run the command with `--help` to see what it expects, or
see [Configuration](configuration.md).

## Exit code 3

auditroo could not finish, for example because the start page did not load
or a file could not be written. The message says what failed and why. A
run with exit code 3 has no findings, so CI should treat it as a failure.

## The installer says the checksum does not match

The download was damaged or cut off. Run the installer again. If it still
fails, download the binary and `checksums.txt` from the release page and
compare the SHA-256 sum by hand.

## Still stuck?

Open an issue with the command you ran and its output. Add the version that
`auditroo --version` prints. Remove tokens and private URLs first.
