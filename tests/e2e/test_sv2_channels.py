"""SV2 extended channels: a channel on each listener gets jobs from the shared core
with the coinbase a V1 session would get, a future job activated by SetNewPrevHash on a new block, a first
target sized from the claimed hashrate, and a refused or missing open handled as V1 handles authorization."""

import tomllib

import pytest

from chain import sats
from clients.sv2 import Sv2Client, Sv2Miner
from regtest import ROOT
from wait import wait_until

# A difficulty between the harness floor and regtest's network difficulty (about 4.7e-10).
AIMED = 3e-10
SHARE_INTERVAL = 10


def coinbase_outputs(chain, miner, extranonce="00" * 8):
    """The coinbase outputs of the miner's job, decoded by the node: (address, satoshis) each."""
    job, channel = miner.job, miner.channel
    coinbase = job["coinbase_prefix"] + channel["extranonce_prefix"] + extranonce + job["coinbase_suffix"]
    vout = chain.call("decoderawtransaction", coinbase)["vout"]
    return [(output["scriptPubKey"].get("address"), sats(output["value"])) for output in vout]


@pytest.fixture
def miners(pool):
    started = []

    def start(port, username, **args):
        miner = Sv2Miner(pool, port, username, **args)
        started.append(miner)
        return miner

    yield start
    for miner in started:
        miner.close()


def test_solo_pays_the_finder_and_pooled_the_pool_wallet_with_b4_fees(pool, regtest_cluster, miners):
    chain = regtest_cluster.chain
    addresses = pool.config["addresses"]
    finder = regtest_cluster.miner_address(0)
    solo = miners(pool.solo_v2_port, f"{finder}.solo")
    pooled = miners(pool.pooled_v2_port, f"{finder}.pooled")
    assert solo.job["prev_hash"] == pooled.job["prev_hash"], "both on one template"
    reward = chain.call("getblocktemplate", {"rules": ["segwit"]})["coinbasevalue"]

    def expected(payee):
        # The reward outputs, then the witness commitment.
        return pool.reward_outputs(reward, payee) + [(None, 0)]

    assert coinbase_outputs(chain, solo) == expected(finder)
    assert coinbase_outputs(chain, pooled) == expected(addresses["pool_wallet"])
    assert solo.channel["extranonce_size"] == 8
    assert len(bytes.fromhex(solo.channel["extranonce_prefix"])) == 4
    assert solo.channel["extranonce_prefix"] != pooled.channel["extranonce_prefix"]


def bip34_height(chain, coinbase_hex):
    """The height a coinbase's scriptSig commits to (BIP34)."""
    script = bytes.fromhex(chain.call("decoderawtransaction", coinbase_hex)["vin"][0]["coinbase"])
    if 0x51 <= script[0] <= 0x60:
        return script[0] - 0x50
    return int.from_bytes(script[1 : 1 + script[0]], "little")


def test_a_new_block_sends_a_future_job_that_set_new_prev_hash_activates(regtest_cluster, miners, pool):
    chain = regtest_cluster.chain
    miner = miners(pool.solo_v2_port, f"{regtest_cluster.miner_address(0)}.e2e")
    [block] = chain.mine(1, regtest_cluster.address("pool_wallet"))
    prevhash = miner.client.wait("SetNewPrevHash", channel_id=miner.channel_id)
    assert prevhash["prev_hash"] == bytes.fromhex(block)[::-1].hex(), "in the header's byte order"
    # The job it activates came first, as a future job; a mempool refresh on the old tip may precede it.
    [future] = [
        m
        for m in miner.client.messages()
        if m["message"] == "NewExtendedMiningJob" and m["job_id"] == prevhash["job_id"]
    ]
    assert future["min_ntime"] is None
    job = miner.next_job()
    assert (job["job_id"], job["prev_hash"]) == (prevhash["job_id"], prevhash["prev_hash"])
    coinbase = job["coinbase_prefix"] + miner.channel["extranonce_prefix"] + "00" * 8 + job["coinbase_suffix"]
    assert bip34_height(chain, coinbase) == chain.tip()[0] + 1


def test_sizes_the_first_target_from_the_claimed_hash_rate(pool, miners, miner_username):
    # The rate that aims at AIMED for one share per interval.
    rate = AIMED * 2**32 / SHARE_INTERVAL
    miner = miners(pool.solo_v2_port, miner_username, nominal_hash_rate=rate)
    assert miner.channel["difficulty"] == pytest.approx(AIMED, rel=1e-6)
    unclaimed = miners(pool.solo_v2_port, miner_username)
    assert unclaimed.channel["difficulty"] == pytest.approx(pool.config["vardiff"]["floor"], rel=1e-6)


def test_a_refused_open_is_answered_with_its_reason_and_logged(pool):
    mainnet = tomllib.loads((ROOT / "config" / "mainnet.toml").read_text())
    client = Sv2Client()
    client.connect(pool.solo_v2_port, pin=pool.authority_key())
    client.setup()
    reply = client.open_extended(f"{mainnet['addresses']['pool_wallet']}.e2e")
    assert reply == {"event": "channel_error", "error_code": "unknown-user"}
    # Logged at warn with its reason, as a refused V1 authorization is, and the miner may try again.
    [refused] = [line for line in pool.logs().splitlines() if "V2 channel open refused" in line]
    assert "WARN" in refused and "not an address on regtest" in refused
    assert client.alive()
    client.close()


def test_a_connection_that_opens_no_channel_is_closed(pool_factory):
    pool = pool_factory(overrides={"stratum": {"authorize_timeout_secs": 1.0}})
    client = Sv2Client()
    client.connect(pool.solo_v2_port, pin=pool.authority_key())
    client.setup()
    assert client.closed_by_pool(timeout=10)
    wait_until(lambda: "no channel opened within" in pool.logs(), 5, "the close logged")
    client.close()


def test_no_work_sends_nothing_and_keeps_the_channel(pool_factory):
    """A mainnet pool pointed at the regtest node refuses to issue work."""
    pool = pool_factory(base="mainnet")
    mainnet = tomllib.loads((ROOT / "config" / "mainnet.toml").read_text())
    client = Sv2Client()
    client.connect(pool.solo_v2_port, pin=pool.authority_key())
    client.setup()
    opened = client.open_extended(f"{mainnet['addresses']['pool_wallet']}.e2e")
    assert opened["event"] == "channel_opened"
    wait_until(lambda: "node is on chain regtest" in pool.logs(), 10, "the wrong-chain log line")
    assert client.alive()
    assert not any("Job" in m["message"] for m in client.messages())
    client.close()
