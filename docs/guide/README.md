# PuntoHunter Guide

PuntoHunter is open-source Bitcoin mining pool software. It can run any number of solo, pooled, and hybrid
pools side by side, each on its own Stratum ports, all from one binary and your own Bitcoin Core node. Miners
connect over Stratum V1 or V2, and watch their devices live on the pool's own portal or on a site built on its
API.

This guide is for operators who run a pool and for the miners who mine on one, and its development,
contributing, and architecture pages are for developers and contributors. It follows the code: a change to a
setting, an endpoint, a Stratum option, or a component updates its page in the same change.

## Pages

1. [Quick Start](quick-start.md): from a synced node to a first miner.
2. [Installation](installation.md): the host, the node, the build, the service, and the ports.
3. [Configuration](configuration.md): every section and key of the config.
4. [Pools and Modes](pools-and-modes.md): solo, pooled, and hybrid pools, finder parts, fees, and PPLNS.
5. [Mining](mining.md): what a miner enters, over V1 and V2, and what the portal shows.
6. [The Web Portal](frontend.md): the portal the pool serves, its brand, and a site of your own.
7. [API Reference](api.md): every resource of `/api/v1`.
8. [Operations](operations.md): deploying, health and metrics, upgrades, and what to keep safe.
9. [Development](development.md): building, running a pool on regtest, the tests, and the project's rules.
10. [Contributing](contributing.md): reporting a bug or a vulnerability, proposing a change, and review.
11. [Architecture](architecture/README.md): how the code works, its parts, and why it is built this way.

The list above is the guide's order: the website's sidebar is read from it, with each architecture page
after the architecture overview, in its README's order.

PuntoHunter is [MIT licensed](../../LICENSE).
