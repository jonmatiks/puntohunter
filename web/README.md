# Web Portal

PuntoHunter's web portal, as a static web app. The pool's binary serves it
on its own listener, built in, and any static host can serve a build of it.
Operators fit the portal's pages to their own needs: each
page is plain HTML, so its wording, its layout, and which of the pool's values
it shows change without touching any JavaScript, and the name, colours, and
logo are a few values to replace. [Pages as HTML](#pages-as-html) and
[Rebranding](#rebranding) say how.

It lives in the PuntoHunter repository, and the binary embeds `src/` as it
is: a change here is a change to the pool's portal. The binary assembles the
pages at startup by the rules of `build.py`, in
[src/portal/site.rs](../src/portal/site.rs), and the two change together; the
e2e suite checks that every file the binary serves is the one `build.py`
writes. [Served by the Pool](#served-by-the-pool) says more.

As a static web app for a PuntoHunter mining pool, it lets miners look up
their devices by Stratum username, watch them live, and see the pool's blocks
and their own balance. Everything it shows comes from the pool's
JSON API, `/api/v1`, called straight from the browser. That makes it a working
reference for anyone building their own front end on that API.

It is plain HTML, CSS, and JavaScript modules, with
[uPlot](https://github.com/leeoniya/uPlot) 1.6.32 vendored for the chart. A
small build, Python's standard library only, puts in the shared header and
footer and the pool's API origin. What it writes is a folder of plain files
that any static host serves as they are: Cloudflare, nginx, Caddy, Netlify,
GitHub Pages, an S3 bucket, or anything else.

## Pages

* `/`: the username lookup, pool statistics refreshed every 30 s, the group's
  totals when the pool is in a group, each pool's Stratum URLs and payout rule,
  the payout terms, and the V2 authority key, with the next key and its day
  while a rotation is announced.
* `/fleet.html#<handle>`: every device mining to one address, with a summary per
  pool, live over Server-Sent Events, its place on the stats, a hash-rate chart,
  and the fleet's blocks.
  `/fleet.html` alone opens the fleet this browser saved.
* `/worker.html#<handle>`: one device, live, with its chart and blocks. While
  it is offline, "Forget" hides it from its fleet page in this browser until it
  connects again, and follows "My fleet" where this browser saved one.
* `/stats.html`: the round, the top hashrates and best shares by shortened
  address, miner types, and, with a country database, countries, refreshed
  every 30 s.
* `/blocks.html`: the pool's blocks, refreshed when the chain tip moves.
* `/balance.html`: a miner's balances, opened with the token the operator gave
  them.

Each page is one file at the site's root. A host that drops `.html` from
addresses, as Cloudflare and Netlify do, serves them as `/fleet` and so on;
the fragment stays.

A fleet or worker handle travels in the fragment of the address, after the
`#`. A browser never sends the fragment to the server, so the host serving the
site needs no rewrite rule for it, and never learns which fleet anyone opens.

Every page shows a banner while the pool's health check says miners get no
work.

## Layout

Paths in this README are relative to `web/`.

```text
src/
  index.html       the overview; each other page is a file beside it: fleet.html, …
  404.html         what a host shows for a path that names no file
  partials/        what every page includes: head, header, outage banner, footer;
                   and the hooks a site fills, empty here: intro, about, nav-extra
  brand.css        the brand colour tokens for dark and light themes
  app.css          shared layout and controls
  js/bind.js       fills a page's HTML from the API's data
  js/common.js     what every page runs: theme button, "My fleet", health, footer
  js/api.js        every call to the pool: fetch, errors, live streams
  js/format.js     how figures read: hashrates, odds, BTC, times
  js/chart.js      the hash-rate chart
  js/blocks.js     the blocks tables
  js/copy.js       the Copy buttons
  js/store.js      what this browser keeps: "My fleet", device names, the chart range, the theme
  js/theme.js      applies the saved theme before the page paints
  js/pages/        one module per page: reads the API and fills the page
  vendor/          uPlot and its licence
site.json          the pool's name, description, and API origin; optionally the
                   main page's title and the site's own origin
build.py           builds src/ and site.json into dist/
serve.py           local development server
hosting/           optional settings for Caddy, nginx, and Cloudflare
```

## Pages as HTML

Each page's HTML holds all its markup and wording, and its module in
`js/pages/` holds none: it reads the API and fills the page. The page's
elements say which value of the API's data they show:

```html
<td data-bind="hash_rate.1m" data-format="rate"></td>
<a data-href="/worker.html#{handle}"><span data-bind="name"></span></a>
<p data-if="!devices">No device has mined here in the last 7 days.</p>
<div data-if="rule=pooled">…</div>
<tbody data-each="devices"><template><tr>…</tr></template></tbody>
```

`js/bind.js` lists every attribute. It sets text, never markup, so nothing the
API sends can inject HTML. The text and layout around the fields are free to
change, and a field can show any value the API sends, written by any function
of `js/format.js`. A page's answers to errors, such as the lookup's refusals,
are HTML too, one element per error code.

The pages' Content-Security-Policy allows no inline script, style attribute,
or `on…` attribute, so a page uses classes from `app.css`, and its module adds
any listener.

## Rules the App Keeps

The API sends raw values, and some of what the portal does is up to the
client. This app keeps the same rules, and an app built on this one should too:

* Odds read as odds, never as a forecast. A window estimate is labelled an
  estimate, credited only if the pool finds a block, never a balance. A hashrate
  whose window has not filled shows "–", not a partial figure.
* No worker name, address, username, fleet password, or token ever goes into a
  URL. A lookup sends the username and any fleet password in a POST body, and
  the balance sends its token in an `Authorization` header. None is stored. A
  handle goes only in the fragment, which never leaves the browser.
* The worker page never links to a fleet and shows no address. It shows a
  device's name only if this browser saved it from the fleet page.
* "My fleet", device names, the devices forgotten, the chart range, and the
  theme stay in this browser's `localStorage`. The pool never names "your"
  fleet.
* Text from the API is never inserted as HTML.
* A stream the pool refuses for too many open streams is retried after 10 s.
  The browser reconnects any other dropped stream on its own.
* Every page carries the risk disclaimer.

## Served by the Pool

A PuntoHunter pool serves this app on its `[portal]` listener, as
`http://<host>:8080` in the default config: each file at its own path, `/` the
overview, and any other path the 404 page. It needs no build: the binary holds
`src/` and assembles the pages when it starts. The pages call the pool's own
API, so `site.json`'s `api` does not apply.

Two keys of the pool's config brand it, as `--site` and `--overlay` brand a
build ([A Site Built on This One](#a-site-built-on-this-one)), so one site
directory brands both:

```toml
[portal]
site = "/opt/puntohunter/site/site.json"
overlay = "/opt/puntohunter/site/overlay"
```

The pool reads them at startup, so a change takes a restart. Without them, the
portal is PuntoHunter's own, as `site.json` here is.

## Run Locally

The pool serves the app as its binary was built. To work on it, serve `src/`
as it is now, assembled on every request. Start a PuntoHunter pool, as the
[repository's README](../README.md) says, that admits this app's local origin,
in its config:

```toml
[api]
allowed_origins = ["http://localhost:8000"]
```

Then serve the app against it, from the repository's root:

```sh
python3 web/serve.py --api http://localhost:8080
```

Open <http://localhost:8000>. `--api` takes the pool's origin, such as
`http://192.0.2.10:8080` for one on the LAN, without editing `site.json`.
The server needs only Python 3. It assembles each page as `build.py` would, on
every request, so an edit shows on reload. It does not send the headers in
`hosting/`.

## Build

```sh
python3 web/build.py
```

writes the site to `dist/`. In each page, `<!--#include NAME -->` takes
`src/partials/NAME`, and `{{name}}`, `{{title}}`, `{{description}}`, and
`{{connect_src}}` take their values from `site.json`. `api` in `site.json` is
the pool's API origin, such as `https://pool.example.com`; empty, as
`site.json` ships, means the site's own origin, where the host passes
`/api/v1` through to the pool. `--api` overrides it, and `--host cloudflare`
adds that host's `_headers`.

Two values of `site.json` are optional:

* `title`: the main page's title, in place of `name`.
* `origin`: the site's own origin, such as `https://pool.example.com`. Each
  page that search engines may index then gets its canonical link, in the
  address form of the host: `/stats` with `--host cloudflare`, which drops
  `.html`, and `/stats.html` otherwise. The fleet and worker pages send
  `noindex`, since they are personal to one miner and many, and get none.

### A Site Built on This One

A site keeps its own files outside `web/` and lays them over `src/`, so it
forks no page:

```sh
python3 web/build.py --site ../site/site.json --overlay ../site/overlay
```

`--site` reads that `site.json`. `--overlay` takes a directory shaped like
`src/`: its file replaces `src/`'s of the same path, partials included, and a
file `src/` lacks, such as a page of its own, is added. `serve.py` takes both
options too.

The overview and the header carry hooks for such a site, each an empty partial
here:

* `partials/intro.html`: above the overview, outside its data-bound parts, so
  it reads while the pool cannot be reached.
* `partials/about.html`: below the overview, likewise.
* `partials/nav-extra.html`: links at the end of the header's nav.
* `partials/footer-extra.html`: extra footer content, preserving the shared disclaimer and credits.

`brand.css` loads before `app.css`; an overlay replaces the theme tokens while keeping shared layout
and controls. Accent-filled buttons and selected tabs use `--bg` for readable labels in both themes.

An empty partial leaves no trace, so a build that fills none of them writes
the same pages as one before the hooks.

## Deploy

The pool's own portal needs no deploy ([Served by the Pool](#served-by-the-pool)).
For a site elsewhere, upload `dist/` to any static host. It needs no rewrite rule: every page is a
file of its own. Each page carries its own Content-Security-Policy
and referrer policy in meta tags. A host that can set headers should add what a
meta tag cannot carry, `frame-ancestors`, `X-Content-Type-Options`, and
`Permissions-Policy`, and `hosting/` has them ready:

* `hosting/caddy/Caddyfile`: a site on the pool's own server, with `/api/v1`
  passed through to the pool. Replace its domain and web root with your own.
* `hosting/nginx/site.conf`: the same for nginx, the API pass-through
  optional.
* `hosting/cloudflare/_headers`: for Cloudflare Pages or Workers static
  assets, and Netlify, which reads the same file. Build with
  `--host cloudflare`.

### The pool's API

The pages call the pool's API in one of two ways:

* **On the pool's own origin.** Set `api` in `site.json` to the pool's HTTPS
  origin, since a browser blocks an HTTPS page from calling plain HTTP. The
  pool must list the site's origin, and restart:

  ```toml
  [api]
  allowed_origins = ["https://app.example.com"]
  ```

  The pool then sees each viewer's own address, so its lookup and stream
  limits key on each viewer. Keep the API's hostname DNS-only, or put it
  behind a Cloudflare Tunnel: `cloudflared` reaches the pool from loopback,
  where `[portal] trusted_proxy = "127.0.0.1"` trusts its `X-Forwarded-For`.
  Any other proxy in front of it, such as Cloudflare's without a tunnel,
  would make the pool see the proxy's addresses rather than the viewers'. The
  site itself can sit behind any proxy or CDN.
* **Through the site's host**, for a site on the pool's own server. Leave
  `api` empty, and have the host pass `/api/v1` to the pool, as
  `hosting/caddy/` and `hosting/nginx/` do. The pool must then trust the
  host's `X-Forwarded-For`:

  ```toml
  [portal]
  trusted_proxy = "127.0.0.1"
  ```

## Rebranding

Change `name` and `description` in `site.json`, the colour tokens in
`src/brand.css`, and `src/logo.png`. To keep them out of `web/`, put them in
an overlay ([A Site Built on This One](#a-site-built-on-this-one)), which the
pool's own portal takes too ([Served by the Pool](#served-by-the-pool)).

## License

[MIT](../LICENSE), as the rest of PuntoHunter. uPlot is MIT-licensed too: see `src/vendor/LICENSE-uPlot`.
