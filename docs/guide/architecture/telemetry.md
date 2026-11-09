# Telemetry

This page explains how the pool tracks its miners: the opaque handles that name workers and fleets,
how a fleet is recorded, open or private, the live values kept in memory, the rollups that keep their
history, and the round behind the stats page. The code is in [src/telemetry/](../../../src/telemetry/):
[mod.rs](../../../src/telemetry/mod.rs), [handle.rs](../../../src/telemetry/handle.rs),
[fleet.rs](../../../src/telemetry/fleet.rs), [rollup.rs](../../../src/telemetry/rollup.rs), and
[round.rs](../../../src/telemetry/round.rs). The guide's [Mining](../mining.md) page describes handles
and private fleets from the miner's side; this page is about the mechanism. [Storage](storage.md) covers the
tables, and [the portal](portal.md) covers the pages that read them.

## The Flow

```text
 V1 authorize / V2 open channel
          |
          v
 Context::worker ── HandleKey::handle, HandleKey::fleet ── worker and fleet handles
          |
          ├── Fleets::record ──────────── store writer ──> fleets
          ├── Telemetry::connect ──> Link  (dropping it is the disconnect)
          ├── Telemetry::describe (Listing), Telemetry::choose (Choices)
          v
 Link::accepted / rejected / target
          |
          ├── Worker live values (memory) ── change broadcast ──> portal streams
          ├── Row of the current period ─── rollup task ───────> rollups, totals, best_shares
          └── Round::share (fleet bests) ────────────────────────> stats page
 submitter: Round::found on a block a node accepted ─> next round
```

`Context::worker`, in [src/stratum/mod.rs](../../../src/stratum/mod.rs), joins a session to
telemetry. A V1 session calls it when it authorizes, and a V2 session for each channel it opens. Both
protocols then report through the same `Link`, so nothing below depends on the protocol except the
transport label, `V1` or `V2-native`.

## Handles

A worker is never keyed on its address in a URL or an API response. It is keyed on a handle: 16
lower-case base32 characters, 80 bits. `Handle` and `HandleKey` are in
[handle.rs](../../../src/telemetry/handle.rs).

Each handle is the first 10 bytes of an HMAC-SHA256 over a tag, a colon, and a text:

| Handle | Tag | Text |
| --- | --- | --- |
| Worker, open | the pool's id | `<address>.<worker>` |
| Worker, private | `<pool id>#private` | `<address>.<worker>#<fleet secret>` |
| Fleet, open | `fleet` | `<address>` |
| Fleet, private | `fleet#private` | `<address>#<fleet secret>` |

* **The key** is 32 random bytes in `handle.key` in the data directory.
  `HandleKey::load_or_generate` creates it on first run with mode 0600, never overwrites one, and
  refuses to start if the file is readable by group or others ([src/secret.rs](../../../src/secret.rs)).
* **Why an HMAC.** Addresses are public. A plain hash of one could be matched by hashing known
  addresses. Without the key, a handle can be neither reversed nor computed, so a handle reveals
  nothing, and nobody can find a stranger's fleet page from an address.
* **Why the tags.** A pool id is never `fleet` and holds no `#`, and neither does a canonical
  username. No two kinds of handle share an input, so a worker handle never equals a fleet handle,
  and a device without the secret never lands on a private handle.
* **Stable.** The same key gives the same handles across restarts, so links and bookmarks keep
  working. A new key changes every handle.
* **Canonical.** The text is rebuilt from the parsed username: the address in its canonical form, so
  a bech32 address typed in upper case names the same worker. Options after `+` are left out, so a
  device that changes its finder part or adds `+name` keeps its handle and its history.
* **Per pool.** The pool id is in a worker handle, so one username in two pools is two workers.
  Changing a pool's id gives its workers new handles.

`Handle` parses only the exact form it prints. The tables add a check that admits nothing else in a
handle column, so a handle can never carry other text.

## Fleets

A fleet is every worker mining to one address, in any pool. [fleet.rs](../../../src/telemetry/fleet.rs)
records each one in the `fleets` table under its fleet handle, with its worker handle, pool, worker
name, the time it was first seen, the address, its miner type, and whether the fleet is private.

`Fleets::record` writes through the store's writer thread without waiting. It remembers in memory
which workers it has written since startup, so a reconnect writes nothing unless the device names a
new miner type. The insert keeps the first row for a handle, and only fills in an address or a miner
type.

### Open and Private

A device without a fleet secret is in its address's open fleet. A device with one is in a private
fleet, keyed on the address and the secret together: the same address with another secret is another
fleet. The secret comes after a `#` in the username, or from a V1 password once its `d=N` hints are
removed, as [the username parser](../../../src/stratum/username.rs) reads it.

The secret enters the HMAC and is kept nowhere else. `Secret` hides its value from debug output, and
`Secret::expose` exists for the handles alone. It is never written to the database, a log, or a
page.

A private fleet differs from an open one in what other pages show:

* The stats lists hold no address for it, not even a shortened one.
* The open fleet's page neither lists nor counts its devices, since they have other handles.

The fleet page itself shows the address to anyone holding the fleet handle, private or not.

### What Else Is Recorded

* **Miner type.** `vendor` takes the first word of a V2 `SetupConnection` vendor or a V1 user agent,
  drops a trailing version, and keeps it only if it is at most 24 safe characters. A version is
  dropped because a rare firmware version would let one device be followed across pools.
* **Shortened address.** `shortened` keeps the first 6 and last 4 characters, as `bc1qw5…f3t4`. That
  is the only form of an address telemetry holds in memory, and too little to find the address from.

## Live Values

[mod.rs](../../../src/telemetry/mod.rs) holds `Telemetry`: a map from worker handle to its live
state, behind one mutex, updated inline by the sessions that mine for it. Nothing here waits on the
database.

* **One worker, many connections.** A worker is its handle, so one username on two devices is one
  worker. Its connections are counted per transport, and its hashrate sums theirs.
* **The `Link`.** `Telemetry::connect` returns a `Link` that the session holds. `Link::accepted`,
  `Link::rejected`, and `Link::target` report shares and difficulty changes. Dropping the link is
  the disconnect, so a session that ends any way at all is counted out.
* **Hashrate.** Each accepted share adds the work of the target it was credited at, not what its
  hash reached. The rate is the work in the window over the time it covers: 10 minutes (`WINDOW`)
  and 1 minute (`MINUTE`). While the worker has been connected for less than the window, the rate is
  over the time since it connected, and is marked `estimating`.
* **Status.** `Status` is online with a share accepted in the last 10 minutes, idle when connected
  without one, and offline when no connection is open.
* **Also held.** The last share, the best share, the current difficulty, when the session began, the
  choices of the connection opened last (`Choices`), and its stats listing (`Listing`: fleet handle,
  shortened address, miner type, country). Refused shares are counted by class (`Reject`).
* **Changes.** Every update sends the worker's handle on a broadcast channel
  (`Telemetry::subscribe`). The portal's streams listen for the handles they show. A subscriber that
  falls more than 1024 changes behind misses some, and reads current values on the next.
* **Eviction.** At each rollup boundary, `Telemetry::evict` forgets workers with no connection that
  have done nothing for 10 minutes. A client cycling through usernames cannot grow the map without
  bound. Their history stays in the rollups.

## Rollups

Live values cover the last 10 minutes. History is kept in rollups: one row per worker, transport, and
period, with accepted shares, their work, refused shares by class, and the best share. The period is
`rollup_secs` in `[telemetry]` ([configuration](../configuration.md)). A worker that did nothing in a
period has no row for it.

Each accepted or refused share is counted into its row in memory as it arrives. The rollup task in
[rollup.rs](../../../src/telemetry/rollup.rs), started from [src/main.rs](../../../src/main.rs),
sleeps to each period boundary, takes the finished rows out with `Telemetry::finished`, evicts idle
workers, and writes the rows in one transaction:

* `rollups`: the rows, added to any row already there for the same handle, transport, and start.
  A share stamped before a boundary but counted after it lands in a row of its own, which this sum
  merges.
* `totals`: each worker's accepted and refused shares, with `through`, the boundary of the newest
  write that added to them.
* `best_shares`: each worker's best share by UTC day.

### Counting Without Gaps

The worker and fleet pages show all-time counts as the stored totals plus what telemetry has not yet
written (`Telemetry::unflushed`). Telemetry keeps the rows it last took out, with their boundary,
until the next write. `unflushed` counts them only if `totals.through` shows the store does not hold
them yet. A reader that read the store just before a write therefore neither misses those shares nor
counts them twice.

### Reading History

* `history`: the pool, last transport, best share, last share, and last seen of a worker that
  telemetry no longer holds, from the rollups, blocks, and fleets.
* `windows`: the 1-hour and 24-hour hashrates, from the rollups up to the last period boundary.
  Before a worker's first row fills the window, the rate is over the time since that row, marked
  `estimating`, so a device that joined ten minutes ago reads its own rate, not a sixth of it.
* `series`: chart data, summed over the given handles and both transports, in buckets that are a
  multiple of the period.
* `known`: whether a handle has a rollup row, a block, or a fleet row. The portal's username lookup
  uses it to decide whether a worker has mined here.

## The Round

[round.rs](../../../src/telemetry/round.rs) holds the round in memory: the time since this instance's
newest block a node accepted, each fleet's best share in it, each fleet's best in each of the last
24 hours, and each miner type's best this round. The stats page lists it.

* `Telemetry` passes every accepted share to `Round::share`, with the worker's `Listing`. The round
  keys fleets on the fleet handle and the shortened address only; the whole address never enters it.
* The block submitter ([src/pool/submitter.rs](../../../src/pool/submitter.rs)) calls `Round::found`
  for a block a node accepted, which clears the round bests and starts the next round. A refused
  block starts nothing, nor does a late answer for a block found before the one that started the
  current round. The share that found a block stays in the round it ended.
* The hourly bests run across rounds. `Round::prune`, called with each eviction, drops hours older
  than 24 and fleets with nothing left.
* Ties rank by the earlier share, then by handle.

### Seeding at Startup

[src/main.rs](../../../src/main.rs) builds the round with `Round::seed` before the submitter starts
and before any share counts. It reads the newest block a node answered and did not reject, then the
best share of each whole day since from `best_shares`, and the rest of the block's day from
`rollups`, leaving out the block's own period. The hourly bests come from the rollups of the workers
with a best share today or yesterday. Every rollup read goes by handle first, along the table's key,
so a long round reads days, not periods. A restart loses at most the period not yet written.

## What the Portal Receives

The portal holds the same `Telemetry` and `HandleKey` as the Stratum sessions. It takes:

* **Live values** with `Telemetry::live`, and each pool's hashrate and connected workers with
  `Telemetry::totals`. [The metrics endpoint](../../../src/metrics.rs) reads connection counts by
  pool and transport with `Telemetry::connected`.
* **Stored values** through its read connection: `fleet::members`, `fleet::address`,
  `fleet::private`, `rollup::history`, `rollup::windows`, `rollup::totals`, and `rollup::series`.
  [src/portal/device.rs](../../../src/portal/device.rs) reads a device from both sources side by
  side and never blends them into one number.
* **Changes** from `Telemetry::subscribe`, which drive the worker and fleet streams in
  [src/portal/worker.rs](../../../src/portal/worker.rs) and
  [src/portal/fleet.rs](../../../src/portal/fleet.rs).
* **The round** through `Telemetry::round`, and connected workers with `Telemetry::connected_workers`,
  for [the stats page](../../../src/portal/stats.rs). These carry handles and shortened addresses only.
* **Handles** from `HandleKey`, to turn a username lookup into a fleet handle
  ([src/portal/lookup.rs](../../../src/portal/lookup.rs)). The lookup computes the worker handle in
  each pool and asks `Telemetry::live` and `rollup::known` whether it has mined here. A wrong secret
  gives a handle that never mined, so it finds nothing.

A device's share of a PPLNS window is not telemetry: [src/portal/window.rs](../../../src/portal/window.rs)
reads it from the share journal under the same worker handles ([accounting](accounting.md)).
