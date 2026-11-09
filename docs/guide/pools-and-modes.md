# Pools and Modes

One PuntoHunter process serves any number of pools, side by side. Each pool is a `[[pools]]` entry
in the config, with Stratum ports of its own, and every pool follows one rule: a block pays its
finder the pool's finder part of the reward, in the block's coinbase, and pays the pool wallet the
rest, which is shared by work among the pool's miners. Solo, pooled, and hybrid are not separate
engines: they are what that one rule does at a finder part of 100%, 0%, or something in between.

Pooled and hybrid pools pay their miners' balances in batched on-chain payouts, which `[payouts]`
configures; without it, balances accrue and nothing is paid. [Configuration](configuration.md) lists
every key; this page explains what the pool keys mean.

## Pools

Each entry defines one pool. The portal lists pools in config order.

```toml
[[pools]]
id = "solo"
name = "Solo"
v1 = "0.0.0.0:3333"
v2 = "0.0.0.0:3334"
finder = { percent = 100 }

[[pools]]
id = "pooled"
name = "Pooled"
v1 = "0.0.0.0:4333"
v2 = "0.0.0.0:4334"
finder = { percent = 0 }

[[pools]]
id = "hybrid"
name = "Hybrid"
v1 = "0.0.0.0:5333"
v2 = "0.0.0.0:5334"
finder = { percent = 33.33 }
```

These three are the defaults of [config/mainnet.toml](../../config/mainnet.toml). Add, remove, or
change pools freely; at least one is required.

| Key | Required | Meaning |
| --- | --- | --- |
| `id` | yes | 1 to 32 lower-case letters, digits, and `-`, never `fleet`; stored with every block, share, balance, and worker handle |
| `name` | no | What the portal calls the pool, up to 64 characters; the id when unset |
| `v1`, `v2` | at least one | The Stratum V1 and V2 listen addresses; a protocol left out has no listener |
| `public_v1`, `public_v2` | no | The port miners reach, when a proxy or port forward differs from the listener's; only beside its listener |
| `finder` | yes | What a block pays its finder: a portion, below |
| `personal` | no | A range each worker may choose its own finder part in |
| `fees` | no | The pool's own fee outputs, in place of the global `[[fees]]`; `fees = []` for none |
| `accounting` | no | The pool's own PPLNS window, `window_multiplier` and `window_min_work` |
| `coinbase` | no | The pool's own `worker_names` and `worker_names_opt_in` |

* The port a miner connects to picks the pool. Switching pools means reconnecting to the other port.
* No two listeners share a port, so a pool serving both protocols takes two: V2 opens with a Noise
  handshake, and V1 is line-delimited JSON.
* Changing a pool's `id` starts a new pool, with new worker handles and an empty window. Removing a
  pool, or changing its id, stops its mining only: its blocks and balances carry on, and the portal
  names it by its id.
* A change to a pool's finder part, fees, or window applies to jobs issued after the restart that
  makes it. Shares already journaled keep what they were mined at.

## The Finder Part

`finder`, a fee, and each `personal` bound are portions, written in one of three forms:

* `percent`: a percent of the block's whole reward, from 0 to 100, to four decimal places, as
  `{ percent = 33.33 }`.
* `btc`: a fixed amount, to eight decimal places, as `{ btc = 0.5 }`.
* `sats`: a fixed amount in whole satoshis, as `{ sats = 10_000_000 }`.

A fixed amount does not halve with the subsidy, so it becomes a larger part of the reward after each
halving. A fixed `finder` is 0 or at least 330 sats, the dust limit of a P2TR output. A `finder`
percent is either 100, or it and the fees' percents total less than 100%, so the pool always pays a
pool output or never does.

### How a Block's Reward Is Split

For a block whose reward is R, its subsidy plus its transaction fees:

1. Each fee takes its portion of R, in config order, each capped at what the fees before it leave.
2. The miner portion M is what the fees leave.
3. The finder takes its part of R, capped at M. The pool output is the rest of M.
4. Only when both would be paid: a pool output below the pool wallet's dust limit goes to the
   finder, or else a finder part below its own address's dust limit goes to the pool output.

The coinbase pays the finder's part to the address in the finder's username, then the pool output to
`[addresses] pool_wallet`, then each fee. An output of 0 is left out, so a solo coinbase has no pool
output and a pooled coinbase has no finder output.

### Solo, Pooled, and Hybrid

The portal names each pool's kind from its `finder`, to word its rule:

| Kind | `finder` | What a block pays | Shares |
| --- | --- | --- | --- |
| Solo | `percent = 100` | The finder the whole miner portion, to its own address | Counted for statistics only |
| Pooled | 0, in any form | The pool wallet the whole miner portion, shared by work | Journaled for the window |
| Hybrid | A percent in between, or a fixed amount | The finder its part; the pool wallet the rest, shared by work | Journaled for the window |
| Personal | Any, with `personal` set | The finder the part it chose; the pool wallet the rest | Journaled where the job pays a pool output |

* On a solo pool, the pool never holds the reward: it sits in the finder's coinbase output,
  spendable after 100 confirmations.
* On a pooled pool, every coinbase pays the pool wallet, so connections share one job, unless the
  pool writes worker names into its blocks.
* On a hybrid pool, the finder's work stays in the window, so the finder also takes its share of the
  pool output. A miner's expected return per unit of work is then the same as on a pooled pool; only
  its variance differs.
* The fleet and worker pages show the odds of a block on a pool that can pay a finder, and a share
  of the window on a pool that can pay a pool output.

## Personal Parts

A pool with `personal` lets each worker choose its own finder part:

```toml
[[pools]]
id = "split"
name = "Your split"
v1 = "0.0.0.0:6333"
v2 = "0.0.0.0:6334"
finder = { percent = 33.33 }
personal = { min = { percent = 0 }, max = { percent = 50 } }
```

* Each bound is a portion, as `finder` is; left out, `min` is 0% and `max` is 100%. The two bounds
  may be in different forms, such as `min = { sats = 100_000 }` beside `max = { percent = 25 }`.
* A worker chooses its part after a `+` in its username, as `<address>.<worker>+50`, `+0.5btc`, or
  `+50000000sats` ([mining](mining.md)). `finder` is the part of a worker that chooses none.
* A part outside the range is paid at the nearer bound. The clamp runs per job, in satoshis of that
  job's reward: the part is raised to `min`, then lowered to `max`, so `max` wins where a percent
  bound and a fixed one cross.
* On a pool without `personal`, a part in the username is accepted and has no effect: every worker
  gets `finder`.
* The worker page shows the part its coinbase pays for a block found now.

Because the window weighs each share by what its job leaves to the pool (below), every part earns
the same in expectation. Choosing a part is choosing variance, not return. `max` keeps a pool a
pool: in a window that is mostly high-part work, a worker keeping 0% is paid mostly from its own
blocks.

## Fees

`[[fees]]` lists the fee outputs every block pays after the miner portion, in order:

```toml
[[fees]]
name = "operator"
address = "bc1q…"
percent = 1.0
```

* Each fee has a `name`, which the portal shows, an `address`, and a portion of the whole reward. A
  percent is rounded down to the satoshi. A fixed amount is at least its address's dust limit.
* The fees' percents total less than 100%. Each fee is capped at what the fees before it leave.
* List as many as needed, or none, as the default config does, to give the miner portion the whole
  reward. Each output adds about 31 bytes to the coinbase, which the node leaves little room for, so
  keep the list short.
* A pool may list `[[pools.fees]]` of its own, which replaces the global list whole, or set
  `fees = []` for none.

The portal names each pool's fees by name and portion, never by address.

## The PPLNS Window

A pooled or hybrid block's pool output is shared by difficulty-weighted PPLNS over a window of the
pool's recent accepted work. Each pool keeps a window of its own, which no other pool's shares
enter.

```toml
[accounting]
window_multiplier = 2.0
window_min_work = 0.0
maturity_buffer = 0
```

* The window covers the last W of accepted work, W = max(`window_multiplier` × the network
  difficulty of the solved job, `window_min_work`), in difficulty-1 shares. `window_multiplier`
  defaults to 2; `window_min_work` has no default. The floor exists for regtest, where twice the
  network difficulty is a fraction of one share.
* A pool may set either key under `[pools.accounting]`, each falling back to `[accounting]`.
  `maturity_buffer` is global only.
* A share counts the work of the target it was assigned, not the difficulty its hash happened to
  reach, so a vardiff change never alters its credit.
* Each share's work is weighted by its job's contribution: the pool output over the miner portion of
  the job it was mined on. On a pooled pool every share counts in full. On a hybrid pool at 33.33%
  with no fees, every share counts two thirds, so the window holds 1.5 times as much plain work. A
  share whose job pays no pool output earns nothing from any window and is not journaled.
* The window is fixed when a block's solution share is journaled. It walks back from that share,
  inclusive, until W is covered, crediting the oldest share only the part W still needs. A window
  holding less than W, as after a pool's first start, divides by what it holds.
* Each address gets its floor of the pool output by its credited work, with the satoshis the floors
  leave going one each by largest remainder, ties to the lower address. Workers paying one address
  are credited together.
* A pooled or hybrid share is written to the journal before the miner hears it was accepted. If the
  journal cannot commit, pooled credit stops until the pool restarts; solo mining carries on.

A window has no round: it never resets on a block, so joining or leaving at any moment gains
nothing.

## Worker Names in Blocks

`[coinbase] tag` is what block explorers show as the pool's name: 1 to 32 printable ASCII
characters, `/puntohunter/` when unset. It is global.

Two flags decide whether a block also names the worker that found it, as `Mined by <worker>/` after
the tag:

| `worker_names` | `worker_names_opt_in` | A worker's name in its blocks |
| --- | --- | --- |
| `false` | `false` | Never, the default |
| `false` | `true` | When the worker adds `+name` to its username |
| `true` | `false` | Always |
| `true` | `true` | Refused at startup |

* Set either under `[coinbase]` for every pool, or under a pool's `[pools.coinbase]` for that pool.
  The refusal applies after the fallback, so a pool that sets `worker_names_opt_in` under a global
  `worker_names = true` must also set `worker_names = false`.
* The name is the worker name after the address's dot, without the options, cut to 32 characters. A
  worker mining as a bare address has none, and its blocks carry the tag alone.
* A name written to a block stays on the chain for good, beside the finder's address where the pool
  pays a finder. That is why both flags are off by default.

## Balances and Payouts

A pooled or hybrid block's pool output is allocated to the addresses in its window. The finder's own
part is a coinbase output, which the pool only tracks and never holds.

* An allocation is pending from the block onwards, and becomes an available balance once the block
  has more than 100 + `maturity_buffer` confirmations. The overview states the number.
* If a block leaves the chain, its allocations are reversed, and restored if it returns. If a
  balance it credited was already reserved or paid, its credits stand instead, and payouts pause
  until the operator lifts the pause ([When Payouts Pause](operations.md#when-payouts-pause)).
* Without `[payouts]`, balances accrue and are kept, and nothing is paid. The overview says payouts
  are not enabled.

With `[payouts]`, a batch runs every `interval_secs`. On regtest it pays a fixed fee rate; on mainnet,
`confirmation_target` takes the place of `fee_rate`:

```toml
[payouts]
wallet = "payouts"
threshold_sats = 100_000
interval_secs = 60.0
outputs_per_batch = 100
fee_rate = 2.0
fee_rate_cap = 10.0
```

* `wallet` is a Bitcoin Core wallet on the primary node, which holds the pool wallet's key and
  nothing else.
* An address is paid once its available balances in every pool together reach `threshold_sats`, at
  least 330. A smaller balance carries forward and never expires.
* Each batch is one transaction paying each address its whole balance in one output, up to
  `outputs_per_batch` addresses (at most 1,000) in address order, with the change back to the pool
  wallet.
* Miners are paid in full. The transaction fee comes from the operator's fee reserve, which
  `puntohunter reserve credit <sats> --config <file>` credits with funds put in the payout wallet.
  The batch pays `fee_rate` sat/vB on regtest, or on mainnet the node's fee estimate for
  `confirmation_target` blocks, or the node's minimum if that is higher. A rate above `fee_rate_cap`, a
  mainnet node with no estimate, or a reserve too small defers it.
* A batch is confirmed at 6 confirmations. A batch is sent once: after a crash or an RPC timeout the
  pool sends the same signed transaction again, never a second payment.
* At startup the pool checks its books against the payout wallet before paying, and halts payouts if
  they disagree.

A miner sees its balances on the portal's balance page, which opens with a token the operator
issues: `puntohunter token issue <address> --config <file>` prints it once, and the pool keeps only
its hash.

Without `[payouts]`, a pooled or hybrid pool allocates its blocks to balances that nothing pays, so
run one only with it. On mainnet a batch pays the node's fee estimate, and the payout wallet is best
encrypted, so that nothing on the host can spend from it; [configuration](configuration.md) describes
both. [Operations](operations.md) covers the payout wallet and the reserve.
