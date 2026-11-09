//! The ledger: append-only entries that turn a block's allocations into balances, which are sums
//! of entries (the schema sets out what each kind adds and takes). A block's allocations move together,
//! and [`settle`] brings them to the stage the block's status calls for, writing only the entries the move
//! needs: run twice, the second run writes nothing, so a block is credited, released, or reversed once
//! whatever runs it again.

use bitcoin::{Amount, BlockHash};
use rusqlite::{Connection, Transaction, params};

use super::{blocks::Status, lifecycle::COINBASE_MATURITY};
use crate::mining::pool::PoolId;

/// Where a block's allocations stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Not credited: never, or reversed.
    Void,
    Pending,
    Available,
}

impl Stage {
    /// Where a block's allocations belong at `status`, its block `confirmations` deep on the active chain:
    /// available once its coinbase is mature and `buffer` blocks more bury it.
    pub fn of(status: Status, confirmations: i64, buffer: u32) -> Stage {
        match status {
            Status::Rejected | Status::Orphaned => Stage::Void,
            _ if confirmations > COINBASE_MATURITY + i64::from(buffer) => Stage::Available,
            _ => Stage::Pending,
        }
    }
}

/// What settling a block wrote.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settled {
    pub entries: usize,
    /// Whether payouts paused: the block left the chain after some of its allocations were reserved or paid.
    pub paused: bool,
}

/// One allocation of a block, and where the ledger has it.
struct Allocation {
    address: String,
    pool: PoolId,
    sats: i64,
    stage: Stage,
    /// Whether a payout batch reserved its address's balance after it became available.
    spent: bool,
}

/// Brings block `hash`'s allocations to `stage`; a block with none, as a solo block, is left alone. An
/// available allocation is never taken back to pending, and one is reversed only if none of the block's was
/// reserved or paid: then payouts pause instead, and its credits stand for the operator to reconcile.
pub fn settle(t: &Transaction, hash: BlockHash, stage: Stage) -> rusqlite::Result<Settled> {
    let block = hash.to_string();
    let allocations: Vec<Allocation> = t
        .prepare_cached(
            "SELECT a.address, a.pool, a.sats, \
             COALESCE(SUM(CASE WHEN l.kind = 'pending' THEN l.sats \
                          WHEN l.kind = 'available' OR l.reverses = 'pending' THEN -l.sats END), 0), \
             COALESCE(SUM(CASE WHEN l.kind = 'available' THEN l.sats \
                          WHEN l.reverses = 'available' THEN -l.sats END), 0), \
             EXISTS (SELECT 1 FROM ledger r WHERE r.address = a.address \
                     AND r.kind IN ('reserved', 'paid') AND r.id > (SELECT MAX(id) FROM ledger \
                     WHERE block = a.block AND address = a.address AND kind = 'available')) \
             FROM allocations a LEFT JOIN ledger l ON l.block = a.block AND l.address = a.address \
             WHERE a.block = ?1 GROUP BY a.address ORDER BY a.address",
        )?
        .query_map([&block], |row| {
            let (pending, available): (i64, i64) = (row.get(3)?, row.get(4)?);
            Ok(Allocation {
                address: row.get(0)?,
                pool: row.get(1)?,
                sats: row.get(2)?,
                stage: if available > 0 {
                    Stage::Available
                } else if pending > 0 {
                    Stage::Pending
                } else {
                    Stage::Void
                },
                spent: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let spent = allocations
        .iter()
        .any(|a| a.stage == Stage::Available && a.spent);
    let mut settled = Settled::default();
    for allocation in &allocations {
        let entries: &[(&str, Option<&str>)] = match (allocation.stage, stage) {
            (Stage::Void, Stage::Pending) => &[("pending", None)],
            (Stage::Void, Stage::Available) => &[("pending", None), ("available", None)],
            (Stage::Pending, Stage::Available) => &[("available", None)],
            (Stage::Pending, Stage::Void) => &[("reversal", Some("pending"))],
            (Stage::Available, Stage::Void) if !spent => &[("reversal", Some("available"))],
            _ => &[],
        };
        for (kind, reverses) in entries {
            t.prepare_cached(
                "INSERT INTO ledger (kind, reverses, address, pool, sats, block) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                kind,
                reverses,
                allocation.address,
                allocation.pool,
                allocation.sats,
                block
            ])?;
        }
        settled.entries += entries.len();
    }
    if stage == Stage::Void && spent {
        // A pause lifted before pauses again if its block leaves the chain once more.
        settled.paused = t.execute(
            "INSERT INTO pauses (reason, block) \
             VALUES ('a block whose allocations were reserved or paid left the chain', ?1) \
             ON CONFLICT (block) DO UPDATE SET lifted_at = NULL WHERE pauses.lifted_at IS NOT NULL",
            [&block],
        )? > 0;
    }
    if settled.paused {
        tracing::error!(
            %hash,
            "CRITICAL: a block whose allocations were reserved or paid left the chain; payouts paused"
        );
    } else if settled.entries > 0 {
        tracing::info!(%hash, ?stage, entries = settled.entries, "block's allocations settled");
    }
    Ok(settled)
}

/// Whether payouts are paused: a pause not yet lifted.
pub fn paused(t: &Transaction) -> rusqlite::Result<bool> {
    t.query_row(
        "SELECT EXISTS (SELECT 1 FROM pauses WHERE lifted_at IS NULL)",
        [],
        |row| row.get(0),
    )
}

/// A pause of payouts, as `puntohunter payouts resume` lifts it.
#[derive(Debug, PartialEq, Eq)]
pub struct Pause {
    pub reason: String,
    /// The block whose leaving the chain caused it, if one did.
    pub block: Option<String>,
}

/// Lifts every pause not yet lifted, at `now`, once the operator has settled what caused it, and returns them.
/// The rows stay, so a paused block stays exempt from reconciliation; payouts resume at the payout worker's
/// next interval.
pub fn resume(t: &Transaction, now: u64) -> rusqlite::Result<Vec<Pause>> {
    t.prepare("UPDATE pauses SET lifted_at = ?1 WHERE lifted_at IS NULL RETURNING reason, block")?
        .query_map([now as i64], |row| {
            Ok(Pause {
                reason: row.get(0)?,
                block: row.get(1)?,
            })
        })?
        .collect()
}

/// A fee reserve credit the payout wallet's funds cannot back.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "refused: the payout wallet can spend {} sats, of which miners are owed {} and the fee reserve holds {}, \
     so it cannot back {} sats more",
    spendable.to_sat(),
    owed.to_sat(),
    reserve.to_sat(),
    sats.to_sat()
)]
pub struct Unbacked {
    pub sats: Amount,
    pub spendable: Amount,
    pub owed: Amount,
    pub reserve: Amount,
}

/// Credits the operator's fee reserve with `sats`, unless the payout wallet's `spendable` funds
/// cannot cover it beside what miners are owed and what the reserve already holds. Miners are owed every
/// unpaid balance, pending ones too, though their coinbases may not be spendable yet: a credit errs toward
/// refusal, never toward spending miners' funds on fees. Returns what the reserve then holds.
pub fn credit_reserve(
    t: &Transaction,
    sats: Amount,
    spendable: Amount,
) -> rusqlite::Result<Result<Amount, Unbacked>> {
    let (owed, reserve) = (owed(t)?, reserve(t)?);
    let needed = owed
        .checked_add(reserve)
        .and_then(|held| held.checked_add(sats));
    if needed.is_none_or(|needed| needed > spendable) {
        return Ok(Err(Unbacked {
            sats,
            spendable,
            owed,
            reserve,
        }));
    }
    t.execute(
        "INSERT INTO ledger (kind, sats) VALUES ('credit', ?1)",
        [sats.to_sat() as i64],
    )?;
    tracing::info!(sats = sats.to_sat(), "fee reserve credited");
    Ok(Ok(reserve + sats))
}

/// What miners are owed: every balance not yet paid, pending, available, or reserved.
pub fn owed(connection: &Connection) -> rusqlite::Result<Amount> {
    sum(
        connection,
        "SELECT COALESCE(SUM(pending + available + reserved), 0) FROM balances",
    )
}

/// What the fee reserve holds in the payout wallet: its credits, less the fees paid from it.
pub fn reserve(connection: &Connection) -> rusqlite::Result<Amount> {
    sum(
        connection,
        "SELECT COALESCE(SUM(CASE kind WHEN 'credit' THEN sats WHEN 'paid' THEN -sats ELSE 0 END), 0) \
         FROM ledger WHERE address IS NULL",
    )
}

fn sum(connection: &Connection, sql: &str) -> rusqlite::Result<Amount> {
    connection
        .query_row(sql, [], |row| row.get::<_, i64>(0))
        .map(|sats| Amount::from_sat(sats as u64))
}

/// The accounting figures the operator metrics report.
#[derive(Debug, PartialEq, Eq)]
pub struct Figures {
    /// What miners are owed, and what the fee reserve holds.
    pub owed: Amount,
    pub reserve: Amount,
    /// Blocks with a pool output not yet allocated: their solution share is not journaled, or a crash came
    /// between the two.
    pub backlog: u64,
    /// Payout batches not yet confirmed, and when the oldest of them was reserved, in Unix seconds.
    pub unconfirmed: u64,
    pub oldest_unconfirmed: Option<u64>,
}

pub fn figures(connection: &Connection) -> rusqlite::Result<Figures> {
    let backlog = connection.query_row(
        "SELECT COUNT(*) FROM blocks b WHERE b.pool_sats > 0 \
         AND NOT EXISTS (SELECT 1 FROM snapshots s WHERE s.block = b.hash)",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let (unconfirmed, oldest) = connection.query_row(
        "SELECT COUNT(*), MIN(reserved_at) FROM batches WHERE state != 'confirmed'",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
    )?;
    Ok(Figures {
        owed: owed(connection)?,
        reserve: reserve(connection)?,
        backlog: backlog as u64,
        unconfirmed: unconfirmed as u64,
        oldest_unconfirmed: oldest.map(|at| at as u64),
    })
}

#[cfg(test)]
mod tests {
    use bitcoin::{Amount, hashes::Hash};

    use super::*;
    use crate::{
        mining::pool::tests::id,
        pool::blocks::{self, FoundBlock},
        store::{DATABASE, Store},
        stratum::Transport,
    };

    const A: &str = "bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s";
    const B: &str = "bcrt1q3jeqwzg70pfkc9k4pvynlmfjlrrghp0cd705hx";

    fn store(name: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-ledger-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join(DATABASE)).unwrap()
    }

    fn hash(n: u8) -> BlockHash {
        BlockHash::from_byte_array([n; 32])
    }

    /// Records block `n` as pooled and allocated, as PPLNS allocation leaves it, before the ledger hears of it.
    async fn allocated(store: &Store, n: u8, allocations: &'static [(&'static str, i64)]) {
        store
            .commit(move |t| {
                let block = FoundBlock {
                    hash: hash(n),
                    height: 100,
                    pool: id("pooled"),
                    worker: "rig".into(),
                    transport: Transport::V1,
                    handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
                    finder_sats: Amount::ZERO,
                    pool_sats: Amount::from_sat(1_000),
                    hex: String::new(),
                };
                blocks::insert(t, &block, 0)?;
                t.execute(
                    "INSERT INTO snapshots (block, pool, cutoff, first, boundary, size, total, amount) \
                     VALUES (?1, 'pooled', 1, 1, '1', '1', '1', 1000)",
                    [hash(n).to_string()],
                )?;
                for (address, sats) in allocations {
                    t.execute(
                        "INSERT INTO allocations (block, pool, address, sats) VALUES (?1, 'pooled', ?2, ?3)",
                        params![hash(n).to_string(), address, sats],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn settle(store: &Store, n: u8, stage: Stage) -> Settled {
        store
            .commit(move |t| super::settle(t, hash(n), stage))
            .await
            .unwrap()
    }

    /// Each address's (pending, available) balance.
    async fn balances(store: &Store) -> Vec<(String, i64, i64)> {
        store
            .commit(|t| {
                t.prepare("SELECT address, pending, available FROM balances ORDER BY address")?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect()
            })
            .await
            .unwrap()
    }

    fn written(entries: usize) -> Settled {
        Settled {
            entries,
            paused: false,
        }
    }

    #[test]
    fn allocations_are_available_once_the_coinbase_is_mature_plus_the_buffer() {
        use Status::{ConfirmedImmature, Mature, Orphaned, Rejected, Submitted};
        assert_eq!(Stage::of(Submitted, 0, 0), Stage::Pending);
        assert_eq!(Stage::of(ConfirmedImmature, 100, 0), Stage::Pending);
        assert_eq!(Stage::of(Mature, 101, 0), Stage::Available);
        assert_eq!(
            Stage::of(Mature, 102, 2),
            Stage::Pending,
            "101 blocks on top"
        );
        assert_eq!(
            Stage::of(Mature, 103, 2),
            Stage::Available,
            "102 blocks on top"
        );
        assert_eq!(Stage::of(Orphaned, -1, 0), Stage::Void);
        assert_eq!(Stage::of(Rejected, 0, 0), Stage::Void);
    }

    #[tokio::test]
    async fn each_transition_moves_the_balances_once() {
        let store = store("transitions");
        allocated(&store, 1, &[(A, 600), (B, 400)]).await;
        assert_eq!(balances(&store).await, [], "allocated, not yet credited");

        assert_eq!(settle(&store, 1, Stage::Pending).await, written(2));
        let pending = [(B.into(), 400, 0), (A.into(), 600, 0)];
        assert_eq!(balances(&store).await, pending);
        assert_eq!(settle(&store, 1, Stage::Pending).await, written(0));

        // Orphaned before maturity, and back on the chain.
        assert_eq!(settle(&store, 1, Stage::Void).await, written(2));
        let none = [(B.into(), 0, 0), (A.into(), 0, 0)];
        assert_eq!(balances(&store).await, none);
        assert_eq!(settle(&store, 1, Stage::Void).await, written(0));
        assert_eq!(settle(&store, 1, Stage::Pending).await, written(2));
        assert_eq!(balances(&store).await, pending);

        // Mature plus the buffer: available once, and never pending again.
        assert_eq!(settle(&store, 1, Stage::Available).await, written(2));
        let available = [(B.into(), 0, 400), (A.into(), 0, 600)];
        assert_eq!(balances(&store).await, available);
        for stage in [Stage::Available, Stage::Pending] {
            assert_eq!(settle(&store, 1, stage).await, written(0));
        }
        assert_eq!(balances(&store).await, available);

        // Orphaned after, with nothing reserved: reversed. Back deep enough, credited and released at once.
        assert_eq!(settle(&store, 1, Stage::Void).await, written(2));
        assert_eq!(balances(&store).await, none);
        assert_eq!(settle(&store, 1, Stage::Available).await, written(4));
        assert_eq!(balances(&store).await, available);
        assert!(!store.commit(paused).await.unwrap());
    }

    #[tokio::test]
    async fn an_orphan_after_its_allocations_were_reserved_pauses_payouts_and_reverses_nothing() {
        let store = store("reserved");
        allocated(&store, 1, &[(A, 600), (B, 400)]).await;
        allocated(&store, 2, &[(A, 500)]).await;
        settle(&store, 1, Stage::Available).await;
        // A batch takes A's balance, then block 2 is released.
        store
            .commit(|t| {
                t.execute(
                    "INSERT INTO ledger (kind, address, pool, sats, batch) \
                     VALUES ('reserved', ?1, 'pooled', 600, 1)",
                    [A],
                )
            })
            .await
            .unwrap();
        settle(&store, 2, Stage::Available).await;
        assert_eq!(
            balances(&store).await,
            [(B.into(), 0, 400), (A.into(), 0, 500)]
        );

        // Block 2's was not reserved: reversed.
        assert_eq!(settle(&store, 2, Stage::Void).await, written(1));
        // Block 1's was: nothing reversed, not even B's, and payouts pause, once.
        let paused_now = Settled {
            entries: 0,
            paused: true,
        };
        assert_eq!(settle(&store, 1, Stage::Void).await, paused_now);
        assert_eq!(
            balances(&store).await,
            [(B.into(), 0, 400), (A.into(), 0, 0)]
        );
        assert!(store.commit(paused).await.unwrap());
        assert_eq!(settle(&store, 1, Stage::Void).await, written(0));
        // Back on the chain, it stands as it was.
        assert_eq!(settle(&store, 1, Stage::Available).await, written(0));

        // The operator lifts the pause, once; the row stays.
        let lifted = store.commit(|t| resume(t, 7)).await.unwrap();
        assert_eq!(
            lifted,
            [Pause {
                reason: "a block whose allocations were reserved or paid left the chain".into(),
                block: Some(BlockHash::from_byte_array([1; 32]).to_string()),
            }]
        );
        assert!(!store.commit(paused).await.unwrap());
        assert_eq!(store.commit(|t| resume(t, 8)).await.unwrap(), []);
        // Leaving the chain again pauses payouts again.
        assert_eq!(settle(&store, 1, Stage::Void).await, paused_now);
        assert!(store.commit(paused).await.unwrap());
    }

    #[tokio::test]
    async fn the_fee_reserve_takes_credits_the_wallet_can_back_beside_what_miners_are_owed() {
        let store = store("reserve");
        allocated(&store, 1, &[(A, 600), (B, 400)]).await;
        settle(&store, 1, Stage::Pending).await;
        let spendable = Amount::from_sat(5_000);
        let credit =
            |sats| store.commit(move |t| credit_reserve(t, Amount::from_sat(sats), spendable));

        assert_eq!(credit(3_000).await.unwrap(), Ok(Amount::from_sat(3_000)));
        // Owed 1,000 pending and holding 3,000: 1,000 more is the most the wallet backs.
        assert_eq!(
            credit(1_001).await.unwrap(),
            Err(Unbacked {
                sats: Amount::from_sat(1_001),
                spendable,
                owed: Amount::from_sat(1_000),
                reserve: Amount::from_sat(3_000),
            })
        );
        assert_eq!(credit(1_000).await.unwrap(), Ok(Amount::from_sat(4_000)));
        assert!(credit(1).await.unwrap().is_err());
        assert!(credit(u64::MAX).await.unwrap().is_err(), "no overflow");

        // The reserve is no miner's balance.
        assert_eq!(
            balances(&store).await,
            [(B.into(), 400, 0), (A.into(), 600, 0)]
        );
        let credits: Vec<i64> = store
            .commit(|t| {
                t.prepare("SELECT sats FROM ledger WHERE kind = 'credit' ORDER BY id")?
                    .query_map([], |row| row.get(0))?
                    .collect()
            })
            .await
            .unwrap();
        assert_eq!(credits, [3_000, 1_000]);
    }

    #[tokio::test]
    async fn the_ledger_refuses_updates_and_deletes() {
        let store = store("append-only");
        allocated(&store, 1, &[(A, 600)]).await;
        settle(&store, 1, Stage::Pending).await;
        for sql in ["UPDATE ledger SET sats = 1", "DELETE FROM ledger"] {
            let error = store
                .commit(move |t| t.execute(sql, []))
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("the ledger is append-only"),
                "{sql}: {error}"
            );
        }
        assert_eq!(balances(&store).await, [(A.into(), 600, 0)]);
    }
}
