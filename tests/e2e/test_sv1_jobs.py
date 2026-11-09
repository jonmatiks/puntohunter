"""V1 jobs: set_difficulty ahead of each notify, clean_jobs on a new
block, difficulty hints, and no work on the wrong chain."""

import tomllib

import pytest

from clients.sv1 import Sv1Client, stratum_prevhash
from regtest import ROOT
from wait import wait_until


def next_notify(client):
    """Reads up to the next mining.notify, asserting a set_difficulty came just before it. Returns both."""
    previous = None
    while True:
        message = client.next_message()
        if message.get("method") == "mining.notify":
            assert previous is not None and previous.get("method") == "mining.set_difficulty"
            return previous["params"], message["params"]
        previous = message


@pytest.fixture
def miner(pool):
    client = Sv1Client(pool.solo_v1_port)
    client.subscribe()
    yield client
    client.close()


def test_set_difficulty_precedes_every_notify_and_new_blocks_clean_jobs(
    miner, miner_username, regtest_cluster
):
    miner.authorize(miner_username)
    _, first = next_notify(miner)
    assert first[8] is True, "a session's first job starts clean"

    for _ in range(2):
        [block] = regtest_cluster.chain.mine(1, regtest_cluster.address("pool_wallet"))
        _, notify = next_notify(miner)
        # A mempool refresh may come first; the job on the new block is clean.
        while notify[1] != stratum_prevhash(block):
            assert notify[8] is False
            _, notify = next_notify(miner)
        assert notify[8] is True


def test_starts_at_the_floor_without_a_hint(pool, miner, miner_username):
    miner.authorize(miner_username)
    [difficulty], _ = next_notify(miner)
    assert difficulty == pool.config["vardiff"]["floor"]


def test_a_password_hint_sets_the_starting_difficulty(miner, miner_username):
    miner.authorize(miner_username, "x,d=512")
    [difficulty], _ = next_notify(miner)
    assert difficulty == 512


def test_suggest_difficulty_sets_the_starting_difficulty(miner, miner_username):
    assert miner.call("mining.suggest_difficulty", 1024) is True
    miner.authorize(miner_username)
    [difficulty], _ = next_notify(miner)
    assert difficulty == 1024


def test_a_hint_after_work_started_applies_to_the_next_job(miner, miner_username):
    miner.authorize(miner_username)
    next_notify(miner)
    miner.call("mining.suggest_difficulty", 2048)
    [difficulty], _ = next_notify(miner)
    assert difficulty == 2048


def test_an_out_of_range_hint_is_clamped(pool_factory, miner_username):
    pool = pool_factory(overrides={"vardiff": {"ceiling": 4096.0}})
    client = Sv1Client(pool.solo_v1_port)
    client.subscribe()
    client.authorize(miner_username, "d=1e9")
    [difficulty], _ = next_notify(client)
    assert difficulty == 4096
    client.close()


def test_wrong_chain_sends_no_work_and_keeps_the_miner(pool_factory):
    """A mainnet pool pointed at the regtest node refuses to issue work."""
    pool = pool_factory(base="mainnet")
    mainnet = tomllib.loads((ROOT / "config" / "mainnet.toml").read_text())
    client = Sv1Client(pool.solo_v1_port)
    client.subscribe()
    assert client.authorize(f"{mainnet['addresses']['pool_wallet']}.e2e") is True

    # The chain check fails before any template is published, and its state is terminal.
    wait_until(lambda: "node is on chain regtest" in pool.logs(), 10, "the wrong-chain log line")
    assert client.alive()
    assert not any(m.get("method") == "mining.notify" for m in client.notifications)
    client.close()
