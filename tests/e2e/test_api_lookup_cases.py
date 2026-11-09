"""More lookup cases (API reference section 2.2), beside test_api_lookup.py's: a username
mining in two modes finds the one fleet that lists it in both; the username in a query string reaches no
handle; a restart still finds a worker from its rollups; and lookups refused past the limit are allowed again
after the minute. Every response passes the privacy scan."""

import re
from urllib.parse import quote

import pytest

from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until

# A rollup period short enough that a restart finds the worker in the rollups within seconds.
PERIOD = 2
LIMIT = 3


def logged_handles(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))


def fleet_of(pool, handle):
    """The fleet handle the pool recorded for a worker."""
    rows = wait_until(lambda: pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle), 10, "the fleet row")
    return rows[0]["fleet"]


def look_up(pool, username):
    return pool.portal.api("/api/v1/lookup", method="POST", body={"username": username})


def reaches_no_handle(response, *handles):
    assert response.status not in (200, 301, 302, 303, 307, 308), response.status
    assert not response.header("location")
    for handle in handles:
        assert handle not in response.body


@pytest.fixture
def pool(pool_factory):
    return pool_factory(overrides={"telemetry": {"rollup_secs": PERIOD}})


@pytest.fixture
def solo(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [handle] = logged_handles(pool, "V1 authorized")
    yield handle
    miner.client.close()


def test_a_worker_in_both_modes_finds_one_fleet_listing_both(pool, solo, miner_username):
    pooled = Sv2Miner(pool, pool.pooled_v2_port, miner_username)
    [pooled_handle] = logged_handles(pool, "V2 channel opened")
    fleet = fleet_of(pool, solo)
    assert fleet_of(pool, pooled_handle) == fleet
    assert look_up(pool, miner_username).json == {"fleet": fleet}
    resource = pool.portal.api(f"/api/v1/fleets/{fleet}").json
    assert {d["handle"]: d["pool"] for d in resource["devices"]} == {solo: "solo", pooled_handle: "pooled"}
    assert [section["pool"] for section in resource["pools"]] == ["solo", "pooled"]
    pooled.close()


def test_the_username_in_a_query_string_reaches_no_handle(pool, solo, miner_username):
    fleet = fleet_of(pool, solo)
    query = quote(miner_username)
    no_body = pool.portal.request("POST", f"/api/v1/lookup?username={query}")
    assert (no_body.status, no_body.json["error"]) == (400, "bad_request")
    reaches_no_handle(no_body, solo, fleet)
    reaches_no_handle(pool.portal.request("GET", f"/api/v1/lookup?username={query}"), solo, fleet)


def test_a_restart_still_finds_a_worker_from_its_rollups(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [handle] = logged_handles(pool, "V1 authorized")
    fleet = fleet_of(pool, handle)
    assert miner.submit(miner.share()) is True
    miner.client.close()
    wait_until(lambda: pool.rollups(handle), PERIOD * 3, "the rollup row")
    pool.restart()
    found = look_up(pool, miner_username)
    assert (found.status, found.json) == (200, {"fleet": fleet})


def test_lookups_beyond_the_limit_are_refused_until_the_minute_passes(pool_factory, miner_username):
    """The refusal itself is checked in test_api_lookup.py."""
    pool = pool_factory(overrides={"portal": {"lookups_per_minute": LIMIT}})
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [handle] = logged_handles(pool, "V1 authorized")
    fleet_of(pool, handle)
    lookup = lambda: look_up(pool, miner_username)

    for _ in range(LIMIT):
        assert lookup().status == 200
    refused = lookup()
    assert (refused.status, refused.json["error"]) == (429, "rate_limited")
    reaches_no_handle(refused, handle)

    allowed = wait_until(lambda: lookup().status == 200, 65, "a lookup allowed again")
    assert allowed
    miner.client.close()
