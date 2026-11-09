//! The rollup task: on each period boundary it writes the rows of the periods
//! that ended through the store's writer thread, with each worker's best share by day. A worker that did
//! nothing in a period has no row for it.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;

use super::{Reject, Row, Telemetry, handle::Handle};
use crate::{mining::pool::PoolId, store::Store, stratum::Transport};

const DAY: u64 = 86_400;

/// Starts the task, writing rows of `period` seconds. It runs for the life of the process.
pub fn spawn(telemetry: Arc<Telemetry>, store: Store, period: u64) {
    tokio::spawn(run(telemetry, store, period));
}

async fn run(telemetry: Arc<Telemetry>, store: Store, period: u64) {
    loop {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let boundary = (now.as_secs() / period + 1) * period;
        tokio::time::sleep(Duration::from_secs(boundary).saturating_sub(now)).await;
        // A share stamped just before the boundary but counted after it lands in a row of its own, which the
        // next write adds to this one.
        let rows = telemetry.finished(boundary);
        telemetry.evict(Instant::now());
        if rows.is_empty() {
            continue;
        }
        let count = rows.len();
        match store.commit(move |t| write(t, boundary, &rows)).await {
            Ok(()) => tracing::debug!(rows = count, "rollups written"),
            Err(e) => tracing::error!("cannot write rollups: {e}"),
        }
    }
}

/// Adds `rows`, taken out before Unix time `boundary`, to the rollups and each worker's totals, and their best
/// shares to each worker's best of the day.
pub fn write(transaction: &Transaction, boundary: u64, rows: &[Row]) -> rusqlite::Result<()> {
    let mut rollup = transaction.prepare(
        "INSERT INTO rollups (start, handle, pool, transport, accepted, work, \
         stale, duplicate, low_difficulty, bad_version, invalid, best) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
         ON CONFLICT (handle, transport, start) DO UPDATE SET \
         accepted = accepted + excluded.accepted, work = work + excluded.work, \
         stale = stale + excluded.stale, duplicate = duplicate + excluded.duplicate, \
         low_difficulty = low_difficulty + excluded.low_difficulty, \
         bad_version = bad_version + excluded.bad_version, invalid = invalid + excluded.invalid, \
         best = max(best, excluded.best)",
    )?;
    let mut best = transaction.prepare(
        "INSERT INTO best_shares (handle, pool, day, best) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (handle, day) DO UPDATE SET best = max(best, excluded.best)",
    )?;
    let mut totals = transaction.prepare(
        "INSERT INTO totals (handle, accepted, rejected, through) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (handle) DO UPDATE SET accepted = accepted + excluded.accepted, \
         rejected = rejected + excluded.rejected, through = excluded.through",
    )?;
    for row in rows {
        let handle = row.handle.to_string();
        rollup.execute(params![
            row.start as i64,
            handle,
            row.pool,
            row.transport.as_str(),
            row.accepted as i64,
            row.work,
            row.rejected(Reject::Stale) as i64,
            row.rejected(Reject::Duplicate) as i64,
            row.rejected(Reject::LowDifficulty) as i64,
            row.rejected(Reject::BadVersion) as i64,
            row.rejected(Reject::Invalid) as i64,
            row.best,
        ])?;
        totals.execute(params![
            handle,
            row.accepted as i64,
            row.refused() as i64,
            boundary as i64
        ])?;
        if row.accepted > 0 {
            best.execute(params![
                handle,
                row.pool,
                (row.start / DAY) as i64,
                row.best
            ])?;
        }
    }
    Ok(())
}

/// Whether `handle` has a rollup row, a block, or a place in a fleet.
pub fn known(connection: &Connection, handle: Handle) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM rollups WHERE handle = ?1) \
         OR EXISTS (SELECT 1 FROM blocks WHERE handle = ?1) \
         OR EXISTS (SELECT 1 FROM fleets WHERE handle = ?1)",
        [handle.to_string()],
        |row| row.get(0),
    )
}

/// What the rollups, blocks, and fleets hold of a worker, for when its live values are gone.
#[derive(Debug, Clone, PartialEq)]
pub struct History {
    pub pool: PoolId,
    /// The transport it last mined over; none for a worker that never had a share.
    pub transport: Option<Transport>,
    /// Its best share of any day, 0 when it has none.
    pub best_share: f64,
    /// Unix seconds at the end of the last period in which it had a share accepted.
    pub last_share: Option<u64>,
    /// Unix seconds at the end of its newest period, or when it joined its fleet if it has none.
    pub last_seen: u64,
    /// Unix seconds when it first connected here; none for a worker recorded before fleets were.
    pub first_seen: Option<u64>,
}

/// A worker's accepted and refused shares in the store, and the boundary of the newest write that added to
/// them; telemetry holds the rest ([`Telemetry::unflushed`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub accepted: u64,
    pub rejected: u64,
    pub through: u64,
}

/// `handle`'s totals in the store; all 0 before its first write.
pub fn totals(connection: &Connection, handle: Handle) -> rusqlite::Result<Totals> {
    Ok(connection
        .query_row(
            "SELECT accepted, rejected, through FROM totals WHERE handle = ?1",
            [handle.to_string()],
            |row| {
                Ok(Totals {
                    accepted: row.get::<_, i64>(0)? as u64,
                    rejected: row.get::<_, i64>(1)? as u64,
                    through: row.get::<_, i64>(2)? as u64,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

/// The worker's history, if it has mined here. `period` is the rollup period in seconds.
pub fn history(
    connection: &Connection,
    handle: Handle,
    period: u64,
) -> rusqlite::Result<Option<History>> {
    let handle = handle.to_string();
    let tags = connection
        .query_row(
            "SELECT pool, transport FROM (\
             SELECT pool, transport, start AS at FROM rollups WHERE handle = ?1 \
             UNION ALL SELECT pool, transport, found_at AS at FROM blocks WHERE handle = ?1) \
             ORDER BY at DESC LIMIT 1",
            [&handle],
            |row| Ok((row.get::<_, PoolId>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let joined: Option<(PoolId, i64)> = connection
        .query_row(
            "SELECT pool, first_seen FROM fleets WHERE handle = ?1",
            [&handle],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (pool, transport) = match (tags, &joined) {
        (Some((pool, transport)), _) => (pool, Transport::parse(&transport)),
        (None, Some((pool, _))) => (pool.clone(), None),
        (None, None) => return Ok(None),
    };
    let best_share: Option<f64> = connection.query_row(
        "SELECT max(best) FROM best_shares WHERE handle = ?1",
        [&handle],
        |row| row.get(0),
    )?;
    let (last_share, newest): (Option<i64>, Option<i64>) = connection.query_row(
        "SELECT max(start) FILTER (WHERE accepted > 0), max(start) FROM rollups WHERE handle = ?1",
        [&handle],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let end = |start: i64| start as u64 + period;
    Ok(Some(History {
        pool,
        transport,
        best_share: best_share.unwrap_or(0.0),
        last_share: last_share.map(end),
        last_seen: newest
            .map(end)
            .or(joined.as_ref().map(|(_, first_seen)| *first_seen as u64))
            .unwrap_or(0),
        first_seen: joined.map(|(_, first_seen)| first_seen as u64),
    }))
}

/// A hashrate over a window, and whether the window reaches back before the worker's first activity, so
/// that the figure is still an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rate {
    pub hash_rate: f64,
    pub estimating: bool,
}

impl Rate {
    /// The rates of several workers summed: estimating while every one of them is, since a window is
    /// part-filled only while it reaches back before the first of them.
    pub fn sum(rates: impl IntoIterator<Item = Rate>) -> Rate {
        rates.into_iter().fold(
            Rate {
                hash_rate: 0.0,
                estimating: true,
            },
            |sum, rate| Rate {
                hash_rate: sum.hash_rate + rate.hash_rate,
                estimating: sum.estimating && rate.estimating,
            },
        )
    }
}

/// A worker's hashrate over the last hour and day.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Windows {
    pub hour: Rate,
    pub day: Rate,
}

pub const HOUR: u64 = 3600;

/// The worker's hashrate over the hour and the day that end on the last period boundary before Unix time
/// `now`: the work of the window's rows, both transports summed, over the window's length. A window is
/// estimating while it starts before the worker's first row, and its work is then over the time since that
/// row's start: a device that joined ten minutes ago reads its rate, not a sixth of it over the hour.
pub fn windows(
    connection: &Connection,
    handle: Handle,
    now: u64,
    period: u64,
) -> rusqlite::Result<Windows> {
    let boundary = now - now % period;
    let (hour_since, day_since) = (boundary.saturating_sub(HOUR), boundary.saturating_sub(DAY));
    let (first, hour, day): (Option<i64>, f64, f64) = connection.query_row(
        "SELECT min(start), \
         total(work) FILTER (WHERE start >= ?2 AND start < ?4), \
         total(work) FILTER (WHERE start >= ?3 AND start < ?4) \
         FROM rollups WHERE handle = ?1",
        params![
            handle.to_string(),
            hour_since as i64,
            day_since as i64,
            boundary as i64
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let rate = |work: f64, span: u64, since: u64| {
        let estimating = first.is_none_or(|first| first as u64 > since);
        let span = match first {
            Some(first) if estimating => boundary.saturating_sub(first as u64).max(period),
            _ => span,
        };
        Rate {
            hash_rate: work / span as f64,
            estimating,
        }
    };
    Ok(Windows {
        hour: rate(hour, HOUR, hour_since),
        day: rate(day, DAY, day_since),
    })
}

/// Rollups as chart series: one entry per bucket that has a row, summed over the workers and both
/// transports, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Series {
    /// The range the series covers, in Unix seconds: from its first bucket's start up to `until`, the end of
    /// the last rollup period written, so a chart spans the whole range however few rows it holds.
    pub since: u64,
    pub until: u64,
    /// Seconds a bucket lasts.
    pub period: u64,
    /// Unix seconds at each bucket's start.
    pub start: Vec<u64>,
    /// Hashes a second: the bucket's accepted work over its length.
    pub hash_rate: Vec<f64>,
    pub accepted: Vec<u64>,
    pub rejected: Vec<u64>,
    pub best: Vec<f64>,
}

/// The series of `handles` over the rows starting at Unix time `since` or later and before `until`, in buckets
/// of `bucket` seconds, each a multiple of the rollup period.
pub fn series(
    connection: &Connection,
    handles: &[Handle],
    since: u64,
    until: u64,
    bucket: u64,
) -> rusqlite::Result<Series> {
    let mut statement = connection.prepare(
        "SELECT start - start % ?3, sum(work), sum(accepted), \
         sum(stale + duplicate + low_difficulty + bad_version + invalid), max(best) \
         FROM rollups WHERE handle = ?1 AND start >= ?2 AND start < ?4 GROUP BY 1",
    )?;
    let mut buckets: BTreeMap<u64, (f64, u64, u64, f64)> = BTreeMap::new();
    for handle in handles {
        let mut rows = statement.query(params![
            handle.to_string(),
            since as i64,
            bucket as i64,
            until as i64
        ])?;
        while let Some(row) = rows.next()? {
            let sum = buckets.entry(row.get::<_, i64>(0)? as u64).or_default();
            sum.0 += row.get::<_, f64>(1)?;
            sum.1 += row.get::<_, i64>(2)? as u64;
            sum.2 += row.get::<_, i64>(3)? as u64;
            sum.3 = sum.3.max(row.get(4)?);
        }
    }
    let mut series = Series {
        since,
        until,
        period: bucket,
        start: Vec::new(),
        hash_rate: Vec::new(),
        accepted: Vec::new(),
        rejected: Vec::new(),
        best: Vec::new(),
    };
    for (start, (work, accepted, rejected, best)) in buckets {
        series.start.push(start);
        series.hash_rate.push(work / bucket as f64);
        series.accepted.push(accepted);
        series.rejected.push(rejected);
        series.best.push(best);
    }
    Ok(series)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::{
        mining::pool::tests::id, store::DATABASE, stratum::Transport, telemetry::handle::Handle,
    };

    fn store(name: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-rollup-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join(DATABASE)).unwrap()
    }

    /// Rows of one worker's shares, with the best share of each.
    fn rows(bests: &[(u64, f64)]) -> Vec<Row> {
        let telemetry = Telemetry::new(60);
        let handle: Handle = "a".repeat(16).parse().unwrap();
        let now = Instant::now();
        let _link = telemetry.connect_at(handle, id("solo"), Transport::V1, now, SystemTime::now());
        for &(unix, best) in bests {
            let at = SystemTime::UNIX_EPOCH + Duration::from_secs(unix);
            telemetry.accept(handle, Transport::V1, 1.0, best, None, now, at);
        }
        telemetry.finished(u64::MAX)
    }

    #[tokio::test]
    async fn adds_to_a_row_written_before_and_keeps_the_best_of_each_day() {
        let store = store("write");
        let first = rows(&[(DAY + 60, 9.0), (DAY + 120, 2.0), (2 * DAY, 4.0)]);
        store
            .commit(move |t| write(t, 3 * DAY, &first))
            .await
            .unwrap();
        // The same period again, as when a share counted after its boundary is written late.
        let late = rows(&[(DAY + 60, 1.0)]);
        store
            .commit(move |t| write(t, 3 * DAY + 60, &late))
            .await
            .unwrap();

        let (count, accepted): (u32, u32) = store
            .commit(|t| {
                t.query_row("SELECT COUNT(*), SUM(accepted) FROM rollups", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
            })
            .await
            .unwrap();
        assert_eq!((count, accepted), (3, 4));
        let bests: Vec<(i64, f64)> = store
            .commit(|t| {
                t.prepare("SELECT day, best FROM best_shares ORDER BY day")?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect()
            })
            .await
            .unwrap();
        assert_eq!(
            bests,
            [(1, 9.0), (2, 4.0)],
            "the maximum, not the most recent"
        );

        let handle: Handle = "a".repeat(16).parse().unwrap();
        let other: Handle = "b".repeat(16).parse().unwrap();
        let (ours, none) = store
            .commit(move |t| Ok((totals(t, handle)?, totals(t, other)?)))
            .await
            .unwrap();
        assert_eq!(
            ours,
            Totals {
                accepted: 4,
                rejected: 0,
                through: 3 * DAY + 60
            }
        );
        assert_eq!(none, Totals::default());
        let (history, series) = store
            .commit(move |t| {
                Ok((
                    history(t, handle, 60)?,
                    series(t, &[handle], DAY + 100, 3 * DAY, 60)?,
                ))
            })
            .await
            .unwrap();
        let history = history.unwrap();
        assert_eq!(
            (history.pool, history.transport),
            (id("solo"), Some(Transport::V1))
        );
        assert_eq!(history.best_share, 9.0);
        assert_eq!(history.last_share, Some(2 * DAY + 60));
        assert_eq!(history.last_seen, 2 * DAY + 60);
        assert_eq!(series.start, [DAY + 120, 2 * DAY]);
        assert_eq!(series.accepted, [1, 1]);
        assert_eq!(series.hash_rate, [1.0 / 60.0, 1.0 / 60.0]);
    }

    /// Rows for workers `a` and `b`: (worker, Unix time, work) each, one accepted share apiece.
    async fn written(name: &str, shares: &[(char, u64, f64)]) -> Store {
        let store = store(name);
        let telemetry = Telemetry::new(60);
        let now = Instant::now();
        let mut links = Vec::new();
        for c in ['a', 'b'] {
            let handle: Handle = c.to_string().repeat(16).parse().unwrap();
            links.push(telemetry.connect_at(
                handle,
                id("solo"),
                Transport::V1,
                now,
                SystemTime::now(),
            ));
        }
        for &(c, unix, work) in shares {
            let handle: Handle = c.to_string().repeat(16).parse().unwrap();
            let at = SystemTime::UNIX_EPOCH + Duration::from_secs(unix);
            telemetry.accept(handle, Transport::V1, work, 1.0, None, now, at);
        }
        let rows = telemetry.finished(u64::MAX);
        store
            .commit(move |t| write(t, u64::MAX, &rows))
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn a_new_device_reads_its_rate_and_an_old_one_its_average() {
        let start = 10 * DAY;
        // Worker a joined ten minutes ago and has mined 60 work a minute since; worker b has mined for days,
        // three rows of 8640 work in the last day.
        let mut shares: Vec<(char, u64, f64)> =
            (0..10).map(|m| ('a', start + 60 * m, 60.0)).collect();
        shares.extend([
            ('b', start - 2 * DAY, 1.0),
            ('b', start - DAY + 3600, 8640.0),
            ('b', start - 3600, 8640.0),
            ('b', start, 8640.0),
        ]);
        let store = written("present", &shares).await;
        let (a, b): (Handle, Handle) = (
            "a".repeat(16).parse().unwrap(),
            "b".repeat(16).parse().unwrap(),
        );
        let (a, b) = store
            .commit(move |t| {
                Ok((
                    windows(t, a, start + 600, 60)?,
                    windows(t, b, start + 600, 60)?,
                ))
            })
            .await
            .unwrap();
        let steady = Rate {
            hash_rate: 1.0,
            estimating: true,
        };
        assert_eq!((a.hour, a.day), (steady, steady));
        assert_eq!(
            b.day,
            Rate {
                hash_rate: 3.0 * 8640.0 / 86_400.0,
                estimating: false
            }
        );
    }

    #[tokio::test]
    async fn computes_each_window_and_marks_it_estimating_while_part_filled() {
        let first = 10 * DAY;
        // Worker a: 60 work in its first minute, then 3600 an hour later, then nothing.
        let store = written(
            "windows",
            &[('a', first, 60.0), ('a', first + HOUR, 3600.0)],
        )
        .await;
        let handle: Handle = "a".repeat(16).parse().unwrap();
        let at = |now: u64| {
            let store = store.clone();
            async move {
                store
                    .commit(move |t| windows(t, handle, now, 60))
                    .await
                    .unwrap()
            }
        };

        // Within the first period, nothing is written for the window yet.
        let w = at(first + 30).await;
        assert_eq!(
            w.hour,
            Rate {
                hash_rate: 0.0,
                estimating: true
            }
        );
        // While the hour reaches back before the first row, its work is over the time since that row.
        let w = at(first + HOUR - 1).await;
        assert_eq!(
            w.hour,
            Rate {
                hash_rate: 60.0 / (3600.0 - 60.0),
                estimating: true
            }
        );
        // An hour after the first row's start, the hour window starts on it: filled.
        let w = at(first + HOUR).await;
        assert_eq!(
            w.hour,
            Rate {
                hash_rate: 60.0 / 3600.0,
                estimating: false
            }
        );
        assert!(w.day.estimating);
        // The hour's own row counts once its period has ended.
        let w = at(first + HOUR + 60).await;
        assert_eq!(w.hour.hash_rate, 3600.0 / 3600.0);
        assert_eq!(
            w.day,
            Rate {
                hash_rate: 3660.0 / 3660.0,
                estimating: true
            }
        );
        let w = at(first + DAY).await;
        assert_eq!(
            w.day,
            Rate {
                hash_rate: 3660.0 / 86_400.0,
                estimating: false
            }
        );
        assert_eq!(
            w.hour,
            Rate {
                hash_rate: 0.0,
                estimating: false
            }
        );
        // A worker with no rows estimates zero.
        let none: Handle = "c".repeat(16).parse().unwrap();
        let w = store
            .commit(move |t| windows(t, none, first, 60))
            .await
            .unwrap();
        assert_eq!(
            w.day,
            Rate {
                hash_rate: 0.0,
                estimating: true
            }
        );

        let filled = Rate {
            hash_rate: 1.0,
            estimating: false,
        };
        let part = Rate {
            hash_rate: 2.0,
            estimating: true,
        };
        assert_eq!(
            Rate::sum([filled, part]),
            Rate {
                hash_rate: 3.0,
                estimating: false
            }
        );
        assert_eq!(
            Rate::sum([part, part]),
            Rate {
                hash_rate: 4.0,
                estimating: true
            }
        );
    }

    #[tokio::test]
    async fn sums_a_series_over_workers_in_buckets() {
        let store = written(
            "buckets",
            &[
                ('a', 600, 6.0),
                ('b', 600, 60.0),
                ('a', 660, 12.0),
                ('b', 1200, 600.0),
            ],
        )
        .await;
        let both: Vec<Handle> = ["a", "b"]
            .iter()
            .map(|c| c.repeat(16).parse().unwrap())
            .collect();
        let (minutes, tens, ranged) = store
            .commit(move |t| {
                Ok((
                    series(t, &both, 0, 1260, 60)?,
                    series(t, &both, 0, 1260, 600)?,
                    series(t, &both, 600, 1200, 60)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!((minutes.since, minutes.until), (0, 1260));
        assert_eq!(minutes.start, [600, 660, 1200]);
        assert_eq!(minutes.hash_rate, [66.0 / 60.0, 12.0 / 60.0, 600.0 / 60.0]);
        assert_eq!(minutes.accepted, [2, 1, 1]);
        assert_eq!(tens.period, 600);
        assert_eq!(tens.start, [600, 1200]);
        assert_eq!(tens.hash_rate, [78.0 / 600.0, 600.0 / 600.0]);
        assert_eq!(tens.accepted, [3, 1]);
        assert_eq!(
            (ranged.since, ranged.until, ranged.start),
            (600, 1200, vec![600, 660]),
            "a row at the range's end is left out"
        );
    }
}
