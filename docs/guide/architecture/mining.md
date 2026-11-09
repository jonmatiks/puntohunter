# Mining

This page follows work through the mining core: how a block template becomes a job, how a job's coinbase
is laid out and how it splits the reward, how a miner's submission is rebuilt into a header and checked as
a share, and how a share that meets the network target becomes a block. The code is in
[src/mining/](../../../src/mining/), which both Stratum front-ends share; [Stratum](stratum.md) covers the
sessions that call it. The module is named `mining`, not `core`, to stay clear of Rust's `core` crate.

```text
template (node) ──► JobCache ──► Job ──► Stratum session ──► miner
                     │            │ coinbase parts, merkle branch, split
                     │            ▼
miner ──► Submission ──► Job::header ──► share::check ──┬─► below target: rejected
                                                       ├─► meets target: credited
                                                       └─► meets network target: block::assemble
                                                                                 ──► submitter
```

## Templates and Jobs

The node watcher publishes the current work on a `tokio::sync::watch` channel as `Work`, defined in
[template.rs](../../../src/node/template.rs): either `Work::None`, when no node may source templates, or a
`Template`. Each `Template` carries a sequence number `seq`, counted across every node, a `clean` flag set
when the prevhash is new, and the node's `BlockTemplate` with its version normalized (below). The watcher
itself is described under [nodes](nodes.md).

A `Job`, in [job.rs](../../../src/mining/job.rs), is the work one template yields for one pool and one
payout script. It holds the template's height, prevhash, version, bits, and `curtime`, the coinbase split
into its parts, the reward `Split` its outputs pay, the coinbase's merkle branch, and the template's other
transactions.

`JobCache` builds the jobs and shares them across every session. Its key is the pool, the coinbase
signature text, the miner payout script, and the worker's own finder part:

* The part enters the key only on a pool with a `personal` range; elsewhere it changes nothing, so a worker
  that asks one shares its address's jobs.
* On a pool that cannot pay a finder, the session passes the pool wallet as the payout script, so its
  workers share one job unless the pool writes worker names into the coinbase.
* The cache holds one template at a time. A higher `seq` clears it; a session still catching up on an older
  template gets a job built for it alone, without evicting the newer jobs.

What every job on one template shares, whatever it pays, is built once per template as `Shared`: the merkle
branch and the transactions. `Transactions` keeps the template's transactions serialized back to back in
one allocation, not one per transaction. Each session keeps the jobs of up to 16 templates, and the code
notes that thousands of small allocations freed on other threads fragment glibc's per-thread arenas.

## The Coinbase

[coinbase.rs](../../../src/mining/coinbase.rs) builds the coinbase as `CoinbaseParts`: `coinb1` and `coinb2`,
the transaction serialized without its witness, split around 12 extranonce bytes. A miner, or the pool on
its behalf, joins them with an extranonce to get the transaction its txid hashes.

```text
coinb1: version 2 | 1 input | null outpoint (32 zero bytes, index 0xffffffff)
        | scriptSig length | BIP34 height | push: signature | push of 12 bytes ...
extranonce: 4-byte prefix (pool) | 8 bytes (miner)
coinb2: sequence 0xffffffff | outputs | lock time 0
```

* **The scriptSig** is the BIP34 height, a push of the signature, then the 12-byte push that holds the
  extranonce. Compile-time assertions keep the longest signature within a single-byte push (75 bytes) and
  the whole scriptSig within consensus's 100 bytes.
* **The signature** (`Signature`) is the configured tag, `/puntohunter/` by default and at most 32 bytes.
  Where the pool writes worker names, under `worker_names` or under `worker_names_opt_in` for a worker that
  asked with `+name`, it is followed by `Mined by <worker>/`, the name cut to 32 bytes. Worker names are
  ASCII, so the cut always falls on a character boundary.
* **The outputs** are the finder's part, then the pool output, each left out when 0, then each fee in
  config order, then the template's witness commitment at value 0 when it has one.
* **The witness.** When the block commits to witnesses, `CoinbaseParts::transaction` adds the 32-byte zero
  reserved value. It is left out of `coinb1` and `coinb2`, since the txid does not cover it.

`build` is deterministic: identical inputs give identical bytes. Tests rebuild 72 recorded coinbases,
across modes, fee sets, finder script types, and the mainnet and regtest templates, from
[coinbases.json](../../../tests/data/mining/coinbases.json) byte for byte, so a refactor cannot change a
coinbase unnoticed.

## The Reward Split

A pool's coinbases pay what its `Rewards` say: its fees (`Fees`, taken from the config, never from code),
the pool wallet's script, its `finder` portion, and its optional `personal` range. `Rewards::pay` splits
the template's `coinbasevalue`, R, with `split`:

1. Each fee takes its portion of R, floor(R × ppm / 10^6) or its fixed amount, capped at what the fees
   before it leave.
2. The miner portion is what the fees leave, rounding remainders included.
3. The finder takes its portion of R, capped at the miner portion; the pool output takes the rest.
4. Only when both would be paid: a pool output below the pool wallet's dust limit goes to the finder;
   else a finder's part below its own script's dust limit goes to the pool output. An output that takes
   the whole miner portion is paid even below its limit.

The dust limits come from each script's `minimal_non_dust`. On a pool with `personal`, the finder's
portion is the worker's own part, or `finder` when it asked none, clamped in satoshis of this reward by
`Personal::clamp`: raised to `min`, then lowered to `max`, so `max` wins where a percent bound and a
fixed one cross.

The `Split` stays on the job. `Job::paid` gives the finder and pool amounts, which a found block records
and which decide whether a share is journaled. [Pools and modes](../pools-and-modes.md) explains the rule
for operators, and [accounting](accounting.md) what happens to the pool output.

## Pools

[pool.rs](../../../src/mining/pool.rs) holds `PoolId` and `Pool`. A `PoolId` is 1 to 32 lower-case letters,
digits, and `-`, cheap to clone, and stored with every block, share, balance, and worker handle. It is
never `fleet`: a worker handle hashes `<id>:<username>` and a fleet handle `fleet:<address>`, so a pool of
that id would give a worker with a bare address its fleet's handle. An id read back from the database
parses whatever the config holds, so a pool no longer configured still shows.

`Pool` is built once from the config and shared as an `Arc`. Its methods answer the questions the rest of
the engine asks:

| Method | Answers |
| --- | --- |
| `can_pay_finder` | Whether a block can pay a finder output: unless `finder`, or the `personal` maximum, is 0 |
| `can_pay_pool` | Whether a block can pay a pool output, so shares are journaled: unless `finder`, or the `personal` minimum, is 100% |
| `writes_name` | Whether a worker's name goes into the coinbase, given whether it asked with `+name` |
| `kind` | Solo, pooled, hybrid, or personal, which only the portal's wording reads |
| `transports` | V1, V2, or both, as its listeners say |

## Extranonce Prefixes

Every V1 connection and every V2 channel holds an `ExtranoncePrefix` from the one `ExtranonceAllocator`
in [extranonce.rs](../../../src/mining/extranonce.rs): 4 bytes unique among the live ones, so no two
connections or channels ever search the same coinbase. The allocator counts on from the last prefix it
handed out, skipping any still live, so a released prefix does not return at once. Dropping the prefix
releases it.

The miner rolls the other 8 bytes: V1's extranonce2, or an extended V2 channel's extranonce. On a standard
V2 channel the pool fills them with zeros.

## Headers and the Merkle Branch

[merkle.rs](../../../src/mining/merkle.rs) computes the branch for the coinbase's position once per
template, from the template's txids: the sibling hashes from the coinbase up to the root. `merkle::root`
folds a coinbase txid up that branch. Tests check it against a recorded mainnet block and against
rust-bitcoin for every transaction count up to 17.

A miner returns a `Submission`: the full 12-byte extranonce (the pool's prefix, then the miner's bytes),
ntime, nonce, and rolled version bits. `Job::header` rebuilds the header it claims:

* The version is BIP310's reconstruction, `rolled_version`: the job's bits outside the negotiated mask,
  the miner's inside it.
* The merkle root is the coinbase's txid, with that extranonce, folded up the job's branch.
* The time and nonce are the miner's; prevhash and bits are the job's.

The node watcher clears the in-mask version bits the template does not require (`normalized_version`),
so a V1 client that reads BIP310 as OR and one that reads it as XOR build the same header.

`Job::notify` serializes a job as V1's `mining.notify` params, with the prevhash in Stratum's order, each
4-byte word reversed. The V2 channel builds its own messages from the same fields.

## Targets and Difficulty

[target.rs](../../../src/mining/target.rs) converts between Stratum difficulty and 256-bit targets.
Difficulty d means the target diff1 / d, where diff1 is 0xffff · 2^208. `from_difficulty` splits 0xffff / d
into a 53-bit mantissa and a power of two, so the target is the mantissa shifted, with no 256-bit division.
A difficulty below 1 gives a target above diff1, saturating at 2^256 − 1.

`work` is the expected number of hashes a share at a target stands for, 2^256 / (target + 1), in `f64` for
telemetry. A share is credited at the target it was assigned, never at the difficulty its hash happened to
reach. On V2 that is the easier of its job's target and its channel's current one ([Stratum](stratum.md)).

## Checking a Share

[share.rs](../../../src/mining/share.rs) does the one check every share takes: `share::check` hashes the
header with double SHA-256 and returns a `Checked` with the hash, whether it meets the share target, and
whether it meets the network target of the header's bits, which makes it a block. The hash also keys
duplicates.

The protocol checks around it, and what happens to a share after it, live in
[stratum/mod.rs](../../../src/stratum/mod.rs) and are covered under [Stratum](stratum.md#the-shared-core).

## Blocks

When a share meets the network target, `block::assemble` in [block.rs](../../../src/mining/block.rs) builds
the whole block from the job alone, as `submitblock` takes it in hex: the header the share claims, the
transaction count, the coinbase with its witness, then the template's transactions as stored. The node is
never asked to put the block together. A test rebuilds a recorded 66 KB mainnet block byte for byte from
its parts.

The share check sends the block to the submitter ([submitter.rs](../../../src/pool/submitter.rs)) as a
`FoundBlock` before anything else happens to the share, so a slow journal write cannot delay a block. A
resent copy of the same share is refused as a duplicate, so a block is sent once.

## Vardiff

[vardiff.rs](../../../src/mining/vardiff.rs) holds the per-connection, or per-channel, difficulty
controller. `Vardiff` aims at one share per configured `share_interval`, clamped between the configured
`floor` and `ceiling`.

* **Start.** At the miner's hint, clamped to the bounds, or at the floor without one.
* **Estimate.** It sums the difficulty of each accepted share at the difficulty its own job assigned, so
  shares still arriving on older, easier jobs after a change do not skew the estimate.
* **When.** It retargets once the window holds 10 shares' worth of work at the current difficulty, or once
  the window spans the 10 intervals they were due in. `on_tick`, called every interval, covers a miner that
  went quiet and sends nothing to trigger a retarget.
* **How far.** The difficulty aimed at is the observed work rate times the interval. A step goes halfway
  there, geometrically (the square root of the ratio), and at most 4× up or down. A change under 15% is
  not sent.

The constants carry their reasons in the code: a 3-share window cut a real miner's difficulty 4× whenever
one share or none arrived, and the half step halves the noise of a 10-share sample while a far-off start
still ramps at the full 4×.
