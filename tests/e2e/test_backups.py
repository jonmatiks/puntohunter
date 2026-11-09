"""Database snapshots: with `[backup]`, the pool writes a snapshot at
start, one at once after a block with a pool output is allocated and after a payout batch is broadcast, each
holding what the event wrote, and a database restored from the last one starts, reconciles with the payout
wallet, and pays nothing twice."""

import contextlib
import shutil
import sqlite3

from payouts import batches, broadcast, earn, mature, start
from wait import wait_until


def snapshots(pool):
    return sorted((pool.data_dir / "backups").glob("puntohunter-*.sqlite3"), key=lambda p: int(p.stem.split("-")[1]))


def rows(path, sql):
    with contextlib.closing(sqlite3.connect(f"file:{path}?mode=ro", uri=True)) as db:
        return db.execute(sql).fetchall()


def test_snapshots_follow_allocations_and_batches_and_a_restore_reconciles(pool_factory, fresh_node, solution_forcer):
    # An hour between turns, so every snapshot after the first is one asked for out of turn.
    pool = start(pool_factory, fresh_node, overrides={"backup": {"interval_secs": 3600}})
    wait_until(lambda: len(snapshots(pool)) == 1, 10, "the snapshot at start")

    earn(pool, fresh_node, solution_forcer, "pooled", finder=0, others=[1])
    taken = wait_until(lambda: len(s := snapshots(pool)) == 2 and s, 10, "a snapshot after the allocation")
    assert rows(taken[-1], "SELECT COUNT(*) FROM allocations")[0][0] > 0, "it holds the allocation"

    mature(pool, fresh_node)
    [batch] = broadcast(pool)
    # The batch's own snapshot, once the broadcast is recorded: the maturing blocks ask for none.
    taken = wait_until(
        lambda: (s := snapshots(pool))
        and rows(s[-1], "SELECT state FROM batches") == [("broadcast",)]
        and s,
        10,
        "a snapshot after the broadcast",
    )

    # The restore: the pool stopped, the last snapshot over its database, handle.key kept.
    pool.kill()
    database = pool.data_dir / "puntohunter.sqlite3"
    for stale in (database, database.with_name(database.name + "-wal"), database.with_name(database.name + "-shm")):
        stale.unlink(missing_ok=True)
    shutil.copy(taken[-1], database)
    pool.start()
    health = pool.portal.api("/api/v1/health").json
    assert health["checks"]["payouts"] not in ("halted", "paused"), health
    assert pool.logs().count("payouts reconciled with the wallet and the chain") == 2, "at both starts"
    assert len(batches(pool)) == 1 and batches(pool)[0]["txid"] == batch["txid"], "the same batch, not a second"
