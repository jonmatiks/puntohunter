//! A fleet as its API resource (API reference sections 2.3 and 2.4) gives it, from one model: the
//! address, then every device mining to it, by worker name, with a summary per pool, live over one SSE stream,
//! and the blocks they found. Anyone holding its handle sees the fleet and its address. The address is sent
//! in the resource, never in a stream.

use std::{collections::HashSet, convert::Infallible, sync::Arc, time::SystemTime};

use axum::response::{
    IntoResponse, Response,
    sse::{Event, KeepAlive, Sse},
};
use futures_util::stream;
use serde::Serialize;

use super::{
    Portal, REFRESH,
    api::{Figure, Rates},
    device::{Device, Solo},
    pool::Chain,
    stats::Ranks,
    streams::{self, Snapshots, StreamGuard},
    window::{Now, Window},
    worker::Unresolved,
};
use crate::{
    mining::pool::PoolId,
    store::StoreError,
    telemetry::{
        Status,
        fleet::{self, Member},
        handle::Handle,
        rollup::{self, Rate, Series},
        unix,
    },
};

/// The fleet resource.
#[derive(Debug, Serialize)]
pub struct FleetResource {
    pub fleet: String,
    /// The payout address, which the page shows; None for a fleet recorded before addresses were.
    pub address: Option<String>,
    /// Whether it is a private fleet, keyed on a fleet password as well as the address; an open fleet's page
    /// says how to make it private.
    pub private: bool,
    /// Where it stands on each stats list.
    pub ranks: Ranks,
    #[serde(flatten)]
    pub live: FleetLive,
}

/// The fleet's live values: the resource less its handle and address, and each stream event.
#[derive(Debug, Clone, Serialize)]
pub struct FleetLive {
    /// Whether the node gives no work.
    pub outage: bool,
    /// The chain the current template builds on; None during an outage. Every pool's summary shows it.
    pub chain: Option<Chain>,
    /// The pools the shown devices mine in: the configured ones in config order, then any no longer
    /// configured, by id.
    pub pools: Vec<PoolSummary>,
    /// The devices seen in the last `keep_offline_days`, by 10-minute hashrate, highest first, then by name.
    pub devices: Vec<DeviceSummary>,
    /// Days an offline device stays on the page. Its row stays in the table.
    pub keep_offline_days: u32,
}

/// One pool's summary.
#[derive(Debug, Clone, Serialize)]
pub struct PoolSummary {
    pub pool: PoolId,
    /// The pool's name, or its id when it is no longer configured.
    pub name: String,
    pub online: usize,
    pub total: usize,
    pub hash_rate: Rates,
    pub best_share: f64,
    /// Accepted and refused shares, by every device of the pool the fleet ever had.
    pub accepted: u64,
    pub rejected: u64,
    pub last_share_at: Option<u64>,
    /// Blocks found, the node's refusals aside, by every device of the pool the fleet ever had.
    pub blocks: u64,
    /// Where the pool can pay a finder.
    pub solo: Option<Solo>,
    /// The fleet's part of the pool's current window, where the pool can pay a pool output.
    pub window: Option<Figure<Window>>,
}

/// One device's row.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceSummary {
    pub handle: String,
    /// Empty for a device mining with the bare address.
    pub name: String,
    /// Its miner type, if it named one.
    pub miner: Option<String>,
    pub pool: PoolId,
    pub transports: Vec<&'static str>,
    /// `online`, `idle`, or `offline`.
    pub status: &'static str,
    pub last_seen_at: u64,
    /// Its open connections: more than one when several devices mine with its username.
    pub connections: usize,
    /// Over 10 minutes.
    pub hash_rate: Rate,
    pub best_share: f64,
    pub last_share_at: Option<u64>,
    pub accepted: u64,
    /// Its part of its pool's current window; None in a pool that cannot pay a pool output.
    pub window: Option<Figure<Window>>,
}

/// A fleet as its streams share it: its live values, and the devices whose changes it follows.
pub struct Snapshot {
    live: Option<FleetLive>,
    ours: HashSet<Handle>,
}

/// The snapshots of the fleets being watched.
pub type FleetSnapshots = Snapshots<Handle, Snapshot>;

/// A fleet's devices, each with its name.
struct Fleet {
    members: Vec<Member>,
    devices: Vec<Device>,
}

impl Portal {
    /// The fleet `text` names, with every device it ever had.
    async fn fleet(&self, text: &str) -> Result<(Handle, Fleet), Unresolved> {
        let handle = text.parse::<Handle>().map_err(|()| Unresolved::NotFound)?;
        let fleet = self.read_fleet(handle).await?;
        if fleet.members.is_empty() {
            return Err(Unresolved::NotFound);
        }
        Ok((handle, fleet))
    }

    /// The fleet `text` names, with the workers it ever had but not their figures.
    pub async fn members(&self, text: &str) -> Result<(Handle, Vec<Member>), Unresolved> {
        let handle = text.parse::<Handle>().map_err(|()| Unresolved::NotFound)?;
        let members = self
            .reader
            .read(move |connection| fleet::members(connection, handle))
            .await?;
        if members.is_empty() {
            return Err(Unresolved::NotFound);
        }
        Ok((handle, members))
    }

    async fn read_fleet(&self, handle: Handle) -> Result<Fleet, StoreError> {
        let members = self
            .reader
            .read(move |connection| fleet::members(connection, handle))
            .await?;
        let devices = self
            .devices(members.iter().map(|m| m.handle).collect())
            .await?;
        Ok(Fleet { members, devices })
    }

    fn fleet_live(&self, fleet: &Fleet, windows: &Now) -> FleetLive {
        let now = unix(SystemTime::now());
        let chain = self.chain();
        let member = |handle: Handle| fleet.members.iter().find(|m| m.handle == handle);
        let name = |handle: Handle| member(handle).map_or(String::new(), |m| m.worker.clone());
        // The devices seen in the last `keep_offline_days`, by hashrate.
        let kept = u64::from(self.keep_offline_days) * 86_400;
        let mut shown: Vec<(&Device, Status)> = fleet
            .devices
            .iter()
            .map(|device| (device, device.status(now)))
            .filter(|(_, status)| match status {
                Status::Offline { last_seen } => now.saturating_sub(*last_seen) <= kept,
                _ => true,
            })
            .collect();
        shown.sort_by(|(a, _), (b, _)| {
            b.rate()
                .hash_rate
                .total_cmp(&a.rate().hash_rate)
                .then_with(|| name(a.handle).cmp(&name(b.handle)))
        });

        let pools = self.ordered(shown.iter().map(|(d, _)| d.pool()));
        let summaries = pools
            .into_iter()
            .filter_map(|pool| {
                let devices: Vec<&(&Device, Status)> =
                    shown.iter().filter(|(d, _)| d.pool() == pool).collect();
                if devices.is_empty() {
                    return None;
                }
                // Counts take every device the fleet ever had in the pool, shown or not.
                let every = || fleet.devices.iter().filter(|d| d.pool() == pool);
                let rate = |f: fn(&Device) -> Rate| Rate::sum(devices.iter().map(|(d, _)| f(d)));
                let day = rate(|d| d.windows.day);
                let best_share = devices
                    .iter()
                    .map(|(d, _)| d.best_share())
                    .fold(0.0, f64::max);
                Some(PoolSummary {
                    pool: pool.clone(),
                    name: self.name(pool),
                    online: devices
                        .iter()
                        .filter(|(_, status)| *status == Status::Online)
                        .count(),
                    total: devices.len(),
                    hash_rate: Rates {
                        minute: rate(Device::minute_rate),
                        ten_minutes: rate(Device::rate),
                        hour: rate(|d| d.windows.hour),
                        day,
                    },
                    best_share,
                    accepted: every().map(Device::accepted).sum(),
                    rejected: every().map(Device::rejected).sum(),
                    last_share_at: devices.iter().filter_map(|(d, _)| d.last_share()).max(),
                    blocks: every().map(|d| d.blocks).sum(),
                    solo: Solo::of(
                        self.configured(pool).map(|pool| &**pool),
                        day,
                        best_share,
                        chain.as_ref(),
                    ),
                    window: windows.view(
                        pool,
                        &every().map(|d| d.handle.to_string()).collect::<Vec<_>>(),
                    ),
                })
            })
            .collect();
        let devices = shown
            .iter()
            .map(|(device, status)| DeviceSummary {
                handle: device.handle.to_string(),
                name: name(device.handle),
                miner: member(device.handle).and_then(|m| m.vendor.clone()),
                pool: device.pool().clone(),
                transports: device.transports().iter().map(|t| t.as_str()).collect(),
                status: status.as_str(),
                last_seen_at: device.last_seen(),
                connections: device.connections(),
                hash_rate: device.rate(),
                best_share: device.best_share(),
                last_share_at: device.last_share(),
                accepted: device.accepted(),
                window: windows.view(device.pool(), &[device.handle.to_string()]),
            })
            .collect();
        FleetLive {
            outage: chain.is_none(),
            chain: chain.as_ref().map(Chain::of),
            pools: summaries,
            devices,
            keep_offline_days: self.keep_offline_days,
        }
    }

    /// The fleet resource of the fleet `text` names.
    pub async fn fleet_resource(&self, text: &str) -> Result<FleetResource, Unresolved> {
        let (handle, fleet) = self.fleet(text).await?;
        Ok(self.resource(handle, &fleet).await?)
    }

    async fn resource(&self, handle: Handle, fleet: &Fleet) -> Result<FleetResource, StoreError> {
        let (address, private) = self
            .reader
            .read(move |connection| {
                Ok((
                    fleet::address(connection, handle)?,
                    fleet::private(connection, handle)?,
                ))
            })
            .await?;
        Ok(FleetResource {
            fleet: handle.to_string(),
            address,
            private,
            ranks: self.ranks(handle),
            live: self.fleet_live(fleet, &self.windows().await),
        })
    }

    /// The fleet `handle` now, as its streams share it.
    async fn fleet_now(&self, handle: Handle) -> Snapshot {
        match self.read_fleet(handle).await {
            Ok(fleet) => Snapshot {
                live: Some(self.fleet_live(&fleet, &self.windows().await)),
                ours: fleet.members.iter().map(|m| m.handle).collect(),
            },
            Err(e) => {
                tracing::error!("cannot read a fleet: {e}");
                Snapshot {
                    live: None,
                    ours: HashSet::new(),
                }
            }
        }
    }
}

/// The summary and table over SSE: at once, on a change to any of the fleet's devices, and every
/// [`REFRESH`], each from the fleet's shared snapshot. The fleet is read again for each snapshot, so a device
/// that joins appears on its own. No event carries the address.
///
/// The stream holds `guard`, its place under the stream caps, until it is dropped.
pub fn fleet_stream(portal: Arc<Portal>, handle: Handle, guard: StreamGuard) -> Response {
    let changes = portal.telemetry.subscribe();
    let mut refresh = tokio::time::interval(REFRESH);
    refresh.reset();
    let state = (portal, changes, refresh, None::<Arc<Snapshot>>, guard);
    let events = stream::unfold(
        state,
        move |(portal, mut changes, mut refresh, last, guard)| async move {
            let not_before = match &last {
                None => streams::recent(),
                Some(last) => {
                    streams::next(&mut changes, &mut refresh, |changed| {
                        last.ours.contains(&changed)
                    })
                    .await?
                }
            };
            let snapshot = portal
                .fleets
                .get(handle, not_before, || portal.fleet_now(handle))
                .await;
            let data = match &snapshot.live {
                None => String::new(),
                Some(live) => serde_json::to_string(live).unwrap_or_default(),
            };
            let event = Event::default().event("fleet").data(data);
            Some((
                Ok::<_, Infallible>(event),
                (portal, changes, refresh, Some(snapshot), guard),
            ))
        },
    );
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// The fleet's chart series: its devices summed per pool, a series for each pool its workers mine in, the
/// configured ones in config order, then any no longer configured, by id.
#[derive(Serialize)]
pub struct FleetSeries {
    pools: Vec<PoolSeries>,
}

#[derive(Serialize)]
struct PoolSeries {
    pool: PoolId,
    /// The pool's name, or its id when it is no longer configured.
    name: String,
    #[serde(flatten)]
    series: Series,
}

impl Portal {
    /// The chart series of the fleet of `members`, over the span [`Range::span`] gives.
    ///
    /// [`Range::span`]: super::worker::Range::span
    pub async fn fleet_series(
        &self,
        members: Vec<Member>,
        (since, until, bucket): (u64, u64, u64),
    ) -> Result<FleetSeries, StoreError> {
        let pools: Vec<(PoolId, String)> = self
            .ordered(members.iter().map(|m| &m.pool))
            .into_iter()
            .map(|pool| (pool.clone(), self.name(pool)))
            .collect();
        self.reader
            .read(move |connection| {
                let pools = pools
                    .into_iter()
                    .map(|(pool, name)| {
                        let handles: Vec<Handle> = members
                            .iter()
                            .filter(|m| m.pool == pool)
                            .map(|m| m.handle)
                            .collect();
                        let series = rollup::series(connection, &handles, since, until, bucket)?;
                        Ok(PoolSeries { pool, name, series })
                    })
                    .collect::<rusqlite::Result<_>>()?;
                Ok(FleetSeries { pools })
            })
            .await
    }
}
