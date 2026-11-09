# Portal

The portal is the pool's web face: the JSON API under `/api/v1`, and the static web app that reads it in
the browser. This page covers how the binary builds each API resource and its live stream from one model,
how it serves the app without rendering a page itself, the limits it keeps, and what each page shows. The
code is in [src/portal/](../../../src/portal/), with the country database in [geo.rs](../../../src/geo.rs)
and the group's polling in [group.rs](../../../src/group.rs). The app is [web/](../../../web/). The
[API reference](../api.md) lists every endpoint and field, and [The Web Portal](../frontend.md) is the
operator's page; this one explains the code behind them.

## The Shape

```text
                       [portal] listener (plain HTTP)
                                   |
             +---------------------+----------------------+
             |                                            |
        /api/v1/...                                 every other path
     api.rs router                             Site::response (site.rs)
             |                                            |
   pool.rs  stats.rs  group.rs  health.rs        web/src/, embedded and
   fleet.rs  worker.rs  blocks.rs  balance.rs    assembled once at startup
             |                                            |
   telemetry (memory), store reader, template      static HTML, CSS, JS
             |                                            |
             +------------- JSON, SSE ----> browser <-----+
```

[mod.rs](../../../src/portal/mod.rs) holds `Portal`, everything the portal reads: the configured pools,
telemetry, a store `Reader`, the current template's `watch` channel, node health, the payout standing, the
group's members, the country database, and the assembled `Site`. `listen` binds the `[portal]` address and
serves an axum `Router` that nests the API under `/api/v1` and hands every other path to the site. The
listener speaks plain HTTP; TLS, where a deployment wants it, sits in front.

## The Binary Renders No Page

The binary writes no figure into any HTML. Every page is a static file of the app, and the page's module
in `web/src/js/pages/` reads the API and fills the page in the browser. One app therefore serves both the
pool's own portal and any static host, and the API is the only contract between them.

[site.rs](../../../src/portal/site.rs) embeds `web/src/` in the binary with `rust_embed`. `Site::build`
assembles it once, before any listener opens, so a site that cannot be built stops the pool before a
miner connects. A request never touches the filesystem. Assembly follows [build.py](../../../web/build.py)
rule for rule:

* `<!--#include NAME -->` takes `partials/NAME`, expanded up to five passes so a partial may include
  another. The line break after an empty partial goes with it, so an unused hook leaves no blank line.
* `{{name}}`, `{{title}}`, `{{description}}`, and `{{connect_src}}` are filled from `site.json`,
  HTML-escaped. A key with no value is an error, not an empty string.
* A page gets a canonical link when `site.json` names an `origin`, except the 404 page and any page that
  carries `noindex`: the fleet and worker pages, which are personal to one miner.
* `partials/` is not served, and `config.js` is written with an empty `API`, the site's own origin.

The site's values come from `[portal] site`, or the `site.json` compiled in, and `[portal] overlay` lays a
directory over `web/src/`: an overlay file replaces the one of the same path, or adds a new one. `build.py`
takes the same files as `--site` and `--overlay`, so one site directory brands both. The binary ignores a
site's `api`: its pages always call its own API, so `connect-src` is `'self'`.

The two assemblers must agree byte for byte, and
[test_portal_site.py](../../../tests/e2e/test_portal_site.py) checks every file the binary serves against
`build.py`'s output. A change to one is a change to the other.

`Site::response` serves `/` as `index.html`, any other known path as its file, and anything else as
`404.html` with status 404. It answers GET and HEAD only. Each response carries the headers a meta tag
cannot: `frame-ancestors 'none'`, `Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff`, and a
`Permissions-Policy` that turns off camera, microphone, and geolocation. Files under `vendor/` carry their
version in their names, so they are cached for a year as immutable; everything else is `no-cache`. The
page's own Content-Security-Policy, in `partials/head.html`, allows no inline script or style.

[web/README.md](../../../web/README.md) describes the app: its layout, the `data-bind` attributes that let
each page's HTML say which value it shows, and the rules it keeps.

## One Model per Resource

Each API resource and its stream are built from one model, so a page that loads a resource and then
follows its stream never sees the two disagree.

| Resource | Model | Stream event |
| --- | --- | --- |
| Fleet ([fleet.rs](../../../src/portal/fleet.rs)) | `FleetResource`, with `FleetLive` flattened in | `fleet`: `FleetLive` |
| Worker ([worker.rs](../../../src/portal/worker.rs)) | `Worker`, with its `Live` | `live`: `Live` and `outage` |
| Blocks ([blocks.rs](../../../src/portal/blocks.rs)) | `Block`, for the pool's, a worker's, and a fleet's list | none |
| Group ([group.rs](../../../src/portal/group.rs)) | `GroupTotals`, for the overview card and the endpoint | none |

The resource adds only what a stream must not carry or need not repeat: a fleet's handle, address,
privacy flag, and its ranks on the stats lists; a worker's handle, pool, miner type, and finder terms.

Shared shapes live in [api.rs](../../../src/portal/api.rs). `Rates` sends a hashrate over 1 minute,
10 minutes, 1 hour, and 24 hours, each with its `estimating` flag. `Figure<T>` sends either the figure or
`{"unavailable": reason}`, where `Lacking` names the reason: no template, no 24-hour hashrate, or no shares
in the window. `Portion` sends a fee or part as parts per million or as satoshis. The API sends raw values
only; wording and units are the app's.

[device.rs](../../../src/portal/device.rs) reads one device from two sources: its live values while
telemetry holds it, and what the rollups, fleets, and blocks tables record. A device telemetry no longer
holds shows as offline with its stored best share and last share. Counts are the store's totals plus the
shares telemetry has not written yet, so a share is counted once through writes, disconnects, and
restarts. Hashrates are never blended across the two: 1 and 10 minutes come from telemetry, 1 hour and
24 hours from the rollups.

[figures.rs](../../../src/portal/figures.rs) gives a solo device's odds: the chance of a block over a day,
a month, and a year, and the median and average waits, from its 24-hour hashrate against the current
network difficulty. `Solo` carries them in any pool that can pay a finder, with `finder_payout` set where
the finder gets only part of the reward. [window.rs](../../../src/portal/window.rs) gives a device's or a
fleet's part of its pool's current PPLNS window, and the estimate a block found now would credit it. It
walks the window at the current template's difficulty and keeps each pool's walk for 5 seconds, so pages
and streams do not walk the share journal on every read.

Pools are always listed the same way (`Portal::ordered`): the configured ones in config order, then any no
longer configured, by id, so stored rows of a removed pool still show.

## Live Streams

The fleet and worker pages update over Server-Sent Events: `GET /api/v1/fleets/{fleet}/events` and
`/workers/{handle}/events`. A stream sends an event at once, on each change to a device it follows, and
every 5 seconds unprompted, so hashrates decay and "ago" times advance.

Changes come from telemetry's broadcast channel, which names the device that changed. A worker stream
follows its own handle; a fleet stream follows every member of the fleet. A receiver that falls behind
sends the current values rather than replaying what it missed.

[streams.rs](../../../src/portal/streams.rs) shares the work. `Snapshots` keeps the latest snapshot of
each watched fleet (`FleetSnapshots`) and worker (`WorkerSnapshots`). A snapshot is computed again at most
once a second (`COALESCE`), and every stream of that fleet or worker waiting meanwhile shares the one
computation. Store reads then grow with the number of fleets and workers being watched, not with their
viewers or their shares: a hundred viewers of one fleet cost one read a second. A snapshot nobody asks for in
60 seconds is forgotten. A fleet's snapshot reads its members again, so a device that joins appears without
a reload.

A stream that cannot read the store sends an empty event, and the page keeps its last values.

## Limits and the Client Address

The binary limits only what it alone can see. Request volume is left to the edge, since per-address limits
on ordinary reads would refuse real users behind one carrier or office address.

| Limit | Where | What it protects |
| --- | --- | --- |
| Lookups per client address per minute (`lookups_per_minute`) | `Lookups`, [lookup.rs](../../../src/portal/lookup.rs) | Slows anyone sweeping addresses against common worker names |
| Open streams per client address (`streams_per_ip`, 20 by default) | `StreamCaps`, [streams.rs](../../../src/portal/streams.rs) | Streams are long-lived and the costliest thing the portal serves |
| Open streams in all (`max_streams`, 2,000 by default) | `StreamCaps` | Bounds the memory held by open connections |

The lookup limit counts over a sliding minute and forgets an address once it has been quiet a minute; a
refused lookup is not counted. Over it, the API answers 429 `rate_limited` with `Retry-After`. A stream
over the per-address cap gets 429 `too_many_streams`, and over the global cap 503 `stream_capacity`, both
with `Retry-After: 10`. A `StreamGuard` holds the stream's place and gives it back when the stream drops.
A browser's `EventSource` does not retry a refused stream, so the app opens it again after 10 seconds.

`streams::client` decides whose address a request counts against: the TCP peer, or, when the peer is
`[portal] trusted_proxy`, the last address in `X-Forwarded-For`, the one that proxy added. From any other
peer the header is ignored, so a client cannot choose its own address. IPv4-mapped IPv6 addresses compare
as IPv4.

## Cross-Origin Access and Caching

Without `[api] allowed_origins`, the API sends no CORS header and is same-origin only. With them, a
`CorsLayer` admits exactly those origins, checked as exact origins at startup, for GET, POST, and OPTIONS,
the `Authorization` and `Content-Type` headers, and a 10-minute preflight. It sends no credentials header,
since the API uses no cookies. It exposes `Retry-After`, so a page on another origin can read a refusal's
delay, and a response to an unlisted origin gets no CORS header at all.

`api::json` sets `Cache-Control` per response: `no-cache` for public figures (`Cache::Public`), and
`no-store` (`Cache::Private`) for anything with an address, a balance, or the pool's health, and for
every error.

## Privacy Rules

The portal names miners only by opaque handles, and the code keeps these rules:

* Fleet and worker URLs carry a 16-character handle, never an address or worker name. A malformed or
  unknown handle is 404 alike. The app puts a handle in the fragment, which the browser never sends.
* Only the fleet resource carries the payout address, so its owner can check it. No stream carries it.
* The worker resource carries no address, worker name, or fleet handle. Worker handles are public, in
  block lists among others, so any of these would lead from a block to its finder's fleet. The worker page
  shows a name only if the browser saved it from the fleet page.
* The username lookup (`POST /api/v1/lookup`) takes the username and any fleet password in the body,
  answers only with the fleet handle, and refuses a bare address. A wrong password names a worker that
  never mined, so it finds nothing.
* The balance view takes its token as `Authorization: Bearer`, never in a URL, and names no address. The
  store keeps only the token's SHA-256.
* The stats lists show a fleet by its address's first 6 and last 4 characters, and nothing for a private
  fleet: no whole address, worker name, handle, or link.
* Errors never echo the username, password, token, or address they were given. Nothing in the portal logs
  a request, so no lookup body or token reaches a log.
* The health check never carries error text, which can name internal hosts, ports, and paths, and with
  several nodes gives counts, never which node is out.
* The pool resource names each fee by name and portion, never its address.

## What Each Page Shows

| Page | API it reads | Built in |
| --- | --- | --- |
| `/` overview | `/pool` every 30 s, `/group`, `/lookup` | [pool.rs](../../../src/portal/pool.rs), [group.rs](../../../src/portal/group.rs), [lookup.rs](../../../src/portal/lookup.rs) |
| `/fleet.html#<handle>` | `/fleets/{fleet}`, its `events`, `series`, and `blocks` | [fleet.rs](../../../src/portal/fleet.rs) |
| `/worker.html#<handle>` | `/workers/{handle}`, its `events`, `series`, and `blocks` | [worker.rs](../../../src/portal/worker.rs) |
| `/stats.html` | `/stats` every 30 s | [stats.rs](../../../src/portal/stats.rs) |
| `/blocks.html` | `/blocks`, again when the tip moves | [blocks.rs](../../../src/portal/blocks.rs) |
| `/balance.html` | `/balance`, with a token | [balance.rs](../../../src/portal/balance.rs) |

Every page also polls `/health` every 30 seconds for the outage banner, and a page with a block table
reads it again when the tip height changes.

**Overview.** `Portal::overview` gives the network, version, uptime, chain, each pool's Stratum URLs, kind,
finder part, fees, PPLNS window policy and current size, and live hashrate and device count, the global
fees, the payout terms and standing, blocks found, and the round's best share. The Stratum URLs name
`[public] host`, or a `stratum.` host under a configured domain the request's `Host` falls in, or that
`Host` itself. The SV2 URL carries the authority key, and an announced rotation adds the next key and its
day.

**Fleet.** Every device seen in the last `keep_offline_days`, by 10-minute hashrate, with a summary per
pool: devices online, the four hashrates, best share, shares, blocks, and the odds or window part. Share
and block counts take every device the fleet ever had in the pool, shown or not. The chart series sums the
fleet's devices per pool from the rollups, over `1h`, `24h`, or `7d`; the 7-day range buckets to about a
thousand points. The block list names each finder by worker name, since the fleet knows them.

**Worker.** One device's status, the four hashrates, difficulty, shares, session times, odds or window
part, and, from its last connection, the finder part it asked for and what a block found now would pay it
(`finder_sats`, computed through the same `Rewards` split the coinbase uses).

**Stats.** `Portal::stats` reads memory only: the round since this instance's last block, the top fleets
by best share this round and over the last 24 hours, the top fleets by hashrate now, and connected workers
by miner type, the types after the configured count folded into "Other". Each list's length comes from
`[stats]`.

**Blocks.** Newest first, at most 200 (`?limit=` from 1 to 200), each with its pool, transport, finder
handle, miner type, status, what it paid the finder and the pool wallet, what its window was allocated,
and blocks until its coinbase matures.

**Balance.** Pending, available, reserved, and paid satoshis per pool, for the address the token opens.

## Countries

With `[geo]`, [geo.rs](../../../src/geo.rs) opens an IP-to-country file in the MaxMind DB format. A Stratum
session looks its peer up once, when it connects, and telemetry keeps only the country, never the address.
The stats page lists connected workers by country; a country with fewer than `[geo] min_workers` folds
into "Elsewhere", so a row need not show one worker's hashrate beside where it is. The file is checked
hourly and reopened when it changes, keeping the last good copy if a reopen fails, so a monthly update
needs no restart. `Geo::source` tells DB-IP from MaxMind, so the stats page can carry the credit the
database's licence asks for.

## The Group

Instances can form a group whose overview shows combined figures. [group.rs](../../../src/group.rs)
polls each member's public `GET /api/v1/pool` every refresh and adds up its pools' hashrate and devices
and its blocks found. It reads only a member's own figures, never its group's, so no figure counts twice.
A good answer counts for three refreshes, so one missed poll changes nothing. Each fetch has a 5-second
timeout, follows no redirect, reads at most 64 KiB, and is refused if it reports another network.

[portal/group.rs](../../../src/portal/group.rs) builds `GroupTotals` from this instance's own overview
plus each member counted now. Without `show_members` it names no member; with it, it lists each instance
by its configured label and optional portal link, never by the member's name or API URL.

## Health

[health.rs](../../../src/portal/health.rs) answers `GET /api/v1/health` from memory, never from the
store, so it answers while the store is slow. Its status is `unavailable` when miners get no work, which
returns 503 with the health body, so a monitor can read the HTTP status alone; `degraded` when a node is
out, pooled credit has stopped because the share journal failed, or payouts are paused or halted; and
`ok` otherwise.

## Related

* [API reference](../api.md): every endpoint, field, error, and limit.
* [The Web Portal](../frontend.md): serving, branding, and hosting the app.
* [Telemetry](telemetry.md): the live device state and rollups the portal reads.
* [Accounting](accounting.md): the PPLNS window, balances, and payouts behind the figures.
* [Security](security.md): the portal's place among the pool's other defences.
