"""Ledger and maturity: a pooled or hybrid block's allocations are credited
pending, reversed when the block leaves the chain before maturity and restored when it returns, and made
available once, at maturity plus the buffer, across a restart. A hybrid block's finder output never reaches
the ledger."""

import re
import time

import pytest

from clients.sv1 import Sv1Miner
from wait import wait_until

BUFFER = 2


def metric(pool, name):
    match = re.search(rf"^{name} (\S+)$", pool.metrics(), re.M)
    assert match, f"no {name}"
    return float(match[1])


def entries(pool, block_hash, kind):
    """The block's ledger entries of `kind`, as {address: [sats, ...]} in the order written."""
    found = {}
    rows = pool.rows("SELECT address, sats FROM ledger WHERE block = ? AND kind = ? ORDER BY id", block_hash, kind)
    for row in rows:
        found.setdefault(row["address"], []).append(row["sats"])
    return found


def balances(pool):
    """Each address's (pending, available) balance."""
    return {row["address"]: (row["pending"], row["available"]) for row in pool.rows("SELECT * FROM balances")}


def confirmations(chain, block_hash):
    return chain.call("getblockheader", block_hash)["confirmations"]


@pytest.mark.parametrize("mode", ["pooled", "hybrid"])
def test_allocations_follow_their_block_through_reorgs_and_maturity(
    pool_factory, regtest_cluster, solution_forcer, mode
):
    pool = pool_factory(overrides={"accounting": {"maturity_buffer": BUFFER}})
    chain = regtest_cluster.chain
    wallet = regtest_cluster.address("pool_wallet")
    addresses = [regtest_cluster.miner_address(n) for n in range(2)]
    port = getattr(pool, f"{mode}_v1_port")
    finder, other = (Sv1Miner(port, f"{address}.rig") for address in addresses)
    for _ in range(3):
        assert other.submit(other.share()) is True
    _, block_hash = solution_forcer(finder)
    # Done with them: the jobs of the next 100 blocks would pile up unread.
    finder.client.close()
    other.client.close()

    def status():
        return pool.block(block_hash).get("status")

    allocated = {
        row["address"]: row["sats"]
        for row in wait_until(
            lambda: pool.rows("SELECT address, sats FROM allocations WHERE block = ?", block_hash),
            10,
            "the block's allocations",
        )
    }
    assert set(allocated) == set(addresses)
    wait_until(lambda: status() == "confirmed_immature", 10, "confirmed-immature")
    assert entries(pool, block_hash, "pending") == {a: [sats] for a, sats in allocated.items()}
    pending = {a: (sats, 0) for a, sats in allocated.items()}
    assert balances(pool) == pending
    # The pool output alone is credited: a hybrid finder's own output is paid in the coinbase.
    block = pool.block(block_hash)
    assert sum(allocated.values()) == block["pool_sats"]
    assert (block["finder_sats"] > 0) == (mode == "hybrid")

    # Off the chain before maturity: each pending entry reversed. Back on it: the same allocations pending again.
    chain.call("invalidateblock", block_hash)
    try:
        wait_until(lambda: status() == "orphaned", 30, "orphaned")
        assert entries(pool, block_hash, "reversal") == {a: [sats] for a, sats in allocated.items()}
        assert balances(pool) == {a: (0, 0) for a in allocated}
    finally:
        chain.call("reconsiderblock", block_hash)
    wait_until(lambda: status() == "confirmed_immature", 30, "back on the chain")
    assert entries(pool, block_hash, "pending") == {a: [sats, sats] for a, sats in allocated.items()}
    assert balances(pool) == pending

    # Mature, the buffer not yet past; a restart; then past it: each allocation available once.
    chain.mine(100 + BUFFER - confirmations(chain, block_hash), wallet)
    wait_until(lambda: status() == "mature", 30, "mature")
    assert entries(pool, block_hash, "available") == {}
    pool.restart()
    chain.mine(1, wallet)
    available = {a: (0, sats) for a, sats in allocated.items()}
    wait_until(lambda: balances(pool) == available, 30, "available")
    assert entries(pool, block_hash, "available") == {a: [sats] for a, sats in allocated.items()}
    pool.restart()
    height, _ = chain.tip()
    chain.mine(1, wallet)
    wait_until(lambda: metric(pool, "puntohunter_tip_height") == height + 1, 30, "the next tip")
    # The lifecycle task reconciles just after the tip is reported.
    time.sleep(1)
    assert entries(pool, block_hash, "available") == {a: [sats] for a, sats in allocated.items()}

    # Off the chain after: nothing reserved or paid, so each available credit is reversed, and payouts run on.
    chain.call("invalidateblock", block_hash)
    try:
        wait_until(lambda: balances(pool) == {a: (0, 0) for a in allocated}, 30, "reversed")
    finally:
        chain.call("reconsiderblock", block_hash)
    wait_until(lambda: balances(pool) == available, 30, "available again")
    assert metric(pool, "puntohunter_payouts_paused") == 0
