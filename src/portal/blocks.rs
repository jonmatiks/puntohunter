//! The pool's blocks with their status, newest first, each tagged by pool and
//! transport and naming its finder by handle. A worker's list holds its own the same way, and a fleet's those
//! of its workers, each by name. The three block lists of the API (API reference section 2.8) are built from
//! one block model.

use bitcoin::Amount;
use serde::Serialize;

use super::Portal;
use crate::{
    mining::pool::PoolId,
    pool::{
        blocks::{self, Finders, Listed, Status},
        lifecycle::COINBASE_MATURITY,
    },
    store::StoreError,
    telemetry::fleet::Member,
};

/// Blocks a list holds at most, and a page shows: enough for a regtest run's recent ones.
pub const LIMIT: u32 = 200;

/// A block, in raw values.
#[derive(Debug, Serialize)]
pub struct Block {
    pub height: u32,
    pub hash: String,
    pub pool: PoolId,
    /// None on blocks recorded before it was noted.
    pub transport: Option<&'static str>,
    /// The finder's worker handle; None on blocks recorded before it was.
    pub finder: Option<String>,
    /// The finder's worker name, on a fleet's list, which knows it; empty for a bare-address worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finder_name: Option<String>,
    /// The finder's miner type, if it named one.
    pub miner: Option<String>,
    pub status: Status,
    /// What the block paid its finder's address and the pool wallet; None where it paid no such output (a
    /// pooled block's finder, a solo block's pool), whatever its pool's finder is now.
    pub finder_sats: Option<u64>,
    pub pool_sats: Option<u64>,
    /// What its window was allocated; None for a solo block, or one not yet allocated.
    pub allocated_sats: Option<u64>,
    /// Blocks until its coinbase matures, 0 once mature; None off the chain, or without a template.
    pub blocks_to_maturity: Option<i64>,
}

impl Block {
    /// `block`, its maturity counted from `tip`, the current tip's height.
    fn new(block: Listed, tip: Option<i64>) -> Self {
        let blocks_to_maturity = match (block.status, tip) {
            (Status::ConfirmedImmature | Status::Mature, Some(tip)) => {
                // Mature with more than COINBASE_MATURITY confirmations, as the lifecycle task counts.
                Some((COINBASE_MATURITY + 1 - (tip - i64::from(block.height) + 1)).max(0))
            }
            _ => None,
        };
        Block {
            height: block.height,
            hash: block.hash.to_string(),
            pool: block.pool.clone(),
            transport: block.transport.map(|t| t.as_str()),
            finder: block.handle.map(|h| h.to_string()),
            finder_name: None,
            miner: block.vendor,
            status: block.status,
            finder_sats: block
                .finder_sats
                .filter(|sats| *sats > Amount::ZERO)
                .map(|a| a.to_sat()),
            pool_sats: block
                .pool_sats
                .filter(|sats| *sats > Amount::ZERO)
                .map(|a| a.to_sat()),
            allocated_sats: block.allocated.map(|a| a.to_sat()),
            blocks_to_maturity,
        }
    }
}

/// A block list, as the API sends it.
#[derive(Debug, Serialize)]
pub struct Blocks {
    pub blocks: Vec<Block>,
}

impl Portal {
    /// The newest `limit` blocks of `finders`, each with its maturity against the current tip.
    pub async fn blocks(&self, finders: Finders, limit: u32) -> Result<Vec<Block>, StoreError> {
        let listed = self
            .reader
            .read(move |connection| blocks::list(connection, finders, limit))
            .await?;
        // The tip is one below the block the template builds.
        let tip = self.chain().map(|chain| i64::from(chain.height) - 1);
        Ok(listed
            .into_iter()
            .map(|block| Block::new(block, tip))
            .collect())
    }
}

/// Names each block's finder by its worker name among `members`, as a fleet's list does.
pub fn name_finders(blocks: &mut [Block], members: &[Member]) {
    for block in blocks {
        block.finder_name = members
            .iter()
            .find(|m| block.finder.as_deref() == Some(&m.handle.to_string()))
            .map(|m| m.worker.clone());
    }
}
