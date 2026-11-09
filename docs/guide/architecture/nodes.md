# Nodes

This page covers how the pool talks to Bitcoin Core: the client for one node, the watcher that keeps each
node's health and latest template live, the selector that picks which node's template the miners work on,
and the submitter that sends a found block to every node. The code is in [src/node/](../../../src/node/),
and block submission is in [submitter.rs](../../../src/pool/submitter.rs). What a found block pays, and when,
is in [Accounting](accounting.md). The config keys named here are in [Configuration](../configuration.md).

A pool has one node, the primary (`[node]`), and any number of secondaries (`[[secondary_nodes]]`), in
config order. Together they are the fleet. With one node, everything below still runs: a fleet of one
publishes that node's work.

```text
             +---------+   +---------+   +---------+
             | primary |   |  node b |   |  node c |     Bitcoin Core: JSON-RPC, ZMQ hashblock
             +----+----+   +----+----+   +----+----+
                  |             |             |
             watcher        watcher        watcher        template.rs, health.rs (one per node)
                  |             |             |
                  +------ View -+---- View ---+
                                |
                            selector                      fleet.rs
                                |
                  Work (one template, or none)  ------>  jobs for every pool
                                                          ...
      found block  ------>  submitter  ------>  every node not quarantined, at once
```

## The Node Interface

The rest of the engine sees a node only through the `NodeClient` trait in
[mod.rs](../../../src/node/mod.rs). It has six methods: `getblockchaininfo`, `getnetworkinfo`,
`getblocktemplate` (with an optional long-poll id), `submitblock`, `getblockheader`, and `tip_events`, a
watch channel of block hashes the node announces.

`Bitcoind` is the real implementation: an `RpcClient` plus a ZMQ subscriber. `FakeNode` in
[fake.rs](../../../src/node/fake.rs) is an in-memory node behind the same trait, compiled only for tests. It
serves responses recorded from real mainnet and regtest nodes, kept in
[tests/data/node/](../../../tests/data/node/), and can be told to fail, hang, lose its peers, or move its
tip. The fleet, health, and submitter tests run against it with Tokio's paused clock, so a 60-second window
is tested without waiting 60 seconds.

The pool reads only the fields it needs: `BlockchainInfo` (chain, height, best hash, chain work, whether the
node is in initial block download), `NetworkInfo` (peer count, whether P2P is on), `BlockTemplate`, and
`BlockHeaderInfo` (height, confirmations, chain work).

A failed call is a `NodeError`. Its kinds stay apart because they are treated differently:

| Kind | Cause | Effect on health |
| --- | --- | --- |
| `Refused` | The connection was refused | Outage, `refused` |
| `Timeout` | No answer within the call's timeout | Outage, `hung`: a hung node counts as down |
| `Rpc` | The node answered with an RPC error | Outage, `failed` |
| `Protocol` | The answer was not usable JSON-RPC, such as a bare HTTP 401 | Outage, `failed` |
| `Credentials` | The cookie or password file cannot be read | Outage, `failed` |

## RPC and ZMQ

[rpc.rs](../../../src/node/rpc.rs) is a small JSON-RPC 2.0 client over `reqwest`. Every call carries HTTP
basic authentication from one of three sources: a cookie file, a user and password from the config, or a user
and a password file. The cookie file is read on every call, because Bitcoin Core writes a new cookie each time
it starts; the password file is also read on every call, so a password changed on both sides needs no
restart. Every call has `rpc_timeout_secs` as its timeout, except a long-polled `getblocktemplate`, which has
`longpoll_timeout_secs`.

A response's `result` is kept as raw JSON until its type is known, and then decoded straight into that type.
A full template parsed into a generic JSON value first would cost thousands of short-lived allocations.
`getblockheader` turns Bitcoin Core's "block not found" error (`-5`) into `None`. `RpcClient::wallet` gives a
client for one wallet's endpoint (`/wallet/<name>`), which the payout code uses on the primary.

[zmq.rs](../../../src/node/zmq.rs) subscribes to the node's `hashblock` topic and publishes each block hash
on the node's `tip_events` channel. It only wakes the template loop early. If the socket fails, it logs once,
retries every five seconds, and the long poll carries on alone.

## One Watcher per Node

[template.rs](../../../src/node/template.rs) starts a watcher for each node. The watcher is the only caller
of its node's template and health calls, and it keeps a `View` of the node current whether or not the node
is selected: its health state, tip, peer count, the latency of its last health check, and its latest
template. Because every node's template is always ready, failing over is a matter of publishing a different
node's template; no call is made at that moment.

Each watcher runs two tasks:

* **Health check**, every second (`HEALTH_INTERVAL`): `getblockchaininfo`, then `getnetworkinfo`. It
  records the tip and its chain work, the latency, and the peer count (0 when P2P is switched off). A node
  that is not healthy loses its template from the view.
* **Template loop**: a long-polled `getblocktemplate` with the segwit rule. It makes a fresh, non-long-poll
  call as soon as either ZMQ or the health check shows a new tip, in case the long poll missed it. If the
  node turns unhealthy mid-poll, the poll is dropped, since a node that hung may answer that poll only at its
  next block; a fresh call is made once it is healthy again.

Two errors in the template loop are not outages:

* A long poll that times out on a quiet chain triggers an immediate health check, which decides whether
  the node is really hung.
* RPC error `-9`, which mainnet's `getblocktemplate` returns while the node has no peers. The peer count
  already shows that as a soft fault; treating it as an outage would make the state flap with every check.

Before a template is published, the watcher normalizes its version: `normalized_version` clears the
version-rolling bits (BIP320's 13 to 28) that the template's `vbrequired` does not set. A V1 miner that
reads BIP310 as OR and one that reads it as XOR then build the same header. The watcher also attaches the
chain work of the block the template builds on, taken from the last tip or template, or else asked of the
node with `getblockheader`, because a template can arrive before the health check has seen its tip.

## Health

[health.rs](../../../src/node/health.rs) turns call outcomes into a node's `State`:

| State | When |
| --- | --- |
| `Starting` | No `getblockchaininfo` answer yet, so the chain is unchecked |
| `Healthy` | On the configured chain, out of initial block download, answering |
| `InIbd` | In initial block download |
| `Outage` | The last call failed: `Refused`, `Hung`, or `Failed` |
| `WrongChain` | On another chain than `network` names |

Any failed call puts the node in an outage, but only a `getblockchaininfo` answer brings it back; a
successful call of another kind changes nothing. `WrongChain` is permanent for the life of the
process; a later answer on the right chain does not clear it, only a restart with a corrected config does.
Each transition is logged once and published as the `puntohunter_node_state` metric.

## Choosing the Template Source

[fleet.rs](../../../src/node/fleet.rs) holds the selector. It wakes whenever any view changes, or when one
of its windows ends, judges every node, and publishes the work of the one it selects. Each node gets a
`Standing`:

| Standing | Kind | Meaning |
| --- | --- | --- |
| `Fit` | none | Free of faults |
| `Recovering` | soft | Out of a fault, not yet through `recovery_secs` |
| `Stuck` | soft | Its tip has had less chain work than the fleet's best for `stuck_secs` |
| `Isolated` | soft | No peers |
| `Starting` | hard | No answer or no template yet |
| `Ibd` | hard | In initial block download |
| `Down` | hard | In an outage: `refused`, `hung`, or `failed` |
| `Quarantined` | hard | On the wrong chain, for the life of the process |

A hard fault always keeps a node from sourcing templates. A soft fault does so only while some other node is
fit, so a pool whose nodes are all isolated or stuck keeps mining rather than stopping. The fleet's best tip
is the one with the most chain work among healthy nodes, so a taller chain with less work loses.

The recovery window applies to a node that was fit or sourced templates before, and then had a fault. At
startup a node is admitted as soon as it is clear, with no window. If no fit node remains, a recovering node
can be selected, and is then admitted at once.

Among the candidates, the selector ranks each template by, in order:

1. the chain work of the block it builds on;
2. how many nodes without a hard fault have a template on the same previous block: the majority's tip;
3. whether it comes from the primary.

The current source keeps its place while nothing ranks above it. Otherwise the top-ranked candidate with
the lowest health-check latency is taken. When nodes disagree on the tip at equal work, a fork or a node on a
minority chain, the selector logs the set of tips once.

## Publishing Work

The selector publishes on a watch channel of `Work`: either `Work::None`, when no node may source
templates, or `Work::Template`, which carries:

* `seq`, a counter across all nodes, so jobs can be cached per template;
* `clean`, true when the previous block differs from the last template published;
* `block`, the normalized template.

`clean` follows the tip, not the node. The last previous-block hash is kept through `Work::None`, so work
that resumes after an outage, or moves to another node on the same tip, is not clean: nothing has made the
miners' jobs on that tip stale. Mining builds jobs from this channel ([Mining](mining.md)).

The selector also publishes a `Status`, every node's standing, height, lag in blocks, and latency, plus the
index of the source. The portal's health check reads it ([Portal](portal.md)), and the metrics
`puntohunter_node_standing`, `puntohunter_node_selected`, and `puntohunter_node_source_changes_total` come
from the same round ([Operations](../operations.md)).

## Submitting a Block

When a share meets the network target, the Stratum session sends a `FoundBlock`, with the block's hex, to the
submitter in [submitter.rs](../../../src/pool/submitter.rs) and goes on to credit the share. The submitter
handles blocks one at a time:

1. **Fan out.** It sends `submitblock` to every node whose standing is not `Quarantined`, each call in a
   task of its own, before anything is written. A slow or hung node delays neither the others nor the
   record.
2. **Persist.** At the same time it writes the hex to `blocks/<hash>.hex` in the data directory, through a
   temporary file, synced, and a rename so a crash never leaves a partial file, and records the block in the
   `blocks` table with a synced commit ([Storage](storage.md)). A failure of either is logged and does not
   hold up submission. A block that pays a pool output is allocated here if its solution share is already
   journaled.
3. **Settle.** It reads answers as they arrive and stops at the first success. The other answers are still
   logged as they come.
4. **Record.** The answer is written to the block's row, once. A rejected block's allocations are voided in
   the same commit. A block a node accepted starts the next round.

Each node's answer is read as follows:

| Answer | Read as |
| --- | --- |
| `null` | Success, recorded as `accepted` |
| `duplicate`, `inconclusive`, `duplicate-inconclusive` | Success: the node already has it or will decide later |
| Any other reason, or an RPC error | Reject, logged as critical for that node |
| Refused, timed out, or not JSON-RPC | No answer |

Without a success, the submitter waits for every node. If any rejected, the block is recorded as rejected.
If none answered, the block stays pending and is sent to every node again each second, oldest first, until
one answers. A block still unanswered when the pool stops is found at the next start, as a row with no
result, and resubmitted from its hex in `blocks/`.

The hex file is synced to disk before the rename, and its directory after it, so a block whose row survives
a power loss has its hex beside it.

## After Submission

[lifecycle.rs](../../../src/pool/lifecycle.rs) follows each found block's status (confirmed, mature,
orphaned) with `getblockheader`. It asks the node the current work comes from, or the primary while none is
selected, on each new tip and after each answer the submitter records. How that status moves balances is in
[Accounting](accounting.md).
