"""Hybrid in the API (API reference sections 2.1 to 2.8): the pool resource counts
hybrid miners, and gives a fixed finder amount in satoshis; its settings and its share of the reward are checked
in test_api_pool.py. A hybrid worker is found by lookup, listed on its fleet with a hybrid section and series,
and shown on its own with the odds of a finder's payout. A hybrid block is listed by pool with what it paid its
finder and the pool wallet. Every response passes the privacy scan."""

import re

import pytest

from clients.sv1 import Sv1Miner
from wait import wait_until

PERIOD = 2


@pytest.fixture
def pool(pool_factory):
    return pool_factory(overrides={"telemetry": {"rollup_secs": PERIOD}})


def logged_handle(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))[-1]


def mined(pool, username, shares=3):
    """A hybrid V1 miner on `username` that has had `shares` accepted, once their rollup rows are written."""
    miner = Sv1Miner(pool.hybrid_v1_port, username)
    handle = logged_handle(pool, "V1 authorized")
    for _ in range(shares):
        assert miner.submit(miner.share()) is True
    wait_until(lambda: sum(r["accepted"] for r in pool.rollups(handle)) == shares, PERIOD * 3, "the rollup rows")
    return miner, handle


def hybrid(pool):
    """The hybrid pool's entry in the pool resource."""
    [entry] = [setup for setup in pool.portal.api("/api/v1/pool").json["pools"] if setup["id"] == "hybrid"]
    return entry


def test_the_pool_resource_counts_a_hybrid_miner(pool, regtest_cluster):
    assert hybrid(pool)["devices"] == 0
    miner = Sv1Miner(pool.hybrid_v1_port, f"{regtest_cluster.miner_address(0)}.hybrid")
    wait_until(lambda: hybrid(pool)["devices"] == 1, 10, "one hybrid miner connected")
    miner.client.close()


def test_a_fixed_finder_amount_is_given_in_sats(pool_factory):
    fixed = pool_factory(overrides={"pools": {"hybrid": {"finder": {"btc": 0.5}}}})
    assert hybrid(fixed)["finder"] == {"sats": 50_000_000}


def test_a_hybrid_worker_is_found_and_shown_on_its_fleet_and_its_own(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    username = f"{address}.hybrid-rig"
    miner, handle = mined(pool, username)
    [fleet] = {r["fleet"] for r in pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle)}

    found = pool.portal.api("/api/v1/lookup", method="POST", body={"username": username})
    assert (found.status, found.json) == (200, {"fleet": fleet})

    response = pool.portal.api(f"/api/v1/fleets/{fleet}")
    assert response.address == address
    resource = response.json
    assert [(d["handle"], d["name"], d["pool"]) for d in resource["devices"]] == [(handle, "hybrid-rig", "hybrid")]
    [section] = resource["pools"]
    assert (section["pool"], section["name"], section["solo"]["finder_payout"]) == ("hybrid", "Hybrid", True)
    assert section["window"]["pool"] == "hybrid" or section["window"] == {"unavailable": "no_shares"}
    series = pool.portal.api(f"/api/v1/fleets/{fleet}/series").json
    [line] = series["pools"]
    assert (line["pool"], line["name"], sum(line["accepted"])) == ("hybrid", "Hybrid", 3)

    worker = pool.portal.api(f"/api/v1/workers/{handle}").json
    assert worker["pool"] == "hybrid"
    solo = worker["live"]["solo"]
    assert solo["finder_payout"] is True, "the odds are those of a finder's payout"
    assert 0 < solo["odds"]["day"] <= 1, solo["odds"]
    assert solo["best_vs_network"] > 0
    miner.client.close()


def test_a_hybrid_block_is_listed_with_what_it_paid(pool, regtest_cluster, solution_forcer):
    miner = Sv1Miner(pool.hybrid_v1_port, f"{regtest_cluster.miner_address(0)}.hybrid")
    handle = logged_handle(pool, "V1 authorized")
    _, block_hash = solution_forcer(miner)
    miner.client.close()
    wait_until(lambda: pool.block(block_hash).get("result"), 10, "the block answered")
    row = pool.block(block_hash)

    for path in ("/api/v1/blocks", f"/api/v1/workers/{handle}/blocks"):
        [block] = [b for b in pool.portal.api(path).json["blocks"] if b["hash"] == block_hash]
        assert (block["pool"], block["transport"], block["finder"]) == ("hybrid", "V1", handle), path
        assert block["finder_sats"] == row["finder_sats"] > 0, path
        assert block["pool_sats"] == row["pool_sats"] > 0, path
