//! A worker by handle as its API resource (API reference sections 2.5 to 2.7) gives it, from
//! one model: its status and live values, pushed over SSE as JSON, and its hash-rate series.
//!
//! The resource carries no payout address, worker name, or fleet handle: worker handles are public,
//! so any of them would lead to a finder's fleet. The portal's page puts back a name the browser saved from
//! the fleet page.

use std::{convert::Infallible, sync::Arc, time::SystemTime};

use axum::response::{
    IntoResponse, Response,
    sse::{Event, KeepAlive, Sse},
};
use futures_util::stream;
use serde::{Deserialize, Serialize};

use super::{
    Portal, REFRESH,
    api::{Figure, Portion, Rates},
    device::{self, Device, Solo},
    pool::Chain,
    streams::{self, Snapshots, StreamGuard},
    window::{Now, Window},
};
use bitcoin::{Address, Amount, address::NetworkUnchecked};

use crate::{
    config,
    mining::{coinbase::Rewards, pool::PoolId},
    node::template::Work,
    store::StoreError,
    telemetry::{
        fleet,
        handle::Handle,
        rollup::{self, Series},
        unix,
    },
};

/// The worker resource.
#[derive(Debug, Serialize)]
pub struct Worker {
    pub handle: String,
    pub pool: PoolId,
    /// The open connections' transports, or the last one's when none is open.
    pub transports: Vec<&'static str>,
    /// The miner type, if the miner named one.
    pub miner: Option<String>,
    /// The finder part its last connection asked for, as asked; None for the pool's own.
    pub part: Option<Portion>,
    /// What the current template's coinbase would pay its address as the finder, its part clamped; None
    /// without a template, or in a pool that pays no finder or is no longer configured.
    pub finder_sats: Option<u64>,
    /// Whether its pool writes its name into the blocks it finds.
    pub name_written: bool,
    /// Whether its last connection asked `+name`, which with `name_written` and its pool's name flags says why
    /// its name is or is not written.
    pub name_asked: bool,
    /// Whether the node gives no work.
    pub outage: bool,
    pub live: Live,
}

/// A worker's live values, raw: the resource's `live`, and each stream event.
#[derive(Debug, Clone, Serialize)]
pub struct Live {
    /// `online`, `idle`, or `offline`.
    pub status: &'static str,
    pub last_seen_at: u64,
    pub connections: usize,
    pub hash_rate: Rates,
    /// The share target last assigned; None while disconnected.
    pub difficulty: Option<f64>,
    pub best_share: f64,
    /// The chain the current template builds on; None during an outage.
    pub chain: Option<Chain>,
    pub last_share_at: Option<u64>,
    pub accepted: u64,
    pub rejected: u64,
    /// Blocks found, the node's refusals aside.
    pub blocks: u64,
    /// When the current session began; None while disconnected.
    pub connected_since_at: Option<u64>,
    /// When it first connected here; None for a device recorded before that was.
    pub first_seen_at: Option<u64>,
    /// The block odds; None on a pooled worker.
    pub solo: Option<Solo>,
    /// Its part of its pool's current window; None in a pool that cannot pay a pool output.
    pub window: Option<Figure<Window>>,
}

/// A `live` event of the API's stream: the live values and whether the node gives no work.
#[derive(Serialize)]
struct LiveEvent<'a> {
    outage: bool,
    #[serde(flatten)]
    live: &'a Live,
}

impl Portal {
    fn live(&self, device: &Device, windows: &Now) -> Live {
        let now = unix(SystemTime::now());
        let best_share = device.best_share();
        let chain = self.chain();
        Live {
            status: device.status(now).as_str(),
            last_seen_at: device.last_seen(),
            connections: device.connections(),
            hash_rate: device::rates(device),
            difficulty: device.live.as_ref().and_then(|live| live.difficulty),
            best_share,
            chain: chain.as_ref().map(Chain::of),
            last_share_at: device.last_share(),
            accepted: device.accepted(),
            rejected: device.rejected(),
            blocks: device.blocks,
            connected_since_at: device.connected_since(),
            first_seen_at: device.first_seen(),
            solo: Solo::of(
                self.configured(device.pool()).map(|pool| &**pool),
                device.windows.day,
                best_share,
                chain.as_ref(),
            ),
            window: windows.view(device.pool(), &[device.handle.to_string()]),
        }
    }

    /// The worker resource of `device`.
    pub async fn worker(&self, device: &Device) -> Result<Worker, StoreError> {
        let handle = device.handle;
        let (address, miner) = self
            .reader
            .read(move |connection| {
                Ok((
                    fleet::worker_address(connection, handle)?,
                    fleet::worker_vendor(connection, handle)?,
                ))
            })
            .await?;
        let live = self.live(device, &self.windows().await);
        let choices = device
            .live
            .as_ref()
            .map(|live| live.choices)
            .unwrap_or_default();
        let part = choices.part;
        let finder_sats = self.finder_sats(device.pool(), address.as_deref(), part);
        Ok(Worker {
            handle: handle.to_string(),
            pool: device.pool().clone(),
            transports: device.transports().iter().map(|t| t.as_str()).collect(),
            miner,
            part: part.map(Portion::from),
            finder_sats,
            name_written: choices.name_written,
            name_asked: choices.asked_name,
            outage: live.chain.is_none(),
            live,
        })
    }

    /// What the current template's coinbase would pay `address` as the finder in `pool`, for a worker that
    /// asked `part`: the part paid, after the clamp and the dust rules.
    fn finder_sats(
        &self,
        pool: &PoolId,
        address: Option<&str>,
        part: Option<config::Portion>,
    ) -> Option<u64> {
        let pool = self.configured(pool).filter(|pool| pool.can_pay_finder())?;
        let reward = match &*self.work.borrow() {
            Work::Template(template) => Amount::from_sat(template.block.coinbasevalue),
            Work::None => return None,
        };
        let address: Address<NetworkUnchecked> = address?.parse().ok()?;
        let script = address.assume_checked().script_pubkey();
        let (split, _) = Rewards::new(pool, self.pool_wallet.clone()).pay(reward, &script, part);
        Some(split.finder.to_sat())
    }

    /// The live values of the worker `handle` now, as the streams share them; None if it cannot be read.
    async fn live_now(&self, handle: Handle) -> Option<Live> {
        match self.device(handle).await {
            Ok(Some(device)) => Some(self.live(&device, &self.windows().await)),
            Ok(None) => None,
            Err(e) => {
                tracing::error!("cannot read a worker's history: {e}");
                None
            }
        }
    }
}

/// Why a handle in a URL names nothing to show.
pub enum Unresolved {
    /// Malformed, or never mined here.
    NotFound,
    Failed,
}

impl From<StoreError> for Unresolved {
    fn from(e: StoreError) -> Self {
        tracing::error!("cannot read the store: {e}");
        Unresolved::Failed
    }
}

/// The device `text` names.
pub async fn resolve(portal: &Portal, text: &str) -> Result<Device, Unresolved> {
    let handle = text.parse::<Handle>().map_err(|()| Unresolved::NotFound)?;
    portal.device(handle).await?.ok_or(Unresolved::NotFound)
}

/// The live values over SSE: at once, on each change to the worker, and every [`REFRESH`], each from the
/// worker's shared snapshot.
///
/// The stream holds `guard`, its place under the stream caps, until it is dropped.
pub fn live_stream(portal: Arc<Portal>, handle: Handle, guard: StreamGuard) -> Response {
    let changes = portal.telemetry.subscribe();
    let mut refresh = tokio::time::interval(REFRESH);
    refresh.reset();
    let state = (portal, changes, refresh, true, guard);
    let events = stream::unfold(
        state,
        move |(portal, mut changes, mut refresh, first, guard)| async move {
            let not_before = if first {
                streams::recent()
            } else {
                streams::next(&mut changes, &mut refresh, |changed| changed == handle).await?
            };
            let live = portal
                .workers
                .get(handle, not_before, || portal.live_now(handle))
                .await;
            let data = match &*live {
                None => String::new(),
                Some(live) => serde_json::to_string(&LiveEvent {
                    outage: live.chain.is_none(),
                    live,
                })
                .unwrap_or_default(),
            };
            let event = Event::default().event("live").data(data);
            Some((
                Ok::<_, Infallible>(event),
                (portal, changes, refresh, false, guard),
            ))
        },
    );
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// The snapshots of the workers being watched.
pub type WorkerSnapshots = Snapshots<Handle, Option<Live>>;

/// A chart's range: `?range=1h|24h|7d`, 24 hours by default.
#[derive(Deserialize)]
pub struct Range {
    range: Option<String>,
}

impl Range {
    /// The Unix times the series starts at and ends before, and its bucket, for a rollup period of `period`
    /// seconds: from the bucket the range starts in up to the end of the last whole period. None for a range
    /// not offered.
    pub fn span(&self, period: u64) -> Option<(u64, u64, u64)> {
        let (span, bucket) = match self.range.as_deref().unwrap_or("24h") {
            "1h" => (rollup::HOUR, period),
            "24h" => (24 * rollup::HOUR, period),
            // About a thousand points.
            "7d" => (7 * 24 * rollup::HOUR, 600_u64.div_ceil(period) * period),
            _ => return None,
        };
        let now = unix(SystemTime::now());
        let since = now.saturating_sub(span);
        Some((since - since % bucket, now - now % period, bucket))
    }
}

impl Portal {
    /// A worker's chart series from the rollups, over the span [`Range::span`] gives.
    pub async fn worker_series(
        &self,
        handle: Handle,
        (since, until, bucket): (u64, u64, u64),
    ) -> Result<Series, StoreError> {
        self.reader
            .read(move |connection| rollup::series(connection, &[handle], since, until, bucket))
            .await
    }
}
