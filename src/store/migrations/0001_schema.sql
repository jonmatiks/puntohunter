-- The whole schema, each table in its final shape. It replaced thirteen earlier migrations before any
-- production deployment. A schema change edits this file in place, and its CHANGELOG entry gives the SQL that
-- brings an existing database to the new shape, since the pool applies no migration to a database already at
-- this version. The database is never deleted: it holds miners' balances.
--
-- Every row of mining or accounting carries its pool: the id of a [[pools]] entry, 1 to 32 lower-case
-- letters, digits, and '-'. A pool's id is what is stored, so a pool no longer configured keeps its rows.

-- The blocks the pool found: one row per hash, whichever session or resent share found it.
CREATE TABLE blocks (
    -- The block hash in display order.
    hash TEXT PRIMARY KEY NOT NULL,
    height INTEGER NOT NULL,
    pool TEXT NOT NULL CHECK (length(pool) BETWEEN 1 AND 32 AND pool NOT GLOB '*[^a-z0-9-]*'),
    -- The lifecycle's states, plus rejected for a block the node refused.
    status TEXT NOT NULL
        CHECK (status IN ('submitted', 'rejected', 'confirmed_immature', 'mature', 'orphaned')),
    -- The node's answer to submitblock, 'accepted' for null; NULL until the node answers.
    result TEXT,
    -- Unix seconds.
    found_at INTEGER NOT NULL,
    -- The finder's worker name, transport, and handle.
    worker TEXT,
    transport TEXT CHECK (transport IN ('V1', 'V2-native')),
    handle TEXT,
    -- What the block's reward outputs paid, in satoshis: the finder's address and the pool wallet.
    finder_sats INTEGER CHECK (finder_sats >= 0),
    pool_sats INTEGER CHECK (pool_sats >= 0),
    -- The window cutoff of a block with a pool output: the journal sequence of its solution share. NULL on a
    -- block without one.
    cutoff INTEGER
) STRICT;

-- Worker telemetry's historical series, keyed by the worker's opaque handle from the first row.
-- Every text column is constrained, so none can hold a payout address.

-- One row per worker, transport, and period in which the worker had a share accepted or refused. A period is
-- telemetry.rollup_secs long: a minute live.
CREATE TABLE rollups (
    -- Unix seconds at the period's start.
    start INTEGER NOT NULL,
    handle TEXT NOT NULL CHECK (length(handle) = 16 AND handle NOT GLOB '*[^a-z2-7]*'),
    pool TEXT NOT NULL CHECK (length(pool) BETWEEN 1 AND 32 AND pool NOT GLOB '*[^a-z0-9-]*'),
    transport TEXT NOT NULL CHECK (transport IN ('V1', 'V2-native')),
    accepted INTEGER NOT NULL,
    -- The work of the targets the accepted shares were credited at, in expected hashes.
    work REAL NOT NULL,
    -- Refused shares by class. An unauthorized share names no worker, so it has no row.
    stale INTEGER NOT NULL,
    duplicate INTEGER NOT NULL,
    low_difficulty INTEGER NOT NULL,
    bad_version INTEGER NOT NULL,
    invalid INTEGER NOT NULL,
    -- The highest difficulty an accepted share's hash reached; 0 when none was accepted.
    best REAL NOT NULL,
    PRIMARY KEY (handle, transport, start)
) STRICT;

-- Each worker's accepted and refused shares over all its rollups, kept with them so a page need not sum every
-- period a worker ever had.
CREATE TABLE totals (
    handle TEXT PRIMARY KEY NOT NULL CHECK (length(handle) = 16 AND handle NOT GLOB '*[^a-z2-7]*'),
    accepted INTEGER NOT NULL,
    -- Every refusal class together.
    rejected INTEGER NOT NULL,
    -- Unix seconds: the boundary the rows of the newest write that added to them were taken out before. Rows
    -- of a later write are still telemetry's.
    through INTEGER NOT NULL
) STRICT;

-- Each worker's best share by UTC day.
CREATE TABLE best_shares (
    handle TEXT NOT NULL CHECK (length(handle) = 16 AND handle NOT GLOB '*[^a-z2-7]*'),
    pool TEXT NOT NULL CHECK (length(pool) BETWEEN 1 AND 32 AND pool NOT GLOB '*[^a-z0-9-]*'),
    -- Days since the Unix epoch.
    day INTEGER NOT NULL,
    best REAL NOT NULL,
    PRIMARY KEY (handle, day)
) STRICT;

-- Fleets: every worker mining to one address, keyed by the address's fleet handle. Like the
-- telemetry tables, no column but the address can hold one: handles are base32, and the worker name is held
-- to the username parser's characters and length.
CREATE TABLE fleets (
    fleet TEXT NOT NULL CHECK (length(fleet) = 16 AND fleet NOT GLOB '*[^a-z2-7]*'),
    -- One row per worker handle, which is per pool.
    handle TEXT PRIMARY KEY NOT NULL CHECK (length(handle) = 16 AND handle NOT GLOB '*[^a-z2-7]*'),
    pool TEXT NOT NULL CHECK (length(pool) BETWEEN 1 AND 32 AND pool NOT GLOB '*[^a-z0-9-]*'),
    -- Empty for a worker mining with the bare address.
    worker TEXT NOT NULL CHECK (length(worker) <= 64 AND worker NOT GLOB '*[^A-Za-z0-9._-]*'),
    -- Unix seconds.
    first_seen INTEGER NOT NULL,
    -- The fleet's payout address, which the fleet page and its workers' pages show.
    -- The check admits the characters of a base58 or bech32 address and nothing else.
    address TEXT
        CHECK (address IS NULL OR (length(address) BETWEEN 14 AND 90 AND address NOT GLOB '*[^0-9A-Za-z]*')),
    -- Each device's miner type: the vendor its V2 SetupConnection named, or the first word of its V1
    -- mining.subscribe user agent, without a version, which would let a rare firmware be followed across
    -- pools. The check also keeps an address out: no address is 24 characters or fewer. A type takes '+', as
    -- NerdQAxe++, and at least one letter or digit. NULL when unknown.
    vendor TEXT
        CHECK (vendor IS NULL OR (length(vendor) BETWEEN 1 AND 24 AND vendor NOT GLOB '*[^A-Za-z0-9._+-]*'
                                  AND vendor GLOB '*[A-Za-z0-9]*')),
    -- Whether the fleet is private, keyed on a fleet secret as well as the address, so the stats
    -- lists show it without its address. The secret itself is kept nowhere.
    private INTEGER NOT NULL DEFAULT 0 CHECK (private IN (0, 1))
) STRICT;

CREATE INDEX fleets_by_fleet ON fleets (fleet);

-- The accepted-share journal: every share of a pool that can pay a pool output, in the order the
-- pool accepted it, before the miner hears it was accepted. Unlike the telemetry tables it holds payout
-- addresses, which no public route reads.

-- Each PPLNS window policy the pool has run: the window multiplier and minimum work,
-- and the accounting policy version. A share is credited under the policy its pool had in the process that
-- issued its job, and pools with the same policy share a row.
CREATE TABLE policies (
    id INTEGER PRIMARY KEY,
    window_multiplier REAL NOT NULL CHECK (window_multiplier > 0),
    window_min_work REAL NOT NULL CHECK (window_min_work >= 0),
    version INTEGER NOT NULL CHECK (version > 0),
    UNIQUE (window_multiplier, window_min_work, version)
) STRICT;

CREATE TABLE journal (
    -- Monotonic across restarts: AUTOINCREMENT never reuses a sequence, even one deleted.
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    pool TEXT NOT NULL CHECK (length(pool) BETWEEN 1 AND 32 AND pool NOT GLOB '*[^a-z0-9-]*'),
    -- The payout address, which the check holds to the characters of a base58 or bech32 address.
    address TEXT NOT NULL
        CHECK (length(address) BETWEEN 14 AND 90 AND address NOT GLOB '*[^0-9A-Za-z]*'),
    handle TEXT NOT NULL CHECK (length(handle) = 16 AND handle NOT GLOB '*[^a-z2-7]*'),
    policy INTEGER NOT NULL REFERENCES policies (id),
    -- The job: the template sequence of the process that issued it, and its id on the session or channel.
    template INTEGER NOT NULL,
    job INTEGER NOT NULL,
    -- The job's network difficulty, as the header's compact bits.
    bits INTEGER NOT NULL,
    -- The job's split, in satoshis: its miner portion, and the pool output paid from it. A share keeps its
    -- job's amounts, so neither a later config change nor a later choice changes its credit.
    miner_sats INTEGER NOT NULL CHECK (miner_sats >= 0),
    pool_sats INTEGER NOT NULL CHECK (pool_sats >= 0 AND pool_sats <= miner_sats),
    -- The target the share was credited at, 32 bytes big-endian. Its work is computed in Rust.
    target BLOB NOT NULL CHECK (length(target) = 32),
    -- The header hash, 32 bytes in internal byte order: a share is credited once, whichever session sends it.
    proof BLOB NOT NULL UNIQUE CHECK (length(proof) = 32),
    -- Unix seconds.
    accepted_at INTEGER NOT NULL
) STRICT;

-- PPLNS allocation: the window of each block with a pool output, fixed when its solution share is
-- journaled, and what it allocates to each address. Both are written once, in one commit, and never changed:
-- later shares cannot alter a block's allocation. Work is in units of 2^-16 expected hashes (see
-- src/pool/pplns.rs), which outgrow SQLite's 64-bit integers at mainnet difficulty, so it is stored as decimal
-- text.

CREATE TABLE snapshots (
    block TEXT PRIMARY KEY NOT NULL REFERENCES blocks (hash),
    pool TEXT NOT NULL CHECK (length(pool) BETWEEN 1 AND 32 AND pool NOT GLOB '*[^a-z0-9-]*'),
    -- The solution share's journal sequence, where the window ends, inclusive.
    cutoff INTEGER NOT NULL,
    -- The oldest share in the window, and the work credited to it: all of its work, or the fraction the
    -- window still needed.
    first INTEGER NOT NULL CHECK (first <= cutoff),
    boundary TEXT NOT NULL CHECK (boundary NOT GLOB '*[^0-9]*' AND boundary != ''),
    -- W, and the work credited in all: W, or less when the journal held less. Work, here and in boundary, is
    -- in 2^-16 hashes; under the solution share's policy version 2, each share's weighted by its job's pool
    -- output over its miner portion.
    size TEXT NOT NULL CHECK (size NOT GLOB '*[^0-9]*' AND size != ''),
    total TEXT NOT NULL CHECK (total NOT GLOB '*[^0-9]*' AND total != ''),
    -- The satoshis allocated: the block's pool output.
    amount INTEGER NOT NULL CHECK (amount >= 0)
) STRICT;

CREATE TABLE allocations (
    block TEXT NOT NULL REFERENCES snapshots (block),
    pool TEXT NOT NULL CHECK (length(pool) BETWEEN 1 AND 32 AND pool NOT GLOB '*[^a-z0-9-]*'),
    address TEXT NOT NULL
        CHECK (length(address) BETWEEN 14 AND 90 AND address NOT GLOB '*[^0-9A-Za-z]*'),
    sats INTEGER NOT NULL CHECK (sats > 0),
    PRIMARY KEY (block, address)
) STRICT;

-- The ledger: append-only entries that turn allocations into balances, each moving
-- satoshis of one address and pool, or of the operator's fee reserve, which has neither. A balance is a sum of
-- entries, never a stored figure:
--
--   pending    credits an allocation whose coinbase is not yet spendable;
--   available  moves it from pending to available, once the coinbase is mature plus the buffer;
--   reserved   moves available satoshis into a payout batch, and paid moves them out of it, paid;
--   reversal   takes back a pending or available credit whose block left the chain;
--   credit     credits the fee reserve, from which batches pay their transaction fees (`puntohunter reserve
--              credit`).

CREATE TABLE ledger (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL
        CHECK (kind IN ('pending', 'available', 'reserved', 'paid', 'reversal', 'credit')),
    -- A reversal's: the balance it takes back from.
    reverses TEXT CHECK (reverses IN ('pending', 'available')),
    -- NULL on the fee reserve's entries: its credits, and the fees batches reserve and pay from it.
    address TEXT CHECK (length(address) BETWEEN 14 AND 90 AND address NOT GLOB '*[^0-9A-Za-z]*'),
    pool TEXT CHECK (length(pool) BETWEEN 1 AND 32 AND pool NOT GLOB '*[^a-z0-9-]*'),
    sats INTEGER NOT NULL CHECK (sats > 0),
    -- What the entry belongs to: a block's allocation, or, for reserved and paid, a payout batch.
    block TEXT,
    batch INTEGER,
    CHECK ((kind = 'reversal') = (reverses IS NOT NULL)),
    CHECK ((kind IN ('pending', 'available', 'reversal')) = (block IS NOT NULL)),
    CHECK ((kind IN ('reserved', 'paid')) = (batch IS NOT NULL)),
    CHECK ((address IS NULL) = (pool IS NULL)),
    CHECK (CASE WHEN address IS NULL THEN kind IN ('credit', 'reserved', 'paid') ELSE kind != 'credit' END),
    FOREIGN KEY (block, address) REFERENCES allocations (block, address)
) STRICT;

CREATE INDEX ledger_block ON ledger (block);
CREATE INDEX ledger_address ON ledger (address);

CREATE TRIGGER ledger_no_update BEFORE UPDATE ON ledger
BEGIN
    SELECT RAISE(ABORT, 'the ledger is append-only');
END;

CREATE TRIGGER ledger_no_delete BEFORE DELETE ON ledger
BEGIN
    SELECT RAISE(ABORT, 'the ledger is append-only');
END;

-- Each address's balances by pool, the fee reserve's aside.
CREATE VIEW balances AS
SELECT
    address,
    pool,
    SUM(CASE WHEN kind = 'pending' THEN sats
             WHEN kind = 'available' OR reverses = 'pending' THEN -sats ELSE 0 END) AS pending,
    SUM(CASE WHEN kind = 'available' THEN sats
             WHEN kind = 'reserved' OR reverses = 'available' THEN -sats ELSE 0 END) AS available,
    SUM(CASE WHEN kind = 'reserved' THEN sats WHEN kind = 'paid' THEN -sats ELSE 0 END) AS reserved,
    SUM(CASE WHEN kind = 'paid' THEN sats ELSE 0 END) AS paid
FROM ledger
WHERE address IS NOT NULL
GROUP BY address, pool;

-- The blocks with allocations still pending, which the lifecycle task follows however deep they are.
CREATE VIEW unreleased AS
SELECT block
FROM ledger
WHERE block IS NOT NULL
GROUP BY block
HAVING SUM(CASE WHEN kind = 'pending' THEN sats
                WHEN kind = 'available' OR reverses = 'pending' THEN -sats ELSE 0 END) > 0;

-- Payouts stay paused while any row is not lifted, each saying why: a block whose allocations were reserved or
-- paid left the chain, and no reversal can take them back without debiting someone else. The
-- operator lifts the pauses with `puntohunter payouts resume` once they have settled the cause. A lifted row
-- stays, so its block stays exempt from reconciliation's check that a block off the chain is credited
-- nothing: its credits stand by design.
CREATE TABLE pauses (
    id INTEGER PRIMARY KEY,
    reason TEXT NOT NULL,
    block TEXT UNIQUE REFERENCES blocks (hash),
    -- When the operator lifted it, in Unix seconds; NULL while it pauses payouts.
    lifted_at INTEGER
) STRICT;

-- Payout batches. One ledger transaction reserves a batch's balances, as reserved entries naming
-- the batch (its recipients), with its fee from the fee reserve, and records the batch and its inputs here. A
-- batch then moves forward only:
--
--   reserved   funded: its inputs are chosen and `funded` is the unsigned transaction;
--   signed     `tx` is the signed transaction, persisted before its first broadcast, and the only bytes
--              ever sent;
--   broadcast  the node took `tx`;
--   confirmed  `tx` is buried deep enough, and the batch's paid entries are written in the same commit.

CREATE TABLE batches (
    id INTEGER PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('reserved', 'signed', 'broadcast', 'confirmed')),
    funded TEXT NOT NULL,
    -- In satoshis, and the rate it was funded at, in sat/vB.
    fee INTEGER NOT NULL CHECK (fee > 0),
    fee_rate REAL NOT NULL CHECK (fee_rate > 0),
    tx TEXT,
    txid TEXT UNIQUE,
    -- When the batch was reserved, in Unix seconds: the overview's last payout and the stuck-batch metric
    -- read it.
    reserved_at INTEGER,
    CHECK ((state = 'reserved') = (tx IS NULL)),
    CHECK ((tx IS NULL) = (txid IS NULL))
) STRICT;

-- Each batch's inputs. An outpoint belongs to one batch at most, so no input is ever spent twice.
CREATE TABLE batch_inputs (
    txid TEXT NOT NULL,
    vout INTEGER NOT NULL CHECK (vout >= 0),
    batch INTEGER NOT NULL REFERENCES batches (id),
    PRIMARY KEY (txid, vout)
) STRICT;

CREATE INDEX batch_inputs_batch ON batch_inputs (batch);
CREATE INDEX ledger_batch ON ledger (batch);

-- Balance tokens: each opens one address's private balance view.
-- `puntohunter token issue` prints a token once and keeps only its SHA-256 here, so the database holds
-- nothing that opens a balance. An address may hold several.

CREATE TABLE tokens (
    hash BLOB PRIMARY KEY CHECK (length(hash) = 32),
    address TEXT NOT NULL
        CHECK (length(address) BETWEEN 14 AND 90 AND address NOT GLOB '*[^0-9A-Za-z]*'),
    -- Unix seconds.
    issued INTEGER NOT NULL
) STRICT;
