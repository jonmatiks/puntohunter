# Stratum

This page covers the Stratum front-ends: the shared core both protocols call, the username parser, a V1
session, and V2's encrypted transport, authority key, sessions, and channels. The code is in
[src/stratum/](../../../src/stratum/). V1 and V2 are wire-format adapters over one core: one context, one
username parser, one share check, and the job, target, and vardiff code that [mining](mining.md) describes.
[Mining](../mining.md) in the operator guide says what a miner enters to connect.

```text
listener per pool and protocol
  └─ one task per connection ── V1 session ──────────────┐
                              └ V2 session ── channels ──┤
                                                         ▼
                          Context: worker, check_share, credit ──► journal, telemetry, submitter
```

## Listeners

Each pool has a listener per protocol it serves, bound in [v1/mod.rs](../../../src/stratum/v1/mod.rs) and
[v2/mod.rs](../../../src/stratum/v2/mod.rs). The listener binds the connection to its pool: switching pools
means reconnecting to another port. Each accepted connection gets a task of its own, and a failure in it
ends that connection and no other. Both listeners set `TCP_NODELAY`, since replies and acknowledgements are
small, and back off 100 ms when an accept fails, most likely for want of file descriptors, rather than
spin.

## The Shared Core

[stratum/mod.rs](../../../src/stratum/mod.rs) holds `Context`, what every session on every listener
shares: the network, the vardiff bounds, the extranonce allocator, the work watch, the `JobCache`, the
pool wallet, the channel to the block submitter, the authorize timeout, the handle key, telemetry, fleets,
the journal, and the optional geo lookup.

`Context::worker` turns a parsed username into a `Worker`, the identity a V1 session or a V2 channel mines
for. It derives the worker's handle and its fleet's handle, records it in its fleet, opens its telemetry
connection, and records its choices. The whole address stays out of telemetry: the stats lists hold its
shortened form, and none for a private fleet. A connection's country is looked up from its peer address
when it opens; the address itself is kept nowhere. [Telemetry](telemetry.md) covers what is recorded.

Every share takes the same two steps once it has matched one of its session's jobs and passed the
protocol's own checks:

1. **`Context::check_share`** rebuilds the header, hashes it, and refuses a duplicate or a miss of the
   share target. A share that passes joins the session's set of seen header hashes. If it is a block, the
   block is assembled and sent to the submitter at once, before the share is credited.
2. **`Context::credit`** accepts it. When its job pays a pool output, the share first goes to the journal
   and is credited only once the commit is written, so a share is in the journal before the miner hears
   it was accepted. The journal refuses a proof it already holds as a duplicate; a failed commit refuses
   the share as `Unavailable`. When the job pays no pool output, as on a solo pool, the share earns from
   no window and is credited at once. Either way, telemetry counts it at the work of its target.

Duplicates are keyed by the header hash, which names the work itself, so the same header is refused
whichever job id it arrives under. The seen set clears whenever the session, or V2 channel, drops its
jobs. [Accounting](accounting.md) covers the journal.

## Usernames

[username.rs](../../../src/stratum/username.rs) is the one parser for usernames, which V1's
`mining.authorize` and V2's channel opens both use:

```text
<address>[.<worker>][#<secret>][+<option>]...
```

`parse` cuts the text in an order that keeps it unambiguous: first at the first `+`, since neither an
address, a worker name, nor a secret holds one, while a part such as `12.5` holds a dot; then at the first
`#`; then at the first `.`.

* **The address** must parse and be on the configured network.
* **The worker** is everything after the first dot: at most 64 ASCII letters, digits, `.`, `-`, and `_`.
* **The secret** (`Secret`) makes the fleet private. It takes at least 8 characters. Over V1 the password
  may carry it instead: `Secret::from_password` drops the comma-separated `d=N` hints, and treats what is
  left as no secret when it is empty, `x`, or shorter than 8, as most firmware ships it. A username may
  not carry a secret both ways.
* **The options** (`Options`) are each after a `+`, in any order, each at most once: a finder part, as a
  percent (`50`, `12.5`), `btc`, or `sats`, held to the config's precision and range, and `name`. A part is
  digits with at most one inner dot, so no sign, exponent, or bare dot a float parser would take slips
  through.

The secret is used to derive the fleet's handles and is kept nowhere else. Its `Debug` form prints
`Secret(..)`. A V1 session never logs the raw `mining.authorize` line, and a V2 session never logs a
channel open's raw user identity, at any level. A success logs the address and worker; a refusal logs the
username through `redacted`, which blanks everything after the `#` up to the options.

## Stratum V1

[v1/message.rs](../../../src/stratum/v1/message.rs) reads line-delimited JSON-RPC 1.0, and
[v1/session.rs](../../../src/stratum/v1/session.rs) runs one connection. A session allocates its extranonce
prefix when the connection opens, then waits on four events: a line from the miner, a change of work, the
vardiff tick, and the authorize deadline.

| Method | What the session does |
| --- | --- |
| `mining.subscribe` | Records the miner type from the user agent; answers with the 4-byte prefix as extranonce1 and 8 as the extranonce2 size |
| `mining.authorize` | Requires a subscribe; parses the username with the password's secret; takes a `d=N` hint from the password; creates the worker and, the first time, the vardiff; then issues a clean job |
| `mining.configure` | Grants BIP310 version rolling within BIP320's bits, if that leaves the asked minimum bit count; declines every other extension |
| `mining.suggest_difficulty` | Takes a positive difficulty as the hint; once work has started, re-seeds vardiff and issues a clean job |
| `mining.extranonce.subscribe` | Answers true; the prefix never changes during a connection |
| `mining.submit` | Checks and credits the share, below |

**Issuing a job.** `issue` takes the job for the worker from the `JobCache`, paying the worker's address on
a pool that can pay a finder and the pool wallet on one that cannot. It sends `mining.set_difficulty`
before the `mining.notify` it governs. The first job on a clean template, or any job that `restart` asks
for, sets `clean_jobs` and drops the session's older jobs and seen hashes. The session keeps its last 16
jobs, each with the target assigned alongside it. Each notify announces an ntime later than the last, and
never before the template's `curtime`, so a job issued again on the same template gives the miner fresh
headers. Without work, nothing is sent and the socket stays open.

**A submit** is refused, in order: from a worker name other than the authorized one; for a job id the
session no longer holds (stale); with malformed params; with version bits outside the negotiated mask. Then
the shared check and credit run, against the target recorded with the job. An accepted share feeds
vardiff, and when the target changes a new job goes out at once, since a new target applies from the next
job. Each refusal answers with its own error code:

| Code | Error |
| --- | --- |
| 20 | Other: malformed params, version bits outside the mask, accounting unavailable, unknown method |
| 21 | Stale job |
| 22 | Duplicate share |
| 23 | Low difficulty share |
| 24 | Unauthorized worker |
| 25 | Not subscribed |

## Stratum V2

### The Transport

[v2/transport.rs](../../../src/stratum/v2/transport.rs) runs the Noise NX handshake as responder and then
reads and writes SV2 frames, each encrypted, through the Stratum V2 reference implementation's crates
(`noise_sv2`, `codec_sv2`). After the handshake, a reader task decodes frames into a queue of 64, each
stamped with when its last byte was read. The session can then wait on frames beside its other events
without cancelling a half-read frame, and a share's response time counts its wait in the queue. A frame
that does not decrypt or parse, or takes more than 16 KiB, ends the reader and with it the connection.

### The Authority Key

[v2/authority.rs](../../../src/stratum/v2/authority.rs) holds the pool's secp256k1 authority keypair. It
lives in the data directory as `sv2-authority.key`, a hex secret key, through
[secret.rs](../../../src/secret.rs): generated on first run with mode 0600, created only if absent so a
secret is never overwritten, and refused at startup when others can read it or it does not parse.

* For each connection, `Authority::responder` makes a Noise responder with a fresh static key and a
  certificate signed by the authority, valid for an hour; a miner checks it once, during the handshake.
* Miners pin the authority's x-only public key, which `public_key` encodes in the reference
  implementation's base58check form, a 2-byte version then the key. The pool logs it at startup on the
  line `SV2 authority key`, and the portal shows it.
* For a rotation, `puntohunter authority next` makes `sv2-authority.next.key`. The pool signs nothing with
  it; with `[public] authority_switch` set, it announces the next key and the day it takes over.
  [Operations](../operations.md#rotating-the-authority-key) gives the steps.

A miner that refuses the pool's certificate hangs up after the handshake. The session warns when a
connection closes between the handshake and `SetupConnection`, since that is the likely cause and would
otherwise log as an ordinary disconnect.

### The Session

[v2/session.rs](../../../src/stratum/v2/session.rs) runs one connection, and
[v2/message.rs](../../../src/stratum/v2/message.rs) parses and frames its messages. One deadline runs from
accept to the first open channel, over the handshake too, so a connection that stalls anywhere before
mining cannot hold its task.

* **`SetupConnection`** must come first, and only once. The session accepts the mining protocol at version
  2 with the flags `REQUIRES_STANDARD_JOBS` and `REQUIRES_VERSION_ROLLING`. It refuses
  `REQUIRES_WORK_SELECTION`, since job declaration is not supported, and any unknown flag, naming them in
  the error; a refusal keeps the socket open for another attempt.
* **Channels.** A connection may hold up to 64, each with its own extranonce prefix. Channel ids count up
  from 1 and are not reused within a connection. `CloseChannel` drops a channel and releases its prefix.
* **Bursts.** The session handles every frame already read, up to 64, then sends one `SubmitSharesSuccess`
  per channel for the shares it accepted. A share that arrives alone is acknowledged at once. A rejection
  first flushes the acknowledgement of the shares accepted before it, so answers stay in sequence order.
* **Accounting.** A share is added to the acknowledgement batch only after it is credited, so a success
  covers committed shares alone. A share the journal could not take is refused with the code
  `accounting-unavailable`, which is the pool's own, since SV2's codes name faults of the share.
* **Other messages.** A well-formed message no miner sends to a pool is ignored and logged at debug level.

### Channels

[v2/channel.rs](../../../src/stratum/v2/channel.rs) is one mining channel on the shared core. It knows
nothing of sockets: each call returns the messages to send, in order.

| | Extended | Standard |
| --- | --- | --- |
| Extranonce | The pool's 4-byte prefix; the miner rolls 8 bytes | All 12 bytes from the pool: the prefix, then zeros |
| Job message | `NewExtendedMiningJob`: coinbase prefix and suffix, merkle path | `NewMiningJob`: the channel's own merkle root |
| The miner rolls | Extranonce, ntime, nonce, version | Ntime, nonce, version: the header only |

**Opening.** `Channel::open` parses the user identity as a username, refuses an extended open that asks
for more than 8 extranonce bytes, and derives the vardiff bounds and starting difficulty (below). Each
refusal answers with its SV2 error code. The success message carries the first target and the prefix; the
group channel id is always 0, since group channels are not supported.

**Jobs.** A channel issues each template once, plus a job on every retarget. A job on a new prevhash goes
out as a future job, without `min_ntime`, followed by `SetNewPrevHash` to activate it, and drops the
channel's older jobs and seen hashes; any other job is active at once. Version rolling is always allowed
within BIP320's bits. As in V1, each job announces a later ntime than the last, and the channel keeps its
last 16 jobs with the target assigned alongside each.

**Shares.** A share for a job the channel no longer holds is stale; one for an id it never issued is
invalid. An extended share must carry exactly 8 extranonce bytes, and a share may change no version bit
outside BIP320's. The share is then checked against the easier of its job's target and the channel's
current one, and credited at that target:

* After a raise, a miner that filters shares by the target it held when the job arrived, as ESP-Miner
  does, still has its in-flight shares accepted.
* After a drop, a miner that applies `SetTarget` at once is honoured on its next share.

A retarget sends `SetTarget`, then a job carrying the new target, as V1 sends `set_difficulty` before a
notify. `SubmitSharesSuccess` reports the batch's summed difficulty, truncated, in `new_shares_sum`.

## Vardiff per Protocol

Both protocols run the `Vardiff` controller of [vardiff.rs](../../../src/mining/vardiff.rs) per V1
connection or V2 channel, and ease a quiet miner on a tick of one share interval. They differ in how a
miner steers it:

* **V1.** A hint from `d=N` in the password or from `mining.suggest_difficulty` sets the starting
  difficulty, clamped to `[vardiff]`'s floor and ceiling.
* **V2.** The claimed `nominal_hash_rate` sets it, at one share per interval: rate × interval / 2^32. A
  claim of 0 starts at the floor; a negative or non-finite one is refused. The channel's `max_target`
  raises the floor to its difficulty, and an open whose `max_target` leaves no difficulty below the
  ceiling is refused. No target sent is easier than `max_target`. `UpdateChannel` re-seeds vardiff from
  its new rate and `max_target`.

## Limits on Hostile Input

Each session bounds what a misbehaving client can cost, and closes the connection past a bound:

| Bound | V1 | V2 |
| --- | --- | --- |
| Input size | 16 KiB per line | 16 KiB per encrypted frame |
| Time to start mining | `authorize_timeout_secs` to authorize | The same, from accept to the first open channel |
| Refused attempts | 16 refused authorizations | 16 refused channel opens |
| Shares for nothing | 16 submits from an unauthorized worker | 16 submits to no open channel |
| Protocol errors | Malformed JSON closes at once | A malformed frame or message, or a message before `SetupConnection`, closes at once |

Refused authorizations and opens are bounded because each is logged; without a bound a hostile client
could fill the log.
