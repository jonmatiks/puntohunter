"""The acceptance reports, run as a live run runs them, by reports.py's command line: the
conservation report finds a regtest pool's pooled and hybrid blocks, their allocations, balances, and a paid
batch balanced, and names a difference planted in a copy of its database; the live privacy scan fetches every
API route, stream, and series of a pool with every resource populated and finds no payout address."""

import contextlib
import sqlite3
import subprocess
import sys

from clients.sv1 import Sv1Miner
from clients.sv2 import Sv2Miner
from payouts import broadcast, confirm, earn, mature, start
from regtest import E2E
from wait import wait_until

MODES = ("solo", "pooled", "hybrid")


def report(pool, name, *args):
    return subprocess.run(
        [sys.executable, E2E / "reports.py", name, "--config", pool.config_path, *args],
        capture_output=True,
        text=True,
        timeout=60,
    )


def test_the_conservation_report_balances_a_paid_run_and_names_a_planted_difference(
    pool_factory, fresh_node, solution_forcer, tmp_path
):
    pool = start(pool_factory, fresh_node)
    pooled = earn(pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1])
    earn(pool, fresh_node, solution_forcer, "hybrid", finder=0)
    mature(pool, fresh_node)
    broadcast(pool)
    [batch] = confirm(pool, fresh_node)

    balanced = report(pool, "conservation")
    assert balanced.returncode == 0, balanced.stdout + balanced.stderr
    assert balanced.stdout.rstrip().endswith("balanced"), balanced.stdout
    assert "2 blocks with a pool output, 2 on the chain" in balanced.stdout, balanced.stdout
    assert f"batch {batch['id']}, confirmed {batch['txid']}: pays " in balanced.stdout, balanced.stdout
    assert f"fee {batch['fee']} sats" in balanced.stdout, balanced.stdout

    # A copy of the database, one pooled allocation a satoshi larger than its block's coinbase allows.
    copy = tmp_path / "planted.sqlite3"
    with contextlib.closing(sqlite3.connect(pool.data_dir / "puntohunter.sqlite3")) as source:
        with contextlib.closing(sqlite3.connect(copy)) as planted:
            source.backup(planted)
            [block] = {row["block"] for row in pool.rows("SELECT block FROM allocations WHERE pool = 'pooled'")}
            address = fresh_node.miner_address(1)
            planted.execute(
                "UPDATE allocations SET sats = sats + 1 WHERE block = ? AND address = ?", (block, address)
            )
            planted.commit()
    found = report(pool, "conservation", "--database", copy)
    assert found.returncode == 1, found.stdout + found.stderr
    assert f"its allocations sum to {sum(pooled.values()) + 1} sats" in found.stdout
    assert f"- {address} (pooled): allocated {pooled[address] + 1} sats" in found.stdout
    assert "differences:" in found.stdout


def test_the_privacy_scan_covers_every_route_of_a_populated_pool(pool_factory, regtest_cluster, solution_forcer):
    pool = pool_factory(overrides={"telemetry": {"rollup_secs": 2}})
    v1 = {mode: Sv1Miner(getattr(pool, f"{mode}_v1_port"), f"{regtest_cluster.miner_address(0)}.rig") for mode in MODES}
    v2 = {
        mode: Sv2Miner(pool, getattr(pool, f"{mode}_v2_port"), f"{regtest_cluster.miner_address(1)}.rig")
        for mode in MODES
    }
    for mode in MODES:
        assert v1[mode].submit(v1[mode].share()) is True
        assert not v2[mode].submit(2)["errors"]
    for found, mode in enumerate(MODES):
        # Onto the tip each block before it made.
        for _ in range(found):
            v1[mode].clean_job()
        solution_forcer(v1[mode])
    wait_until(lambda: pool.rows("SELECT 1 FROM allocations"), 10, "the pooled block's allocations")
    wait_until(lambda: len(pool.rows("SELECT DISTINCT handle FROM rollups")) == 6, 10, "every device's rollups")
    token = pool.issue_token(regtest_cluster.miner_address(0))

    scan = report(pool, "privacy", "--token", token)
    assert scan.returncode == 0, scan.stdout + scan.stderr
    assert scan.stdout.rstrip().endswith("no payout address leaked"), scan.stdout
    lines = scan.stdout.splitlines()
    fetched = lambda pattern: [line for line in lines if pattern in line]
    assert len(fetched("GET /api/v1/fleets/")) == 2 * (1 + 1 + 3 + 1), "a resource, blocks, three series, a stream"
    assert len(fetched("GET /api/v1/workers/")) == 6 * (1 + 1 + 3 + 1)
    resources = fetched("GET /api/v1/fleets/") + fetched("GET /api/v1/workers/")
    assert all(line.endswith(": 200") or ": event " in line for line in resources), scan.stdout
    assert all(line.endswith(": 200") for line in fetched("/series?")), scan.stdout
    assert all("event" in line for line in fetched("/events:")), scan.stdout
    lookups = fetched("POST /api/v1/lookup")
    assert len(lookups) == 2 and all(line.endswith(": 200") for line in lookups)
    for line in (
        "GET /metrics: 200", "GET /api/v1/pool: 200", "GET /api/v1/blocks: 200", "GET /api/v1/health: 200",
        "GET /api/v1/stats: 200", "GET /api/v1/balance: 200",
    ):
        assert line in lines, line
    for miner in [*(m.client for m in v1.values()), *v2.values()]:
        miner.close()
