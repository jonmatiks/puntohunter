"""Interop with the SRI reference roles of sv2-apps v0.8.0: its mining device on a standard channel, and its translator on an extended channel with a V1 client
behind it. The harness's SV2 client shares the pool's codec; these roles are what check that codec."""

import fcntl
import os
import pathlib
import subprocess
import tempfile
import uuid

import pytest
import tomli_w

from chain import sats
from clients.sv1 import Sv1Miner
from regtest import E2E, free_port
from wait import wait_until

COMPOSE_FILE = E2E / "docker-compose.sri.yml"


def build():
    """Builds the SRI images, once at a time; a no-op when they are built already, as CI builds them first."""
    with open(pathlib.Path(tempfile.gettempdir()) / "puntohunter-e2e-sri-build.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        subprocess.run(["docker", "compose", "-f", COMPOSE_FILE, "build", "--quiet"], check=True)


class Role:
    """One SRI role under a project name of its own."""

    def __init__(self, service, **env):
        self.service = service
        self.project = f"puntohunter-sri-{uuid.uuid4().hex[:8]}"
        self.env = {**os.environ, **env}

    def compose(self, *args):
        return subprocess.run(
            ["docker", "compose", "-p", self.project, "-f", COMPOSE_FILE, *args],
            env=self.env,
            check=True,
            capture_output=True,
            text=True,
        )

    def up(self):
        self.compose("up", "--detach", "--no-build", self.service)

    def down(self):
        self.compose("down", "--remove-orphans")

    def logs(self):
        return self.compose("logs", "--no-color", self.service).stdout


@pytest.fixture(scope="session")
def sri_images():
    build()


@pytest.fixture
def roles(sri_images):
    started = []

    def start(service, **env):
        role = Role(service, **env)
        started.append(role)
        role.up()
        return role

    start.logs = lambda: "\n".join(role.logs() for role in started)
    yield start
    for role in started:
        role.down()


def coinbase_payee(chain, block_hash):
    vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
    return vout[0]["scriptPubKey"]["address"], sum(sats(o["value"]) for o in vout)


def accepted_blocks(pool):
    return [h for h, row in pool.blocks().items() if row["result"] == "accepted"]


def test_the_sri_mining_device_mines_on_a_standard_channel(pool_factory, fresh_node, roles):
    pool = pool_factory(node=fresh_node)
    address = fresh_node.miner_address(0)
    device = roles(
        "mining-device",
        POOL_PORT=str(pool.solo_v2_port),
        AUTHORITY_KEY=pool.authority_key(),
        USER_IDENTITY=f"{address}.sri-device",
    )
    wait_until(lambda: "V2 channel opened" in pool.logs(), 60, "the device's channel")
    [opened] = [line for line in pool.logs().splitlines() if "V2 channel opened" in line]
    assert "kind=standard" in opened and "worker=sri-device" in opened
    # Its shares are acknowledged, and about half of all shares on regtest are blocks.
    wait_until(lambda: "Received SubmitSharesSuccess" in device.logs(), 60, "an acknowledgement")
    wait_until(lambda: accepted_blocks(pool), 60, "a block from the device")
    payee, _ = coinbase_payee(fresh_node.chain, accepted_blocks(pool)[0])
    assert payee == address


def test_the_sri_translator_carries_a_v1_miner_on_an_extended_channel(
    pool, regtest_cluster, roles, tmp_path, miner_username
):
    address = regtest_cluster.miner_address(1)
    downstream = free_port()
    config = {
        "downstream_address": "127.0.0.1",
        "downstream_port": downstream,
        "max_supported_version": 2,
        "min_supported_version": 2,
        "downstream_extranonce2_size": 4,
        "verify_payout": False,
        # A channel per V1 miner, as each is its own worker.
        "aggregate_channels": False,
        "supported_extensions": [],
        "downstream_difficulty_config": {
            # A rate below any real miner's; without its own vardiff the translator relays the pool's target.
            "min_individual_miner_hashrate": 0.1,
            "shares_per_minute": 6.0,
            "enable_vardiff": False,
            "job_keepalive_interval_secs": 60,
        },
        "upstreams": [
            {
                "address": "127.0.0.1",
                "port": pool.solo_v2_port,
                "authority_pubkey": pool.authority_key(),
                "user_identity": f"{address}.translated",
            }
        ],
    }
    (tmp_path / "translator.toml").write_text(tomli_w.dumps(config))
    tmp_path.chmod(0o755)
    (tmp_path / "translator.toml").chmod(0o644)
    translator = roles("translator", SRI_CONFIG_DIR=str(tmp_path))

    # From its log, not by probing the port: a probe is a V1 connection too, and the translator takes the
    # first message of the next one as the probe's.
    wait_until(lambda: "Translator Proxy: listening on" in translator.logs(), 60, "the translator's V1 port")
    miner = Sv1Miner(downstream, miner_username)
    # SRI's translator opens the channel under its own configured identity, one worker per V1 miner, and
    # does not pass the V1 username on.
    wait_until(lambda: "V2 channel opened" in pool.logs(), 30, "the translator's channel")
    [opened] = [line for line in pool.logs().splitlines() if "V2 channel opened" in line]
    assert "kind=extended" in opened
    assert f"address={address}" in opened and "worker=translated.miner1" in opened

    # SRI's translator gives the V1 miner regtest's network difficulty whatever the pool's target, so every
    # translated share is a block. Once version rolling is negotiated it also requires version bits on every
    # submit, as ESP-Miner sends them; the harness's V1 client leaves them out when it rolls none.
    assert miner.difficulty == pytest.approx(4.6565e-10, rel=1e-4)
    params, block_hash = miner.generator.block(miner.notify, miner.difficulty, version_bits=0x0000_2000)
    assert miner.submit(params) is True
    wait_until(lambda: "SubmitSharesSuccess" in translator.logs(), 30, "the pool's acknowledgement")
    # The block pays the channel's worker: credit follows the channel.
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 30, "the block recorded")
    payee, _ = coinbase_payee(regtest_cluster.chain, block_hash)
    assert payee == address
    miner.client.close()
