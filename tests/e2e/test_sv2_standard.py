"""SV2 standard channels: header-only mining on the pool's whole extranonce. The
open returns a 256-bit target, a mid-session SetTarget holds from the next share, acknowledgements reconcile,
and a block found on a standard channel is submitted."""

import hashlib

from clients.sv2 import Sv2Miner
from test_sv2_submit import CEILING, PINNED, reconcile
from wait import wait_until


def sha256d(data):
    return hashlib.sha256(hashlib.sha256(data).digest()).digest()


def test_opens_with_a_256_bit_target_and_the_channels_own_merkle_root(pool, regtest_cluster, miner_username):
    miner = Sv2Miner(pool, pool.solo_v2_port, miner_username, standard=True)
    assert len(bytes.fromhex(miner.channel["target"])) == 32
    extranonce = bytes.fromhex(miner.channel["extranonce_prefix"])
    assert len(extranonce) == 12, "the pool fills the whole extranonce"

    # The merkle root is the coinbase an extended channel would get, with this channel's 12 bytes in it.
    extended = Sv2Miner(pool, pool.solo_v2_port, miner_username)
    assert extended.job["prev_hash"] == miner.job["prev_hash"]
    coinbase = (
        bytes.fromhex(extended.job["coinbase_prefix"])
        + extranonce
        + bytes.fromhex(extended.job["coinbase_suffix"])
    )
    root = sha256d(coinbase)
    for sibling in extended.job["merkle_path"]:
        root = sha256d(root + bytes.fromhex(sibling))
    assert miner.job["merkle_root"] == root.hex()
    miner.close()
    extended.close()


def test_acknowledgements_reconcile(pool_factory, miner_username):
    pool = pool_factory(overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}})
    miner = Sv2Miner(pool, pool.solo_v2_port, miner_username, standard=True)
    reconcile(miner.submit(50))
    assert [e["error_code"] for e in miner.submit(kind="duplicate")["errors"]] == ["duplicate-share"]
    for kind, error_code in (
        ("low_difficulty", "difficulty-too-low"),
        ("bad_version", "invalid-non-rollable-version-bit"),
    ):
        assert [e["error_code"] for e in miner.submit(kind=kind)["errors"]] == [error_code]
    miner.close()


def test_a_mid_session_set_target_holds_from_the_next_share(pool_factory, miner_username):
    pool = pool_factory(overrides={"vardiff": {"ceiling": CEILING}})
    miner = Sv2Miner(pool, pool.solo_v2_port, miner_username, standard=True)
    old = miner.channel["target"]
    raised = None
    for _ in range(40):
        reconcile(miner.submit())
        raised = next((m for m in miner.client.messages() if m["message"] == "SetTarget"), None)
        if raised:
            break
    assert raised and int(raised["target"], 16) < int(old, 16)
    miner.next_job()
    missed = miner.submit(kind="between", meets=old)
    assert [e["error_code"] for e in missed["errors"]] == ["difficulty-too-low"]
    reconcile(miner.submit())
    miner.close()


def test_a_block_on_a_standard_channel_is_accepted(pool, regtest_cluster):
    chain = regtest_cluster.chain
    finder = regtest_cluster.miner_address(0)
    miner = Sv2Miner(pool, pool.solo_v2_port, f"{finder}.standard", standard=True)
    result = miner.submit(kind="block")
    reconcile(result)
    [block_hash] = result["hashes"]
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "its answer recorded")
    assert chain.tip()[1] == block_hash
    vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
    assert vout[0]["scriptPubKey"]["address"] == finder
    miner.close()
