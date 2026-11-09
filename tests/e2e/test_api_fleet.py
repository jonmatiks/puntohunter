"""`GET /api/v1/fleets/{fleet}` and its stream (API reference sections 2.3 and 2.4): a fleet across pools and
transports with its own address, its bare-address device named "", its pools limited to those its devices mine,
and its devices ordered by hashrate, then by name; a device's status from idle to online to offline, its
last-seen time surviving a restart; a row two connections share counting both, its shares counted after one
disconnects and after a restart; each device's miner type without a version; and a share reaching the stream
within a bounded interval, no event carrying the address. Unknown fleets answer JSON 404.
Every response passes the privacy scan, which admits the resource's top-level address and nothing else."""

import json
import re
import time

import pytest

from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until

PERIOD = 2
# How soon an accepted share must reach the stream, as on the worker's.
SSE_BOUND = 2.0
POOL = {"pool", "name", "online", "total", "hash_rate", "best_share", "accepted", "rejected", "last_share_at",
        "blocks", "solo", "window"}
DEVICE = {"handle", "name", "miner", "pool", "transports", "status", "last_seen_at", "connections", "hash_rate",
          "best_share", "last_share_at", "accepted", "window"}


def logged_handles(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))


def fleet_of(pool, handle):
    rows = wait_until(lambda: pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle), 10, "the fleet row")
    return rows[0]["fleet"]


def resource(pool, fleet):
    response = pool.portal.api(f"/api/v1/fleets/{fleet}")
    assert response.status == 200, response.body
    assert response.header("cache-control") == "no-store"
    return response


def devices(body):
    return {device["handle"]: device for device in body["devices"]}


@pytest.fixture
def pool(pool_factory):
    return pool_factory(overrides={"telemetry": {"rollup_secs": PERIOD}})


def test_a_fleet_across_pools_and_transports(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    v1 = [Sv1Miner(pool.solo_v1_port, f"{address}.kitchen"), Sv1Miner(pool.pooled_v1_port, address)]
    v2 = [Sv2Miner(pool, pool.pooled_v2_port, f"{address}.garage_2")]
    stranger = Sv1Miner(pool.solo_v1_port, f"{regtest_cluster.miner_address(1)}.kitchen")
    kitchen, bare, _ = logged_handles(pool, "V1 authorized")
    [garage] = logged_handles(pool, "V2 channel opened")
    fleet = fleet_of(pool, kitchen)
    wait_until(lambda: len(pool.rows("SELECT * FROM fleets WHERE fleet = ?", fleet)) == 3, 10, "three rows")

    tip = regtest_cluster.chain.call("getblockcount")
    response = resource(pool, fleet)
    body = response.json
    assert response.address == address, "its own address, admitted by the scan"
    assert set(body) == {
        "fleet", "address", "private", "ranks", "outage", "chain", "pools", "devices", "keep_offline_days",
    }
    assert body["private"] is False, "an open fleet"
    assert (body["fleet"], body["outage"], body["keep_offline_days"]) == (fleet, False, 7)
    assert set(body["chain"]) == {"tip_height", "network_difficulty"}, "as the pool resource gives it"
    assert body["chain"]["tip_height"] in (tip, tip + 1) and body["chain"]["network_difficulty"] > 0
    assert [(p["pool"], p["name"]) for p in body["pools"]] == [("solo", "Solo"), ("pooled", "Pooled")], (
        "only the pools its devices mine"
    )
    solo, pooled = body["pools"]
    for section in (solo, pooled):
        assert set(section) == POOL
        assert set(section["hash_rate"]) == {"1m", "10m", "1h", "24h"}
    assert (solo["online"], solo["total"], pooled["online"], pooled["total"]) == (0, 1, 0, 2)
    assert solo["window"] is None and solo["solo"]["finder_payout"] is False
    assert pooled["solo"] is None and pooled["window"] is not None

    rows = devices(body)
    assert set(rows) == {kitchen, bare, garage}, "a stranger on another address is not listed"
    for row in rows.values():
        assert set(row) == DEVICE and row["status"] == "idle", "connected, with no share yet"
    assert (rows[kitchen]["name"], rows[kitchen]["pool"], rows[kitchen]["transports"]) == ("kitchen", "solo", ["V1"])
    assert (rows[garage]["name"], rows[garage]["pool"], rows[garage]["transports"]) == (
        "garage_2", "pooled", ["V2-native"],
    )
    assert (rows[bare]["name"], rows[bare]["pool"]) == ("", "pooled")
    assert rows[kitchen]["window"] is None and rows[bare]["window"] is not None
    # No hashrate yet, so by name.
    assert [d["name"] for d in body["devices"]] == ["", "garage_2", "kitchen"]

    for miner in v1 + [stranger]:
        miner.client.close()
    for miner in v2:
        miner.close()


def test_devices_are_ordered_by_hash_rate_then_by_name(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    quiet = Sv1Miner(pool.solo_v1_port, f"{address}.a")
    busy = Sv1Miner(pool.solo_v1_port, f"{address}.b")
    quiet_handle, busy_handle = logged_handles(pool, "V1 authorized")
    fleet = fleet_of(pool, quiet_handle)
    for _ in range(3):
        assert busy.submit(busy.share()) is True
    body = wait_until(
        lambda: (b := resource(pool, fleet).json)["devices"][0]["accepted"] == 3 and b, 10, "the busy device first"
    )
    assert [d["handle"] for d in body["devices"]] == [busy_handle, quiet_handle]
    assert body["devices"][0]["hash_rate"]["hash_rate"] > 0
    quiet.client.close()
    busy.client.close()


@pytest.mark.parametrize("transport", ["V1", "V2-native"])
def test_a_share_reaches_the_stream_within_a_bounded_interval(pool, miner_username, transport):
    if transport == "V1":
        miner = Sv1Miner(pool.solo_v1_port, miner_username)
        submit, close = (lambda: miner.submit(miner.share())), miner.client.close
        [handle] = logged_handles(pool, "V1 authorized")
    else:
        miner = Sv2Miner(pool, pool.solo_v2_port, miner_username)
        submit, close = (lambda: not miner.submit()["errors"]), miner.close
        [handle] = logged_handles(pool, "V2 channel opened")
    fleet = fleet_of(pool, handle)

    api = pool.portal.events(f"/api/v1/fleets/{fleet}/events")
    first_api = next(api)
    assert first_api["event"] == "fleet"
    first = json.loads(first_api["data"])
    assert set(first) == {"outage", "chain", "pools", "devices", "keep_offline_days"}, "never the handle or the address"
    assert devices(first)[handle]["accepted"] == 0
    assert first["pools"][0]["hash_rate"]["1m"]["hash_rate"] == 0

    assert submit()
    accepted = time.monotonic()
    for event in api:
        live = json.loads(event["data"])
        if devices(live)[handle]["accepted"] == 1:
            break
    assert time.monotonic() - accepted < SSE_BOUND
    assert devices(live)[handle]["status"] == "online"
    assert live["pools"][0]["hash_rate"]["1m"]["hash_rate"] > 0, "the summary's minute holds the share"
    api.close()
    close()


def test_a_device_goes_idle_online_and_offline_and_keeps_its_last_seen_time(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [handle] = logged_handles(pool, "V1 authorized")
    fleet = fleet_of(pool, handle)
    status = lambda: pool.portal.api(f"/api/v1/workers/{handle}").json["live"]["status"]
    row = lambda: devices(resource(pool, fleet).json)[handle]
    assert status() == row()["status"] == "idle"
    assert miner.submit(miner.share()) is True
    assert status() == "online"

    miner.client.close()
    wait_until(lambda: status() == "offline", 10, "offline once disconnected")
    wait_until(lambda: row()["status"] == "offline", 10, "offline on the fleet")
    last_seen = row()["last_seen_at"]
    assert last_seen == pytest.approx(time.time(), abs=10)

    # After a restart, the rollups carry the last-seen time.
    wait_until(lambda: pool.rollups(handle), PERIOD * 3, "the rollup row")
    pool.restart()
    assert status() == "offline"
    assert row()["status"] == "offline"
    assert row()["last_seen_at"] == pytest.approx(last_seen, abs=PERIOD)


def test_a_shared_username_says_so_and_its_shares_count_after_a_disconnect_and_a_restart(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    first, second = Sv1Miner(pool.solo_v1_port, address), Sv1Miner(pool.solo_v1_port, address)
    handle, again = logged_handles(pool, "V1 authorized")
    assert handle == again, "one username, one worker"
    fleet = fleet_of(pool, handle)
    for miner in (first, second):
        for _ in range(2):
            assert miner.submit(miner.share()) is True
    row = wait_until(lambda: (r := devices(resource(pool, fleet).json)[handle])["accepted"] == 4 and r, 10, "4")
    assert row["connections"] == 2, "the row counts both"

    second.client.close()
    wait_until(lambda: devices(resource(pool, fleet).json)[handle]["connections"] == 1, 10, "one connection left")
    body = resource(pool, fleet).json
    assert (body["pools"][0]["accepted"], devices(body)[handle]["accepted"]) == (4, 4), "the closed one's shares stay"

    assert first.submit(first.share()) is True
    first.client.close()
    wait_until(
        lambda: pool.rows("SELECT accepted FROM totals WHERE handle = ?", handle) == [{"accepted": 5}],
        PERIOD * 3, "the totals written",
    )
    pool.restart()
    body = resource(pool, fleet).json
    assert (body["pools"][0]["accepted"], devices(body)[handle]["accepted"]) == (5, 5), "after a restart"
    worker = pool.portal.api(f"/api/v1/workers/{handle}").json["live"]
    assert (worker["accepted"], worker["rejected"]) == (5, 0), "the worker resource counts the same"


def test_each_device_names_its_miner_type_without_a_version(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    axe = Sv1Miner(pool.solo_v1_port, f"{address}.axe", agent="bitaxe/BM1370/v2.15.3")
    axe_handle = logged_handles(pool, "V1 authorized")[-1]
    punto = Sv2Miner(pool, pool.pooled_v2_port, f"{address}.punto", vendor="PuntoMiner/0.4.2")
    punto_handle = logged_handles(pool, "V2 channel opened")[-1]
    plain = Sv1Miner(pool.solo_v1_port, f"{address}.plain", agent="")
    plain_handle = logged_handles(pool, "V1 authorized")[-1]
    fleet = fleet_of(pool, axe_handle)
    wait_until(lambda: len(devices(resource(pool, fleet).json)) == 3, 10, "three devices")
    response = resource(pool, fleet)
    rows = devices(response.json)
    assert (rows[axe_handle]["miner"], rows[punto_handle]["miner"], rows[plain_handle]["miner"]) == (
        "bitaxe", "PuntoMiner", None,
    )
    assert pool.portal.api(f"/api/v1/workers/{axe_handle}").json["miner"] == "bitaxe"
    for version in ("2.15.3", "BM1370", "0.4.2"):
        assert version not in response.body
    axe.client.close()
    punto.close()
    plain.client.close()


def test_an_unknown_fleet_is_not_found(pool):
    for path in ("/api/v1/fleets/aaaaaaaaaaaaaaaa", "/api/v1/fleets/aaaaaaaaaaaaaaaa/events", "/api/v1/fleets/NOPE"):
        response = pool.portal.api(path)
        assert (response.status, response.json["error"]) == (404, "not_found"), path
