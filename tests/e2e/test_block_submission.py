"""Block submission (Coinbase and solo payout): a forced block reaches the node, is
recorded once with its hex on disk, and pays the exact split; a duplicate answer is success; a genuine
reject is logged as critical and leaves the tip alone."""

import pytest

from chain import sats
from clients.sv1 import Sv1Miner
from wait import wait_until

STALE, DUPLICATE = 21, 22


@pytest.fixture
def miner(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    yield miner
    miner.client.close()


def submissions(pool, block_hash):
    return [
        line
        for line in pool.logs().splitlines()
        if "submitting block" in line and block_hash in line
    ]


def test_a_forced_block_is_accepted_recorded_once_and_split_exactly(
    pool, miner, regtest_cluster, solution_forcer
):
    chain = regtest_cluster.chain
    params, block_hash = solution_forcer(miner)
    wait_until(lambda: chain.tip()[1] == block_hash, 10, "the block at the tip")
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "its answer recorded")

    # ESP-Miner resends shares it already sent. The copy is refused, a duplicate, or stale if the job on the
    # new tip got there first, and the block is not submitted again.
    assert miner.rejection(params).code in (DUPLICATE, STALE)
    assert len(submissions(pool, block_hash)) == 1
    row = pool.block(block_hash)
    assert (row["height"], row["pool"]) == (chain.tip()[0], "solo")
    assert pool.block_hex(block_hash) == chain.call("getblock", block_hash, 0)

    stats = chain.call("getblockstats", block_hash, ["subsidy", "totalfee"])
    reward = stats["subsidy"] + stats["totalfee"]
    vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
    expected = pool.reward_outputs(reward, miner.username.split(".")[0])
    assert len(expected) == 3, "the miner portion and the checked-in configs' two fees"
    assert [(o["scriptPubKey"].get("address"), sats(o["value"])) for o in vout[:3]] == expected
    assert sum(sats(o["value"]) for o in vout) == reward
    assert vout[3]["scriptPubKey"]["type"] == "nulldata", "the witness commitment"
    assert len(vout) == 4
    # The found-block line and the row name the finder's worker and transport.
    [found] = [l for l in pool.logs().splitlines() if "share is a block" in l and block_hash in l]
    assert "worker=e2e" in found and "transport=V1" in found
    assert (row["worker"], row["transport"]) == ("e2e", "V1")


def test_a_block_the_node_already_has_is_a_duplicate_and_success(
    pool_factory, fresh_node, miner_username, solution_forcer
):
    pool = pool_factory(node=fresh_node, overrides={"node": {"rpc_timeout_secs": 1.0}})
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    fresh_node.pause()
    try:
        _, block_hash = solution_forcer(miner)
        wait_until(
            lambda: "block submission unanswered" in pool.logs(), 10, "the unanswered submission"
        )
    finally:
        # Stopped before the node returns, so this run never resubmits.
        pool.kill()
        fresh_node.unpause()
        miner.client.close()
    assert pool.block(block_hash)["result"] is None

    # The block reaches the node another way, as it would from a peer. The node may already have taken it
    # from the request it was frozen in.
    assert fresh_node.chain.call("submitblock", pool.block_hex(block_hash)) in (None, "duplicate")
    assert fresh_node.chain.tip()[1] == block_hash

    # The next run resubmits what the last left unanswered, and the node's duplicate counts as success.
    pool.start()
    wait_until(
        lambda: pool.block(block_hash).get("result") == "duplicate", 10, "the duplicate recorded"
    )
    assert pool.block(block_hash)["status"] != "rejected"
    assert "CRITICAL" not in pool.logs()


def test_a_genuine_reject_is_critical_and_leaves_the_tip(
    pool, miner, regtest_cluster, solution_forcer
):
    chain = regtest_cluster.chain
    tip = chain.tip()
    # Hours behind the ntime the miner will submit, so the node sees the block as too far in the future.
    chain.call("setmocktime", int(miner.notify[7], 16) - 3 * 3600)
    try:
        _, block_hash = solution_forcer(miner)
        wait_until(
            lambda: pool.block(block_hash).get("status") == "rejected", 10, "the reject recorded"
        )
    finally:
        chain.call("setmocktime", 0)

    assert pool.block(block_hash)["result"] == "time-too-new"
    assert chain.tip() == tip
    assert pool.block_hex(block_hash), "on disk whatever the result"
    [critical] = [line for line in pool.logs().splitlines() if "CRITICAL" in line]
    assert block_hash in critical and "time-too-new" in critical
