# Crawling your own site

auditroo is meant for sites you run. This page shows how to let it through
a firewall or a login, and how fast it should crawl.

## How auditroo identifies itself

auditroo sends this User-Agent header with each request:

```text
auditroo/0.1.0 (+https://github.com/vincentsch/auditroo)
```

auditroo follows the rules in robots.txt, the file where a site tells
crawlers which pages they may fetch. It uses the rules for the `auditroo`
user agent, or the rules for `*` when there are none for it. It does not
pretend to be a browser or Googlebot, and it does not try to get around bot
protection. On your own site, you can let it through as described below.

## Let auditroo through a firewall

Bot protection such as Cloudflare's often blocks crawlers or shows them a
challenge page. A blocked crawl shows up as many `403` findings, or as a
login page instead of your content. The fix is a secret header that your
firewall lets through:

1. Make a long random token, for example with `openssl rand -hex 32`.
2. Store it as a CI secret, for example `AUDITROO_TOKEN`.
3. Add a firewall rule that lets requests with that header through.
4. Send the header with each auditroo run.

```sh
auditroo check https://www.example.com --header 'x-auditroo-token: ${AUDITROO_TOKEN}'
```

Or put it in `auditroo.toml`:

```toml
[crawl]
headers = [
  { name = "x-auditroo-token", value = "${AUDITROO_TOKEN}" },
]
```

auditroo reads `${AUDITROO_TOKEN}` from the environment, so the token never
appears in the file or on the command line. It sends the header only to the
host of the start URL. When a redirect goes to another host, or from HTTPS
to HTTP, the header is dropped.

In Cloudflare, the firewall rule is a custom rule with the action Skip.
Match it on the header, for example with this expression:

```text
any(http.request.headers["x-auditroo-token"][*] eq "your-token")
```

Then pick the protections that the rule turns off. Other firewalls have
similar rules. Allowing the IP addresses of CI runners works less well,
because hosted runners change their addresses.

## Previews behind a login

For a staging site with HTTP basic authentication, send the authorization
header. Put `user:password` in base64 into a secret first:

```sh
export STAGING_AUTH=$(printf '%s' 'user:password' | base64)
auditroo compare https://www.example.com https://staging.example.com \
  --header 'authorization: Basic ${STAGING_AUTH}'
```

For Vercel Deployment Protection, see [CI recipes](ci.md#previews-behind-vercel-deployment-protection).

## Staging sites that block all crawlers

A staging site often serves a robots.txt with `Disallow: /`, so that search
engines stay away. auditroo then stops at the start page, because auditroo
also follows robots.txt. Use `--ignore-robots` to crawl your own staging
site anyway. auditroo still reads robots.txt and checks it for Googlebot.

In a `compare`, auditroo reports a robots.txt like that as
`robots_blocked`, because the same file in production would block Google.
A better setup serves the production robots.txt on staging and sends an
`X-Robots-Tag: noindex` header instead. That HTTP header tells search
engines not to index the page, the same as a noindex meta tag. Pass
`--ignore-noindex-header` so that auditroo ignores the header. Vercel and
Netlify already do it this way, and auditroo ignores the header on their
preview domains without the flag.

## How fast to crawl

By default auditroo fetches 8 URLs at the same time and waits 100 ms between
two requests to the same host. That is about 10 pages a second.

- For `check` on a preview or a staging site, you can use
  `--rate-limit-ms 0`. There are no visitors there, so the crawl can go
  faster.
- On production, leave the default unless you know the server can take
  more. `compare` also crawls production with the same setting, so leave
  the default for `compare`.
- To check part of a large site, use `--max-pages` or `--include`.

auditroo crawls the start URL's host and its `www` twin, for example
`example.com` and `www.example.com`. It does not follow links to other
subdomains or other sites.
