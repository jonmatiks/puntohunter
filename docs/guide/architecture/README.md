# Architecture

These pages describe how PuntoHunter's code works, read from [src/](../../../src/): its parts, how work and
money flow between them, and why the main choices were made. They are for developers, contributors, and
operators who want to know what runs inside the pool. Like the rest of the guide, they follow the code: a
change to a component updates its page in the same change.

## Pages

1. [Mining](mining.md): templates to jobs, shares, and blocks; the coinbase and its split.
2. [Stratum](stratum.md): V1 and V2 sessions, channels, vardiff, usernames, and the authority key.
3. [Nodes](nodes.md): the node fleet, template choice, failover, and block submission.
4. [Accounting](accounting.md): the journal, PPLNS, the ledger, payout batches, and reconciliation.
5. [Storage](storage.md): the database, when a share is durable, and snapshots.
6. [Telemetry](telemetry.md): handles, open and private fleets, rollups, and rounds.
7. [Portal](portal.md): the JSON API, streams, limits, and the web app the binary serves.
8. [Security](security.md): secrets and keys, private fleets, and trust boundaries.
9. [Decisions](decisions.md): the main design choices and the alternatives set aside.

## One Process

PuntoHunter is one binary and, when it serves, one process: every pool, both Stratum versions, the
accounting, the portal, and the metrics run in it, on one Tokio runtime. It needs nothing beside it but
Bitcoin Core. Its state is one SQLite database and a few key files in the data directory, and the web app it
serves is compiled into the binary. One config file describes one network; a second network is a second
process with its own config and data directory.

The binary's subcommands, from [main.rs](../../../src/main.rs):

| Command | What it does |
| --- | --- |
| `serve --config <file>` | Runs the pool until Ctrl-C or SIGTERM. |
| `reserve credit <sats> --config <file>` | Credits the fee reserve that payout batches pay their fees from. |
| `token issue <address> --config <file>` | Issues a balance token for one address, printed once; only its hash is kept. |
| `authority next --config <file>` | Makes the next SV2 authority key, for a rotation, and prints its public key. |
| `payouts resume --config <file>` | Lifts every pause of payouts, once the operator has settled its cause. |

## Startup

`serve` starts the parts in an order that keeps each one's inputs ready before anything uses them:

1. The config is read and checked as a whole ([config.rs](../../../src/config.rs)). A rejected value names
   its key, and nothing else starts.
2. The metrics endpoint opens ([metrics.rs](../../../src/metrics.rs)), then the data directory and the
   database: one writer and one reader ([store/](../../../src/store/)). With `[backup]`, the snapshot task
   starts.
3. The secrets load, or are made on first run: the SV2 authority key, a next key if a rotation is announced,
   and the handle key ([secret.rs](../../../src/secret.rs)).
4. The node fleet starts with the primary node first ([node/](../../../src/node/)).
5. The current round is read back from the database before anything can end it or count in it
   ([round.rs](../../../src/telemetry/round.rs)), then the block submitter, the block lifecycle, and the
   rollup task start.
6. Each pool that pays a pool output records its accounting window. Blocks whose solution was journaled but
   not yet allocated before a crash are allocated now. A pause for reconciliation is reported.
7. With `[payouts]`, the payout worker starts; then the share journal.
8. With `[geo]`, the country database opens. The portal's pages are assembled, so a site that cannot be
   built stops the pool before a miner connects.
9. Each pool's Stratum listeners open, V1 and V2 as the pool configures them.
10. With `[group]`, polling of the group's members starts; then the portal listener opens.
11. The metrics endpoint reports `puntohunter_ready 1`, and the process waits for its stop signal.

## How Work and Money Flow

```text
Bitcoin Core nodes ── templates, tips (RPC, ZMQ) ──▶ node fleet ── Work ──▶ job cache ── jobs ──▶ sessions ──▶ miners
                                                                                                      │
miners ── shares ──▶ V1 / V2 sessions ── share check ──┬──▶ telemetry (memory) ── rollups ──▶ store  │
                                                       ├──▶ journal (pooled credit) ───────▶ store  │
                                                       └──▶ found block ──▶ submitter ──▶ every node
node fleet ── tip ──▶ lifecycle ── confirmed / orphaned / mature ──▶ store ──▶ PPLNS ──▶ ledger
ledger ──▶ payout batches ──▶ payout wallet (on the node)
store, telemetry ──▶ portal: JSON API and streams ──▶ the web app in the browser
```

* **Work.** The node fleet chooses the node whose template the pool mines and publishes it as `Work`. Each
  pool's jobs are built from it, with that pool's coinbase ([Mining](mining.md), [Nodes](nodes.md)).
* **Shares.** A V1 session or a V2 channel turns a miner's submission into one share check, shared by both
  protocols ([Stratum](stratum.md)). The share updates its worker's live values in memory at once
  ([Telemetry](telemetry.md)). In a pool that pays a pool output, it is also written to the share journal
  before the miner is answered ([Accounting](accounting.md), [Storage](storage.md)).
* **Blocks.** A share that meets the network target is a block. It is written to disk and submitted to every
  node at once ([Nodes](nodes.md)). The lifecycle follows it until it matures or is orphaned, and a pooled
  block's pool output is then shared by PPLNS into miners' balances, which payout batches pay
  ([Accounting](accounting.md)).
* **Reading.** The portal builds each JSON resource and its stream from one model over the store and the
  live telemetry. The binary renders no page: the web app it serves reads the API in the browser
  ([Portal](portal.md)).

## Tasks and Channels

Each long-running part is a Tokio task, or a thread where it blocks, and they talk over channels:

| Part | Module | Runs | Talks over |
| --- | --- | --- | --- |
| Node fleet | [node/fleet.rs](../../../src/node/fleet.rs) | Per node, tasks for health, templates, and ZMQ tips; one that chooses | `watch`: the current `Work`, the fleet's status, the primary's state |
| Stratum sessions | [stratum/](../../../src/stratum/) | One task per connection | reads `Work`; sends found blocks and journal entries |
| Share journal | [pool/journal.rs](../../../src/pool/journal.rs) | One task | `mpsc` of entries, each answered over a `oneshot` once committed |
| Store writer | [store/mod.rs](../../../src/store/mod.rs) | One thread owns the database | each commit answered over a `oneshot` |
| Block submitter | [pool/submitter.rs](../../../src/pool/submitter.rs) | One task | `mpsc` of found blocks, fanned out to every node |
| Block lifecycle | [pool/lifecycle.rs](../../../src/pool/lifecycle.rs) | One task | watches `Work` and the fleet's status for new tips |
| Rollups | [telemetry/rollup.rs](../../../src/telemetry/rollup.rs) | One task, each period | reads telemetry, writes the store |
| Payouts | [pool/payouts.rs](../../../src/pool/payouts.rs) | One task, each interval, with `[payouts]` | the store and the payout wallet |
| Snapshots | [store/backup.rs](../../../src/store/backup.rs) | One task, each interval, with `[backup]` | its own read of the database |
| Country database | [geo.rs](../../../src/geo.rs) | One task, hourly, with `[geo]` | reopens the file when it changes |
| Group polling | [group.rs](../../../src/group.rs) | One task, with `[group]` | the members' public API |
| Metrics | [metrics.rs](../../../src/metrics.rs) | The Prometheus listener | reads state when scraped |
| Portal | [portal/](../../../src/portal/) | The HTTP listener | reads the store, telemetry, and the fleet's state |

A session never waits on the database for anything but its own pooled shares: telemetry is updated in
memory, and the store's writer is the only code that writes the database.

## Modules

| Path | What it holds | Page |
| --- | --- | --- |
| [main.rs](../../../src/main.rs) | The subcommands, and `serve`'s startup | this page |
| [lib.rs](../../../src/lib.rs) | The library's modules, and `VERSION`, the package version and the commit it is built from | this page |
| [config.rs](../../../src/config.rs) | The config, checked as a whole at startup | [Configuration](../configuration.md) |
| [mining/](../../../src/mining/) | Jobs, the coinbase, targets, vardiff, shares, and blocks | [Mining](mining.md) |
| [stratum/](../../../src/stratum/) | The V1 and V2 front-ends, usernames, the authority key | [Stratum](stratum.md) |
| [node/](../../../src/node/) | The node client, templates, health, and the fleet | [Nodes](nodes.md) |
| [pool/](../../../src/pool/) | The journal, PPLNS, ledger, block lifecycle, submission, payouts, reconciliation | [Accounting](accounting.md) |
| [wallet.rs](../../../src/wallet.rs), [tokens.rs](../../../src/tokens.rs) | The payout wallet's client, and balance tokens | [Accounting](accounting.md) |
| [store/](../../../src/store/) | The SQLite store, its schema, and snapshots | [Storage](storage.md) |
| [telemetry/](../../../src/telemetry/) | Handles, live values, fleets, rollups, and rounds | [Telemetry](telemetry.md) |
| [portal/](../../../src/portal/), [geo.rs](../../../src/geo.rs), [group.rs](../../../src/group.rs) | The API, the site, the country database, the group | [Portal](portal.md) |
| [secret.rs](../../../src/secret.rs) | Key files in the data directory | [Security](security.md) |
| [metrics.rs](../../../src/metrics.rs) | The Prometheus endpoint | this page |
| [failpoint.rs](../../../src/failpoint.rs) | Crash points for the recovery tests | this page |

## Metrics

[metrics.rs](../../../src/metrics.rs) serves the Prometheus endpoint on its own listener, `[metrics] bind`:
each node's state and lag, the template's age, shares and refusals by pool and transport, submission
latency, connected workers, and `puntohunter_ready`, which deployment scripts and the tests wait on. No
metric has a per-worker label, since a label is where an address would leak; per-worker figures are the
portal's. Values that change with the clock or the tip are read when the endpoint is scraped.
[Operations](../operations.md) lists what to watch.

## Tests and Failpoints

The unit tests sit beside the code they test. [tests/serve.rs](../../../tests/serve.rs) runs the built binary,
and [tests/e2e/](../../../tests/e2e/) drives it end to end on regtest, with Bitcoin Core in Docker, real
Stratum clients, and the portal in a browser. [tests/data/](../../../tests/data/) holds captured mining and
node fixtures.

[failpoint.rs](../../../src/failpoint.rs) gives the crash-recovery tests named points where the process
stops, or a commit fails, chosen by the `PUNTOHUNTER_FAILPOINT` environment variable. Without it every
point is inert. They are compiled into the release binary, so the tests exercise the artifact that ships.
