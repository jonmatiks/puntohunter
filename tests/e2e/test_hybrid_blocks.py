"""Hybrid blocks: a block forced on a hybrid listener, over V1 or
V2, pays its finder's address exactly the finder's part, the pool wallet the rest of the miner portion, and
the fees as solo does, summing to the reward; a finder's part above the miner portion leaves one finder output
and no pool output. Each block row records what its reward outputs paid."""

import pytest

from chain import sats
from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until


def mined(pool, chain, block_hash):
    """The block's reward and coinbase outputs, as (address, sats), once the pool has its answer."""
    wait_until(lambda: chain.tip()[1] == block_hash, 10, "the block at the tip")
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "its answer recorded")
    stats = chain.call("getblockstats", block_hash, ["subsidy", "totalfee"])
    vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
    paid = [(o["scriptPubKey"].get("address"), sats(o["value"])) for o in vout]
    return stats["subsidy"] + stats["totalfee"], paid


def force_v1(pool, username, solution_forcer):
    miner = Sv1Miner(pool.hybrid_v1_port, username)
    try:
        return solution_forcer(miner)[1]
    finally:
        miner.client.close()


def force_v2(pool, username, _):
    miner = Sv2Miner(pool, pool.hybrid_v2_port, username)
    try:
        [block_hash] = miner.submit(kind="block")["hashes"]
        return block_hash
    finally:
        miner.close()


@pytest.mark.parametrize("force", [force_v1, force_v2], ids=["V1", "V2"])
def test_a_hybrid_block_pays_its_finder_its_part_and_the_pool_wallet_the_rest(
    pool, regtest_cluster, solution_forcer, force
):
    chain = regtest_cluster.chain
    finder = regtest_cluster.miner_address(0)
    block_hash = force(pool, f"{finder}.hybrid", solution_forcer)
    reward, paid = mined(pool, chain, block_hash)

    expected = pool.reward_outputs(reward, finder, hybrid=True)
    pool_wallet = pool.config["addresses"]["pool_wallet"]
    assert [address for address, _ in expected[:2]] == [finder, pool_wallet], "both reward outputs"
    assert paid[: len(expected)] == expected
    # The fees, exactly as a solo block of this reward pays them, then the witness commitment.
    assert expected[2:] == pool.reward_outputs(reward, finder)[1:]
    assert len(paid) == len(expected) + 1 and paid[-1] == (None, 0)
    assert sum(value for _, value in paid) == reward
    # The checked-in config's third: N = floor(R × 33.33%), the pool wallet M − N.
    assert expected[0][1] == reward * 333_300 // 1_000_000

    row = pool.block(block_hash)
    assert (row["pool"], row["worker"]) == ("hybrid", "hybrid")
    assert (row["finder_sats"], row["pool_sats"]) == (expected[0][1], expected[1][1])


def test_a_finders_part_above_the_miner_portion_leaves_no_pool_output(
    pool_factory, regtest_cluster, solution_forcer
):
    pool = pool_factory(overrides={"pools": {"hybrid": {"finder": {"btc": 21_000_000.0}}}})
    chain = regtest_cluster.chain
    finder = regtest_cluster.miner_address(0)
    block_hash = force_v1(pool, f"{finder}.hybrid", solution_forcer)
    reward, paid = mined(pool, chain, block_hash)

    expected = pool.reward_outputs(reward, finder, hybrid=True)
    assert expected == pool.reward_outputs(reward, finder), "as solo pays"
    assert paid[: len(expected)] == expected
    assert pool.config["addresses"]["pool_wallet"] not in [address for address, _ in paid]
    assert sum(value for _, value in paid) == reward
    row = pool.block(block_hash)
    assert (row["finder_sats"], row["pool_sats"]) == (expected[0][1], 0)


def test_solo_and_pooled_blocks_record_what_their_miner_portion_paid(pool, regtest_cluster, solution_forcer):
    chain = regtest_cluster.chain
    finder = regtest_cluster.miner_address(0)
    for port, payee, mode in [
        (pool.solo_v1_port, finder, "solo"),
        (pool.pooled_v1_port, pool.config["addresses"]["pool_wallet"], "pooled"),
    ]:
        miner = Sv1Miner(port, f"{finder}.{mode}")
        _, block_hash = solution_forcer(miner)
        miner.client.close()
        reward, paid = mined(pool, chain, block_hash)
        [(address, portion), *_] = pool.reward_outputs(reward, payee)
        assert paid[0] == (address, portion)
        row = pool.block(block_hash)
        assert row["pool"] == mode
        assert (row["finder_sats"], row["pool_sats"]) == ((portion, 0) if mode == "solo" else (0, portion))


def test_without_hybrid_mode_there_are_no_hybrid_listeners(pool_factory):
    pool = pool_factory(overrides={"pools": {"hybrid": None}})
    assert "hybrid" not in [entry["id"] for entry in pool.config["pools"]]
    [endpoint] = [line for line in pool.logs().splitlines() if "public Stratum endpoint" in line]
    assert "pooled_v2=" in endpoint and "hybrid_v1=" not in endpoint
