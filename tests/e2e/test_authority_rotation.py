"""The SV2 authority key's rotation, rehearsed as the operator runs it: the next key made
with `puntohunter authority next`, announced in the pool resource with the day it takes over, then moved over the
current key at the switch, after which a V2 client pinned to the old key is refused and one pinned to the new key
is admitted."""

import re
import subprocess

import pytest

from clients.sv2 import Sv2Client, Sv2Error

SWITCH = "2026-11-15"


@pytest.fixture
def clients():
    started = []

    def start():
        client = Sv2Client()
        started.append(client)
        return client

    yield start
    for client in started:
        client.close()


def make_next_key(pool):
    made = subprocess.run(
        [pool.binary, "authority", "next", "--config", pool.config_path], capture_output=True, text=True
    )
    assert made.returncode == 0, made.stderr
    return re.search(r"next SV2 authority key \S+: (\w+)$", made.stdout, re.M)[1]


def admitted(clients, pool, key):
    client = clients()
    try:
        client.connect(pool.solo_v2_port, pin=key)
    except Sv2Error:
        return False
    return client.setup()["event"] == "setup_success"


def test_a_rotation_is_announced_then_switches_the_pinned_key(pool_factory, clients):
    pool = pool_factory()
    old = pool.authority_key()
    assert pool.portal.api("/api/v1/pool").json["next_authority_key"] is None

    new = make_next_key(pool)
    assert new != old
    assert make_next_key(pool) == new, "run again, the next key is kept"
    assert (pool.data_dir / "sv2-authority.next.key").stat().st_mode & 0o077 == 0

    # Announced: the next key and its day, beside the current key, which still signs.
    pool.config["public"] = {"authority_switch": SWITCH}
    pool.restart()
    assert pool.authority_key() == old
    assert pool.portal.api("/api/v1/pool").json["next_authority_key"] == {"key": new, "switch_on": SWITCH}
    assert admitted(clients, pool, old)
    assert not admitted(clients, pool, new), "the new pin waits for the switch"

    # The switch: the next key over the current one, the day removed.
    (pool.data_dir / "sv2-authority.next.key").rename(pool.data_dir / "sv2-authority.key")
    del pool.config["public"]
    pool.restart()
    assert pool.authority_key() == new
    assert not admitted(clients, pool, old), "a device pinned to the old key is refused"
    assert admitted(clients, pool, new), "and admitted with the new pin"
    body = pool.portal.api("/api/v1/pool").json
    assert body["authority_key"] == new and body["next_authority_key"] is None
