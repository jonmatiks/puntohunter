# Installation

This page sets up a host to run a PuntoHunter pool: the Bitcoin Core node it takes work from, the build
of the `puntohunter` binary, the service that runs it, and the ports it listens on. The settings
themselves are in [configuration](configuration.md), and keeping a running pool healthy is in
[operations](operations.md).

## What a Host Needs

* A Linux host. The pool checks Unix file permissions on its keys and password files, and the deploy
  script expects systemd.
* A fully synced Bitcoin Core node, normally on the same host. Node RPC and ZMQ carry no encryption, so
  the pool reads its primary node over loopback.
* A Rust toolchain and a C compiler to build the binary, on the host itself or on another machine with the
  same OS release.
* Disk for the node, and a little for the pool's data directory: its SQLite database, its two key files,
  and a copy of each block it finds.

## Bitcoin Core

### Version

The pool checks no Bitcoin Core version. The test suite runs against Bitcoin Core 32.1, and the stock
release is all it needs: there is no patched build and no separate Template Provider. The pool reads
block templates over RPC with `getblocktemplate`, long-polled, and serves Stratum V2 itself, with its own
authority key.

### bitcoin.conf

The pool needs RPC and the ZMQ `hashblock` notification. A minimal `bitcoin.conf` for a node on the same
host:

```ini
server=1
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
zmqpubhashblock=tcp://127.0.0.1:28332
```

`zmqpubhashblock` tells the pool of each new block at once, so miners get new work without waiting for
the long poll. `txindex` is not needed. With several nodes in the fleet, or many miners, raise the RPC
limits too, for example `rpcthreads=16` and `rpcworkqueue=64`, so the pool's health polling runs beside
template requests and block submissions.

The calls the pool makes of a node are `getblockchaininfo`, `getnetworkinfo`, `getblocktemplate`,
`submitblock`, and `getblockheader`. Payouts, where they are configured, also use the payout wallet on the
primary node, and on mainnet its `estimatesmartfee`.

### RPC Credentials

The pool authenticates to a node in one of three ways, set in `[node]`:

* `cookie_file`: the node's `.cookie` file, which Bitcoin Core writes in its data directory at each start.
  When the pool runs as a user of its own, let it read the cookie through a group: set
  `rpccookieperms=group` in `bitcoin.conf` and add the pool's user to the node's group.
* `rpc_user` with `rpc_password`: a user from the node's `rpcauth` lines, the password in the config.
* `rpc_user` with `rpc_password_file`: the same user, the password in a file of its own. The pool refuses
  to start when anyone but the file's owner can read it, so `chmod 600` it.

### Initial Sync

The pool starts while the node is still syncing, but gives miners no work until the node leaves initial
block download. The health check reports `"node":"ibd"` meanwhile. On mainnet, `getblocktemplate` also
refuses while the node has no peers, and the pool waits for them.

### A Pruned Node

A pruned node works. It cuts disk from about 900 GB (October 2026) to about 25 GB with `prune=10000`,
which keeps about six weeks of blocks. A pruned node cannot rescan blocks it has dropped, so load a wallet
before it receives funds, and restore a wallet backup only while its blocks are still kept; an older
backup needs a full resync. Pruning rules out `txindex`, which the pool does not use.

### Secondary Nodes

The pool can take work from more than one node: `[node]` is the primary, and each `[[secondary_nodes]]`
entry another (see [configuration](configuration.md)). A secondary node on another host is reached over
the network, and its RPC and ZMQ must never cross the public internet: RPC is HTTP basic authentication
without encryption, and ZMQ has no authentication at all. Join the hosts with a private tunnel such as
WireGuard, bind the node's RPC and ZMQ to loopback and the tunnel address alone, and give the pool an
`rpcauth` user of its own. An `rpcwhitelist` can limit that user to the calls a secondary node answers:

```ini
rpcwhitelist=puntohunter:getblockchaininfo,getnetworkinfo,getblocktemplate,submitblock,getblockheader
rpcwhitelistdefault=0
```

`rpcwhitelistdefault=0` leaves the local cookie unrestricted.

## Building the Binary

Install Rust with [rustup](https://rustup.rs) and a C compiler (on Debian or Ubuntu, `build-essential`):
the build compiles SQLite and a TLS library from source. The code is Rust 2024 and uses `let` chains, so
it needs Rust 1.89 or newer; the latest stable release is the one to use.

```sh
git clone https://github.com/jonmatik/puntohunter.git
cd puntohunter
cargo build --release --locked
```

The binary is `target/release/puntohunter`, one file with the portal, [web/](../../web/), built in.
`puntohunter --version` prints the package version and the commit it was built from, such as
`0.2.0+1a2b3c4d`, marked `-dirty` when the sources differed from that commit.

A binary built on one machine runs on another with a compatible OS release, such as the same Ubuntu
release. [deploy.sh](../../deploy.sh) relies on that: it builds locally and uploads the result (see
[operations](operations.md)).

## Running the Pool

The pool runs as one command:

```sh
puntohunter serve --config /path/to/pool.toml
```

It validates the whole config first, and a refused value names its key. Relative paths in the config,
and the default data directory `data/<network>`, are read against the working directory, so set
`data_dir` to an absolute path on a server.

It logs to standard output. Without `RUST_LOG` the level is `info`, with the ZMQ library's reconnect
warnings silenced; setting `RUST_LOG`, such as `RUST_LOG=debug`, replaces both.

At startup the pool logs, in order:

* `config loaded`, then `store open` with the data directory;
* `SV2 authority key`, the public key Stratum V2 miners pin;
* `public Stratum endpoint`, the host and each pool's public ports;
* `ready`, once every Stratum listener and the portal are bound.

From `ready` on, the metrics endpoint reports `puntohunter_ready 1`. Miners get work only once the node
is synced, which the health check, `curl -s 127.0.0.1:8080/api/v1/health`, reports as `"work":true`.
Ctrl-C stops the pool.

On first run the pool creates the data directory, its database, `sv2-authority.key`, and `handle.key`.
Keep them: [operations](operations.md) says why.

### The Mainnet Script

[mainnet.sh](../../mainnet.sh) builds the release binary and runs it with
[config/mainnet.toml](../../config/mainnet.toml) from the repository root, logging to the terminal and to
a new file in `data/main/logs/` on each run. It suits a pool run by hand on a desktop or a home server.
The checked-in config is an example: set its cookie path and `[public] host` to your own, and replace
`pool_wallet`, BIP 173's example address, whose key is public, with an address of your own wallet before the
first run.

### Trying It on Regtest

[config/regtest.toml](../../config/regtest.toml) runs every mode, pooled and hybrid payouts included,
against a throwaway regtest chain. `tests/e2e/live.py up` starts that chain in Docker on the config's
ports, with the wallets it needs; then run `puntohunter serve --config config/regtest.toml`, and stop the
chain with `tests/e2e/live.py down`. The script needs Docker and the Python packages of
`tests/e2e/requirements.txt`.

### Other Commands

These write to the database, so run them as the user that owns the data directory, beside a running pool
or not:

* `puntohunter token issue <address> --config <file>` issues a balance token, which opens one address's
  private balance view at `/balance.html`. It is printed once; only its hash is kept.
* `puntohunter reserve credit <sats> --config <file>` credits the fee reserve that payout batches pay
  their transaction fees from. It needs `[payouts]`.
* `puntohunter payouts resume --config <file>` lifts a pause of payouts once you have settled its cause
  ([When Payouts Pause](operations.md#when-payouts-pause)).

## Running as a systemd Service

[deploy/puntohunter.service](../../deploy/puntohunter.service) is the unit below. [deploy.sh](../../deploy.sh)
expects the pool to run as the systemd service `puntohunter`, with the binary at
`<installation directory>/puntohunter`. One layout that works, with everything under `/opt/puntohunter`:

```text
/opt/puntohunter/                root:root 755
├── puntohunter                  root:root 755                the binary; puntohunter.prev is the one before
├── config/pool.toml             root:root 644                the config
├── config/<node>.password       puntohunter:puntohunter 600  a secondary node's RPC password, if any
└── data/                        puntohunter:puntohunter 700  the data directory
```

root owns the binary and the config, so the service cannot rewrite what it runs. If the config holds an
`rpc_password`, make it `root:puntohunter 640` instead.

Create the user and the directories, as root:

```sh
useradd --system --user-group --home-dir /opt/puntohunter --no-create-home \
  --shell /usr/sbin/nologin puntohunter
usermod -aG bitcoin puntohunter    # to read the node's cookie through its group
install -d -m 755 /opt/puntohunter /opt/puntohunter/config
install -d -o puntohunter -g puntohunter -m 700 /opt/puntohunter/data
install -m 755 target/release/puntohunter /opt/puntohunter/puntohunter
install -m 644 pool.toml /opt/puntohunter/config/pool.toml
```

Set `data_dir = "/opt/puntohunter/data"` in the config. Then write
`/etc/systemd/system/puntohunter.service`:

```ini
[Unit]
Description=PuntoHunter mining pool
After=network-online.target bitcoind.service
Wants=network-online.target bitcoind.service

[Service]
User=puntohunter
Group=puntohunter
SupplementaryGroups=bitcoin
WorkingDirectory=/opt/puntohunter
ExecStart=/opt/puntohunter/puntohunter serve --config /opt/puntohunter/config/pool.toml
Restart=on-failure
RestartSec=5
LimitNOFILE=65536
CPUWeight=200

[Install]
WantedBy=multi-user.target
```

`bitcoind.service` is the node's unit, if it runs under systemd; drop it from `After=` and `Wants=`
otherwise, and drop `SupplementaryGroups=` when the pool does not read the cookie through a group.
`LimitNOFILE` leaves room for a thousand miner connections beside the node, portal, and metrics sockets.
On a host shared with the node, `CPUWeight=200` here and a lower weight and memory limits on the node,
such as `CPUWeight=50` and `MemoryMax=`, keep a node's initial sync or a reorg from starving the pool.

Start it:

```sh
systemctl daemon-reload
systemctl enable --now puntohunter
journalctl -u puntohunter -f
```

## Ports

| Port | Serves | Open to |
| --- | --- | --- |
| Each pool's `v1` and `v2` | Stratum V1 and V2, such as 3333 and 3334 | Miners |
| `[portal] bind`, 8080 in the examples | The portal and `/api/v1`, plain HTTP | Visitors, or a reverse proxy |
| `[metrics] bind`, 127.0.0.1:9100 in the examples | Prometheus metrics | Loopback only |
| 8332 and 28332 | Node RPC and ZMQ | Loopback, or a private tunnel to the pool's host |
| 8333 | Bitcoin P2P, the node's own | As the node needs |

Every pool has listeners of its own, so open each port the config's `[[pools]]` entries bind. With ufw,
for example, for the three example pools and the portal:

```sh
ufw allow 22/tcp
ufw allow 3333,3334,4333,4334,5333,5334/tcp
ufw allow 8080/tcp
ufw enable
```

On a LAN, admit the LAN alone, such as `ufw allow from 192.0.2.0/24 to any port 3333,3334,8080 proto tcp`
with your LAN's range in place of `192.0.2.0/24`.
Never expose node RPC or ZMQ beyond loopback or the tunnel, and keep metrics on loopback.

Stratum V2 connections are encrypted, and miners check the pool by its authority key; Stratum V1 and the
portal are plain.
For HTTPS, put a reverse proxy such as Caddy or nginx in front of the portal, and set
`[portal] trusted_proxy` to its address, so the portal's limits key on each visitor rather than on the
proxy. A proxy or port forward that gives miners other ports than the listeners' is named with a pool's
`public_v1` and `public_v2`, and the host miners connect to with `[public] host`, or, behind a proxy such
as Cloudflare's that does not pass Stratum, a `stratum.` name for each site with `[public] domains` (see
[configuration](configuration.md)).
