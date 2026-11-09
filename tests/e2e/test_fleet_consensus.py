"""Chain-tip consensus across the node fleet: work follows the most-work tip
when a partition heals, the majority's tip on equal work, and a node on another chain is quarantined for the
life of the process. The fork where the taller tip has less work cannot arise on regtest, whose blocks all
carry the same work; unit tests on the fake node hold it (src/node/fleet.rs)."""

import pool as pool_module
from clients.sv1 import Sv1Miner, stratum_prevhash
from fleet import health, selected, standing
from wait import wait_until

STALE = 21


def username(fleet):
    return f"{fleet.nodes[0].miner_address(0)}.e2e"


def knows(node, block_hash):
    """Whether the node holds the block's data, whether or not it is on its active chain."""
    tips = node.chain.call("getchaintips")
    return any(tip["hash"] == block_hash and tip["status"] != "headers-only" for tip in tips)


def test_a_healed_partition_leaves_work_on_the_longer_chain(fleet_pool, regtest_fleet):
    pool, fleet = fleet_pool, regtest_fleet
    primary = fleet.nodes[0]
    wait_until(lambda: selected(pool) == "primary", 30, "work from the primary")
    miner = Sv1Miner(pool.solo_v1_port, username(fleet))

    # The primary, cut off, mines three blocks alone while the other two mine two.
    primary.isolate()
    wait_until(lambda: selected(pool) != "primary", 10, "work from another node")
    private = fleet.mine(0, 3)
    public = fleet.mine(1, 2, among=[1, 2])
    while miner.notify[1] != stratum_prevhash(public[-1]):
        miner.next_job()
    on_public = miner.share()

    primary.reconnect()
    tip = fleet.synced()
    assert tip[1] == private[-1], "the other nodes reorganized onto the longer chain"
    assert miner.clean_job()[1] == stratum_prevhash(private[-1])
    assert miner.rejection(on_public).code == STALE
    assert miner.submit(miner.share()) is True
    wait_until(lambda: selected(pool) == "primary", 30, "the primary back after its window")
    miner.client.close()


def test_equal_work_goes_to_the_majority(fleet_pool, regtest_fleet):
    pool, fleet = fleet_pool, regtest_fleet
    primary = fleet.nodes[0]
    wait_until(lambda: selected(pool) == "primary", 30, "work from the primary")

    # A block apiece at one height: the primary's alone, the other two's shared. Once every node has both,
    # preciousblock, Core's tie-break at equal work, holds each side on its own, so the fork stays.
    primary.isolate()
    [alone] = fleet.mine(0, 1)
    [shared] = fleet.mine(1, 1, among=[1, 2])
    primary.reconnect()
    fleet.connected()
    wait_until(lambda: all(knows(node, alone) and knows(node, shared) for node in fleet.nodes), 30, "both blocks")
    primary.chain.call("preciousblock", alone)
    for node in fleet.nodes[1:]:
        node.chain.call("preciousblock", shared)
    assert [node.chain.tip()[1] for node in fleet.nodes] == [alone, shared, shared]

    # The primary, fit, still yields to the majority's tip.
    wait_until(
        lambda: standing(pool, "primary") == "fit" and selected(pool) in ("node1", "node2"),
        30,
        "work from the majority's tip with the primary fit",
    )
    miner = Sv1Miner(pool.solo_v1_port, username(fleet))
    assert miner.notify[1] == stratum_prevhash(shared)
    wait_until(lambda: "nodes disagree on the tip at equal work" in pool.logs(), 5, "the fork logged")
    miner.client.close()


def test_a_node_on_another_chain_is_quarantined_until_the_pool_restarts_without_it(
    pool_binary, regtest_fleet, tmp_path
):
    fleet = regtest_fleet
    fleet.wrong.up()
    pool = pool_module.Pool(pool_binary, fleet.nodes[0], tmp_path, overrides=fleet.overrides(wrong=True))
    try:
        pool.start()
        for _ in range(2):
            wait_until(lambda: standing(pool, "wrong") == "quarantined", 30, "the signet node quarantined")
            assert selected(pool) == "primary"
            status, body = health(pool)
            assert (status, body["status"]) == (200, "degraded")
            assert body["checks"]["nodes"] == {"fit": 3, "configured": 4}
            assert "quarantined until restarted with the right config" in pool.logs()
            # A restart checks every node's chain again before taking work from it.
            pool.restart()

        # Restarted without it, the fleet is whole.
        pool.config["secondary_nodes"] = fleet.secondaries()
        pool.restart()
        wait_until(lambda: health(pool)[1]["checks"]["nodes"] == {"fit": 3, "configured": 3}, 30, "3 of 3 fit")
        assert health(pool)[1]["status"] == "ok"
    finally:
        pool.kill()
