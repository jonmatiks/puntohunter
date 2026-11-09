"""SV2 extended shares: a run of shares reconciles exactly
against the batched acknowledgements, each rejection class has its own error, a mid-session SetTarget holds
from the next share, and a block found over SV2 is submitted and recorded once with V1's split."""

import pytest

from chain import sats
from clients.sv2 import Sv2Miner
from wait import wait_until

# Between the harness floor and regtest's network difficulty (about 4.7e-10), so a share takes a few hashes
# and need not be a block.
PINNED = 3e-10
# The ceiling for vardiff tests: below regtest's network difficulty, so a raised target still leaves shares
# that are not blocks.
CEILING = 4.5e-10


@pytest.fixture
def pinned(pool_factory, miner_username):
    pool = pool_factory(overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}})
    miner = Sv2Miner(pool, pool.solo_v2_port, miner_username)
    yield miner
    miner.close()


def reconcile(result):
    """Checks a submit's acknowledgements against what it sent, exactly, and returns them."""
    acks, errors = result["acks"], result["errors"]
    assert not errors, errors
    assert sum(ack["accepted"] for ack in acks) == result["count"]
    assert max(ack["last_sequence_number"] for ack in acks) == result["last_sequence_number"]
    # In order, each batch ending where the next one's shares begin.
    ends = [ack["last_sequence_number"] for ack in acks]
    assert ends == sorted(ends)
    assert ends[-1] - result["first_sequence_number"] + 1 == result["count"]
    # new_shares_sum is each batch's summed difficulty, truncated, as SRI reports it.
    for ack in acks:
        assert ack["sum"] == int(ack["accepted"] * result["difficulty"])
    return acks


def test_a_run_of_shares_reconciles_exactly_against_the_batched_acknowledgements(pinned):
    first = reconcile(pinned.submit(50))
    assert max(ack["accepted"] for ack in first) > 1, "sent in one write, acknowledged in batches"
    # Sequence numbers carry on across runs, and so does the reconciliation.
    second = pinned.submit(20)
    assert second["first_sequence_number"] == 50
    reconcile(second)


@pytest.mark.parametrize(
    ("kind", "error_code"),
    [
        ("low_difficulty", "difficulty-too-low"),
        ("bad_version", "invalid-non-rollable-version-bit"),
        ("bad_extranonce", "bad-extranonce-size"),
    ],
)
def test_each_rejection_class_has_its_own_error(pinned, kind, error_code):
    result = pinned.submit(kind=kind)
    assert [e["error_code"] for e in result["errors"]] == [error_code]
    reconcile(pinned.submit(2))


def test_a_duplicate_is_refused_under_its_new_sequence_number(pinned):
    reconcile(pinned.submit())
    duplicate = pinned.submit(kind="duplicate")
    assert duplicate["errors"] == [
        {"sequence_number": duplicate["last_sequence_number"], "error_code": "duplicate-share"}
    ]


def test_a_share_on_a_job_before_the_last_block_is_stale(pinned, regtest_cluster):
    stale_job = pinned.job["job_id"]
    regtest_cluster.chain.mine(1, regtest_cluster.address("pool_wallet"))
    pinned.client.wait("SetNewPrevHash", channel_id=pinned.channel_id)
    pinned.next_job()
    # The old job's data is still held by the client, so the share is well formed; only its job is stale.
    result = pinned.submit(kind="low_difficulty", job_id=stale_job)
    assert [e["error_code"] for e in result["errors"]] == ["stale-share"]
    reconcile(pinned.submit())


def test_a_mid_session_set_target_holds_from_the_next_share(pool_factory, miner_username):
    pool = pool_factory(overrides={"vardiff": {"ceiling": CEILING}})
    miner = Sv2Miner(pool, pool.solo_v2_port, miner_username)
    old = miner.channel["target"]
    # Shares far faster than the interval: vardiff raises the target within ten of them.
    raised = None
    for _ in range(40):
        reconcile(miner.submit())
        raised = next((m for m in miner.client.messages() if m["message"] == "SetTarget"), None)
        if raised:
            break
    assert raised and int(raised["target"], 16) < int(old, 16), "a harder target"
    # A job carrying the new target follows the SetTarget, and the next share is held to it.
    job = miner.next_job()
    assert job["target"] == raised["target"]
    missed = miner.submit(kind="between", meets=old)
    assert [e["error_code"] for e in missed["errors"]] == ["difficulty-too-low"]
    reconcile(miner.submit())
    miner.close()


def test_a_lowered_target_holds_from_the_next_share_on_any_job(pool_factory, miner_username):
    pool = pool_factory(overrides={"vardiff": {"ceiling": CEILING}})
    # Claiming a rate that starts it at the ceiling.
    miner = Sv2Miner(pool, pool.solo_v2_port, miner_username, nominal_hash_rate=1.0)
    hard = miner.channel["target"]
    old_job = miner.job["job_id"]
    miner.client.command("update_channel", channel_id=miner.channel_id, nominal_hash_rate=0.0)
    lowered = miner.client.wait("SetTarget", channel_id=miner.channel_id)
    assert int(lowered["target"], 16) > int(hard, 16), "an easier target"
    # Shares meeting the new target count at once, even on the job issued before it, and some of them miss
    # the old one.
    accepted = miner.submit(10, job_id=old_job)
    reconcile(accepted)
    assert any(int(h, 16) > int(hard, 16) for h in accepted["hashes"])
    miner.close()


def test_a_block_over_sv2_is_accepted_and_recorded_once_with_v1s_split(pool, regtest_cluster):
    chain = regtest_cluster.chain
    finder = regtest_cluster.miner_address(0)
    miner = Sv2Miner(pool, pool.solo_v2_port, f"{finder}.sv2")
    result = miner.submit(kind="block")
    reconcile(result)
    [block_hash] = result["hashes"]
    wait_until(lambda: chain.tip()[1] == block_hash, 10, "the block at the tip")
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "its answer recorded")

    # A resend is refused, and the block is not submitted again.
    resent = miner.submit(kind="duplicate")
    assert [e["error_code"] for e in resent["errors"]] in (["duplicate-share"], ["stale-share"])
    submissions = [l for l in pool.logs().splitlines() if "submitting block" in l and block_hash in l]
    assert len(submissions) == 1
    row = pool.block(block_hash)
    assert (row["height"], row["pool"]) == (chain.tip()[0], "solo")
    assert pool.block_hex(block_hash) == chain.call("getblock", block_hash, 0)

    stats = chain.call("getblockstats", block_hash, ["subsidy", "totalfee"])
    reward = stats["subsidy"] + stats["totalfee"]
    vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
    expected = pool.reward_outputs(reward, finder)
    assert [(o["scriptPubKey"].get("address"), sats(o["value"])) for o in vout[: len(expected)]] == expected
    assert sum(sats(o["value"]) for o in vout) == reward
    # The found-block line and the row name the finder's worker and transport.
    [found] = [l for l in pool.logs().splitlines() if "share is a block" in l and block_hash in l]
    assert "worker=sv2" in found and "transport=V2-native" in found
    assert (row["worker"], row["transport"]) == ("sv2", "V2-native")
    miner.close()


def test_a_pooled_block_over_sv2_pays_the_pool_wallet(pool, regtest_cluster):
    chain = regtest_cluster.chain
    miner = Sv2Miner(pool, pool.pooled_v2_port, f"{regtest_cluster.miner_address(1)}.sv2")
    [block_hash] = miner.submit(kind="block")["hashes"]
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "its answer recorded")
    assert pool.block(block_hash)["pool"] == "pooled"
    vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
    assert vout[0]["scriptPubKey"]["address"] == pool.config["addresses"]["pool_wallet"]
    miner.close()
