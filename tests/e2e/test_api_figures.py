"""The figures (API reference sections 1.3, 2.5 to 2.7): hashrate over 1 and 10 minutes, 1 hour, and 24
hours, each marked while its window fills, in the worker's resource and its stream, the hour's figure matching the
rollup rows; a solo worker's odds of a block, marked while its day's hashrate fills; and the chart's ranges,
each point matching the rollup rows it sums, each reaching back its own length to now however few rows it holds,
a fleet's series the sum of its devices', and an unknown range refused."""

import json
import re
import time
from collections import defaultdict

import pytest

from clients.sv1 import Sv1Miner
from wait import wait_until

# Between the harness floor and regtest's network difficulty, so shares need not be blocks.
PINNED = 3e-10
PERIOD = 2
HOUR, DAY, TEN_MINUTES = 3600, 86_400, 600


def logged_handles(pool, event):
    return re.findall(r"handle=([a-z2-7]{16})", "\n".join(l for l in pool.logs().splitlines() if event in l))


def rate(live, window):
    """A hashrate of the worker's live values: its value, and whether it is estimating."""
    rate = live["hash_rate"][window]
    return rate["hash_rate"], rate["estimating"]


@pytest.fixture
def pool(pool_factory):
    return pool_factory(
        overrides={"vardiff": {"floor": PINNED, "ceiling": PINNED}, "telemetry": {"rollup_secs": PERIOD}}
    )


def mined(pool, username, port=None, shares=3):
    """A V1 miner on `username` that has had `shares` accepted, once their rollup rows are written."""
    miner = Sv1Miner(port or pool.solo_v1_port, username)
    handle = logged_handles(pool, "V1 authorized")[-1]
    for _ in range(shares):
        assert miner.submit(miner.share()) is True
    wait_until(lambda: sum(r["accepted"] for r in pool.rollups(handle)) == shares, PERIOD * 3, "the rollup rows")
    return miner, handle


def test_four_windows_in_the_resource_and_its_stream(pool, miner_username):
    miner, handle = mined(pool, miner_username)
    rows = pool.rollups(handle)
    work, first = sum(r["work"] for r in rows), min(r["start"] for r in rows)

    before = time.time()
    live = pool.portal.api(f"/api/v1/workers/{handle}").json["live"]
    after = time.time()
    minute, _ = rate(live, "1m")
    assert minute > 0, "the shares came in the last minute"
    ten, estimating = rate(live, "10m")
    assert ten > 0 and estimating, "connected under 10 minutes ago"
    # A device newer than the hour and the day: their work is over the time since its first row, which ends on
    # the period boundary the resource was read after.
    since_first = {int(t) - int(t) % PERIOD - first for t in (before, after)}
    hour, estimating = rate(live, "1h")
    assert estimating and any(hour == pytest.approx(work / span, rel=1e-9) for span in since_first), hour
    assert rate(live, "24h") == (pytest.approx(hour, rel=1e-9), True)

    assert live["solo"]["estimating"], "the odds rest on a day's hashrate still filling"

    # The stream sends the same windows, each marked as the resource marks it.
    events = pool.portal.events(f"/api/v1/workers/{handle}/events")
    streamed = json.loads(next(events)["data"])["hash_rate"]
    events.close()
    marks = lambda rates: {window: rate["estimating"] for window, rate in rates.items()}
    assert marks(streamed) == marks(pool.portal.api(f"/api/v1/workers/{handle}").json["live"]["hash_rate"])
    assert set(streamed) == {"1m", "10m", "1h", "24h"} and streamed["1m"]["hash_rate"] > 0
    miner.client.close()


def test_a_solo_worker_has_its_odds_and_its_best_share_against_the_network(pool, miner_username):
    """A pooled worker has neither: test_api_worker.py."""
    miner, handle = mined(pool, miner_username)
    solo = pool.portal.api(f"/api/v1/workers/{handle}").json["live"]["solo"]
    odds = solo["odds"]
    assert set(odds) == {"day", "month", "year", "median_wait_secs", "average_wait_secs"}, odds
    assert 0 < odds["day"] <= odds["month"] <= odds["year"] <= 1, odds
    assert 0 < odds["median_wait_secs"] < odds["average_wait_secs"], odds
    assert solo["best_vs_network"] > 0
    miner.client.close()


def expected(rows, bucket):
    """The series the rows make in buckets of `bucket` seconds: {start: (hash_rate, accepted, refused)}."""
    sums = defaultdict(lambda: [0.0, 0, 0])
    for row in rows:
        refused = sum(row[c] for c in ("stale", "duplicate", "low_difficulty", "bad_version", "invalid"))
        bucketed = sums[row["start"] - row["start"] % bucket]
        bucketed[0] += row["work"]
        bucketed[1] += row["accepted"]
        bucketed[2] += refused
    return {start: (work / bucket, accepted, refused) for start, (work, accepted, refused) in sorted(sums.items())}


def points(series):
    return {
        start: (rate, accepted, refused)
        for start, rate, accepted, refused in zip(
            series["start"], series["hash_rate"], series["accepted"], series["rejected"]
        )
    }


def same(got, want):
    assert list(got) == list(want)
    for start in want:
        assert got[start][0] == pytest.approx(want[start][0], rel=1e-9), start
        assert got[start][1:] == want[start][1:], start


def series(pool, path, range_=None):
    response = pool.portal.api(path + (f"?range={range_}" if range_ else ""))
    assert response.status == 200, response.status
    assert response.header("cache-control") == "no-cache"
    return response.json


def test_each_range_matches_the_rows_it_sums_and_a_fleet_sums_its_devices(pool, regtest_cluster):
    address = regtest_cluster.miner_address(0)
    a, first = mined(pool, f"{address}.one")
    # A duplicate, so the series carries a refused share.
    share = a.share()
    assert a.submit(share) is True
    assert a.rejection(share) is not None
    wait_until(lambda: sum(r["duplicate"] for r in pool.rollups(first)) == 1, PERIOD * 3, "the refused share's row")
    b, second = mined(pool, f"{address}.two", shares=2)

    rows = {handle: pool.rollups(handle) for handle in (first, second)}
    for range_, bucket in ((None, PERIOD), ("1h", PERIOD), ("24h", PERIOD), ("7d", TEN_MINUTES)):
        got = series(pool, f"/api/v1/workers/{first}/series", range_)
        assert got["period"] == bucket
        same(points(got), expected(rows[first], bucket))

    [fleet] = {r["fleet"] for r in pool.rows("SELECT fleet FROM fleets WHERE handle IN (?, ?)", first, second)}
    for range_, bucket in (("1h", PERIOD), ("7d", TEN_MINUTES)):
        got = series(pool, f"/api/v1/fleets/{fleet}/series", range_)
        [solo] = got["pools"]
        assert (solo["pool"], solo["name"]) == ("solo", "Solo"), "a series for each pool its workers mine in"
        same(points(solo), expected(rows[first] + rows[second], bucket))

    for path in (f"/api/v1/workers/{first}/series", f"/api/v1/fleets/{fleet}/series"):
        for bad in ("5m", "", "7D"):
            refused = pool.portal.api(f"{path}?range={bad}")
            assert (refused.status, refused.json["error"]) == (400, "bad_request"), (path, bad)
    for path in ("/api/v1/workers/aaaaaaaaaaaaaaaa/series", "/api/v1/fleets/aaaaaaaaaaaaaaaa/series"):
        assert pool.portal.api(path).status == 404, path
    a.client.close()
    b.client.close()


def test_each_range_spans_its_own_time_however_few_rows_it_holds(pool, regtest_cluster):
    miner, handle = mined(pool, f"{regtest_cluster.miner_address(0)}.span")
    [fleet] = {r["fleet"] for r in pool.rows("SELECT fleet FROM fleets WHERE handle = ?", handle)}
    now = time.time()
    got = {range_: series(pool, f"/api/v1/workers/{handle}/series", range_) for range_ in ("1h", "24h", "7d")}
    # Under a minute of rows, and each range still reaches back its own length to now.
    for range_, length, bucket in (("1h", HOUR, PERIOD), ("24h", DAY, PERIOD), ("7d", 7 * DAY, TEN_MINUTES)):
        span = got[range_]
        assert span["since"] % bucket == 0 and span["until"] % PERIOD == 0, range_
        assert span["since"] == pytest.approx(now - length, abs=bucket + 5), range_
        assert span["until"] == pytest.approx(now, abs=PERIOD + 5), range_
        assert span["since"] < min(span["start"]) <= max(span["start"]) < span["until"], range_
    assert got["1h"]["since"] - got["24h"]["since"] == pytest.approx(DAY - HOUR, abs=PERIOD)

    # A fleet's pools share the one range, rows or not.
    fleet_series = series(pool, f"/api/v1/fleets/{fleet}/series", "24h")
    ranges = {(s["since"], s["until"]) for s in fleet_series["pools"]}
    assert len(ranges) == 1
    miner.client.close()
