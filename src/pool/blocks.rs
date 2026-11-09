//! The blocks table: each block the pool found, once per hash, and where it stands.

use bitcoin::{Amount, BlockHash, hashes::Hash};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use crate::{mining::pool::PoolId, stratum::Transport, telemetry::handle::Handle};

/// A block found by a share, on its way to the node.
#[derive(Debug, Clone)]
pub struct FoundBlock {
    pub hash: BlockHash,
    pub height: u32,
    pub pool: PoolId,
    /// The finder's worker name, the part of its username after the address.
    pub worker: String,
    pub transport: Transport,
    pub handle: Handle,
    /// What the block's reward outputs pay the finder's address and the pool wallet.
    pub finder_sats: Amount,
    pub pool_sats: Amount,
    /// The block in hex, as submitblock takes it.
    pub hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Sent to the node, and not yet seen on its chain.
    Submitted,
    /// The node refused it: a genuine reject.
    Rejected,
    ConfirmedImmature,
    /// Its coinbase is spendable.
    Mature,
    /// Off the node's active chain.
    Orphaned,
}

impl Status {
    /// The status as a reader would say it.
    pub fn label(self) -> &'static str {
        match self {
            Status::Submitted => "submitted",
            Status::Rejected => "rejected",
            Status::ConfirmedImmature => "confirmed, immature",
            Status::Mature => "mature",
            Status::Orphaned => "orphaned",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Submitted => "submitted",
            Status::Rejected => "rejected",
            Status::ConfirmedImmature => "confirmed_immature",
            Status::Mature => "mature",
            Status::Orphaned => "orphaned",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [
            Status::Submitted,
            Status::Rejected,
            Status::ConfirmedImmature,
            Status::Mature,
            Status::Orphaned,
        ]
        .into_iter()
        .find(|status| status.as_str() == text)
    }
}

/// As its code, which the JSON API sends (API reference section 2.8).
impl serde::Serialize for Status {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A block as the portal lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub hash: BlockHash,
    pub height: u32,
    pub pool: PoolId,
    /// The finder's transport and handle; None on blocks recorded before it was.
    pub transport: Option<Transport>,
    pub handle: Option<Handle>,
    pub status: Status,
    /// What the reward outputs paid the finder's address and the pool wallet; None on blocks recorded before
    /// they were.
    pub finder_sats: Option<Amount>,
    pub pool_sats: Option<Amount>,
    /// What its window was allocated, once it was: a pooled or hybrid block's.
    pub allocated: Option<Amount>,
    /// The finder's miner type, if it named one.
    pub vendor: Option<String>,
}

/// Whose blocks a list holds.
#[derive(Debug, Clone, Copy)]
pub enum Finders {
    All,
    Worker(Handle),
    /// The workers of the fleet this handle names.
    Fleet(Handle),
}

/// The newest `limit` blocks of `finders`, newest first.
pub fn list(
    connection: &Connection,
    finders: Finders,
    limit: u32,
) -> rusqlite::Result<Vec<Listed>> {
    let (worker, fleet) = match finders {
        Finders::All => (None, None),
        Finders::Worker(handle) => (Some(handle.to_string()), None),
        Finders::Fleet(handle) => (None, Some(handle.to_string())),
    };
    let mut statement = connection.prepare(
        "SELECT hash, height, pool, transport, handle, status, finder_sats, pool_sats, \
         (SELECT amount FROM snapshots WHERE block = hash), \
         (SELECT vendor FROM fleets f WHERE f.handle = blocks.handle) FROM blocks \
         WHERE (?1 IS NULL OR handle = ?1) \
         AND (?2 IS NULL OR handle IN (SELECT handle FROM fleets WHERE fleet = ?2)) \
         ORDER BY found_at DESC, height DESC LIMIT ?3",
    )?;
    statement
        .query_map(params![worker, fleet, limit], |row| {
            let text = |index: usize| row.get::<_, Option<String>>(index);
            let status = text(5)?.unwrap_or_default();
            Ok(Listed {
                hash: hash(row, 0)?,
                height: row.get(1)?,
                pool: row.get(2)?,
                transport: text(3)?.as_deref().and_then(Transport::parse),
                handle: text(4)?.and_then(|h| h.parse().ok()),
                status: Status::parse(&status).ok_or_else(|| conversion(5, status))?,
                finder_sats: row
                    .get::<_, Option<i64>>(6)?
                    .map(|sats| Amount::from_sat(sats as u64)),
                pool_sats: row
                    .get::<_, Option<i64>>(7)?
                    .map(|sats| Amount::from_sat(sats as u64)),
                allocated: row
                    .get::<_, Option<i64>>(8)?
                    .map(|sats| Amount::from_sat(sats as u64)),
                vendor: text(9)?,
            })
        })?
        .collect()
}

/// How many blocks the pool found, the node's refusals aside.
pub fn found(connection: &Connection) -> rusqlite::Result<u64> {
    connection.query_row(
        "SELECT COUNT(*) FROM blocks WHERE status != 'rejected'",
        [],
        |row| row.get::<_, i64>(0).map(|n| n as u64),
    )
}

/// How many blocks the worker `handle` found, the node's refusals aside.
pub fn found_by(connection: &Connection, handle: Handle) -> rusqlite::Result<u64> {
    connection.query_row(
        "SELECT COUNT(*) FROM blocks WHERE handle = ?1 AND status != 'rejected'",
        [handle.to_string()],
        |row| row.get::<_, i64>(0).map(|n| n as u64),
    )
}

/// Records `block` as submitted, unless its hash is already recorded, whichever session found it. A pooled or
/// hybrid block takes its window cutoff from the journal if its solution share is already there; otherwise
/// the journal sets it once the share's commit is written.
pub fn insert(
    transaction: &Transaction,
    block: &FoundBlock,
    found_at: u64,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT OR IGNORE INTO blocks \
         (hash, height, pool, status, found_at, worker, transport, handle, finder_sats, pool_sats, cutoff) \
         VALUES (?1, ?2, ?3, 'submitted', ?4, ?5, ?6, ?7, ?8, ?9, \
                 (SELECT sequence FROM journal WHERE proof = ?10))",
        params![
            block.hash.to_string(),
            block.height,
            block.pool,
            found_at as i64,
            block.worker,
            block.transport.as_str(),
            block.handle.to_string(),
            block.finder_sats.to_sat() as i64,
            block.pool_sats.to_sat() as i64,
            &block.hash.to_byte_array()[..],
        ],
    )?;
    Ok(())
}

/// Records the node's answer to block `hash`, marking a genuine reject rejected.
pub fn record_answer(
    transaction: &Transaction,
    hash: BlockHash,
    result: &str,
    rejected: bool,
) -> rusqlite::Result<()> {
    transaction.execute(
        "UPDATE blocks SET result = ?2, status = IIF(?3, 'rejected', status) WHERE hash = ?1",
        params![hash.to_string(), result, rejected],
    )?;
    Ok(())
}

/// Block `hash`'s height and when it was found, in Unix seconds, if it is recorded.
pub fn placed(connection: &Connection, hash: BlockHash) -> rusqlite::Result<Option<(u32, u64)>> {
    connection
        .query_row(
            "SELECT height, found_at FROM blocks WHERE hash = ?1",
            [hash.to_string()],
            |row| Ok((row.get(0)?, row.get::<_, i64>(1)? as u64)),
        )
        .optional()
}

/// The blocks the node never answered for, oldest first.
pub fn unanswered(transaction: &Transaction) -> rusqlite::Result<Vec<BlockHash>> {
    let mut statement = transaction
        .prepare("SELECT hash FROM blocks WHERE result IS NULL ORDER BY found_at, hash")?;
    statement.query_map([], |row| hash(row, 0))?.collect()
}

/// A block whose status follows the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tracked {
    pub hash: BlockHash,
    pub status: Status,
    /// Whether its allocations are pending in the ledger, waiting for maturity.
    pub unreleased: bool,
}

/// The blocks whose status follows the chain, answered and not rejected: those at `min_height` or above, and
/// those deeper whose allocations are still pending.
pub fn tracked(transaction: &Transaction, min_height: u32) -> rusqlite::Result<Vec<Tracked>> {
    let mut statement = transaction.prepare(
        "SELECT hash, status, hash IN (SELECT block FROM unreleased) FROM blocks \
         WHERE result IS NOT NULL AND status != 'rejected' \
         AND (height >= ?1 OR hash IN (SELECT block FROM unreleased)) ORDER BY height",
    )?;
    statement
        .query_map([min_height], |row| {
            let text: String = row.get(1)?;
            let status = Status::parse(&text).ok_or_else(|| conversion(1, text))?;
            Ok(Tracked {
                hash: hash(row, 0)?,
                status,
                unreleased: row.get(2)?,
            })
        })?
        .collect()
}

pub fn set_status(
    transaction: &Transaction,
    hash: BlockHash,
    status: Status,
) -> rusqlite::Result<()> {
    transaction.execute(
        "UPDATE blocks SET status = ?2 WHERE hash = ?1",
        params![hash.to_string(), status.as_str()],
    )?;
    Ok(())
}

fn hash(row: &Row, index: usize) -> rusqlite::Result<BlockHash> {
    let text: String = row.get(index)?;
    text.parse().map_err(|_| conversion(index, text))
}

fn conversion(index: usize, text: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        format!("unexpected value {text:?}").into(),
    )
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash;

    use super::*;
    use crate::{
        mining::pool::tests::id,
        store::{DATABASE, Store},
    };

    fn store(name: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-blocks-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join(DATABASE)).unwrap()
    }

    fn found(n: u8, height: u32) -> FoundBlock {
        FoundBlock {
            hash: BlockHash::from_byte_array([n; 32]),
            height,
            pool: id("solo"),
            worker: "rig".into(),
            transport: Transport::V1,
            handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
            finder_sats: Amount::ZERO,
            pool_sats: Amount::ZERO,
            hex: String::new(),
        }
    }

    #[tokio::test]
    async fn records_a_block_once_per_hash() {
        let store = store("once");
        let block = found(1, 10);
        for _ in 0..2 {
            let block = block.clone();
            store.commit(move |t| insert(t, &block, 0)).await.unwrap();
        }
        let count: u32 = store
            .commit(|t| t.query_row("SELECT COUNT(*) FROM blocks", [], |row| row.get(0)))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn counts_a_workers_blocks_but_not_the_rejected() {
        let store = store("found-by");
        let (a, b) = (found(1, 10), found(2, 11));
        let (handle, rejected) = (a.handle, b.hash);
        store
            .commit(move |t| {
                insert(t, &a, 0)?;
                insert(t, &b, 0)?;
                set_status(t, rejected, Status::Rejected)
            })
            .await
            .unwrap();
        let other: Handle = "bbbbbbbbbbbbbbbb".parse().unwrap();
        let counts = store
            .commit(move |t| Ok((found_by(t, handle)?, found_by(t, other)?)))
            .await
            .unwrap();
        assert_eq!(counts, (1, 0));
    }

    #[tokio::test]
    async fn lists_the_newest_first_every_finders_one_or_a_fleets() {
        let store = store("list");
        let mut other = found(2, 11);
        other.handle = "bbbbbbbbbbbbbbbb".parse().unwrap();
        other.transport = Transport::V2Native;
        other.pool = id("hybrid");
        other.finder_sats = Amount::from_sat(104_989_500);
        other.pool_sats = Amount::from_sat(205_285_500);
        let (a, b) = (found(1, 10), other.clone());
        store
            .commit(move |t| {
                insert(t, &a, 100)?;
                insert(t, &b, 200)
            })
            .await
            .unwrap();
        let all = store.commit(|t| list(t, Finders::All, 10)).await.unwrap();
        assert_eq!(
            all.iter().map(|b| b.height).collect::<Vec<_>>(),
            [11, 10],
            "newest first"
        );
        assert_eq!(
            (all[0].transport, all[0].handle, all[0].status),
            (
                Some(Transport::V2Native),
                Some(other.handle),
                Status::Submitted
            )
        );
        assert_eq!(
            (all[0].pool.clone(), all[0].finder_sats, all[0].pool_sats),
            (id("hybrid"), Some(other.finder_sats), Some(other.pool_sats))
        );
        let mine = store
            .commit(move |t| list(t, Finders::Worker(other.handle), 10))
            .await
            .unwrap();
        assert_eq!(mine.len(), 1);
        let fleet: Handle = "cccccccccccccccc".parse().unwrap();
        store
            .commit(move |t| {
                t.execute(
                    "INSERT INTO fleets (fleet, handle, pool, worker, first_seen) \
                     VALUES (?1, 'aaaaaaaaaaaaaaaa', 'solo', 'rig', 0)",
                    [fleet.to_string()],
                )
            })
            .await
            .unwrap();
        let fleets = store
            .commit(move |t| list(t, Finders::Fleet(fleet), 10))
            .await
            .unwrap();
        assert_eq!(
            fleets.iter().map(|b| b.height).collect::<Vec<_>>(),
            [10],
            "only the fleet's worker's"
        );
        assert_eq!(
            store
                .commit(|t| list(t, Finders::All, 1))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn tracks_answered_blocks_that_were_not_rejected() {
        let store = store("tracked");
        let (a, b, c, d) = (found(1, 10), found(2, 11), found(3, 12), found(4, 5));
        let hashes = (a.hash, b.hash, c.hash, d.hash);
        store
            .commit(move |t| {
                for block in [&a, &b, &c, &d] {
                    insert(t, block, 0)?;
                }
                record_answer(t, a.hash, "accepted", false)?;
                record_answer(t, b.hash, "time-too-new", true)?;
                record_answer(t, d.hash, "duplicate", false)
            })
            .await
            .unwrap();

        assert_eq!(store.commit(unanswered).await.unwrap(), [hashes.2]);
        let tracked = |min_height| {
            store.commit(move |t| {
                Ok(tracked(t, min_height)?
                    .into_iter()
                    .map(|block| (block.hash, block.status))
                    .collect::<Vec<_>>())
            })
        };
        assert_eq!(
            tracked(6).await.unwrap(),
            [(hashes.0, Status::Submitted)],
            "b rejected, c unanswered, d too deep"
        );

        store
            .commit(move |t| set_status(t, hashes.0, Status::Mature))
            .await
            .unwrap();
        assert_eq!(
            tracked(0).await.unwrap(),
            [(hashes.3, Status::Submitted), (hashes.0, Status::Mature)]
        );
    }
}
