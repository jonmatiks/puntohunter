"""More of a worker's resources (API reference sections 2.5 to 2.8), beside test_api_worker.py's: a V2 worker is tagged with its transport; a disconnected worker gives when it
was last seen; its series matches the rollup rows; and neither its resource, its stream, its series, its blocks,
nor the pool's lists carry its fleet handle or its worker name, which only its fleet's resource gives. Every
response passes the privacy scan, which admits no address in them."""

import json
import re
import time

import pytest

from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until

# Between the harness floor and regtest's network difficulty, so shares need not be blocks.
PINNED = 3e-10
PERIOD = 2


def logged_handle(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))[-1]


def worker(pool, handle):
    response = pool.portal.api(f"/api/v1/workers/{handle}")
    assert response.status == 200, response.body
    return response.json


@pytest.fixture
def pool(pool_factory):
    return pool_factory(
        overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}, "telemetry": {"rollup_secs": PERIOD}}
    )


def test_a_v2_worker_is_tagged_with_its_transport(pool, miner_username):
    """A V1 worker's tags are checked in test_api_worker.py."""
    miner = Sv2Miner(pool, pool.solo_v2_port, miner_username)
    handle = logged_handle(pool, "V2 channel opened")
    response = pool.portal.api(f"/api/v1/workers/{handle}")
    assert response.address is None, "no address, which the scan would refuse anywhere"
    body = response.json
    assert (body["pool"], body["transports"], body["live"]["accepted"]) == ("solo", ["V2-native"], 0)
    miner.close()


def test_a_disconnected_worker_gives_when_it_was_last_seen(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    handle = logged_handle(pool, "V1 authorized")
    miner.client.close()
    live = wait_until(lambda: (l := worker(pool, handle)["live"])["status"] == "offline" and l, 10, "the disconnect")
    assert live["connections"] == 0
    assert live["last_seen_at"] == pytest.approx(time.time(), abs=10)


def test_the_json_series_matches_the_rollup_rows(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    handle = logged_handle(pool, "V1 authorized")
    # Two periods' shares, each batch sent early in its period and the next held past its middle.
    for count in (3, 1):
        wait_until(lambda: time.time() % PERIOD < PERIOD / 4, PERIOD * 2, "a period's start")
        for _ in range(count):
            assert miner.submit(miner.share()) is True
        wait_until(lambda: time.time() % PERIOD >= PERIOD / 2, PERIOD, "past the period's middle")
    rows = wait_until(
        lambda: sum(r["accepted"] for r in pool.rollups(handle)) == 4 and pool.rollups(handle), PERIOD * 3, "rows"
    )

    response = pool.portal.api(f"/api/v1/workers/{handle}/series")
    assert response.status == 200 and response.header("content-type") == "application/json"
    series = response.json
    assert series["period"] == PERIOD
    assert series["start"] == [r["start"] for r in rows]
    assert series["accepted"] == [r["accepted"] for r in rows]
    assert series["hash_rate"] == pytest.approx([r["work"] / PERIOD for r in rows], rel=1e-12)
    assert series["best"] == pytest.approx([r["best"] for r in rows], rel=1e-12)
    assert series["rejected"] == [0] * len(rows)
    miner.client.close()


def test_a_workers_resources_carry_neither_its_fleet_nor_its_name(pool, regtest_cluster):
    name = "porch-rig-7"
    miner = Sv1Miner(pool.solo_v1_port, f"{regtest_cluster.miner_address(0)}.{name}")
    handle = logged_handle(pool, "V1 authorized")
    [fleet] = wait_until(
        lambda: [r["fleet"] for r in pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle)], 10, "the fleet"
    )
    assert miner.submit(miner.share()) is True
    assert [d["name"] for d in pool.portal.api(f"/api/v1/fleets/{fleet}").json["devices"]] == [name]
    for path in (
        f"/api/v1/workers/{handle}",
        f"/api/v1/workers/{handle}/series",
        f"/api/v1/workers/{handle}/blocks",
        "/api/v1/blocks",
        "/api/v1/pool",
    ):
        body = pool.portal.api(path).body
        assert fleet not in body, path
        assert name not in body, path
    events = pool.portal.events(f"/api/v1/workers/{handle}/events")
    data = next(events)["data"]
    events.close()
    assert set(json.loads(data)) >= {"status", "accepted"}
    assert fleet not in data and name not in data
    miner.client.close()
