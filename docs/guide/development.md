# Development

This page takes you from a fresh clone to a pool running on your machine and a passing test run, and shows
where the code lives and which rules a change keeps. [Contributing](contributing.md) says how to propose a
change, and the [architecture pages](architecture/README.md) explain how the code works.

## What You Need

| Tool | For |
| --- | --- |
| Rust, stable, with `rustfmt` and `clippy` | Building and the Rust tests. The crate uses edition 2024. |
| Docker with Compose | The end-to-end suite and a local regtest chain: Bitcoin Core runs in containers. |
| Python 3.14 | The end-to-end suite, in a virtual environment with the pinned packages of [requirements.txt](../../tests/e2e/requirements.txt). |
| Google Chrome | The browser suite, which drives the portal's pages headless with the host's own Chrome. |
| Node.js | `markdownlint-cli2`, run with `npx`. |

CI runs on Ubuntu 24.04, and the suite is developed on Linux.

## Building

```sh
cargo build --release --locked
```

The binary is `target/release/puntohunter`. Its version names the commit it is built from, as
`0.2.0+1a2b3c4d`, and ends in `-dirty` when the tree has uncommitted changes ([build.rs](../../build.rs)).
The workspace also holds [sv2-testclient/](../../sv2-testclient/), the end-to-end suite's Stratum V2 client;
`--workspace` builds it too.

## Running a Pool on Regtest

[config/regtest.toml](../../config/regtest.toml) runs every mode, pooled and hybrid payouts included,
against a throwaway regtest chain. Set up the Python environment once (as under [Tests](#tests)), then:

```sh
.venv/bin/python tests/e2e/live.py up
cargo run --release -- serve --config config/regtest.toml
```

`live.py up` starts Bitcoin Core in Docker on the config's ports, creates the payout wallet and funds it,
and prints the chain's tip and a regtest address for each role. Every `up` starts a fresh chain.

* **The portal** is at `http://localhost:8080`; the metrics are at `http://127.0.0.1:9100/metrics`.
* **A miner** connects over Stratum V1 to `stratum+tcp://<this host>:3333` (solo), `4333` (pooled), or
  `5333` (hybrid), with the username `<address>.<worker>`, the address one that `live.py up` printed. V2
  listens one port above each, and the pool logs the authority key a V2 miner pins on the line
  `SV2 authority key`. Regtest's difficulty is so low that any CPU miner finds blocks.
* **A block** to move the chain without a miner, which gives miners a new template:

  ```sh
  docker exec puntohunter-live-bitcoind-1 bitcoin-cli -regtest -rpcuser=puntohunter \
    -rpcpassword=puntohunter generatetoaddress 1 <address>
  ```

Stop the pool with Ctrl-C and the chain with `.venv/bin/python tests/e2e/live.py down`. `live.py fleet-up`
starts three connected nodes instead and writes `data/regtest-fleet.toml` for a pool with secondary nodes;
`live.py fleet-down` stops them. The pool's data directory is `data/regtest/`, which git ignores; delete it
to start the pool's own records afresh.

## Working on the Web App

The binary embeds [web/](../../web/) as it was when it was built. To see an edit to the app on reload, serve
`web/src/` as it is against a running pool:

```sh
python3 web/serve.py --api http://localhost:8080
```

and admit the app's origin in the pool's config, `[api] allowed_origins = ["http://localhost:8000"]`.
[web/README.md](../../web/README.md) describes the app, its layout, and the rules it keeps.

## Tests

The unit tests sit beside the code they test, and [tests/serve.rs](../../tests/serve.rs) runs the built
binary:

```sh
cargo test --workspace --locked
```

The end-to-end suite in [tests/e2e/](../../tests/e2e/) drives the release binary on regtest, with Bitcoin
Core in Docker, real Stratum V1 and V2 clients, the Stratum V2 reference implementation's interop images, and
the portal in headless Chrome:

```sh
python3 -m venv .venv
.venv/bin/pip install -r tests/e2e/requirements.txt
docker compose -f tests/e2e/docker-compose.sri.yml build
.venv/bin/pytest tests/e2e -n 4
```

* The suite builds the release binaries itself; `PUNTOHUNTER_BIN` names a pool binary to use instead. It
  builds the regtest node's image on first use. Build the interop images first, as above: they take
  minutes, longer than a test may wait.
* Each test has 120 seconds ([pytest.ini](../../tests/e2e/pytest.ini)). `-n 4` runs four workers; CI runs
  two.
* The default run leaves out two marked groups. `-m load` runs 1,000 simulated workers against a pool, and
  `-m hardware` checks a Bitaxe on your LAN against a running pool; each file's docstring gives its
  settings.
* A test that fails under `-n` but passes alone, `pytest tests/e2e/<file>::<test>`, is likely timing on a
  loaded machine; one that fails alone is a real failure.
* [failpoint.rs](../../src/failpoint.rs) gives the crash-recovery tests points where the release binary
  stops or a commit fails, chosen by `PUNTOHUNTER_FAILPOINT`.

Builds and parallel runs load every core; on a laptop, watch its temperature and use fewer jobs
(`CARGO_BUILD_JOBS`) or workers if it runs hot.

## Before You Send a Change

CI runs these on every pull request; run them first:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
npx markdownlint-cli2@0.23.2 "**/*.md"
.venv/bin/pytest tests/e2e -n 4
```

## Where Things Live

| Path | What it holds |
| --- | --- |
| [src/](../../src/) | The engine; the [architecture overview](architecture/README.md) maps each module to its page |
| [tests/e2e/](../../tests/e2e/) | The end-to-end suite, its Compose files, and `live.py` |
| [tests/data/](../../tests/data/) | Captured mining and node fixtures the unit tests read |
| [sv2-testclient/](../../sv2-testclient/) | The end-to-end suite's Stratum V2 client |
| [web/](../../web/) | The portal's web app, which the binary embeds and any static host can serve |
| [config/](../../config/) | The default configs, for mainnet and regtest |
| [deploy/](../../deploy/) | Templates for running a pool under systemd, with backups, a tunnel, and monitoring |
| [docs/guide/](README.md) | This guide, which follows the code |

## The Rules a Change Keeps

[AGENTS.md](../../AGENTS.md) holds the project's rules, for people and AI agents alike. In short:

* **Rust style.** One merged `use` per crate, in three groups (standard library, external crates, this
  crate), and no glob imports outside a test module's `use super::*`. `cargo fmt` sorts within a group.
* **Markdown.** Every `*.md` file lints clean, at most 110 columns, with spaced table delimiter rows.
* **The web app.** A change a visitor can see is a change to `web/`. `web/build.py` and the binary's
  [site.rs](../../src/portal/site.rs) assemble pages by the same rules, and a test checks every file the
  binary serves against `build.py`'s output byte for byte, so a change to one is a change to the other.
* **The guide follows the code.** A change to a setting, an endpoint, a Stratum option, or a component
  updates its page of this guide in the same change.
* **The changelog.** A change a user or an operator can see gets an entry at the top of
  [CHANGELOG.md](../../CHANGELOG.md), under an `## Unreleased` heading, which the first change after a
  release adds.
* **The database.** A schema change edits [0001_schema.sql](../../src/store/migrations/0001_schema.sql) in
  place, and its changelog entry gives the SQL that brings an existing database up to date.
