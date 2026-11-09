"""Payout wallet and fee reserve: the payout wallet holds the pool's coinbases and none of the
miners' outputs, and `puntohunter reserve credit` credits the fee reserve up to what the wallet can spend beside
what miners are owed."""

import tomli_w

from chain import RpcError, sats
from clients.sv1 import Sv1Miner
from regtest import MINERS_WALLET, PAYOUT_WALLET
from wait import wait_until


def holds(wallet, txid):
    """Whether the wallet has transaction `txid`: whether any of its outputs is the wallet's."""
    try:
        wallet.call("gettransaction", txid)
        return True
    except RpcError as e:
        if e.code == -5:
            return False
        raise


def coinbase(chain, block_hash):
    return chain.call("getblock", block_hash)["tx"][0]


def test_the_payout_wallet_holds_the_pools_coinbases_and_none_of_the_miners_outputs(
    pool_factory, regtest_cluster, solution_forcer
):
    chain = regtest_cluster.chain
    pool = pool_factory()
    payouts, miners = regtest_cluster.wallet(PAYOUT_WALLET), regtest_cluster.wallet(MINERS_WALLET)
    blocks = {}
    for mode, n in (("pooled", 0), ("solo", 1)):
        miner = Sv1Miner(getattr(pool, f"{mode}_v1_port"), f"{regtest_cluster.miner_address(n)}.rig")
        _, block_hash = solution_forcer(miner)
        wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, f"the {mode} block answered")
        miner.client.close()
        blocks[mode] = coinbase(chain, block_hash)

    # The wallets see a block once the node has passed it to them.
    wait_until(lambda: holds(payouts, blocks["pooled"]), 10, "the payout wallet sees the pooled coinbase")
    wait_until(lambda: holds(miners, blocks["solo"]), 10, "the miners' wallet sees the solo coinbase")
    details = payouts.call("gettransaction", blocks["pooled"])["details"]
    assert {d["address"] for d in details} == {regtest_cluster.address("pool_wallet")}
    assert {d["category"] for d in details} == {"immature"}
    assert not holds(payouts, blocks["solo"]), "a solo coinbase pays its miner and the fee addresses"
    assert not holds(miners, blocks["pooled"])


def test_the_fee_reserve_takes_credits_up_to_what_the_wallet_can_back(
    pool_factory, fresh_node, solution_forcer, tmp_path
):
    chain = fresh_node.chain
    payouts = fresh_node.wallet(PAYOUT_WALLET)
    fresh_node.fund_payouts(60)
    pool = pool_factory(node=fresh_node, overrides={"payouts": {}})
    miner = Sv1Miner(pool.pooled_v1_port, f"{fresh_node.miner_address(0)}.rig")
    _, block_hash = solution_forcer(miner)
    miner.client.close()
    wait_until(lambda: holds(payouts, coinbase(chain, block_hash)), 10, "the payout wallet sees the block")
    [owed] = wait_until(
        lambda: [r["pending"] for r in pool.rows("SELECT * FROM balances")], 10, "the block's allocations"
    )
    spendable = sats(payouts.call("getbalances")["mine"]["trusted"])
    assert spendable == 6_000_000_000, "the operator's funds; the pool's block is immature"
    headroom = spendable - owed

    first = pool.credit_reserve(headroom - 1_000)
    assert first.returncode == 0, first.stderr
    assert f"it holds {headroom - 1_000} sats" in first.stdout
    refused = pool.credit_reserve(1_001)
    assert refused.returncode != 0
    assert f"miners are owed {owed} and the fee reserve holds {headroom - 1_000}" in refused.stderr
    last = pool.credit_reserve(1_000)
    assert last.returncode == 0, last.stderr
    assert f"it holds {headroom} sats" in last.stdout
    credits = pool.rows("SELECT sats FROM ledger WHERE kind = 'credit' ORDER BY id")
    assert [row["sats"] for row in credits] == [headroom - 1_000, 1_000]

    unconfigured = {key: value for key, value in pool.config.items() if key != "payouts"}
    path = tmp_path / "no-payouts.toml"
    path.write_text(tomli_w.dumps(unconfigured))
    refused = pool.credit_reserve(1, config=path)
    assert refused.returncode != 0
    assert "payouts are not configured" in refused.stderr
