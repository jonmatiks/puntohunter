"""Configurable pools: a fourth pool beside the three default ones, paying
its finder a fixed amount in sats and with fees of its own, whose blocks' coinbases pay exactly its finder, its
pool output, and its fees, over V1 and V2; with a window policy of its own, its snapshots sized by its own k
while the other pools' follow the global one, and no window holding another pool's shares; and pools serving
Stratum V1 only or V2 only, each mining over its one protocol with no listener for the other. The pool resource
lists every pool in config order, with its kind; a fleet in the fourth pool has its section and chart line, and its
block, fleet, and worker still show by id once the pool is removed from the config."""

import re

import pytest

from chain import sats
from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from pplns import reckoned, size, snapshot
from wait import wait_until

# Between the harness floor and regtest's network difficulty: a share takes a few hashes and need not be a
# block.
PINNED = 3e-10


def bonus(regtest_cluster):
    """A fourth pool: 0.5 BTC to each block's finder, the rest shared, and fees of its own in place of the
    global ones, a fixed one first."""
    return {
        "name": "Finder bonus",
        "v1": True,
        "v2": True,
        "finder": {"sats": 50_000_000},
        "fees": [
            {"name": "host", "address": regtest_cluster.miner_address(3), "sats": 1_000_000},
            {"name": "partner", "address": regtest_cluster.miner_address(4), "percent": 2.0},
        ],
    }


def mined(pool, chain, block_hash):
    """The block's reward and coinbase outputs, as (address, sats), once the pool has its answer."""
    wait_until(lambda: chain.tip()[1] == block_hash, 10, "the block at the tip")
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "its answer recorded")
    stats = chain.call("getblockstats", block_hash, ["subsidy", "totalfee"])
    vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
    paid = [(o["scriptPubKey"].get("address"), sats(o["value"])) for o in vout]
    return stats["subsidy"] + stats["totalfee"], paid


def force_v1(pool, username, solution_forcer):
    miner = Sv1Miner(pool.port("bonus", "v1"), username)
    try:
        return solution_forcer(miner)[1]
    finally:
        miner.client.close()


def force_v2(pool, username, _):
    miner = Sv2Miner(pool, pool.port("bonus", "v2"), username)
    try:
        [block_hash] = miner.submit(kind="block")["hashes"]
        return block_hash
    finally:
        miner.close()


@pytest.mark.parametrize("force", [force_v1, force_v2], ids=["V1", "V2"])
def test_a_fourth_pool_pays_its_fixed_finder_its_pool_output_and_its_own_fees(
    pool_factory, regtest_cluster, solution_forcer, force
):
    pool = pool_factory(overrides={"pools": {"bonus": bonus(regtest_cluster)}})
    assert [entry["id"] for entry in pool.config["pools"]] == ["solo", "pooled", "hybrid", "bonus"]
    chain = regtest_cluster.chain
    finder = regtest_cluster.miner_address(0)
    block_hash = force(pool, f"{finder}.bonus", solution_forcer)
    reward, paid = mined(pool, chain, block_hash)

    expected = pool.reward_outputs(reward, finder, pool="bonus")
    host, partner = regtest_cluster.miner_address(3), regtest_cluster.miner_address(4)
    pool_wallet = pool.config["addresses"]["pool_wallet"]
    assert [address for address, _ in expected] == [finder, pool_wallet, host, partner]
    assert paid[: len(expected)] == expected
    assert len(paid) == len(expected) + 1 and paid[-1] == (None, 0), "then the witness commitment"
    assert sum(value for _, value in paid) == reward
    # The pool's own fees, the fixed one first; not the global 0.9% and 0.6%.
    assert expected[2:] == [(host, 1_000_000), (partner, reward * 20_000 // 1_000_000)]
    assert expected[0] == (finder, 50_000_000)

    row = pool.block(block_hash)
    assert (row["pool"], row["finder_sats"], row["pool_sats"]) == ("bonus", expected[0][1], expected[1][1])
    # Its solution share is journaled under the pool, with the job's split.
    [solution] = wait_until(
        lambda: [r for r in pool.journal() if r["proof"] == block_hash], 10, "the solution share journaled"
    )
    assert (solution["pool"], solution["miner_sats"], solution["pool_sats"]) == (
        "bonus",
        expected[0][1] + expected[1][1],
        expected[1][1],
    )


def test_a_pool_serves_v1_only_or_v2_only(pool_factory, regtest_cluster):
    pool = pool_factory(
        overrides={
            "vardiff": {"floor": PINNED, "ceiling": PINNED},
            "pools": {
                "v1-only": {"name": "V1 only", "v1": True, "finder": {"percent": 100}},
                "v2-only": {"name": "V2 only", "v2": True, "finder": {"percent": 0}},
            },
        }
    )
    assert ("v1-only", "v1") in pool.ports and ("v1-only", "v2") not in pool.ports
    assert ("v2-only", "v2") in pool.ports and ("v2-only", "v1") not in pool.ports
    logs = pool.logs()
    listening = {
        (protocol, id) for protocol, id in re.findall(r"(V[12]) listening bind=\S+ pool=(\S+)", logs)
    }
    assert {("V1", "v1-only"), ("V2", "v2-only")} <= listening
    assert ("V2", "v1-only") not in listening and ("V1", "v2-only") not in listening
    [endpoint] = [line for line in logs.splitlines() if "public Stratum endpoint" in line]
    assert "v1-only_v1=" in endpoint and "v1-only_v2=" not in endpoint
    assert "v2-only_v2=" in endpoint and "v2-only_v1=" not in endpoint
    # The workers-connected gauge keeps a series only for the transports each pool serves.
    gauged = set(re.findall(r'puntohunter_workers_connected\{(?=[^}]*pool="([^"]+)")(?=[^}]*transport="([^"]+)")', pool.metrics()))
    assert {("v1-only", "V1"), ("v2-only", "V2-native")} <= gauged
    assert not {("v1-only", "V2-native"), ("v2-only", "V1")} & gauged

    # Each mines over its one protocol: a share accepted, the V2-only pool's journaled under it.
    username = f"{regtest_cluster.miner_address(0)}.single"
    v1 = Sv1Miner(pool.port("v1-only", "v1"), username)
    assert v1.submit(v1.share()) is True
    v1.client.close()
    v2 = Sv2Miner(pool, pool.port("v2-only", "v2"), username)
    assert sum(ack["accepted"] for ack in v2.submit(count=2)["acks"]) == 2
    v2.close()
    assert {row["pool"] for row in pool.journal()} == {"v2-only"}, "the V1-only pool is solo"


def test_a_pools_own_window_policy_sizes_its_windows_which_hold_its_shares_alone(
    pool_factory, regtest_cluster, solution_forcer
):
    # Its own k, with no floor, so its window is k times the network's work; the others keep W_min's.
    own = {"window_multiplier": 4.0, "window_min_work": 0.0}
    pool = pool_factory(
        overrides={
            "vardiff": {"floor": PINNED, "ceiling": PINNED},
            "pools": {"bonus": {**bonus(regtest_cluster), "accounting": own}},
        }
    )
    shared, pooled_only, bonus_only = (regtest_cluster.miner_address(n) for n in range(3))
    # One address in both pools, beside a finder of each on an address of its own.
    miners = {
        "pooled": Sv1Miner(pool.pooled_v1_port, f"{shared}.rig"),
        "bonus": Sv1Miner(pool.port("bonus", "v1"), f"{shared}.rig"),
    }
    for miner in miners.values():
        for _ in range(3):
            assert miner.submit(miner.share()) is True
    blocks = {}
    for id, finder in [("pooled", pooled_only), ("bonus", bonus_only)]:
        miner = Sv1Miner(pool.port(id, "v1"), f"{finder}.rig")
        _, blocks[id] = solution_forcer(miner)
        wait_until(lambda: pool.block(blocks[id]).get("result") == "accepted", 10, "the block answered")
        miner.client.close()

    journal = pool.journal()
    policies = {id: {row["policy"] for row in journal if row["pool"] == id} for id in blocks}
    assert len(policies["pooled"]) == len(policies["bonus"]) == 1
    assert policies["pooled"] != policies["bonus"], "a policy row of its own"
    expected = {
        "pooled": ({shared, pooled_only}, (2.0, 1_000_000.0)),
        "bonus": ({shared, bonus_only}, (4.0, 0.0)),
    }
    for id, block_hash in blocks.items():
        snap, allocated = snapshot(pool, block_hash)
        [solution] = [row for row in journal if row["proof"] == block_hash]
        addresses, (k, min_work) = expected[id]
        assert snap["pool"] == id
        assert int(snap["size"]) == size(k, min_work, solution["bits"]), f"{id}'s W from its own policy"
        assert set(allocated) <= addresses and allocated, "no work of the other pool"
        assert allocated == reckoned(pool, snap)[0]
    assert int(snapshot(pool, blocks["bonus"])[0]["size"]) < int(snapshot(pool, blocks["pooled"])[0]["size"])
    for miner in miners.values():
        miner.client.close()


def test_the_pool_resource_lists_every_pool_in_config_order(pool_factory, regtest_cluster):
    pool = pool_factory(
        overrides={
            "pools": {
                "bonus": bonus(regtest_cluster),
                "v1-only": {"name": "V1 only", "v1": True, "finder": {"percent": 100}},
            }
        }
    )
    body = pool.portal.api("/api/v1/pool").json
    assert [(setup["id"], setup["name"], setup["kind"]) for setup in body["pools"]] == [
        ("solo", "Solo", "solo"), ("pooled", "Pooled", "pooled"), ("hybrid", "Hybrid", "hybrid"),
        ("bonus", "Finder bonus", "hybrid"), ("v1-only", "V1 only", "solo"),
    ]
    *_, fourth, v1_only = body["pools"]
    key = pool.authority_key()
    assert fourth == {
        "id": "bonus",
        "name": "Finder bonus",
        "kind": "hybrid",
        "stratum_v1": f"stratum+tcp://127.0.0.1:{pool.port('bonus', 'v1')}",
        "stratum_v2": f"stratum2+tcp://127.0.0.1:{pool.port('bonus', 'v2')}/{key}",
        "finder": {"sats": 50_000_000},
        "miner": None,
        "fees": [
            {"name": "host", "portion": {"sats": 1_000_000}},
            {"name": "partner", "portion": {"ppm": 20_000}},
        ],
        "window": {"window_multiplier": 2.0, "window_min_work": 1_000_000.0, "window_size": 1_000_000.0},
        "personal": None,
        "worker_names": False,
        "worker_names_opt_in": False,
        "hash_rate": 0,
        "devices": 0,
    }
    assert (v1_only["kind"], v1_only["stratum_v2"], v1_only["window"]) == ("solo", None, None)
    assert v1_only["stratum_v1"] == f"stratum+tcp://127.0.0.1:{pool.port('v1-only', 'v1')}"
    assert (v1_only["finder"], v1_only["miner"]) == ({"ppm": 1_000_000}, {"ppm": 985_000}), "98.5% to the finder"


def test_a_fleet_in_the_fourth_pool_has_its_section_and_line_and_its_block_lists_after_the_pool_goes(
    pool_factory, regtest_cluster, solution_forcer
):
    pool = pool_factory(
        overrides={"telemetry": {"rollup_secs": 2}, "pools": {"bonus": bonus(regtest_cluster)}}
    )
    finder = regtest_cluster.miner_address(0)
    miner = Sv1Miner(pool.port("bonus", "v1"), f"{finder}.bonus-rig")
    assert miner.submit(miner.share()) is True
    _, block_hash = solution_forcer(miner)
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block answered")
    [handle] = re.findall(r"V1 authorized.*handle=([a-z2-7]{16})", pool.logs())
    [fleet] = {r["fleet"] for r in pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle)}
    wait_until(lambda: pool.rollups(handle), 10, "the rollup rows")
    miner.client.close()

    resource = pool.portal.api(f"/api/v1/fleets/{fleet}").json
    [section] = resource["pools"]
    assert (section["pool"], section["name"]) == ("bonus", "Finder bonus")
    assert section["solo"]["finder_payout"] is True, "it pays a finder, and a pool output beside"
    [line] = pool.portal.api(f"/api/v1/fleets/{fleet}/series").json["pools"]
    assert (line["pool"], line["name"]) == ("bonus", "Finder bonus") and sum(line["accepted"]) >= 1

    def listed():
        [block] = [b for b in pool.portal.api("/api/v1/blocks").json["blocks"] if b["hash"] == block_hash]
        return block

    block = listed()
    assert block["pool"] == "bonus" and block["finder_sats"] == 50_000_000 and block["pool_sats"] > 0
    assert block["transport"] == "V1"
    # Each resource names the pool by its id, which the pool resource names.
    worker = pool.portal.api(f"/api/v1/workers/{handle}").json
    assert (worker["pool"], worker["transports"]) == ("bonus", ["V1"])
    [device] = resource["devices"]
    assert (device["pool"], device["transports"]) == ("bonus", ["V1"]), "its device's row"

    # The pool removed and the process restarted: its block, its fleet section, and its worker show by id.
    pool.config["pools"] = [entry for entry in pool.config["pools"] if entry["id"] != "bonus"]
    pool.restart()
    assert [setup["id"] for setup in pool.portal.api("/api/v1/pool").json["pools"]] == ["solo", "pooled", "hybrid"]
    assert listed() == {**block, "blocks_to_maturity": listed()["blocks_to_maturity"]}, "from its own amounts"
    [section] = pool.portal.api(f"/api/v1/fleets/{fleet}").json["pools"]
    assert (section["pool"], section["name"], section["solo"], section["window"]) == ("bonus", "bonus", None, None)
    assert pool.portal.api(f"/api/v1/workers/{handle}").json["pool"] == "bonus"
    [line] = pool.portal.api(f"/api/v1/fleets/{fleet}/series").json["pools"]
    assert (line["pool"], line["name"]) == ("bonus", "bonus")
    found = pool.portal.api("/api/v1/lookup", method="POST", body={"username": f"{finder}.bonus-rig"})
    assert found.json == {"fleet": fleet}, "still found"


def test_a_block_keeps_what_it_paid_after_its_pools_finder_changes(pool_factory, regtest_cluster, solution_forcer):
    pool = pool_factory()
    finder = regtest_cluster.miner_address(0)
    miner = Sv1Miner(pool.port("hybrid", "v1"), f"{finder}.hybrid")
    _, block_hash = solution_forcer(miner)
    miner.client.close()
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block answered")
    row = pool.block(block_hash)
    assert row["finder_sats"] > 0 and row["pool_sats"] > 0

    def listed():
        [block] = [b for b in pool.portal.api("/api/v1/blocks").json["blocks"] if b["hash"] == block_hash]
        return block["finder_sats"], block["pool_sats"]

    # The pool no longer pays a pool output, then no longer a finder: the block shows both amounts it paid.
    for percent in (100, 0):
        pool.pool("hybrid")["finder"] = {"percent": percent}
        pool.restart()
        assert listed() == (row["finder_sats"], row["pool_sats"]), percent
