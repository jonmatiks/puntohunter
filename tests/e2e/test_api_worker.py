"""`GET /api/v1/workers/{handle}` and its stream (API reference sections 2.5 and 2.6): every field and its null
cases, neither its address, its name, nor its fleet, and a share reaching the stream within a bounded
interval. A disconnected worker keeps its best share, from the rollups after a restart;
a worker shows its session's start and when it was first seen. Unknown handles answer JSON 404. Every response
passes the privacy scan, which admits no address in it."""

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
# How soon an accepted share must reach the stream.
SSE_BOUND = 2.0
FAST_TIMEOUTS = {"node": {"rpc_timeout_secs": 1.0, "longpoll_timeout_secs": 2.0}}
LIVE = {
    "status", "last_seen_at", "connections", "hash_rate", "difficulty", "best_share", "chain", "last_share_at",
    "accepted", "rejected", "blocks", "connected_since_at", "first_seen_at", "solo", "window",
}
NAME = "porch-rig-7"


def logged_handle(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))[-1]


def worker(pool, handle):
    response = pool.portal.api(f"/api/v1/workers/{handle}")
    assert response.status == 200, response.body
    assert response.header("cache-control") == "no-store"
    return response.json


@pytest.fixture
def pool(pool_factory):
    return pool_factory(
        overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}, "telemetry": {"rollup_secs": PERIOD}}
    )


def test_every_field_of_a_solo_worker_and_its_nulls(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    miner = Sv1Miner(pool.solo_v1_port, f"{address}.{NAME}")
    handle = logged_handle(pool, "V1 authorized")
    [fleet] = wait_until(
        lambda: [r["fleet"] for r in pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle)], 10, "the fleet"
    )
    tip = regtest_cluster.chain.call("getblockcount")
    response = pool.portal.api(f"/api/v1/workers/{handle}")
    body = response.json
    assert response.address is None, "no address, which the scan would refuse anywhere"
    assert set(body) == {
        "handle", "pool", "transports", "miner", "part", "finder_sats", "name_written", "name_asked", "outage",
        "live",
    }
    assert (body["handle"], body["pool"], body["transports"], body["outage"]) == (handle, "solo", ["V1"], False)
    assert body["part"] is None and body["finder_sats"] > 0, "a solo worker asked none, and is paid the miner portion"
    assert (body["name_written"], body["name_asked"]) == (False, False), "the harness writes and asks no names"
    assert NAME not in response.body and fleet not in response.body, "neither its name nor its fleet"
    live = body["live"]
    assert set(live) == LIVE
    assert (live["status"], live["connections"], live["accepted"], live["rejected"], live["blocks"]) == (
        "idle", 1, 0, 0, 0,
    )
    assert live["difficulty"] == pytest.approx(PINNED)
    assert set(live["chain"]) == {"tip_height", "network_difficulty"}, "as the pool resource gives it"
    assert live["chain"]["tip_height"] in (tip, tip + 1) and live["chain"]["network_difficulty"] > 0
    assert live["last_share_at"] is None
    assert isinstance(live["connected_since_at"], int) and isinstance(live["first_seen_at"], int)
    assert set(live["hash_rate"]) == {"1m", "10m", "1h", "24h"}
    for rate in live["hash_rate"].values():
        assert set(rate) == {"hash_rate", "estimating"}
    assert live["window"] is None, "a solo worker has no window"
    assert live["solo"]["odds"] == {"unavailable": "no_hash_rate"}
    assert live["solo"]["finder_payout"] is False
    assert live["solo"]["best_vs_network"] == 0

    assert miner.submit(miner.share()) is True
    live = wait_until(lambda: (l := worker(pool, handle)["live"])["accepted"] == 1 and l, 10, "the share")
    assert live["status"] == "online"
    assert live["last_share_at"] == pytest.approx(time.time(), abs=5)
    assert live["best_share"] > PINNED
    miner.client.close()

    live = wait_until(lambda: (l := worker(pool, handle)["live"])["connections"] == 0 and l, 10, "the disconnect")
    assert live["status"] == "offline"
    assert live["difficulty"] is None and live["connected_since_at"] is None


def test_a_pooled_worker_has_a_window_and_no_odds(pool, regtest_cluster):
    miner = Sv1Miner(pool.pooled_v1_port, f"{regtest_cluster.miner_address(0)}.{NAME}")
    handle = logged_handle(pool, "V1 authorized")
    live = worker(pool, handle)["live"]
    assert live["solo"] is None
    assert live["window"] == {"unavailable": "no_shares"} or live["window"]["pool"] == "pooled"
    assert miner.submit(miner.share()) is True
    window = wait_until(lambda: worker(pool, handle)["live"]["window"].get("fraction"), 15, "a window walked")
    assert 0 < window <= 1
    window = worker(pool, handle)["live"]["window"]
    assert window["pool"] == "pooled" and window["estimate_sats"] > 0
    miner.client.close()


def test_figures_that_need_the_chain_say_so_while_the_node_is_down(pool_factory, fresh_node):
    pool = pool_factory(node=fresh_node, overrides=FAST_TIMEOUTS)
    solo = Sv1Miner(pool.solo_v1_port, f"{fresh_node.miner_address(0)}.solo")
    solo_handle = logged_handle(pool, "V1 authorized")
    pooled = Sv1Miner(pool.pooled_v1_port, f"{fresh_node.miner_address(0)}.pooled")
    pooled_handle = logged_handle(pool, "V1 authorized")
    fresh_node.stop()
    wait_until(lambda: worker(pool, solo_handle)["outage"], 30, "the outage")
    live = worker(pool, solo_handle)["live"]
    assert live["chain"] is None
    assert live["solo"]["odds"] == {"unavailable": "no_template"} and live["solo"]["best_vs_network"] is None
    assert worker(pool, pooled_handle)["live"]["window"] == {"unavailable": "no_template"}
    solo.client.close()
    pooled.client.close()


@pytest.mark.parametrize("transport", ["V1", "V2-native"])
def test_a_share_reaches_the_stream_within_a_bounded_interval(pool, miner_username, transport):
    if transport == "V1":
        miner = Sv1Miner(pool.solo_v1_port, miner_username)
        submit, close = lambda: miner.submit(miner.share()), miner.client.close
        handle = logged_handle(pool, "V1 authorized")
    else:
        miner = Sv2Miner(pool, pool.solo_v2_port, miner_username)
        submit, close = lambda: not miner.submit()["errors"], miner.close
        handle = logged_handle(pool, "V2 channel opened")
    api = pool.portal.events(f"/api/v1/workers/{handle}/events")
    first_api = next(api)
    assert first_api["event"] == "live"
    first = json.loads(first_api["data"])
    assert set(first) == LIVE | {"outage"}, "the live values and the outage, never the address"
    assert (first["accepted"], first["connections"], first["outage"]) == (0, 1, False)

    assert submit()
    accepted = time.monotonic()
    for event in api:
        if json.loads(event["data"])["accepted"] == 1:
            break
    assert time.monotonic() - accepted < SSE_BOUND
    live = json.loads(event["data"])
    assert live["difficulty"] == pytest.approx(PINNED)
    assert live["best_share"] > PINNED
    api.close()
    close()


def test_a_disconnected_worker_keeps_its_best_share(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    handle = logged_handle(pool, "V1 authorized")
    assert miner.submit(miner.share()) is True
    best = wait_until(lambda: worker(pool, handle)["live"]["best_share"], 10, "the best share")
    miner.client.close()
    live = wait_until(lambda: (l := worker(pool, handle)["live"])["connections"] == 0 and l, 10, "the disconnect")
    assert live["best_share"] == best

    # After a restart the live values are gone, and the rollups carry the best share.
    wait_until(lambda: pool.rollups(handle), PERIOD * 3, "the rollup row")
    pool.restart()
    body = worker(pool, handle)
    assert body["live"]["connections"] == 0
    assert body["live"]["best_share"] == pytest.approx(best, rel=1e-12)
    assert body["transports"] == ["V1"], "the last connection's"


def test_a_worker_shows_its_session_and_when_it_was_first_seen(pool, miner_username):
    before = int(time.time())
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    handle = logged_handle(pool, "V1 authorized")
    rows = wait_until(lambda: pool.rows("SELECT first_seen FROM fleets WHERE handle = ?", handle), 10, "the fleet row")
    live = worker(pool, handle)["live"]
    assert before <= live["connected_since_at"] <= time.time()
    assert live["first_seen_at"] == rows[0]["first_seen"]
    miner.client.close()
    live = wait_until(lambda: (l := worker(pool, handle)["live"])["connections"] == 0 and l, 10, "the disconnect")
    assert live["connected_since_at"] is None
    assert live["first_seen_at"] == rows[0]["first_seen"], "kept after the session"


def test_an_unknown_handle_is_not_found(pool):
    for path in ("/api/v1/workers/aaaaaaaaaaaaaaaa", "/api/v1/workers/aaaaaaaaaaaaaaaa/events", "/api/v1/workers/NOPE"):
        response = pool.portal.api(path)
        assert (response.status, response.json["error"]) == (404, "not_found"), path
