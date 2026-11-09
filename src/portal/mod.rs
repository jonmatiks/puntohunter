//! The web portal: the JSON API under `/api/v1`, each resource and its stream built
//! from one model, and on every other path the portal's pages, the reference web app embedded and assembled at
//! startup (`site`), which read the API in the browser. Plain HTTP on the LAN listener, and the binary stays the
//! one artifact.
//!
//! Public URLs key on opaque worker and fleet handles, never on the address. Only a fleet's own JSON
//! resource carries the payout address, no page carries any, and nothing carries a full Stratum username.
//! Nothing here logs a request, so neither a lookup's body nor a balance token is ever logged.

mod api;
mod balance;
mod blocks;
mod device;
mod figures;
mod fleet;
mod group;
mod health;
mod lookup;
mod pool;
mod site;
mod stats;
mod streams;
mod window;
mod worker;

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime},
};

use axum::{
    Router,
    http::{HeaderMap, Uri, header},
    routing::get,
};
use tokio::{net::TcpListener, sync::watch};

pub use self::{
    fleet::FleetSnapshots,
    lookup::Lookups,
    pool::NextAuthorityKey,
    site::{Site, SiteError},
    streams::StreamCaps,
    window::Windows,
    worker::WorkerSnapshots,
};
use crate::{
    config::{self, Network, Portion},
    geo::Geo,
    group::Members,
    mining::pool::{Pool, PoolId},
    node::{self, template::Work},
    pool::{journal::Journal, payouts::Standing},
    store::Reader,
    telemetry::{Telemetry, handle::HandleKey},
};

/// The running version every page's footer shows, such as v0.1.5+fb8d1c55 (crate::VERSION).
const VERSION: &str = concat!("v", env!("PUNTOHUNTER_VERSION"));

/// How often a live stream re-sends its values unprompted, so hashrates decay and "ago" advances.
const REFRESH: Duration = Duration::from_secs(5);

/// What the portal reads.
pub struct Portal {
    pub network: Network,
    /// The configured pools, in config order.
    pub pools: Vec<Arc<Pool>>,
    /// Where miners connect, for the overview's connection settings.
    pub public: config::Public,
    /// The SV2 authority key miners pin, in SRI's base58check form.
    pub authority_key: String,
    /// The key it changes to, and when, while a rotation is announced.
    pub next_authority_key: Option<NextAuthorityKey>,
    pub handles: HandleKey,
    pub telemetry: Arc<Telemetry>,
    pub reader: Reader,
    /// The coinbase's fees, by name and portion; never their addresses.
    pub fees: Vec<FeeShare>,
    /// Seconds a rollup period lasts.
    pub rollup_secs: u64,
    /// The current template, for the network's height and difficulty.
    pub work: watch::Receiver<Work>,
    /// The primary node's state, for the health check.
    pub node: watch::Receiver<node::health::State>,
    /// Every node's standing, for the health check's fleet state and counts.
    pub nodes: watch::Receiver<node::fleet::Status>,
    /// The share journal, whose failure stops pooled credit.
    pub journal: Journal,
    pub lookups: Lookups,
    /// The open event streams, capped per client address and in all.
    pub streams: Arc<StreamCaps>,
    /// The reverse proxy whose `X-Forwarded-For` names the client.
    pub trusted_proxy: Option<IpAddr>,
    /// Days an offline device stays on its fleet's page (`[portal] keep_offline_days`).
    pub keep_offline_days: u32,
    /// The origins whose browser pages may call the API.
    pub allowed_origins: Vec<String>,
    /// When the pool started, for its uptime.
    pub started: SystemTime,
    /// The pool wallet's script, which takes what a finder leaves: for what a block found now would allocate.
    pub pool_wallet: bitcoin::ScriptBuf,
    /// The PPLNS window policy and the maturity buffer.
    pub accounting: config::Accounting,
    /// The payout terms and where payouts stand; None without `[payouts]`.
    pub payouts: Option<PayoutTerms>,
    /// The current windows, walked lately.
    pub windows: Windows,
    /// The snapshots of the workers and fleets the live streams watch.
    pub workers: WorkerSnapshots,
    pub fleets: FleetSnapshots,
    /// The group's members, read in the background; None without `[group]`.
    pub group: Option<Members>,
    /// The country database, whose credit the stats page carries; None without `[geo]`.
    pub geo: Option<Arc<Geo>>,
    /// The fewest connected workers a country is shown with (`[geo] min_workers`).
    pub country_min_workers: usize,
    /// How many rows each list of the stats page shows (`[stats]`).
    pub stats_limits: config::Stats,
    /// The portal's pages and their files, `web/` as the operator brands it.
    pub site: Site,
}

/// The payout terms the overview publishes, and where payouts stand.
pub struct PayoutTerms {
    pub settings: config::Payouts,
    pub standing: watch::Receiver<Standing>,
}

/// A fee as the overview names it.
pub struct FeeShare {
    pub name: String,
    pub portion: Portion,
}

impl From<&config::Fee> for FeeShare {
    fn from(fee: &config::Fee) -> Self {
        FeeShare {
            name: fee.name.clone(),
            portion: fee.portion,
        }
    }
}

/// Binds `bind` and serves the portal in the background. The listener is bound when this returns.
pub async fn listen(bind: SocketAddr, portal: Portal) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    let app = router(Arc::new(portal)).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("portal stopped: {e}");
        }
    });
    tracing::info!(bind = %local, "portal listening");
    Ok(local)
}

fn router(portal: Arc<Portal>) -> Router {
    let site = portal.clone();
    Router::new()
        .nest("/api/v1", api::router(&portal.allowed_origins))
        // Every other path is the site's: a file of the app, `/` its overview, or its 404 page. GET and HEAD
        // only; any other method is 405.
        .fallback_service(get(async move |uri: Uri| site.site.response(&uri)))
        .with_state(portal)
}

impl Portal {
    /// The configured pool `id` names; None for one no longer configured, whose stored rows still show.
    fn configured(&self, id: &PoolId) -> Option<&Arc<Pool>> {
        self.pools.iter().find(|pool| pool.id == *id)
    }

    /// What the pages call pool `id`: its name, or its id when it is no longer configured.
    fn name(&self, id: &PoolId) -> String {
        self.configured(id)
            .map_or_else(|| id.to_string(), |pool| pool.name.clone())
    }

    /// `ids`, once each, as the pages list pools: the configured ones in config order, then any no longer
    /// configured, by id.
    fn ordered<'a>(&'a self, ids: impl IntoIterator<Item = &'a PoolId>) -> Vec<&'a PoolId> {
        let ids: Vec<&PoolId> = ids.into_iter().collect();
        let mut gone: Vec<&PoolId> = ids
            .iter()
            .copied()
            .filter(|id| self.configured(id).is_none())
            .collect();
        gone.sort();
        gone.dedup();
        self.pools
            .iter()
            .map(|pool| &pool.id)
            .filter(|id| ids.contains(id))
            .chain(gone)
            .collect()
    }
}

/// The host a request named, without its port.
fn host_of(headers: &HeaderMap) -> Option<&str> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let host = match host.find(']') {
        // An IPv6 literal, [::1]:8080.
        Some(end) => &host[..=end],
        None => host.split(':').next().unwrap_or(host),
    };
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn the_host_drops_its_port() {
        let host = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_str(value).unwrap());
            host_of(&headers).map(str::to_owned)
        };
        assert_eq!(host("pool.lan:8080").as_deref(), Some("pool.lan"));
        assert_eq!(host("192.0.2.5").as_deref(), Some("192.0.2.5"));
        assert_eq!(host("[::1]:8080").as_deref(), Some("[::1]"));
        assert_eq!(host(":8080"), None);
        assert_eq!(host_of(&HeaderMap::new()), None);
    }
}
