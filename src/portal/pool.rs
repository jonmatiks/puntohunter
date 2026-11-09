//! The pool at a glance, naming no one: `GET /api/v1/pool` (API reference section 2.1), which
//! the portal's overview shows, in raw values.

use std::time::SystemTime;

use serde::Serialize;

use super::{Portal, VERSION, api::Portion, device};
use crate::{
    config::{self, Portion as Configured},
    mining::pool::PoolId,
    pool::{
        blocks as pool_blocks,
        lifecycle::COINBASE_MATURITY,
        payouts::{self, Standing},
    },
    store::StoreError,
    telemetry::unix,
};

/// An announced SV2 authority key rotation: the key miners pin from `switch_on`, a day, `YYYY-MM-DD`.
#[derive(Debug, Clone, Serialize)]
pub struct NextAuthorityKey {
    pub key: String,
    pub switch_on: String,
}

/// The pool resource.
#[derive(Debug, Serialize)]
pub struct Overview {
    pub network: &'static str,
    pub version: &'static str,
    pub started_at: u64,
    pub uptime_secs: u64,
    /// Whether the node gives no work.
    pub outage: bool,
    /// The chain the current template builds on; None during an outage.
    pub chain: Option<Chain>,
    /// The SV2 authority key miners pin, in SRI's base58check form.
    pub authority_key: String,
    /// The key it changes to, and when; None unless a rotation is announced.
    pub next_authority_key: Option<NextAuthorityKey>,
    /// The configured pools, in config order.
    pub pools: Vec<PoolSetup>,
    pub reward: Reward,
    pub accounting: Accounting,
    /// None without `[payouts]`: balances accrue but are not paid yet.
    pub payouts: Option<Payouts>,
    /// Blocks found, the node's refusals aside.
    pub blocks_found: u64,
    /// The best share any worker reached this round, since this instance's last block; None before one.
    pub round_best_share: Option<f64>,
}

/// The chain the current template builds on, as the pool, fleet, and worker resources give it.
#[derive(Debug, Clone, Serialize)]
pub struct Chain {
    pub tip_height: u32,
    pub network_difficulty: f64,
}

impl Chain {
    pub fn of(chain: &device::Chain) -> Self {
        Self {
            // The tip is one below the block the template builds.
            tip_height: chain.height.saturating_sub(1),
            network_difficulty: chain.difficulty,
        }
    }
}

/// A pool's Stratum URLs, its rule, and what it mines now.
#[derive(Debug, Serialize)]
pub struct PoolSetup {
    pub id: PoolId,
    pub name: String,
    /// `solo`, `pooled`, `hybrid`, or `personal`, as its finder and range make it: for the wording only.
    pub kind: &'static str,
    /// None for a protocol the pool does not serve.
    pub stratum_v1: Option<String>,
    pub stratum_v2: Option<String>,
    /// What a block pays its finder of its reward, capped at the miner portion.
    pub finder: Portion,
    /// The miner portion with the pool's fees; None when a fee is a fixed amount, which takes a different part
    /// of each reward.
    pub miner: Option<Portion>,
    pub fees: Vec<Fee>,
    /// Its PPLNS window; None in a pool that cannot pay a pool output, which has none.
    pub window: Option<WindowPolicy>,
    /// The range each worker may choose its finder part in; None where every worker gets `finder`.
    pub personal: Option<Range>,
    /// Whether its blocks name every worker, or each worker that asks with `+name`.
    pub worker_names: bool,
    pub worker_names_opt_in: bool,
    pub hash_rate: f64,
    pub devices: usize,
}

/// A pool's `personal` range, each bound a portion of the reward.
#[derive(Debug, Serialize)]
pub struct Range {
    pub min: Portion,
    pub max: Portion,
}

/// A pool's PPLNS window policy, and W now.
#[derive(Debug, Serialize)]
pub struct WindowPolicy {
    pub window_multiplier: f64,
    pub window_min_work: f64,
    /// W now, in difficulty-1 units; None during an outage.
    pub window_size: Option<f64>,
}

/// How a block's reward is split by the global fees, which a pool without fees of its own pays.
#[derive(Debug, Serialize)]
pub struct Reward {
    /// The miner portion; None when a fee is a fixed amount, which takes a different part of each reward.
    pub miner: Option<Portion>,
    pub fees: Vec<Fee>,
}

/// A fee by name, never by address.
#[derive(Debug, Serialize)]
pub struct Fee {
    pub name: String,
    pub portion: Portion,
}

/// When allocations become available, in every pool.
#[derive(Debug, Serialize)]
pub struct Accounting {
    pub available_after_confirmations: i64,
}

/// The payout terms, and where payouts stand.
#[derive(Debug, Serialize)]
pub struct Payouts {
    pub threshold_sats: u64,
    pub interval_secs: u64,
    pub standing: StandingView,
    /// None before the first batch.
    pub last: Option<Last>,
}

/// Where payouts stand: `starting`, `running`, `deferred` with its reason, `paused`, `halted`, or `locked`.
#[derive(Debug, Serialize)]
pub struct StandingView {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl From<&Standing> for StandingView {
    fn from(standing: &Standing) -> Self {
        let (state, reason) = match standing {
            Standing::Starting => ("starting", None),
            Standing::Running => ("running", None),
            Standing::Deferred(why) => ("deferred", Some(why.clone())),
            Standing::Paused => ("paused", None),
            Standing::Halted => ("halted", None),
            Standing::Locked => ("locked", None),
        };
        StandingView { state, reason }
    }
}

/// The last payout batch.
#[derive(Debug, Serialize)]
pub struct Last {
    pub batch: i64,
    pub state: String,
    pub payees: u64,
    pub paid_sats: u64,
    pub fee_sats: u64,
    /// None on a batch reserved before the time was recorded.
    pub reserved_at: Option<u64>,
}

/// The miner portion that `fees` leave; None when one is a fixed amount.
fn miner<'a>(fees: impl IntoIterator<Item = &'a Configured>) -> Option<Portion> {
    let mut percents = 0;
    for fee in fees {
        match fee {
            Configured::Ppm(ppm) => percents += ppm,
            Configured::Amount(_) => return None,
        }
    }
    Some(Portion::Ppm(config::PPM - percents))
}

impl Portal {
    /// The pool now. Its Stratum URLs name the host [`config::Public::stratum_host`] gives for `host`, the host
    /// the request named.
    pub async fn overview(&self, host: Option<&str>) -> Result<Overview, StoreError> {
        let host = self.public.stratum_host(host);
        let blocks_found = self.reader.read(pool_blocks::found).await?;
        let last = match &self.payouts {
            Some(_) => self.reader.read(payouts::last).await?,
            None => None,
        };
        let chain = self.chain();
        let fees = |fees: &[config::Fee]| -> Vec<Fee> {
            fees.iter()
                .map(|fee| Fee {
                    name: fee.name.clone(),
                    portion: fee.portion.into(),
                })
                .collect()
        };
        let pools = self
            .pools
            .iter()
            .map(|pool| {
                let (hash_rate, devices) = self.telemetry.totals(&pool.id);
                let window = pool.window;
                PoolSetup {
                    id: pool.id.clone(),
                    name: pool.name.clone(),
                    kind: pool.kind().as_str(),
                    stratum_v1: pool
                        .public_v1
                        .map(|port| format!("stratum+tcp://{host}:{port}")),
                    stratum_v2: pool
                        .public_v2
                        .map(|port| format!("stratum2+tcp://{host}:{port}/{}", self.authority_key)),
                    finder: pool.finder.into(),
                    miner: miner(pool.fees.iter().map(|fee| &fee.portion)),
                    fees: fees(&pool.fees),
                    window: pool.can_pay_pool().then(|| WindowPolicy {
                        window_multiplier: window.window_multiplier,
                        window_min_work: window.window_min_work,
                        window_size: chain.as_ref().map(|chain| {
                            (window.window_multiplier * chain.difficulty)
                                .max(window.window_min_work)
                        }),
                    }),
                    personal: pool.personal.map(|range| Range {
                        min: range.min.into(),
                        max: range.max.into(),
                    }),
                    worker_names: pool.worker_names,
                    worker_names_opt_in: pool.worker_names_opt_in,
                    hash_rate,
                    devices,
                }
            })
            .collect();
        let started_at = unix(self.started);
        Ok(Overview {
            network: self.network.chain(),
            version: VERSION,
            started_at,
            uptime_secs: unix(SystemTime::now()).saturating_sub(started_at),
            outage: chain.is_none(),
            chain: chain.as_ref().map(Chain::of),
            authority_key: self.authority_key.clone(),
            next_authority_key: self.next_authority_key.clone(),
            pools,
            reward: Reward {
                miner: miner(self.fees.iter().map(|fee| &fee.portion)),
                fees: self
                    .fees
                    .iter()
                    .map(|fee| Fee {
                        name: fee.name.clone(),
                        portion: fee.portion.into(),
                    })
                    .collect(),
            },
            accounting: Accounting {
                available_after_confirmations: COINBASE_MATURITY
                    + 1
                    + i64::from(self.accounting.maturity_buffer),
            },
            payouts: self.payouts.as_ref().map(|terms| Payouts {
                threshold_sats: terms.settings.threshold.to_sat(),
                interval_secs: terms.settings.interval.as_secs(),
                standing: (&*terms.standing.borrow()).into(),
                last: last.map(|last| Last {
                    batch: last.id,
                    state: last.state,
                    payees: last.payees,
                    paid_sats: last.paid.to_sat(),
                    fee_sats: last.fee.to_sat(),
                    reserved_at: last.reserved_at,
                }),
            }),
            blocks_found,
            round_best_share: self.telemetry.round().best(),
        })
    }
}
