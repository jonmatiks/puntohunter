# PuntoHunter

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
