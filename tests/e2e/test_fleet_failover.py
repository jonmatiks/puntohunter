"""The node fleet's failover: the template source killed, hung, or cut off from its
peers hands work to another node within the failover budget, with no miner disconnected; it takes sourcing back
only after its recovery window; and with every node lost the miners are held, as with one node."""

import time

import pytest

from clients.sv1 import Sv1Miner, stratum_prevhash
from fleet import FLEET_TIMINGS, health, selected, standing
from wait import wait_until

STALE = 21
RECOVERY = FLEET_TIMINGS["recovery_secs"]
# What each fault may take to move the work, from the selector's budget: one health check after a refused call, the RPC
# timeout after a hung one, one health check for a node without peers; each with a second for the scrape.
FAULTS = {
    "killed": (lambda node: node.kill(), lambda node: node.start(), 2.0),
    "paused": (lambda node: node.pause(), lambda node: node.unpause(), FLEET_TIMINGS["rpc_timeout_secs"] + 2.0),
    "isolated": (lambda node: node.isolate(), lambda node: node.reconnect(), 2.0),
}


def username(fleet):
    return f"{fleet.nodes[0].miner_address(0)}.e2e"


@pytest.mark.parametrize("fault", FAULTS)
def test_a_failing_template_source_hands_work_to_another_node_without_a_disconnect(
    fleet_pool, regtest_fleet, fault
):
    pool, fleet = fleet_pool, regtest_fleet
    wait_until(lambda: selected(pool) == "primary", 30, "work from the primary")
    assert health(pool)[1]["checks"]["nodes"] == {"fit": 3, "configured": 3}
    miner = Sv1Miner(pool.solo_v1_port, username(fleet))
    tip = fleet.nodes[0].chain.tip()[1]
    assert miner.notify[1] == stratum_prevhash(tip)
    fail, restore, budget = FAULTS[fault]

    failed = time.monotonic()
    fail(fleet.nodes[0])
    try:
        wait_until(lambda: selected(pool) in ("node1", "node2"), budget, "work from another node")
        assert time.monotonic() - failed <= budget
        status, body = health(pool)
        assert (status, body["status"], body["checks"]["node"]) == (200, "degraded", "degraded")
        assert body["checks"]["nodes"] == {"fit": 2, "configured": 3}
        assert miner.client.alive()
        assert miner.submit(miner.share()) is True, "the jobs on the tip stay good"
        # The chain goes on without the primary, and so does the work.
        [block] = fleet.mine(1, among=[1, 2])
        assert miner.clean_job()[1] == stratum_prevhash(block)
        assert miner.submit(miner.share()) is True
    finally:
        restored = time.monotonic()
        restore(fleet.nodes[0])

    wait_until(lambda: selected(pool) == "primary", 60, "the primary taking sourcing back")
    assert time.monotonic() - restored >= RECOVERY, "not before its recovery window"
    assert health(pool)[1]["checks"]["nodes"] == {"fit": 3, "configured": 3}
    assert miner.client.alive()
    assert miner.submit(miner.share()) is True
    miner.client.close()


def test_a_bounced_node_is_not_readmitted_before_its_recovery_window(fleet_pool, regtest_fleet):
    pool, fleet = fleet_pool, regtest_fleet
    primary = fleet.nodes[0]
    wait_until(lambda: selected(pool) == "primary", 30, "work from the primary")
    for _ in range(3):
        primary.isolate()
        wait_until(lambda: standing(pool, "primary") == "isolated", 10, "the primary isolated")
        assert selected(pool) != "primary"
        reconnected = time.monotonic()
        primary.reconnect()
        wait_until(lambda: standing(pool, "primary") == "recovering", 10, "the primary recovering")
        # Bounced again well inside the window, it never gets back in between.
        while time.monotonic() < reconnected + RECOVERY / 2:
            assert selected(pool) != "primary"
            time.sleep(0.2)

    wait_until(lambda: selected(pool) == "primary", 30, "the primary back")
    assert time.monotonic() - reconnected >= RECOVERY, "a full window after the last bounce"


def test_total_loss_holds_the_miner_and_resumes_on_the_new_tip(fleet_pool, regtest_fleet):
    pool, fleet = fleet_pool, regtest_fleet
    wait_until(lambda: selected(pool) == "primary", 30, "work from the primary")
    miner = Sv1Miner(pool.solo_v1_port, username(fleet))
    before = miner.share()

    for node in fleet.nodes:
        node.stop()
    wait_until(lambda: health(pool)[0] == 503, 30, "no work")
    status, body = health(pool)
    assert body["checks"]["work"] is False and body["checks"]["nodes"] == {"fit": 0, "configured": 3}
    assert "no node may source templates; issuing no work" in pool.logs()
    assert miner.client.alive()
    # Nothing has shown the tip stale, so its jobs are still worked.
    assert miner.submit(miner.share()) is True

    # The nodes return on fresh chains, their datadirs being tmpfs: the tip has moved.
    for node in fleet.nodes:
        node.start()
    fleet.connected()
    [block] = fleet.mine(0, 1)
    assert miner.clean_job()[1] == stratum_prevhash(block)
    assert miner.rejection(before).code == STALE
    assert miner.submit(miner.share()) is True
    miner.client.close()
