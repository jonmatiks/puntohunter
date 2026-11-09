"""Mining modes over V1: mode is bound by the listener, a pooled coinbase pays the pool wallet
and a solo one the finder, each with the pool's fee outputs, and pooled sessions share one coinbase. The fee outputs are
whatever the config lists, in its order, each a share or a fixed amount capped at what is left, or none, the miner
then taking the whole reward, as the pool resource gives them. The scriptSig carries the configured tag, and the
worker's name after it when the config asks."""

import pytest

from chain import sats
from clients.sv1 import Sv1Client


def job(port, username):
    client = Sv1Client(port)
    client.subscribe()
    client.authorize(username)
    return client, client.notification("mining.notify")["params"]


def decoded(chain, client, notify):
    """The coinbase a notify announces, decoded by the node."""
    coinbase = notify[2] + client.extranonce1 + "00" * client.extranonce2_size + notify[3]
    return chain.call("decoderawtransaction", coinbase)


def outputs(chain, client, notify):
    """The coinbase outputs a notify announces, decoded by the node: (address, satoshis) each."""
    vout = decoded(chain, client, notify)["vout"]
    return [(output["scriptPubKey"].get("address"), sats(output["value"])) for output in vout]


def test_pooled_coinbases_pay_the_pool_wallet_and_solo_ones_the_finder(pool, regtest_cluster):
    chain = regtest_cluster.chain
    addresses = pool.config["addresses"]
    finder, other = regtest_cluster.miner_address(0), regtest_cluster.miner_address(1)
    solo, solo_job = job(pool.solo_v1_port, f"{finder}.solo")
    pooled, pooled_job = job(pool.pooled_v1_port, f"{finder}.pooled")
    pooled_other, pooled_other_job = job(pool.pooled_v1_port, f"{other}.pooled")
    assert solo_job[1] == pooled_job[1] == pooled_other_job[1], "all on one template"
    reward = chain.call("getblocktemplate", {"rules": ["segwit"]})["coinbasevalue"]

    def expected(payee):
        # The reward outputs, then the witness commitment.
        return pool.reward_outputs(reward, payee) + [(None, 0)]

    assert outputs(chain, solo, solo_job) == expected(finder)
    assert outputs(chain, pooled, pooled_job) == expected(addresses["pool_wallet"])
    assert outputs(chain, pooled_other, pooled_other_job) == expected(addresses["pool_wallet"])
    # Whatever address a pooled miner authorizes with, its job's coinbase is the pool's one coinbase.
    assert pooled_job[2:4] == pooled_other_job[2:4]
    for client in (solo, pooled, pooled_other):
        client.close()


@pytest.mark.parametrize("count", [0, 3])
def test_the_coinbase_pays_whatever_fees_the_config_lists(pool_factory, regtest_cluster, count):
    receivers = [("alpha", 1.0), ("beta", 0.25), ("gamma", 0.0125)][:count]
    fees = [
        {"name": name, "address": regtest_cluster.miner_address(i + 1), "percent": percent}
        for i, (name, percent) in enumerate(receivers)
    ]
    pool = pool_factory(overrides={"fees": fees})
    chain = regtest_cluster.chain
    finder = regtest_cluster.miner_address(0)
    solo, solo_job = job(pool.solo_v1_port, f"{finder}.solo")
    reward = chain.call("getblocktemplate", {"rules": ["segwit"]})["coinbasevalue"]

    paid = outputs(chain, solo, solo_job)
    assert paid == pool.reward_outputs(reward, finder) + [(None, 0)]
    assert [address for address, _ in paid[1:-1]] == [fee["address"] for fee in fees], "in config order"
    assert sum(value for _, value in paid) == reward

    terms = pool.portal.api("/api/v1/pool").json["reward"]
    shares = [{"ppm": 10_000}, {"ppm": 2_500}, {"ppm": 125}][:count]
    assert terms["fees"] == [{"name": name, "portion": share} for (name, _), share in zip(receivers, shares)]
    if count:
        assert terms["miner"] == {"ppm": 987_375}
    else:
        assert paid[0] == (finder, reward), "no fees: the miner takes it all"
        assert terms["miner"] == {"ppm": 1_000_000}
    solo.close()


@pytest.mark.parametrize(
    "beta, fixed", [(0.01, 1_000_000), (21_000_000.0, 2_100_000_000_000_000)], ids=["fixed", "more than the reward"]
)
def test_a_fee_takes_a_share_or_a_fixed_amount_capped_at_what_is_left(pool_factory, regtest_cluster, beta, fixed):
    fees = [
        {"name": "alpha", "address": regtest_cluster.miner_address(1), "percent": 1.0},
        {"name": "beta", "address": regtest_cluster.miner_address(2), "btc": beta},
    ]
    pool = pool_factory(overrides={"fees": fees})
    chain = regtest_cluster.chain
    finder = regtest_cluster.miner_address(0)
    solo, solo_job = job(pool.solo_v1_port, f"{finder}.solo")
    reward = chain.call("getblocktemplate", {"rules": ["segwit"]})["coinbasevalue"]

    paid = outputs(chain, solo, solo_job)
    assert paid == pool.reward_outputs(reward, finder) + [(None, 0)]
    alpha = reward // 100
    if beta < 1:
        assert paid[1:3] == [(fees[0]["address"], alpha), (fees[1]["address"], 1_000_000)]
    else:
        # The fixed fee takes all the share leaves it, and the empty miner portion has no output.
        assert paid[:2] == [(fees[0]["address"], alpha), (fees[1]["address"], reward - alpha)]
    assert sum(value for _, value in paid) == reward

    terms = pool.portal.api("/api/v1/pool").json["reward"]
    assert terms["fees"] == [
        {"name": "alpha", "portion": {"ppm": 10_000}},
        {"name": "beta", "portion": {"sats": fixed}},
    ]
    assert terms["miner"] is None, "the miner portion differs per block"
    solo.close()


@pytest.mark.parametrize("worker_names", [False, True])
def test_the_script_sig_carries_the_tag_and_names_the_worker_when_configured(
    pool_factory, regtest_cluster, worker_names
):
    pool = pool_factory(overrides={"coinbase": {"tag": "/e2e pool/", "worker_names": worker_names}})
    chain = regtest_cluster.chain
    finder, other = regtest_cluster.miner_address(0), regtest_cluster.miner_address(1)
    sessions = [
        job(pool.solo_v1_port, f"{finder}.rack-1"),
        job(pool.pooled_v1_port, f"{finder}.rack-2"),
        job(pool.pooled_v1_port, f"{other}.rack-3"),
        job(pool.pooled_v1_port, other),
    ]
    script_sigs = [
        bytes.fromhex(decoded(chain, client, notify)["vin"][0]["coinbase"]) for client, notify in sessions
    ]

    # Each scriptSig pushes its signature right before the 13-byte push of the extranonce.
    if worker_names:
        expected = [
            b"/e2e pool/Mined by rack-1/",
            b"/e2e pool/Mined by rack-2/",
            b"/e2e pool/Mined by rack-3/",
            b"/e2e pool/",  # An unnamed worker gets the tag alone.
        ]
    else:
        expected = [b"/e2e pool/"] * 4
        assert sessions[1][1][2:4] == sessions[2][1][2:4], "pooled sessions still share one coinbase"
    for script_sig, signature in zip(script_sigs, expected):
        assert script_sig[:-13].endswith(bytes([len(signature)]) + signature), script_sig
    for client, _ in sessions:
        client.close()
