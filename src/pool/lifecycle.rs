//! Block lifecycle: each block's status follows the chain the miners mine on, reconciled through getblockheader
//! to the node the work comes from, on each tip change and after each answer the submitter records.
//! Solo blocks need only the status; nothing is paid. A pooled or hybrid block's allocations are settled
//! in the ledger in the same commit as its status, and released at maturity plus the buffer.

use std::sync::Arc;

use bitcoin::BlockHash;
use tokio::sync::{Notify, watch};

use super::{
    blocks::{self, Status},
    ledger::{self, Stage},
};
use crate::{
    metrics,
    node::{
        NodeClient,
        fleet::{self, Member},
        template::Work,
    },
    store::Store,
};

/// Blocks after a coinbase before its outputs may be spent (consensus's COINBASE_MATURITY). Like Bitcoin
/// Core's wallet, a coinbase counts as mature once the block holding it has more confirmations than this.
pub const COINBASE_MATURITY: i64 = 100;

/// Blocks further than this below the tip are no longer reconciled. A reorg that deep is outside what one
/// node plans for, and the bound keeps a long regtest run, a block every few seconds, from asking about
/// thousands of blocks on every tip change.
const DEPTH: u32 = 2 * COINBASE_MATURITY as u32;

/// The status a block with `confirmations` on the node's active chain has; -1 means off it.
pub fn status(confirmations: i64) -> Status {
    if confirmations < 1 {
        Status::Orphaned
    } else if confirmations <= COINBASE_MATURITY {
        Status::ConfirmedImmature
    } else {
        Status::Mature
    }
}

/// Starts the task, asking whichever of `nodes` `status` shows sourcing the work. It runs while the fleet
/// does. Allocations become available `buffer` blocks after their coinbase matures.
pub fn spawn<N: NodeClient>(
    nodes: Arc<[Member<N>]>,
    status: watch::Receiver<fleet::Status>,
    store: Store,
    work: watch::Receiver<Work>,
    answered: Arc<Notify>,
    buffer: u32,
) {
    tokio::spawn(run(nodes, status, store, work, answered, buffer));
}

/// The node the work comes from, or the primary while none is selected.
fn selected<'a, N>(nodes: &'a [Member<N>], status: &watch::Receiver<fleet::Status>) -> &'a N {
    let source = status.borrow().source.unwrap_or(0);
    &nodes[source].client
}

async fn run<N: NodeClient>(
    nodes: Arc<[Member<N>]>,
    status: watch::Receiver<fleet::Status>,
    store: Store,
    mut work: watch::Receiver<Work>,
    answered: Arc<Notify>,
    buffer: u32,
) {
    // The prevhash and height of the tip the last template built on.
    let mut tip: Option<(BlockHash, u32)> = None;
    loop {
        let reconcile = tokio::select! {
            changed = work.changed() => {
                if changed.is_err() {
                    return;
                }
                match &*work.borrow_and_update() {
                    Work::Template(template)
                        if tip.map(|(hash, _)| hash) != Some(template.block.previousblockhash) =>
                    {
                        tip = Some((
                            template.block.previousblockhash,
                            template.block.height.saturating_sub(1),
                        ));
                        true
                    }
                    _ => false,
                }
            }
            _ = answered.notified() => tip.is_some(),
        };
        if let (true, Some((_, height))) = (reconcile, tip) {
            self::reconcile(selected(&nodes, &status), &store, height, buffer).await;
        }
    }
}

/// Brings each tracked block's status, and its allocations' stage, in line with the node's chain at
/// `tip_height`.
async fn reconcile(node: &impl NodeClient, store: &Store, tip_height: u32, buffer: u32) {
    let min_height = tip_height.saturating_sub(DEPTH);
    let tracked = match store.commit(move |t| blocks::tracked(t, min_height)).await {
        Ok(tracked) => tracked,
        Err(e) => {
            tracing::error!("cannot read the blocks to reconcile: {e}");
            return;
        }
    };
    for block in tracked {
        let (hash, current) = (block.hash, block.status);
        let confirmations = match node.getblockheader(hash).await {
            Ok(Some(header)) => header.confirmations,
            // The node never saw it, as after a regtest chain was started afresh: leave it be.
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!("block reconciliation stopped until the next tip change: {e}");
                return;
            }
        };
        let next = status(confirmations);
        let stage = Stage::of(next, confirmations, buffer);
        // Maturity plus the buffer comes with no change of status unless the buffer is 0.
        if next == current && !(block.unreleased && stage == Stage::Available) {
            continue;
        }
        let settled = store.commit(move |t| {
            blocks::set_status(t, hash, next)?;
            ledger::settle(t, hash, stage)
        });
        match settled.await {
            Ok(settled) => {
                if next != current {
                    tracing::info!(
                        %hash,
                        from = %current.as_str(),
                        to = %next.as_str(),
                        "block status changed"
                    );
                }
                if settled.paused {
                    metrics::set_payouts_paused(true);
                }
            }
            Err(e) => tracing::error!(%hash, "cannot record the block's status: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{Amount, hashes::Hash};

    use super::*;
    use crate::{
        mining::pool::tests::id,
        node::{BlockHeaderInfo, fake::FakeNode},
        pool::blocks::FoundBlock,
        store::DATABASE,
        stratum::Transport,
    };

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!(
            "puntohunter-lifecycle-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join(DATABASE)).unwrap()
    }

    fn hash(n: u8) -> BlockHash {
        BlockHash::from_byte_array([n; 32])
    }

    /// Block `n` of `pool`, found at height 100 + `n`, and the node's acceptance of it.
    fn accepted(t: &rusqlite::Transaction, n: u8, pool: &str) -> rusqlite::Result<()> {
        let block = FoundBlock {
            hash: hash(n),
            height: 100 + u32::from(n),
            pool: id(pool),
            worker: "rig".into(),
            transport: Transport::V1,
            handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
            finder_sats: Amount::ZERO,
            pool_sats: Amount::ZERO,
            hex: String::new(),
        };
        blocks::insert(t, &block, 0)?;
        blocks::record_answer(t, block.hash, "accepted", false)
    }

    fn header(confirmations: i64) -> Option<BlockHeaderInfo> {
        Some(BlockHeaderInfo {
            height: 0,
            confirmations,
            chainwork: bitcoin::Work::from_be_bytes([0; 32]),
        })
    }

    #[tokio::test]
    async fn reconciles_statuses_with_the_node() {
        let store = store("statuses");
        let node = FakeNode::regtest();
        store
            .commit(move |t| (1..=4).try_for_each(|n| accepted(t, n, "solo")))
            .await
            .unwrap();
        node.set_header(hash(1), header(101));
        node.set_header(hash(2), header(1));
        node.set_header(hash(3), header(-1));

        reconcile(&node, &store, 105, 0).await;
        let mut statuses: Vec<_> = store
            .commit(|t| blocks::tracked(t, 0))
            .await
            .unwrap()
            .into_iter()
            .map(|block| (block.hash, block.status))
            .collect();
        statuses.sort_by_key(|(hash, _)| *hash);
        assert_eq!(
            statuses,
            [
                (hash(1), Status::Mature),
                (hash(2), Status::ConfirmedImmature),
                (hash(3), Status::Orphaned),
                (hash(4), Status::Submitted),
            ],
            "the node does not know block 4"
        );
    }

    #[tokio::test]
    async fn releases_allocations_at_maturity_plus_the_buffer_once_however_deep() {
        let store = store("release");
        let node = FakeNode::regtest();
        store
            .commit(|t| {
                accepted(t, 1, "pooled")?;
                t.execute(
                    "INSERT INTO snapshots (block, pool, cutoff, first, boundary, size, total, amount) \
                     VALUES (?1, 'pooled', 1, 1, '1', '1', '1', 1000)",
                    [hash(1).to_string()],
                )?;
                t.execute(
                    "INSERT INTO allocations (block, pool, address, sats) \
                     VALUES (?1, 'pooled', 'bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s', 1000)",
                    [hash(1).to_string()],
                )?;
                ledger::settle(t, hash(1), Stage::Pending)
            })
            .await
            .unwrap();
        let entries = || {
            store.commit(|t| {
                t.prepare("SELECT kind FROM ledger ORDER BY id")?
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
        };

        // Mature, and the buffer of 2 not yet past; then past, with the tip far beyond the depth followed.
        node.set_header(hash(1), header(102));
        reconcile(&node, &store, 202, 2).await;
        assert_eq!(entries().await.unwrap(), ["pending"]);
        node.set_header(hash(1), header(103));
        for tip in [1_000, 1_001] {
            reconcile(&node, &store, tip, 2).await;
            assert_eq!(entries().await.unwrap(), ["pending", "available"]);
        }
        let tracked = store.commit(|t| blocks::tracked(t, 0)).await.unwrap();
        assert_eq!(
            tracked,
            [blocks::Tracked {
                hash: hash(1),
                status: Status::Mature,
                unreleased: false,
            }]
        );
        assert!(
            store
                .commit(|t| blocks::tracked(t, 900))
                .await
                .unwrap()
                .is_empty(),
            "released, it is followed only to the depth"
        );
    }

    #[test]
    fn asks_the_node_the_work_comes_from() {
        let nodes: Arc<[Member<FakeNode>]> = ["primary", "b"]
            .into_iter()
            .map(|name| Member {
                name: name.into(),
                client: Arc::new(FakeNode::regtest()),
            })
            .collect();
        let status = watch::Sender::new(fleet::Status {
            nodes: Vec::new(),
            source: None,
        });
        let receiver = status.subscribe();
        assert!(std::ptr::eq(selected(&nodes, &receiver), &*nodes[0].client));
        status.send_modify(|status| status.source = Some(1));
        assert!(std::ptr::eq(selected(&nodes, &receiver), &*nodes[1].client));
    }

    #[test]
    fn status_follows_confirmations() {
        assert_eq!(status(-1), Status::Orphaned);
        assert_eq!(status(1), Status::ConfirmedImmature);
        assert_eq!(status(100), Status::ConfirmedImmature, "99 blocks on top");
        assert_eq!(status(101), Status::Mature, "100 blocks on top");
    }
}
