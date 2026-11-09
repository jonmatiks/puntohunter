"""Block submission across the node fleet: a found block goes to every node at
once and is recorded once, a hung node holds up neither the others nor the record, and a node's genuine reject
escalates while the block still counts as found. A duplicate answer's handling is held by the unit tests on
fake nodes (src/pool/submitter.rs) and the single-node resubmission case: which of two nodes sees a block
first, over P2P or from the pool, is a race no test here can fix."""

import re

import pool as pool_module
from clients.sv1 import Sv1Miner
from fleet import health, selected
from wait import wait_until

SUCCESS = ("accepted", "duplicate", "inconclusive")
# A found block's status, until it matures: lifecycle may already have seen it confirmed.
FOUND = ("submitted", "confirmed_immature")


def username(fleet):
    return f"{fleet.nodes[0].miner_address(0)}.e2e"


def field(line, name):
    """A field of a log line, written `name=value` or `name="value"`."""
    return re.search(rf'{name}="?([^"\s]+)', line)[1]


def answers(pool, block_hash):
    """Each node's answer to the block, from the pool's log, as {node: result or "unanswered"}."""
    found = {}
    for line in pool.logs().splitlines():
        if block_hash not in line or "node=" not in line:
            continue
        node = field(line, "node")
        if "node answered the block" in line:
            found[node] = field(line, "result")
        elif "rejected a block" in line:
            found[node] = field(line, "reason")
        elif "did not answer the block" in line:
            found[node] = "unanswered"
    return found


def test_a_found_block_reaches_every_node_and_is_recorded_once(fleet_pool, regtest_fleet, solution_forcer):
    pool, fleet = fleet_pool, regtest_fleet
    wait_until(lambda: selected(pool) == "primary", 30, "work from the primary")
    miner = Sv1Miner(pool.solo_v1_port, username(fleet))
    _, block_hash = solution_forcer(miner)

    wait_until(lambda: len(answers(pool, block_hash)) == 3, 10, "an answer from each node")
    found = answers(pool, block_hash)
    assert set(found) == {"primary", "node1", "node2"}
    assert all(answer in SUCCESS for answer in found.values()), found
    assert pool.block(block_hash)["status"] in FOUND
    assert pool.block(block_hash)["result"] in SUCCESS
    assert [row["hash"] for row in pool.rows("SELECT hash FROM blocks")] == [block_hash]
    assert fleet.synced()[1] == block_hash
    assert "CRITICAL" not in pool.logs()
    miner.client.close()


def test_a_hung_node_holds_up_neither_the_others_nor_the_record(
    pool_binary, regtest_fleet, tmp_path, solution_forcer
):
    fleet = regtest_fleet
    overrides = fleet.overrides()
    # node2's calls wait long enough that holding up the record would show.
    overrides["secondary_nodes"][1]["rpc_timeout_secs"] = 15.0
    pool = pool_module.Pool(pool_binary, fleet.nodes[0], tmp_path, overrides=overrides)
    pool.start()
    try:
        wait_until(lambda: selected(pool) == "primary", 30, "work from the primary")
        miner = Sv1Miner(pool.solo_v1_port, username(fleet))
        fleet.nodes[2].pause()
        try:
            _, block_hash = solution_forcer(miner)
            wait_until(
                lambda: pool.block(block_hash).get("result") in SUCCESS, 5, "the block recorded meanwhile"
            )
            found = answers(pool, block_hash)
            assert found.get("primary") in SUCCESS and found.get("node1") in SUCCESS, found
            assert "node2" not in found, "still waiting on the hung node"
        finally:
            fleet.nodes[2].unpause()
        wait_until(lambda: "node2" in answers(pool, block_hash), 20, "the hung node's answer logged")
        assert fleet.synced()[1] == block_hash
        miner.client.close()
    finally:
        pool.kill()


def test_a_reject_on_one_node_escalates_while_the_block_counts_as_found(
    fleet_pool, regtest_fleet, solution_forcer
):
    pool, fleet = fleet_pool, regtest_fleet
    wait_until(lambda: selected(pool) == "primary", 30, "work from the primary")
    miner = Sv1Miner(pool.solo_v1_port, username(fleet))
    # node2 refuses the tip the work builds on, so the block's parent is invalid to it.
    tip = fleet.nodes[0].chain.tip()[1]
    fleet.nodes[2].chain.call("invalidateblock", tip)
    try:
        _, block_hash = solution_forcer(miner)
        wait_until(lambda: len(answers(pool, block_hash)) == 3, 10, "an answer from each node")
        found = answers(pool, block_hash)
        assert found["primary"] in SUCCESS and found["node1"] in SUCCESS, found
        assert found["node2"] not in SUCCESS, found
        assert pool.block(block_hash)["status"] in FOUND, "found, whatever node2 says"
        [critical] = [line for line in pool.logs().splitlines() if "CRITICAL" in line]
        assert block_hash in critical and field(critical, "node") == "node2"
        assert health(pool)[0] == 200
    finally:
        fleet.nodes[2].chain.call("reconsiderblock", tip)
    miner.client.close()
