# Mining

This page is for miners: what to enter in a miner to mine on a PuntoHunter pool, over Stratum V1 or
V2, and what the pool's portal shows about it. The operator's portal lists the same settings on its
overview, for each pool it runs.

## Connecting

Each pool listens on ports of its own, and the port a miner connects to picks the pool. The default
config runs three:

| Pool | Stratum V1 | Stratum V2 |
| --- | --- | --- |
| Solo | `stratum+tcp://<host>:3333` | `stratum2+tcp://<host>:3334/<authority key>` |
| Pooled | `stratum+tcp://<host>:4333` | `stratum2+tcp://<host>:4334/<authority key>` |
| Hybrid | `stratum+tcp://<host>:5333` | `stratum2+tcp://<host>:5334/<authority key>` |

An operator may run other pools on other ports, or a pool on one protocol only. The overview's setup
card for each pool gives its URLs, with the operator's host and the authority key filled in, and its
payout rule. [Pools and modes](pools-and-modes.md) explains the rules. A pooled or hybrid pool pays
its miners where the operator has configured payouts; its overview's payouts card says whether it has.

A Bitaxe's AxeOS reads each job's coinbase and warns when it pays less than the whole reward to the
device's address: on a pooled pool, that you have no share in the mining reward, and on a hybrid or
personal pool, that your share is only the finder's part. That is expected. The rest pays the pool
wallet, which credits it by work over the [PPLNS window](pools-and-modes.md#the-pplns-window) and pays
it out as [balances and payouts](pools-and-modes.md#balances-and-payouts) describes.

## The Username

A miner's username is its payout address, a dot, and a worker name:

```text
<address>.<worker>
```

such as `bc1q….bitaxe1`. The whole form is `<address>[.<worker>][#<password>][+<option>]...`.

* **The address** is a Bitcoin address on the pool's network: a mainnet address on a mainnet pool, a
  regtest one on regtest. Any standard type parses. A block a solo or hybrid device finds pays its
  finder's part to this address in the coinbase, and pooled balances are credited to it.
* **The worker name** is everything after the first dot, up to the options: at most 64 letters,
  digits, `.`, `-`, and `_`, so `rack.1` is one name. It may be left out, but give every device a
  name of its own, and not the one it came with: the portal finds a fleet only by a full username,
  never by an address alone.
* **A fleet password** may follow, after a `#`: see [Private fleets](#private-fleets).
* **Options** follow, each after a `+`, in any order, each at most once:

| Option | Example | Effect |
| --- | --- | --- |
| A finder part, as a percent | `+50`, `+12.5` | On a pool with a personal range, the part of a block this device keeps; a percent to at most four decimal places |
| A finder part, in BTC | `+0.5btc` | The same, as a fixed amount, to at most eight decimal places |
| A finder part, in satoshis | `+50000000sats` | The same, in whole satoshis |
| `name` | `+name` | On a pool that offers it, writes the worker name into the blocks this device finds |

Options combine, as `<address>.<worker>+50+name`.

* A part outside the pool's range is paid at the nearer bound. On a pool without a personal range, a
  part has no effect.
* On a pool that writes no worker names, `+name` writes nothing, and the worker page says so. A name
  written to a block stays on the chain for good, beside the finder's address where the pool pays a
  finder.
* Options are read when the device authorizes (V1) or opens its channel (V2). To change one, change
  the username and reconnect.
* The pool knows a device by its username without the options, so a device that changes them keeps
  its statistics, its page, and its fleet.

The pool refuses a username it cannot read, with a reason the miner logs: an address that is not
one, or is for another network; a worker name with other characters or over 64; a fleet password
after a `#` of fewer than 8 characters; an empty option, an unknown one (options are lower case, and
`+50%` or `+0.5BTC` is refused), a part out of range, or an option given twice.

## Private Fleets

Without a fleet password, a device is in its address's open fleet: anyone who knows the address can
open its fleet page, by mining under it and looking that username up. A fleet password makes the
fleet private. Set the same one on every device that should share the page:

* **Over V1**, in the password field, as most firmware offers it: `garage-pass` or
  `garage-pass,d=512`. A `d=N` part is a difficulty hint, not the password. What is left counts as no
  password when it is empty, `x`, or shorter than 8 characters, as most devices ship.
* **Over V2**, which has no password field, after a `#` in the username, before any options:
  `<address>.<worker>#garage-pass+name`. It takes at least 8 characters and no `+`, which starts the
  options. It works over V1 too, but a V1 device may not set both forms.

A private fleet's page lists the devices that set its password, and its blocks and counts are theirs
alone; the open fleet's page neither lists nor counts them. A device with the same address and
another password is in a fleet of its own, and the pool never keeps the password itself: it is used
to make the fleet's handles, and kept out of every log, page, and URL. On the stats page a private
fleet shows no address.

A firmware that shows the username in its own interface or logs, as AxeOS does on the device's
network, shows the password there too; one that masks the password field does not.

## Stratum V1

V1 is line-delimited JSON over plain TCP. The pool handles `mining.subscribe`, `mining.authorize`,
`mining.submit`, `mining.configure`, `mining.suggest_difficulty`, and `mining.extranonce.subscribe`.

* **Password:** a fleet password, if it carries one ([Private fleets](#private-fleets)); the pool
  checks nothing else. A `d=N` in it sets the starting difficulty, as `d=512` or `x,d=512`.
  `mining.suggest_difficulty` does the same, and also re-seeds the difficulty once work has started.
  Either is held within the pool's difficulty bounds.
* **Version rolling:** `mining.configure` grants BIP 310 version rolling within BIP 320's bits,
  which ESP-Miner firmware such as AxeOS uses.
* A connection that has not authorized within the pool's `authorize_timeout_secs` is closed.

## Stratum V2

V2 is binary and encrypted: every connection opens with a Noise handshake, in which the pool
presents a certificate signed by its authority key. A miner that pins that key knows it is talking
to this pool, and no one on the path can read or alter what it sends.

* **The authority key** is in the V2 URL, after the last `/`. The overview shows it with a Copy
  button, and the pool logs it at startup on the line `SV2 authority key`. On a Bitaxe, enter it in
  AxeOS's authority-key field, beside the V2 host and port.
* The pool generates the key on its first run and keeps it in its data directory. If the operator
  replaces it, every V2 miner must pin the new one. A planned change is announced on the overview,
  under the key, with the new key and the day it takes over. Keep the current key until that day:
  from then on, a miner pinned to the old key cannot connect until you enter the new key or the new
  V2 URL.
* **The username** goes in the channel's user identity, in the same form as V1's. V2 has no password
  field, so a fleet password goes in the username, after a `#`.
* The pool accepts standard channels, the fit for Bitaxe-class devices, and extended channels, which
  puntominer and proxies use, up to 64 on one connection. Group channels and job declaration are not
  supported.
* The nominal hashrate a channel claims when it opens sizes its first target, for one share per
  share interval. UpdateChannel re-sizes it. Without a claim, the channel starts at the pool's
  floor, or at the difficulty of the channel's `max_target`, the easiest target the miner accepts, if
  that is higher.

## Difficulty

The pool adjusts each V1 connection's, and each V2 channel's, share difficulty to aim at one share
per `share_interval_secs`, 10 s in the default config:

* It starts at the miner's hint, or at the floor without one, within `[vardiff]`'s `floor` and
  `ceiling`.
* It retargets once about ten shares' worth of work has arrived, or after ten intervals of quiet, so
  a miner that goes quiet is eased.
* Each step moves at most four times up or down, and a change under 15% is not sent.

A share is credited at the difficulty its job assigned, never at the difficulty its hash happened to
reach. On V2, a share is checked and credited at the easier of its job's target and the channel's current
one, so a share in flight when the difficulty rises still counts.

## What the Portal Shows

The portal is at `http://<host>:8080` unless the operator serves it elsewhere ([the web
portal](frontend.md)). Its pages need JavaScript.

### Finding Your Fleet

Enter a device's full username, `<address>.<worker>`, on the overview, with its fleet password if
it sets one, in the password field or after a `#`. If that worker has mined here in any pool, the
portal opens the fleet page of its address, `/fleet.html#<handle>`: the private fleet of that password,
or the open fleet without one. Pasted options are ignored, and an address alone opens nothing. A
wrong password finds nothing. Lookups are limited per IP address per minute.

The handle is opaque: it contains neither the address nor the password. Bookmark the fleet page; the
browser also remembers it as "My fleet" in the header. Anyone holding the link sees the fleet, its
address, and its worker names. An open fleet's page says how to make it private.

### The Fleet Page

Every device mining to one address, in every pool, live:

* A card per pool the devices mine in: how many are online, the hashrate over 1 minute, 10 minutes,
  1 hour, and 24 hours, the best share, the shares accepted and refused, and the blocks found. On a
  pool that can pay a finder it shows the odds of a block; on one that can pay a pool output, the
  fleet's share of the window.
* Where the fleet ranks on the stats page, when it does.
* The devices, by worker name: each one's miner type, status (online, idle, or offline with when it
  was last seen), hashrate, pool, best share, last share, and share of its pool's window. A row
  that several devices share, by mining with one username, says how many. An
  offline device stays listed for the operator's `keep_offline_days`, 7 by default; "Forget" hides
  one in this browser.
* A hash-rate chart over 1 hour, 24 hours, or 7 days, and the fleet's blocks.

### The Worker Page

One device, live, linked from its fleet page and from every block it found:

* Its pool, protocol, and miner type, but not its address: its link is public, since the blocks
  page links every finder's.
* Its hashrate over the four windows, its current share difficulty, its best share against the
  network difficulty, and its shares accepted and refused.
* On a pool that can pay a finder, its odds of a block per day, month, and year, and the median and
  average wait for one. On a hybrid pool these are the odds of a finder's payout.
* On a pool that can pay a pool output, its share of the window, and about what a block found now
  would credit it.
* On a pool with a personal range, the part its coinbase pays for a block found now, and the part it
  asked for. On a pool that offers `+name`, whether its name is written.
* Its uptime, when it connected and was first seen, its blocks, and a hash-rate chart.

The fleet and worker pages are kept out of search engines, since each is personal to one miner.

## Odds, Not Promises

Solo mining is a lottery. A block takes, on average, the network difficulty times 2^32 hashes, so a
device of hashrate H finds one in t seconds with probability 1 − e^(−H·t / (D·2^32)). The worker
page quotes these odds from the device's 24-hour hashrate, marked "estimating" until a day's
history has filled. They are odds, not a forecast: a small miner may never find a block, and nothing
guarantees one, or any reward, ever.

The window figure on a pooled or hybrid pool is an estimate too. It is credited only if the pool
finds a block, and it is never a balance.
