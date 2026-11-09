# Quick Start

This page is the shortest path from nothing to a miner on your own PuntoHunter pool, on mainnet,
mining solo. [Installation](installation.md) covers the full setup: the host, the service, and the
firewall.

## What You Need

* A host with a fully synced Bitcoin Core node. The pool waits while the node is in initial block
  download. A pruned node works too, and needs about 25 GB of disk with `prune=10000`.
* A stable Rust toolchain, from [rustup](https://rustup.rs), and a C compiler: SQLite is built into
  the binary.
* A Bitcoin address of your own to be paid at, and a miner, such as a Bitaxe.

## Set Up the Node

The pool asks the node for block templates over RPC and hears of new blocks over ZMQ. In
`bitcoin.conf`:

```ini
server=1
zmqpubhashblock=tcp://127.0.0.1:28332
```

Restart the node after the change. txindex is not needed. The pool reads the node's cookie file, or
a user and password, so nothing else needs setting on the node.

## Build

```sh
git clone https://github.com/jonmatik/puntohunter.git
cd puntohunter
cargo build --release
```

The binary is `target/release/puntohunter`.

## Configure

Edit [config/mainnet.toml](../../config/mainnet.toml). The checked-in values are examples; set these
to your own:

* `[node]`: `rpc_url`, `zmq_hashblock`, and `cookie_file`, the path of the node's `.cookie`. A node
  that uses a user and password takes `rpc_user` and `rpc_password` in place of `cookie_file`.
* `[addresses]`: `pool_wallet`, a `bc1…` address of yours, in place of BIP 173's example address, whose key
  is public. A solo pool never pays it, but the config requires it.
* `[public]`: `host`, the domain name or IP address miners connect to, which the portal shows them.
  Without it the portal shows the host the browser used; behind a web proxy, set `domains` instead (see
  [configuration](configuration.md)).

The file defines three pools: `solo` on ports 3333 (V1) and 3334 (V2), `pooled` on 4333 and 4334,
and `hybrid` on 5333 and 5334. It sets no fees, so a solo block pays its finder the whole reward.
Pooled and hybrid pools pay out only with `[payouts]`, which this file leaves out, so mine on the solo
pool, or remove the other two `[[pools]]` entries, until you configure it. [Configuration](configuration.md)
describes every key, and [pools and modes](pools-and-modes.md) what each pool pays.

## Start

```sh
./mainnet.sh
```

[mainnet.sh](../../mainnet.sh) builds the release binary and runs
`puntohunter serve --config config/mainnet.toml`. It logs to the terminal and to a new file in
`data/main/logs/` on each run. Ctrl-C stops the pool. Set `RUST_LOG` to change the log level; the
default is `info`.

The pool keeps its state in `data/main/`, which it creates on its first run. Keep that directory: it
holds the database, the SV2 authority key, and the key behind every portal link.

The pool has started once every listener is bound:

```sh
curl -s 127.0.0.1:9100/metrics | grep puntohunter_ready
```

reports `puntohunter_ready 1`. Miners get work once the node is synced, which
`curl -s 127.0.0.1:8080/api/v1/health` reports as `"work":true`. While the node is in initial block
download, it reports `"node":"ibd"`.

## Point a Miner at the Pool

In the miner's pool settings:

* **URL:** `stratum+tcp://<host>:3333`, the solo pool's Stratum V1 port.
* **Username:** `<bc1 address>.<worker>`, your address, a dot, and a name for this device, such as
  `bc1q….bitaxe1`. A block the device finds pays that address in its coinbase.
* **Password:** `x`, or a fleet password of 8 characters or more to make your fleet page private
  ([private fleets](mining.md#private-fleets)).

For Stratum V2, use `stratum2+tcp://<host>:3334/<authority key>` instead. The pool logs its
authority key at startup, on the line `SV2 authority key`, and the portal's overview shows it.
[Mining](mining.md) describes every option a username takes.

A miner on another machine needs the Stratum and portal ports open in the host's firewall. Never
expose the node's RPC (8332) or ZMQ (28332) port beyond loopback.

## Watch It Mine

Open `http://<host>:8080` and enter the device's full username, `<address>.<worker>`. The portal
opens the fleet page: every device mining to that address, each with its status, hashrate, and best
share, live. Each device links to a page of its own, with its odds of a block. Bookmark the fleet
page; an address alone opens nothing.

Solo mining is a lottery: a small miner may never find a block, and nothing guarantees one.
