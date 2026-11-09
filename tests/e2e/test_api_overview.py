"""The overview's settings and the running version on `GET /api/v1/pool` (API reference section 2.1):
copy-ready Stratum URLs built from the `[public]` host and ports when set, a port not set being its listener's;
only the modes that are on, matching the listeners the pool opened and the endpoint it logged; and the running
version and its commit, as `--version` and the startup log give them. URLs built from the request's Host header,
and each mode's name and figures, are checked in test_api_pool.py. Every response passes the privacy scan."""

import re
import socket
import subprocess
import tomllib

import pytest

from regtest import ROOT


def urls(pool):
    """Each pool's Stratum URLs on the pool resource, V1 then V2, in config order."""
    body = pool.portal.api("/api/v1/pool", headers={"Host": f"127.0.0.1:{pool.portal_port}"}).json
    return [url for setup in body["pools"] for url in (setup["stratum_v1"], setup["stratum_v2"])]


def test_the_settings_name_the_public_host_and_ports_when_set(pool_factory):
    pool = pool_factory(
        overrides={
            "public": {"host": "Pool.Example.com"},
            "pools": {"solo": {"public_v1": 443}, "hybrid": {"public_v2": 8443}},
        }
    )
    key = pool.authority_key()
    assert urls(pool) == [
        "stratum+tcp://pool.example.com:443",
        f"stratum2+tcp://pool.example.com:{pool.solo_v2_port}/{key}",
        f"stratum+tcp://pool.example.com:{pool.pooled_v1_port}",
        f"stratum2+tcp://pool.example.com:{pool.pooled_v2_port}/{key}",
        f"stratum+tcp://pool.example.com:{pool.hybrid_v1_port}",
        f"stratum2+tcp://pool.example.com:8443/{key}",
    ], "a port not configured is its listener's"

    lan = pool_factory(overrides={"public": {"host": "192.0.2.10"}})
    key = lan.authority_key()
    assert urls(lan) == [
        f"stratum+tcp://192.0.2.10:{lan.solo_v1_port}",
        f"stratum2+tcp://192.0.2.10:{lan.solo_v2_port}/{key}",
        f"stratum+tcp://192.0.2.10:{lan.pooled_v1_port}",
        f"stratum2+tcp://192.0.2.10:{lan.pooled_v2_port}/{key}",
        f"stratum+tcp://192.0.2.10:{lan.hybrid_v1_port}",
        f"stratum2+tcp://192.0.2.10:{lan.hybrid_v2_port}/{key}",
    ]


def test_the_overview_lists_only_the_modes_that_are_on(pool_factory):
    """A pool not configured is not listed (test_api_pool.py), and its listeners are not opened."""
    pool = pool_factory(overrides={"pools": {"pooled": None}})
    key = pool.authority_key()
    assert urls(pool) == [
        f"stratum+tcp://127.0.0.1:{pool.solo_v1_port}",
        f"stratum2+tcp://127.0.0.1:{pool.solo_v2_port}/{key}",
        f"stratum+tcp://127.0.0.1:{pool.hybrid_v1_port}",
        f"stratum2+tcp://127.0.0.1:{pool.hybrid_v2_port}/{key}",
    ]
    [endpoint] = [line for line in pool.logs().splitlines() if "public Stratum endpoint" in line]
    assert "solo_v1=" in endpoint and "pooled_v1=" not in endpoint
    for port in (pool.pooled_v1_port, pool.pooled_v2_port):
        with pytest.raises(ConnectionRefusedError):
            socket.create_connection(("127.0.0.1", port), timeout=5).close()


def test_the_pool_resource_gives_the_running_version_and_its_commit(pool):
    """The version, its commit, and -dirty when built from uncommitted sources; the same in `--version` and the
    startup log."""
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    commit = subprocess.run(
        ["git", "rev-parse", "--short=8", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout.strip()
    sent = pool.portal.api("/api/v1/pool").json["version"]
    assert re.fullmatch(rf"v{re.escape(version)}\+{commit}(-dirty)?", sent), sent
    running = sent.removeprefix("v")
    shown = subprocess.run([pool.binary, "--version"], capture_output=True, text=True, check=True).stdout
    assert shown == f"puntohunter {running}\n"
    assert f'version="{running}"' in pool.logs()
