"""The pool stats: `GET /api/v1/stats`, and the pool resource's best
share this round. A block a node accepts starts a new round without its solution share, and one it refuses starts
nothing; a restart keeps the lists. Fleets rank by their workers together, over V1 and V2, and each fleet's resource
gives its ranks. Rows show a fleet by its address's first 6 and last 4 characters only, never more of it, a worker
name, or a handle. Miner types fold into "Other" past `[stats] miner_types` and sum to the pool's connected workers.
With `[geo]`, countries come from the peer address, those under `min_workers` folding into "Elsewhere"; without it,
there are none. With `show_members`, the group resource lists each instance by its label. Every response passes the
privacy scan."""

import re
import time

import pytest

import geo
import pool as pool_module
from clients.http import listed
from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until

PERIOD = 2
TR, JP = ("TR", "Türkiye"), ("JP", "Japan")
REFRESH = 0.5
GONE_WITHIN = REFRESH * 3 + 5
NAMES = ["member-alpha", "member-bravo", "member-charlie"]
LABELS = ["Alpha <A>", "Bravo", "Charlie"]
LINK = "https://alpha.example"


def stats(pool):
    response = pool.portal.api("/api/v1/stats")
    assert response.status == 200, response.body
    assert response.header("cache-control") == "no-cache"
    return response.json


def round_best_share(pool):
    """The best share this round, as the pool resource gives it."""
    return pool.portal.api("/api/v1/pool").json["round_best_share"]


def addresses(rows):
    return [row["short_address"] for row in rows]


def fleet_of(pool, username):
    response = pool.portal.api("/api/v1/lookup", method="POST", body={"username": username})
    assert response.status == 200, response.body
    return response.json["fleet"]


def handles(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))


def kept(rows):
    """A best-share list as a restart must keep it: the time is read back to the start of its rollup period."""
    return [(row["short_address"], row["best"], row["miner"], row["pool"]) for row in rows]


def test_a_block_starts_a_round_and_a_refused_one_does_not(pool_factory, regtest_cluster, solution_forcer):
    pool = pool_factory(overrides={"telemetry": {"rollup_secs": PERIOD}})
    chain = regtest_cluster.chain
    body = stats(pool)
    assert body["round"] == {"started_at": None, "since_block": None}
    assert body["best_shares"] == {"round": [], "last_24h": []}
    assert (body["hash_rates"], body["miners"], body["fleets"], body["workers"]) == ([], [], 0, 0)
    assert not {"countries", "country_min_workers", "country_source"} & set(body), "no [geo]"
    assert body["limits"] == {
        "best_shares_round": 10,
        "best_shares_24h": 10,
        "hash_rates": 10,
        "miner_types": 10,
        "countries": 10,
    }
    assert round_best_share(pool) is None

    a_address, b_address = regtest_cluster.miner_address(0), regtest_cluster.miner_address(1)
    a = Sv1Miner(pool.solo_v1_port, f"{a_address}.rig")
    b = Sv1Miner(pool.solo_v1_port, f"{b_address}.rig")
    assert a.submit(a.share()) is True
    wait_until(
        lambda: addresses(stats(pool)["best_shares"]["round"]) == [listed(a_address)], 10, "a's share listed"
    )
    assert stats(pool)["round"]["started_at"] is not None, "the round starts with the first share"
    assert round_best_share(pool) == stats(pool)["best_shares"]["round"][0]["best"]

    # b finds a block the node accepts: the next round starts at it, without b's solution share.
    _, block_hash = solution_forcer(b)
    wait_until(lambda: pool.block(block_hash).get("result") == "accepted", 10, "the block accepted")
    block = pool.block(block_hash)
    body = wait_until(
        lambda: (s := stats(pool))["round"]["since_block"] == block["height"] and s, 10, "the next round"
    )
    assert body["round"]["started_at"] == block["found_at"]
    assert body["best_shares"]["round"] == [], "neither a's share before the block nor b's solution share"
    assert round_best_share(pool) is None
    assert set(addresses(body["best_shares"]["last_24h"])) == {listed(a_address), listed(b_address)}

    # After the block's own rollup period, which a restart leaves out, a finds a block the node refuses: the
    # node's clock is hours behind its ntime. The round stays, and a's share is on it.
    boundary = (block["found_at"] // PERIOD + 1) * PERIOD
    wait_until(lambda: time.time() >= boundary + 0.2, PERIOD + 2, "the block's period over")
    a.clean_job()
    chain.call("setmocktime", int(a.notify[7], 16) - 3 * 3600)
    try:
        _, refused = solution_forcer(a)
        wait_until(lambda: pool.block(refused).get("status") == "rejected", 10, "the refusal recorded")
    finally:
        chain.call("setmocktime", 0)
    body = stats(pool)
    assert body["round"]["since_block"] == block["height"]
    [row] = body["best_shares"]["round"]
    assert (row["short_address"], row["miner"], row["pool"]) == (listed(a_address), "puntohunter-e2e", "solo")
    assert row in body["best_shares"]["last_24h"]

    # Once its period is written, a restart keeps both lists.
    wait_until(
        lambda: pool.rows(
            "SELECT 1 FROM rollups WHERE start >= ? AND best >= ?", boundary, row["best"] * 0.999999
        ),
        PERIOD * 3 + 5,
        "the refused block's share written",
    )
    before = stats(pool)
    pool.restart()
    after = stats(pool)
    assert after["round"] == before["round"]
    for key in ("round", "last_24h"):
        assert kept(after["best_shares"][key]) == kept(before["best_shares"][key]), key
        for was, now in zip(before["best_shares"][key], after["best_shares"][key]):
            assert 0 <= was["at"] - now["at"] < PERIOD, (was, now)
    for miner in (a, b):
        miner.client.close()


def test_fleets_rank_by_their_workers_together(pool_factory, regtest_cluster):
    pool = pool_factory(overrides={"telemetry": {"rollup_secs": PERIOD}})
    a_address, b_address = regtest_cluster.miner_address(2), regtest_cluster.miner_address(3)
    a1 = Sv1Miner(pool.solo_v1_port, f"{a_address}.wkr-left")
    a2 = Sv1Miner(pool.pooled_v1_port, f"{a_address}.wkr-right")
    b = Sv2Miner(pool, pool.solo_v2_port, f"{b_address}.wkr-v2", vendor="NerdQAxe++/BM1370/v1.0")
    for _ in range(3):
        for miner in (a1, a2):
            assert miner.submit(miner.share()) is True
    b.submit(count=3)
    fleets = {a_address: fleet_of(pool, f"{a_address}.wkr-left"), b_address: fleet_of(pool, f"{b_address}.wkr-v2")}
    body = wait_until(
        lambda: (s := stats(pool))["fleets"] == 2 and len(s["best_shares"]["round"]) == 2 and s,
        10,
        "both fleets listed",
    )
    assert body["workers"] == 3
    rates = {row["short_address"]: row for row in body["hash_rates"]}
    assert set(rates) == {listed(a_address), listed(b_address)}
    assert (rates[listed(a_address)]["workers"], rates[listed(b_address)]["workers"]) == (2, 1)
    devices = pool.portal.api(f"/api/v1/fleets/{fleets[a_address]}").json["devices"]
    summed = sum(device["hash_rate"]["hash_rate"] for device in devices)
    assert rates[listed(a_address)]["hash_rate"] == pytest.approx(summed, rel=0.1), "its devices summed"
    assert {row["miner"] for row in body["best_shares"]["round"]} == {"puntohunter-e2e", "NerdQAxe++"}

    body = stats(pool)
    for address, fleet in fleets.items():
        resource = pool.portal.api(f"/api/v1/fleets/{fleet}").json
        place = lambda rows: addresses(rows).index(listed(address)) + 1  # noqa: E731
        assert resource["ranks"] == {
            "best_share": place(body["best_shares"]["round"]),
            "best_share_24h": place(body["best_shares"]["last_24h"]),
            "hash_rate": place(body["hash_rates"]),
        }, address

    # Each fleet in its 6…4 form, and nothing longer of it; no worker name, and no handle.
    named = handles(pool, "V1 authorized") + handles(pool, "V2 channel opened") + list(fleets.values())
    text = pool.portal.api("/api/v1/stats").body
    for address in fleets:
        assert listed(address) in text
        assert address[:7] not in text and address[-5:] not in text, address
    for word in ["wkr-left", "wkr-right", "wkr-v2", *named]:
        assert word not in text, word
    for miner in (a1, a2):
        miner.client.close()
    b.close()


def test_miner_types_fold_past_their_length_and_sum_to_the_connected(pool_factory, regtest_cluster):
    pool = pool_factory(overrides={"stats": {"miner_types": 5}})
    address = regtest_cluster.miner_address(4)
    agents = ["bitaxe/BM1370/v2.15.3", "bitaxe/BM1366/v2.9.0", "BitAxe", "NerdQAxe++/BM1370/v1.0", ""]
    agents += [f"filler{letter}/1.0" for letter in "abcdefghij"]
    miners = [Sv1Miner(pool.solo_v1_port, f"{address}.m{n}", agent=agent) for n, agent in enumerate(agents)]
    # The three bitaxes and the NerdQAxe++ mine; the rest connect only.
    for miner in miners[:3] + [miners[3]] * 3:
        assert miner.submit(miner.share()) is True
    body = wait_until(lambda: (s := stats(pool))["workers"] == len(agents) and s, 10, "every worker counted")
    assert [row["kind"] for row in body["miners"]] == ["type"] * 5 + ["other", "unknown"]
    named = {row["miner"]: row for row in body["miners"] if row["kind"] == "type"}
    assert named["bitaxe"]["workers"] == 3, "grouped regardless of case, spelt as most of its workers are"
    assert named["NerdQAxe++"]["workers"] == 1 and named["NerdQAxe++"]["best"] > 0
    assert (body["miners"][-2]["workers"], body["miners"][-1]["workers"]) == (7, 1), "seven fillers in Other"
    assert body["miners"][-1]["miner"] is None and body["miners"][-2]["miner"] is None
    connected = sum(setup["devices"] for setup in pool.portal.api("/api/v1/pool").json["pools"])
    assert sum(row["workers"] for row in body["miners"]) == connected == len(agents)
    assert pool.rows("SELECT handle FROM fleets WHERE vendor = 'NerdQAxe++'"), "recorded with its '+'"
    assert body["limits"]["miner_types"] == 5, "the first 5 by hashrate"
    for miner in miners:
        miner.client.close()


def test_countries_come_from_the_peer_address(pool_factory, regtest_cluster, tmp_path):
    database = geo.write(
        tmp_path / "country.mmdb", {"127.0.0.2": TR, "127.0.0.3": TR, "127.0.0.4": TR, "127.0.0.5": JP}
    )
    pool = pool_factory(overrides={"geo": {"database": str(database), "min_workers": 3}})
    address = regtest_cluster.miner_address(5)
    v1 = [
        Sv1Miner(pool.solo_v1_port, f"{address}.tr-1", source="127.0.0.2"),
        Sv1Miner(pool.solo_v1_port, f"{address}.tr-2", source="127.0.0.3"),
        Sv1Miner(pool.solo_v1_port, f"{address}.jp-1", source="127.0.0.5"),
        # 127.0.0.1, which the database gives no country.
        Sv1Miner(pool.solo_v1_port, f"{address}.home"),
    ]
    v2 = Sv2Miner(pool, pool.solo_v2_port, f"{address}.tr-3", source="127.0.0.4")
    body = wait_until(lambda: (s := stats(pool))["workers"] == 5 and s, 10, "every worker counted")
    assert [(row["kind"], row["country"], row["name"], row["workers"]) for row in body["countries"]] == [
        ("country", "TR", "Türkiye", 3),
        ("elsewhere", None, None, 1),
        ("unknown", None, None, 1),
    ]
    assert body["country_min_workers"] == 3
    assert body["country_source"] == "dbip", "the test database is typed as DB-IP's"
    text = pool.portal.api("/api/v1/stats").body
    assert "Japan" not in text and '"JP"' not in text, "a country of one worker is not named"
    for peer in ("127.0.0.2", "127.0.0.3", "127.0.0.4", "127.0.0.5"):
        assert peer not in text
    for miner in v1:
        miner.client.close()
    v2.close()


@pytest.fixture
def labelled(pool_binary, regtest_cluster, tmp_path):
    """Three instances on the cluster with show_members, each listing the other two by label."""
    pools = [
        pool_module.Pool(pool_binary, regtest_cluster, tmp_path / f"instance-{i}") for i in range(3)
    ]
    for i, pool in enumerate(pools):
        pool.config["group"] = {
            "name": "E2E Group",
            "refresh_secs": REFRESH,
            "label": LABELS[i],
            "show_members": True,
            "members": [
                {
                    "name": NAMES[j],
                    "url": f"http://127.0.0.1:{other.portal_port}",
                    "label": LABELS[j],
                    **({"link": LINK} if j == 0 else {}),
                }
                for j, other in enumerate(pools)
                if j != i
            ],
        }
    try:
        for pool in pools:
            pool.start()
        yield pools
    finally:
        for pool in pools:
            pool.kill()


def group_of(pool):
    response = pool.portal.api("/api/v1/group")
    assert response.status == 200, response.body
    return response.json


def test_instances_are_listed_by_label_with_show_members(labelled):
    for pool in labelled:
        wait_until(lambda: group_of(pool)["reporting"] == 3, GONE_WITHIN, "all three counted")
    body = group_of(labelled[1])
    assert [(m["label"], m["this"], m["reporting"], m["link"]) for m in body["members"]] == [
        ("Bravo", True, True, None),
        ("Alpha <A>", False, True, LINK),
        ("Charlie", False, True, None),
    ], "this instance first, then the members in config order"
    assert all(isinstance(m["devices"], int) for m in body["members"])

    labelled[2].kill()
    wait_until(
        lambda: not group_of(labelled[1])["members"][2]["reporting"], GONE_WITHIN, "the stopped one not reporting"
    )
    charlie = group_of(labelled[1])["members"][2]
    assert (charlie["hash_rate"], charlie["devices"], charlie["blocks_found"]) == (None, None, None)
    text = labelled[1].portal.api("/api/v1/group").body
    for pool in labelled:
        assert f"127.0.0.1:{pool.portal_port}" not in text
        assert not any(name in text for name in NAMES)
