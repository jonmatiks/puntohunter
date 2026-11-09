"""The share journal: every pooled and hybrid share is journaled before the
miner hears it was accepted, over V1 and V2. Killed after a journal commit or before an acknowledgement, the
restarted pool holds every acknowledged share, and the same work sent again, through a new session over either
transport, is refused as a duplicate. A failed commit refuses pooled and hybrid shares until a restart, raises
the alert metric, and leaves solo mining alone. A batch of SV2 shares is acknowledged exactly as journaled, a
pooled block's window cutoff is its solution share's sequence, each share carries its job's miner portion and
pool output, and solo shares never reach the journal. Every portal response passes the privacy scan."""

import re

import pytest

from clients.sv1 import ConnectionClosed, Sv1Miner
from clients.sv2 import Sv2Error, Sv2Miner
from wait import wait_until

# Between the harness floor and regtest's network difficulty: a share takes a few hashes, need not be a block,
# and vardiff, pinned, never issues another job.
PINNED = 3e-10
DUPLICATE, OTHER = 22, 20
# The version bits V1 miners roll: configure's default mask.
MASK = 0x1FFFE000
MODES = ["pooled", "hybrid"]
FAILPOINTS = ["after-journal-commit", "before-acknowledgement"]


def pinned(**overrides):
    return {"vardiff": {"floor": PINNED, "ceiling": PINNED}, **overrides}


def metric(pool, name):
    match = re.search(rf"^{name} (\S+)$", pool.metrics(), re.M)
    assert match, f"no {name}"
    return float(match[1])


def crashed(pool):
    """Waits for the failpoint to stop the pool, then starts it again without one."""
    wait_until(pool.exited, 10, "the failpoint to stop the pool")
    assert "failpoint: exiting" in pool.logs()
    pool.env = {}
    pool.start()


def port(pool, mode, transport):
    return getattr(pool, f"{mode}_{transport}_port")


@pytest.mark.parametrize("failpoint", FAILPOINTS)
@pytest.mark.parametrize("mode", MODES)
def test_a_crash_over_v1_keeps_every_acknowledged_share_and_refuses_it_again(
    pool_factory, regtest_cluster, mode, failpoint
):
    pool = pool_factory(overrides=pinned(), env={"PUNTOHUNTER_FAILPOINT": f"{failpoint}:3"})
    username = f"{regtest_cluster.miner_address(0)}.crash"
    miner = Sv1Miner(port(pool, mode, "v1"), username)
    acknowledged = [miner.share() for _ in range(2)]
    for params in acknowledged:
        assert miner.submit(params) is True
    unanswered = miner.share()
    with pytest.raises((ConnectionClosed, OSError)):
        miner.submit(unanswered)
    crashed(pool)

    journal = pool.journal()
    assert len(journal) == 3, "both acknowledged, and the third, committed but never acknowledged"
    assert {row["pool"] for row in journal} == {mode}
    assert [row["sequence"] for row in journal] == sorted(row["sequence"] for row in journal)
    # The restarted pool's first session gets the same extranonce prefix, so its first job has the same
    # coinbase: the same params are the same headers, which the journal already holds.
    again = Sv1Miner(port(pool, mode, "v1"), username)
    assert again.client.extranonce1 == miner.client.extranonce1
    for params in acknowledged + [unanswered]:
        assert again.rejection(params).code == DUPLICATE
    assert len(pool.journal()) == 3
    again.client.close()


@pytest.mark.parametrize("failpoint", FAILPOINTS)
@pytest.mark.parametrize("mode", MODES)
def test_a_crash_over_v2_keeps_every_acknowledged_share_and_v1_cannot_send_it_again(
    pool_factory, regtest_cluster, mode, failpoint
):
    pool = pool_factory(overrides=pinned(), env={"PUNTOHUNTER_FAILPOINT": f"{failpoint}:3"})
    username = f"{regtest_cluster.miner_address(0)}.crash"
    miner = Sv2Miner(pool, port(pool, mode, "v2"), username)
    acknowledged, hashes = [], []
    for _ in range(2):
        result = miner.submit()
        assert [ack["accepted"] for ack in result["acks"]] == [1], result
        acknowledged += result["shares"]
        hashes += result["hashes"]
    with pytest.raises(Sv2Error):
        miner.submit()
    miner.close()
    crashed(pool)

    journal = pool.journal()
    assert len(journal) == 3, "both acknowledged, and the third, committed but never acknowledged"
    assert set(hashes) <= {row["proof"] for row in journal}
    # The channel's prefix was the first the pool gave out, as a V1 session's is after the restart, and the
    # two transports build the same coinbase: the channel's shares, sent over V1, are the same headers.
    v1 = Sv1Miner(port(pool, mode, "v1"), username)
    for share in acknowledged:
        params = [
            v1.notify[0],
            share["extranonce"],
            f"{share['ntime']:08x}",
            f"{share['nonce']:08x}",
            f"{share['version'] & MASK:08x}",
        ]
        assert v1.rejection(params).code == DUPLICATE, share
    assert len(pool.journal()) == 3
    v1.client.close()


def test_a_failed_commit_refuses_pooled_and_hybrid_shares_until_a_restart_but_not_solo(
    pool_factory, regtest_cluster
):
    pool = pool_factory(overrides=pinned(), env={"PUNTOHUNTER_FAILPOINT": "journal-commit-fails"})
    username = f"{regtest_cluster.miner_address(0)}.failed"
    assert metric(pool, "puntohunter_journal_failed") == 0

    pooled = Sv1Miner(pool.pooled_v1_port, username)
    for _ in range(2):
        refusal = pooled.rejection(pooled.share())
        assert (refusal.code, refusal.message) == (OTHER, "Pool accounting unavailable")
    assert metric(pool, "puntohunter_journal_failed") == 1
    assert "CRITICAL: a journal commit failed" in pool.logs()
    hybrid = Sv1Miner(pool.hybrid_v1_port, username)
    assert hybrid.rejection(hybrid.share()).code == OTHER
    for mode in MODES:
        miner = Sv2Miner(pool, port(pool, mode, "v2"), username)
        result = miner.submit()
        assert [e["error_code"] for e in result["errors"]] == ["accounting-unavailable"], result
        assert result["acks"] == []
        miner.close()

    solo = Sv1Miner(pool.solo_v1_port, username)
    assert solo.submit(solo.share()) is True
    solo_v2 = Sv2Miner(pool, pool.solo_v2_port, username)
    assert [ack["accepted"] for ack in solo_v2.submit()["acks"]] == [1]
    assert pool.journal() == []

    pool.env = {}
    pool.restart()
    assert metric(pool, "puntohunter_journal_failed") == 0
    pooled = Sv1Miner(pool.pooled_v1_port, username)
    assert pooled.submit(pooled.share()) is True
    assert len(pool.journal()) == 1
    for client in (pooled.client, hybrid.client, solo.client):
        client.close()
    solo_v2.close()


@pytest.mark.parametrize("mode", MODES)
def test_a_batch_of_sv2_shares_is_acknowledged_exactly_as_journaled(pool_factory, regtest_cluster, mode):
    pool = pool_factory(overrides=pinned())
    miner = Sv2Miner(pool, port(pool, mode, "v2"), f"{regtest_cluster.miner_address(0)}.batch")
    result = miner.submit(count=20)
    acks = result["acks"]
    assert not result["errors"], result["errors"]
    assert sum(ack["accepted"] for ack in acks) == 20
    assert max(ack["last_sequence_number"] for ack in acks) == result["last_sequence_number"]

    journal = pool.journal()
    assert [row["proof"] for row in journal] == result["hashes"], "every share, in the order it was sent"
    assert {(row["pool"], row["address"]) for row in journal} == {(mode, regtest_cluster.miner_address(0))}
    assert len({row["handle"] for row in journal}) == 1
    assert all(len(row["target"]) == 32 for row in journal)
    miner.close()


def test_solo_shares_never_reach_the_journal_and_a_pooled_block_records_its_cutoff(
    pool_factory, regtest_cluster, solution_forcer
):
    pool = pool_factory(overrides=pinned())
    username = f"{regtest_cluster.miner_address(0)}.cutoff"
    solo = Sv1Miner(pool.solo_v1_port, username)
    for _ in range(3):
        assert solo.submit(solo.share()) is True
    solo_v2 = Sv2Miner(pool, pool.solo_v2_port, username)
    assert sum(ack["accepted"] for ack in solo_v2.submit(count=3)["acks"]) == 3
    assert pool.journal() == []

    pooled = Sv1Miner(pool.pooled_v1_port, username)
    assert pooled.submit(pooled.share()) is True
    _, block_hash = solution_forcer(pooled)
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block answered")
    journal = pool.journal()
    [solution] = [row for row in journal if row["proof"] == block_hash]
    assert solution["sequence"] == journal[-1]["sequence"]
    assert pool.block(block_hash)["cutoff"] == solution["sequence"]
    for client in (solo.client, pooled.client):
        client.close()
    solo_v2.close()


@pytest.mark.parametrize("mode", MODES)
def test_a_share_is_journaled_with_its_jobs_miner_portion_and_pool_output(
    pool_factory, regtest_cluster, solution_forcer, mode
):
    pool = pool_factory(overrides=pinned())
    miner = Sv1Miner(port(pool, mode, "v1"), f"{regtest_cluster.miner_address(0)}.split")
    assert miner.submit(miner.share()) is True
    _, block_hash = solution_forcer(miner)
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block answered")
    block = pool.block(block_hash)
    journal = pool.journal()
    [solution] = [row for row in journal if row["proof"] == block_hash]
    # The block's own split: the miner portion is what its finder and the pool wallet were paid together.
    assert (solution["miner_sats"], solution["pool_sats"]) == (
        block["finder_sats"] + block["pool_sats"],
        block["pool_sats"],
    )
    assert (solution["pool_sats"] < solution["miner_sats"]) == (mode == "hybrid")
    # The share before it, on the same template, was journaled with the same amounts.
    assert (journal[0]["miner_sats"], journal[0]["pool_sats"]) == (solution["miner_sats"], solution["pool_sats"])
    miner.client.close()
