"""Block lifecycle: a solo block's status follows the chain, from
submitted through confirmed-immature to mature, to orphaned when invalidated and back when restored."""

from clients.sv1 import Sv1Miner
from wait import wait_until


def test_a_solo_block_follows_the_chain(pool, regtest_cluster, miner_username, solution_forcer):
    chain = regtest_cluster.chain
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    _, block_hash = solution_forcer(miner)
    # Done with it: the jobs of the next 100 blocks would pile up unread.
    miner.client.close()

    def status():
        return pool.block(block_hash).get("status")

    wait_until(lambda: status() == "confirmed_immature", 10, "confirmed-immature")
    [changed] = [
        line
        for line in pool.logs().splitlines()
        if "block status changed" in line and block_hash in line
    ]
    assert "from=submitted" in changed and "to=confirmed_immature" in changed

    # Mature once 100 more blocks bury its coinbase: 101 confirmations. The boundary itself, 100
    # confirmations still immature, is the cargo test's.
    wallet = regtest_cluster.address("pool_wallet")
    chain.mine(100, wallet)
    wait_until(lambda: status() == "mature", 30, "mature")

    chain.call("invalidateblock", block_hash)
    try:
        wait_until(lambda: status() == "orphaned", 30, "orphaned")
    finally:
        chain.call("reconsiderblock", block_hash)
    wait_until(lambda: status() == "mature", 30, "mature again")
