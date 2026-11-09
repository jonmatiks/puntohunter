"""Cross-protocol equivalence: the same job worked over V1 and over SV2 is treated
identically by the one core. It compares the coinbase for the same payout, the target assigned, the rejection
class of each case, the block recorded for a forced solution, the telemetry each front-end produces for the
same shares, and, in pooled and hybrid modes, the journal rows, allocation, and ledger entries of the same
work."""

import re
import time

import pytest

from chain import sats
from clients.miner import target
from clients.sv1 import StratumError, Sv1Miner, stratum_prevhash
from clients.sv2 import Sv2Miner
from pplns import reckoned, snapshot
from wait import wait_until

# Between the harness floor and regtest's network difficulty, so shares need not be blocks.
PINNED = 3e-10
# Each V1 rejection class and its SV2 counterpart.
STALE, DUPLICATE, LOW_DIFFICULTY, OTHER = 21, 22, 23, 20
PERIOD = 2


@pytest.fixture
def pinned(pool_factory):
    return pool_factory(overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}})


@pytest.fixture
def pair(pinned, regtest_cluster):
    """A V1 and an SV2 miner with one username, on the solo listeners of one pool."""
    username = f"{regtest_cluster.miner_address(0)}.same"
    v1 = Sv1Miner(pinned.solo_v1_port, username)
    v2 = Sv2Miner(pinned, pinned.solo_v2_port, username)
    yield v1, v2
    v1.client.close()
    v2.close()


def display(internal_hex):
    return bytes.fromhex(internal_hex)[::-1].hex()


def test_the_coinbase_is_byte_identical_apart_from_the_extranonce_prefix(pair):
    v1, v2 = pair
    assert v1.notify[1] == stratum_prevhash(display(v2.job["prev_hash"])), "one template"
    assert v1.notify[2] == v2.job["coinbase_prefix"]
    assert v1.notify[3] == v2.job["coinbase_suffix"]
    assert v1.notify[4] == v2.job["merkle_path"]
    assert int(v1.notify[5], 16) == v2.job["version"]
    assert int(v1.notify[6], 16) == v2.job["nbits"]
    # The prefixes come from one allocator, so the two differ, as any two connections' do.
    assert v1.client.extranonce1 != v2.channel["extranonce_prefix"]
    assert len(bytes.fromhex(v1.client.extranonce1)) == len(bytes.fromhex(v2.channel["extranonce_prefix"]))
    assert v1.client.extranonce2_size == v2.channel["extranonce_size"]


def test_the_same_target_is_assigned(pair):
    v1, v2 = pair
    assert v1.difficulty == PINNED
    # V1 carries the difficulty and SV2 the target the pool derives from it, to f64's 53-bit precision.
    assert v2.job["difficulty"] == pytest.approx(PINNED, rel=1e-12)
    assert int(v2.job["target"], 16) == pytest.approx(target(PINNED), rel=2**-52)


def test_each_case_is_rejected_with_the_same_class(pair, regtest_cluster):
    v1, v2 = pair

    def v1_rejection(params):
        try:
            v1.submit(params)
        except StratumError as e:
            return e.code
        raise AssertionError("accepted")

    def v2_rejection(**args):
        [error] = v2.submit(**args)["errors"]
        return error["error_code"]

    assert v1_rejection(v1.share(meets=False)) == LOW_DIFFICULTY
    assert v2_rejection(kind="low_difficulty") == "difficulty-too-low"

    assert v1_rejection(v1.share(version_bits=0x0000_0001)) == OTHER
    assert v2_rejection(kind="bad_version") == "invalid-non-rollable-version-bit"

    share = v1.share()
    assert v1.submit(share) is True
    assert v1_rejection(share) == DUPLICATE
    assert not v2.submit()["errors"]
    assert v2_rejection(kind="duplicate") == "duplicate-share"

    v1_stale, v2_stale = v1.share(), v2.job["job_id"]
    regtest_cluster.chain.mine(1, regtest_cluster.address("pool_wallet"))
    v1.clean_job()
    v2.client.wait("SetNewPrevHash", channel_id=v2.channel_id)
    v2.next_job()
    assert v1_rejection(v1_stale) == STALE
    assert v2_rejection(kind="low_difficulty", job_id=v2_stale) == "stale-share"


def test_a_forced_block_is_recorded_alike_over_either_protocol(pinned, pair, regtest_cluster, solution_forcer):
    chain = regtest_cluster.chain
    v1, v2 = pair
    _, v1_block = solution_forcer(v1)
    wait_until(lambda: pinned.block(v1_block).get("result") == "accepted", 10, "the V1 block recorded")
    v2.client.wait("SetNewPrevHash", channel_id=v2.channel_id)
    v2.next_job()
    [v2_block] = v2.submit(kind="block")["hashes"]
    wait_until(lambda: pinned.block(v2_block).get("result") == "accepted", 10, "the SV2 block recorded")

    def recorded(block_hash):
        row = pinned.block(block_hash)
        vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
        reward = sum(sats(o["value"]) for o in vout)
        paid = [(o["scriptPubKey"].get("address"), sats(o["value"])) for o in vout]
        return row["pool"], row["status"], reward, paid

    v1_mode, v1_status, v1_reward, v1_paid = recorded(v1_block)
    v2_mode, v2_status, v2_reward, v2_paid = recorded(v2_block)
    assert (v1_mode, v1_status) == (v2_mode, v2_status) == ("solo", "confirmed_immature")
    # One height apart on an empty mempool, the reward is the same subsidy, split the same way.
    assert v1_reward == v2_reward
    assert v1_paid == v2_paid
    for block_hash in (v1_block, v2_block):
        submissions = [l for l in pinned.logs().splitlines() if "submitting block" in l and block_hash in l]
        assert len(submissions) == 1


def resource(pool, handle):
    """The worker's API resource."""
    return pool.portal.api(f"/api/v1/workers/{handle}").json


def logged_handle(pool, event, worker):
    lines = [l for l in pool.logs().splitlines() if event in l and f"worker={worker} " in l]
    return re.search(r"handle=([a-z2-7]{16})", lines[-1])[1]


def test_the_same_shares_produce_the_same_telemetry_over_either_protocol(pool_factory, regtest_cluster):
    pool = pool_factory(
        overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}, "telemetry": {"rollup_secs": PERIOD}}
    )
    address = regtest_cluster.miner_address(0)
    v1 = Sv1Miner(pool.solo_v1_port, f"{address}.v1")
    v2 = Sv2Miner(pool, pool.solo_v2_port, f"{address}.v2")
    handles = {"V1": logged_handle(pool, "V1 authorized", "v1"), "V2-native": logged_handle(pool, "V2 channel opened", "v2")}

    # The same work over each: four shares at the same target, a duplicate, and a miss of the target.
    shares = [v1.share() for _ in range(4)]
    for share in shares:
        assert v1.submit(share) is True
    v1.rejection(shares[0])
    v1.rejection(v1.share(meets=False))
    assert not v2.submit(4)["errors"]
    assert v2.submit(kind="duplicate")["errors"]
    assert v2.submit(kind="low_difficulty")["errors"]

    for transport, handle in handles.items():
        worker = resource(pool, handle)
        assert (worker["transports"], worker["pool"]) == ([transport], "solo")
        live = worker["live"]
        assert (live["accepted"], live["rejected"], live["connections"]) == (4, 2, 1), transport
        assert live["difficulty"] == pytest.approx(PINNED, rel=1e-12)

    def totals(transport):
        rows = pool.rollups(handles[transport])
        return {k: sum(r[k] for r in rows) for k in ("accepted", "work", "duplicate", "low_difficulty", "stale")}

    wait_until(lambda: all(totals(t)["accepted"] == 4 for t in handles), PERIOD * 3, "both workers' rows")
    for transport, handle in handles.items():
        assert {(r["pool"], r["transport"]) for r in pool.rollups(handle)} == {("solo", transport)}
    v1_totals, v2_totals = totals("V1"), totals("V2-native")
    assert v1_totals.pop("work") == pytest.approx(v2_totals.pop("work"), rel=1e-12), "credited at one target"
    assert v1_totals == v2_totals == {"accepted": 4, "duplicate": 1, "low_difficulty": 1, "stale": 0}
    v1.client.close()
    v2.close()


def test_one_username_over_both_protocols_is_one_worker(pinned, pair):
    handle = re.search(r"handle=([a-z2-7]{16})", [l for l in pinned.logs().splitlines() if "V1 authorized" in l][-1])[1]
    worker = resource(pinned, handle)
    assert (worker["live"]["connections"], worker["transports"]) == (2, ["V1", "V2-native"])


@pytest.mark.parametrize("mode", ["pooled", "hybrid"])
def test_the_same_work_over_either_protocol_is_journaled_allocated_and_credited_alike(
    pool_factory, regtest_cluster, solution_forcer, mode
):
    pool = pool_factory(overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}})
    v1_address, v2_address, finder_address = (regtest_cluster.miner_address(n) for n in range(3))
    v1 = Sv1Miner(getattr(pool, f"{mode}_v1_port"), f"{v1_address}.rig")
    v2 = Sv2Miner(pool, getattr(pool, f"{mode}_v2_port"), f"{v2_address}.rig")
    finder = Sv1Miner(getattr(pool, f"{mode}_v1_port"), f"{finder_address}.rig")
    assert v1.notify[1] == stratum_prevhash(display(v2.job["prev_hash"])), "one template"
    for _ in range(4):
        assert v1.submit(v1.share()) is True
    assert sum(ack["accepted"] for ack in v2.submit(4)["acks"]) == 4
    _, block_hash = solution_forcer(finder)
    wait_until(lambda: pool.block(block_hash).get("status") == "confirmed_immature", 10, "the block confirmed")

    # The same job context and assigned target in each journal row, whichever protocol carried the share.
    journal = pool.journal()
    fields = ("pool", "policy", "template", "bits", "miner_sats", "pool_sats", "target")
    context = {
        address: [tuple(r[f] for f in fields) for r in journal if r["address"] == address]
        for address in (v1_address, v2_address)
    }
    assert len(context[v1_address]) == 4 and len(set(context[v1_address])) == 1
    assert context[v1_address] == context[v2_address]

    # The same credit, and so the same allocation but for the satoshi the tie-break gives the lower address.
    snap, allocated = snapshot(pool, block_hash)
    reckoning, credits = reckoned(pool, snap)
    assert allocated == reckoning
    assert credits[v1_address] == credits[v2_address]
    assert abs(allocated[v1_address] - allocated[v2_address]) <= 1

    # And the same ledger entries, each its allocation, pending, in the block's mode.
    entries = pool.rows("SELECT address, pool, kind, sats FROM ledger WHERE block = ? ORDER BY id", block_hash)
    by_address = {e["address"]: (e["pool"], e["kind"], e["sats"]) for e in entries}
    for address in (v1_address, v2_address):
        assert [e["address"] for e in entries].count(address) == 1
        assert by_address[address] == (mode, "pending", allocated[address])
    for client in (v1.client, finder.client):
        client.close()
    v2.close()

