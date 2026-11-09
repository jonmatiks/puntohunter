"""`GET /api/v1/pool` (API reference section 2.1): the overview's figures in raw
values, naming no one. Every field in its units, and each configured pool in config order with its own; a pool
not configured is not listed; the chain is null while the node gives no work; the miner portion is null with a
fixed fee; payouts are null without `[payouts]`. A connected miner and a found block are counted. Unknown paths
under /api/v1 answer JSON 404. Every response passes the privacy scan."""

import re

from clients.sv1 import Sv1Miner
from payouts import start
from regtest import REGTEST_CONFIG
from wait import wait_until

FAST_TIMEOUTS = {"node": {"rpc_timeout_secs": 1.0, "longpoll_timeout_secs": 2.0}}


def pool_of(pool, host=None):
    headers = {"Host": f"{host}:{pool.portal_port}"} if host else None
    response = pool.portal.api("/api/v1/pool", headers=headers)
    assert response.status == 200, response.body
    assert response.header("cache-control") == "no-cache"
    return response.json


def test_every_field_in_its_units(pool, regtest_cluster):
    body = pool_of(pool, "pool.example")
    assert set(body) == {
        "network", "version", "started_at", "uptime_secs", "outage", "chain", "authority_key",
        "next_authority_key", "pools",
        "reward", "accounting", "payouts", "blocks_found", "round_best_share",
    }
    assert body["network"] == "regtest"
    logged = re.search(r'\bstarting version="([^"]+)"', pool.logs())[1]
    assert body["version"] == f"v{logged}", "the running version"
    assert isinstance(body["started_at"], int) and 0 <= body["uptime_secs"] < 600
    assert body["outage"] is False
    tip = regtest_cluster.chain.call("getblockcount")
    chain = wait_until(lambda: pool_of(pool)["chain"], 10, "a template")
    assert chain["tip_height"] in (tip, tip + 1) and chain["network_difficulty"] > 0
    key = pool.authority_key()
    assert body["authority_key"] == key
    assert body["next_authority_key"] is None, "no rotation announced"
    fees = [
        {"name": "operator", "portion": {"ppm": 9_000}},
        {"name": "partner", "portion": {"ppm": 6_000}},
    ]
    # W_min, above k × regtest's difficulty.
    window = {"window_multiplier": 2.0, "window_min_work": 1_000_000.0, "window_size": 1_000_000.0}
    assert [setup["id"] for setup in body["pools"]] == ["solo", "pooled", "hybrid"], "in config order"
    for setup, (name, finder, shared) in zip(
        body["pools"],
        [("Solo", {"ppm": 1_000_000}, False), ("Pooled", {"ppm": 0}, True), ("Hybrid", {"ppm": 333_300}, True)],
    ):
        id = setup["id"]
        assert setup == {
            "id": id,
            "name": name,
            "kind": id,
            "stratum_v1": f"stratum+tcp://pool.example:{getattr(pool, f'{id}_v1_port')}",
            "stratum_v2": f"stratum2+tcp://pool.example:{getattr(pool, f'{id}_v2_port')}/{key}",
            "finder": finder,
            "miner": {"ppm": 985_000},
            "fees": fees,
            "window": window if shared else None,
            "personal": None,
            "worker_names": False,
            "worker_names_opt_in": False,
            "hash_rate": 0,
            "devices": 0,
        }
    assert body["reward"] == {"miner": {"ppm": 985_000}, "fees": fees}
    assert body["accounting"] == {"available_after_confirmations": 101}
    assert body["payouts"] is None, "the harness removes [payouts]"
    assert isinstance(body["blocks_found"], int)


def test_the_counts_follow_a_connected_miner_and_a_found_block(pool, regtest_cluster, miner_username, solution_forcer):
    devices = lambda body, id: next(setup for setup in body["pools"] if setup["id"] == id)["devices"]
    body = pool_of(pool)
    assert devices(body, "solo") == 0
    blocks = body["blocks_found"]

    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    _, block_hash = solution_forcer(miner)
    wait_until(lambda: pool.block(block_hash).get("result"), 10, "the block recorded")
    body = pool_of(pool)
    assert devices(body, "solo") == 1
    assert devices(body, "pooled") == 0
    assert body["pools"][0]["hash_rate"] > 0
    assert body["blocks_found"] == blocks + 1
    wait_until(
        lambda: pool_of(pool)["chain"]["tip_height"] == regtest_cluster.chain.call("getblockcount"),
        10,
        "the height of the new tip",
    )
    miner.client.close()


def test_a_pool_not_configured_is_not_listed(pool_factory):
    pool = pool_factory(overrides={"pools": {"pooled": None, "hybrid": None}})
    assert [setup["id"] for setup in pool_of(pool)["pools"]] == ["solo"]


def test_the_miner_portion_is_null_with_a_fixed_fee(pool_factory):
    operator = REGTEST_CONFIG["fees"][0]
    pool = pool_factory(overrides={"fees": [{"name": "operator", "address": operator["address"], "btc": 0.01}]})
    body = pool_of(pool)
    fees = [{"name": "operator", "portion": {"sats": 1_000_000}}]
    assert body["reward"] == {"miner": None, "fees": fees}
    assert all((setup["miner"], setup["fees"]) == (None, fees) for setup in body["pools"])


def test_payout_terms_with_payouts(pool_factory, fresh_node):
    pool = start(pool_factory, fresh_node)
    payouts = pool_of(pool)["payouts"]
    assert payouts["threshold_sats"] == pool.config["payouts"]["threshold_sats"]
    assert payouts["interval_secs"] == 0, "half a second, in whole seconds"
    assert payouts["standing"]["state"] in ("starting", "running", "deferred")
    assert payouts["last"] is None


def test_the_chain_is_null_while_the_node_gives_no_work(pool_factory, fresh_node):
    pool = pool_factory(node=fresh_node, overrides=FAST_TIMEOUTS)
    wait_until(lambda: pool_of(pool)["chain"], 10, "a template")
    fresh_node.stop()
    body = wait_until(lambda: (b := pool_of(pool))["outage"] and b, 30, "the outage")
    assert body["chain"] is None
    assert [setup["window"]["window_size"] for setup in body["pools"][1:]] == [None, None]
    # The node returns on a fresh chain, its datadir being tmpfs.
    fresh_node.up()
    fresh_node.chain.mine(1, fresh_node.address("operator"))
    body = wait_until(lambda: (b := pool_of(pool))["chain"] and b, 30, "the chain back")
    assert body["outage"] is False


def test_an_unknown_path_answers_json_404(pool):
    for path in ("/api/v1/nothing", "/api/v1/pool/more", "/api/v1"):
        response = pool.portal.api(path)
        assert response.status == 404, path
        assert response.json == {"error": "not_found", "message": "no such resource"}, path
    assert pool.portal.get("/nothing").header("content-type").startswith("text/html")
