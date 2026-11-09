"""PPLNS allocation: a pooled or hybrid block's pool output is divided by
work over its mode's journal when its solution share is journaled, each allocation matching an independent
reckoning from the journal rows. Two miners at unequal targets, over V1 and V2, are allocated in
proportion to their work; shares after a solution leave its snapshot unchanged; and a hybrid block's window
holds its finder's work and no pooled or solo work, as a pooled block's holds none of hybrid's."""

import pytest

from clients.miner import header, sha256d, target
from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from pplns import reckoned, size, snapshot, weighted, work
from wait import wait_until

MODES = ["pooled", "hybrid"]
# Two share difficulties between the harness floor and regtest's network difficulty (about 4.7e-10): the
# second miner's work per share is 5/3 of the first's, and neither miner's shares need be blocks.
EASY, HARD = 2.4e-10, 4e-10
STALE, LOW_DIFFICULTY = 21, 23


def port(pool, mode, transport):
    return getattr(pool, f"{mode}_{transport}_port")


@pytest.mark.parametrize("finder", ["V1", "V2"])
@pytest.mark.parametrize("mode", MODES)
def test_two_miners_at_unequal_targets_are_allocated_by_work_and_later_shares_change_nothing(
    pool_factory, regtest_cluster, solution_forcer, mode, finder
):
    pool = pool_factory(overrides={"vardiff": {"floor": EASY, "ceiling": HARD}})
    easy_address, hard_address = regtest_cluster.miner_address(0), regtest_cluster.miner_address(1)
    # Fewer shares than vardiff retargets on, so each keeps its starting difficulty: V2 the floor, V1 its d=.
    easy = Sv2Miner(pool, port(pool, mode, "v2"), f"{easy_address}.easy")
    hard = Sv1Miner(port(pool, mode, "v1"), f"{hard_address}.hard", password=f"d={HARD}")
    assert hard.difficulty == HARD
    assert easy.submit(count=4)["acks"]
    for _ in range(4):
        assert hard.submit(hard.share()) is True

    if finder == "V1":
        _, block_hash = solution_forcer(hard)
    else:
        [block_hash] = easy.submit(kind="block")["hashes"]
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block answered")
    snap, allocated = snapshot(pool, block_hash)
    row = pool.block(block_hash)
    assert (snap["pool"], snap["cutoff"], snap["amount"]) == (mode, row["cutoff"], row["pool_sats"])
    expected, credits = reckoned(pool, snap)
    assert allocated == expected
    assert sum(allocated.values()) == row["pool_sats"] > 0
    # The window outgrows regtest's journal: everything is credited, at each share's own target.
    assert snap["first"] == pool.journal()[0]["sequence"]
    per_share = {
        address: {work(r["target"]) for r in pool.journal() if r["address"] == address}
        for address in (easy_address, hard_address)
    }
    assert all(len(works) == 1 for works in per_share.values()), "each miner kept its target"
    [easy_work], [hard_work] = per_share[easy_address], per_share[hard_address]
    assert hard_work > easy_work
    assert allocated[hard_address] / allocated[easy_address] == pytest.approx(
        credits[hard_address] / credits[easy_address], rel=1e-6
    )

    # Shares after the solution, and another block, leave the first block's snapshot as it was.
    # Each miner takes the job on the new tip first.
    hard.clean_job()
    easy.next_job()
    for _ in range(3):
        assert hard.submit(hard.share()) is True
    assert easy.submit(count=3)["acks"]
    _, second = solution_forcer(hard)
    wait_until(lambda: pool.block(second).get("result") == "accepted", 10, "the second block answered")
    later, _ = snapshot(pool, second)
    assert later["cutoff"] > snap["cutoff"] and int(later["total"]) > int(snap["total"])
    assert snapshot(pool, block_hash) == (snap, allocated)
    easy.close()
    hard.client.close()


def test_each_block_allocates_its_own_modes_work_only(pool_factory, regtest_cluster, solution_forcer):
    pool = pool_factory()
    addresses = [regtest_cluster.miner_address(n) for n in range(4)]
    finder = Sv1Miner(pool.hybrid_v1_port, f"{addresses[0]}.finder")
    other_hybrid = Sv2Miner(pool, pool.hybrid_v2_port, f"{addresses[1]}.hybrid")
    pooled = Sv2Miner(pool, pool.pooled_v2_port, f"{addresses[2]}.pooled")
    solo = Sv1Miner(pool.solo_v1_port, f"{addresses[3]}.solo")
    for _ in range(3):
        assert finder.submit(finder.share()) is True
        assert solo.submit(solo.share()) is True
    assert other_hybrid.submit(count=3)["acks"]
    assert pooled.submit(count=3)["acks"]

    _, hybrid_block = solution_forcer(finder)
    wait_until(lambda: pool.block(hybrid_block).get("result") == "accepted", 10, "the hybrid block answered")
    snap, allocated = snapshot(pool, hybrid_block)
    assert snap["pool"] == "hybrid"
    assert set(allocated) == {addresses[0], addresses[1]}, "the finder's work and the other hybrid miner's"
    assert allocated == reckoned(pool, snap)[0]
    assert sum(allocated.values()) == pool.block(hybrid_block)["pool_sats"]

    [pooled_block] = pooled.submit(kind="block")["hashes"]
    wait_until(lambda: pool.block(pooled_block).get("result") == "accepted", 10, "the pooled block answered")
    snap, allocated = snapshot(pool, pooled_block)
    assert snap["pool"] == "pooled"
    assert set(allocated) == {addresses[2]}, "no hybrid or solo work"
    assert allocated[addresses[2]] == pool.block(pooled_block)["pool_sats"]
    for client in (finder.client, solo.client):
        client.close()
    other_hybrid.close()
    pooled.close()


def test_a_hybrid_block_with_no_pool_output_allocates_nothing(pool_factory, regtest_cluster, solution_forcer):
    pool = pool_factory(overrides={"pools": {"hybrid": {"finder": {"btc": 21_000_000.0}}}})
    finder = Sv1Miner(pool.hybrid_v1_port, f"{regtest_cluster.miner_address(0)}.finder")
    assert finder.submit(finder.share()) is True
    _, block_hash = solution_forcer(finder)
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block answered")
    # Its jobs pay no pool output, so its shares earn from no window and are not journaled: the block has no window to snapshot, and allocates nothing.
    assert pool.block(block_hash)["pool_sats"] == 0
    assert pool.journal() == []
    assert pool.rows("SELECT * FROM snapshots") == [] and pool.rows("SELECT * FROM allocations") == []
    finder.client.close()


def send(miner, params):
    """Sends a submit without waiting for its answer, and returns its request id."""
    request_id = miner.client.next_id
    miner.client.next_id += 1
    miner.client.send({"id": request_id, "method": "mining.submit", "params": [miner.username, *params]})
    return request_id


def answer(miner, request_id):
    """The result of request `request_id`, queueing the notifications that come first."""
    while True:
        message = miner.client.read()
        if message.get("id") == request_id and "method" not in message:
            return message["result"]
        miner.client.notifications.append(message)


def test_one_address_in_both_modes_and_blocks_found_together_never_cross_credit(
    pool_factory, regtest_cluster
):
    pool = pool_factory(overrides={"vardiff": {"floor": EASY, "ceiling": HARD}})
    shared, pooled_only, hybrid_only = (regtest_cluster.miner_address(n) for n in range(3))
    # One address mining both modes, at different targets and over different transports, beside a finder of
    # each mode on an address of its own.
    shared_pooled = Sv1Miner(pool.pooled_v1_port, f"{shared}.rig", password=f"d={HARD}")
    shared_hybrid = Sv2Miner(pool, pool.hybrid_v2_port, f"{shared}.rig")
    finders = {
        "pooled": Sv1Miner(pool.pooled_v1_port, f"{pooled_only}.rig"),
        "hybrid": Sv1Miner(pool.hybrid_v1_port, f"{hybrid_only}.rig"),
    }
    for _ in range(4):
        assert shared_pooled.submit(shared_pooled.share()) is True
    assert sum(ack["accepted"] for ack in shared_hybrid.submit(count=4)["acks"]) == 4
    assert {row["pool"] for row in pool.journal() if row["address"] == shared} == {"pooled", "hybrid"}

    # A block in each mode on one template, sent together: the node keeps one on its chain and the other
    # beside it, and the pool records both.
    assert finders["pooled"].notify[1] == finders["hybrid"].notify[1], "one template"
    found = {mode: finder.generator.block(finder.notify, finder.difficulty) for mode, finder in finders.items()}
    requests = {mode: send(finders[mode], params) for mode, (params, _) in found.items()}
    assert all(answer(finders[mode], request_id) is True for mode, request_id in requests.items())
    blocks = {mode: block_hash for mode, (_, block_hash) in found.items()}
    for block_hash in blocks.values():
        wait_until(lambda: pool.block(block_hash).get("result") is not None, 10, "the block answered")
        assert pool.block(block_hash)["status"] != "rejected"

    expected = {"pooled": {shared, pooled_only}, "hybrid": {shared, hybrid_only}}
    for mode, block_hash in blocks.items():
        snap, allocated = snapshot(pool, block_hash)
        assert (snap["pool"], pool.block(block_hash)["pool"]) == (mode, mode)
        assert set(allocated) == expected[mode], "no work of the other mode"
        reckoning, credits = reckoned(pool, snap)
        assert allocated == reckoning
        own = sum(weighted(r, 2) for r in pool.journal() if (r["address"], r["pool"]) == (shared, mode))
        assert credits[shared] == own, "the shared address is credited its work in this mode alone"
        ledger = pool.rows("SELECT DISTINCT pool FROM ledger WHERE block = ?", block_hash)
        assert [row["pool"] for row in ledger] == [mode]
    for miner in (shared_pooled, *finders.values()):
        miner.client.close()
    shared_hybrid.close()


def hash_on(miner, params):
    """The header hash of V1 submit `params` on `miner`'s current job and extranonce prefix, as an integer."""
    extranonce = bytes.fromhex(miner.client.extranonce1) + bytes.fromhex(params[1])
    data = header(miner.notify, extranonce, int(params[2], 16), int(params[3], 16))
    return int.from_bytes(sha256d(data), "little")


def test_switching_endpoints_never_reinterprets_an_outstanding_job(pool_factory, regtest_cluster):
    pool = pool_factory(overrides={"vardiff": {"floor": EASY, "ceiling": EASY}})
    username = f"{regtest_cluster.miner_address(0)}.rig"
    pooled = Sv1Miner(pool.pooled_v1_port, username)
    hybrid = Sv1Miner(pool.hybrid_v1_port, username)
    # The same job id on one template at either endpoint. A share found on the pooled job, sent to the hybrid
    # endpoint, is checked against the hybrid job, whose coinbase and prefix differ: one that misses there is
    # refused, not credited as the pooled work it was.
    assert (pooled.notify[0], pooled.notify[1]) == (hybrid.notify[0], hybrid.notify[1])
    while hash_on(hybrid, share := pooled.share()) <= target(hybrid.difficulty):
        pass
    proof = hash_on(pooled, share)
    assert hybrid.rejection(share).code == LOW_DIFFICULTY
    assert pooled.submit(share) is True

    # Reconnected to the hybrid endpoint, the miner cannot send the pooled session's outstanding job there.
    regtest_cluster.chain.mine(1, regtest_cluster.address("pool_wallet"))
    pooled.clean_job()
    outstanding = pooled.share()
    pooled.client.close()
    hybrid.client.close()
    switched = Sv1Miner(pool.hybrid_v1_port, username)
    assert switched.notify[0] != outstanding[0]
    assert switched.rejection(outstanding).code == STALE
    assert switched.submit(switched.share()) is True

    journal = pool.journal()
    assert [row["pool"] for row in journal] == ["pooled", "hybrid"]
    assert int(journal[0]["proof"], 16) == proof
    switched.client.close()


@pytest.mark.parametrize("mode", MODES)
def test_a_retarget_changes_credit_from_its_own_job_on_and_never_before(
    pool_factory, regtest_cluster, solution_forcer, mode
):
    pool = pool_factory(overrides={"vardiff": {"floor": EASY, "ceiling": HARD}})
    v1_address, v2_address = regtest_cluster.miner_address(0), regtest_cluster.miner_address(1)
    v1 = Sv1Miner(port(pool, mode, "v1"), f"{v1_address}.rig")
    v2 = Sv2Miner(pool, port(pool, mode, "v2"), f"{v2_address}.rig")
    # Shares far faster than the interval: vardiff raises each miner to the ceiling on its tenth. Until the V1
    # miner reads the new job, it works the old one, as do the V2 shares sent in one batch past the tenth.
    old, before = v1.notify, 0
    while not any(m.get("method") == "mining.notify" for m in v1.client.notifications):
        assert v1.submit(v1.share()) is True
        before += 1
    late = v1.generator.share(old, EASY)
    v1.next_job()
    assert v1.difficulty == HARD
    assert v1.submit(late) is True, "a share on the old job, sent after the retarget"
    for _ in range(2):
        assert v1.submit(v1.share()) is True
    assert sum(ack["accepted"] for ack in v2.submit(count=12)["acks"]) == 12
    raised = v2.client.wait("SetTarget", channel_id=v2.channel_id)
    assert v2.next_job()["target"] == raised["target"]
    assert sum(ack["accepted"] for ack in v2.submit(count=2)["acks"]) == 2
    _, block_hash = solution_forcer(v1)
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block answered")

    # Each share is credited at its job's target: the easier one up to the retarget, the harder one after.
    journal = pool.journal()

    def works(address):
        return [work(row["target"]) for row in journal if row["address"] == address]

    easy, hard = works(v1_address)[0], works(v1_address)[-1]
    # Work is counted in 2^-16 hashes, and a share here is about one hash, so the ratio holds to 1e-4.
    assert hard / easy == pytest.approx(HARD / EASY, rel=1e-4)
    assert works(v1_address) == [easy] * (before + 1) + [hard] * 3, "the solution at its job's target too"
    assert works(v2_address) == [easy] * 12 + [hard] * 2
    snap, allocated = snapshot(pool, block_hash)
    reckoning, credits = reckoned(pool, snap)
    assert allocated == reckoning
    # Each share weighted by its job's contribution under policy 2: all of it pooled, two thirds hybrid.
    assert credits == {
        address: sum(weighted(row, 2) for row in journal if row["address"] == address)
        for address in (v1_address, v2_address)
    }
    assert all(row["pool_sats"] == row["miner_sats"] for row in journal) == (mode == "pooled")
    v1.client.close()
    v2.close()


def test_a_policy_change_applies_to_jobs_issued_after_it(pool_factory, regtest_cluster, solution_forcer):
    # W_min 0 leaves W at k times regtest's network work, a few shares: each window ends inside the journal, at
    # an oldest share credited in part.
    pool = pool_factory(
        overrides={
            "vardiff": {"floor": EASY, "ceiling": EASY},
            "accounting": {"window_multiplier": 2.0, "window_min_work": 0.0},
        }
    )
    addresses = [regtest_cluster.miner_address(n) for n in range(2)]

    def mine(count):
        miners = [Sv1Miner(pool.pooled_v1_port, f"{address}.rig") for address in addresses]
        for _ in range(count):
            for miner in miners:
                assert miner.submit(miner.share()) is True
        return miners

    def found(miner):
        _, block_hash = solution_forcer(miner)
        wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block answered")
        return block_hash

    miners = mine(3)
    first = found(miners[0])
    snap, allocated = snapshot(pool, first)
    journal = pool.journal()
    bits = journal[-1]["bits"]
    assert int(snap["size"]) == size(2.0, 0.0, bits)
    [oldest] = [row for row in journal if row["sequence"] == snap["first"]]
    assert oldest["sequence"] > journal[0]["sequence"], "the window ends inside the journal"
    assert 0 < int(snap["boundary"]) < work(oldest["target"]), "its oldest share credited in part"
    assert allocated == reckoned(pool, snap)[0]

    # A job of the old policy is outstanding when the pool restarts under a larger k. It is not credited under
    # the new policy, or at all.
    miners[1].clean_job()
    outstanding = miners[1].share()
    pool.config["accounting"]["window_multiplier"] = 5.0
    pool.restart()
    earlier = len(pool.journal())
    miners = mine(2)
    assert miners[1].notify[0] != outstanding[0]
    assert miners[1].rejection(outstanding).code == STALE
    second = found(miners[1])

    rows = pool.rows("SELECT * FROM policies")
    policies = {row["id"]: (row["window_multiplier"], row["window_min_work"]) for row in rows}
    journal = pool.journal()
    assert {policies[row["policy"]] for row in journal[:earlier]} == {(2.0, 0.0)}
    assert {policies[row["policy"]] for row in journal[earlier:]} == {(5.0, 0.0)}
    # The new block's W is its own job's, over shares of either policy; the old block's snapshot stands.
    later, later_allocated = snapshot(pool, second)
    assert int(later["size"]) == size(5.0, 0.0, bits)
    assert later["first"] <= journal[earlier - 1]["sequence"], "the window reaches back past the restart"
    assert later_allocated == reckoned(pool, later)[0]
    assert snapshot(pool, first) == (snap, allocated)
    for miner in miners:
        miner.client.close()
