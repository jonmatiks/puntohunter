"""Helpers of the payout tests: a pool paying out on a node of the test's own, its miners'
earnings, and the batches that pay them. Blocks are mined to the funding wallet, so the payout wallet holds
no coinbase but the pool's, as startup reconciliation requires."""

from chain import sats
from clients.sv1 import Sv1Miner
from regtest import MINERS_WALLET, PAYOUT_WALLET
from wait import wait_until

# src/pool/payouts.rs: how deep a batch's transaction must be before the batch is confirmed.
CONFIRMATIONS = 6
RESERVE = 1_000_000


def start(pool_factory, node, payouts=None, reserve=RESERVE, **kwargs):
    """A pool on `node` attempting a batch every half second, `payouts` overriding its other settings, and the
    payout wallet funded with 10 BTC for fees. The fee reserve is credited with `reserve`."""
    node.fund_payouts(10)
    overrides = kwargs.pop("overrides", {})
    overrides["payouts"] = {"interval_secs": 0.5, **(payouts or {})}
    pool = pool_factory(node=node, overrides=overrides, **kwargs)
    if reserve:
        credited = pool.credit_reserve(reserve)
        assert credited.returncode == 0, credited.stderr
    return pool


def earn(pool, node, solution_forcer, mode, finder, others=(), shares=3, finder_shares=0):
    """Has miner `finder` find a block in `mode`, after `shares` shares of each of miners `others` and
    `finder_shares` of its own, and returns the block's allocations, as {address: sats}, once written."""
    port = getattr(pool, f"{mode}_v1_port")
    # The miners' jobs must build on the node's tip: a block found on the job before the last block lands
    # would leave the chain, and on a slow host the pool can still be serving that job.
    tip = node.chain.call("getblockcount")
    wait_until(
        lambda: (pool.portal.api("/api/v1/pool").json["chain"] or {}).get("tip_height") == tip,
        10,
        "the pool's work on the node's tip",
    )
    miners = [Sv1Miner(port, f"{node.miner_address(n)}.rig") for n in others]
    finding = Sv1Miner(port, f"{node.miner_address(finder)}.rig")
    for miner, count in [*((miner, shares) for miner in miners), (finding, finder_shares)]:
        for _ in range(count):
            assert miner.submit(miner.share()) is True
    _, block_hash = solution_forcer(finding)
    # Done with them: the jobs of the next 100 blocks would pile up unread.
    for miner in [*miners, finding]:
        miner.client.close()
    rows = wait_until(
        lambda: pool.rows("SELECT address, sats FROM allocations WHERE block = ?", block_hash),
        10,
        "the block's allocations",
    )
    return {row["address"]: row["sats"] for row in rows}


def balances(pool):
    """Each (address, pool)'s balances."""
    return {(row["address"], row["pool"]): row for row in pool.rows("SELECT * FROM balances")}


def mature(pool, node):
    """Mines the pool's blocks to maturity and waits for their allocations to be available."""
    node.chain.mine(100, node.funder_address())
    wait_until(
        lambda: balances(pool) and all(row["pending"] == 0 for row in balances(pool).values()),
        30,
        "every allocation available",
    )


def batches(pool):
    return pool.rows("SELECT * FROM batches ORDER BY id")


def broadcast(pool, count=1):
    """Waits for `count` batches, each broadcast, and returns them."""
    return wait_until(
        lambda: len(found := batches(pool)) == count
        and all(batch["state"] == "broadcast" for batch in found)
        and found,
        30,
        f"{count} batches broadcast",
    )


def confirm(pool, node, count=1):
    """Buries the broadcast batches deep enough and waits for them to be confirmed."""
    node.chain.mine(CONFIRMATIONS, node.funder_address())
    return wait_until(
        lambda: len(found := batches(pool)) == count
        and all(batch["state"] == "confirmed" for batch in found)
        and found,
        30,
        f"{count} batches confirmed",
    )


def outputs(node, txid):
    """What transaction `txid` of the payout wallet pays, as {address: sats}."""
    hex = node.wallet(PAYOUT_WALLET).call("gettransaction", txid)["hex"]
    tx = node.chain.call("decoderawtransaction", hex)
    paid = {}
    for output in tx["vout"]:
        address = output["scriptPubKey"]["address"]
        paid[address] = paid.get(address, 0) + sats(output["value"])
    return paid


def payments(node, address):
    """The transactions paying `address`, as the miners' wallet sees them."""
    transactions = node.wallet(MINERS_WALLET).call("listtransactions", "*", 1000, 0, True)
    return {tx["txid"] for tx in transactions if tx["category"] == "receive" and tx["address"] == address}


def held_reserve(pool):
    """What the fee reserve holds: its credits, less the fees paid from it."""
    [row] = pool.rows(
        "SELECT COALESCE(SUM(CASE kind WHEN 'credit' THEN sats WHEN 'paid' THEN -sats ELSE 0 END), 0) "
        "AS sats FROM ledger WHERE address IS NULL"
    )
    return row["sats"]


def health(pool):
    """The health check's status and payouts state."""
    checked = pool.portal.api("/api/v1/health").json
    return checked["status"], checked["checks"]["payouts"]


def metric(pool, name):
    line = next(line for line in pool.metrics().splitlines() if line.startswith(f"{name} "))
    return float(line.split()[1])
