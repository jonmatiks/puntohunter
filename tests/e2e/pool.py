"""The pool under test: the release binary, run on the host with a generated config."""

import contextlib
import copy
import os
import re
import sqlite3
import subprocess
import tomllib

import tomli_w

from clients.http import Http
from regtest import REGTEST_CONFIG, ROOT, free_port
from wait import wait_until

READY_TIMEOUT = 30
# Below regtest's network difficulty (about 4.7e-10), so the share target is easier than the block target
# and about half of all shares are blocks.
HARNESS_FLOOR = 2.4e-10
# The dust limit of the regtest pool wallet's P2WPKH script: a pool output below it goes to the finder.
POOL_WALLET_DUST = 294
# The pools of config/regtest.toml, whose ports the harness names, as solo_v1_port.
DEFAULT_POOLS = ("solo", "pooled", "hybrid")


def portion(entry, reward):
    """What a fee or a finder takes of `reward` sats before any cap: floor(reward × percent%), or its fixed btc
    or sats."""
    if "percent" in entry:
        return reward * round(entry["percent"] * 10_000) // 1_000_000
    if "btc" in entry:
        return round(entry["btc"] * 100_000_000)
    return entry["sats"]


def dust(address):
    """The dust limit of a regtest address's script: P2PKH, P2SH, P2WPKH, or a 32-byte witness program."""
    if address.startswith(("m", "n")):
        return 546
    if address.startswith("2"):
        return 540
    return 294 if address.startswith("bcrt1q") and len(address) == 44 else 330


def build():
    """Builds the release binaries, the pool and sv2-testclient, unless PUNTOHUNTER_BIN names the pool's, and
    returns the pool's path."""
    if binary := os.environ.get("PUNTOHUNTER_BIN"):
        return binary
    subprocess.run(
        ["cargo", "build", "--release", "--locked", "--quiet", "--workspace"], cwd=ROOT, check=True
    )
    return str(ROOT / "target" / "release" / "puntohunter")


class Pool:
    """The pool, configured from config/<base>.toml pointed at the cluster's node and this run's ports.
    `overrides` maps a config section to the keys it replaces, a key set to None to be removed, an array of
    tables, such as `fees`, to its new entries, or None to remove it. `overrides["pools"]` maps a pool's id to
    the keys it replaces, None to remove that pool, and a new id to the keys of a pool added after the others.
    Each pool's `v1` and `v2`, where it has them, listen on a free port, whatever an override sets them to: the
    default pools' as `solo_v1_port` and its siblings, and every pool's as `port(id, protocol)`. The base
    config's `payouts` section is removed unless overridden, when the override starts from it: payouts run
    only where a test sets them up. `env` adds environment variables, such as a failpoint, for each start until it is changed."""

    def __init__(self, binary, node, workdir, base="regtest", overrides=None, env=None):
        self.binary = binary
        self.env = env or {}
        workdir.mkdir(parents=True, exist_ok=True)
        self.config_path = workdir / "puntohunter.toml"
        self.log_path = workdir / "puntohunter.log"
        self.process = None
        self.data_dir = workdir / "data"
        self.solo_v1_port = free_port()
        self.pooled_v1_port = free_port()
        self.solo_v2_port = free_port()
        self.pooled_v2_port = free_port()
        self.hybrid_v1_port = free_port()
        self.hybrid_v2_port = free_port()
        self.metrics_port = free_port()
        self.portal_port = free_port()
        self.config = tomllib.loads((ROOT / "config" / f"{base}.toml").read_text())
        # Whatever the base, the node is the cluster's regtest node, with its credentials.
        self.config["node"] = {
            **copy.deepcopy(REGTEST_CONFIG["node"]),
            "rpc_url": f"http://127.0.0.1:{node.rpc_port}",
            "zmq_hashblock": f"tcp://127.0.0.1:{node.zmq_port}",
        }
        self.config["data_dir"] = str(self.data_dir)
        self.config["metrics"]["bind"] = f"127.0.0.1:{self.metrics_port}"
        self.config["portal"]["bind"] = f"127.0.0.1:{self.portal_port}"
        self.config["vardiff"]["floor"] = HARNESS_FLOOR
        # Another test's pool would pay from the node's payout wallet, or halt payouts over the coinbases tests
        # mine to it.
        payouts = self.config.pop("payouts", None)
        # The suite looks workers up many times from 127.0.0.1; the rate limit's own test sets it back.
        self.config["portal"]["lookups_per_minute"] = 1_000_000
        # The base config's public host, if any, is not the test's; a test sets its own.
        self.config.pop("public", None)
        overrides = dict(overrides or {})
        pools = {pool["id"]: pool for pool in self.config.get("pools", [])}
        for id, values in overrides.pop("pools", {}).items():
            if values is None:
                pools.pop(id, None)
                continue
            kept = pools.setdefault(id, {"id": id})
            kept.update(values)
            for key in [key for key, value in values.items() if value is None]:
                del kept[key]
        self.ports = {}
        for id, pool in pools.items():
            for protocol in ("v1", "v2"):
                if protocol in pool:
                    port = getattr(self, f"{id}_{protocol}_port") if id in DEFAULT_POOLS else free_port()
                    self.ports[(id, protocol)] = port
                    pool[protocol] = f"127.0.0.1:{port}"
        self.config["pools"] = list(pools.values())
        for section, values in overrides.items():
            if values is None:
                self.config.pop(section, None)
            elif isinstance(values, list):
                self.config[section] = values
            else:
                base = copy.deepcopy(payouts) if section == "payouts" else {}
                kept = self.config.setdefault(section, base or {})
                kept.update(values)
                for key in [key for key, value in values.items() if value is None]:
                    del kept[key]
        # The payout addresses of the run: the config's, and every address a test may mine to. No response
        # may contain one, but for the address a fleet or worker page shows (clients/http.py).
        self.payout_addresses = (
            set(self.config["addresses"].values())
            | {fee["address"] for fee in self.config.get("fees", [])}
            | {fee["address"] for pool in self.config["pools"] for fee in pool.get("fees", [])}
            | set(node.wallet_addresses())
        )
        # The portal, fetched through the privacy scan.
        self.portal = Http(self.portal_port, self.payout_addresses)

    def start(self):
        """Starts the process and returns once it reports ready."""
        self.config_path.write_text(tomli_w.dumps(self.config))
        with open(self.log_path, "a") as log:
            self.process = subprocess.Popen(
                [self.binary, "serve", "--config", self.config_path],
                stdout=log,
                stderr=subprocess.STDOUT,
                env={**os.environ, **self.env},
            )
        wait_until(self._ready, READY_TIMEOUT, "puntohunter_ready 1")

    def kill(self):
        if self.process and self.process.poll() is None:
            self.process.kill()
            self.process.wait()

    def restart(self):
        self.kill()
        self.start()

    def metrics(self):
        """The metrics endpoint's scrape, through the privacy scan."""
        response = Http(self.metrics_port, self.payout_addresses).get("/metrics")
        assert response.status == 200, response.status
        return response.body

    def pool(self, id):
        """Pool `id`'s config entry."""
        [entry] = [pool for pool in self.config["pools"] if pool["id"] == id]
        return entry

    def port(self, id, protocol):
        """The port pool `id` listens on for `protocol`, v1 or v2."""
        return self.ports[(id, protocol)]

    def reward_outputs(self, reward, payee, hybrid=False, pool=None, finder=None):
        """The reward outputs for a block of `reward` sats in `pool`, by default the solo pool, or the hybrid
        pool where `hybrid`, whose finder is `payee`: (address, sats) for the finder's part and the pool output,
        each left out at 0, then each of the pool's fees in order, floor(reward × percent%) or its fixed
        amount, capped at what the fees before it leave, the miner portion taking the
        rest. A solo pool's coinbase pays all of the miner portion to `payee`. `finder`, a
        portion as the config writes one, stands for the pool's own: a worker's part, already clamped."""
        entry = self.pool(pool or ("hybrid" if hybrid else "solo"))
        left, fees = reward, []
        for fee in entry.get("fees", self.config.get("fees", [])):
            sats = min(portion(fee, reward), left)
            left -= sats
            fees.append((fee["address"], sats))
        finder = min(portion(finder or entry["finder"], reward), left)
        rest = left - finder
        if finder and rest < POOL_WALLET_DUST:
            finder, rest = left, 0
        elif rest and finder < dust(payee):
            finder, rest = 0, left
        miner = [(payee, finder)] if finder else []
        pooled = [(self.config["addresses"]["pool_wallet"], rest)] if rest else []
        return miner + pooled + fees

    def logs(self):
        return self.log_path.read_text() if self.log_path.exists() else ""

    def issue_token(self, address):
        """Runs `puntohunter token issue` for `address` and returns the balance token it prints."""
        issued = subprocess.run(
            [self.binary, "token", "issue", address, "--config", self.config_path],
            capture_output=True,
            text=True,
        )
        assert issued.returncode == 0, issued.stderr
        return re.search(rf"balance token for {address}: ([0-9a-f]{{64}})$", issued.stdout, re.M)[1]

    def credit_reserve(self, sats, config=None):
        """Runs `puntohunter reserve credit` against the pool's config, or `config`, and returns the process."""
        return subprocess.run(
            [self.binary, "reserve", "credit", str(sats), "--config", config or self.config_path],
            capture_output=True,
            text=True,
        )

    def authority_key(self):
        """The SV2 authority key the pool printed last, in SRI's base58check form."""
        return re.findall(r"authority_key=(\w+)", self.logs())[-1]

    def blocks(self):
        """The pool's blocks table, as rows keyed by block hash."""
        with contextlib.closing(sqlite3.connect(self.data_dir / "puntohunter.sqlite3")) as db:
            db.row_factory = sqlite3.Row
            return {row["hash"]: dict(row) for row in db.execute("SELECT * FROM blocks")}

    def rows(self, sql, *params):
        """The rows a query of the pool's database returns, as dicts."""
        with contextlib.closing(sqlite3.connect(self.data_dir / "puntohunter.sqlite3")) as db:
            db.row_factory = sqlite3.Row
            return [dict(row) for row in db.execute(sql, params)]

    def journal(self):
        """The share journal, oldest first, each proof as a block hash in display order."""
        rows = self.rows("SELECT * FROM journal ORDER BY sequence")
        for row in rows:
            row["proof"] = bytes(row["proof"])[::-1].hex()
        return rows

    def exited(self):
        """Whether the process has exited, as a failpoint makes it."""
        return self.process is not None and self.process.poll() is not None

    def rollups(self, handle):
        """The worker's rollup rows, oldest first."""
        return self.rows("SELECT * FROM rollups WHERE handle = ? ORDER BY start, transport", handle)

    def block(self, block_hash):
        """The block's row, or an empty dict before it is recorded."""
        return self.blocks().get(block_hash, {})

    def block_hex(self, block_hash):
        """The block hex the pool wrote to disk."""
        return (self.data_dir / "blocks" / f"{block_hash}.hex").read_text()

    def _ready(self):
        if self.process.poll() is not None:
            raise AssertionError(f"pool exited with {self.process.returncode}:\n{self.logs()}")
        try:
            return "puntohunter_ready 1" in self.metrics().splitlines()
        except OSError:
            return False
