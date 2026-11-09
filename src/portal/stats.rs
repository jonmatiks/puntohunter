//! The pool stats: the round since this instance's last block, the top fleets by best
//! share this round and over the last 24 hours, and by hashrate now, and the connected miners by type, as
//! `GET /api/v1/stats` (API reference section 2.12) gives them and the portal's stats page shows them.
//!
//! Each row shows its fleet by its address's first 6 and last 4 characters, the form telemetry holds, and
//! nothing else of it: no whole address, worker name, or handle, and no link.

use std::{collections::HashMap, time::SystemTime};

use serde::Serialize;

use super::Portal;
use crate::{
    config,
    geo::{Country, Source},
    mining::pool::PoolId,
    telemetry::{
        Connected,
        handle::Handle,
        round::{self, Entry},
        unix,
    },
};

/// The stats resource.
#[derive(Debug, Serialize)]
pub struct Stats {
    pub round: RoundView,
    pub best_shares: BestShares,
    /// The fleets with a worker connected, by hashrate, highest first.
    pub hash_rates: Vec<RateRow>,
    /// The connected workers by miner type: the types by hashrate, then "Other", then "Unknown".
    pub miners: Vec<MinerRow>,
    /// With `[geo]`, the connected workers by country: the countries of `country_min_workers` workers or more
    /// by workers, then "Elsewhere", then "Unknown".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub countries: Option<Vec<CountryRow>>,
    /// With `[geo]`, the fewest workers a country is shown with (`[geo] min_workers`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_min_workers: Option<usize>,
    /// The most rows each list shows (`[stats]`).
    pub limits: config::Stats,
    /// With `[geo]`, who made the country database, for the credit its licence asks for.
    #[serde(rename = "country_source", skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// The fleets and workers connected now, which the hashrates and miner types cover.
    pub fleets: usize,
    pub workers: usize,
}

/// Where the round starts.
#[derive(Debug, Serialize)]
pub struct RoundView {
    /// Unix seconds: when its block was found, or the instance's first share before the first block; None
    /// before any share.
    pub started_at: Option<u64>,
    /// The height of the block that started it; None before the first.
    pub since_block: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct BestShares {
    /// This round, highest first.
    pub round: Vec<BestRow>,
    /// The current hour and the 23 before it, highest first.
    pub last_24h: Vec<BestRow>,
}

/// A fleet's best share.
#[derive(Debug, Serialize)]
pub struct BestRow {
    /// None for a private fleet, whose row shows no address.
    pub short_address: Option<String>,
    pub best: f64,
    /// The miner type of the worker that reached it; None for one that named none.
    pub miner: Option<String>,
    pub pool: PoolId,
    /// Unix seconds, to the minute.
    pub at: u64,
}

/// A fleet's hashrate now.
#[derive(Debug, Serialize)]
pub struct RateRow {
    /// None for a private fleet.
    pub short_address: Option<String>,
    /// Over 10 minutes, its connected workers summed.
    pub hash_rate: f64,
    /// Whether every one of them is still filling its window.
    pub estimating: bool,
    pub workers: usize,
}

/// A miner type's connected workers.
#[derive(Debug, Serialize)]
pub struct MinerRow {
    /// `type`, `other` for the types after the first ten, or `unknown` for workers that named none.
    pub kind: &'static str,
    /// The type as most of its workers spell it; None on `other` and `unknown`.
    pub miner: Option<String>,
    pub workers: usize,
    pub hash_rate: f64,
    /// The best share any worker of the type reached this round, 0 before one.
    pub best: f64,
}

/// A country's connected workers.
#[derive(Debug, Serialize)]
pub struct CountryRow {
    /// `country`, `elsewhere` for the countries of fewer than `country_min_workers`, or `unknown` for workers the
    /// database gives no country for.
    pub kind: &'static str,
    /// Its ISO 3166-1 code and English name; None on `elsewhere` and `unknown`.
    pub country: Option<String>,
    pub name: Option<String>,
    pub workers: usize,
    pub hash_rate: f64,
}

/// The connected workers by country: the first `max` countries of `min_workers` workers or more by workers,
/// the rest as "Elsewhere".
fn countries(connected: &[Connected], min_workers: usize, max: usize) -> Vec<CountryRow> {
    let mut known: HashMap<&Country, (usize, f64)> = HashMap::new();
    let mut unknown = (0, 0.0);
    for worker in connected {
        let sum = match worker.listing.as_ref().and_then(|l| l.country.as_ref()) {
            Some(country) => known.entry(country).or_default(),
            None => &mut unknown,
        };
        sum.0 += 1;
        sum.1 += worker.hash_rate;
    }
    let mut shown: Vec<(&Country, (usize, f64))> = Vec::new();
    let mut elsewhere = (0, 0.0);
    for (country, sum) in known {
        if sum.0 >= min_workers {
            shown.push((country, sum));
        } else {
            elsewhere.0 += sum.0;
            elsewhere.1 += sum.1;
        }
    }
    shown.sort_by(|(a, x), (b, y)| {
        y.0.cmp(&x.0)
            .then(y.1.total_cmp(&x.1))
            .then_with(|| a.cmp(b))
    });
    for (_, sum) in shown.split_off(shown.len().min(max)) {
        elsewhere.0 += sum.0;
        elsewhere.1 += sum.1;
    }
    let mut rows: Vec<CountryRow> = shown
        .into_iter()
        .map(|(country, (workers, hash_rate))| CountryRow {
            kind: "country",
            country: Some(country.code.clone()),
            name: Some(country.name.clone()),
            workers,
            hash_rate,
        })
        .collect();
    for (kind, (workers, hash_rate)) in [("elsewhere", elsewhere), ("unknown", unknown)] {
        if workers > 0 {
            rows.push(CountryRow {
                kind,
                country: None,
                name: None,
                workers,
                hash_rate,
            });
        }
    }
    rows
}

/// A fleet's place on each list among every fleet, 1 for the first; None where it has none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Ranks {
    pub best_share: Option<usize>,
    pub best_share_24h: Option<usize>,
    pub hash_rate: Option<usize>,
}

/// Every fleet with a connected worker, by hashrate, highest first: its handle and its row.
fn rates(connected: &[Connected]) -> Vec<(Handle, RateRow)> {
    let mut fleets: HashMap<Handle, RateRow> = HashMap::new();
    for worker in connected {
        let Some(listing) = &worker.listing else {
            continue;
        };
        let row = fleets.entry(listing.fleet).or_insert_with(|| RateRow {
            short_address: listing.short_address.clone(),
            hash_rate: 0.0,
            estimating: true,
            workers: 0,
        });
        row.hash_rate += worker.hash_rate;
        row.estimating &= worker.estimating;
        row.workers += 1;
    }
    let mut rows: Vec<(Handle, RateRow)> = fleets.into_iter().collect();
    rows.sort_by(|(a, x), (b, y)| {
        y.hash_rate
            .total_cmp(&x.hash_rate)
            .then(y.workers.cmp(&x.workers))
            .then_with(|| a.to_string().cmp(&b.to_string()))
    });
    rows
}

/// The connected workers by miner type, grouped regardless of case: the first `max` types by hashrate,
/// the rest as "Other", and the workers that named none as "Unknown". `bests` holds each type's best this
/// round, by [`round::type_key`].
fn miners(
    connected: &[Connected],
    bests: &HashMap<Option<String>, f64>,
    max: usize,
) -> Vec<MinerRow> {
    struct Kind {
        spellings: HashMap<String, usize>,
        workers: usize,
        hash_rate: f64,
    }
    let mut kinds: HashMap<Option<String>, Kind> = HashMap::new();
    for worker in connected {
        let miner = worker.listing.as_ref().and_then(|l| l.miner.as_deref());
        let kind = kinds.entry(round::type_key(miner)).or_insert_with(|| Kind {
            spellings: HashMap::new(),
            workers: 0,
            hash_rate: 0.0,
        });
        if let Some(miner) = miner {
            *kind.spellings.entry(miner.to_owned()).or_default() += 1;
        }
        kind.workers += 1;
        kind.hash_rate += worker.hash_rate;
    }
    let best = |key: &Option<String>| bests.get(key).copied().unwrap_or(0.0);
    let unknown = kinds.remove(&None).map(|kind| MinerRow {
        kind: "unknown",
        miner: None,
        workers: kind.workers,
        hash_rate: kind.hash_rate,
        best: best(&None),
    });
    let mut named: Vec<(Option<String>, Kind)> = kinds.into_iter().collect();
    named.sort_by(|(a, x), (b, y)| {
        y.hash_rate
            .total_cmp(&x.hash_rate)
            .then(y.workers.cmp(&x.workers))
            .then_with(|| a.cmp(b))
    });
    let mut rows: Vec<MinerRow> = Vec::new();
    let mut other: Option<MinerRow> = None;
    for (n, (key, kind)) in named.into_iter().enumerate() {
        if n < max {
            // The spelling most of its workers use, the first by text on a tie.
            let spelling = kind
                .spellings
                .into_iter()
                .max_by(|(a, x), (b, y)| x.cmp(y).then_with(|| b.cmp(a)))
                .map(|(spelling, _)| spelling);
            rows.push(MinerRow {
                kind: "type",
                miner: spelling,
                workers: kind.workers,
                hash_rate: kind.hash_rate,
                best: best(&key),
            });
            continue;
        }
        let row = other.get_or_insert(MinerRow {
            kind: "other",
            miner: None,
            workers: 0,
            hash_rate: 0.0,
            best: 0.0,
        });
        row.workers += kind.workers;
        row.hash_rate += kind.hash_rate;
        row.best = row.best.max(best(&key));
    }
    rows.extend(other);
    rows.extend(unknown);
    rows
}

/// Where `fleet` stands in `fleets`, 1 for the first.
fn rank<'a>(mut fleets: impl Iterator<Item = &'a Handle>, fleet: Handle) -> Option<usize> {
    fleets.position(|handle| *handle == fleet).map(|n| n + 1)
}

impl Portal {
    fn best_rows(&self, entries: Vec<Entry>, max: usize) -> Vec<BestRow> {
        entries
            .into_iter()
            .take(max)
            .map(|entry| BestRow {
                short_address: entry.short_address,
                best: entry.best.best,
                miner: entry.best.miner,
                pool: entry.best.pool,
                at: entry.best.at,
            })
            .collect()
    }

    /// The stats now.
    pub fn stats(&self) -> Stats {
        let round = self.telemetry.round();
        let start = round.start();
        let connected = self.telemetry.connected_workers();
        let rates = rates(&connected);
        let limits = self.stats_limits;
        Stats {
            round: RoundView {
                started_at: start.at,
                since_block: start.height,
            },
            best_shares: BestShares {
                round: self.best_rows(round.round(), limits.best_shares_round),
                last_24h: self
                    .best_rows(round.day(unix(SystemTime::now())), limits.best_shares_24h),
            },
            fleets: rates.len(),
            workers: connected.len(),
            hash_rates: rates
                .into_iter()
                .take(limits.hash_rates)
                .map(|(_, row)| row)
                .collect(),
            miners: miners(&connected, &round.types(), limits.miner_types),
            countries: self
                .geo
                .as_ref()
                .map(|_| countries(&connected, self.country_min_workers, limits.countries)),
            country_min_workers: self.geo.as_ref().map(|_| self.country_min_workers),
            limits,
            source: self.geo.as_ref().map(|geo| geo.source()),
        }
    }

    /// Where `fleet` stands on each list, among every fleet.
    pub fn ranks(&self, fleet: Handle) -> Ranks {
        let round = self.telemetry.round();
        let rates = rates(&self.telemetry.connected_workers());
        Ranks {
            best_share: rank(round.round().iter().map(|e| &e.fleet), fleet),
            best_share_24h: rank(
                round.day(unix(SystemTime::now())).iter().map(|e| &e.fleet),
                fleet,
            ),
            hash_rate: rank(rates.iter().map(|(handle, _)| handle), fleet),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::Listing;

    /// The default length of every list.
    const TOP: usize = 10;

    fn handle(c: char) -> Handle {
        c.to_string().repeat(16).parse().unwrap()
    }

    fn worker(fleet: char, miner: Option<&str>, hash_rate: f64, estimating: bool) -> Connected {
        Connected {
            listing: Some(Listing {
                fleet: handle(fleet),
                short_address: Some(format!("bc1q{fleet}{fleet}…{fleet}{fleet}{fleet}{fleet}")),
                miner: miner.map(str::to_owned),
                country: None,
            }),
            hash_rate,
            estimating,
        }
    }

    #[test]
    fn a_fleet_sums_its_workers_and_estimates_while_every_one_does() {
        let connected = [
            worker('a', None, 1e12, true),
            worker('b', None, 3e12, false),
            worker('a', None, 1.5e12, false),
            worker('c', None, 2e9, true),
        ];
        let rows = rates(&connected);
        let summary: Vec<(Handle, f64, bool, usize)> = rows
            .iter()
            .map(|(h, r)| (*h, r.hash_rate, r.estimating, r.workers))
            .collect();
        assert_eq!(
            summary,
            [
                (handle('b'), 3e12, false, 1),
                (handle('a'), 2.5e12, false, 2),
                (handle('c'), 2e9, true, 1)
            ]
        );
        assert_eq!(rank(rows.iter().map(|(h, _)| h), handle('a')), Some(2));
        assert_eq!(rank(rows.iter().map(|(h, _)| h), handle('z')), None);
    }

    #[test]
    fn types_group_regardless_of_case_and_fold_after_ten() {
        let mut connected = vec![
            worker('a', Some("bitaxe"), 5e12, false),
            worker('a', Some("BitAxe"), 1e12, false),
            worker('b', Some("bitaxe"), 1e12, false),
            worker('c', None, 2e12, false),
            worker('d', None, 1e9, false),
        ];
        // Twelve more types, each slower than the last.
        for n in 0..12 {
            connected.push(worker(
                'e',
                Some(&format!("type{n:02}")),
                1e11 - n as f64,
                false,
            ));
        }
        let mut bests = HashMap::new();
        bests.insert(Some("bitaxe".to_owned()), 7e9);
        bests.insert(Some("type11".to_owned()), 3e8);
        bests.insert(Some("type10".to_owned()), 2e8);
        bests.insert(None, 4e5);
        let rows = miners(&connected, &bests, TOP);
        assert_eq!(rows.len(), TOP + 2);
        assert_eq!(
            (
                rows[0].kind,
                rows[0].miner.as_deref(),
                rows[0].workers,
                rows[0].hash_rate,
                rows[0].best
            ),
            ("type", Some("bitaxe"), 3, 7e12, 7e9),
            "spelt as most of its workers are"
        );
        assert_eq!(rows[1].miner.as_deref(), Some("type00"));
        assert_eq!(rows[9].miner.as_deref(), Some("type08"));
        let other = &rows[TOP];
        assert_eq!(
            (
                other.kind,
                other.miner.as_deref(),
                other.workers,
                other.best
            ),
            ("other", None, 3, 3e8),
            "type09, type10, and type11 fold into Other"
        );
        let unknown = &rows[TOP + 1];
        assert_eq!(
            (
                unknown.kind,
                unknown.workers,
                unknown.hash_rate,
                unknown.best
            ),
            ("unknown", 2, 2.001e12, 4e5)
        );
        assert_eq!(
            rows.iter().map(|row| row.workers).sum::<usize>(),
            connected.len(),
            "the rows sum to the connected workers"
        );
    }

    fn located(code: Option<&str>, hash_rate: f64) -> Connected {
        let mut worker = worker('a', None, hash_rate, false);
        worker.listing.as_mut().unwrap().country = code.map(|code| Country {
            code: code.into(),
            name: format!("{code} land"),
        });
        worker
    }

    #[test]
    fn countries_under_three_workers_fold_into_elsewhere() {
        let connected = [
            located(Some("TR"), 1.0),
            located(Some("TR"), 1.0),
            located(Some("TR"), 1.0),
            located(Some("US"), 5.0),
            located(Some("US"), 5.0),
            located(Some("US"), 5.0),
            located(Some("US"), 5.0),
            located(Some("JP"), 7.0),
            located(Some("DE"), 2.0),
            located(Some("DE"), 2.0),
            located(None, 3.0),
        ];
        let rows: Vec<_> = countries(&connected, 3, TOP)
            .into_iter()
            .map(|r| (r.kind, r.country, r.name, r.workers, r.hash_rate))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "country",
                    Some("US".into()),
                    Some("US land".into()),
                    4,
                    20.0
                ),
                ("country", Some("TR".into()), Some("TR land".into()), 3, 3.0),
                ("elsewhere", None, None, 3, 11.0),
                ("unknown", None, None, 1, 3.0),
            ]
        );
        let every: Vec<_> = countries(&connected, 1, TOP)
            .into_iter()
            .map(|r| (r.kind, r.country, r.workers))
            .collect();
        assert_eq!(
            every,
            [
                ("country", Some("US".into()), 4),
                ("country", Some("TR".into()), 3),
                ("country", Some("DE".into()), 2),
                ("country", Some("JP".into()), 1),
                ("unknown", None, 1),
            ],
            "at 1, no country folds"
        );

        // Twelve countries: past the cap, those with the fewest workers fold, whatever min_workers allows.
        let many: Vec<Connected> = (0..12)
            .flat_map(|n| {
                let code = format!("C{n:02}");
                (0..12 - n).map(move |_| located(Some(code.as_str()), 1.0))
            })
            .collect();
        let rows = countries(&many, 1, 10);
        assert_eq!(rows.len(), 11);
        assert_eq!(rows[9].country.as_deref(), Some("C09"));
        assert_eq!((rows[10].kind, rows[10].workers), ("elsewhere", 2 + 1));
        let rows = countries(&many, 1, 3);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            (rows[3].kind, rows[3].workers),
            ("elsewhere", (1..=9).sum())
        );
    }

    #[test]
    fn no_workers_make_no_rows() {
        assert!(countries(&[], 3, TOP).is_empty());
        assert!(miners(&[], &HashMap::new(), TOP).is_empty());
        assert!(rates(&[]).is_empty());
    }
}
