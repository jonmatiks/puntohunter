"""Fleets: every worker mining to one address, over either transport and in either mode, is recorded
under one fleet handle with its worker name and address, a bare-address worker with an empty name, and no other
column holds the address."""

import re

from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from wait import wait_until

HANDLE = re.compile(r"handle=([a-z2-7]{16})\b")


def handles(pool, event):
    return [HANDLE.search(line)[1] for line in pool.logs().splitlines() if event in line]


def test_workers_on_one_address_share_one_fleet(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    miners = [
        Sv1Miner(pool.solo_v1_port, f"{address}.alpha"),
        Sv1Miner(pool.pooled_v1_port, f"{address}.alpha"),
        Sv1Miner(pool.solo_v1_port, address),
    ]
    sv2 = [
        Sv2Miner(pool, pool.solo_v2_port, f"{address}.beta"),
        Sv2Miner(pool, pool.pooled_v2_port, f"{address}.beta"),
    ]
    other = Sv1Miner(pool.solo_v1_port, f"{regtest_cluster.miner_address(1)}.alpha")
    workers = handles(pool, "V1 authorized")[:3] + handles(pool, "V2 channel opened")

    rows = wait_until(lambda: len(pool.rows("SELECT * FROM fleets")) == 6 and pool.rows("SELECT * FROM fleets"), 10, "fleet rows")
    ours = [row for row in rows if row["handle"] in workers]
    assert len(ours) == 5
    assert len({row["fleet"] for row in ours}) == 1, "one fleet handle for the address"
    [theirs] = [row for row in rows if row["handle"] not in workers]
    assert theirs["fleet"] != ours[0]["fleet"], "another address is another fleet"
    assert theirs["fleet"] not in workers and ours[0]["fleet"] not in workers

    by_handle = {row["handle"]: row for row in ours}
    assert [(by_handle[h]["pool"], by_handle[h]["worker"]) for h in workers] == [
        ("solo", "alpha"),
        ("pooled", "alpha"),
        ("solo", ""),
        ("solo", "beta"),
        ("pooled", "beta"),
    ]

    # A reconnect writes nothing new.
    again = Sv1Miner(pool.solo_v1_port, f"{address}.alpha")
    assert again.submit(again.share()) is True
    assert len(pool.rows("SELECT * FROM fleets")) == 6

    assert {row["address"] for row in ours} == {address}
    assert theirs["address"] == regtest_cluster.miner_address(1)
    for row in rows:
        for column, value in row.items():
            if column == "address":
                continue
            for payout in pool.payout_addresses:
                assert payout.lower() not in str(value).lower(), row

    for miner in miners + [other, again]:
        miner.client.close()
    for miner in sv2:
        miner.close()
