"""Private fleets: a device that sets a fleet password, in its V1 password or
after a `#` in its username over V1 or V2, joins the private fleet of its address and password, which the lookup
opens only with that password; a device without one stays in its address's open fleet, which neither lists nor
counts the private devices and says it is open. A device with another password lands in a fleet of
its own, and one without the secret does not merge into a protected worker. `d=512` beside a password is a hint,
`x` and a 7-character password set none, and a 7-character `#` password, a V1 device setting both forms, and a `+`
inside a `#` password are refused with a reason. The stats lists show a private fleet without its address. No
response, stream, or line of the engine's log carries a password: the privacy scan holds them as it holds
the addresses."""

import pytest

from clients.sv1 import Sv1Client, Sv1Miner
from clients.sv2 import Sv2Client, Sv2Miner
from wait import wait_until

PERIOD = 2
SECRET = "garage-pass-1"
OTHER = "porch-pass-22"


@pytest.fixture
def pool(pool_factory):
    pool = pool_factory(overrides={"telemetry": {"rollup_secs": PERIOD}})
    pool.portal.secrets += [SECRET, OTHER]
    return pool


def lookup(pool, username, password=None):
    """The JSON lookup's answer: the fleet handle, or None when no worker is found."""
    body = {"username": username} | ({} if password is None else {"password": password})
    response = pool.portal.api("/api/v1/lookup", method="POST", body=body)
    if response.status == 404:
        return None
    assert response.status == 200, response.body
    return response.json["fleet"]


def fleet(pool, handle):
    response = pool.portal.api(f"/api/v1/fleets/{handle}")
    assert response.status == 200, response.body
    return response.json


def names(body):
    return sorted(device["name"] for device in body["devices"])


def test_an_open_and_a_private_fleet_side_by_side_over_v1_and_v2(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    open_rig = Sv1Miner(pool.solo_v1_port, f"{address}.open")
    v1 = Sv1Miner(pool.solo_v1_port, f"{address}.rig", password=SECRET)
    v2 = Sv2Miner(pool, pool.solo_v2_port, f"{address}.rig2#{SECRET}")
    assert open_rig.submit(open_rig.share()) is True
    assert v1.submit(v1.share()) is True
    assert not v2.submit()["errors"]

    opened = lookup(pool, f"{address}.open")
    private = lookup(pool, f"{address}.rig", SECRET)
    assert opened and private and opened != private
    assert lookup(pool, f"{address}.rig2#{SECRET}") == private, "after a # in the username"
    assert lookup(pool, f"{address}.rig2#{SECRET}+50+name") == private, "pasted with its options"
    assert lookup(pool, f"{address}.rig") is None, "no open worker by that name"
    assert lookup(pool, f"{address}.rig", OTHER) is None, "a wrong password is an unknown username"
    assert lookup(pool, f"{address}.open", SECRET) is None

    public = wait_until(lambda: (b := fleet(pool, opened))["pools"][0]["accepted"] == 1 and b, 10, "one share")
    assert (public["private"], names(public)) == (False, ["open"]), "the private devices are not listed"
    hidden = wait_until(lambda: (b := fleet(pool, private))["pools"][0]["accepted"] == 2 and b, 10, "two shares")
    assert (hidden["private"], names(hidden), hidden["address"]) == (True, ["rig", "rig2"], address)
    events = pool.portal.events(f"/api/v1/fleets/{private}/events")
    next(events)
    events.close()

    # Another password on the same address and name lands in a fleet of its own; no password, in the open one.
    stranger = Sv1Miner(pool.solo_v1_port, f"{address}.rig", password=OTHER)
    plain = Sv1Miner(pool.solo_v1_port, f"{address}.rig")
    third = lookup(pool, f"{address}.rig", OTHER)
    assert third not in (None, opened, private)
    assert lookup(pool, f"{address}.rig") == opened
    # Each connection's worker is recorded in its fleet just after it is authorized.
    wait_until(lambda: names(fleet(pool, opened)) == ["open", "rig"], 10, "the plain worker in the open fleet")
    assert names(fleet(pool, private)) == ["rig", "rig2"], "neither merged into the protected worker"

    stats = wait_until(lambda: (s := pool.portal.api("/api/v1/stats").json)["hash_rates"] and s, 10, "the lists")
    rows = stats["hash_rates"] + stats["best_shares"]["round"]
    assert any(row["short_address"] is None for row in rows), "a private fleet's row"
    assert all(row["short_address"] in (None, f"{address[:6]}…{address[-4:]}") for row in rows)

    for miner in (open_rig, v1, stranger, plain):
        miner.client.close()
    v2.close()
    logs = pool.logs()
    assert SECRET not in logs and OTHER not in logs, "no log line carries a password"


def test_a_password_without_its_hints_and_the_forms_refused(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    hinted = Sv1Miner(pool.solo_v1_port, f"{address}.hinted", password=f"{SECRET},d=512")
    seven = Sv1Miner(pool.solo_v1_port, f"{address}.seven", password="1234567")
    x = Sv1Miner(pool.solo_v1_port, f"{address}.x", password="x,d=512")
    assert lookup(pool, f"{address}.hinted", SECRET) is not None, "d=512 is a hint, the rest the password"
    opened = lookup(pool, f"{address}.seven")
    assert opened is not None and lookup(pool, f"{address}.x") == opened, "7 characters and x set none"
    assert lookup(pool, f"{address}.seven", "1234567") == opened, "nor in the lookup"

    refused = [
        (f"{address}.short#1234567", "x", "at least 8 characters"),
        (f"{address}.both#{SECRET}", OTHER, "both after '#' and as the password"),
        (f"{address}.plus#garage+pass", "x", 'option "pass"'),
    ]
    for username, password, reason in refused:
        client = Sv1Client(pool.solo_v1_port)
        client.subscribe()
        response = client.request("mining.authorize", username, password)
        assert response["error"][0] == 24 and reason in response["error"][1], (username, response)
        client.close()
    for username, _, _ in (refused[0], refused[2]):
        v2 = Sv2Client()
        v2.connect(pool.solo_v2_port, pin=pool.authority_key())
        v2.setup()
        assert v2.open_extended(username) == {"event": "channel_error", "error_code": "unknown-user"}
        v2.close()

    for miner in (hinted, seven, x):
        miner.client.close()
    logs = pool.logs()
    assert SECRET not in logs and OTHER not in logs and "1234567" not in logs
