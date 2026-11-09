"""V1 connections: the handshake, and hostile input that fails only its own socket."""

import tomllib

import pytest

from clients.sv1 import HostileSv1Client, Sv1Client
from regtest import ROOT
from wait import wait_until

# The pool's line cap, src/stratum/v1/session.rs.
MAX_LINE = 16 * 1024


@pytest.fixture
def connect(pool):
    clients = []

    def connect(cls=Sv1Client):
        client = cls(pool.solo_v1_port)
        clients.append(client)
        return client

    yield connect
    for client in clients:
        client.close()


def test_subscribe_comes_before_authorize(connect, miner_username):
    client = connect()
    early = client.request("mining.authorize", miner_username, "x")
    assert early["error"][0] == 25

    result = client.subscribe()
    assert len(bytes.fromhex(client.extranonce1)) == 4
    assert client.extranonce2_size == 8
    assert [name for name, _ in result[0]] == ["mining.set_difficulty", "mining.notify"]
    assert client.authorize(miner_username) is True


def test_extranonce1_is_unique_across_connections(connect):
    clients = [connect() for _ in range(8)]
    for client in clients:
        client.subscribe()
    assert len({client.extranonce1 for client in clients}) == len(clients)


def test_refuses_an_address_for_the_other_network(pool, connect):
    mainnet = tomllib.loads((ROOT / "config" / "mainnet.toml").read_text())
    client = connect()
    client.subscribe()
    response = client.request("mining.authorize", f"{mainnet['addresses']['pool_wallet']}.e2e", "x")
    assert response["error"][0] == 24
    assert "regtest" in response["error"][1]
    # Logged with its reason, as a live miner needs, and the miner may try again.
    [refused] = [line for line in pool.logs().splitlines() if "V1 authorization refused" in line]
    assert "not an address on regtest" in refused
    assert client.alive()


def test_a_connection_that_never_authorizes_is_closed_at_the_deadline(pool_factory, miner_username):
    pool = pool_factory(overrides={"stratum": {"authorize_timeout_secs": 1.0}})
    idle, miner = Sv1Client(pool.solo_v1_port), Sv1Client(pool.solo_v1_port)
    idle.subscribe()
    miner.subscribe()
    assert miner.authorize(miner_username) is True
    assert idle.closed_by_pool(timeout=10)
    wait_until(lambda: "not authorized within 1s" in pool.logs(), 5, "the reason logged")
    assert miner.alive()
    idle.close()
    miner.close()


def test_negotiates_version_rolling_within_bip320(connect):
    client = connect()
    assert client.configure("ffffffff", 2) == {
        "version-rolling": True,
        "version-rolling.mask": "1fffe000",
    }
    assert client.configure("00000fff", 1) == {"version-rolling": False}


def test_acknowledges_extranonce_subscribe(connect):
    client = connect()
    client.subscribe()
    assert client.call("mining.extranonce.subscribe") is True


def test_malformed_json_closes_only_its_connection(connect):
    bystander = connect()
    bystander.subscribe()
    hostile = connect(HostileSv1Client)
    hostile.send_raw(b'{"id": 1, "method": \n')
    assert hostile.closed_by_pool()
    assert bystander.alive()


def test_a_frame_split_across_segments_is_answered(connect):
    bystander = connect()
    hostile = connect(HostileSv1Client)
    hostile.send_bytewise({"id": 7, "method": "mining.subscribe", "params": []})
    response = hostile.read()
    assert response["id"] == 7
    assert len(bytes.fromhex(response["result"][1])) == 4
    assert bystander.alive()


def test_an_oversized_line_closes_only_its_connection(connect):
    bystander = connect()
    bystander.subscribe()
    hostile = connect(HostileSv1Client)
    hostile.send_raw(b"x" * (MAX_LINE + 1) + b"\n")
    assert hostile.closed_by_pool()
    assert bystander.alive()
