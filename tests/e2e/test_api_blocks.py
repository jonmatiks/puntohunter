"""The block lists (API reference section 2.8): `/api/v1/blocks`, a worker's, and a fleet's, newest first, in
raw values. Each status, the nulls each pool gives, `blocks_to_maturity` counting down as the chain advances,
`finder_name` on the fleet's list alone, and `?limit`. Unknown and malformed handles answer JSON 404. Every
response passes the privacy scan."""

import re

from clients.sv1 import Sv1Miner
from wait import wait_until

FIELDS = {
    "height", "hash", "pool", "transport", "finder", "miner", "status", "finder_sats", "pool_sats",
    "allocated_sats", "blocks_to_maturity",
}


def listed(pool, path="/api/v1/blocks"):
    response = pool.portal.api(path)
    assert response.status == 200, (path, response.body)
    assert response.header("cache-control") == "no-cache"
    return response.json["blocks"]


def block(pool, block_hash, path="/api/v1/blocks"):
    return next((b for b in listed(pool, path) if b["hash"] == block_hash), None)


def logged_handle(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))[-1]


def test_a_solo_block_through_each_status(pool_factory, fresh_node, miner_username, solution_forcer):
    pool = pool_factory(node=fresh_node, overrides={"node": {"rpc_timeout_secs": 1.0}})
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    handle = logged_handle(pool, "V1 authorized")
    worker = f"/api/v1/workers/{handle}/blocks"
    # Frozen, the node cannot answer the submission, so the block holds at submitted.
    fresh_node.pause()
    try:
        _, block_hash = solution_forcer(miner)
        miner.client.close()
        wait_until(lambda: "block submission unanswered" in pool.logs(), 10, "the unanswered submission")
        found = block(pool, block_hash)
        assert found["status"] == "submitted" and found["blocks_to_maturity"] is None
    finally:
        fresh_node.unpause()

    # One confirmation of the 101 maturity takes, once the template builds on it.
    found = wait_until(
        lambda: (b := block(pool, block_hash)) and b["blocks_to_maturity"] == 100 and b, 30, "100 to go"
    )
    assert set(found) == FIELDS, "finder_name is the fleet list's alone"
    assert found["status"] == "confirmed_immature"
    assert (found["pool"], found["transport"], found["finder"]) == ("solo", "V1", handle)
    assert isinstance(found["height"], int) and found["finder_sats"] > 0
    assert found["pool_sats"] is None and found["allocated_sats"] is None, "a solo block has neither"
    assert block(pool, block_hash, worker) == found

    fresh_node.chain.mine(1, fresh_node.funder_address())
    wait_until(lambda: block(pool, block_hash)["blocks_to_maturity"] == 99, 10, "99 to go")
    fresh_node.chain.mine(99, fresh_node.funder_address())
    wait_until(lambda: block(pool, block_hash)["status"] == "mature", 30, "mature")
    assert block(pool, block_hash)["blocks_to_maturity"] == 0

    fresh_node.chain.call("invalidateblock", block_hash)
    try:
        wait_until(lambda: block(pool, block_hash)["status"] == "orphaned", 30, "orphaned")
        assert block(pool, block_hash)["blocks_to_maturity"] is None, "off the chain"
    finally:
        fresh_node.chain.call("reconsiderblock", block_hash)
    wait_until(lambda: block(pool, block_hash)["status"] == "mature", 30, "mature again")


def test_a_rejected_block(pool_factory, fresh_node, miner_username, solution_forcer):
    pool = pool_factory(node=fresh_node)
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    # Hours behind the ntime the miner will submit, so the node sees the block as too far in the future.
    fresh_node.chain.call("setmocktime", int(miner.notify[7], 16) - 3 * 3600)
    try:
        _, block_hash = solution_forcer(miner)
        wait_until(lambda: (b := block(pool, block_hash)) and b["status"] == "rejected", 10, "rejected")
    finally:
        fresh_node.chain.call("setmocktime", 0)
    miner.client.close()
    assert block(pool, block_hash)["blocks_to_maturity"] is None


def test_each_list_holds_its_own_and_only_the_fleets_names_finders(pool, regtest_cluster, solution_forcer):
    address = regtest_cluster.miner_address(0)
    named = Sv1Miner(pool.pooled_v1_port, f"{address}.rig")
    named_handle = logged_handle(pool, "V1 authorized")
    _, first = solution_forcer(named)
    named.client.close()
    wait_until(lambda: pool.block(first).get("result"), 10, "the first block answered")
    bare = Sv1Miner(pool.pooled_v1_port, address)
    bare_handle = logged_handle(pool, "V1 authorized")
    _, second = solution_forcer(bare)
    bare.client.close()
    wait_until(lambda: pool.block(second).get("result"), 10, "the second block answered")
    stranger = Sv1Miner(pool.solo_v1_port, f"{regtest_cluster.miner_address(1)}.rig")
    _, third = solution_forcer(stranger)
    stranger.client.close()
    wait_until(lambda: pool.block(third).get("result"), 10, "the third block answered")

    everything = [b["hash"] for b in listed(pool)]
    assert everything.index(third) < everything.index(second) < everything.index(first), "newest first"
    pooled = block(pool, first)
    assert pooled["finder_sats"] is None, "a pooled block has no finder output"
    assert pooled["pool_sats"] > 0
    wait_until(lambda: block(pool, first)["allocated_sats"], 10, "the window allocated")
    assert "finder_name" not in pooled

    assert [b["hash"] for b in listed(pool, f"/api/v1/workers/{named_handle}/blocks")] == [first]
    [fleet] = {r["fleet"] for r in pool.rows("SELECT fleet FROM fleets WHERE handle = ?", named_handle)}
    rows = listed(pool, f"/api/v1/fleets/{fleet}/blocks")
    assert [b["hash"] for b in rows] == [second, first], "the fleet's own, newest first"
    assert [(b["finder"], b["finder_name"]) for b in rows] == [(bare_handle, ""), (named_handle, "rig")]


def test_limit_and_unknown_handles(pool, regtest_cluster, miner_username, solution_forcer):
    for _ in range(2):
        # A connection a block, each on the tip the last one left.
        miner = Sv1Miner(pool.solo_v1_port, miner_username)
        handle = logged_handle(pool, "V1 authorized")
        _, block_hash = solution_forcer(miner)
        miner.client.close()
        wait_until(lambda: pool.block(block_hash).get("result"), 10, "the block answered")
    assert len(listed(pool, "/api/v1/blocks?limit=1")) == 1
    assert len(listed(pool, f"/api/v1/workers/{handle}/blocks?limit=200")) == 2
    for limit in ("0", "201", "abc", "-1", ""):
        response = pool.portal.api(f"/api/v1/blocks?limit={limit}")
        assert response.status == 400, limit
        assert response.json["error"] == "bad_request", limit
    for path in ("/api/v1/workers/aaaaaaaaaaaaaaaa/blocks", "/api/v1/workers/nope/blocks",
                 "/api/v1/fleets/aaaaaaaaaaaaaaaa/blocks", "/api/v1/fleets/NOPE/blocks"):
        response = pool.portal.api(path)
        assert response.status == 404, path
        assert response.json["error"] == "not_found", path
