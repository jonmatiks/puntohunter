# Storage

This page covers what the pool keeps on disk and when a write is safe: the SQLite store and its two kinds of
commit, when an accepted share is durable, the schema, and the database snapshots. The code is in
[src/store/](../../../src/store/): [mod.rs](../../../src/store/mod.rs) for the store,
[backup.rs](../../../src/store/backup.rs) for snapshots, and
[0001_schema.sql](../../../src/store/migrations/0001_schema.sql) for the schema. The share journal is in
[journal.rs](../../../src/pool/journal.rs). How the journal becomes payouts (PPLNS windows, allocations, the
ledger) is in [Accounting](accounting.md).

Everything lives in the data directory (`data_dir`):

| Path | What it holds |
| --- | --- |
| `puntohunter.sqlite3` | The database, in WAL mode, with its `-wal` and `-shm` files |
| `blocks/` | Each found block's hex, `<hash>.hex`, kept for resubmission ([Nodes](nodes.md)) |
| `backups/` | Database snapshots, with `[backup]`; the directory is configurable |
| `handle.key`, `sv2-authority.key` | Keys, not covered here ([Security](security.md)) |

## One Writer, Many Readers

`Store::open` opens the database, applies migrations, and starts one writer thread, named `store`, that owns
the only writable connection. Every write is a closure sent to that thread over a channel. The thread runs
it in a transaction and sends the result back on a one-shot channel, so code that must not go on before its
write is safe simply awaits the reply.

Each transaction begins `IMMEDIATE`, taking the write lock at once. A transaction that reads and then writes
therefore never meets another process's commit in between, such as the one `puntohunter reserve credit` makes
while the pool runs. SQLite's busy timeout, five seconds by default in `rusqlite`, waits for that other
commit to end. An error from the closure rolls the transaction back, and nothing of it is written.

The portal reads through a `Reader`: a read-only connection beside the writer, used from a blocking task. WAL
mode lets it read while the writer commits, so a page never queues behind a commit, and a write through it
fails.

## Two Kinds of Commit

The connection runs with `journal_mode = wal` and `synchronous = normal`. With those settings SQLite writes
each commit to the write-ahead log without syncing it; the store syncs the log itself, by calling
`sync_data` on its own handle to the `-wal` file. SQLite still syncs the log before each checkpoint.

| Call | Returns when | Used for |
| --- | --- | --- |
| `Store::commit` | The transaction is committed and the log is synced to disk | Everything but the journal |
| `Store::journal` | The transaction is written to the log, not yet synced | The share journal alone |
| `Store::write` | Never awaited; it is synced like `commit`, and a failure is logged | Writes nothing waits on |

A synced commit flushes the whole log, so it also makes every journal commit before it durable. While any
journal commit is unsynced, the writer syncs within `SYNC_AFTER`, half a second, of the first one, even if no
other commit comes. A failed sync is logged and tried again after another interval. The age of the oldest
unsynced journal commit is the `puntohunter_journal_unsynced_seconds` metric.

This gives two levels of safety:

* A journal commit survives a crash of the pool process as soon as it returns, since the data is in the
  operating system's file cache.
* A power loss or kernel crash can lose at most the journal commits of the last half second or so, and never
  a commit that moves money: those are all synced before they return.

## When a Share Is Durable

Only a share whose job pays a pool output is journaled, since only such a share earns from a window. That
is a share on a pooled or hybrid job; a share on a job that pays the finder everything is credited at once,
for statistics alone. For a journaled share, the order is:

```text
session          journal writer task          store writer thread
   |  record(entry)       |                          |
   |--------------------->|  batch of up to 256      |
   |                      |------------------------->|  INSERT ... ON CONFLICT (proof) DO NOTHING
   |                      |                          |  written to the WAL (not yet synced)
   |                      |<-------------------------|  sequence, or none for a duplicate
   |<---------------------|                          |
   |  answer the miner: accepted                     |  synced within 0.5 s, or by the next commit
```

1. The session calls `Journal::record` and waits.
2. The journal's writer task takes every share already queued, up to `MAX_BATCH` (256), without waiting for
   more, so a lone share is committed at once. The batch is one `Store::journal` transaction.
3. Each insert returns the share's `sequence`, or nothing when its header hash (`proof`) is already in the
   journal. A duplicate, from any session or from before a restart, is refused as a duplicate share, and the
   rest of its batch commits.
4. Only then does the session tell the miner the share was accepted.

So a share the miner saw accepted is in the journal, and survives a crash of the pool process. It reaches
the disk within about half a second.

If a journal commit fails, the journal latches into a failed state: every later pooled or hybrid share is
refused as unavailable until the pool restarts, and the `puntohunter_journal_failed` metric is set. Solo
mining carries on. No share that pays from a window is ever acknowledged without being in the journal.

`sequence` is an `AUTOINCREMENT` key, so it rises across restarts and is never reused, though a refused
insert may skip one. When the journaled share is a block, its sequence becomes the block's window cutoff,
written to the block's row in the same transaction. Whichever is recorded second, the block's row or its
solution share, triggers the block's allocation in a commit of its own.

## The Schema

The schema is plain SQL, numbered migrations applied in order against SQLite's `user_version`. Today the list
holds one, `0001_schema.sql`, the whole schema, which a schema change edits in place; the change's CHANGELOG
entry gives the SQL that brings an existing database to the new shape. Each migration runs in its own
transaction with the version update, and logs `database migrated`. A database whose version is above the
binary's count was written by a newer binary, and `Store::open` refuses it with `StoreError::Newer` rather
than run on a schema it does not know.

| Table or view | What it holds | Written by |
| --- | --- | --- |
| `blocks` | One row per found block: height, pool, status, the node's answer, the finder, the reward split | The submitter, the lifecycle task, and the journal (the window cutoff) |
| `rollups` | Each worker's shares per period, by transport | Telemetry ([Telemetry](telemetry.md)) |
| `totals` | Each worker's accepted and refused shares over all periods | Telemetry |
| `best_shares` | Each worker's best share per UTC day | Telemetry |
| `fleets` | Each worker handle, its fleet, worker name, device type, and payout address | Telemetry |
| `policies` | Each PPLNS window policy a pool has run | Startup |
| `journal` | Every accepted share that pays from a window, in order | The journal |
| `snapshots` | Each block's PPLNS window, fixed when its solution share is journaled | Accounting |
| `allocations` | What each block allocates to each address | Accounting |
| `ledger` | Append-only entries that move satoshis between balances | Accounting and payouts |
| `balances` (view) | Each address's pending, available, reserved, and paid sums by pool | Read only |
| `unreleased` (view) | Blocks with allocations still pending | Read only |
| `pauses` | Why payouts were paused, and when the operator lifted each pause | Accounting, `puntohunter payouts resume` |
| `batches`, `batch_inputs` | Payout transactions and the outputs they spend | Payouts |
| `tokens` | The SHA-256 of each balance token, with its address | `puntohunter token issue` |

The `snapshots` table holds PPLNS windows; it has nothing to do with the database snapshots below.

The schema leans on SQLite to refuse bad rows rather than trusting every caller:

* Every table is `STRICT`, so a column holds only its declared type.
* Every `pool` column checks the pool id's form: 1 to 32 lower-case letters, digits, and `-`. A row keeps its
  pool's id, so a pool no longer configured keeps its rows. No table stores a pool's mode, only its id.
* Worker handles are checked as 16 base32 characters, and payout addresses as 14 to 90 letters and digits.
  The telemetry tables' text columns are constrained so that none of them, except `fleets.address`, can hold
  an address.
* A journal row's `pool_sats` lies between 0 and its `miner_sats`, and its `proof` is unique, so a share is
  credited once whichever session sends it.
* The `ledger` cannot be updated or deleted: two triggers abort any attempt. A balance is always a sum of
  entries (the `balances` view), never a stored figure.
* Work too large for a 64-bit integer at mainnet difficulty is stored as decimal text, with a check that it
  holds only digits.
* `REFERENCES` clauses are enforced: the store turns foreign keys on for its connection.

The store's tests check these constraints directly, for example that every `pool` column refuses a malformed
id and that the journal refuses a pool output larger than its miner portion.

Nothing in the binary deletes rows from the journal or the telemetry tables; they grow with the pool.

## Database Snapshots

With `[backup]`, [backup.rs](../../../src/store/backup.rs) takes a snapshot when the pool starts, every
`interval_secs`, and at once when `backup::now` is called: after a block with a pool output is allocated, or
a payout batch is broadcast. The records that move money then wait for no interval.

A snapshot is `VACUUM INTO` on a read-only connection of its own. It copies the database as one read
transaction sees it, so neither the writer nor the portal waits, and the copy is a complete database that
`Store::open` can open as it is. The steps are:

1. Remove any temporary file a failed snapshot left, since `VACUUM INTO` refuses to overwrite one.
2. Write `.puntohunter-<Unix milliseconds>.sqlite3.tmp`, a hidden name.
3. Rename it to `puntohunter-<Unix milliseconds>.sqlite3`, so a copy pulled from another host never takes
   half of one, and the names sort by time.
4. Remove the oldest snapshots beyond `keep`, leaving any other file in the directory alone.

A failed snapshot is logged and counted in `puntohunter_backups_failed_total`; mining and the journal carry
on. Each success sets `puntohunter_backup_last_seconds`. Copying snapshots off the host and restoring one are
in [Operations](../operations.md#snapshots).
