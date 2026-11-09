//! The accepted-share journal: every pooled and hybrid share, serialized into one monotonic
//! sequence, written before the miner hears the share was accepted.
//!
//! Sessions hand each share to one writer task and await its commit. The task commits the shares that arrive
//! together in one transaction, as many as are queued up to [`MAX_BATCH`], waiting for no more, so a share
//! alone is committed at once. A proof already journaled, by any session or before a restart, is refused as a
//! duplicate, and the rest of its batch commits. A failed commit refuses every later share until the pool
//! restarts: solo mining carries on, but no pooled or hybrid share is acknowledged that is not in the journal.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use bitcoin::{Amount, BlockHash, CompactTarget, Target, hashes::Hash};
use rusqlite::{OptionalExtension, Transaction, params};
use tokio::sync::{mpsc, oneshot};

use crate::{
    config::Window,
    failpoint, metrics,
    mining::pool::PoolId,
    pool::pplns,
    store::{Store, StoreError, backup},
    telemetry::handle::Handle,
};

/// The accounting policy version: the PPLNS rules, with each share's work weighted by its job's
/// contribution. Version 1 was plain work.
pub const POLICY_VERSION: i64 = pplns::WEIGHTED;
/// The most shares one commit takes.
pub const MAX_BATCH: usize = 256;

/// An accepted pooled or hybrid share, as the journal records it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub pool: PoolId,
    /// The payout address the share's worker mines to.
    pub address: String,
    pub handle: Handle,
    /// The template sequence of the share's job, and its id on the session or channel that issued it.
    pub template: u64,
    pub job: u64,
    /// The job's network difficulty.
    pub bits: CompactTarget,
    /// The job's split: its miner portion, and the pool output paid from it.
    pub miner_sats: Amount,
    pub pool_sats: Amount,
    /// The target the share was credited at.
    pub target: Target,
    /// The header hash.
    pub proof: BlockHash,
    /// Unix seconds.
    pub accepted_at: u64,
    /// Whether the share is a block, whose window ends at its sequence.
    pub block: bool,
}

/// Why the journal did not take a share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Its proof is already journaled.
    Duplicate,
    /// A journal commit failed; nothing more is taken until the pool restarts.
    Failed,
}

type Pending = (Entry, oneshot::Sender<Result<u64, Refusal>>);

/// A handle to the writer task.
#[derive(Clone)]
pub struct Journal {
    entries: mpsc::UnboundedSender<Pending>,
    failed: Arc<AtomicBool>,
}

/// Each pool's policy row, by its id, as [`policy`] returns them.
pub type Policies = HashMap<PoolId, i64>;

impl Journal {
    /// Starts the writer task, recording each share under its pool's row in `policies`.
    pub fn spawn(store: Store, policies: Policies) -> Journal {
        let (entries, queue) = mpsc::unbounded_channel();
        let failed = Arc::new(AtomicBool::new(false));
        metrics::set_journal_failed(false);
        tokio::spawn(run(store, Arc::new(policies), queue, failed.clone()));
        Journal { entries, failed }
    }

    /// Journals `entry`, returning its sequence once its commit is written.
    pub async fn record(&self, entry: Entry) -> Result<u64, Refusal> {
        if self.failed() {
            return Err(Refusal::Failed);
        }
        let (reply, answer) = oneshot::channel();
        if self.entries.send((entry, reply)).is_err() {
            return Err(Refusal::Failed);
        }
        answer.await.unwrap_or(Err(Refusal::Failed))
    }

    /// Whether a commit has failed, so every share is refused.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

/// The id of the policy row for `window` under [`POLICY_VERSION`], added when a pool first runs it. Pools with
/// one policy share its row.
pub async fn policy(store: &Store, window: Window) -> Result<i64, StoreError> {
    store
        .commit(move |t| {
            let row = params![
                window.window_multiplier,
                window.window_min_work,
                POLICY_VERSION
            ];
            t.execute(
                "INSERT OR IGNORE INTO policies (window_multiplier, window_min_work, version) \
                 VALUES (?1, ?2, ?3)",
                row,
            )?;
            t.query_row(
                "SELECT id FROM policies \
                 WHERE window_multiplier = ?1 AND window_min_work = ?2 AND version = ?3",
                row,
                |r| r.get(0),
            )
        })
        .await
}

async fn run(
    store: Store,
    policies: Arc<Policies>,
    mut queue: mpsc::UnboundedReceiver<Pending>,
    failed: Arc<AtomicBool>,
) {
    while let Some(first) = queue.recv().await {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match queue.try_recv() {
                Ok(pending) => batch.push(pending),
                Err(_) => break,
            }
        }
        let (entries, replies): (Vec<Entry>, Vec<_>) = batch.into_iter().unzip();
        let blocks = entries.iter().any(|entry| entry.block);
        let results = if failed.load(Ordering::Relaxed) {
            vec![Err(Refusal::Failed); entries.len()]
        } else {
            commit(&store, &policies, entries, &failed).await
        };
        let journaled = results.iter().any(Result::is_ok);
        for (reply, result) in replies.into_iter().zip(results) {
            let _ = reply.send(result);
        }
        // A solution share is journaled: allocate its block in a commit of its own, unless the submitter has
        // not recorded the block yet, when its own commit does. Nothing waits on it.
        if blocks && journaled {
            let store = store.clone();
            tokio::spawn(async move {
                match store.commit(pplns::allocate_ready).await {
                    Ok(allocated) if !allocated.is_empty() => backup::now(),
                    Ok(_) => {}
                    Err(e) => tracing::error!("cannot allocate a block: {e}"),
                }
            });
        }
    }
}

/// Commits `entries` in one transaction: each one's sequence, or its refusal.
async fn commit(
    store: &Store,
    policies: &Arc<Policies>,
    entries: Vec<Entry>,
    failed: &AtomicBool,
) -> Vec<Result<u64, Refusal>> {
    let count = entries.len();
    let start = Instant::now();
    let result = if failpoint::fires(failpoint::JOURNAL_COMMIT_FAILS) {
        Err(format!("failpoint {}", failpoint::JOURNAL_COMMIT_FAILS))
    } else {
        let policies = policies.clone();
        store
            .journal(move |t| insert(t, &policies, &entries))
            .await
            .map_err(|e| e.to_string())
    };
    metrics::journal_committed(start.elapsed().as_secs_f64());
    match result {
        Ok(sequences) => {
            failpoint::exit_if(failpoint::AFTER_JOURNAL_COMMIT);
            sequences
                .into_iter()
                .map(|sequence| sequence.ok_or(Refusal::Duplicate))
                .collect()
        }
        Err(e) => {
            failed.store(true, Ordering::Relaxed);
            metrics::set_journal_failed(true);
            tracing::error!(
                "CRITICAL: a journal commit failed, so pooled and hybrid shares are refused until the pool \
                 restarts: {e}"
            );
            vec![Err(Refusal::Failed); count]
        }
    }
}

/// Inserts `entries` in order, returning each one's sequence, or None for a proof already journaled. Sequences
/// rise in insertion order, but a refused insert may spend one, so they need not be consecutive. A block's
/// window cutoff is its sequence: set here if the submitter recorded the block first, or by the submitter.
fn insert(
    t: &Transaction,
    policies: &Policies,
    entries: &[Entry],
) -> rusqlite::Result<Vec<Option<u64>>> {
    let mut statement = t.prepare_cached(
        "INSERT INTO journal \
         (pool, address, handle, policy, template, job, bits, miner_sats, pool_sats, target, proof, \
         accepted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
         ON CONFLICT (proof) DO NOTHING RETURNING sequence",
    )?;
    entries
        .iter()
        .map(|entry| {
            let sequence: Option<i64> = statement
                .query_row(
                    params![
                        entry.pool,
                        entry.address,
                        entry.handle.to_string(),
                        policies[&entry.pool],
                        entry.template as i64,
                        entry.job as i64,
                        entry.bits.to_consensus(),
                        entry.miner_sats.to_sat() as i64,
                        entry.pool_sats.to_sat() as i64,
                        &entry.target.to_be_bytes()[..],
                        &entry.proof.to_byte_array()[..],
                        entry.accepted_at as i64,
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(sequence) = sequence
                && entry.block
            {
                t.execute(
                    "UPDATE blocks SET cutoff = ?1 WHERE hash = ?2",
                    params![sequence, entry.proof.to_string()],
                )?;
            }
            Ok(sequence.map(|sequence| sequence as u64))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::{
        mining::pool::tests::id,
        pool::blocks::{self, FoundBlock},
        store::DATABASE,
        stratum::Transport,
    };

    const ADDRESS: &str = "bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s";

    fn path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-journal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(DATABASE)
    }

    const WINDOW: Window = Window {
        window_multiplier: 2.0,
        window_min_work: 1e6,
    };

    /// A journal whose pooled and hybrid pools share one policy.
    async fn open(path: &Path) -> (Store, Journal) {
        let store = Store::open(path).unwrap();
        let policy = policy(&store, WINDOW).await.unwrap();
        let policies = [(id("pooled"), policy), (id("hybrid"), policy)].into();
        let journal = Journal::spawn(store.clone(), policies);
        (store, journal)
    }

    fn entry(n: u8) -> Entry {
        Entry {
            pool: id(if n.is_multiple_of(2) {
                "pooled"
            } else {
                "hybrid"
            }),
            address: ADDRESS.into(),
            handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
            template: 1,
            job: u64::from(n),
            bits: CompactTarget::from_consensus(0x207f_ffff),
            miner_sats: Amount::from_sat(1_000),
            pool_sats: Amount::from_sat(600),
            target: Target::MAX,
            proof: BlockHash::from_byte_array([n; 32]),
            accepted_at: 1_700_000_000,
            block: false,
        }
    }

    /// The journal's (sequence, proof's first byte), in sequence order.
    async fn rows(store: &Store) -> Vec<(u64, u8)> {
        store
            .commit(|t| {
                let mut statement =
                    t.prepare("SELECT sequence, proof FROM journal ORDER BY sequence")?;
                statement
                    .query_map([], |row| {
                        Ok((row.get::<_, i64>(0)? as u64, row.get::<_, Vec<u8>>(1)?[0]))
                    })?
                    .collect()
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_batch_commits_in_arrival_order_refusing_a_duplicate_alone() {
        let (store, _) = open(&path("batch")).await;
        let failed = AtomicBool::new(false);
        let entries = vec![entry(3), entry(1), entry(3), entry(2)];
        let policies = Arc::new([(id("pooled"), 1), (id("hybrid"), 1)].into());
        let results = commit(&store, &policies, entries, &failed).await;
        assert_eq!(results[..3], [Ok(1), Ok(2), Err(Refusal::Duplicate)]);
        // The refused insert may spend a sequence number: sequences rise, but need not be consecutive.
        let Ok(last) = results[3] else {
            panic!("{results:?}")
        };
        assert!(last > 2, "the rest committed after it");
        assert_eq!(rows(&store).await, [(1, 3), (2, 1), (last, 2)]);
        assert!(!failed.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn shares_queued_together_take_their_arrival_order() {
        let (store, journal) = open(&path("queued")).await;
        // join_all polls each future in order, so each share is queued in order before any commit returns.
        let results =
            futures_util::future::join_all((10..20).map(|n| journal.record(entry(n)))).await;
        let sequences: Vec<u64> = results.into_iter().map(Result::unwrap).collect();
        assert!(sequences.windows(2).all(|w| w[0] < w[1]), "{sequences:?}");
        let proofs: Vec<u8> = rows(&store).await.into_iter().map(|(_, p)| p).collect();
        assert_eq!(proofs, (10..20).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn the_sequence_keeps_rising_across_restarts_and_a_proof_stays_refused() {
        let path = path("restart");
        let (store, journal) = open(&path).await;
        assert_eq!(journal.record(entry(1)).await, Ok(1));
        assert_eq!(journal.record(entry(2)).await, Ok(2));
        // Deleted, as compaction would: its sequence is still never reused.
        store
            .commit(|t| t.execute("DELETE FROM journal WHERE sequence = 2", []))
            .await
            .unwrap();
        drop((store, journal));

        let (_store, journal) = open(&path).await;
        assert_eq!(journal.record(entry(3)).await, Ok(3));
        assert_eq!(
            journal.record(entry(1)).await,
            Err(Refusal::Duplicate),
            "journaled before the restart"
        );
    }

    #[tokio::test]
    async fn a_failed_commit_refuses_every_later_share() {
        let (store, journal) = open(&path("failed")).await;
        assert_eq!(journal.record(entry(1)).await, Ok(1));
        store
            .commit(|t| t.execute_batch("ALTER TABLE journal RENAME TO gone"))
            .await
            .unwrap();
        assert_eq!(journal.record(entry(2)).await, Err(Refusal::Failed));
        assert!(journal.failed());
        store
            .commit(|t| t.execute_batch("ALTER TABLE gone RENAME TO journal"))
            .await
            .unwrap();
        assert_eq!(
            journal.record(entry(3)).await,
            Err(Refusal::Failed),
            "until the pool restarts"
        );
        assert_eq!(rows(&store).await, [(1, 1)]);
    }

    fn found(n: u8) -> FoundBlock {
        FoundBlock {
            hash: BlockHash::from_byte_array([n; 32]),
            height: 1,
            pool: id("pooled"),
            worker: "rig".into(),
            transport: Transport::V1,
            handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
            finder_sats: bitcoin::Amount::ZERO,
            pool_sats: bitcoin::Amount::from_sat(1),
            hex: String::new(),
        }
    }

    async fn cutoff(store: &Store, n: u8) -> Option<i64> {
        let hash = BlockHash::from_byte_array([n; 32]).to_string();
        store
            .commit(move |t| {
                t.query_row("SELECT cutoff FROM blocks WHERE hash = ?1", [hash], |row| {
                    row.get(0)
                })
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_block_records_its_solution_shares_sequence_whichever_is_written_first() {
        let (store, journal) = open(&path("cutoff")).await;
        assert_eq!(journal.record(entry(9)).await, Ok(1));

        // The block recorded first, then its share journaled.
        let block = found(5);
        store
            .commit(move |t| blocks::insert(t, &block, 0))
            .await
            .unwrap();
        assert_eq!(cutoff(&store, 5).await, None);
        let share = Entry {
            block: true,
            ..entry(5)
        };
        assert_eq!(journal.record(share).await, Ok(2));
        assert_eq!(cutoff(&store, 5).await, Some(2));

        // The share journaled first, then its block recorded.
        let share = Entry {
            block: true,
            ..entry(6)
        };
        assert_eq!(journal.record(share).await, Ok(3));
        let block = found(6);
        store
            .commit(move |t| blocks::insert(t, &block, 0))
            .await
            .unwrap();
        assert_eq!(cutoff(&store, 6).await, Some(3));

        // A solo block has no journal row, so no cutoff.
        let block = FoundBlock {
            pool: id("solo"),
            ..found(7)
        };
        store
            .commit(move |t| blocks::insert(t, &block, 0))
            .await
            .unwrap();
        assert_eq!(cutoff(&store, 7).await, None);
    }

    #[tokio::test]
    async fn each_policy_is_recorded_once() {
        let store = Store::open(&path("policy")).unwrap();
        let first = policy(&store, WINDOW).await.unwrap();
        assert_eq!(policy(&store, WINDOW).await.unwrap(), first);
        let other = policy(
            &store,
            Window {
                window_multiplier: 3.0,
                ..WINDOW
            },
        )
        .await
        .unwrap();
        assert_ne!(other, first);
    }

    #[tokio::test]
    async fn each_share_is_recorded_under_its_pools_policy() {
        let path = path("pools");
        let store = Store::open(&path).unwrap();
        let shared = policy(&store, WINDOW).await.unwrap();
        let own = policy(
            &store,
            Window {
                window_multiplier: 5.0,
                ..WINDOW
            },
        )
        .await
        .unwrap();
        let policies = [
            (id("pooled"), shared),
            (id("hybrid"), shared),
            (id("bonus"), own),
        ]
        .into();
        let journal = Journal::spawn(store.clone(), policies);
        for (n, pool) in [(1, "pooled"), (2, "bonus"), (3, "hybrid")] {
            let share = Entry {
                pool: id(pool),
                ..entry(n)
            };
            journal.record(share).await.unwrap();
        }
        let recorded: Vec<(String, i64)> = store
            .commit(|t| {
                t.prepare("SELECT pool, policy FROM journal ORDER BY sequence")?
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect()
            })
            .await
            .unwrap();
        assert_eq!(
            recorded,
            [
                ("pooled".to_owned(), shared),
                ("bonus".to_owned(), own),
                ("hybrid".to_owned(), shared)
            ]
        );
    }
}
