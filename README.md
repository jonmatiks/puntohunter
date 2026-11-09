# PuntoHunter

**Status: beta.** PuntoHunter runs pools on mainnet, but it is young: test a release on regtest before you
run it, and expect changes between releases.

## Multi-Mode Mining

**PuntoHunter** is Bitcoin mining operations software that can run any number of solo, pooled, and hybrid
mining pools side by side, each on its own Stratum ports, allowing operators to select or switch between them
according to their needs.

**One platform. Multiple ways to mine.**

## Quick Start

1. Run a fully synced Bitcoin Core node on the same host, with RPC (`server=1`) and ZMQ block notifications
   (`zmqpubhashblock=tcp://127.0.0.1:28332`). A pruned node works.
2. Install a stable Rust toolchain, and edit [config/mainnet.toml](config/mainnet.toml): the node, the pools
   and their ports, the pool wallet's address, and the host miners connect to.
3. Start the pool:

   ```sh
   ./mainnet.sh
   ```

4. Point a miner at `stratum+tcp://<host>:3333` with the username `<payout address>.<worker name>`, and
   open `http://<host>:8080` to watch it. A password of 8 characters or more makes its fleet page private.

Pooled and hybrid pools pay their miners in batched on-chain payouts where the operator configures them.
**The operator of a pooled or hybrid pool holds its miners' funds** from a block until its payouts, in a wallet
on the pool's node: keep that wallet, its passphrase, and its backups safe, as
[Operations](docs/guide/operations.md#the-payout-wallet) describes.

## Documentation

The [guide](docs/guide/README.md), also published at <https://puntohunter.com/docs/>, covers the rest:

* [Quick Start](docs/guide/quick-start.md) and [Installation](docs/guide/installation.md): the node, the
  build, the service, and the ports.
* [Configuration](docs/guide/configuration.md): every section and key of the config.
* [Pools and Modes](docs/guide/pools-and-modes.md) and [Mining](docs/guide/mining.md): what each pool pays,
  and what miners enter over Stratum V1 and V2.
* [The Web Portal](docs/guide/frontend.md): [web/](web/), the static web app over the pool's API that the
  pool serves as its portal, branded by config, and that any static host can serve as a site of your own.
* [API Reference](docs/guide/api.md): every resource of `/api/v1`.
* [Operations](docs/guide/operations.md): deploying with [deploy.sh](deploy.sh), health, metrics, and
  upgrades.
* [Development](docs/guide/development.md) and [Contributing](docs/guide/contributing.md): building and
  testing PuntoHunter, and proposing a change.
* [Architecture](docs/guide/architecture/README.md): how the code works, its parts, and why it is built this
  way, for developers and contributors.

## License

[MIT](LICENSE).
