# Accounting

This page follows a pooled or hybrid miner's money from an accepted share to an on-chain payout: the
share journal, the PPLNS window that divides a block's pool output, the append-only ledger that turns
allocations into balances, the payout batches that pay them, the payout wallet, and the reconciliation
that checks the books against the wallet and the chain. The code is in [src/pool/](../../../src/pool/):
[journal.rs](../../../src/pool/journal.rs), [pplns.rs](../../../src/pool/pplns.rs),
[ledger.rs](../../../src/pool/ledger.rs), [blocks.rs](../../../src/pool/blocks.rs),
[lifecycle.rs](../../../src/pool/lifecycle.rs), [submitter.rs](../../../src/pool/submitter.rs),
[payouts.rs](../../../src/pool/payouts.rs), and [reconcile.rs](../../../src/pool/reconcile.rs), with the
wallet client in [wallet.rs](../../../src/wallet.rs) and balance tokens in
[tokens.rs](../../../src/tokens.rs).

How a block's reward is split into a finder output and a pool output is in [mining](mining.md); how a
block reaches the nodes is in [nodes](nodes.md); the tables and commit durability are in
[storage](storage.md). The operator's view of the same rules is in [pools and modes](../pools-and-modes.md)
and [operations](../operations.md).

Solo blocks touch almost none of this. A solo coinbase pays the finder directly, so the pool records the
block's status and holds nothing.

## From Share to Payout

```text
 share accepted ──► journal row, sequence n ──► miner hears "accepted"
                          │
 block recorded ──────────┤  (whichever of the two is written second)
                          ▼
            snapshot + allocations: the window ending at n, fixed for good
                          │
                          ▼
 ledger:  pending ── confirmations > 100 + maturity_buffer ──► available
             │                                                  │
   block leaves the chain: reversal               payout batch: reserved
                                                                │
                                         transaction 6 deep: paid
```

Every arrow is a database commit. A balance is never a stored number: it is the sum of ledger entries.

## The Journal

[journal.rs](../../../src/pool/journal.rs) records every share whose job pays a pool output, in one
sequence across all pools. `Context::credit` in [src/stratum/mod.rs](../../../src/stratum/mod.rs) awaits
`Journal::record` before it acknowledges the share, so a pooled or hybrid share is in the journal before
the miner hears it was accepted. A share whose job pays no pool output, as on a solo pool, earns from no
window and is acknowledged without a journal row.

* **One writer.** Sessions send each `Entry` to one writer task and await its reply. The task commits
  whatever is queued, up to `MAX_BATCH` (256) shares, in one transaction, and waits for no more, so a
  lone share commits at once. Shares queued together keep their arrival order.
* **The sequence.** `journal.sequence` is an `AUTOINCREMENT` key, so it rises across restarts and is
  never reused, even for a deleted row. A refused insert may spend a number, so sequences need not be
  consecutive. The window walks this order.
* **Duplicates.** `proof`, the header hash, is unique. A proof already journaled, from any session or
  before a restart, is refused as `Refusal::Duplicate` and the rest of its batch commits. A share is
  credited once, whichever session sends it.
* **What a row keeps.** The pool, payout address, worker handle, template and job, the job's network
  `bits`, its miner portion and pool output (`miner_sats`, `pool_sats`), the target the share was
  credited at, and its policy row. Because the row carries its job's amounts, neither a later config
  change nor a worker's later choice of finder part changes the share's credit.
* **Policies.** `journal::policy` records each pool's window settings (`window_multiplier`,
  `window_min_work`) and the accounting version as a row of `policies` at startup; pools with the same
  settings share a row. A share is credited under the policy of the process that issued its job.
* **Durability.** A journal commit returns once written, which survives a crash of the process, and is
  synced to disk within half a second; [storage](storage.md) covers the two commit kinds.

### When the Journal Cannot Write

A failed commit sets a flag that refuses every later share with `Refusal::Failed`, and the session
refuses the share, as `Pool accounting unavailable` on Stratum V1, instead of acknowledging it. The flag
clears only on restart. Solo mining carries on, since it needs no journal. The rule this keeps: no pooled
or hybrid share is acknowledged that is not in the journal. The health check's `pooled_credit` reads
`stopped` and `puntohunter_journal_failed` is 1 ([operations](../operations.md)).

A block found during the failure is still submitted, since a share that solves a block goes to the
submitter before its journal commit. Its solution share is never journaled, so until the restart it waits
unallocated, in the `puntohunter_accounting_backlog_blocks` count. At the next start,
`pplns::cut_off_unjournaled` gives it a window cutoff before the journal opens: its pool's newest share
journaled when it was found, which is all the pool credited before it, since the shares refused during the
failure were never acknowledged. It is then allocated as any other block. The same covers a crash between
a block's record and its solution share's journal commit.

Nothing in the engine deletes journal rows: the journal grows with accepted pooled and hybrid work.

## PPLNS

[pplns.rs](../../../src/pool/pplns.rs) divides a block's pool output by work over a window of its pool's
journal. The operator-facing rules are in [pools and modes](../pools-and-modes.md#the-pplns-window);
this section is how the code computes them.

### Work, Exactly

`pplns::work` gives a share's work as the expected hash count of its assigned target, in units of
2^-16 hashes: 2^256 / (⌊target / 2^16⌋ + 1), held in a `u128`. Whole hashes would round the tiny
regtest shares used in testing to one or two hashes; the finer unit keeps their proportions. A window at
mainnet difficulty is about 2^96 units, so a `u128` has room for difficulty to grow about four billion
times. Work is stored as decimal text in `snapshots`, since it outgrows SQLite's 64-bit integers.

The share counts the work of its assigned target, never the difficulty its hash happened to reach, so a
lucky hash or a vardiff change does not alter its credit.

### Weighted Work

Under the current policy version, `WEIGHTED` (2), each share counts
`weigh(work, miner, pool)` = ⌊work × pool / miner⌋: its work scaled by its job's pool output over its
miner portion. The 192-bit product is split so every step stays within 128 bits. On a pooled pool every
share counts in full. On a hybrid pool, or under a worker's personal part, a share counts only the part
of its job that pays the pool, which is why every finder part earns the same in expectation. Version 1,
plain work, is still read for any block whose solution share was journaled under it.

### The Window

`pplns::size` sets W = max(`window_multiplier` × the work of the solved job's network difficulty,
`window_min_work` difficulty-1 shares), at least 1. These floating-point products only place the window's
boundary, and the same inputs always give the same W.

`pplns::window` walks the pool's journal newest first, starting at the solution share, inclusive, and
credits each share's work until W is reached. The oldest share in the window is credited only the part W
still needs, recorded as `boundary`. A journal that holds less than W, as after a pool's first start, is
taken whole and divided by what it holds. Credits are summed per payout address, so workers paying one
address count together. No other pool's shares enter the window.

### Allocation

`pplns::allocate` gives each address ⌊amount × credit / total⌋ satoshis, computed exactly by `mul_div`,
then hands out the satoshis the floors leave one at a time by largest remainder, ties to the lower
address. The allocations sum to the pool output exactly. An address allocated nothing gets no row.

The window and its allocations are written once, as a `snapshots` row and its `allocations` rows, and
never changed. Shares accepted after the solution share cannot alter the block's allocation.

`pplns::current` walks the same way from the newest share to report the window a block found now would
take; the portal shows each worker's part of it as an estimate, never as a balance.

## Pairing a Block With Its Window

A block is allocated once two facts are recorded: the block itself, written by the submitter
([submitter.rs](../../../src/pool/submitter.rs)) through `blocks::insert`, and its solution share,
written by the journal. They come from separate tasks and either may land first. The block's `cutoff`,
the solution share's sequence, is set by whichever is written second: `blocks::insert` looks the proof up
in the journal, and the journal's insert updates a block row that is already there.

`pplns::allocate_ready` then snapshots and allocates every block that has a cutoff, a pool output, and no
snapshot. It runs in three places:

* after the journal commits a solution share, in a commit of its own;
* after the submitter records a block with a pool output, in a commit of its own;
* at startup, before the journal opens, for a crash that came between the two.

Each run is one transaction, and the "no snapshot yet" condition makes a second run a no-op, so a block is
allocated exactly once. Allocation is kept out of the block's own record commit, so a failed allocation
never costs the record of the block. A block the node rejected, or one off the chain when it is allocated,
still gets its snapshot and allocations, but they are not credited unless it returns.

## The Ledger

[ledger.rs](../../../src/pool/ledger.rs) turns allocations into balances with append-only entries.
Triggers on the `ledger` table abort any `UPDATE` or `DELETE`, so history is never rewritten: every change
is a new entry. Each entry moves satoshis of one address and pool, or of the operator's fee reserve, which
has neither.

| Kind | Belongs to | Effect |
| --- | --- | --- |
| `pending` | a block's allocation | Credits the allocation while its coinbase cannot be spent |
| `available` | a block's allocation | Moves it from pending to available |
| `reversal` | a block's allocation | Takes back a pending or available credit, named in `reverses` |
| `reserved` | a payout batch | Moves available satoshis into the batch; or a batch's fee out of the reserve |
| `paid` | a payout batch | Moves reserved satoshis out, paid; or marks the fee spent |
| `credit` | the fee reserve | Credits the reserve, from `puntohunter reserve credit` |

The `balances` view sums these per address and pool into `pending`, `available`, `reserved`, and `paid`.
Check constraints tie each kind to what it may name: a block for allocation entries, a batch for
`reserved` and `paid`, no address for `credit`.

### Settling a Block

All of a block's allocations move together. `ledger::settle` reads where each allocation stands from its
entries, and writes only the entries needed to bring it to the stage the block's status calls for
(`Stage::Void`, `Pending`, or `Available`):

| From | To | Entries written |
| --- | --- | --- |
| Void | Pending | `pending` |
| Void | Available | `pending`, `available` |
| Pending | Available | `available` |
| Pending | Void | `reversal` of pending |
| Available | Void | `reversal` of available, unless the block's balances were spent |
| Available | Pending | nothing: an available allocation never goes back |

Because it computes the stage from the ledger rather than trusting a flag, `settle` is idempotent: run
twice, the second run writes nothing. A block is credited, released, or reversed once, whichever task runs
it again, and a block that leaves the chain and returns is credited again from `Void`.

### A Reorg After Payment

A reversal must not debit money that has already left. `settle` checks whether any address of the block
had a `reserved` or `paid` entry after the block's `available` entry. If so, it reverses nothing: the
credits stand, a row is added to `pauses`, and payouts pause. Taking the satoshis back would mean debiting
someone else's balance, so the decision is left to the operator. While any `pauses` row is not lifted,
the payout worker neither reserves a batch nor takes an unconfirmed one forward. Once the operator has
settled the cause, `puntohunter payouts resume` lifts every pause (`ledger::resume`), and a running pool
resumes at its next interval. A lifted row stays, so reconciliation still accepts that block's standing
credits; if the block leaves the chain again, its pause is set again. The `puntohunter_payouts_paused`
metric follows the pauses at each interval.

## Block Lifecycle

[lifecycle.rs](../../../src/pool/lifecycle.rs) keeps each block's status in line with the node the work
comes from, asking `getblockheader` for its confirmations on each tip change and after each answer the
submitter records. [blocks.rs](../../../src/pool/blocks.rs) holds the statuses:

| Status | Meaning | Allocations |
| --- | --- | --- |
| `submitted` | Sent, not yet answered or seen | Pending once allocated |
| `rejected` | Every node that answered refused it | Void; final |
| `confirmed_immature` | 1 to 100 confirmations | Pending |
| `mature` | More than 100 confirmations: its coinbase is spendable | Pending, then available |
| `orphaned` | Off the active chain | Void until it returns |

An allocation becomes available once its block has more than 100 + `maturity_buffer` confirmations
(`Stage::of`). The status change and the ledger entries are written in one commit, so the books never show
a status without its money.

Blocks more than 200 below the tip are no longer asked about, which keeps a fast regtest chain from
querying thousands of blocks per tip change. A block whose allocations are still pending is followed
however deep it is (the `unreleased` view). A block the node no longer knows at all is left as it is.
Each status change uses the selected node's view; which node that is, and how nodes are chosen, is in
[nodes](nodes.md).

## Payout Batches

[payouts.rs](../../../src/pool/payouts.rs) runs only with `[payouts]`. Once every `interval_secs`, the
worker first takes every unconfirmed batch forward, then reserves the next one if any address is due.

### Choosing and Funding

`payouts::due` sums each address's available balances across all pools and takes those that reach
`threshold_sats` and are not dust in an output to that address, in address order, up to
`outputs_per_batch`. Each payee gets one output with its whole balance; smaller balances carry forward.

The fee rate is the configured `fee_rate` on regtest, or the node's `estimatesmartfee` for
`confirmation_target` on mainnet, raised to the node's mempool minimum either way. A rate above
`fee_rate_cap`, or no estimate, defers the batch. The wallet then funds a transaction with
`fundrawtransaction` from confirmed outputs it has not locked, with change to `[addresses] pool_wallet`.

### Reserving

`payouts::reserve` writes the batch in one transaction:

* the `batches` row, state `reserved`, with the unsigned funded transaction;
* a `batch_inputs` row for each input, whose primary key lets an outpoint belong to one batch only;
* a `reserved` entry for each payee's balance in each pool, and one for the fee from the reserve.

Before writing, it rechecks that payouts are not paused, that each payee's available balance is exactly
what was funded (`Deferral::Changed` otherwise), and that the reserve has the fee free. A deferred plan
writes nothing and the next interval tries again. Miners are paid in full: the fee comes from the
operator's reserve, never from a balance.

After the commit the worker locks the inputs in the wallet with a persistent `lockunspent`, so no later
batch can be funded with them.

### One Payment, Whatever Fails

A batch only moves forward:

| State | What is true |
| --- | --- |
| `reserved` | Balances, fee, and inputs held; `funded` is the unsigned transaction |
| `signed` | `tx` is the signed transaction, written before it is first sent; the only bytes ever sent |
| `broadcast` | The node took `tx` |
| `confirmed` | `tx` is at least 6 deep; the batch's `paid` entries are written in the same commit |

The signed transaction is persisted before its first broadcast. After a crash or an RPC timeout the worker
asks the wallet how deep that transaction is and, unless it is in a block, sends the same bytes again.
Resending is harmless: the node already has it, or answers that it is already in a block. A second
transaction is never funded for the same batch, so a batch makes one payment.

The batch is confirmed at `CONFIRMATIONS` (6). Until then, a reorg that drops the transaction leaves the
batch reserved, its balances and inputs held, and the worker sends the same bytes again. On each interval
and at startup, the worker relocks any input of an unconfirmed batch that the wallet lost or never took the
lock for, as after a node restart.

Signing keeps the txid, since every input the wallet holds is segwit; reconciliation relies on this to
find a reserved batch's transaction by its funded txid.

## The Fee Reserve

Batch fees come from the fee reserve, a ledger account with no address. `puntohunter reserve credit
<sats>` calls `ledger::credit_reserve`, which refuses the credit unless the wallet's spendable funds cover
what miners are owed, what the reserve already holds, and the new credit. Miners are owed every unpaid
balance, pending ones too, though their coinbases may not be spendable yet: the check errs toward
refusal, never toward spending miners' funds on fees.

The reserve has two figures. Its free part, for new batches, is its credits less every fee a batch
reserved. Its holding, which the books expect the wallet to hold, is its credits less the fees batches
paid.

## The Payout Wallet

[wallet.rs](../../../src/wallet.rs) talks JSON-RPC to the primary node's `/wallet/<name>` endpoint,
separately from the node client, which knows nothing of wallets. The wallet is meant to hold the pool
wallet's key alone, so all it can spend is the pool outputs of the pool's coinbases, never a finder's or a
fee address's output.

The engine never holds a private key or a passphrase. Bitcoin Core funds, signs, and keeps the input
locks. On mainnet the wallet is best encrypted and kept locked. Funding needs no key, so a batch is
funded and reserved as usual, and `Wallet::sign` maps Core's unlock-needed error to `SignError::Locked`.
The batch then waits in `reserved`, its balances and inputs held, and the worker's standing reads
`locked`. Once the operator unlocks the wallet outside the engine, the next interval signs and sends it.
The wait survives a restart because it is just a reserved batch. Later intervals may reserve further
batches from balances that became available since, and each waits the same way.

## Reconciliation

[reconcile.rs](../../../src/pool/reconcile.rs) runs when the payout worker starts, before its first
batch, and is retried each interval until the node answers. `reconcile::audit` checks the books alone:

* each block's allocations sum to the amount it allocated;
* each allocation is credited in full while its block may be on the chain, and not at all once it is
  rejected or orphaned, paused blocks aside, and its ledger entries carry its amount and pool;
* no balance is below zero, and the reserve never reserved more than its credits;
* each batch reserved its payees and its fee, and paid them once confirmed and not before.

`reconcile::compare` holds the books against the wallet:

* every input the wallet has locked belongs to an unconfirmed batch;
* each batch's transaction stands where its state says: a reserved batch's is not in the wallet, a
  broadcast one's is, a confirmed one's is on the chain, and no conflicting transaction confirmed;
* every transaction the wallet sent was made by a batch, and every coinbase it holds is from a block
  the pool recorded;
* the wallet holds at least what miners are owed plus the fee reserve, less what sent batches already
  spent.

Any mismatch halts payouts until a later start finds none. Mining and the journal carry on. A restored
database backup that lags the wallet fails these checks, so a restore needs no special detection to be
safe: a batch the backup forgot is a transaction the wallet sent that no batch made.

While running, each interval also compares what the wallet holds with what the books expect and reports it
as `puntohunter_wallet_covers_liabilities`; that check reports only and halts nothing.

## Balance Tokens

A miner's balances are shown only behind a token. [tokens.rs](../../../src/tokens.rs) issues a token
that opens one address's balances: 32 random bytes in hex, printed once by `puntohunter token issue`. The
database keeps only its SHA-256, so nothing stored can open a balance, and the token is never logged. The
portal takes it as a bearer token on `GET /api/v1/balance`, so it never appears in a URL
([portal](portal.md), [API](../api.md)). An address may hold several tokens.

## Failure Summary

| Event | What happens to the money |
| --- | --- |
| Crash before a share's journal commit | The miner was never told it was accepted; nothing is owed |
| Journal commit fails | Pooled and hybrid shares refused until restart; solo mining carries on |
| Crash between a block's record and its allocation | `allocate_ready` at startup allocates it |
| Block found while the journal cannot write | At the next start, its window ends at the last journaled share, and it is allocated |
| Node rejects a block | Its allocations, if any, are void; final |
| Block orphaned before payout | Pending or available credits reversed; restored if it returns |
| Block orphaned after its balances were reserved or paid | Credits stand; payouts pause until the operator runs `payouts resume` |
| Crash or timeout during a batch | The same signed transaction is sent again; never a second payment |
| Batch transaction reorged out | Batch stays unconfirmed, inputs relocked, same bytes resent |
| Payout wallet locked | Batch waits reserved until the wallet is unlocked |
| Books disagree with wallet or chain at startup | Payouts halted until a start finds them in agreement |
