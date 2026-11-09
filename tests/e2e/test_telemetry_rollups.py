"""Telemetry rollups: shares over known periods produce one row per
worker and period, a period with nothing in it produces none, best share is the maximum rather than the most
recent, and no rollup column can hold an address."""

import re
import sqlite3
import time

import pytest

from clients.miner import DIFF1, header, sha256d, target
from clients.sv1 import Sv1Miner
from wait import wait_until

# Between the harness floor and regtest's network difficulty, so shares need not be blocks.
PINNED = 3e-10
# Seconds a rollup period lasts under these tests; a minute live.
PERIOD = 2
HANDLE = re.compile(r"handle=([a-z2-7]{16})\b")


@pytest.fixture
def pool(pool_factory):
    return pool_factory(
        overrides={
            "vardiff": {"floor": PINNED, "ceiling": PINNED},
            "telemetry": {"rollup_secs": PERIOD},
        }
    )


@pytest.fixture
def miner(pool, miner_username):
    miner = Sv1Miner(pool.solo_v1_port, miner_username)
    yield miner
    miner.client.close()


def handle_of(pool):
    return HANDLE.findall("\n".join(l for l in pool.logs().splitlines() if "V1 authorized" in l))[-1]


def reached(miner, params):
    """The difficulty a share's hash reached."""
    job, extranonce2, ntime, nonce = params[:4]
    assert job == miner.notify[0]
    data = header(
        miner.notify,
        bytes.fromhex(miner.client.extranonce1 + extranonce2),
        int(ntime, 16),
        int(nonce, 16),
    )
    return DIFF1 / int.from_bytes(sha256d(data), "little")


def in_one_period(submit):
    """Runs `submit` from the start of a period to its end, retrying if it spilled over, and returns that
    period's start."""
    for _ in range(5):
        wait_until(lambda: time.time() % PERIOD < PERIOD / 4, PERIOD * 2, "a period's start")
        start = int(time.time()) // PERIOD * PERIOD
        submit()
        if int(time.time()) // PERIOD * PERIOD == start:
            return start
    raise AssertionError("the shares never fit one period")


def test_shares_over_known_periods_make_one_row_each(pool, miner):
    handle = handle_of(pool)
    first = in_one_period(lambda: [miner.submit(miner.share()) for _ in range(3)])
    # A period with nothing in it between the two.
    wait_until(lambda: time.time() >= first + 2 * PERIOD, PERIOD * 3, "an idle period")
    second = in_one_period(lambda: [miner.submit(miner.share()) for _ in range(2)])
    assert second >= first + 2 * PERIOD
    miner.client.close()

    rows = wait_until(lambda: len(pool.rollups(handle)) == 2 and pool.rollups(handle), PERIOD * 3, "rows")
    assert [(r["start"], r["accepted"]) for r in rows] == [(first, 3), (second, 2)]
    work = 2**256 / (target(PINNED) + 1)
    for row in rows:
        assert (row["pool"], row["transport"]) == ("solo", "V1")
        assert row["work"] == pytest.approx(row["accepted"] * work, rel=1e-9)
        assert row["stale"] == row["duplicate"] == row["low_difficulty"] == 0


def test_refused_shares_are_counted_by_class(pool, miner):
    handle = handle_of(pool)

    def submit():
        share = miner.share()
        miner.submit(share)
        miner.rejection(share)
        miner.rejection(miner.share(meets=False))
        miner.rejection(miner.share(version_bits=0x0000_0001))

    start = in_one_period(submit)
    [row] = wait_until(lambda: pool.rollups(handle), PERIOD * 3, "the row")
    assert row["start"] == start
    assert (row["accepted"], row["duplicate"], row["low_difficulty"], row["bad_version"]) == (1, 1, 1, 1)


def test_best_share_is_the_maximum_not_the_most_recent(pool, miner):
    handle = handle_of(pool)
    shares = sorted((miner.share() for _ in range(6)), key=lambda p: reached(miner, p), reverse=True)
    best = reached(miner, shares[0])
    assert reached(miner, shares[-1]) < best
    in_one_period(lambda: [miner.submit(share) for share in shares])

    [row] = wait_until(lambda: pool.rollups(handle), PERIOD * 3, "the row")
    assert row["best"] == pytest.approx(best, rel=1e-9)
    [day] = pool.rows("SELECT * FROM best_shares WHERE handle = ?", handle)
    assert day["best"] == pytest.approx(best, rel=1e-9)
    assert day["day"] == row["start"] // 86400


def test_no_rollup_column_can_hold_an_address(pool, miner, miner_username):
    address = miner_username.split(".")[0]
    handle = handle_of(pool)
    valid = {
        "rollups": {
            "start": 0, "handle": handle, "pool": "solo", "transport": "V1", "accepted": 0, "work": 0.0,
            "stale": 0, "duplicate": 0, "low_difficulty": 0, "bad_version": 0, "invalid": 0, "best": 0.0,
        },
        "best_shares": {"handle": handle, "pool": "solo", "day": 0, "best": 0.0},
    }
    with sqlite3.connect(pool.data_dir / "puntohunter.sqlite3") as db:
        for table, row in valid.items():
            columns = [c[1] for c in db.execute(f"PRAGMA table_info({table})")]
            assert sorted(columns) == sorted(row), table
            for column in columns:
                attempt = {**row, column: address}
                with pytest.raises(sqlite3.IntegrityError):
                    db.execute(
                        f"INSERT INTO {table} ({', '.join(attempt)}) VALUES ({', '.join('?' * len(attempt))})",
                        list(attempt.values()),
                    )
            # The valid row itself goes in, so each refusal above was the address's.
            db.execute(f"INSERT INTO {table} ({', '.join(row)}) VALUES ({', '.join('?' * len(row))})", list(row.values()))
            db.rollback()
