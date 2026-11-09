"""Personal choices: a username's options after its
first `+`, a finder part and `name`. A malformed, unknown, or repeated option is refused at authorize and channel
open with its reason, and the miner may try again; a well-formed one is accepted, keeps the worker's handle, and on
a pool without `personal` mines the pool's own finder part; and the lookup finds a fleet from a username pasted
with any suffix. On a pool with `personal`, each worker's block pays the part it chose, clamped to the range, and
the rest to the pool wallet, and its resource shows the part asked and paid. A window mixing parts credits each share
its work weighted by what its job leaves to the pool, as tests/e2e/pplns.py reckons it, and a worker keeping
everything journals nothing. On a pool with `worker_names_opt_in`, a block found with `+name` carries
`Mined by <worker>/` and one found without it the tag alone; on a pool with neither name flag, `+name` writes
nothing. `/api/v1/pool` gives a personal pool's range and name flags; a worker's resource says whether its name is
written, and its page why not; and its window estimate is the miner portion times the window's weighted work over
its plain work, times the worker's part of the weighted window."""

import re

import pytest

from chain import sats
from pplns import reckoned, snapshot, weighted, work
from clients.sv1 import Sv1Client, Sv1Miner
from clients.sv2 import Sv2Client
from wait import wait_until

REFUSED = [
    ("+", "an empty option"),
    ("+50%", 'option "50%"'),
    ("+12.34567", 'part "12.34567"'),
    ("+50+name+0.5btc", "a part is given twice"),
    ("+name+name", "name is given twice"),
]


def handles(pool, event):
    return re.findall(rf"{event}.*handle=([a-z2-7]{{16}})", pool.logs())


@pytest.mark.parametrize("suffix,reason", REFUSED, ids=[suffix for suffix, _ in REFUSED])
def test_a_malformed_suffix_is_refused_over_v1_and_v2_with_its_reason(pool, miner_username, suffix, reason):
    client = Sv1Client(pool.hybrid_v1_port)
    client.subscribe()
    response = client.request("mining.authorize", f"{miner_username}{suffix}", "x")
    assert response["error"][0] == 24 and reason in response["error"][1], response
    assert client.alive(), "the miner may try again"
    assert client.authorize(miner_username) is True
    client.close()

    v2 = Sv2Client()
    v2.connect(pool.hybrid_v2_port, pin=pool.authority_key())
    v2.setup()
    reply = v2.open_extended(f"{miner_username}{suffix}")
    assert reply == {"event": "channel_error", "error_code": "unknown-user"}
    [refused] = [line for line in pool.logs().splitlines() if "V2 channel open refused" in line]
    assert reason in refused
    assert v2.alive()
    v2.close()


def test_a_suffix_keeps_the_handle_and_on_a_pool_without_personal_mines_its_finder(
    pool, regtest_cluster, miner_username, solution_forcer
):
    plain = Sv1Miner(pool.hybrid_v1_port, miner_username)
    plain.client.close()
    chose = Sv1Miner(pool.hybrid_v1_port, f"{miner_username}+100+name")
    [first, second] = handles(pool, "V1 authorized")
    assert first == second, "options are not identity"

    _, block_hash = solution_forcer(chose)
    chose.client.close()
    chain = regtest_cluster.chain
    wait_until(lambda: chain.tip()[1] == block_hash, 10, "the block at the tip")
    stats = chain.call("getblockstats", block_hash, ["subsidy", "totalfee"])
    vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
    paid = [(o["scriptPubKey"].get("address"), sats(o["value"])) for o in vout]
    finder = regtest_cluster.miner_address(0)
    expected = pool.reward_outputs(stats["subsidy"] + stats["totalfee"], finder, hybrid=True)
    assert paid[: len(expected)] == expected, "the hybrid pool's 33.33%, not the 100% asked for"


def test_the_lookup_finds_a_fleet_from_a_username_with_any_suffix(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    [handle] = handles(pool, "V1 authorized")
    rows = wait_until(lambda: pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle), 10, "the fleet row")
    fleet = rows[0]["fleet"]
    for suffix in ["+33.33+name", "+0.5btc", "+", "+not-an-option"]:
        found = pool.portal.api("/api/v1/lookup", method="POST", body={"username": f"{miner_username}{suffix}"})
        assert found.json == {"fleet": fleet}, suffix
    miner.client.close()


def split_pool(most=50):
    """A pool letting each worker choose a part from 0% to `most`%, 33.33% for one that chooses none."""
    return {
        "name": "Your split",
        "v1": True,
        "v2": True,
        "finder": {"percent": 33.33},
        "personal": {"min": {"percent": 0}, "max": {"percent": most}},
    }


@pytest.mark.parametrize(
    "suffix,clamped",
    [("+100", {"percent": 50}), ("+0", {"percent": 0}), ("+12.5", {"percent": 12.5}), ("", {"percent": 33.33})],
    ids=["above-max", "zero", "inside", "default"],
)
def test_a_personal_block_pays_the_part_chosen_clamped_to_the_range(
    pool_factory, regtest_cluster, solution_forcer, suffix, clamped
):
    pool = pool_factory(overrides={"pools": {"split": split_pool()}})
    finder = regtest_cluster.miner_address(0)
    miner = Sv1Miner(pool.port("split", "v1"), f"{finder}.rig{suffix}")
    _, block_hash = solution_forcer(miner)
    miner.client.close()
    chain = regtest_cluster.chain
    wait_until(lambda: chain.tip()[1] == block_hash, 10, "the block at the tip")
    stats = chain.call("getblockstats", block_hash, ["subsidy", "totalfee"])
    vout = chain.call("getblock", block_hash, 2)["tx"][0]["vout"]
    paid = [(o["scriptPubKey"].get("address"), sats(o["value"])) for o in vout]
    expected = pool.reward_outputs(stats["subsidy"] + stats["totalfee"], finder, pool="split", finder=clamped)
    assert paid[: len(expected)] == expected
    wallet = pool.config["addresses"]["pool_wallet"]
    assert (finder in dict(paid)) == (clamped["percent"] > 0) and wallet in dict(paid)


def test_the_worker_resource_shows_the_part_asked_and_paid(pool_factory, regtest_cluster):
    pool = pool_factory(overrides={"pools": {"split": split_pool()}})
    finder = regtest_cluster.miner_address(0)
    asked = Sv1Miner(pool.port("split", "v1"), f"{finder}.asked+100")
    default = Sv1Miner(pool.port("split", "v1"), f"{finder}.default")
    [asked_handle, default_handle] = handles(pool, "V1 authorized")
    reward = regtest_cluster.chain.call("getblocktemplate", {"rules": ["segwit"]})["coinbasevalue"]
    for handle, part, clamped in [
        (asked_handle, {"ppm": 1_000_000}, {"percent": 50}),
        (default_handle, None, {"percent": 33.33}),
    ]:
        # A worker's part and its job are recorded just after it is authorized.
        body = wait_until(
            lambda: (body := pool.portal.api(f"/api/v1/workers/{handle}").json)["finder_sats"] is not None and body,
            10,
            "the worker's job recorded",
        )
        [(_, finder_sats), *_] = pool.reward_outputs(reward, finder, pool="split", finder=clamped)
        assert (body["part"], body["finder_sats"]) == (part, finder_sats), handle
    # A pool without personal lets no worker choose a part, so the app shows none.
    solo = Sv1Miner(pool.solo_v1_port, f"{finder}.solo+50")
    [*_, solo_handle] = handles(pool, "V1 authorized")
    assert pool.portal.api(f"/api/v1/workers/{solo_handle}").json["pool"] == "solo"
    [setup] = [setup for setup in pool.portal.api("/api/v1/pool").json["pools"] if setup["id"] == "solo"]
    assert setup["personal"] is None
    for miner in (asked, default, solo):
        miner.client.close()


@pytest.mark.parametrize("finder", ["+0", "+33.33"])
def test_a_mixed_window_credits_each_share_by_its_contribution(pool_factory, regtest_cluster, solution_forcer, finder):
    pool = pool_factory(overrides={"pools": {"split": split_pool(100)}})
    port = pool.port("split", "v1")
    keeper, third, giver = (regtest_cluster.miner_address(n) for n in range(3))
    miners = {
        "+100": Sv1Miner(port, f"{keeper}.rig+100"),
        "+33.33": Sv1Miner(port, f"{third}.rig+33.33"),
        "+0": Sv1Miner(port, f"{giver}.rig+0"),
    }
    for _ in range(3):
        for miner in miners.values():
            assert miner.submit(miner.share()) is True
    _, block_hash = solution_forcer(miners[finder])
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block answered")

    journal = pool.journal()
    assert keeper not in {row["address"] for row in journal}, "a share keeping everything earns from no window"
    contribution = {row["address"]: row["pool_sats"] / row["miner_sats"] for row in journal}
    assert contribution[giver] == 1 and contribution[third] == pytest.approx(1 - 0.3333 / 0.985, abs=1e-6)
    snap, allocated = snapshot(pool, block_hash)
    assert snap["amount"] == pool.block(block_hash)["pool_sats"] > 0
    reckoning, credits = reckoned(pool, snap)
    assert allocated == reckoning, "as tests/e2e/pplns.py reckons it from the journal"
    assert set(allocated) == {third, giver}
    for address in (third, giver):
        assert credits[address] == sum(weighted(row, 2) for row in journal if row["address"] == address)
    for miner in miners.values():
        miner.client.close()


def script_sig(chain, block_hash):
    return bytes.fromhex(chain.call("getblock", block_hash, 2)["tx"][0]["vin"][0]["coinbase"])


def test_a_name_is_written_only_for_a_worker_that_asks_on_a_pool_that_lets_it(
    pool_factory, regtest_cluster, solution_forcer
):
    names = {**split_pool(), "coinbase": {"worker_names_opt_in": True}}
    pool = pool_factory(overrides={"coinbase": {"tag": "/e2e pool/"}, "pools": {"names": names}})
    chain = regtest_cluster.chain
    address = regtest_cluster.miner_address(0)
    for port, username, written in [
        (pool.port("names", "v1"), f"{address}.asker+name", True),
        (pool.port("names", "v1"), f"{address}.quiet", False),
        (pool.port("names", "v1"), f"{address}+name", False),  # a bare address has no name to write
        (pool.hybrid_v1_port, f"{address}.asker+name", False),  # neither flag on the hybrid pool
    ]:
        miner = Sv1Miner(port, username)
        _, block_hash = solution_forcer(miner)
        miner.client.close()
        wait_until(lambda: chain.tip()[1] == block_hash, 10, "the block at the tip")
        signed = script_sig(chain, block_hash)
        assert b"/e2e pool/" in signed, username
        assert (b"/e2e pool/Mined by asker/" in signed) == written, username
        assert (b"Mined by" in signed) == written, username


def test_the_pool_resource_gives_a_personal_pools_range_and_name_flags(pool_factory):
    names = {**split_pool(), "coinbase": {"worker_names_opt_in": True}}
    pool = pool_factory(overrides={"pools": {"split": names}})
    setups = {setup["id"]: setup for setup in pool.portal.api("/api/v1/pool").json["pools"]}
    entry = setups["split"]
    assert entry["kind"] == "personal" and entry["finder"] == {"ppm": 333_300}
    assert entry["personal"] == {"min": {"ppm": 0}, "max": {"ppm": 500_000}}
    assert (entry["worker_names"], entry["worker_names_opt_in"]) == (False, True)
    hybrid = setups["hybrid"]
    assert hybrid["personal"] is None, "no worker chooses a part there"
    assert (hybrid["worker_names"], hybrid["worker_names_opt_in"]) == (False, False)


def test_a_worker_resource_says_whether_its_name_is_written_and_whether_it_asked(pool_factory, regtest_cluster):
    names = {**split_pool(), "coinbase": {"worker_names_opt_in": True}}
    pool = pool_factory(overrides={"pools": {"names": names}})
    address = regtest_cluster.miner_address(0)
    # Each worker: whether its name is written, and whether it asked. A client words why from these and its pool's
    # flags: asked on an opt-in pool and not written is a bare address; asked on a pool writing no names writes
    # nothing.
    cases = [
        (pool.port("names", "v1"), f"{address}.asker+name", True, True),
        (pool.port("names", "v1"), f"{address}.quiet", False, False),
        (pool.port("names", "v1"), f"{address}+name", False, True),
        (pool.hybrid_v1_port, f"{address}.asker+name", False, True),
        (pool.solo_v1_port, f"{address}.plain", False, False),
    ]
    miners = []
    for port, username, written, asked in cases:
        miners.append(Sv1Miner(port, username))
        [*_, handle] = handles(pool, "V1 authorized")
        body = pool.portal.api(f"/api/v1/workers/{handle}").json
        assert (body["name_written"], body["name_asked"]) == (written, asked), username
    for miner in miners:
        miner.client.close()


def test_the_window_estimate_takes_the_expected_pool_output_of_a_mixed_window(pool_factory, regtest_cluster):
    pool = pool_factory(overrides={"pools": {"split": split_pool(100)}})
    port = pool.port("split", "v1")
    giver, half = regtest_cluster.miner_address(0), regtest_cluster.miner_address(1)
    miners = [Sv1Miner(port, f"{giver}.rig+0"), Sv1Miner(port, f"{half}.rig+50")]
    giver_handle, half_handle = handles(pool, "V1 authorized")
    for _ in range(3):
        for miner in miners:
            assert miner.submit(miner.share()) is True

    # W_min holds the whole journal: the window is every share, each weighted by its own job's contribution.
    journal = pool.journal()
    plain = sum(work(row["target"]) for row in journal)
    reward = regtest_cluster.chain.call("getblocktemplate", {"rules": ["segwit"]})["coinbasevalue"]
    miner_portion = reward - sum(fee for _, fee in pool.reward_outputs(reward, giver)[1:])
    for handle in (giver_handle, half_handle):
        credits = sum(weighted(row, 2) for row in journal if row["handle"] == handle)
        expected = miner_portion * credits / plain
        window = wait_until(
            lambda: pool.portal.api(f"/api/v1/workers/{handle}").json["live"]["window"].get("estimate_sats"),
            15,
            "a window walked",
        )
        assert window == pytest.approx(expected, abs=2), handle
    for miner in miners:
        miner.client.close()
