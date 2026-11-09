//! Each device's part of its pool's current window, in a pool that can pay a pool output, and what a block
//! found now would credit it: an estimate, worded as conditional on a block being found, never a
//! balance. The window is walked at the current template's network difficulty, and kept for [`REFRESH`], so
//! the pages and their streams do not walk the journal on every render.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use bitcoin::Amount;
use serde::Serialize;

use super::{
    Portal, REFRESH,
    api::{Figure, Lacking},
};
use crate::{
    mining::{coinbase::Rewards, pool::PoolId},
    node::template::Work,
    pool::pplns::{self, Current},
};

/// The windows walked lately, by pool.
#[derive(Default)]
pub struct Windows {
    cache: Mutex<HashMap<PoolId, Cached>>,
}

struct Cached {
    at: Instant,
    size: u128,
    window: Option<Arc<Current>>,
}

/// The window of each configured pool that can pay a pool output, as a block found now would take it.
#[derive(Default)]
pub struct Now {
    /// Each such pool's window, or None without a template.
    windows: HashMap<PoolId, Option<PoolWindow>>,
}

struct PoolWindow {
    /// None while the pool's journal holds no share.
    current: Option<Arc<Current>>,
    /// The miner portion of a block found now.
    miner: Amount,
}

/// A device's or a fleet's part of a window.
#[derive(Debug, Clone, Serialize)]
pub struct Window {
    pub pool: PoolId,
    /// From 0 to 1.
    pub fraction: f64,
    /// What a block found now would credit: an estimate, never a balance. When parts differ, the next block's
    /// pool output depends on who finds it, so this takes the expected one: the miner portion times the
    /// window's weighted work over its plain work.
    pub estimate_sats: u64,
}

impl Now {
    /// The part of `pool`'s window that `handles` hold together, or why there is none; None for a pool with no
    /// window: one that cannot pay a pool output, or one no longer configured.
    pub fn view(&self, pool: &PoolId, handles: &[String]) -> Option<Figure<Window>> {
        let window = self.windows.get(pool)?;
        let Some(window) = window else {
            return Some(Figure::missing(Lacking::Template));
        };
        let PoolWindow {
            current: Some(current),
            miner,
        } = window
        else {
            return Some(Figure::missing(Lacking::Shares));
        };
        let fraction: f64 = handles.iter().map(|handle| current.part(handle)).sum();
        Some(Figure::Known(Window {
            pool: pool.clone(),
            fraction,
            estimate_sats: (miner.to_sat() as f64 * current.total as f64
                / current.plain.max(1) as f64
                * fraction) as u64,
        }))
    }
}

impl Portal {
    /// The window of each configured pool that can pay a pool output, as a block found now would take it.
    pub async fn windows(&self) -> Now {
        let template = match &*self.work.borrow() {
            Work::Template(template) => Some((
                template.block.bits,
                Amount::from_sat(template.block.coinbasevalue),
            )),
            Work::None => None,
        };
        let mut now = Now::default();
        for pool in self.pools.iter().filter(|pool| pool.can_pay_pool()) {
            let Some((bits, reward)) = template else {
                now.windows.insert(pool.id.clone(), None);
                continue;
            };
            let window = &pool.window;
            let size = pplns::size(window.window_multiplier, window.window_min_work, bits);
            let current = self.current(&pool.id, size).await;
            let miner = Rewards::new(pool, self.pool_wallet.clone()).miner(reward);
            now.windows
                .insert(pool.id.clone(), Some(PoolWindow { current, miner }));
        }
        now
    }

    async fn current(&self, pool: &PoolId, size: u128) -> Option<Arc<Current>> {
        if let Some(cached) = self.windows.cache.lock().unwrap().get(pool)
            && cached.size == size
            && cached.at.elapsed() < REFRESH
        {
            return cached.window.clone();
        }
        let id = pool.clone();
        let window = match self
            .reader
            .read(move |connection| pplns::current(connection, &id, size))
            .await
        {
            Ok(window) => window.map(Arc::new),
            Err(e) => {
                tracing::error!("cannot walk the {pool} window: {e}");
                return None;
            }
        };
        self.windows.cache.lock().unwrap().insert(
            pool.clone(),
            Cached {
                at: Instant::now(),
                size,
                window: window.clone(),
            },
        );
        window
    }
}
