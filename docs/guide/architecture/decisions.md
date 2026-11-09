# Decisions

The main design choices behind the code, and the alternatives set aside. Each is a few lines: what was
chosen, what was not, and why. The other pages describe how each choice works.

## The Shape of the Engine

### One process, one binary

**Chosen:** every pool, both Stratum versions, accounting, the portal, and the metrics in one process, with
SQLite in the data directory and the web app compiled in. **Set aside:** separate services and a database
server. **Why:** an operator runs one artifact beside Bitcoin Core and backs up one directory; a share's
path from socket to journal crosses no network ([Architecture](README.md)).

### Solo, pooled, and hybrid are one rule

**Chosen:** every pool pays its finder a configured part of the reward and the pool wallet the rest, shared
by PPLNS; solo is a part of 100%, pooled 0%. **Set aside:** a separate engine per mode. **Why:** one code
path for jobs, shares, and accounting, and hybrid pools or a worker's own part come for free
([Mining](mining.md#the-reward-split)).

### One core under two protocols

**Chosen:** V1 and V2 are thin adapters over one context, one username parser, one share check, and one job
cache. **Why:** both protocols get identical share, accounting, and payout rules, and a fix lands in both
([Stratum](stratum.md#the-shared-core)).

### The pool builds the block

**Chosen:** the pool assembles the whole block from the job and submits it as hex. **Set aside:** asking the
node to put it together. **Why:** a found block depends on nothing outside the pool, and a test rebuilds a
real mainnet block byte for byte ([Mining](mining.md#blocks)).

## Nodes

### Every node watched all the time

**Chosen:** each node has its own watcher that keeps its health and latest template current, selected or
not. **Why:** failing over publishes a template that is already there, with no RPC call at that moment
([Nodes](nodes.md#one-watcher-per-node)).

### The most work wins, not the tallest tip

**Chosen:** templates rank by the chain work they build on, then by how many nodes share the tip, then by
the primary. **Set aside:** height alone. **Why:** a taller tip with less work is the wrong chain. Soft
faults (stuck, isolated, recovering) bench a node only while another is fit, so a fleet whose nodes are all
degraded keeps mining ([Nodes](nodes.md#choosing-the-template-source)).

### A found block goes to every node at once

**Chosen:** `submitblock` to every node in parallel, recorded at the first success, retried every second
until some node answers, and resubmitted from disk after a restart. **Why:** a slow or hung node delays
neither the others nor the record ([Nodes](nodes.md#submitting-a-block)).

## Shares and Storage

### Journal before acknowledgement

**Chosen:** a pooled or hybrid share is acknowledged only once its journal commit is written; after one
failed commit, every later pooled share is refused until restart, while solo mining carries on.
**Set aside:** acknowledging first, or retrying in place. **Why:** a miner never hears "accepted" for a
share the books do not hold. A share that solves a block goes to the submitter first, so a slow write never
delays a block ([Accounting](accounting.md#the-journal)).

### Two kinds of commit

**Chosen:** journal commits return once written to the log, which survives a crash of the process, and are
synced within half a second; every other commit syncs before it returns. **Why:** share throughput without a
sync per share, while no commit that moves money can be lost to a power cut ([Storage](storage.md)).

### One writer

**Chosen:** one thread owns the only writable connection and takes each transaction `IMMEDIATE`; readers
use their own read-only connection. **Why:** no lock contention inside the process, a read-then-write never
meets another process's commit in between, and pages never queue behind a commit
([Storage](storage.md#one-writer-many-readers)).

## Accounting

### Balances are sums, never stored figures

**Chosen:** an append-only ledger, its updates and deletes refused by triggers, with each balance the sum of
its entries. **Why:** history is never rewritten, and settling a block reads its stage from the entries, so
running it twice writes nothing and a block is credited, released, or reversed once
([Accounting](accounting.md#the-ledger)).

### Work in exact integers

**Chosen:** work in units of 2^-16 hashes in 128-bit integers, exact multiply-divide, and largest-remainder
rounding, ties to the lower address. **Set aside:** whole hashes, and floating point. **Why:** allocations
sum to the pool output exactly, and tiny test shares keep their proportions
([Accounting](accounting.md#work-exactly)).

### Shares weighted by what they pay the pool

**Chosen:** each share counts its work scaled by its job's pool output over its miner portion. **Set
aside:** plain work. **Why:** under plain work, a worker that keeps its whole finder part would also be paid
from the window, twice over; weighting makes every finder part earn the same in expectation. Each share
keeps its job's amounts, so later config changes do not touch it ([Accounting](accounting.md#weighted-work)).

### One signed transaction per payout batch

**Chosen:** a batch's signed transaction is stored before its first broadcast and only ever resent as the
same bytes; its inputs are locked in the wallet. **Why:** a crash or an RPC timeout can never fund a second
payment ([Accounting](accounting.md#one-payment-whatever-fails)).

### A reorg after payment pauses payouts

**Chosen:** when a block leaves the chain after its balances were reserved or paid, its credits stand and
payouts pause until the operator lifts the pause with `puntohunter payouts resume`. **Set aside:** reversing
the credits. **Why:** the money has left; taking it back would debit someone else
([Accounting](accounting.md#a-reorg-after-payment)).

### The engine holds no spending key

**Chosen:** Bitcoin Core funds and signs payouts from a wallet that holds the pool wallet's key alone, which
can be encrypted and kept locked; the engine never sees its passphrase. **Why:** a compromised pool process
cannot spend, and the wallet cannot spend a finder's or a fee output ([Security](security.md#secrets)).

### Reconcile at every start

**Chosen:** before its first batch, the payout worker checks the books against themselves and against the
wallet and the chain, and halts payouts on any mismatch. **Why:** a restored backup that lags the wallet is
caught without any special detection of a restore ([Accounting](accounting.md#reconciliation)).

## Privacy and the Portal

### Handles under a secret key

**Chosen:** worker and fleet handles are an HMAC under a per-instance key. **Set aside:** a plain hash of the
address. **Why:** addresses are public, so a plain hash could be matched by hashing known addresses
([Telemetry](telemetry.md#handles)).

### The binary renders no page

**Chosen:** the binary serves the static web app, embedded and assembled at startup, and each page fills
itself from the JSON API in the browser. **Set aside:** server-rendered pages. **Why:** one app serves the
pool's own portal and any static host, and the API is the only contract between them. The cost is that the
binary's assembly must match `web/build.py` byte for byte, which a test checks
([Portal](portal.md#the-binary-renders-no-page)).

### One model per resource and its stream, shared per fleet

**Chosen:** each resource and its live stream are built from one model, and a fleet's or worker's snapshot
is computed at most once a second and shared by every viewer. **Why:** a page never sees its resource and its
stream disagree, and store reads grow with the fleets watched, not with viewers or shares
([Portal](portal.md#live-streams)).

### Limit only what the binary alone can see

**Chosen:** the binary limits lookups and open streams per client address and in all, and leaves request
volume to a proxy or edge in front. **Why:** per-address limits on ordinary reads would refuse real users
who share one carrier or office address ([Portal](portal.md#limits-and-the-client-address)).

### No per-worker metrics

**Chosen:** no metric carries a worker label; per-worker figures are the portal's. **Why:** a metric label is
where an address would leak ([Architecture](README.md#metrics)).

## Difficulty

### Vardiff that reads its own jobs

**Chosen:** retarget after about ten shares' worth of work, step by the square root of the ratio, at most
four times, skip changes under 15%, and count each share at its own job's difficulty. **Set aside:** a
three-share window. **Why:** a short window cut a real miner's difficulty fourfold whenever one share or none
arrived ([Mining](mining.md#vardiff)).

### V2 credits at the easier target

**Chosen:** a V2 share is checked and credited at the easier of its job's target and its channel's current
one. **Why:** a miner that filters shares by the target it held when the job arrived keeps its in-flight
shares after a raise, and one that applies a lower target at once is honoured on its next share
([Stratum](stratum.md#channels)).
