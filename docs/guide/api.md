# API Reference

The pool serves its data as JSON under `/api/v1`, on the same listener as its web portal, for a browser
page or any other client to read. The API is versioned by its path: a breaking change gets a new version,
and fields and endpoints are added without one. This page follows the API as it is.

## 1. Conventions

### 1.1 Paths and versioning

* Base path `/api/v1`. A breaking change gets `/api/v2`, and v1 stays served until its known clients move.
  Adding a field or an endpoint is not breaking, and clients must ignore fields they do not know.
* Handles in paths are the same 16 lower-case base32 characters the portal uses. A handle that is malformed
  or unknown returns 404, in the same way.

### 1.2 Values and units

The API returns raw values only. Formatting ("1.06 TH/s", "3 h ago", "1 in 4,000", BTC with eight places) is
the client's job, and section 3 lists the rules it must keep.

| Kind | Encoding | Example |
| --- | --- | --- |
| Hashrate | number, hashes per second | `1.06e12` |
| Difficulty, best share, window work | number, difficulty-1 multiples | `4123456.7` |
| Amount | integer satoshis, field name ending `_sats` | `"paid_sats": 1250000` |
| Moment | integer Unix seconds (UTC), field name ending `_at` | `"last_share_at": 1760261512` |
| Duration | integer seconds, field name ending `_secs` | `"interval_secs": 86400` |
| Fraction, probability | number from 0 to 1 | `0.0123` |
| Fee or reward portion | `{"ppm": n}` (parts per million of the reward) or `{"sats": n}` (a fixed amount) | `{"ppm": 9000}` |
| Enumerations | lower-case snake strings | `"confirmed_immature"` |

Amounts below 21 million BTC fit exactly in a JSON number (under 2^53), so satoshis are never sent as
strings.

### 1.3 Hash-rate windows

A hashrate is always sent with its `estimating` flag:

```json
{ "hash_rate": 1.06e12, "estimating": false }
```

`estimating` is true while the window has not yet filled since the device started mining here, so the
average covers only part of it and reads low. Where the portal shows four windows, they come as one object
keyed by window:

```json
"hash_rate": {
  "1m":  { "hash_rate": 1.10e12, "estimating": false },
  "10m": { "hash_rate": 1.06e12, "estimating": false },
  "1h":  { "hash_rate": 1.04e12, "estimating": false },
  "24h": { "hash_rate": 0.98e12, "estimating": true }
}
```

`1m` and `10m` come from in-memory telemetry and are current to the second. `1h` and `24h` come from the
stored rollups, one per `[telemetry] rollup_secs` period (a minute in the shipped configs), and advance at
the end of each period.

### 1.4 Figures that may be unavailable

Some figures need the node's current template, a hashrate, or shares in a window. Where one cannot be
computed, its object is replaced with a reason code, so the client can say why:

```json
"window": { "unavailable": "no_template" }
```

| Reason | Meaning |
| --- | --- |
| `no_template` | The node gives no work right now (an outage). |
| `no_hash_rate` | The device has no 24-hour hashrate yet, so it has no odds. |
| `no_shares` | The pool's window holds no shares yet. |

A figure that does not apply at all is `null`. For example, `solo` is `null` on a pooled device and `window`
is `null` on a solo one.

### 1.5 Errors

Every error is JSON with a stable code and an English message meant for developers, not end users:

```json
{ "error": "not_found", "message": "no worker or fleet has that handle" }
```

| HTTP | `error` | When |
| --- | --- | --- |
| 400 | `bad_request` | Malformed JSON, or a query parameter out of range |
| 400 | `invalid_username` | The lookup's username does not parse |
| 400 | `worker_name_required` | The lookup's username is a bare address |
| 401 | `unauthorized` | The balance token is missing, or opens no balance |
| 404 | `not_found` | Unknown or malformed handle; no worker under the looked-up username; unknown path |
| 429 | `rate_limited` | Too many lookups; `Retry-After` header and `retry_after_secs` in the body |
| 429 | `too_many_streams` | This client address already holds `streams_per_ip` open streams; `Retry-After` |
| 503 | `stream_capacity` | The pool already holds `max_streams` open streams; `Retry-After` |
| 500 | `internal` | The store could not be read |

An error never echoes the username, fleet password, token, or address it was given. `GET /api/v1/health` is
the one exception to this format: its 503 carries the health body of section 2.10, not an error object.

### 1.6 Caching

| Response | `Cache-Control` |
| --- | --- |
| Anything carrying an address or a balance | `no-store` |
| `GET /api/v1/health` | `no-store`, so no cache between a monitor and the pool answers for it |
| `GET /api/v1/pool`, blocks, series | `no-cache` |
| SSE streams | `no-cache` (axum's default for SSE) |

### 1.7 Logging

Nothing under `/api/v1` logs a request, so lookup bodies and balance tokens never reach a
log.

### 1.8 Limits

The binary limits only what it alone can see. Traffic volume is limited at the edge.

| Limit | Where | Why |
| --- | --- | --- |
| Lookups per client address, per minute (`lookups_per_minute`, 30 in the shipped configs) | binary | A privacy control: it slows guessing worker names against addresses. |
| Open streams per client address (`streams_per_ip`, default 20) | binary | Streams are long-lived and the costliest resource. A proxy counts requests, not open streams. |
| Open streams in all (`max_streams`, default 2,000) | binary | Bounds memory: at 20–60 KB per connection, 2,000 streams stay under about 120 MB. |
| Request rate and volumetric abuse | edge | Cloudflare's proxy, or Caddy's rate-limit module (not in Caddy's standard build). |

```toml
[portal]
lookups_per_minute = 30
streams_per_ip = 20
max_streams = 2000
# Unset without a reverse proxy. With one, the only peer whose X-Forwarded-For is trusted.
trusted_proxy = "127.0.0.1"
```

* A stream over the per-address cap is refused with `429 too_many_streams`, and one over the
  global cap with `503 stream_capacity`, each with `Retry-After`. A browser's `EventSource` does not retry a
  refused stream on its own, so the client retries after the delay.
* No other endpoint is limited in the binary. Mobile carriers and office networks put many users behind one
  address, so per-address limits on ordinary reads would refuse real users, while a flood spreads across
  addresses. Reads are cheap, the more so since a fleet's figures are computed at most once a second and
  shared by every reader. The balance token is 32 random bytes and cannot be guessed, so the balance endpoint
  needs no limit.
* Behind a proxy, every per-address limit keys on the last entry of `X-Forwarded-For`, the one the proxy
  added, and only when the request comes from `trusted_proxy`. From any other peer the header is ignored, so a
  client cannot choose its own address. Without `trusted_proxy`, the peer's address counts. When
  Cloudflare sits in front of Caddy, Caddy resolves the client address from Cloudflare's ranges, so the binary
  still trusts one proxy.

## 2. Endpoints

### 2.1 `GET /api/v1/pool`

The pool at a glance: what the overview page shows, naming no one. Poll it every 30 s, as the overview
refreshes.

```json
{
  "network": "main",
  "version": "v0.2.0+1a2b3c4d",
  "started_at": 1760000000,
  "uptime_secs": 261512,
  "outage": false,
  "chain": { "tip_height": 967926, "network_difficulty": 1.27e14 },
  "authority_key": "<base58check key>",
  "next_authority_key": null,
  "pools": [
    {
      "id": "solo",
      "name": "Solo",
      "kind": "solo",
      "stratum_v1": "stratum+tcp://pool.example.com:3333",
      "stratum_v2": "stratum2+tcp://pool.example.com:3334/<authority key>",
      "finder": { "ppm": 1000000 },
      "miner": { "ppm": 985000 },
      "fees": [
        { "name": "operator", "portion": { "ppm": 9000 } },
        { "name": "partner", "portion": { "ppm": 6000 } }
      ],
      "window": null,
      "personal": null,
      "worker_names": false,
      "worker_names_opt_in": false,
      "hash_rate": 2.1e12,
      "devices": 3
    },
    {
      "id": "my-bonus",
      "name": "Finder bonus",
      "kind": "hybrid",
      "stratum_v1": null,
      "stratum_v2": "stratum2+tcp://pool.example.com:6334/<authority key>",
      "finder": { "sats": 50000000 },
      "miner": { "ppm": 990000 },
      "fees": [{ "name": "host", "portion": { "ppm": 10000 } }],
      "window": { "window_multiplier": 4.0, "window_min_work": 0.0, "window_size": 5.08e14 },
      "personal": null,
      "worker_names": false,
      "worker_names_opt_in": false,
      "hash_rate": 0.0,
      "devices": 0
    },
    {
      "id": "my-split",
      "name": "Your split",
      "kind": "personal",
      "stratum_v1": null,
      "stratum_v2": "stratum2+tcp://pool.example.com:6335/<authority key>",
      "finder": { "ppm": 333300 },
      "miner": { "ppm": 990000 },
      "fees": [{ "name": "operator", "portion": { "ppm": 10000 } }],
      "window": { "window_multiplier": 2.0, "window_min_work": 0.0, "window_size": 2.54e14 },
      "personal": { "min": { "ppm": 0 }, "max": { "ppm": 500000 } },
      "worker_names": false,
      "worker_names_opt_in": true,
      "hash_rate": 1.1e12,
      "devices": 1
    }
  ],
  "reward": {
    "miner": { "ppm": 985000 },
    "fees": [
      { "name": "operator", "portion": { "ppm": 9000 } },
      { "name": "partner", "portion": { "ppm": 6000 } }
    ]
  },
  "accounting": { "available_after_confirmations": 101 },
  "payouts": {
    "threshold_sats": 100000,
    "interval_secs": 86400,
    "standing": { "state": "running" },
    "last": {
      "batch": 12,
      "state": "confirmed",
      "payees": 7,
      "paid_sats": 1250000,
      "fee_sats": 1840,
      "reserved_at": 1760250000
    }
  },
  "blocks_found": 4,
  "round_best_share": 3.4e9
}
```

* `chain` is `null` during an outage, and so is each pool's `window.window_size`.
* `next_authority_key` is `null` unless the operator has announced a new authority key:
  `{ "key": "<base58check key>", "switch_on": "YYYY-MM-DD" }`, the key V2 miners pin from that day, in UTC.
  Until then they keep `authority_key`: a miner pinned to the new key is refused before that day.
* `round_best_share` is the best share any worker reached this round, since this instance's last block, as
  `GET /api/v1/stats` lists the round; `null` before one.
* `pools` lists the configured pools in config order. `kind` is `solo`, `pooled`, `hybrid`, or `personal`, as
  the pool's `finder` and `personal` make it, for wording only: they decide what a block pays. A Stratum URL
  is `null` for a protocol the pool does not serve.
* `personal` is the range each worker may choose its finder part in, by a `+` option on its username, each
  bound a portion; `null` where every worker gets `finder`. On a personal pool, `finder` is the part of a
  worker that chooses none.
* `worker_names` says the pool's blocks name every worker, and `worker_names_opt_in` each worker that adds
  `+name` to its username; never both.
* `finder` is what a block pays its finder, capped at the miner portion: `{ "ppm": n }` for a part of the
  reward, `{ "sats": n }` for a fixed amount. A fee's `portion` takes the same two forms.
* A pool's `fees` are its own, or the global ones when it lists none. `reward` gives the global fees.
* `window` is the pool's PPLNS policy and its size W now, in difficulty-1 units of weighted work: each share
  counts its work times what its job leaves to the pool. It is `null` in a pool that pays its finder the whole
  miner portion, which has no window.
* A pool's `miner` and `reward.miner` are `null` when one of their fees is a fixed amount, since the miner
  portion then differs per block.
* `payouts` is `null` without `[payouts]`: balances accrue but are not paid yet. `last` is `null` before the
  first batch, and `reserved_at` is `null` on batches from before it was recorded.
* `standing.state` is `starting`, `running`, `deferred`, `paused`, `halted`, or `locked`. `deferred` carries a
  `reason`, as the overview shows it today. `locked` means a batch is reserved and waits for the operator to
  unlock the payout wallet to sign it.
* Stratum hosts come from `[public].host`, or, when that is unset, from the request's `Host` header, with
  `stratum.` before the domain it is or is under when that domain is in `[public].domains`. An app on
  another origin should set one of them, because an external app's request names the API's host, which may
  differ from where miners connect.

### 2.2 `POST /api/v1/lookup`

The username lookup. The username and any fleet password travel in the body, never in a URL.

Request (`Content-Type: application/json`):

```json
{ "username": "bc1q....garage-2", "password": "garage-pass" }
```

Response `200`:

```json
{ "fleet": "k3m2q7abcdxyz234" }
```

The username is parsed as `mining.authorize` parses it, so an upper-case bech32 address matches, and pasted
options are ignored. If that worker has mined here in any pool, the response is the handle of its fleet: the
private fleet of its fleet password, given in `password` or after a `#` in the username, or its address's open
fleet without one. `password` is optional; one that is empty, `x`, or shorter than 8 characters, after its `d=N`
parts, counts as none, as a V1 device's does. A wrong password is answered as an unknown username. It returns
`400 invalid_username`, `400 worker_name_required`, `404 not_found`, or `429 rate_limited` otherwise, under a
per-IP limit.

### 2.3 `GET /api/v1/fleets/{fleet}`

Every device that has mined to the fleet's address, with a summary per pool. A device offline for more than
`keep_offline_days` drops off, as on the page.

```json
{
  "fleet": "k3m2q7abcdxyz234",
  "address": "bc1q...",
  "private": false,
  "ranks": { "best_share": 2, "best_share_24h": 7, "hash_rate": null },
  "outage": false,
  "chain": { "tip_height": 967926, "network_difficulty": 1.27e14 },
  "pools": [
    {
      "pool": "solo",
      "name": "Solo",
      "online": 2,
      "total": 3,
      "hash_rate": { "1m": {}, "10m": {}, "1h": {}, "24h": {} },
      "best_share": 4123456.7,
      "accepted": 5120,
      "rejected": 3,
      "last_share_at": 1760261512,
      "blocks": 0,
      "solo": {
        "odds": {
          "day": 0.0000123,
          "month": 0.000369,
          "year": 0.00448,
          "median_wait_secs": 4.8e9,
          "average_wait_secs": 6.9e9
        },
        "estimating": false,
        "best_vs_network": 0.0000325,
        "finder_payout": false
      },
      "window": null
    }
  ],
  "devices": [
    {
      "handle": "a7b3c2d4e5f6g7h2",
      "name": "garage-2",
      "miner": "bitaxe",
      "pool": "solo",
      "transports": ["V2-native"],
      "status": "online",
      "last_seen_at": 1760261515,
      "connections": 1,
      "hash_rate": { "hash_rate": 1.06e12, "estimating": false },
      "best_share": 4123456.7,
      "last_share_at": 1760261512,
      "accepted": 2210,
      "window": null
    }
  ],
  "keep_offline_days": 7
}
```

* `address` is the fleet's payout address, so its owner can check which address the devices pay. It is in
  this response alone, never in a list, stream, series, block row, error, header, or URL. It is `null` for a
  fleet recorded before addresses were.
* `private` is `true` for a private fleet, whose devices set a fleet password, and `false` for an address's open
  fleet, which anyone who knows the address can open. The page of an open fleet says how to make it private.
* `chain` is the pool resource's, `null` during an outage. The page shows its network difficulty and block
  height in each pool's summary, below the best difficulty. The API sends it once, since every pool has the
  same value.
* `pools` lists only the pools the shown devices mine in: the configured ones in config order, then any no
  longer configured, by id, whose `name` is its id. In each `hash_rate` window object, `{}` above stands for a
  rate object as in section 1.3.
* `devices` is ordered by 10-minute hashrate, highest first, then by name. `name` is `""` for a device mining
  with the bare address. `miner` is `null` when the device named none. `status` is `online`, `idle`, or
  `offline`. `connections` counts its open connections: more than one when several devices mine with one
  username, which then share one row and its figures, and the page says so.
* `keep_offline_days` is how many days an offline device stays listed, the operator's `[portal]
  keep_offline_days`, 7 by default.
* `solo` is present on a pool that pays a finder. Where the pool also pays a pool output, `finder_payout` is
  `true`: the odds are those of the finder's payout. `odds` may be `{ "unavailable": ... }`, and
  `best_vs_network` is `null` during an outage.
* `window` is present on a pool that pays a pool output, and on its devices:

  ```json
  "window": { "pool": "pooled", "fraction": 0.0123, "estimate_sats": 3870000 }
  ```

  `estimate_sats` is what a block found now would credit. It is an estimate and never a balance
  (section 3). Where workers keep different parts, the next block's pool output depends on who finds it, so
  the estimate takes the expected one: the miner portion times the window's weighted work over its plain
  work, times `fraction`, the part of the weighted window held.
* Both are `null` on a pool no longer configured.
* `last_share_at` is `null` before the first share. `accepted` and `rejected` count every share since the device
  first mined here, through restarts; a pool's are its devices' summed, every device the fleet ever had in it,
  as `blocks` are, whether or not it is still listed.
* `ranks` is the fleet's place, 1 for the first, among every fleet on each list of
  `GET /api/v1/stats` (section 2.12): its best share this round, over the last 24 hours, and its hashrate
  now; `null` where it has none. Like `address`, it is not in the stream.

### 2.4 `GET /api/v1/fleets/{fleet}/events`

Server-Sent Events. One `fleet` event goes out at once, then again on any change to one of the fleet's devices,
and at least every 5 s so that hashrates decay and "ago" figures advance. Its data is the fleet resource above
without `fleet` and `address`. The address is never in a stream.

```text
event: fleet
data: {"outage":false,"chain":{"tip_height":967926,"network_difficulty":1.27e14},"pools":[...],"devices":[...]}
```

The fleet is read again for each event, so a device that joins appears on its own. Streams count against the
limits of section 1.8.

### 2.5 `GET /api/v1/workers/{handle}`

One device. This response carries no payout address, worker name, or fleet handle: worker
handles are public (the blocks list names each finder by handle), so any of them would lead to a finder's fleet.

```json
{
  "handle": "a7b3c2d4e5f6g7h2",
  "pool": "solo",
  "transports": ["V2-native"],
  "miner": "bitaxe",
  "part": null,
  "finder_sats": 312500000,
  "name_written": false,
  "name_asked": false,
  "outage": false,
  "live": {
    "status": "online",
    "last_seen_at": 1760261515,
    "connections": 1,
    "hash_rate": { "1m": {}, "10m": {}, "1h": {}, "24h": {} },
    "difficulty": 1024.0,
    "best_share": 4123456.7,
    "chain": { "tip_height": 967926, "network_difficulty": 1.27e14 },
    "last_share_at": 1760261512,
    "accepted": 2210,
    "rejected": 1,
    "blocks": 0,
    "connected_since_at": 1760180000,
    "first_seen_at": 1759900000,
    "solo": { "odds": {}, "estimating": false, "best_vs_network": 0.0000325, "finder_payout": false },
    "window": null
  }
}
```

* `difficulty` is the share target last assigned, `null` while disconnected. `connected_since_at` is `null`
  while disconnected, and `first_seen_at` is `null` for a device recorded before it was.
* `chain` is the pool resource's, `null` during an outage. The page shows its network difficulty and block
  height below the best difficulty.
* `transports` holds the open connections' transports, or the last one's when none is open.
* `accepted` and `rejected` count every share since the device first mined here, through restarts, as its
  fleet's row does.
* `part` is the finder part the worker's last connection asked for, in portion form, or `null` for its pool's
  own. `finder_sats` is what a block found now would pay its address, its part clamped to the pool's range
  and the dust rules applied; `null` without a template, or in a pool that pays no finder or is no longer
  configured.
* `name_written` says whether its pool writes its worker name into the blocks it finds: under
  `worker_names`, or under `worker_names_opt_in` when it asked with `+name`. The name itself is never sent.
* `name_asked` says whether its last connection asked with `+name`. Under `worker_names_opt_in`, a worker that
  asked but whose name is not written mines as a bare address, with no name to write; a pool with neither flag
  writes no names, so its `+name` writes nothing.
* `{}` above stands for the rate and odds objects shown in section 2.3.
* The client computes uptime as now minus `connected_since_at`.

### 2.6 `GET /api/v1/workers/{handle}/events`

Server-Sent Events. One `live` event goes out at once, on each change to the worker, and at least every 5 s.
Its data is the `live` object above, plus `outage`. Streams count against the limits of section 1.8.

```text
event: live
data: {"outage":false,"status":"online",...}
```

### 2.7 Series: `GET /api/v1/workers/{handle}/series` and `GET /api/v1/fleets/{fleet}/series`

Chart data from the rollups, with `?range=1h|24h|7d` (default `24h`; anything else is `400 bad_request`). A
worker's response is one series. A fleet's is its devices summed per pool, `{"pools": [...]}`, each entry
the series below with the pool's `pool` id and `name`, in the order of the fleet resource's `pools`. A
worker's series has this shape:

```json
{
  "since": 1760175000,
  "until": 1760261460,
  "period": 60,
  "start": [1760175000, 1760175060],
  "hash_rate": [1.05e12, 1.07e12],
  "accepted": [6, 7],
  "rejected": [0, 1],
  "best": [812345.0, 1023456.0]
}
```

Buckets are `period` seconds: the rollup period for `1h` and `24h`, and about 10 minutes for `7d`. A bucket with
no row is absent from the arrays, and the chart draws it as zero. The data only changes at the end of a bucket,
so the client refetches just after `until + period` instead of polling.

### 2.8 Blocks: `GET /api/v1/blocks`, `/api/v1/workers/{handle}/blocks`, `/api/v1/fleets/{fleet}/blocks`

The newest blocks first, up to `?limit=` (1–200, default 200; the portal shows 200). Poll every 30 s, as the
portal does.

```json
{
  "blocks": [
    {
      "height": 967926,
      "hash": "00000000000000000001a2b3...",
      "pool": "solo",
      "transport": "V2-native",
      "finder": "a7b3c2d4e5f6g7h2",
      "miner": "bitaxe",
      "status": "confirmed_immature",
      "finder_sats": 312500000,
      "pool_sats": null,
      "allocated_sats": null,
      "blocks_to_maturity": 42
    }
  ]
}
```

* `status` is `submitted`, `rejected`, `confirmed_immature`, `mature`, or `orphaned`.
* `finder` is the finder's worker handle. On the fleet's list only, each row also carries `finder_name`, the
  worker name the fleet knows (`""` for a bare-address device), as the fleet page shows it.
* `pool` is the pool's id, which a block keeps after its pool leaves the config.
* `transport`, `finder`, `finder_sats`, and `pool_sats` are `null` on blocks recorded before those were
  noted. `finder_sats` or `pool_sats` is `null` where the block paid no such output, as on a pooled block's
  finder or a solo block's pool. `allocated_sats` is `null` until the block's window is allocated, and always
  on a block with no pool output.
* `blocks_to_maturity` is `0` once mature, and `null` off the chain or during an outage.

### 2.9 `GET /api/v1/balance`

The private balance view. The token goes in a header, never in a URL:
`Authorization: Bearer <token>`.

```json
{
  "pools": [
    { "pool": "pooled", "pending_sats": 0, "available_sats": 41200, "reserved_sats": 0, "paid_sats": 1250000 },
    { "pool": "my-bonus", "pending_sats": 4900000, "available_sats": 0, "reserved_sats": 0, "paid_sats": 0 }
  ]
}
```

`pools` holds a balance per pool the address has one in: the configured pools in config order, then any no
longer configured, by id. The threshold of `[payouts]` applies to the sum over every pool. The response is
`no-store` and names no address. It returns `401 unauthorized` with `WWW-Authenticate:
Bearer` for a missing token or one that opens nothing.

### 2.10 `GET /api/v1/health`

Whether the pool is serving, for uptime monitors and for a status banner in the portal or an app. It reads
memory only, never the store, so it is cheap to poll every few seconds.

```json
{
  "status": "degraded",
  "version": "v0.1.5+fb8d1c55",
  "uptime_secs": 261512,
  "tip_height": 967926,
  "checks": {
    "node": "healthy",
    "work": true,
    "pooled_credit": "accepting",
    "payouts": "paused"
  }
}
```

| `status` | HTTP | When |
| --- | --- | --- |
| `ok` | 200 | Miners get work, and no check below is failing |
| `degraded` | 200 | Miners get work, but a node of the fleet is out, pooled credit is stopped, or payouts are paused or halted |
| `unavailable` | 503 | Miners get no work |

* `tip_height` is the chain tip the current template builds on, as `GET /api/v1/pool` gives it, or `null`
  without a template. The portal refreshes its block tables when it changes, about once a block, instead
  of polling them.
* `node` is `starting`, `healthy`, `degraded`, `ibd`, `outage`, or `wrong_chain`. With secondary nodes it is
  the fleet's: `healthy` while every node is fit, `degraded` while work continues with a node out, and the
  primary's code while there is no work. The metrics endpoint's `refused`, `hung`, and `failed` all read
  `outage` here.
* `nodes`, with secondary nodes only, is how many are fit of how many are configured, as
  `{ "fit": 2, "configured": 3 }`; it never says which.
* `work` is whether the pool has a template to give miners now. The `outage` flag of the other resources is
  its negation.
* `pooled_credit` is `accepting` or `stopped`: `stopped` while the share journal cannot be written, when the
  pool stops crediting pooled work, so pooled and hybrid miners learn why their work earns nothing. It is
  `null` when no pool pays a pool output.
* `payouts` is the `standing.state` of `GET /api/v1/pool`, or `null` without `[payouts]`. Only `paused` and
  `halted` degrade the status, since both wait for the operator to reconcile. `starting` (reconciling after a
  start), `deferred` (for example, by the fee-rate cap), and `locked` (a batch waiting for the operator to
  unlock an encrypted payout wallet) are the payout policy working as designed.
* A monitor can use the HTTP status alone. Any answer at all means the process is up and the portal
  listener is bound.
* The response carries codes only, never the underlying error text: a node or wallet error can name internal
  hosts, ports, and file paths. The operator reads the detail from Prometheus and the logs. Balances,
  liabilities, the fee reserve, and the wallet stay on the metrics endpoint.
* `deploy.sh` keeps checking `puntohunter_ready`. Its question is whether the new binary started, and a node
  hiccup during a deploy must not roll back a good binary.

### 2.11 `GET /api/v1/group`

The group's totals: this instance's figures plus those of each member of `[group]`
whose last good answer is fresh. Poll it every 30 s, as the overview refreshes.

```json
{
  "name": "PuntoHunter Group",
  "instances": 3,
  "reporting": 2,
  "hash_rate": 4.2e12,
  "devices": 14,
  "blocks_found": 5
}
```

* `instances` counts this instance and its members; `reporting` those whose figures are in the totals, this
  instance always among them.
* `hash_rate` and `devices` are the sums of each counted instance's pools, as `GET /api/v1/pool` gives them,
  and `blocks_found` the sum of theirs.
* A member is read from its `GET /api/v1/pool`, never its `GET /api/v1/group`, so no figure counts twice. A
  member counts for three refreshes after its last good answer: a pool resource on this instance's network.
* Without `[group] show_members` it names no member, and carries no member's URL or figures.
* With it, `members` lists each instance: this one first, `"this": true`, then each member in
  config order, by its `label`, with its portal's origin as `link` when one is set. `reporting` says whether
  its figures count now, and `hash_rate`, `devices`, and `blocks_found` are its own while they do and `null`
  while they do not. A member's name and URL are never given.

  ```json
  "members": [
    { "label": "East", "link": null, "this": true, "reporting": true,
      "hash_rate": 1.2e12, "devices": 4, "blocks_found": 1 },
    { "label": "Asia", "link": "https://asia.example.com", "this": false, "reporting": false,
      "hash_rate": null, "devices": null, "blocks_found": null }
  ]
  ```

* It returns `404 not_found` without `[group]`.

### 2.12 `GET /api/v1/stats`

The pool stats, public, from
the model of the portal's stats page, `/stats.html`, which reads it every 30 s. Poll it no more often.

```json
{
  "round": { "started_at": 1792300000, "since_block": 972019 },
  "best_shares": {
    "round": [
      { "short_address": "bc1qw5…f3t4", "best": 2.1e11, "miner": "bitaxe", "pool": "solo", "at": 1792351200 }
    ],
    "last_24h": [
      { "short_address": "bc1q7k…2m4c", "best": 3.4e9, "miner": "NerdQAxe++", "pool": "solo", "at": 1792390800 }
    ]
  },
  "hash_rates": [
    { "short_address": "bc1qw5…f3t4", "hash_rate": 4.2e12, "estimating": false, "workers": 3 }
  ],
  "miners": [
    { "kind": "type", "miner": "bitaxe", "workers": 12, "hash_rate": 1.4e13, "best": 2.1e11 },
    { "kind": "other", "miner": null, "workers": 2, "hash_rate": 2.0e12, "best": 4.0e9 },
    { "kind": "unknown", "miner": null, "workers": 1, "hash_rate": 0.0, "best": 0.0 }
  ],
  "countries": [
    { "kind": "country", "country": "TR", "name": "Türkiye", "workers": 5, "hash_rate": 6.0e12 },
    { "kind": "elsewhere", "country": null, "name": null, "workers": 2, "hash_rate": 2.1e12 }
  ],
  "country_min_workers": 1,
  "country_source": "dbip",
  "limits": { "best_shares_round": 10, "best_shares_24h": 10, "hash_rates": 10, "miner_types": 10, "countries": 10 },
  "fleets": 4,
  "workers": 15
}
```

* `round.started_at` is when the round's block was found, or the instance's first share before the first
  block, and `null` before any share; `since_block` is that block's height, `null` before the first. A round
  starts at the newest block a node accepted, in any pool; one the node refuses starts nothing, and an
  orphaned one still started its round.
* Each list holds at most its `limits` rows, in rank order. A best-share row is a fleet's best this round, or
  over the current hour and the 23 before it, the block's solution share left in the round it ends: its
  difficulty, the miner type of the worker that reached it (`null` if it named none), its pool, and when, to
  the minute. A hash-rate row is a fleet's connected workers summed over 10 minutes, `estimating` while every
  one of them is.
* `short_address` is the fleet's address cut to its first 6 and last 4 characters, and nothing else of it is
  given: no whole address, worker name, or handle. It is `null` for a private fleet, which the page
  shows as "private fleet".
* A worker is one username in one pool, as `GET /api/v1/pool`'s `devices` counts them: devices sharing a
  username are one worker.
* `miners` counts the connected workers, so its rows sum to the pools' `devices`: the first
  `limits.miner_types` types by hashrate, grouped regardless of case and spelt as most of their workers spell
  them, then `other` for the rest, then `unknown` for workers that named none. `best` is the type's best share
  this round, 0 before one.
* `countries`, present only with `[geo]`, counts them by the country of their newest connection: the first
  `limits.countries` with `country_min_workers` workers or more (`[geo] min_workers`, 1 by default), by
  workers, then `elsewhere` for the rest, then `unknown` for addresses the database places in no country.
  `country_min_workers` is present only with `[geo]`, and so is `country_source`, who made the country
  database, for the credit its licence asks for: `dbip`, `maxmind`, or `other`.
* `limits` gives each list's length, as `[stats]` sets it, 10 by default. A fleet's `ranks` (section 2.3)
  count every fleet, whatever is shown.
* `fleets` and `workers` are the fleets and workers connected now, which the hashrates and miner types
  cover.

## 3. Client Responsibilities

The portal, [web/](../../web/), is itself a client of this API: it does some things in the browser, and some
of its wording is required. It keeps these rules, and any other app must too:

* Wording: show odds as odds, never as a forecast. Label `estimate_sats` as an estimate,
  credited only if the pool finds a block, and never as a balance. Show a hashrate as "–" (not as the low
  partial number) while `estimating` is true.
* Privacy: never link a worker view to its fleet, and never put a worker name or address in a URL.
  The portal shows a worker's name on its worker page only from what this browser saved on the fleet page. An
  app may do the same.
* Saved state: the fleet handle for a "My Fleet" link, worker names by handle, the chart range, and the theme
  stay in the client. The server never names "your" fleet.
* Times: send Unix seconds, show them in the viewer's time zone.
* Status labels: `confirmed_immature` reads "confirmed, immature"; `offline` reads "offline, last seen …"
  from `last_seen_at`.
* Copy buttons for Stratum URLs and the authority key, and the username advice the overview gives: that a
  device's default worker name such as `bitaxe` is the first name anyone would guess.
* The risk disclaimer every portal page carries in its footer.
* Live updates: reconnect a dropped `EventSource` (the browser does it on its own) and keep the last values on
  screen meanwhile.
