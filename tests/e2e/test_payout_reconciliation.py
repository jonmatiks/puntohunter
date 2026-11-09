"""Startup reconciliation: payouts run only when the books, the payout wallet,
and the chain agree. A database restored from a copy taken before a batch paid lags the wallet, so payouts
stay halted, start after start, and nothing is paid twice; a clean start runs them."""

import contextlib
import shutil
import sqlite3
import time

from payouts import batches, broadcast, confirm, earn, mature, metric, payments, start
from wait import wait_until

DATABASE = "puntohunter.sqlite3"


def copy(pool, path):
    """Copies the stopped pool's database to `path`, its write-ahead log included."""
    with (
        contextlib.closing(sqlite3.connect(pool.data_dir / DATABASE)) as db,
        contextlib.closing(sqlite3.connect(path)) as backup,
    ):
        db.backup(backup)


def restore(pool, path):
    """Puts the copy at `path` back in place of the stopped pool's database."""
    for suffix in ("", "-wal", "-shm"):
        (pool.data_dir / f"{DATABASE}{suffix}").unlink(missing_ok=True)
    shutil.copy(path, pool.data_dir / DATABASE)


def test_a_restored_backup_keeps_payouts_halted_and_a_clean_start_runs_them(
    pool_factory, fresh_node, solution_forcer, tmp_path
):
    # No batch until the copy is taken.
    pool = start(pool_factory, fresh_node, payouts={"interval_secs": 3600.0})
    owed = earn(pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1])
    mature(pool, fresh_node)
    pool.kill()
    backup = tmp_path / "backup.sqlite3"
    copy(pool, backup)

    pool.config["payouts"]["interval_secs"] = 0.5
    pool.start()
    wait_until(lambda: "payouts reconciled with the wallet and the chain" in pool.logs(), 10, "reconciled")
    assert metric(pool, "puntohunter_payouts_halted") == 0
    broadcast(pool)
    [batch] = confirm(pool, fresh_node)
    pool.kill()

    restore(pool, backup)
    failed = "payout reconciliation failed: "
    unknown = f"{failed}the wallet sent transaction {batch['txid']}, which no batch made"
    for starts in (1, 2):
        pool.start()
        wait_until(lambda: metric(pool, "puntohunter_payouts_halted") == 1, 10, "payouts halted")
        # The log runs on across starts: each start finds both mismatches.
        assert pool.logs().count(unknown) == starts
        assert pool.logs().count(f"{failed}the wallet holds") == starts, "short of what the copy owes"
        # Several intervals, and the copy's balances, all available, are not paid again.
        time.sleep(2)
        assert batches(pool) == []
        for address in owed:
            assert payments(fresh_node, address) == {batch["txid"]}
        pool.kill()
