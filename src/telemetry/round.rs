//! The round: the time since this instance's newest block a node accepted, and each
//! fleet's best share in it and in each of the last 24 hours, held in memory for the stats lists.
//!
//! Each accepted share can raise its fleet's bests, and a block a node accepted starts the next round. A
//! refused block starts nothing. At startup the round is seeded from the store: the newest block answered
//! and not refused, `best_shares` for each whole day of the round, and `rollups` for the rest, so a restart
//! loses no more than the rollup period not yet written. Fleets are known by their handle and by their
//! address's shortened form: the whole address never enters the round.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
};

use rusqlite::{Connection, OptionalExtension, params};

use super::{Listing, fleet, handle::Handle};
use crate::mining::pool::PoolId;

const DAY: u64 = 86_400;
const HOUR: u64 = 3_600;
/// The hours the day's list covers: the current one and the 23 before it.
pub const HOURS: u64 = 24;

/// A best share: its hash's difficulty, the miner type of the device that reached it, its pool, and when.
#[derive(Debug, Clone, PartialEq)]
pub struct Best {
    pub best: f64,
    pub miner: Option<String>,
    pub pool: PoolId,
    /// Unix seconds: the share's own time, or the start of the rollup period it was read back from.
    pub at: u64,
}

impl Best {
    /// Higher first; at the same difficulty, the earlier.
    fn beats(&self, other: &Best) -> bool {
        self.best > other.best || (self.best == other.best && self.at < other.at)
    }
}

/// Where the round starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Start {
    /// Unix seconds: when its block was found, or before the first block, the instance's first share. None
    /// before any share.
    pub at: Option<u64>,
    /// The height of the block that started it; None before the first block.
    pub height: Option<u32>,
}

/// A fleet's row on a best-share list.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub fleet: Handle,
    /// None for a private fleet, whose rows show no address.
    pub short_address: Option<String>,
    pub best: Best,
}

/// The round, shared by telemetry, which feeds it each accepted share, the submitter, which starts the next,
/// and the portal, which lists it.
#[derive(Default)]
pub struct Round {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    start: Start,
    fleets: HashMap<Handle, Fleet>,
    /// Each miner type's best this round, by the type in lower case; None for the devices that named none.
    types: HashMap<Option<String>, f64>,
}

struct Fleet {
    short_address: Option<String>,
    /// Its best this round; None when it has had no share in it.
    round: Option<Best>,
    /// Its best in each of the last [`HOURS`] hours that had a share, by the hour's start.
    hours: BTreeMap<u64, Best>,
}

impl Fleet {
    fn new(short_address: Option<String>) -> Self {
        Fleet {
            short_address,
            round: None,
            hours: BTreeMap::new(),
        }
    }

    fn raise_round(&mut self, best: &Best) {
        if self.round.as_ref().is_none_or(|held| best.beats(held)) {
            self.round = Some(best.clone());
        }
    }

    fn raise_hour(&mut self, best: &Best) {
        let hour = best.at - best.at % HOUR;
        match self.hours.get(&hour) {
            Some(held) if !best.beats(held) => {}
            _ => {
                self.hours.insert(hour, best.clone());
            }
        }
        // Keep the newest hours only.
        let oldest = hour.saturating_sub((HOURS - 1) * HOUR);
        self.hours = self.hours.split_off(&oldest);
    }

    /// Its best over the [`HOURS`] hours up to the one holding Unix time `now`.
    fn day(&self, now: u64) -> Option<&Best> {
        let oldest = (now - now % HOUR).saturating_sub((HOURS - 1) * HOUR);
        self.hours
            .range(oldest..)
            .map(|(_, best)| best)
            .reduce(|a, b| if b.beats(a) { b } else { a })
    }
}

/// A miner type as the round groups it: regardless of case.
pub fn type_key(miner: Option<&str>) -> Option<String> {
    miner.map(str::to_lowercase)
}

impl Round {
    pub fn new() -> Self {
        Round::default()
    }

    /// Counts a share accepted at Unix time `at` from a device of `listing`'s fleet in `pool`, whose hash
    /// reached `difficulty`. `block` is the height of the block it found, if it found one: a block's solution
    /// share stays in the round it ends, so it is left out once that block has started the next.
    pub fn share(
        &self,
        listing: &Listing,
        pool: &PoolId,
        difficulty: f64,
        at: u64,
        block: Option<u32>,
    ) {
        let mut state = self.state.lock().unwrap();
        if state.start.at.is_none() {
            state.start.at = Some(at);
        }
        let best = Best {
            best: difficulty,
            miner: listing.miner.clone(),
            pool: pool.clone(),
            at,
        };
        let ended = block.is_some() && block == state.start.height;
        if !ended {
            let kind = state
                .types
                .entry(type_key(listing.miner.as_deref()))
                .or_default();
            *kind = kind.max(difficulty);
        }
        let fleet = state
            .fleets
            .entry(listing.fleet)
            .or_insert_with(|| Fleet::new(listing.short_address.clone()));
        if !ended {
            fleet.raise_round(&best);
        }
        fleet.raise_hour(&best);
    }

    /// Starts the next round at the block a node accepted at `height`, found at Unix time `at`. A block found
    /// before the round's own starts nothing: its answer came late.
    pub fn found(&self, height: u32, at: u64) {
        let mut state = self.state.lock().unwrap();
        if state.start.height.is_some() && state.start.at.is_some_and(|start| at < start) {
            return;
        }
        state.start = Start {
            at: Some(at),
            height: Some(height),
        };
        state.types.clear();
        state.fleets.retain(|_, fleet| {
            fleet.round = None;
            !fleet.hours.is_empty()
        });
    }

    /// Forgets the hours that left the [`HOURS`] up to the one holding Unix time `now`, and the fleets left
    /// with no share this round or in them, so fleets that stopped mining do not stay in memory.
    pub fn prune(&self, now: u64) {
        let oldest = (now - now % HOUR).saturating_sub((HOURS - 1) * HOUR);
        let mut state = self.state.lock().unwrap();
        state.fleets.retain(|_, fleet| {
            fleet.hours = fleet.hours.split_off(&oldest);
            fleet.round.is_some() || !fleet.hours.is_empty()
        });
    }

    pub fn start(&self) -> Start {
        self.state.lock().unwrap().start
    }

    /// Every fleet with a share this round, by its best, highest first.
    pub fn round(&self) -> Vec<Entry> {
        let state = self.state.lock().unwrap();
        ranked(
            state
                .fleets
                .iter()
                .filter_map(|(handle, fleet)| Some((handle, fleet, fleet.round.as_ref()?))),
        )
    }

    /// The best share any fleet reached this round; None before one.
    pub fn best(&self) -> Option<f64> {
        let state = self.state.lock().unwrap();
        state
            .fleets
            .values()
            .filter_map(|fleet| fleet.round.as_ref())
            .map(|best| best.best)
            .reduce(f64::max)
    }

    /// Every fleet with a share in the [`HOURS`] hours up to the one holding Unix time `now`, by its best in
    /// them, highest first.
    pub fn day(&self, now: u64) -> Vec<Entry> {
        let state = self.state.lock().unwrap();
        ranked(
            state
                .fleets
                .iter()
                .filter_map(|(handle, fleet)| Some((handle, fleet, fleet.day(now)?))),
        )
    }

    /// Each miner type's best share this round, by [`type_key`].
    pub fn types(&self) -> HashMap<Option<String>, f64> {
        self.state.lock().unwrap().types.clone()
    }

    /// The round as the store holds it at Unix time `now`, with rollup periods of `period` seconds.
    ///
    /// Each fleet's round best is read from `best_shares` for each whole day after the round's block, and
    /// from the block's day's `rollups` after the block's own period, which is left out: its shares before
    /// the block belong to the round it ended. The hourly bests are read from the `rollups` of the last
    /// [`HOURS`] hours of each worker with a best share today or yesterday. Every read of `rollups` goes
    /// through its primary key, which starts with the handle, so a round years long reads days, not minutes.
    pub fn seed(connection: &Connection, now: u64, period: u64) -> rusqlite::Result<Round> {
        let block: Option<(u32, i64)> = connection
            .query_row(
                "SELECT height, found_at FROM blocks \
                 WHERE result IS NOT NULL AND status != 'rejected' \
                 ORDER BY found_at DESC, height DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let workers = workers(connection)?;
        let mut state = State::default();
        let seen = |state: &mut State, handle: &Handle| -> Option<Worker> {
            let worker = workers.get(handle)?;
            state
                .fleets
                .entry(worker.fleet)
                .or_insert_with(|| Fleet::new(worker.short_address.clone()));
            Some(worker.clone())
        };

        // The round's bests, per worker.
        let mut bests: Vec<(Handle, Best)> = Vec::new();
        let first_day = match block {
            Some((height, found_at)) => {
                let found_at = found_at as u64;
                state.start = Start {
                    at: Some(found_at),
                    height: Some(height),
                };
                let day = found_at / DAY;
                let after = found_at - found_at % period + period;
                for handle in active(connection, day as i64, day as i64)? {
                    if let Some(best) = best_in(
                        connection,
                        handle,
                        after,
                        (day + 1) * DAY,
                        workers.get(&handle),
                    )? {
                        bests.push((handle, best));
                    }
                }
                day as i64 + 1
            }
            None => i64::MIN,
        };
        let mut daily: HashMap<Handle, (f64, i64, PoolId)> = HashMap::new();
        {
            let mut statement = connection
                .prepare("SELECT handle, pool, day, best FROM best_shares WHERE day >= ?1")?;
            let mut rows = statement.query([first_day])?;
            while let Some(row) = rows.next()? {
                let Ok(handle) = row.get::<_, String>(0)?.parse::<Handle>() else {
                    continue;
                };
                let (pool, day, best): (PoolId, i64, f64) = (row.get(1)?, row.get(2)?, row.get(3)?);
                match daily.get(&handle) {
                    Some((held, held_day, _))
                        if *held > best || (*held == best && *held_day <= day) => {}
                    _ => {
                        daily.insert(handle, (best, day, pool));
                    }
                }
            }
        }
        for (handle, (best, day, pool)) in daily {
            if best <= 0.0 {
                continue;
            }
            let day = day.max(0) as u64;
            let at =
                time_of(connection, handle, day * DAY, (day + 1) * DAY, best)?.unwrap_or(day * DAY);
            bests.push((
                handle,
                Best {
                    best,
                    miner: workers.get(&handle).and_then(|w| w.miner.clone()),
                    pool,
                    at,
                },
            ));
        }
        if block.is_none() {
            state.start.at = first_share(connection)?;
        }
        for (handle, best) in &bests {
            let Some(worker) = seen(&mut state, handle) else {
                continue;
            };
            let kind = state
                .types
                .entry(type_key(worker.miner.as_deref()))
                .or_default();
            *kind = kind.max(best.best);
            if let Some(fleet) = state.fleets.get_mut(&worker.fleet) {
                fleet.raise_round(best);
            }
        }

        // The hourly bests.
        let today = (now / DAY) as i64;
        let since = (now - now % HOUR).saturating_sub((HOURS - 1) * HOUR);
        for handle in active(connection, today - 1, today)? {
            let Some(worker) = seen(&mut state, &handle) else {
                continue;
            };
            let mut statement = connection.prepare_cached(
                "SELECT start, pool, best FROM rollups \
                 WHERE handle = ?1 AND transport IN ('V1', 'V2-native') AND start >= ?2 AND accepted > 0",
            )?;
            let mut rows = statement.query(params![handle.to_string(), since as i64])?;
            while let Some(row) = rows.next()? {
                let best = Best {
                    at: row.get::<_, i64>(0)? as u64,
                    pool: row.get(1)?,
                    best: row.get(2)?,
                    miner: worker.miner.clone(),
                };
                if let Some(fleet) = state.fleets.get_mut(&worker.fleet) {
                    fleet.raise_hour(&best);
                }
            }
        }
        state
            .fleets
            .retain(|_, fleet| fleet.round.is_some() || !fleet.hours.is_empty());
        Ok(Round {
            state: Mutex::new(state),
        })
    }
}

/// The fleets of `rows`, by best, highest first, then earliest, then by handle.
fn ranked<'a>(rows: impl Iterator<Item = (&'a Handle, &'a Fleet, &'a Best)>) -> Vec<Entry> {
    let mut entries: Vec<Entry> = rows
        .map(|(handle, fleet, best)| Entry {
            fleet: *handle,
            short_address: fleet.short_address.clone(),
            best: best.clone(),
        })
        .collect();
    entries.sort_by(|a, b| {
        b.best
            .best
            .total_cmp(&a.best.best)
            .then(a.best.at.cmp(&b.best.at))
            .then_with(|| a.fleet.to_string().cmp(&b.fleet.to_string()))
    });
    entries
}

/// A worker as the seed needs it: its fleet, the fleet's shortened address, and its miner type.
#[derive(Clone)]
struct Worker {
    fleet: Handle,
    short_address: Option<String>,
    miner: Option<String>,
}

/// Every worker recorded in a fleet with an address.
fn workers(connection: &Connection) -> rusqlite::Result<HashMap<Handle, Worker>> {
    let mut statement = connection.prepare(
        "SELECT handle, fleet, address, vendor, private FROM fleets WHERE address IS NOT NULL",
    )?;
    let mut rows = statement.query([])?;
    let mut workers = HashMap::new();
    while let Some(row) = rows.next()? {
        let (Ok(handle), Ok(fleet)) = (
            row.get::<_, String>(0)?.parse::<Handle>(),
            row.get::<_, String>(1)?.parse::<Handle>(),
        ) else {
            continue;
        };
        let address: String = row.get(2)?;
        let private: bool = row.get(4)?;
        workers.insert(
            handle,
            Worker {
                fleet,
                short_address: (!private).then(|| fleet::shortened(&address)),
                miner: row.get(3)?,
            },
        );
    }
    Ok(workers)
}

/// The workers with a best share on any day from `first` to `last`, days since the epoch.
fn active(connection: &Connection, first: i64, last: i64) -> rusqlite::Result<Vec<Handle>> {
    let mut statement = connection
        .prepare("SELECT DISTINCT handle FROM best_shares WHERE day BETWEEN ?1 AND ?2")?;
    let handles = statement
        .query_map([first, last], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(handles.into_iter().filter_map(|h| h.parse().ok()).collect())
}

/// The worker `handle`'s best share in the periods starting from Unix time `since` and before `until`.
fn best_in(
    connection: &Connection,
    handle: Handle,
    since: u64,
    until: u64,
    worker: Option<&Worker>,
) -> rusqlite::Result<Option<Best>> {
    connection
        .prepare_cached(
            "SELECT start, pool, best FROM rollups \
             WHERE handle = ?1 AND transport IN ('V1', 'V2-native') AND start >= ?2 AND start < ?3 \
             AND accepted > 0 ORDER BY best DESC, start LIMIT 1",
        )?
        .query_row(
            params![handle.to_string(), since as i64, until as i64],
            |row| {
                Ok(Best {
                    at: row.get::<_, i64>(0)? as u64,
                    pool: row.get(1)?,
                    best: row.get(2)?,
                    miner: worker.and_then(|w| w.miner.clone()),
                })
            },
        )
        .optional()
}

/// The start of the first period from Unix time `since` and before `until` in which the worker `handle`
/// reached `best`.
fn time_of(
    connection: &Connection,
    handle: Handle,
    since: u64,
    until: u64,
    best: f64,
) -> rusqlite::Result<Option<u64>> {
    connection
        .prepare_cached(
            "SELECT min(start) FROM rollups \
             WHERE handle = ?1 AND transport IN ('V1', 'V2-native') AND start >= ?2 AND start < ?3 \
             AND best = ?4",
        )?
        .query_row(
            params![handle.to_string(), since as i64, until as i64, best],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map(|start| start.map(|start| start as u64))
}

/// The start of the period of the instance's first accepted share, read from its first day's workers.
fn first_share(connection: &Connection) -> rusqlite::Result<Option<u64>> {
    let day: Option<i64> =
        connection.query_row("SELECT min(day) FROM best_shares", [], |row| row.get(0))?;
    let Some(day) = day else {
        return Ok(None);
    };
    let mut first: Option<u64> = None;
    for handle in active(connection, day, day)? {
        let start: Option<i64> = connection
            .prepare_cached(
                "SELECT min(start) FROM rollups \
                 WHERE handle = ?1 AND transport IN ('V1', 'V2-native') AND start >= ?2 AND accepted > 0",
            )?
            .query_row(params![handle.to_string(), day * DAY as i64], |row| row.get(0))?;
        if let Some(start) = start {
            first = Some(first.map_or(start as u64, |first| first.min(start as u64)));
        }
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant, SystemTime},
    };

    use super::*;
    use crate::{
        mining::pool::tests::id,
        store::{DATABASE, Store},
        stratum::Transport,
        telemetry::{
            Telemetry,
            fleet::{self, Member},
            rollup,
        },
    };

    /// A UTC midnight.
    const T0: u64 = 1_790_035_200;
    const PERIOD: u64 = 60;
    const F: &str = "bcrt1qfleetaddressffffffffffffffffffffffff";
    const G: &str = "bcrt1qfleetaddressgggggggggggggggggggggggg";

    fn handle(c: char) -> Handle {
        c.to_string().repeat(16).parse().unwrap()
    }

    fn listing(fleet: char, miner: Option<&str>) -> Listing {
        Listing {
            fleet: handle(fleet),
            short_address: Some(fleet::shortened(if fleet == 'f' { F } else { G })),
            miner: miner.map(str::to_owned),
            country: None,
        }
    }

    fn bests(entries: &[Entry]) -> Vec<(Handle, f64, u64)> {
        entries
            .iter()
            .map(|e| (e.fleet, e.best.best, e.best.at))
            .collect()
    }

    fn database(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("puntohunter-round-{}-{name}", std::process::id()))
            .join(DATABASE)
    }

    fn store(name: &str) -> Store {
        let path = database(name);
        let dir = path.parent().unwrap();
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        Store::open(&path).unwrap()
    }

    #[test]
    fn a_share_raises_its_fleets_bests_and_a_fleet_holds_its_highest() {
        let round = Round::new();
        assert_eq!(round.start(), Start::default());
        assert_eq!(round.best(), None);
        let f = listing('f', Some("bitaxe"));
        round.share(&f, &id("solo"), 5.0, T0 + 10, None);
        // A second device of the fleet, in another pool.
        round.share(
            &listing('f', Some("BitAxe")),
            &id("pooled"),
            9.0,
            T0 + 20,
            None,
        );
        round.share(&f, &id("solo"), 7.0, T0 + 30, None);
        round.share(&listing('g', None), &id("solo"), 8.0, T0 + 40, None);
        let list = round.round();
        assert_eq!(
            bests(&list),
            [(handle('f'), 9.0, T0 + 20), (handle('g'), 8.0, T0 + 40)]
        );
        assert_eq!(list[0].best.pool, id("pooled"));
        assert_eq!(list[0].best.miner.as_deref(), Some("BitAxe"));
        assert_eq!(list[0].short_address, Some(fleet::shortened(F)));
        assert_eq!(round.best(), Some(9.0));
        assert_eq!(bests(&round.day(T0 + 40)), bests(&list));
        assert_eq!(
            round.start(),
            Start {
                at: Some(T0 + 10),
                height: None
            },
            "before the first block, the round starts with the first share"
        );
        let types = round.types();
        assert_eq!(
            types[&Some("bitaxe".into())],
            9.0,
            "grouped regardless of case"
        );
        assert_eq!(types[&None], 8.0);
    }

    #[test]
    fn an_accepted_block_starts_a_round_without_its_solution_share() {
        let round = Round::new();
        let (f, g) = (listing('f', None), listing('g', None));
        round.share(&f, &id("solo"), 50.0, T0 + 10, None);
        // g finds block 100; its solution share is accepted before the node's answer.
        round.share(&g, &id("solo"), 1e9, T0 + 20, Some(100));
        round.found(100, T0 + 20);
        assert!(round.round().is_empty());
        assert!(round.types().is_empty());
        assert_eq!(
            round.best(),
            None,
            "the solution share stays in the round it ends"
        );
        assert_eq!(
            round.start(),
            Start {
                at: Some(T0 + 20),
                height: Some(100)
            }
        );
        // f finds block 101, but the answer comes first this time.
        round.found(101, T0 + 30);
        round.share(&f, &id("solo"), 2e9, T0 + 30, Some(101));
        round.share(&g, &id("solo"), 3.0, T0 + 31, None);
        assert_eq!(bests(&round.round()), [(handle('g'), 3.0, T0 + 31)]);
        assert_eq!(round.best(), Some(3.0));
        // The day's list keeps every share.
        assert_eq!(
            bests(&round.day(T0 + 31)),
            [(handle('f'), 2e9, T0 + 30), (handle('g'), 1e9, T0 + 20)]
        );
        // An answer for an older block, recorded late, starts nothing.
        round.found(99, T0 + 5);
        assert_eq!(round.start().height, Some(101));
    }

    #[test]
    fn an_hour_leaves_the_day() {
        let round = Round::new();
        let f = listing('f', None);
        round.share(&f, &id("solo"), 9.0, T0 + 10, None);
        round.share(&f, &id("solo"), 4.0, T0 + 3 * HOUR, None);
        let held = |now| bests(&round.day(now));
        assert_eq!(
            held(T0 + 23 * HOUR + 59 * 60),
            [(handle('f'), 9.0, T0 + 10)]
        );
        assert_eq!(
            held(T0 + 24 * HOUR),
            [(handle('f'), 4.0, T0 + 3 * HOUR)],
            "the first hour left the 24"
        );
        assert!(held(T0 + 27 * HOUR).is_empty());
        assert_eq!(bests(&round.round()), [(handle('f'), 9.0, T0 + 10)]);

        // Once a block clears the round, pruning forgets a fleet with no hour left.
        round.found(5, T0 + 27 * HOUR);
        round.prune(T0 + 26 * HOUR);
        assert_eq!(
            round.state.lock().unwrap().fleets.len(),
            1,
            "an hour is left"
        );
        round.prune(T0 + 27 * HOUR);
        assert!(round.state.lock().unwrap().fleets.is_empty());
    }

    /// Telemetry over `round` with a connected worker per (handle, listing).
    fn mining(round: &Arc<Round>) -> Arc<Telemetry> {
        Telemetry::with_round(PERIOD, round.clone())
    }

    async fn record_fleets(store: &Store) {
        store
            .commit(|t| {
                for (c, fleet, address, vendor) in [
                    ('a', 'f', F, Some("bitaxe")),
                    ('b', 'f', F, Some("NerdQAxe++")),
                    ('c', 'g', G, None),
                ] {
                    let member = Member {
                        handle: handle(c),
                        pool: id("solo"),
                        worker: c.to_string(),
                        first_seen: 0,
                        vendor: vendor.map(str::to_owned),
                    };
                    fleet::insert(t, handle(fleet), address, false, &member)?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn insert_block(store: &Store, height: u32, found_at: u64, status: &str, result: &str) {
        let (status, result) = (status.to_owned(), result.to_owned());
        store
            .commit(move |t| {
                t.execute(
                    "INSERT INTO blocks (hash, height, pool, status, result, found_at) \
                     VALUES (?1, ?2, 'solo', ?3, ?4, ?5)",
                    params![
                        format!("{height:064x}"),
                        height,
                        status,
                        result,
                        found_at as i64
                    ],
                )
            })
            .await
            .unwrap();
    }

    /// Mines shares of (worker, its fleet's listing, difficulty, Unix time) and writes every period.
    async fn mine(store: &Store, telemetry: &Arc<Telemetry>, shares: &[(char, Listing, f64, u64)]) {
        let now = Instant::now();
        let mut links = Vec::new();
        for (c, listing, _, _) in shares {
            links.push(telemetry.connect_at(
                handle(*c),
                id("solo"),
                Transport::V1,
                now,
                SystemTime::now(),
            ));
            telemetry.describe(handle(*c), listing.clone());
        }
        for (c, _, difficulty, at) in shares {
            let at = SystemTime::UNIX_EPOCH + Duration::from_secs(*at);
            telemetry.accept(handle(*c), Transport::V1, 1.0, *difficulty, None, now, at);
        }
        let rows = telemetry.finished(u64::MAX);
        store
            .commit(move |t| rollup::write(t, u64::MAX, &rows))
            .await
            .unwrap();
    }

    async fn seed(name: &str, now: u64) -> Round {
        let reader = crate::store::Reader::open(&database(name)).unwrap();
        reader
            .read(move |connection| Round::seed(connection, now, PERIOD))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_restart_seeds_the_lists_the_round_held() {
        let store = store("restart");
        record_fleets(&store).await;
        let round = Arc::new(Round::new());
        let telemetry = mining(&round);
        let (f, f2, g) = (
            listing('f', Some("bitaxe")),
            listing('f', Some("NerdQAxe++")),
            listing('g', None),
        );
        // A block on the second day at 10:00:30: before it, shares that end in its round.
        let block = T0 + DAY + 10 * HOUR + 30;
        mine(
            &store,
            &telemetry,
            &[
                ('a', f.clone(), 1e12, T0 + 5 * HOUR),
                ('c', g.clone(), 7e11, T0 + DAY + 10 * HOUR),
            ],
        )
        .await;
        insert_block(&store, 100, block, "confirmed_immature", "accepted").await;
        round.found(100, block);
        // After it: the rest of its day, then three more days.
        mine(
            &store,
            &telemetry,
            &[
                ('a', f.clone(), 300.0, T0 + DAY + 11 * HOUR),
                ('b', f2.clone(), 900.0, T0 + 2 * DAY + 3 * HOUR),
                ('c', g.clone(), 800.0, T0 + 3 * DAY + HOUR),
                ('a', f.clone(), 500.0, T0 + 4 * DAY + 2 * HOUR),
                ('c', g.clone(), 600.0, T0 + 4 * DAY + 5 * HOUR),
            ],
        )
        .await;
        let now = T0 + 4 * DAY + 6 * HOUR;
        let seeded = seed("restart", now).await;
        assert_eq!(seeded.start(), round.start());
        assert_eq!(bests(&seeded.round()), bests(&round.round()));
        assert_eq!(
            bests(&seeded.round()),
            [
                (handle('f'), 900.0, T0 + 2 * DAY + 3 * HOUR),
                (handle('g'), 800.0, T0 + 3 * DAY + HOUR)
            ]
        );
        assert_eq!(seeded.round()[0].best.miner.as_deref(), Some("NerdQAxe++"));
        assert_eq!(seeded.round()[0].short_address, Some(fleet::shortened(F)));
        assert_eq!(bests(&seeded.day(now)), bests(&round.day(now)));
        assert_eq!(seeded.types(), round.types());
    }

    #[tokio::test]
    async fn the_seed_leaves_out_the_blocks_own_period() {
        let store = store("period");
        record_fleets(&store).await;
        let telemetry = mining(&Arc::new(Round::new()));
        let block = T0 + HOUR + 30;
        mine(
            &store,
            &telemetry,
            &[
                // In the block's period: before or after it, the seed cannot tell.
                ('a', listing('f', None), 1e6, T0 + HOUR + 10),
                ('c', listing('g', None), 5.0, T0 + HOUR + 60),
            ],
        )
        .await;
        insert_block(&store, 7, block, "submitted", "accepted").await;
        let seeded = seed("period", T0 + 2 * HOUR).await;
        assert_eq!(bests(&seeded.round()), [(handle('g'), 5.0, T0 + HOUR + 60)]);
        assert_eq!(seeded.day(T0 + 2 * HOUR).len(), 2);
    }

    #[tokio::test]
    async fn a_refused_block_starts_nothing_and_an_orphaned_one_started_its_round() {
        let store = store("refused");
        record_fleets(&store).await;
        let telemetry = mining(&Arc::new(Round::new()));
        mine(
            &store,
            &telemetry,
            &[
                ('a', listing('f', None), 40.0, T0 + 600),
                ('c', listing('g', None), 30.0, T0 + 1800),
            ],
        )
        .await;
        insert_block(&store, 10, T0 + 1200, "orphaned", "accepted").await;
        insert_block(&store, 11, T0 + 2400, "rejected", "bad-txns").await;
        // Never answered: no node accepted it.
        let unanswered = "INSERT INTO blocks (hash, height, pool, status, found_at) \
                          VALUES ('ff', 12, 'solo', 'submitted', ?1)";
        store
            .commit(move |t| t.execute(unanswered, [(T0 + 2500) as i64]))
            .await
            .unwrap();
        let seeded = seed("refused", T0 + 3600).await;
        assert_eq!(
            seeded.start(),
            Start {
                at: Some(T0 + 1200),
                height: Some(10)
            }
        );
        assert_eq!(bests(&seeded.round()), [(handle('g'), 30.0, T0 + 1800)]);
    }

    #[tokio::test]
    async fn with_no_block_the_round_runs_from_the_first_share() {
        let store = store("first");
        let seeded = seed("first", T0).await;
        assert_eq!(seeded.start(), Start::default());
        assert!(seeded.round().is_empty());
        record_fleets(&store).await;
        let telemetry = mining(&Arc::new(Round::new()));
        mine(
            &store,
            &telemetry,
            &[
                ('a', listing('f', None), 40.0, T0 + 2 * DAY + 120),
                ('c', listing('g', None), 30.0, T0 + 3 * DAY + 60),
            ],
        )
        .await;
        let seeded = seed("first", T0 + 3 * DAY + 600).await;
        assert_eq!(
            seeded.start(),
            Start {
                at: Some(T0 + 2 * DAY + 120),
                height: None
            }
        );
        assert_eq!(
            bests(&seeded.round()),
            [
                (handle('f'), 40.0, T0 + 2 * DAY + 120),
                (handle('g'), 30.0, T0 + 3 * DAY + 60)
            ]
        );
        assert_eq!(
            bests(&seeded.day(T0 + 3 * DAY + 600)),
            [(handle('g'), 30.0, T0 + 3 * DAY + 60)]
        );
    }

    /// A year of best shares and a day of rollups for 100 workers: seeded within a second in a release build.
    #[tokio::test]
    async fn the_seed_of_a_year_for_a_hundred_workers_is_quick() {
        let store = store("year");
        let now = T0 + 365 * DAY + 12 * HOUR;
        store
            .commit(move |t| {
                let mut best = t.prepare(
                    "INSERT INTO best_shares (handle, pool, day, best) VALUES (?1, 'solo', ?2, ?3)",
                )?;
                let mut rollup = t.prepare(
                    "INSERT INTO rollups (start, handle, pool, transport, accepted, work, stale, duplicate, \
                     low_difficulty, bad_version, invalid, best) \
                     VALUES (?1, ?2, 'solo', 'V1', 1, 1.0, 0, 0, 0, 0, 0, ?3)",
                )?;
                let mut fleets = t.prepare(
                    "INSERT INTO fleets (fleet, handle, pool, worker, first_seen, address) \
                     VALUES (?1, ?2, 'solo', 'rig', 0, ?3)",
                )?;
                for n in 0..100u32 {
                    let handle = format!("{:a>16}", base32(n));
                    fleets.execute(params![handle, handle, format!("bcrt1q{handle}aaaaaaaaaaaaaaaaaa")])?;
                    for day in 0..=365u64 {
                        let day = T0 / DAY + day;
                        best.execute(params![handle, day as i64, (n as u64 * 1000 + day) as f64])?;
                    }
                    for minute in 0..1440u64 {
                        let start = now - DAY + minute * 60;
                        rollup.execute(params![start as i64, handle, minute as f64])?;
                    }
                }
                Ok(())
            })
            .await
            .unwrap();
        let started = Instant::now();
        let seeded = seed("year", now).await;
        let took = started.elapsed();
        assert_eq!(seeded.round().len(), 100);
        assert_eq!(seeded.day(now).len(), 100);
        if !cfg!(debug_assertions) {
            assert!(took < Duration::from_secs(1), "{took:?}");
        }
    }

    /// `n` in the handle alphabet.
    fn base32(mut n: u32) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
        let mut text = String::new();
        loop {
            text.insert(0, ALPHABET[(n % 32) as usize] as char);
            n /= 32;
            if n == 0 {
                return text;
            }
        }
    }
}
