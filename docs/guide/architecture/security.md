# Security

This page gathers what the code does to keep the pool's secrets, its miners' privacy, and their money safe:
the keys it holds, what it trusts and what it does not, what it never shows or logs, and the bounds it puts
on hostile input. Each topic links the page that covers its code. It describes the code's model, not any one
deployment; [Operations](../operations.md) covers running a pool safely.

## Secrets

| Secret | Where | Protects |
| --- | --- | --- |
| `handle.key` | Data directory | Every worker and fleet handle: without it, a handle can be neither computed from an address nor reversed |
| `sv2-authority.key` | Data directory | The Stratum V2 authority: V2 miners pin its public key |
| `sv2-authority.next.key` | Data directory, during a rotation | Nothing yet: it is only announced, never used to sign |
| The node's RPC cookie or password | Bitcoin Core's cookie file, the config, or a password file | Access to the node and its wallets |
| The payout wallet's passphrase | Never on the host | The pool outputs the payout wallet can spend |
| Balance tokens | Shown once; only their SHA-256 is stored | One address's balance view |

[secret.rs](../../../src/secret.rs) keeps the two key files: each is made on first run with mode 0600,
created only if absent so a key is never overwritten, and refused at startup when group or others can read it
or it does not parse. [config.rs](../../../src/config.rs) refuses an RPC password file that anyone but its
owner can read, and the cookie and the password file are read again on every call, so neither is cached
beyond its use.

The engine holds no private key that spends money. The payout wallet lives in Bitcoin Core, holds the pool
wallet's key alone, and can be kept encrypted and locked: a payout batch then waits, funded and reserved,
until the operator unlocks the wallet from outside the engine, which never asks for or stores its passphrase
([Accounting](accounting.md#the-payout-wallet)).

## What the Pool Trusts

| Party | Trusted for | How the code limits it |
| --- | --- | --- |
| Bitcoin Core nodes | Templates, the coinbase value, chain work, block status, fee estimates, funding and signing payouts | A node on another chain is quarantined; one in initial block download, or whose template builds on an unknown block, sources nothing ([Nodes](nodes.md)) |
| The operator | The config, the data directory, the payout wallet | Every config value is checked at startup, and a rejected value names its key |
| `[portal] trusted_proxy` | The client address it adds to `X-Forwarded-For` | The header is ignored from any other peer ([Portal](portal.md#limits-and-the-client-address)) |
| Group members | Their own public pool figures, for the group's totals | 5-second timeout, no redirects, at most 64 KiB, the same network, and only their own figures ([Portal](portal.md#the-group)) |
| Miners | Nothing | Every share is rebuilt and checked; input is bounded ([Stratum](stratum.md#limits-on-hostile-input)) |
| Web clients | Nothing | Limits, CORS by exact origin, and no echo of what they send ([Portal](portal.md)) |

The node is the trust anchor: the pool mines what its selected node offers and believes what it says of the
chain. A fleet of several nodes guards against one that stops, hangs, or falls behind, not against one that
lies, so every node should be one the operator runs and controls.

## The Network

| Listener | Protocol | Notes |
| --- | --- | --- |
| Stratum V1 | Plain TCP, line-delimited JSON | No authentication or encryption: a username names where the reward goes, and the password carries only the fleet secret and difficulty hints |
| Stratum V2 | Noise NX, then encrypted frames | Each connection gets a fresh static key and a certificate signed by the authority key, which miners pin ([Stratum](stratum.md#the-authority-key)) |
| Portal and API | Plain HTTP | TLS, where wanted, is a reverse proxy's in front |
| Metrics | Plain HTTP | Loopback by default, `[metrics] bind` |
| To the nodes | JSON-RPC over HTTP, ZMQ over TCP | Neither is encrypted: keep nodes on the same host or a private network |

Because V1 carries the fleet secret in the clear, a private fleet's secret is only as private as the path
between the miner and the pool; V2 encrypts it.

## Privacy

The portal and the logs name miners by opaque handles, and the code keeps these rules
([Telemetry](telemetry.md#handles), [Portal](portal.md#privacy-rules)):

* A handle is an HMAC of the address, or of the address and worker, under `handle.key`, so an address cannot
  be turned into a handle by anyone without the key, nor a handle back into an address.
* A private fleet keys on the address and the fleet secret together; the secret is used for the handles alone
  and kept nowhere.
* No URL carries an address, worker name, password, or token. Only the fleet resource carries the payout
  address, so its owner can check it; no stream, list, or worker resource does. The stats lists show a
  shortened address, and none for a private fleet.
* Nothing logs a portal request, and errors never echo a username, password, token, or address. A V1
  session never logs the raw `mining.authorize` line, nor a V2 session a channel's raw user identity; a
  refused username is logged with its secret blanked.
* No metric has a per-worker label.
* With `[geo]`, a connection's country is looked up once; the peer address is kept nowhere.

What the pool does store about miners is in the database: payout addresses in the journal, allocations,
ledger, and fleets tables, and the hashes of balance tokens. The database's snapshots hold the same, so they
need the same care as the database ([Storage](storage.md)).

## Hostile Input

* **Stratum.** Lines and frames are capped at 16 KiB; a client has `authorize_timeout_secs` to start mining,
  16 refused authorizations or channel opens, and 16 submits for nothing before the connection closes. A V2
  connection holds at most 64 channels and a decode queue of 64 frames ([Stratum](stratum.md)).
* **The portal.** Lookups are limited per client address; open streams per address and in all; block lists
  to 200 rows. Request volume is left to a proxy or edge in front ([Portal](portal.md#limits-and-the-client-address)).
* **The pages.** Every page carries a Content-Security-Policy that allows no inline script or style,
  `frame-ancestors 'none'`, `Referrer-Policy: no-referrer`, and `nosniff`, and the app sets text, never
  markup, from what the API returns ([Portal](portal.md#the-binary-renders-no-page)).
* **The database.** Every table is `STRICT` and checks its columns' forms, so a malformed pool id, handle,
  or address cannot be stored ([Storage](storage.md#the-schema)).

Without `[public] host`, the overview's Stratum URLs name the host the request named. A page shows that host
only to the client that sent it, but behind a shared cache set `[public] host`, so every visitor sees the
same URLs.

## Money

The accounting keeps its own invariants, each enforced in code ([Accounting](accounting.md)):

* No pooled or hybrid share is acknowledged before it is in the journal, and a share is credited once.
* The ledger is append-only, and a balance is always a sum of its entries.
* A payout batch is signed once, stored before it is sent, and only ever resent as the same bytes.
* Miners are paid in full; fees come from the operator's reserve, which cannot be credited beyond what the
  wallet holds over what miners are owed.
* At startup the books are reconciled against the wallet and the chain, and any mismatch halts payouts.

## Reporting a Vulnerability

Report a vulnerability privately, as [SECURITY.md](../../../SECURITY.md) describes, never in an issue.
