# The Web Portal

A PuntoHunter pool shows miners their devices on its web portal, [web/](../../web/): plain HTML, CSS,
and JavaScript modules over the pool's JSON API, with uPlot vendored for the chart. The pool serves it
itself, built into the binary with nothing else to install, and any static host can serve a build of
it too. Either way, an operator shapes it to their needs: its name, colours, wording, and pages.

## The Pool's Own Portal

The pool serves the portal on the `[portal]` listener, plain HTTP, port 8080 in the default config,
beside the JSON API, `/api/v1` ([API reference](api.md)). Each page reads everything it shows from the
API in the browser, so the pages need JavaScript.

| Page | What it shows |
| --- | --- |
| `/` | The username lookup, the pool's statistics, the group's totals when the pool is in a group, a setup card per pool with its Stratum URLs, payout rule, and fees, the payout terms, the miner settings, and the V2 authority key |
| `/fleet.html#<handle>` | Every device mining to one address, live, with a card per pool, a hash-rate chart, and the fleet's blocks |
| `/worker.html#<handle>` | One device, live, with its odds or window share, its chart, and its blocks |
| `/stats.html` | The round, the top hashrates and best shares, miner types, and countries with a country database |
| `/blocks.html` | The pool's blocks |
| `/balance.html` | A miner's balances, opened with a token the operator issues |

A fleet or worker handle travels in the fragment, after the `#`, which a browser never sends to the
server. A path that names no page gets the portal's 404 page. The fleet and worker pages update live
over Server-Sent Events, and [Mining](mining.md) describes what they show. The portal is dark by
default; the theme button in the header switches it to light or to the system's theme, and the browser
remembers the choice.

`[portal]` takes:

* `bind`: the listen address, such as `0.0.0.0:8080`.
* `lookups_per_minute`: the username lookups each client address may make a minute.
* `streams_per_ip` and `max_streams`: the API's live streams each client address may hold open, 20 by
  default, and the pool in all, 2,000 by default.
* `keep_offline_days`: the days an offline device stays on its fleet page, 7 by default.
* `trusted_proxy`: behind a reverse proxy, its address, the only peer whose `X-Forwarded-For` names
  the client. Without it, a proxy's viewers would share the proxy's limits.
* `site` and `overlay`: the portal's brand (below).

### Its Brand

Without `site` and `overlay`, the portal carries PuntoHunter's own name and look. `site` names a
`site.json`, and `overlay` a directory laid over `web/src/`: the files `build.py --site` and
`--overlay` read ([Customize It](#customize-it)), so one site directory brands the pool's own portal
and a static build alike.

```toml
[portal]
site = "/opt/puntohunter/site/site.json"
overlay = "/opt/puntohunter/site/overlay"
```

The pool reads both once, at startup, so a change takes a restart, and it does not start with a site it
cannot use ([configuration](configuration.md)). Its pages always call its own API, so `site.json`'s
`api` is ignored. An overlay's files are served as the portal's own: a script in one runs on every
page, so it keeps the [rules an app keeps](#rules-an-app-keeps).

## On a Static Host

The same app, built by `web/build.py`, runs on any static host, such as a site on a domain of its own
that reads the pool's API from there. It is also a working reference for any frontend built on the API.
Its pages are the pool's own, at the same paths; a host that drops `.html` from addresses, as
Cloudflare and Netlify do, serves them as `/fleet` and so on.

[web/README.md](../../web/README.md) describes the app in full; this section is a summary.

### Run It Locally

Start a pool whose config admits the app's local origin:

```toml
[api]
allowed_origins = ["http://localhost:8000"]
```

Then, from the repository's root:

```sh
python3 web/serve.py --api http://localhost:8080
```

and open `http://localhost:8000`. `--api` takes the pool's origin, such as one on the LAN. The
server needs only Python 3, and assembles each page on every request, so an edit shows on reload,
where the pool's own portal reads `web/src/` only when it starts. `--port` changes its port.

### Build It

```sh
python3 web/build.py
```

writes the site to `web/dist/`, plain files that any static host serves as they are. The build fills
each page from `web/site.json`:

| Key | Required | Meaning |
| --- | --- | --- |
| `name` | yes | The pool's name, in the header and the titles |
| `description` | yes | The pages' meta description |
| `api` | no | The pool's API origin, such as `https://api.pool.example.com`; empty or left out for the site's own origin |
| `title` | no | The main page's title, in place of `name` |
| `origin` | no | The site's own origin, such as `https://pool.example.com`, which gives each page search engines may index its canonical link |

`--api` overrides `api`, `--out` writes elsewhere, and `--host cloudflare` adds the `_headers` file
Cloudflare and Netlify read. The fleet and worker pages are kept out of search, so they get no
canonical link.

### Customize It

Paths here are in `web/`.

* **Rebrand:** change `name` and `description` in `site.json`, the colour tokens at the top of
  `src/brand.css`, for the dark theme and the light one, and `src/logo.png`, or put them in a site of
  your own (below), which the pool's `[portal] site` and `overlay` read too.
* **Wording and layout:** each page's HTML holds all its markup and text, and its module in
  `src/js/pages/` only reads the API and fills it in. An element names the value it shows with
  `data-bind`, `data-if`, and related attributes, which `src/js/bind.js` lists, so the text and
  layout around the values change without touching any JavaScript. The pages'
  Content-Security-Policy allows no inline script or style attribute.
* **A site of your own:** keep your files outside `web/` and lay them over it, so you fork no page:

  ```sh
  python3 web/build.py --site ../site/site.json --overlay ../site/overlay
  ```

  `--site` reads your `site.json`. `--overlay` takes a directory shaped like `web/src/`: each of its
  files replaces the file of the same path, partials included, and a file `web/src/` lacks, such as
  a page of your own, is added. `serve.py` takes both options too.
* **Hooks:** three partials, empty in `web/`, are there for an overlay to fill:
  * `partials/intro.html`, above the overview, outside its data-bound parts, so it reads while the
    pool cannot be reached.
  * `partials/about.html`, below the overview, likewise.
  * `partials/nav-extra.html`, links at the end of the header's navigation.

## Hosting It

Upload `web/dist/` to any static host. It needs no rewrite rule: every page is a file of its own.
Each page carries its own Content-Security-Policy and referrer policy. A host that can set headers
should add what a page cannot carry itself, and [web/hosting/](../../web/hosting/) has them ready:

* [hosting/caddy/Caddyfile](../../web/hosting/caddy/Caddyfile): a site on the pool's own server,
  with `/api/v1` passed through to the pool.
* [hosting/nginx/site.conf](../../web/hosting/nginx/site.conf): the same for nginx, the pass-through
  optional.
* [hosting/cloudflare/_headers](../../web/hosting/cloudflare/_headers): for Cloudflare Pages,
  Workers static assets, and Netlify. Build with `--host cloudflare`.

Replace their domain, `pool.example.com`, and web root with your own.

### Reading the API

The pages call the pool's API in one of two ways:

* **On the site's own origin, through a proxy.** Leave `api` empty, and have the host pass `/api/v1`
  to the pool, as the Caddy and nginx files do. The pool must trust the proxy's `X-Forwarded-For`,
  so that its lookup and stream limits key on each viewer:

  ```toml
  [portal]
  trusted_proxy = "127.0.0.1"
  ```

* **On a hostname of its own**, such as `https://api.pool.example.com`. Set `api` in `site.json` to
  that origin, and list the site's origin in the pool's config, then restart the pool:

  ```toml
  [api]
  allowed_origins = ["https://pool.example.com"]
  ```

  `allowed_origins` takes exact origins, none by default, so the API is same-origin only. The API's
  origin must be HTTPS, since a browser blocks an HTTPS page from calling plain HTTP, and the portal
  listener speaks plain HTTP, so it sits behind something that terminates TLS. The pool must see
  each viewer's address: keep the hostname DNS-only, or put it behind a Cloudflare Tunnel, whose
  `cloudflared` reaches the pool from loopback, where `trusted_proxy = "127.0.0.1"` trusts its
  `X-Forwarded-For`. `trusted_proxy` names one address, so a CDN's proxy, which forwards from many,
  would make every viewer look like the CDN. The site itself can sit behind any proxy or CDN.

### Cloudflare Workers

A site whose domain is on Cloudflare can be served as Workers static assets. Build with
`--host cloudflare`, then write a `wrangler.jsonc` beside the build that serves `dist/` and answers
a missing file with `404.html`. How the Worker attaches to the domain follows how the site reads the
API:

* **The API on a hostname of its own:** attach the Worker by Custom Domain, which makes it the
  origin of every path on the domain.

  ```json
  {
    "name": "<worker name>",
    "compatibility_date": "2026-10-01",
    "assets": { "directory": "dist", "not_found_handling": "404-page" },
    "routes": [{ "pattern": "pool.example.com", "custom_domain": true }]
  }
  ```

* **The API on the site's own origin:** attach the Worker by route, since a Custom Domain would
  leave `/api/*` no way to the pool. The domain's record points at the pool (such as through a
  Cloudflare tunnel), the Worker runs on `pool.example.com/*`, and a second route,
  `pool.example.com/api/*` with no Worker, added in the dashboard, sends the API on to the pool:
  when two routes match, the more specific wins.

  ```json
  {
    "name": "<worker name>",
    "compatibility_date": "2026-10-01",
    "assets": { "directory": "dist", "not_found_handling": "404-page" },
    "routes": [{ "pattern": "pool.example.com/*", "zone_name": "example.com" }]
  }
  ```

Deploy with `npx wrangler deploy`, after `npx wrangler login` once. Deploy the site again after a
change to `web/`, and after an engine upgrade that changes the API, so the app matches the API it
reads.

## Rules an App Keeps

The API sends raw values, and some of what the portal does is up to the client. The portal keeps
these rules, and an app built on it, or on the API alone, should too:

* Odds read as odds, never as a forecast. A window estimate is labelled an estimate, credited only
  if the pool finds a block, never a balance. A hashrate whose window has not filled shows "–", not
  a partial figure.
* No worker name, address, username, fleet password, or token goes into a URL. A lookup sends the
  username and any fleet password in a POST body, and the balance page sends its token in an
  `Authorization` header; none is stored. A handle goes only in the fragment.
* The worker page never links to a fleet and shows no address. It shows a device's name only if this
  browser saved it from the fleet page.
* "My fleet", device names, forgotten devices, the chart range, and the theme stay in the browser's
  `localStorage`. The pool never names "your" fleet.
* Text from the API is never inserted as HTML.
* A stream the pool refuses for too many open streams is retried after 10 s.
* Every page carries the risk disclaimer: finding a block is probabilistic, and no block or reward
  is ever guaranteed.
