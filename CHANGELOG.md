# PuntoHunter

## Unreleased

## [0.2.1] 2026-11-13

* fix: the portal lookup gives the username the full card width, with password and button below and
  stacked controls on phones
* fix: show the overview's quote above the introduction while statistics load, and label the group
  member column Region
* fix: readable text on accent-filled buttons and selected tabs in dark and light themes
* feat: site overlays can replace `brand.css` and add footer content without copying shared styles or credits
* docs: from a run of the guide on a clean Ubuntu 24.04: the installation page gives the commands for trying
  the pool on regtest, its Python environment included, and both it and the development page say that a CPU
  miner needs a lower `[vardiff] floor` than the regtest config's 1.0. The development page says how to get
  Python 3.14 and Chrome on Ubuntu 24.04
* test: `tests/e2e/run.sh` refuses a Python older than 3.14, which the suite needs, and `E2E_PYTHON` names the
  one that makes `.venv`; five tests that read the pool's records before it wrote them now wait for them

## [0.2.0] 2026-11-09

The first public release. What it ships, by area:

* **Pools:** any number of solo, pooled, and hybrid pools side by side from one binary and your own Bitcoin
  Core node, each on its own Stratum ports, with its own finder part, fees, PPLNS window, and personal parts
  a worker may ask for; worker names in blocks on request. See [Pools and Modes](docs/guide/pools-and-modes.md).
* **Stratum:** V1 with version rolling and per-connection vardiff; V2 over the Noise handshake, on standard
  and extended channels, with an authority key that can be rotated on an announced day. Usernames carry the
  payout address, the worker name, and an optional fleet password. See [Mining](docs/guide/mining.md).
* **Nodes:** a fleet of Bitcoin Core nodes over RPC and ZMQ: work from the fittest node on the most-work
  chain, failover when one stalls, falls behind, or leaves the chain, and each block submitted to every node
  and kept on disk until one answers.
* **Accounting:** a durable share journal, difficulty-weighted PPLNS, an append-only ledger with coinbase
  maturity, and batched on-chain payouts from a wallet on the node, encrypted if you choose, with a fee
  reserve, reconciliation at startup, and a pause when a paid block leaves the chain.
* **Storage:** one SQLite database, with snapshots on a timer and after each allocation and payout.
* **Portal and API:** the web portal, embedded in the binary and branded by config, over the JSON API under
  `/api/v1`, with live streams: fleets and workers by opaque handles, never by address; private fleets;
  private balances; pool stats; and groups of instances. Any static host can serve the same app as a site of
  your own. See [The Web Portal](docs/guide/frontend.md) and the [API Reference](docs/guide/api.md).
* **Operations:** a health check, Prometheus metrics, `deploy.sh` with rollback, `unlock.sh` and
  `payout-wallet.sh` for an encrypted payout wallet, and templates for the systemd unit, backups, a tunnel,
  and monitoring. See [Operations](docs/guide/operations.md).
* **Documentation:** [the guide](docs/guide/README.md), for operators and miners, with pages on development,
  contributing, and the architecture.
