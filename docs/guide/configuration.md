# Configuration

One TOML file configures a pool instance: `puntohunter serve --config <file>`. The pool reads it once at
startup and validates every value before it binds a port. A refused value stops the pool with the key it
names, such as `invalid config pool.toml: vardiff.floor: must be a positive number`. A change takes effect
at the next restart.

Every table refuses keys it does not know, so a misspelt key, or a key from an older release, stops the
pool rather than being ignored. The source of truth is [src/config.rs](../../src/config.rs). The
repository's [config/mainnet.toml](../../config/mainnet.toml) and
[config/regtest.toml](../../config/regtest.toml) are worked examples, each key commented.

## A Short Example

A mainnet instance with a solo pool and a hybrid pool, one fee output, and the portal on the LAN:

```toml
network = "main"                     # "main" or "regtest"
data_dir = "/opt/puntohunter/data"   # the database, the keys, and found blocks

[node]                               # the primary node, on this host
rpc_url = "http://127.0.0.1:8332"
cookie_file = "/var/lib/bitcoind/.cookie"
zmq_hashblock = "tcp://127.0.0.1:28332"
rpc_timeout_secs = 10.0
longpoll_timeout_secs = 120.0

[[pools]]                            # solo: the finder takes the whole miner portion
id = "solo"
name = "Solo"
v1 = "0.0.0.0:3333"
v2 = "0.0.0.0:3334"
finder = { percent = 100 }

[[pools]]                            # hybrid: a third to the finder, the rest shared by work
id = "hybrid"
name = "Hybrid"
v1 = "0.0.0.0:5333"
v2 = "0.0.0.0:5334"
finder = { percent = 33.33 }

[[fees]]                             # paid by every block of every pool without fees of its own
name = "operator"
address = "bc1q…"
percent = 1.0

[addresses]
pool_wallet = "bc1q…"                # takes what finders leave; required even with solo pools only

[accounting]                         # the PPLNS window of pools that share
window_min_work = 0.0

[public]
host = "pool.example.com"            # what the portal tells miners to connect to

[stratum]
authorize_timeout_secs = 60.0

[vardiff]
floor = 1.0
ceiling = 1_000_000.0
share_interval_secs = 10.0

[telemetry]
rollup_secs = 60

[portal]
bind = "0.0.0.0:8080"
lookups_per_minute = 30

[metrics]
bind = "127.0.0.1:9100"              # keep on loopback
```

## Required and Optional Sections

A config must have `network`, `[node]`, at least one `[[pools]]` entry, `[stratum]`, `[metrics]`,
`[portal]`, `[vardiff]`, `[telemetry]`, `[addresses]`, and `[accounting]`. Every other section is
optional: `[[secondary_nodes]]`, `[public]`, `[api]`, `[coinbase]`, `[[fees]]`, `[payouts]`, `[group]`,
`[geo]`, `[stats]`, and `[backup]`.

Durations are in seconds and take any positive number, such as `10` or `0.5`; zero is refused. Where the
tables below say "integer", a fraction is refused.

## Top-Level Keys

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `network` | `"main"` or `"regtest"` | required | The chain the pool mines. Every address in the config must be on it, and every node must report it. |
| `data_dir` | path | `data/<network>`, such as `data/main` | Holds the database, `sv2-authority.key`, `handle.key`, and `blocks/`. Created on first run. |

A relative `data_dir`, like every relative path in the config, is read against the pool's working
directory. Testnet and signet are not supported.

## `[node]`

The primary node: the one on the pool's own host, and the one that holds the payout wallet where payouts
are configured.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `rpc_url` | string | required | The node's RPC URL, such as `http://127.0.0.1:8332`. |
| `cookie_file` | path | none | The node's `.cookie` file. |
| `rpc_user` | string | none | An RPC user, with `rpc_password` or `rpc_password_file`. |
| `rpc_password` | string | none | That user's password, in the config. |
| `rpc_password_file` | path | none | A file holding that user's password. |
| `zmq_hashblock` | string | required | The node's ZMQ `hashblock` endpoint, such as `tcp://127.0.0.1:28332`. |
| `rpc_timeout_secs` | seconds | required | The timeout of every RPC call but the long poll. |
| `longpoll_timeout_secs` | seconds | required | The timeout of the long-polled `getblocktemplate`. |
| `stuck_secs` | seconds | 30 | How long a healthy node's tip may have less chain work than the fleet's best before it counts as stuck. |
| `recovery_secs` | seconds | 60 | How long a node out of a fault must pass its checks before it sources templates again. |

Credentials take one of three forms: `cookie_file` alone, `rpc_user` with `rpc_password`, or `rpc_user`
with `rpc_password_file`. Any other combination is refused. A password file must hold a password and be
readable by its owner alone (`chmod 600`), or the pool refuses to start. The cookie file is not checked at
startup: it is read on every call, because the node writes a new one each time it starts.

`stuck_secs` and `recovery_secs` apply to every node of the fleet, primary and secondary alike.

## `[[secondary_nodes]]`

The other nodes of the fleet, in order, each an entry of its own. Without any, the primary is the whole
fleet. With them, a block template comes from a fit node, and a found block goes to every node at once.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `name` | string | required | The node's name in logs and metric labels: 1 to 32 lower-case letters, digits, and `-`. |
| `rpc_url` | string | required | As in `[node]`. |
| `cookie_file` | path | none | As in `[node]`. |
| `rpc_user` | string | none | As in `[node]`. |
| `rpc_password` | string | none | As in `[node]`. |
| `rpc_password_file` | path | none | As in `[node]`. |
| `zmq_hashblock` | string | required | As in `[node]`. |
| `rpc_timeout_secs` | seconds | `[node]`'s | As in `[node]`. |
| `longpoll_timeout_secs` | seconds | `[node]`'s | As in `[node]`. |

Each name must be unique and never `primary`, which is `[node]`'s. Credentials take the same three forms
as `[node]`'s. [Installation](installation.md) says how to reach a node on another host safely.

## `[[pools]]`

One entry per pool, in the order the portal lists them. At least one is required. [Pools and
modes](pools-and-modes.md) explains how the finder part, fees, and PPLNS window fit together.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `id` | string | required | 1 to 32 lower-case letters, digits, and `-`; never `fleet`. Keys the pool's blocks, balances, window, and worker handles. |
| `name` | string | the `id` | What the portal calls the pool: 1 to 64 characters. |
| `v1` | address and port | none | The Stratum V1 listener, such as `0.0.0.0:3333`. |
| `v2` | address and port | none | The Stratum V2 listener, such as `0.0.0.0:3334`. |
| `public_v1` | port | `v1`'s port | The V1 port miners reach, when a proxy or port forward differs from the listener. |
| `public_v2` | port | `v2`'s port | The V2 port miners reach, likewise. |
| `finder` | portion | required | What a block's finder is paid of its reward, capped at the miner portion. |
| `personal` | table | none | A range each worker may choose its own finder part in. |
| `fees` | list of fee tables | the global `[[fees]]` | The pool's own fee outputs, in place of the global ones; `fees = []` for none. |
| `accounting` | table | the global `[accounting]` | The pool's own PPLNS window. |
| `coinbase` | table | the global `[coinbase]` | The pool's own worker-name settings. |

The pool refuses:

* an `id` used twice, or one that is not 1 to 32 of `a`–`z`, `0`–`9`, and `-`, or is `fleet`;
* a pool with neither `v1` nor `v2`;
* a listener on a port another listener takes: the same port on the same address, or on any address
  when either listens on every one (`0.0.0.0` or `[::]`);
* `public_v1` without `v1`, `public_v2` without `v2`, or a public port of 0.

Changing a pool's `id` starts a new pool; the old id's blocks and balances stay in the database and still
show.

### Portions

`finder`, the bounds of `personal`, and each fee take a portion: exactly one of

* `percent`: a percentage of the block's whole reward, from 0 to 100, to at most four decimal places;
* `btc`: a fixed amount in BTC, to at most eight decimal places;
* `sats`: a fixed amount in whole satoshis.

A fixed amount is at most 21 million BTC, and does not halve with the subsidy. Written inline, a portion
reads `finder = { percent = 100 }`, `finder = { btc = 0.5 }`, or `finder = { sats = 50_000_000 }`.

### `finder`

What a pool's `finder` makes it:

* `percent = 100`: solo. The finder is paid the whole miner portion in the block's coinbase.
* `percent = 0`, or a fixed 0: pooled. The pool wallet takes the miner portion, shared by work.
* anything between, or a fixed amount: hybrid. The finder is paid that part, and the rest is shared.

A `finder` of 0 is allowed. A percent below 100 must leave the pool output something beside the pool's
fees: the finder's percent and the fees' percents together must stay below 100%. A fixed amount above 0
must be at least 330 sats, the P2TR dust limit, so it is never dust whatever address the finder mines to.

### `personal`

`personal = { min = { percent = 10 }, max = { percent = 90 } }` lets each worker choose its finder part
within the range, by adding it to its username after a `+` (see [mining](mining.md)). `finder` is then the
part of a worker that chooses none.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `min` | portion | 0% | The lowest finder part a worker may choose. |
| `max` | portion | 100% | The highest. |

Each bound may be 0; a fixed bound above 0 must be at least 330 sats. With both bounds in one form,
`min` must not be above `max`, and a `finder` in that form must lie between them. Bounds in different
forms are compared on each job, where `max` wins.

### `[[pools.fees]]`

A pool's own fee outputs, each a table as `[[fees]]` takes it, and checked the same way. Written as
`[[pools.fees]]` entries under the pool, or `fees = []` for a pool that pays none while global fees exist.

### `[pools.accounting]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `window_multiplier` | number | `[accounting]`'s | As in `[accounting]`. |
| `window_min_work` | number | `[accounting]`'s | As in `[accounting]`. |

`maturity_buffer` is global and refused here.

### `[pools.coinbase]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `worker_names` | boolean | `[coinbase]`'s | As in `[coinbase]`. |
| `worker_names_opt_in` | boolean | `[coinbase]`'s | As in `[coinbase]`. |

The coinbase `tag` stays global. A pool that ends up with both `worker_names` and `worker_names_opt_in`
on, from its own table or from `[coinbase]`, is refused.

## `[[fees]]`

The fee outputs every coinbase carries after the miner portion, in this order, for every pool that sets
no `fees` of its own. Without any, the miner portion is the whole reward.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `name` | string | required | What the portal calls the receiver, such as `operator`: 1 to 64 characters, each name once. |
| `address` | Bitcoin address | required | Where the output pays, on the configured network. |
| `percent` | number | none | A percent of the block's whole reward, rounded down to the satoshi. |
| `btc` | number | none | A fixed amount in BTC. |
| `sats` | integer | none | A fixed amount in satoshis. |

Each fee takes exactly one of `percent`, `btc`, and `sats`, and it must be above 0. A fixed amount must be
at least its address's dust limit. The percents of one list together must stay below 100%, so the miner
always has something. Each fee is capped at what the fees before it leave. Each output adds about 31
bytes to the coinbase, so keep the list short.

## `[accounting]`

The PPLNS window of every pool that shares its blocks: a block's pool output is shared among the last
max(`window_multiplier` × network difficulty, `window_min_work`) of accepted work, in difficulty-1 units.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `window_multiplier` | number | 2.0 | k: the window as a multiple of the network difficulty. Must be positive. |
| `window_min_work` | number | required | W_min: the window's floor, in difficulty-1 units. Zero or positive. |
| `maturity_buffer` | integer | 0 | Blocks beyond coinbase maturity (100) before a block's allocations become available balances. |

A change applies to jobs issued after the restart that makes it. On regtest, whose network difficulty is
a fraction of one share, `window_min_work` sets the window.

## `[addresses]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `pool_wallet` | Bitcoin address | required | Takes the pool output of every block that has one: what finders leave, to share by work. |

It is required even when every pool is solo, and must be on the configured network.

## `[payouts]`

Batched on-chain payouts of the balances that pooled and hybrid blocks credit. Without this section the
payout worker does not run, and balances carry forward.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `wallet` | string | required | The payout wallet's name on the primary node: 1 to 64 letters, digits, `-`, and `_`. |
| `threshold_sats` | integer | required | The minimum payout, which an address's available balances in every pool together must reach. At least 330. |
| `interval_secs` | seconds | required | How often a batch is attempted. |
| `outputs_per_batch` | integer | required | The most addresses one batch pays: 1 to 1,000. |
| `fee_rate` | number | regtest: required | The fee rate a batch pays on regtest, in sat/vB, or the node's minimum if that is higher. Positive, at most `fee_rate_cap`. Refused on mainnet. |
| `confirmation_target` | integer | mainnet: required | The blocks within which a mainnet batch should confirm: it pays the node's `estimatesmartfee` for them, or the node's minimum if that is higher, and is deferred while the node has no estimate. 1 to 1,008. Refused on regtest, which has no estimates. |
| `fee_rate_cap` | number | required | A rate above it defers the batch. Positive. |

Without this section a pooled or hybrid pool still allocates each block's pool output to its window, but
nothing is paid out.

Batch fees come from the fee reserve, which `puntohunter reserve credit <sats> --config <file>` credits.

The payout wallet may be encrypted, so that nothing on the host can spend from it while it is locked. A batch
is then funded and reserved as usual, its inputs locked, and waits to be signed until the wallet is unlocked
with `walletpassphrase`; the next interval after that signs and sends it. While it waits, the health check's
`payouts` reads `locked`, and `puntohunter_payouts_locked` is 1. The wait is logged once, and survives a
restart.

## `[public]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `host` | string | the request's `Host` | Where miners connect, as the portal's connection settings give it. |
| `domains` | list of strings | `[]` | The sites' domains, for an instance that several sites share. |
| `authority_switch` | string | none | The day, `YYYY-MM-DD`, a new Stratum V2 authority key takes over, announced on the overview. |

`host` is a domain name or an IP address, an IPv6 one in brackets, with no scheme and no port; each pool's
ports come from its `public_v1` and `public_v2`. Without it, the portal names the host the browser used.

`domains` serves a portal behind a web proxy, such as Cloudflare's, that does not pass Stratum: each site
gets a DNS-only `stratum.` name for miners. A request to one of the domains, or to a name under it, is told
to connect to `stratum.` before that domain, so with `domains = ["example.com"]`, both `example.com` and
`portal.example.com` name `stratum.example.com`. A request to any other host, such as an IP address, names
that host. `domains` takes domain names only, and cannot be set beside `host`.

`authority_switch` announces a rotation of the authority key, together with the next key that
`puntohunter authority next` makes in the data directory: the overview then shows the next key and the day
under the current one ([operations](operations.md#rotating-the-authority-key)). Either one alone announces
nothing, and the pool logs a warning.

## `[stratum]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `authorize_timeout_secs` | seconds | required | A connection that has not authorized (V1) or opened a channel (V2) by then is closed. |

## `[vardiff]`

The share difficulty each connection is held to, adjusted to aim at one share per interval.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `floor` | number | required | The lowest share difficulty. Positive; may be below 1. |
| `ceiling` | number | required | The highest. At least `floor`. |
| `share_interval_secs` | seconds | required | The time between shares the difficulty aims at. |

[config/mainnet.toml](../../config/mainnet.toml) keeps the floor at 1; a floor below 1 is accepted.

## `[telemetry]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `rollup_secs` | integer | required | The period each stored rollup row of a worker's shares covers, a minute in the examples. Positive. |

## `[portal]`

The portal, [web/](../../web/), and the JSON API, `/api/v1`, on one listener ([the web portal](frontend.md)).

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `bind` | address and port | required | Where the portal listens, plain HTTP, such as `0.0.0.0:8080`. |
| `lookups_per_minute` | integer | required | Username lookups each client address may make a minute. Positive. |
| `streams_per_ip` | integer | 20 | The API's event streams each client address may hold open. Positive. |
| `max_streams` | integer | 2000 | Event streams the pool holds open in all. Positive. |
| `trusted_proxy` | IP address | none | The reverse proxy whose `X-Forwarded-For` names the client. |
| `keep_offline_days` | integer | 7 | Days an offline device stays on its fleet's page. Positive. |
| `site` | path | none | The `site.json` of the portal's brand: its name, description, and optional title and origin. Unset, PuntoHunter's own. |
| `overlay` | path | none | A directory laid over the portal's files, `web/src/`: each file replaces the one of the same path, or is added. |

Behind a reverse proxy, set `trusted_proxy` to its address, such as `127.0.0.1`. Only a request from that
address has its `X-Forwarded-For` read, so the limits key on each visitor; without it, they key on the
peer's address.

`site` and `overlay` brand the portal as `web/build.py --site` and `--overlay` brand a build of it for a
static host ([its brand](frontend.md#its-brand)), so one site directory serves both. A path is relative to
the pool's working directory. Both are read once, at startup, so a change takes a restart. The pool does
not start when `site` cannot be read or lacks `name` or `description`, when its `origin` is not an
origin, when `overlay` is not a directory, or when a page names a partial or a `{{key}}` the site does not
have; the error names the file. The portal's pages always call the pool's own API, so `site.json`'s `api` is
ignored.

## `[api]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `allowed_origins` | list of strings | none | The origins whose browser pages may call `/api/v1`. |

Without it, the API is same-origin only. Each entry is one exact origin, such as
`https://dashboard.example.com`: `http://` or `https://`, a lower-case host, and an optional port, with no
path, no trailing slash, and no wildcard. Anything else is refused. [API reference](api.md) and
[frontend](frontend.md) say when a site of your own needs it.

## `[metrics]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `bind` | address and port | required | Where the Prometheus endpoint, `/metrics`, listens, such as `127.0.0.1:9100`. |

Keep it on loopback. [Operations](operations.md) lists the series.

## `[coinbase]`

What every coinbase's scriptSig says after the block height.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `tag` | string | `/puntohunter/` | Shown by block explorers as the pool's name: 1 to 32 printable ASCII characters. |
| `worker_names` | boolean | false | Follow the tag with `Mined by <worker>/` in every block. |
| `worker_names_opt_in` | boolean | false | Do so only for a worker that adds `+name` to its username. |

The worker name is the part of the username after the address's dot, cut to 32 characters. A name written
to a block stays on the chain for good, so both are off by default. A pool may set either under its own
`[pools.coinbase]`, and no pool may have both on.

## `[group]`

Other PuntoHunter instances, on hosts of their own, whose figures the portal's overview adds to this
one's, as totals alone. Without `[group]`, the overview shows this instance alone.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `name` | string | required | The heading of the overview's group card: 1 to 64 characters. |
| `refresh_secs` | seconds | 30 | How often each member is read. |
| `label` | string | none | This instance's public label, shown with `show_members`. |
| `show_members` | boolean | false | List each instance on the card and in the API by its label. |

At least one `[[group.members]]` entry is required. With `show_members`, `[group]` and every member need a
`label`: 1 to 32 printable characters.

### `[[group.members]]`

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `name` | string | required | The member's name in logs and metric labels, never shown: 1 to 32 lower-case letters, digits, and `-`. |
| `url` | origin | required | The member's origin, such as `https://pool.example.com`, read at `<url>/api/v1/pool`. Never shown. |
| `label` | string | none | Public text the card shows with `show_members`. |
| `link` | origin | none | The origin of the member's portal, which its label links to. |

Each member's `name` and `url` must be unique, and `url` and `link` exact origins, as `[api]` takes them.

## `[geo]`

The IP-to-country database the stats page counts connected workers by country from. Without `[geo]`, the
page has no countries.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `database` | path | required | A MaxMind DB file, such as DB-IP's IP to Country Lite or MaxMind's GeoLite2 Country. |
| `min_workers` | integer | 1 | The fewest connected workers a country is shown with; fewer count as "Elsewhere". At least 1. |

The file must exist when the pool starts, or it refuses to. The pool checks it hourly and reopens it when
it changes, so a monthly update needs no restart. Only a connection's country is kept, never its address.
A `min_workers` above 1 keeps a row from showing one worker's hashrate beside where it is.

## `[stats]`

How many rows each list of the stats page and `GET /api/v1/stats` shows.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `best_shares_round` | integer | 10 | Fleets by best share since the last block. |
| `best_shares_24h` | integer | 10 | Fleets by best share over the last 24 hours. |
| `hash_rates` | integer | 10 | Fleets by hashrate now. |
| `miner_types` | integer | 10 | Miner types; the rest fold into "Other". |
| `countries` | integer | 10 | Countries; the rest fold into "Elsewhere". |

Each is at least 1. A fleet's own ranks count every fleet, whatever the lists show.

## `[backup]`

Snapshots of the database. Without `[backup]`, the pool takes none.

| Key | Type | Default | What it does |
| --- | --- | --- | --- |
| `dir` | path | `backups` | Where snapshots are written, relative to `data_dir`. Created if missing. |
| `interval_secs` | integer | 900 | Seconds between snapshots. Positive. |
| `keep` | integer | 96 | How many of the newest snapshots are kept; older ones are removed. At least 1. |

The pool takes a snapshot when it starts, every `interval_secs`, and at once after a block with a pool
output is allocated or a payout batch is broadcast, so the records that move money wait for no interval.
[Operations](operations.md#snapshots) says how to copy them off the host and restore one.
