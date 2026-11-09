//! One device as the worker and fleet pages show it: its live values when telemetry holds it, and
//! what the rollups and fleets record, read together. A device telemetry no longer holds, after a restart or
//! once idle, shows as offline with its best share and last share from the rollups; the two sources are never
//! blended into one number.

use std::time::SystemTime;

use bitcoin::Target;

use serde::Serialize;

use super::{
    Portal,
    api::{Figure, Lacking, Rates},
    figures::Odds,
};
use crate::{
    mining::{
        pool::{Pool, PoolId},
        target,
    },
    node::template::Work,
    pool::blocks,
    store::{Reader, StoreError},
    stratum::Transport,
    telemetry::{
        Live, Status, Telemetry,
        handle::Handle,
        rollup::{self, History, Rate, Windows},
        unix,
    },
};

/// What the portal knows of a device that has mined here.
pub struct Device {
    pub handle: Handle,
    pub live: Option<Live>,
    pub history: Option<History>,
    pub windows: Windows,
    /// Blocks it found, the node's refusals aside.
    pub blocks: u64,
    /// Its accepted and refused shares since it first mined here: the store's totals and the shares
    /// telemetry has not written yet.
    accepted: u64,
    rejected: u64,
}

impl Device {
    pub fn pool(&self) -> &PoolId {
        match (&self.live, &self.history) {
            (Some(live), _) => &live.pool,
            (None, Some(history)) => &history.pool,
            (None, None) => unreachable!("a device has one or the other"),
        }
    }

    pub fn transports(&self) -> Vec<Transport> {
        match (&self.live, &self.history) {
            (Some(live), _) => live.transports.clone(),
            (None, Some(history)) => history.transport.into_iter().collect(),
            (None, None) => Vec::new(),
        }
    }

    pub fn connections(&self) -> usize {
        self.live.as_ref().map_or(0, |live| live.connections)
    }

    /// Unix seconds of the last accepted share.
    pub fn last_share(&self) -> Option<u64> {
        let history = self.history.as_ref().and_then(|h| h.last_share);
        match &self.live {
            Some(live) => live.last_share.map(unix).or(history),
            None => history,
        }
    }

    /// Unix seconds when it was last seen: from telemetry while it holds the device, and from the store after.
    pub fn last_seen(&self) -> u64 {
        match (&self.live, &self.history) {
            (Some(live), _) => unix(live.last_seen),
            (None, Some(history)) => history.last_seen,
            (None, None) => 0,
        }
    }

    pub fn status(&self, now: u64) -> Status {
        Status::new(
            self.connections() > 0,
            self.last_share(),
            self.last_seen(),
            now,
        )
    }

    pub fn best_share(&self) -> f64 {
        let history = self.history.as_ref().map_or(0.0, |h| h.best_share);
        self.live
            .as_ref()
            .map_or(history, |live| live.best_share.max(history))
    }

    /// The hashrate over the last 10 minutes, from telemetry.
    pub fn rate(&self) -> Rate {
        match &self.live {
            Some(live) => Rate {
                hash_rate: live.hash_rate,
                estimating: live.estimating,
            },
            None => Rate {
                hash_rate: 0.0,
                estimating: false,
            },
        }
    }

    /// The hashrate over the last minute, from telemetry.
    pub fn minute_rate(&self) -> Rate {
        match &self.live {
            Some(live) => Rate {
                hash_rate: live.minute_hash_rate,
                estimating: live.minute_estimating,
            },
            None => Rate {
                hash_rate: 0.0,
                estimating: false,
            },
        }
    }

    /// Unix seconds when it first connected here, from its fleet row.
    pub fn first_seen(&self) -> Option<u64> {
        self.history.as_ref().and_then(|h| h.first_seen)
    }

    /// Unix seconds when its current session began; none while disconnected.
    pub fn connected_since(&self) -> Option<u64> {
        self.live
            .as_ref()
            .and_then(|live| live.connected_since)
            .map(unix)
    }

    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    pub fn rejected(&self) -> u64 {
        self.rejected
    }
}

/// The network the current template mines on.
pub struct Chain {
    /// The height of the block the template builds.
    pub height: u32,
    pub difficulty: f64,
}

/// The devices among `handles` that have mined here, in the same order, from the store and from telemetry, whose
/// rollup periods last `period` seconds.
async fn devices(
    reader: &Reader,
    telemetry: &Telemetry,
    period: u64,
    handles: Vec<Handle>,
) -> Result<Vec<Device>, StoreError> {
    let now = unix(SystemTime::now());
    let stored = reader
        .read(move |connection| {
            handles
                .into_iter()
                .map(|handle| {
                    Ok((
                        handle,
                        rollup::history(connection, handle, period)?,
                        rollup::windows(connection, handle, now, period)?,
                        blocks::found_by(connection, handle)?,
                        rollup::totals(connection, handle)?,
                    ))
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    Ok(stored
        .into_iter()
        .filter_map(|(handle, history, windows, blocks, totals)| {
            let live = telemetry.live(handle);
            let (accepted, rejected) = telemetry.unflushed(handle, totals.through);
            (live.is_some() || history.is_some()).then_some(Device {
                handle,
                live,
                history,
                windows,
                blocks,
                accepted: totals.accepted + accepted,
                rejected: totals.rejected + rejected,
            })
        })
        .collect())
}

impl Portal {
    /// The devices among `handles` that have mined here, in the same order.
    pub async fn devices(&self, handles: Vec<Handle>) -> Result<Vec<Device>, StoreError> {
        devices(&self.reader, &self.telemetry, self.rollup_secs, handles).await
    }

    /// The device `handle` names, or None if it never mined here.
    pub async fn device(&self, handle: Handle) -> Result<Option<Device>, StoreError> {
        Ok(self.devices(vec![handle]).await?.pop())
    }

    /// The current template's height and network difficulty; none during an outage.
    pub fn chain(&self) -> Option<Chain> {
        match &*self.work.borrow() {
            Work::Template(template) => Some(Chain {
                height: template.block.height,
                difficulty: target::difficulty(Target::from_compact(template.block.bits)),
            }),
            Work::None => None,
        }
    }
}

/// The figures of solo mining: the odds of a block, and the best share's scale.
#[derive(Debug, Clone, Serialize)]
pub struct Solo {
    /// The odds over a day, a month, and a year, and the waits; or why there are none.
    pub odds: Figure<Chances>,
    /// While the day's hashrate is estimating, so are the odds.
    pub estimating: bool,
    /// The best share as a fraction of the network difficulty; None during an outage.
    pub best_vs_network: Option<f64>,
    /// In a pool that pays its finder only a part of a block, the rest to the pool output: the odds are of
    /// that payout.
    pub finder_payout: bool,
}

/// The chance of a block over a day, a month, and a year, and the waits for one, in seconds.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Chances {
    pub day: f64,
    pub month: f64,
    pub year: f64,
    pub median_wait_secs: f64,
    pub average_wait_secs: f64,
}

impl Solo {
    /// From the 24-hour hashrate `day` and the best share of a device mining in `pool`, against the current
    /// template; None in a pool that cannot pay a finder, or one no longer configured, which mines no more.
    pub fn of(
        pool: Option<&Pool>,
        day: Rate,
        best_share: f64,
        chain: Option<&Chain>,
    ) -> Option<Self> {
        let pool = pool.filter(|pool| pool.can_pay_finder())?;
        let odds = match chain {
            None => Figure::missing(Lacking::Template),
            Some(chain) => match Odds::new(day.hash_rate, chain.difficulty) {
                None => Figure::missing(Lacking::HashRate),
                Some(odds) => Figure::Known(Chances {
                    day: odds.day,
                    month: odds.month,
                    year: odds.year,
                    median_wait_secs: odds.median,
                    average_wait_secs: odds.average,
                }),
            },
        };
        Some(Solo {
            odds,
            estimating: day.estimating,
            best_vs_network: chain.map(|chain| best_share / chain.difficulty),
            finder_payout: pool.can_pay_pool(),
        })
    }
}

/// The four windows of a device's hashrate: from telemetry over 1 and 10 minutes, and from the rollups over
/// the hour and the day.
pub fn rates(device: &Device) -> Rates {
    Rates {
        minute: device.minute_rate(),
        ten_minutes: device.rate(),
        hour: device.windows.hour,
        day: device.windows.day,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::{
        mining::pool::tests::id,
        store::{DATABASE, Store},
        telemetry::{Reject, WINDOW, rollup::write},
    };

    /// A fresh database, its writer, and a reader.
    fn database(name: &str) -> (Store, Reader) {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-device-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DATABASE);
        (Store::open(&path).unwrap(), Reader::open(&path).unwrap())
    }

    /// The accepted and refused shares the portal shows for `handle`.
    async fn counts(reader: &Reader, telemetry: &Telemetry, handle: Handle) -> (u64, u64) {
        let device = devices(reader, telemetry, 60, vec![handle])
            .await
            .unwrap()
            .pop()
            .expect("it has mined here");
        (device.accepted(), device.rejected())
    }

    /// Writes the rollup rows telemetry has finished, as the rollup task does.
    async fn write_rollups(store: &Store, telemetry: &Telemetry) {
        let rows = telemetry.finished(u64::MAX);
        store
            .commit(move |t| write(t, u64::MAX, &rows))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn counts_every_share_it_ever_had_through_writes_disconnects_and_restarts() {
        let (store, reader) = database("counts");
        let telemetry = Telemetry::new(60);
        let handle: Handle = "a".repeat(16).parse().unwrap();
        let link = telemetry.connect(handle, id("solo"), Transport::V1);
        for _ in 0..3 {
            link.accepted(1.0, 1.0, None);
        }
        link.rejected(Reject::Stale);
        assert_eq!(counts(&reader, &telemetry, handle).await, (3, 1));

        let rows = telemetry.finished(u64::MAX);
        assert_eq!(
            counts(&reader, &telemetry, handle).await,
            (3, 1),
            "taken out for writing, not written yet"
        );
        store
            .commit(move |t| write(t, u64::MAX, &rows))
            .await
            .unwrap();
        assert_eq!(
            counts(&reader, &telemetry, handle).await,
            (3, 1),
            "written, and counted once"
        );

        link.accepted(1.0, 1.0, None);
        link.accepted(1.0, 1.0, None);
        drop(link);
        telemetry.evict(Instant::now() + WINDOW + Duration::from_secs(1));
        assert!(telemetry.live(handle).is_none(), "telemetry forgot it");
        assert_eq!(
            counts(&reader, &telemetry, handle).await,
            (5, 1),
            "disconnected and forgotten"
        );

        write_rollups(&store, &telemetry).await;
        let restarted = Telemetry::new(60);
        assert_eq!(
            counts(&reader, &restarted, handle).await,
            (5, 1),
            "after a restart"
        );
    }
}
