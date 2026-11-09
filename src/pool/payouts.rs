//! The payout worker. Once an interval, a batch pays each address whose
//! available balances, every pool's together, reach the threshold, its whole balance in one output, up
//! to the outputs per batch in address order. The fee comes from the fee reserve, and the change returns to
//! the pool wallet.
//!
//! A batch makes one payment, whatever crashes or times out (the schema sets out its states). One ledger
//! transaction reserves its balances, its fee, and the inputs the wallet funded it with; the worker then
//! locks those inputs. The signed transaction is persisted before it is first sent, and from then on only
//! those bytes are sent: a crash or an RPC timeout is resolved by asking the wallet where the transaction
//! stands and sending the same bytes again, never by funding a second one.
//!
//! On mainnet the payout wallet is encrypted and locked between payouts. A batch is funded and reserved all
//! the same, and waits for its signature until the operator unlocks the wallet.

use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
    time::SystemTime,
};

use bitcoin::{
    Address, Amount, OutPoint, Transaction, address::NetworkUnchecked, consensus::encode,
};
use rusqlite::{OptionalExtension, params};
use tokio::{
    sync::watch,
    time::{Instant, MissedTickBehavior},
};

use super::{ledger, reconcile};
use crate::{
    config::{Payouts, Rate},
    failpoint, metrics,
    mining::pool::PoolId,
    node::NodeError,
    store::{Store, StoreError, backup},
    telemetry::unix,
    wallet::{Funded, SignError, Wallet},
};

/// How deep a batch's transaction must be before the batch is confirmed and its paid entries written. Until
/// then a reorg that takes the transaction out leaves the batch reserved, its balances and inputs held, and
/// the worker sends the same bytes again.
pub const CONFIRMATIONS: i64 = 6;

/// An address a batch pays: its available balance in each pool, together in one output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payee {
    pub address: String,
    /// Each pool with an available balance, and the balance.
    pub balances: Vec<(PoolId, i64)>,
}

impl Payee {
    pub fn sats(&self) -> Amount {
        Amount::from_sat(self.balances.iter().map(|(_, sats)| *sats as u64).sum())
    }
}

/// Why no batch was reserved. Balances stay as they were, and the next interval tries again.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum Deferral {
    #[error("the fee rate of {rate} sat/vB is above the cap of {cap} sat/vB")]
    FeeRate { rate: f64, cap: f64 },
    #[error("the node has no fee estimate for confirmation within {target} blocks")]
    NoEstimate { target: u16 },
    #[error(
        "the batch's fee is {} sats, but the fee reserve has {} sats free",
        fee.to_sat(),
        free.to_sat()
    )]
    Reserve { fee: Amount, free: Amount },
    #[error("the payout wallet cannot fund it: {0}")]
    Unfunded(String),
    #[error("payouts are paused")]
    Paused,
    #[error("its balances changed while it was funded")]
    Changed,
}

/// A batch to reserve: its payees, and the transaction the wallet funded to pay them, at `fee_rate` sat/vB.
pub struct Plan {
    pub payees: Vec<Payee>,
    pub tx: Transaction,
    pub fee: Amount,
    pub fee_rate: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Reserved,
    Signed,
    Broadcast,
}

/// A batch not yet confirmed: its funded transaction and, once signed, its signed one.
pub struct Unsettled {
    pub id: i64,
    pub state: State,
    pub funded: Transaction,
    pub tx: Option<Transaction>,
}

/// The addresses a batch pays now, in address order and up to `outputs` of them: each whose available
/// balances together reach `threshold`, and are not dust in an output to it. The others carry forward.
pub fn due(
    t: &rusqlite::Transaction,
    threshold: Amount,
    outputs: u32,
) -> rusqlite::Result<Vec<Payee>> {
    let mut payees: Vec<Payee> = Vec::new();
    let mut rows = t.prepare_cached(
        "SELECT address, pool, available FROM balances WHERE available > 0 ORDER BY address, pool",
    )?;
    for row in rows.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))? {
        let (address, pool, sats): (String, PoolId, i64) = row?;
        match payees.last_mut() {
            Some(payee) if payee.address == address => payee.balances.push((pool, sats)),
            _ => payees.push(Payee {
                address,
                balances: vec![(pool, sats)],
            }),
        }
    }
    Ok(payees
        .into_iter()
        .filter(|payee| payee.sats() >= threshold.max(dust(&payee.address)))
        .take(outputs as usize)
        .collect())
}

/// The smallest output to `address` that is not dust. Every address in the ledger was checked when its miner
/// authorized; one that did not parse would never be paid.
fn dust(address: &str) -> Amount {
    address
        .parse::<Address<NetworkUnchecked>>()
        .map_or(Amount::MAX_MONEY, |address| {
            address.assume_checked().script_pubkey().minimal_non_dust()
        })
}

/// Reserves `plan` under a new batch ID, all in this transaction: each payee's balances, the fee from the fee
/// reserve, and the funded transaction with its inputs. A deferred plan writes nothing.
pub fn reserve(t: &rusqlite::Transaction, plan: &Plan) -> rusqlite::Result<Result<i64, Deferral>> {
    if ledger::paused(t)? {
        return Ok(Err(Deferral::Paused));
    }
    for payee in &plan.payees {
        for (pool, sats) in &payee.balances {
            let available: i64 = t.query_row(
                "SELECT COALESCE((SELECT available FROM balances WHERE address = ?1 AND pool = ?2), 0)",
                params![payee.address, pool],
                |row| row.get(0),
            )?;
            if available != *sats {
                return Ok(Err(Deferral::Changed));
            }
        }
    }
    let free = Amount::from_sat(t.query_row(FREE, [], |row| row.get::<_, i64>(0))? as u64);
    if free < plan.fee {
        return Ok(Err(Deferral::Reserve {
            fee: plan.fee,
            free,
        }));
    }
    t.execute(
        "INSERT INTO batches (state, funded, fee, fee_rate, reserved_at) \
         VALUES ('reserved', ?1, ?2, ?3, ?4)",
        params![
            encode::serialize_hex(&plan.tx),
            plan.fee.to_sat() as i64,
            plan.fee_rate,
            unix(SystemTime::now()) as i64
        ],
    )?;
    let batch = t.last_insert_rowid();
    for input in &plan.tx.input {
        t.prepare_cached("INSERT INTO batch_inputs (txid, vout, batch) VALUES (?1, ?2, ?3)")?
            .execute(params![
                input.previous_output.txid.to_string(),
                input.previous_output.vout,
                batch
            ])?;
    }
    for payee in &plan.payees {
        for (pool, sats) in &payee.balances {
            t.prepare_cached(
                "INSERT INTO ledger (kind, address, pool, sats, batch) VALUES ('reserved', ?1, ?2, ?3, ?4)",
            )?
            .execute(params![payee.address, pool, sats, batch])?;
        }
    }
    t.execute(
        "INSERT INTO ledger (kind, sats, batch) VALUES ('reserved', ?1, ?2)",
        params![plan.fee.to_sat() as i64, batch],
    )?;
    Ok(Ok(batch))
}

/// What the fee reserve has free for new batches' fees: its credits, less every fee a batch reserved.
const FREE: &str = "SELECT COALESCE(SUM(CASE kind WHEN 'credit' THEN sats \
                    WHEN 'reserved' THEN -sats ELSE 0 END), 0) FROM ledger WHERE address IS NULL";

/// The batches not yet confirmed, oldest first.
pub fn unsettled(t: &rusqlite::Transaction) -> rusqlite::Result<Vec<Unsettled>> {
    t.prepare_cached(
        "SELECT id, state, funded, tx FROM batches WHERE state != 'confirmed' ORDER BY id",
    )?
    .query_map([], |row| {
        let state = match row.get_ref(1)?.as_str()? {
            "reserved" => State::Reserved,
            "signed" => State::Signed,
            _ => State::Broadcast,
        };
        let tx: Option<String> = row.get(3)?;
        Ok(Unsettled {
            id: row.get(0)?,
            state,
            funded: decode(2, &row.get::<_, String>(2)?)?,
            tx: tx.map(|tx| decode(3, &tx)).transpose()?,
        })
    })?
    .collect()
}

pub(super) fn decode(column: usize, hex: &str) -> rusqlite::Result<Transaction> {
    encode::deserialize_hex(hex).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// The newest batch, as the overview shows it.
#[derive(Debug, PartialEq, Eq)]
pub struct Last {
    pub id: i64,
    pub state: String,
    /// The addresses it pays, and what it pays them all.
    pub payees: u64,
    pub paid: Amount,
    pub fee: Amount,
    /// Unix seconds; None on a batch reserved before the time was recorded.
    pub reserved_at: Option<u64>,
}

pub fn last(connection: &rusqlite::Connection) -> rusqlite::Result<Option<Last>> {
    connection
        .query_row(
            "SELECT b.id, b.state, b.fee, b.reserved_at, \
             (SELECT COUNT(DISTINCT address) FROM ledger WHERE kind = 'reserved' AND batch = b.id), \
             (SELECT COALESCE(SUM(sats), 0) FROM ledger \
              WHERE kind = 'reserved' AND batch = b.id AND address IS NOT NULL) \
             FROM batches b ORDER BY b.id DESC LIMIT 1",
            [],
            |row| {
                Ok(Last {
                    id: row.get(0)?,
                    state: row.get(1)?,
                    fee: Amount::from_sat(row.get::<_, i64>(2)? as u64),
                    reserved_at: row.get::<_, Option<i64>>(3)?.map(|at| at as u64),
                    payees: row.get::<_, i64>(4)? as u64,
                    paid: Amount::from_sat(row.get::<_, i64>(5)? as u64),
                })
            },
        )
        .optional()
}

/// Records batch `id`'s signed transaction, the only one it will ever send.
pub fn sign(t: &rusqlite::Transaction, id: i64, tx: &Transaction) -> rusqlite::Result<()> {
    t.execute(
        "UPDATE batches SET state = 'signed', tx = ?2, txid = ?3 WHERE id = ?1 AND state = 'reserved'",
        params![
            id,
            encode::serialize_hex(tx),
            tx.compute_txid().to_string()
        ],
    )?;
    Ok(())
}

/// Records that the node took batch `id`'s transaction.
pub fn broadcast(t: &rusqlite::Transaction, id: i64) -> rusqlite::Result<()> {
    t.execute(
        "UPDATE batches SET state = 'broadcast' WHERE id = ?1 AND state = 'signed'",
        [id],
    )?;
    Ok(())
}

/// Confirms batch `id` and writes its paid entries, one for each reserved entry, the fee's among them.
/// Returns false, writing nothing, for a batch already confirmed.
pub fn confirm(t: &rusqlite::Transaction, id: i64) -> rusqlite::Result<bool> {
    let confirmed = t.execute(
        "UPDATE batches SET state = 'confirmed' WHERE id = ?1 AND state IN ('signed', 'broadcast')",
        [id],
    )? > 0;
    if confirmed {
        t.execute(
            "INSERT INTO ledger (kind, address, pool, sats, batch) \
             SELECT 'paid', address, pool, sats, batch FROM ledger \
             WHERE kind = 'reserved' AND batch = ?1 ORDER BY id",
            [id],
        )?;
    }
    Ok(confirmed)
}

/// Where payouts stand, as the overview shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Reconciling at startup, or waiting for the node to reconcile.
    Starting,
    Running,
    /// The batch due is deferred, for this reason.
    Deferred(String),
    /// Paused for reconciliation: a block whose allocations were reserved or paid left the chain.
    Paused,
    /// Halted by startup reconciliation until a start finds no mismatch.
    Halted,
    /// A batch is reserved and waits for the operator to unlock the payout wallet to sign it.
    Locked,
}

/// Starts the worker and returns where payouts stand. It reconciles first, and halts payouts on any mismatch
/// (module reconcile). Its first batch comes an interval after the start, and the change of every batch goes
/// to `change`, the pool wallet.
pub fn spawn(
    wallet: Wallet,
    store: Store,
    payouts: Payouts,
    change: Address,
) -> watch::Receiver<Standing> {
    let (standing, receiver) = watch::channel(Standing::Starting);
    tokio::spawn(run(Worker {
        wallet,
        store,
        payouts,
        change,
        standing,
        locked: AtomicBool::new(false),
    }));
    receiver
}

struct Worker {
    wallet: Wallet,
    store: Store,
    payouts: Payouts,
    change: Address,
    standing: watch::Sender<Standing>,
    /// Whether a batch waited for the wallet's unlock at the last interval, so the wait is logged once.
    locked: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Node(#[from] NodeError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

async fn run(worker: Worker) {
    let interval = worker.payouts.interval;
    // Startup reconciliation first, retried each interval until the node answers.
    loop {
        match worker.reconcile().await {
            Ok(mismatches) if mismatches.is_empty() => break,
            Ok(mismatches) => {
                for mismatch in mismatches {
                    tracing::error!("payout reconciliation failed: {mismatch}");
                }
                tracing::error!(
                    "CRITICAL: payouts halted until a start finds the books, the wallet, and the chain agree"
                );
                metrics::set_payouts_halted(true);
                worker.standing.send_replace(Standing::Halted);
                return;
            }
            Err(e) => {
                tracing::warn!("payout reconciliation waits for the next interval: {e}");
                tokio::time::sleep(interval).await;
            }
        }
    }
    metrics::set_payouts_halted(false);
    worker.standing.send_replace(Standing::Running);
    tracing::info!("payouts reconciled with the wallet and the chain");
    let mut ticks = tokio::time::interval_at(Instant::now() + interval, interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        if let Err(e) = worker.tick().await {
            tracing::warn!("payouts wait for the next interval: {e}");
        }
        if let Err(e) = worker.cover().await {
            tracing::warn!("cannot compare the wallet with the liabilities: {e}");
        }
    }
}

impl Worker {
    /// Locks the inputs of the batches not yet confirmed where the wallet lost or never took their locks,
    /// then returns each mismatch between the books, the wallet, and the chain.
    async fn reconcile(&self) -> Result<Vec<String>, Error> {
        let unsettled = self.store.commit(unsettled).await?;
        self.lock(&unsettled).await?;
        let (mut mismatches, books) = self
            .store
            .commit(|t| Ok((reconcile::audit(t)?, reconcile::books(t)?)))
            .await?;
        mismatches.extend(reconcile::compare(&books, &self.holdings().await?));
        Ok(mismatches)
    }

    async fn holdings(&self) -> Result<reconcile::Holdings, NodeError> {
        Ok(reconcile::Holdings {
            locked: self.wallet.locked().await?,
            transactions: self.wallet.transactions().await?,
            held: self.wallet.held().await?,
        })
    }

    /// Reports what the wallet holds, and whether that covers what miners are owed and the fee reserve.
    async fn cover(&self) -> Result<(), Error> {
        let books = self.store.commit(reconcile::books).await?;
        let holdings = self.holdings().await?;
        let due = reconcile::due(&books, &holdings);
        metrics::set_wallet(holdings.held, holdings.held >= due);
        Ok(())
    }

    /// Takes every unconfirmed batch forward, then reserves the next if one is due. Nothing runs while
    /// payouts are paused.
    async fn tick(&self) -> Result<(), Error> {
        let paused = self.store.commit(ledger::paused).await?;
        // Each interval, so a pause from a reorg while running, or one lifted, shows without a restart.
        metrics::set_payouts_paused(paused);
        if paused {
            tracing::debug!("payouts are paused");
            self.standing.send_replace(Standing::Paused);
            return Ok(());
        }
        let unsettled = self.store.commit(unsettled).await?;
        self.lock(&unsettled).await?;
        let mut waiting = false;
        for batch in unsettled {
            waiting |= self.settle(batch).await?;
        }
        waiting |= self.next().await?;
        self.wait_for_unlock(waiting);
        Ok(())
    }

    /// Records whether a batch waits for the wallet's unlock, which outranks what [`Worker::next`] made of
    /// the batch due: the operator has a batch to sign.
    fn wait_for_unlock(&self, waiting: bool) {
        metrics::set_payouts_locked(waiting);
        if waiting {
            self.standing.send_replace(Standing::Locked);
        }
        if waiting && !self.locked.swap(true, Ordering::Relaxed) {
            tracing::info!(
                "payout batch reserved: it is signed once the payout wallet is unlocked"
            );
        } else if !waiting {
            self.locked.store(false, Ordering::Relaxed);
        }
    }

    /// Locks each input of `batches` that the wallet has neither locked nor spent, so no new batch is funded
    /// with it: after a node restart, say, or once a reorg drops a batch's transaction from the mempool.
    async fn lock(&self, batches: &[Unsettled]) -> Result<(), Error> {
        let locked: HashSet<OutPoint> = self.wallet.locked().await?.into_iter().collect();
        let inputs = batches
            .iter()
            .flat_map(|batch| &batch.funded.input)
            .map(|input| input.previous_output);
        for input in inputs.filter(|input| !locked.contains(input)) {
            self.wallet.lock(input).await?;
        }
        Ok(())
    }

    /// Takes a batch as far as it goes now: signs it, sends its one transaction until that is
    /// [`CONFIRMATIONS`] deep, and then confirms it. Returns whether it waits for the wallet's unlock to be
    /// signed.
    async fn settle(&self, batch: Unsettled) -> Result<bool, Error> {
        let id = batch.id;
        let (tx, state) = match batch.tx {
            Some(tx) => (tx, batch.state),
            None => {
                let tx = match self.wallet.sign(&batch.funded).await {
                    Ok(tx) => tx,
                    Err(SignError::Locked) => return Ok(true),
                    Err(SignError::Node(e)) => return Err(e.into()),
                };
                let signed = tx.clone();
                self.store.commit(move |t| sign(t, id, &signed)).await?;
                tracing::info!(batch = id, txid = %tx.compute_txid(), "payout batch signed");
                failpoint::exit_if(failpoint::AFTER_SIGNING);
                (tx, State::Signed)
            }
        };
        let txid = tx.compute_txid();
        match self.wallet.confirmations(txid).await? {
            Some(depth) if depth >= CONFIRMATIONS => {
                if self.store.commit(move |t| confirm(t, id)).await? {
                    tracing::info!(batch = id, %txid, "payout batch confirmed: its payees are paid");
                }
                return Ok(false);
            }
            Some(depth) if depth > 0 => {}
            // Never sent, waiting in the mempool, or out of a block a reorg took away: the same bytes again.
            _ => {
                self.wallet.send(&tx).await?;
                failpoint::exit_if(failpoint::AFTER_BROADCAST);
            }
        }
        if state == State::Signed {
            self.store.commit(move |t| broadcast(t, id)).await?;
            tracing::info!(batch = id, %txid, "payout batch broadcast");
            backup::now();
        }
        Ok(false)
    }

    /// Reserves the batch due, if any, and takes it forward. Returns whether it waits for the wallet's unlock.
    async fn next(&self) -> Result<bool, Error> {
        let (threshold, outputs) = (self.payouts.threshold, self.payouts.outputs_per_batch);
        let payees = self
            .store
            .commit(move |t| due(t, threshold, outputs))
            .await?;
        if payees.is_empty() {
            self.run_on();
            return Ok(false);
        }
        let plan = match self.plan(payees).await? {
            Ok(plan) => plan,
            Err(deferral) => {
                self.defer(deferral);
                return Ok(false);
            }
        };
        let (funded, payees, sats, fee) = (
            plan.tx.clone(),
            plan.payees.len(),
            plan.payees.iter().map(Payee::sats).sum::<Amount>(),
            plan.fee,
        );
        let id = match self.store.commit(move |t| reserve(t, &plan)).await? {
            Ok(id) => id,
            Err(deferral) => {
                self.defer(deferral);
                return Ok(false);
            }
        };
        self.run_on();
        tracing::info!(
            batch = id,
            payees,
            sats = sats.to_sat(),
            fee = fee.to_sat(),
            "payout batch reserved"
        );
        failpoint::exit_if(failpoint::AFTER_RESERVATION);
        let batch = Unsettled {
            id,
            state: State::Reserved,
            funded,
            tx: None,
        };
        self.lock(std::slice::from_ref(&batch)).await?;
        self.settle(batch).await
    }

    /// Has the wallet fund a transaction paying `payees` at [`rate`]'s rate, unless it defers the batch.
    async fn plan(&self, payees: Vec<Payee>) -> Result<Result<Plan, Deferral>, NodeError> {
        let estimate = match self.payouts.rate {
            Rate::Estimate(target) => self.wallet.estimate_fee_rate(target).await?,
            Rate::Fixed(_) => None,
        };
        let minimum = self.wallet.min_fee_rate().await?;
        let rate = match rate(
            self.payouts.rate,
            estimate,
            minimum,
            self.payouts.fee_rate_cap,
        ) {
            Ok(rate) => rate,
            Err(deferral) => return Ok(Err(deferral)),
        };
        let fee_rate = rate as f64 / 1000.0;
        let outputs: Vec<(String, Amount)> = payees
            .iter()
            .map(|payee| (payee.address.clone(), payee.sats()))
            .collect();
        match self.wallet.fund(&outputs, &self.change, rate).await {
            Ok(Funded { tx, fee }) => Ok(Ok(Plan {
                payees,
                tx,
                fee,
                fee_rate,
            })),
            Err(NodeError::Rpc { message, .. }) => Ok(Err(Deferral::Unfunded(message))),
            Err(e) => Err(e),
        }
    }

    fn defer(&self, deferral: Deferral) {
        tracing::warn!("payout batch deferred: {deferral}");
        metrics::set_payouts_deferred(true);
        self.standing
            .send_replace(Standing::Deferred(deferral.to_string()));
    }

    /// Records that payouts run: nothing is due, or a batch was reserved.
    fn run_on(&self) {
        metrics::set_payouts_deferred(false);
        self.standing.send_replace(Standing::Running);
    }
}

/// The rate a batch pays, in sat/kvB: `fixed` on regtest, or the node's `estimate` on mainnet, raised to the
/// node's `minimum` either way. The batch is deferred when mainnet has no estimate, or the rate is above `cap`
/// sat/vB.
fn rate(rate: Rate, estimate: Option<u64>, minimum: u64, cap: f64) -> Result<u64, Deferral> {
    let chosen = match rate {
        Rate::Fixed(sat_per_vb) => sat_per_kvb(sat_per_vb),
        Rate::Estimate(target) => estimate.ok_or(Deferral::NoEstimate { target })?,
    }
    .max(minimum);
    let sat_per_vb = chosen as f64 / 1000.0;
    if sat_per_vb > cap {
        return Err(Deferral::FeeRate {
            rate: sat_per_vb,
            cap,
        });
    }
    Ok(chosen)
}

/// A fee rate in sat/vB as whole sat/kvB, rounded up: the node takes a rate to three decimals.
fn sat_per_kvb(sat_per_vb: f64) -> u64 {
    (sat_per_vb * 1000.0 - 1e-6).ceil() as u64
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        BlockHash, Network, PubkeyHash, ScriptBuf, Sequence, TxIn, TxOut, Txid, Witness,
        absolute::LockTime, hashes::Hash, transaction::Version,
    };

    use super::*;
    use crate::{
        mining::pool::tests::id,
        pool::blocks::{self, FoundBlock},
        store::DATABASE,
        stratum::Transport,
    };

    const A: &str = "bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s";
    const B: &str = "bcrt1q3jeqwzg70pfkc9k4pvynlmfjlrrghp0cd705hx";
    const C: &str = "bcrt1q9heskcpee3fhxgm82a5gwqfswqrcfzwg3uqe2a";

    fn store(name: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-payouts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join(DATABASE)).unwrap()
    }

    /// Credits `address` with an available balance in `pool`, as a released allocation of a block of its own
    /// leaves it.
    fn available(t: &rusqlite::Transaction, address: &str, pool: &str, sats: i64) {
        let n: u8 = t
            .query_row("SELECT COUNT(*) + 1 FROM blocks", [], |row| row.get(0))
            .unwrap();
        let hash = BlockHash::from_byte_array([n; 32]);
        let block = FoundBlock {
            hash,
            height: 100,
            pool: id(pool),
            worker: "rig".into(),
            transport: Transport::V1,
            handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
            finder_sats: Amount::ZERO,
            pool_sats: Amount::from_sat(sats as u64),
            hex: String::new(),
        };
        blocks::insert(t, &block, 0).unwrap();
        t.execute(
            "INSERT INTO snapshots (block, pool, cutoff, first, boundary, size, total, amount) \
             VALUES (?1, ?2, 1, 1, '1', '1', '1', ?3)",
            params![hash.to_string(), pool, sats],
        )
        .unwrap();
        t.execute(
            "INSERT INTO allocations (block, pool, address, sats) VALUES (?1, ?2, ?3, ?4)",
            params![hash.to_string(), pool, address, sats],
        )
        .unwrap();
        ledger::settle(t, hash, ledger::Stage::Available).unwrap();
    }

    fn credit(t: &rusqlite::Transaction, sats: i64) {
        t.execute(
            "INSERT INTO ledger (kind, sats) VALUES ('credit', ?1)",
            [sats],
        )
        .unwrap();
    }

    /// A transaction spending output `vout` of a made-up transaction.
    fn spending(vout: u32) -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([7; 32]), vout),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        }
    }

    fn balances(t: &rusqlite::Transaction) -> Vec<(String, String, i64, i64, i64)> {
        t.prepare(
            "SELECT address, pool, available, reserved, paid FROM balances ORDER BY address, pool",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    #[tokio::test]
    async fn a_batch_pays_whole_balances_that_reach_the_threshold_in_address_order_and_no_dust() {
        let store = store("due");
        let legacy =
            Address::p2pkh(PubkeyHash::from_byte_array([1; 20]), Network::Regtest).to_string();
        let due = store
            .commit(move |t| {
                // Pooled and hybrid reach the threshold together; neither does alone.
                available(t, B, "pooled", 60_000);
                available(t, B, "hybrid", 40_000);
                available(t, A, "pooled", 99_999);
                available(t, C, "pooled", 250_000);
                // Above a 400-sat threshold, but dust to a P2PKH address.
                available(t, &legacy, "pooled", 500);
                Ok((
                    due(t, Amount::from_sat(100_000), 100)?,
                    due(t, Amount::from_sat(100_000), 1)?,
                    due(t, Amount::from_sat(400), 100)?,
                ))
            })
            .await
            .unwrap();
        let b = Payee {
            address: B.into(),
            balances: vec![(id("hybrid"), 40_000), (id("pooled"), 60_000)],
        };
        let c = Payee {
            address: C.into(),
            balances: vec![(id("pooled"), 250_000)],
        };
        assert_eq!(due.0, [b.clone(), c.clone()], "A's balance carries forward");
        assert_eq!(b.sats(), Amount::from_sat(100_000));
        assert_eq!(
            due.1,
            std::slice::from_ref(&b),
            "the outputs per batch, in address order"
        );
        let a = Payee {
            address: A.into(),
            balances: vec![(id("pooled"), 99_999)],
        };
        assert_eq!(due.2, [b, a, c], "the legacy address's 500 sats are dust");
    }

    #[tokio::test]
    async fn a_batch_reserves_its_balances_fee_and_inputs_at_once_or_nothing() {
        let store = store("reserve");
        let (short, untouched, reserved, changed) = store
            .commit(|t| {
                available(t, A, "pooled", 150_000);
                available(t, A, "hybrid", 50_000);
                credit(t, 1_000);
                let plan = Plan {
                    payees: due(t, Amount::from_sat(100_000), 100)?,
                    tx: spending(0),
                    fee: Amount::from_sat(1_500),
                    fee_rate: 2.0,
                };
                let short = reserve(t, &plan)?;
                let untouched = balances(t);
                credit(t, 500);
                let reserved = reserve(t, &plan)?;
                // The same balances are reserved once.
                Ok((short, untouched, reserved, reserve(t, &plan)?))
            })
            .await
            .unwrap();
        assert_eq!(
            short,
            Err(Deferral::Reserve {
                fee: Amount::from_sat(1_500),
                free: Amount::from_sat(1_000)
            })
        );
        assert_eq!(
            untouched,
            [
                (A.into(), "hybrid".into(), 50_000, 0, 0),
                (A.into(), "pooled".into(), 150_000, 0, 0)
            ],
            "the short reserve left the balances as they were"
        );
        assert_eq!(reserved, Ok(1));
        assert_eq!(changed, Err(Deferral::Changed));
        let respent = store.commit(|t| {
            available(t, B, "pooled", 1);
            credit(t, 1);
            let plan = Plan {
                payees: due(t, Amount::from_sat(1), 100)?,
                tx: spending(0),
                fee: Amount::from_sat(1),
                fee_rate: 2.0,
            };
            reserve(t, &plan)
        });
        assert!(respent.await.is_err(), "the input is the first batch's");
        let (balances, batches, inputs, free) = store
            .commit(|t| {
                let count = |sql| t.query_row(sql, [], |row| row.get::<_, i64>(0));
                Ok((
                    balances(t),
                    count("SELECT COUNT(*) FROM batches WHERE state = 'reserved'")?,
                    count("SELECT COUNT(*) FROM batch_inputs WHERE batch = 1")?,
                    count(FREE)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            balances,
            [
                (A.into(), "hybrid".into(), 0, 50_000, 0),
                (A.into(), "pooled".into(), 0, 150_000, 0)
            ]
        );
        assert_eq!((batches, inputs, free), (1, 1, 0));
    }

    async fn state(store: &Store) -> (String, Option<String>) {
        store
            .commit(|t| {
                t.query_row("SELECT state, txid FROM batches WHERE id = 1", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_batch_moves_forward_only_and_confirming_it_pays_once() {
        let store = store("confirm");
        store
            .commit(|t| {
                available(t, A, "pooled", 200_000);
                credit(t, 5_000);
                let plan = Plan {
                    payees: due(t, Amount::from_sat(100_000), 100)?,
                    tx: spending(1),
                    fee: Amount::from_sat(2_000),
                    fee_rate: 2.0,
                };
                reserve(t, &plan).map(|reserved| assert_eq!(reserved, Ok(1)))
            })
            .await
            .unwrap();
        assert!(
            !store.commit(|t| confirm(t, 1)).await.unwrap(),
            "not signed"
        );
        let signed = spending(2);
        let txid = signed.compute_txid().to_string();
        store.commit(move |t| sign(t, 1, &signed)).await.unwrap();
        store.commit(|t| sign(t, 1, &spending(3))).await.unwrap();
        assert_eq!(
            state(&store).await,
            ("signed".into(), Some(txid.clone())),
            "signed once"
        );
        let [batch] = &store.commit(unsettled).await.unwrap()[..] else {
            panic!("one batch")
        };
        assert_eq!(
            (batch.state, batch.funded.clone()),
            (State::Signed, spending(1))
        );
        assert_eq!(batch.tx, Some(spending(2)));

        store.commit(|t| broadcast(t, 1)).await.unwrap();
        assert!(store.commit(|t| confirm(t, 1)).await.unwrap());
        assert!(!store.commit(|t| confirm(t, 1)).await.unwrap(), "once");
        store.commit(|t| broadcast(t, 1)).await.unwrap();
        assert_eq!(state(&store).await, ("confirmed".into(), Some(txid)));
        assert!(store.commit(unsettled).await.unwrap().is_empty());
        let (balances, reserve, free) = store
            .commit(|t| {
                let sum = |sql| t.query_row(sql, [], |row| row.get::<_, i64>(0));
                Ok((
                    balances(t),
                    sum(
                        "SELECT SUM(CASE kind WHEN 'credit' THEN sats WHEN 'paid' THEN -sats END) \
                         FROM ledger WHERE address IS NULL",
                    )?,
                    sum(FREE)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(balances, [(A.into(), "pooled".into(), 0, 0, 200_000)]);
        assert_eq!(
            (reserve, free),
            (3_000, 3_000),
            "the fee paid from the reserve"
        );
    }

    #[test]
    fn mainnet_pays_the_estimate_raised_to_the_minimum_and_regtest_its_fixed_rate() {
        let estimate = Rate::Estimate(6);
        assert_eq!(
            rate(estimate, Some(4_321), 1_000, 10.0),
            Ok(4_321),
            "the estimate"
        );
        assert_eq!(
            rate(estimate, Some(800), 1_000, 10.0),
            Ok(1_000),
            "raised to the minimum"
        );
        assert_eq!(
            rate(estimate, None, 1_000, 10.0),
            Err(Deferral::NoEstimate { target: 6 })
        );
        assert_eq!(
            rate(estimate, Some(12_500), 1_000, 10.0),
            Err(Deferral::FeeRate {
                rate: 12.5,
                cap: 10.0
            })
        );
        assert_eq!(
            rate(estimate, Some(800), 10_001, 10.0),
            Err(Deferral::FeeRate {
                rate: 10.001,
                cap: 10.0
            }),
            "a minimum above the cap defers too"
        );
        assert_eq!(
            rate(Rate::Fixed(2.0), None, 1_000, 10.0),
            Ok(2_000),
            "regtest"
        );
        assert_eq!(
            rate(Rate::Fixed(2.0), Some(9_000), 1_000, 10.0),
            Ok(2_000),
            "regtest ignores any estimate"
        );
        assert_eq!(rate(Rate::Fixed(0.5), None, 1_000, 10.0), Ok(1_000));
    }

    #[test]
    fn fee_rates_round_up_to_whole_sat_per_kvb() {
        assert_eq!(sat_per_kvb(2.0), 2_000);
        assert_eq!(sat_per_kvb(0.1), 100);
        assert_eq!(sat_per_kvb(1.0001), 1_001);
    }
}
