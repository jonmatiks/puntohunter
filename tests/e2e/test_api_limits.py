"""The binary's limits (API reference section 1.8): open event streams capped
per client address and in all, a worker's and a fleet's streams together, a closed stream freeing
its place, each refusal with its code and `Retry-After`; and the client address the lookup limit and the caps
key on, which is `X-Forwarded-For` only when the peer is `trusted_proxy`. Every response passes the privacy
scan."""

import re

import pytest

from clients.sv1 import Sv1Miner
from wait import wait_until


@pytest.fixture
def paths(request):
    """A worker's and its fleet's streams, of a pool configured by the test's `portal` marker."""
    pool = request.getfixturevalue("pool_factory")(overrides={"portal": request.param})
    cluster = request.getfixturevalue("regtest_cluster")
    miner = Sv1Miner(pool.solo_v1_port, f"{cluster.miner_address(0)}.limits")
    [handle] = re.findall(r"V1 authorized.*handle=([a-z2-7]{16})", pool.logs())
    [fleet] = wait_until(
        lambda: [r["fleet"] for r in pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle)], 10, "the fleet"
    )
    yield pool, [f"/api/v1/workers/{handle}/events", f"/api/v1/fleets/{fleet}/events"]
    miner.client.close()


def opened(pool, path, headers=None):
    """A stream of `path`, open with its first event read."""
    events = pool.portal.events(path, headers=headers)
    assert next(events)["event"] in ("live", "fleet")
    return events


def refused(pool, path, headers=None):
    """A refused stream's status, error code, and Retry-After."""
    response = pool.portal.request("GET", path, headers={"Accept": "text/event-stream", **(headers or {})})
    assert response.header("content-type") == "application/json", response.headers
    return response.status, response.json["error"], int(response.header("retry-after"))


@pytest.mark.parametrize("paths", [{"streams_per_ip": 2}], indirect=True)
def test_a_client_holds_at_most_streams_per_ip_worker_and_fleet_together(paths):
    pool, (worker, fleet) = paths
    first = opened(pool, worker)
    second = opened(pool, fleet)
    for path in (worker, fleet):
        status, code, retry = refused(pool, path)
        assert (status, code) == (429, "too_many_streams"), path
        assert retry > 0
    # A closed stream frees its place once the pool next writes to it.
    first.close()
    third = wait_until(lambda: _try(pool, worker), 15, "the place freed")
    for stream in (second, third):
        stream.close()


@pytest.mark.parametrize("paths", [{"max_streams": 2}], indirect=True)
def test_the_pool_holds_at_most_max_streams(paths):
    pool, (worker, fleet) = paths
    streams = [opened(pool, worker), opened(pool, fleet)]
    status, code, retry = refused(pool, fleet)
    assert (status, code) == (503, "stream_capacity") and retry > 0
    streams.pop().close()
    streams.append(wait_until(lambda: _try(pool, worker), 15, "the place freed"))
    for stream in streams:
        stream.close()


@pytest.mark.parametrize(
    "paths", [{"streams_per_ip": 1, "trusted_proxy": "127.0.0.1"}], indirect=True
)
def test_behind_the_trusted_proxy_each_forwarded_client_has_its_own_streams(paths):
    pool, (worker, fleet) = paths
    a, b = {"X-Forwarded-For": "203.0.113.7, 198.51.100.1"}, {"X-Forwarded-For": "198.51.100.2"}
    first = opened(pool, worker, a)
    assert refused(pool, fleet, a)[:2] == (429, "too_many_streams"), "the last entry is the client"
    second = opened(pool, worker, b)
    first.close()
    second.close()


def _try(pool, path):
    try:
        return opened(pool, path)
    except AssertionError:
        return None


def lookups(pool, forwarded):
    """The statuses of lookups of an unknown worker, each sent as forwarded from `forwarded`."""
    username = f"{sorted(pool.payout_addresses)[0]}.nobody"
    return [
        pool.portal.api(
            "/api/v1/lookup", method="POST", body={"username": username}, headers={"X-Forwarded-For": ip}
        ).status
        for ip in forwarded
    ]


def test_the_lookup_limit_keys_on_the_forwarded_address_only_from_the_trusted_proxy(pool_factory):
    trusted = pool_factory(overrides={"portal": {"lookups_per_minute": 2, "trusted_proxy": "127.0.0.1"}})
    assert lookups(trusted, ["198.51.100.1"] * 3) == [404, 404, 429]
    assert lookups(trusted, ["198.51.100.2"] * 2) == [404, 404], "another client, another budget"
    unforwarded = trusted.portal.api("/api/v1/lookup", method="POST", body={"username": "x"})
    assert unforwarded.status == 400, "the peer's own budget, unspent"

    for proxy in (None, "192.0.2.1"):
        overrides = {"lookups_per_minute": 2} | ({"trusted_proxy": proxy} if proxy else {})
        untrusted = pool_factory(overrides={"portal": overrides})
        assert lookups(untrusted, ["198.51.100.1", "198.51.100.2", "198.51.100.3"]) == [404, 404, 429], proxy
