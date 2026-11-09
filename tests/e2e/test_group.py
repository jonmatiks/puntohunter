"""The group: instances on the regtest node, each listing the
others, show the same totals, the sum of their own `GET /api/v1/pool`, at `GET /api/v1/group`; a block found
on one adds to every total; one stopped leaves the others' totals within three refreshes and is counted again
once restarted. Members that time out, refuse, redirect, answer on another network, answer something else, or
answer too much are never counted, and the instance's own figures still are. No resource names a member or
carries its URL, and every response passes the privacy scan. Without `[group]` the resource answers 404."""

import http.server
import json
import re
import threading
import time

import pytest

import pool as pool_module
from clients.sv1 import Sv1Miner
from regtest import free_port
from wait import wait_until

REFRESH = 0.5
# A member counts for three refreshes; a stopped one is gone after them, give or take a poll.
GONE_WITHIN = REFRESH * 3 + 5
NAMES = ["member-alpha", "member-bravo", "member-charlie"]


def group_of(pool):
    response = pool.portal.api("/api/v1/group")
    assert response.status == 200, response.body
    assert response.header("cache-control") == "no-cache"
    return response.json


def own_figures(pool):
    """An instance's figures as a member reads them from its pool resource."""
    body = pool.portal.api("/api/v1/pool").json
    return {
        "hash_rate": sum(setup["hash_rate"] for setup in body["pools"]),
        "devices": sum(setup["devices"] for setup in body["pools"]),
        "blocks_found": body["blocks_found"],
    }


def grouped(pool, members, refresh=REFRESH):
    """Gives `pool` a [group] of `members`, (name, origin) pairs, before it starts."""
    pool.config["group"] = {
        "name": "E2E Group",
        "refresh_secs": refresh,
        "members": [{"name": name, "url": url} for name, url in members],
    }


@pytest.fixture
def three(pool_binary, regtest_cluster, tmp_path):
    """Three instances on the cluster, each listing the other two, started together."""
    pools = [
        pool_module.Pool(pool_binary, regtest_cluster, tmp_path / f"instance-{i}") for i in range(3)
    ]
    for i, pool in enumerate(pools):
        grouped(
            pool,
            [(NAMES[j], f"http://127.0.0.1:{other.portal_port}") for j, other in enumerate(pools) if j != i],
        )
    try:
        for pool in pools:
            pool.start()
        yield pools
    finally:
        for pool in pools:
            pool.kill()


def test_every_instance_shows_the_groups_totals(three, miner_username, solution_forcer):
    for pool in three:
        wait_until(lambda: group_of(pool)["reporting"] == 3, GONE_WITHIN, "all three counted")
    body = group_of(three[1])
    assert set(body) == {"name", "instances", "reporting", "hash_rate", "devices", "blocks_found"}
    assert (body["name"], body["instances"]) == ("E2E Group", 3)
    for key in ("instances", "reporting", "devices", "blocks_found"):
        assert isinstance(body[key], int), key

    # A miner and a block on the first instance reach every instance's totals.
    miner = Sv1Miner(three[0].solo_v1_port, miner_username)
    _, block_hash = solution_forcer(miner)
    wait_until(lambda: three[0].block(block_hash).get("result"), 10, "the block recorded")
    own = [own_figures(pool) for pool in three]
    assert own[0]["devices"] == 1 and own[0]["blocks_found"] == 1 and own[0]["hash_rate"] > 0
    expected = {key: sum(figures[key] for figures in own) for key in ("devices", "blocks_found")}
    for pool in three:
        wait_until(
            lambda: {key: group_of(pool)[key] for key in expected} == expected,
            GONE_WITHIN,
            "the block and the miner in the totals",
        )
        assert group_of(pool)["hash_rate"] > 0

    # Another instance says the same, and names no member.
    body = group_of(three[2])
    assert (body["name"], body["devices"], body["blocks_found"], body["instances"], body["reporting"]) == (
        "E2E Group", 1, 1, 3, 3,
    )
    for pool in three:
        assert f"127.0.0.1:{pool.portal_port}" not in json.dumps(body)
        assert not any(name in json.dumps(body) for name in NAMES)
    miner.client.close()


def test_a_stopped_instance_leaves_the_totals_and_rejoins(three):
    for pool in three:
        wait_until(lambda: group_of(pool)["reporting"] == 3, GONE_WITHIN, "all three counted")
    three[2].kill()
    stopped = time.monotonic()
    for pool in three[:2]:
        wait_until(lambda: group_of(pool)["reporting"] == 2, GONE_WITHIN, "the stopped instance left")
    assert time.monotonic() - stopped >= REFRESH, "counted through a missed poll"
    # The poller judges a member after each fetch, the totals when read, so the metric may trail them a poll.
    wait_until(lambda: 'member="member-charlie"} 0' in three[0].metrics(), REFRESH * 4, "the member down")
    logs = three[0].logs()
    assert re.search(r"group member not counted.*member=member-charlie.*reason=cannot connect", logs), logs[-2000:]

    three[2].start()
    for pool in three:
        wait_until(lambda: group_of(pool)["reporting"] == 3, GONE_WITHIN, "the instance back")
    wait_until(lambda: 'member="member-charlie"} 1' in three[0].metrics(), REFRESH * 4, "the member up")
    assert re.search(r"group member counted in the totals.*member=member-charlie", three[0].logs().split("group member not counted")[-1])


def pool_resource(network, padding=0):
    resource = {
        "network": network,
        "pools": [{"id": "solo", "hash_rate": 1e18, "devices": 5000}],
        "blocks_found": 7000,
    }
    if padding:
        resource["padding"] = "x" * padding
    return json.dumps(resource).encode()


class Stub(http.server.BaseHTTPRequestHandler):
    """A member that misbehaves as its `behaviour` says."""

    behaviour = None
    # Where the redirect points: a member answering a good pool resource.
    target = None

    def do_GET(self):
        if self.behaviour == "slow":
            time.sleep(8)
            self.answer(200, pool_resource("regtest"))
        elif self.behaviour == "redirect":
            self.send_response(307)
            self.send_header("Location", f"{self.target}/api/v1/pool")
            self.send_header("Content-Length", "0")
            self.end_headers()
        elif self.behaviour == "good":
            self.answer(200, pool_resource("regtest"))
        elif self.behaviour == "mainnet":
            self.answer(200, pool_resource("main"))
        elif self.behaviour == "html":
            self.answer(200, b"<html><body>a website</body></html>", "text/html")
        elif self.behaviour == "large":
            self.answer(200, pool_resource("regtest", padding=70_000))

    def answer(self, status, body, content_type="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass  # the instance gave up first

    def log_message(self, *args):
        pass


@pytest.fixture
def stubs():
    """Serves one stub member per behaviour, returning each one's origin by behaviour."""
    servers = []

    def serve(behaviour, **attrs):
        handler = type(f"Stub_{behaviour}", (Stub,), {"behaviour": behaviour, **attrs})
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        servers.append(server)
        return f"http://127.0.0.1:{server.server_address[1]}"

    good = serve("good")
    origins = {behaviour: serve(behaviour) for behaviour in ("slow", "mainnet", "html", "large")}
    origins["redirect"] = serve("redirect", target=good)
    # A port nothing listens on.
    origins["refused"] = f"http://127.0.0.1:{free_port()}"
    try:
        yield origins
    finally:
        for server in servers:
            server.shutdown()
            server.server_close()


def test_misbehaving_members_are_never_counted(pool_binary, regtest_cluster, tmp_path, stubs):
    pool = pool_module.Pool(pool_binary, regtest_cluster, tmp_path / "instance")
    grouped(pool, [(f"stub-{behaviour}", origin) for behaviour, origin in stubs.items()])
    reasons = {
        "slow": "timed out",
        "refused": "cannot connect",
        "redirect": "status 307",
        "mainnet": 'on network "main"',
        "html": "not a pool resource",
        "large": "answer over 65536 bytes",
    }
    try:
        pool.start()
        for behaviour, reason in reasons.items():
            wait_until(
                lambda: re.search(
                    rf"group member not counted.*member=stub-{behaviour}.*reason={re.escape(reason)}",
                    pool.logs(),
                ),
                15,
                f"stub-{behaviour} refused as {reason}",
            )
        # Three refreshes and a slow poll later, still none counted, and each logged once.
        time.sleep(REFRESH * 3)
        body = group_of(pool)
        assert (body["instances"], body["reporting"]) == (7, 1)
        assert {key: body[key] for key in ("hash_rate", "devices", "blocks_found")} == own_figures(pool)
        for behaviour in reasons:
            assert pool.logs().count(f"member=stub-{behaviour}") == 1, behaviour
            assert f'member="stub-{behaviour}"}} 0' in pool.metrics()
        # A member's outage is not this instance's.
        assert pool.portal.api("/api/v1/health").json["status"] == "ok"
    finally:
        pool.kill()


def test_without_a_group_there_is_no_resource(pool):
    response = pool.portal.api("/api/v1/group")
    assert response.status == 404
    assert response.json["error"] == "not_found"
