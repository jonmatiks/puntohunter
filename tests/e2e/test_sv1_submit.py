"""V1 submits: accepted shares, each rejection class with its own error, a flood of
unauthorized submits failing only its own connection, and vardiff following the miner."""

import json

import pytest

from clients.sv1 import HostileSv1Client, Sv1Miner

STALE, DUPLICATE, LOW_DIFFICULTY, UNAUTHORIZED, OTHER = 21, 22, 23, 24, 20
# Between the harness floor and regtest's network difficulty (about 4.7e-10), so a share takes a few hashes
# and need not be a block: a block would move the chain and make the test's next share stale.
PINNED = 3e-10


@pytest.fixture
def miner(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    yield miner
    miner.client.close()


@pytest.fixture
def pinned(pool_factory, miner_username):
    pool = pool_factory(overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}})
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    yield miner
    miner.client.close()


def test_shares_at_a_pinned_difficulty_are_all_accepted(pinned):
    assert pinned.difficulty == PINNED
    for _ in range(25):
        assert pinned.submit(pinned.share()) is True
    assert not any(m.get("method") == "mining.set_difficulty" for m in pinned.client.notifications)


def test_rejects_a_stale_job(miner, regtest_cluster):
    stale = miner.share()
    regtest_cluster.chain.mine(1, regtest_cluster.address("pool_wallet"))
    miner.clean_job()
    assert miner.rejection(stale).code == STALE


def test_rejects_a_duplicate(pinned):
    share = pinned.share()
    assert pinned.submit(share) is True
    assert pinned.rejection(share).code == DUPLICATE


def test_rejects_a_low_difficulty_share(pinned):
    assert pinned.rejection(pinned.share(meets=False)).code == LOW_DIFFICULTY


def test_rejects_an_unauthorized_worker(pinned, regtest_cluster):
    other = f"{regtest_cluster.miner_address(1)}.other"
    assert pinned.rejection(pinned.share(), username=other).code == UNAUTHORIZED


def test_rejects_version_bits_outside_the_mask(pinned):
    rejected = pinned.rejection(pinned.share(version_bits=0x0000_0001))
    assert rejected.code == OTHER
    assert "mask" in rejected.message
    assert pinned.submit(pinned.share(version_bits=0x0123_4000)) is True, "in-mask bits are fine"


def test_an_unauthorized_flood_fails_only_its_connection(pool, miner):
    hostile = HostileSv1Client(pool.solo_v1_port)
    hostile.subscribe()
    params = [miner.username, miner.notify[0], "00" * 8, miner.notify[7], "00000000"]
    flood = b"".join(
        json.dumps({"id": i, "method": "mining.submit", "params": params}).encode() + b"\n"
        for i in range(100)
    )
    hostile.send_raw(flood)
    assert hostile.closed_by_pool()
    assert miner.client.alive()
    assert miner.submit(miner.share()) is True
    hostile.close()


def test_vardiff_raises_the_difficulty_of_a_fast_miner(miner):
    start = miner.difficulty
    while miner.difficulty == start:
        assert miner.submit(miner.share()) is True
        # A retarget sends set_difficulty and a new job right after the accepted share.
        if any(m.get("method") == "mining.notify" for m in miner.client.notifications):
            miner.next_job()
    assert miner.difficulty > start


def test_a_header_is_a_duplicate_under_any_job_id(miner):
    """A job issued again on the same template must not let the miner's earlier headers count twice, as they
    once did for a CPU miner."""
    first = miner.notify
    accepted = []
    while miner.notify is first:
        share = miner.share()
        assert miner.submit(share) is True
        accepted.append(share)
        if any(m.get("method") == "mining.notify" for m in miner.client.notifications):
            miner.next_job()
    assert miner.notify[8] is False, "a retarget reissues on the same template"
    assert int(miner.notify[7], 16) > int(first[7], 16), "with a later ntime, so its headers are new"
    replayed = [miner.notify[0], *accepted[0][1:]]
    assert miner.rejection(replayed).code == DUPLICATE

