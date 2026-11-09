# JavaScript sites

auditroo reads the HTML that your server sends. Static sites and sites with
server rendering put everything auditroo needs into that HTML, so they work
without any setup.

Some single-page apps send an almost empty HTML page and build the rest in
the browser with JavaScript. For those sites auditroo can load each page in
headless Chrome first, which is Chrome without a window, and read the page
after its scripts ran. This is slower and needs Chrome or Chromium on your
computer.

## Do you need it?

Open a page of your site with `curl` and look for the title and the
canonical tag. The canonical tag tells Google which URL to show for the
page.

```sh
curl -s https://www.example.com/ | grep -iE '<title|rel="canonical"'
```

If both are there, you do not need rendering. If they are missing, try a
rendered crawl.

## Get a build with rendering

The release binaries do not include rendering. Build auditroo from source
with the `browser` feature. You need Rust 1.89 or newer:

```sh
cargo install --locked --features browser --git https://github.com/vincentsch/auditroo
```

auditroo does not download a browser. It starts the Chrome or Chromium it
finds on your system. When auditroo does not find it, set `AUDITROO_CHROME`
to the browser's path:

```sh
export AUDITROO_CHROME=/usr/bin/chromium
auditroo check https://www.example.com --render-js
```

You can also set `render_js = true` under `[crawl]` in `auditroo.toml`.

On the `ubuntu-latest` runners of GitHub Actions, Chrome is already
installed at `/usr/bin/google-chrome`. For repeatable CI runs, pin the
browser version yourself.

## What changes with rendering

- Each page loads on its own, without cookies from other pages.
- After the page has loaded, auditroo waits until its URL has not changed
  for half a second. Then it reads the page as the browser sees it.
- robots.txt and sitemaps are still fetched as plain files.
- When a script changes the URL, for example with `history.pushState`, the
  new URL counts as the page's final URL.

A snapshot stores whether it was rendered. `diff` stops with exit code 2
when the baseline and the new crawl differ in this setting, because rendered
and raw HTML are too different to compare. In `compare`, both sites use the
same setting.

## Load on your site

The browser loads everything a page needs, including scripts and images.
`--concurrency` and `--rate-limit-ms` only limit the pages that auditroo
loads, not those extra requests. With the default concurrency of 8, up to 8
pages load at the same time, each with all its resources. Lower
`--concurrency` when that is too much for your server.

`--timeout-ms` is the time limit for one whole page in the browser,
including scripts. `--max-body-bytes` limits the size of the rendered HTML.

Headers from `--header` go only to the start URL's host, also when the
browser loads resources from other hosts.
