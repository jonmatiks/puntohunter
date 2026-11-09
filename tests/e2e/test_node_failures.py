"""The single node's failures: a stopped node, a hung one, and a reorg. Each keeps
every miner connected, work resumes on recovery, and shares against jobs the tip left behind are stale."""

from clients.sv1 import Sv1Miner, stratum_prevhash
from wait import wait_until

STALE = 21
# Short enough that a frozen node is found hung within seconds.
FAST_TIMEOUTS = {"node": {"rpc_timeout_secs": 1.0, "longpoll_timeout_secs": 2.0}}


def test_total_loss_holds_the_miner_and_resumes_on_the_new_tip(
    pool_factory, fresh_node, miner_username
):
    pool = pool_factory(node=fresh_node, overrides=FAST_TIMEOUTS)
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    before = miner.share()

    fresh_node.stop()
    wait_until(lambda: "node refused the connection" in pool.logs(), 30, "the outage logged")
    assert miner.client.alive()
    # Nothing has shown the tip stale, so its jobs are still worked.
    assert miner.submit(miner.share()) is True

    # The node returns on a fresh chain, its datadir being tmpfs: the tip has moved.
    fresh_node.up()
    fresh_node.chain.mine(1, fresh_node.address("pool_wallet"))
    notify = miner.clean_job()
    assert notify[1] == stratum_prevhash(fresh_node.chain.tip()[1])
    assert miner.rejection(before).code == STALE
    assert miner.submit(miner.share()) is True
    miner.client.close()


def test_a_hung_node_holds_the_miner_and_a_block_found_meanwhile_waits(
    pool_factory, fresh_node, miner_username, solution_forcer
):
    pool = pool_factory(node=fresh_node, overrides=FAST_TIMEOUTS)
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    before = miner.share()

    fresh_node.pause()
    try:
        wait_until(lambda: "node hung" in pool.logs(), 30, "the hung node logged")
        assert "node refused the connection" not in pool.logs()
        assert miner.client.alive()
        assert miner.submit(miner.share()) is True
        _, block_hash = solution_forcer(miner)
        wait_until(
            lambda: "block submission unanswered" in pool.logs(), 10, "the unanswered submission"
        )
    finally:
        fresh_node.unpause()

    # The block reaches the node once it answers again, and work resumes on it.
    wait_until(lambda: fresh_node.chain.tip()[1] == block_hash, 30, "the block at the tip")
    wait_until(
        lambda: pool.block(block_hash).get("result") in ("accepted", "duplicate"),
        30,
        "the answer recorded",
    )
    assert miner.clean_job()[1] == stratum_prevhash(block_hash)
    assert miner.rejection(before).code == STALE
    assert miner.submit(miner.share()) is True
    assert "CRITICAL" not in pool.logs()
    miner.client.close()


def test_a_reorg_invalidates_every_job_at_once(pool, regtest_cluster, miner_username):
    chain = regtest_cluster.chain
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [block] = chain.mine(1, regtest_cluster.address("pool_wallet"))
    while miner.notify[1] != stratum_prevhash(block):
        miner.next_job()
    on_block = miner.share()

    chain.call("invalidateblock", block)
    try:
        parent = chain.tip()[1]
        assert miner.clean_job()[1] == stratum_prevhash(parent)
        assert miner.rejection(on_block).code == STALE
        assert miner.client.alive()
        assert miner.submit(miner.share()) is True
    finally:
        chain.call("reconsiderblock", block)
        miner.client.close()
