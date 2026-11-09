"""`POST /api/v1/lookup` (API reference section 2.2): the lookup cases over JSON. A
full username, an upper-case bech32 address included, finds its fleet; an address alone, an unknown worker, a
username that does not parse, and a malformed body are refused, each with its code. A client's lookups past
its budget are refused, whatever they send. None logs or echoes the username. Every response passes the
privacy scan."""

import re

import pytest

from clients.sv1 import Sv1Miner
from wait import wait_until

LIMIT = 4


def fleet_of(pool, handle):
    rows = wait_until(lambda: pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle), 10, "the fleet row")
    return rows[0]["fleet"]


def look_up(pool, body):
    return pool.portal.api("/api/v1/lookup", method="POST", body=body)


@pytest.fixture
def solo(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [handle] = re.findall(r"V1 authorized.*handle=([a-z2-7]{16})", pool.logs())
    yield handle
    miner.client.close()


def test_a_full_username_finds_its_fleet(pool, solo, miner_username):
    fleet = fleet_of(pool, solo)
    found = look_up(pool, {"username": miner_username})
    assert found.status == 200 and found.json == {"fleet": fleet}
    assert found.header("cache-control") == "no-store"
    address, worker = miner_username.split(".", 1)
    typed = look_up(pool, {"username": f" {address.upper()}.{worker} "})
    assert typed.json == {"fleet": fleet}, "an upper-case bech32 address is the same address"


def test_each_refusal_has_its_code_and_echoes_nothing(pool, solo, miner_username):
    fleet = fleet_of(pool, solo)
    address = miner_username.split(".")[0]
    for body, status, code in (
        ({"username": address}, 400, "worker_name_required"),
        ({"username": f"{address}.nosuchworker"}, 404, "not_found"),
        ({"username": "not a username"}, 400, "invalid_username"),
        ({"name": miner_username}, 400, "bad_request"),
        ("{not json", 400, "bad_request"),
        ([miner_username], 400, "bad_request"),
    ):
        refused = look_up(pool, body)
        assert (refused.status, refused.json["error"]) == (status, code), body
        assert set(refused.json) == {"error", "message"}, body
        for secret in (fleet, solo, "nosuchworker", "not a username"):
            assert secret not in refused.body, (body, secret)
    form = pool.portal.request(
        "POST", "/api/v1/lookup", f"username={miner_username}", {"Content-Type": "application/x-www-form-urlencoded"}
    )
    assert (form.status, form.json["error"]) == (400, "bad_request"), "JSON only"
    assert "nosuchworker" not in pool.logs(), "the request body is never logged"


def test_lookups_past_the_budget_are_refused(pool_factory, miner_username):
    pool = pool_factory(overrides={"portal": {"lookups_per_minute": LIMIT}})
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [handle] = re.findall(r"V1 authorized.*handle=([a-z2-7]{16})", pool.logs())
    fleet_of(pool, handle)
    for _ in range(LIMIT):
        assert look_up(pool, {"username": miner_username}).status == 200
    refused = look_up(pool, {"username": miner_username})
    assert (refused.status, refused.json["error"]) == (429, "rate_limited")
    assert 1 <= refused.json["retry_after_secs"] == int(refused.header("retry-after")) <= 60
    assert look_up(pool, "{not json").status == 429, "the limit holds whatever is sent"
    miner.client.close()
