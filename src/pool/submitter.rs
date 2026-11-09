//! The submitter: a found block reaches every node of the fleet at once, its hex is written to disk while
//! submission proceeds, and it is recorded once, at the first success. A block no node answered is
//! resubmitted until one does, from disk after a restart.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use bitcoin::{Amount, BlockHash};
use tokio::sync::{Notify, mpsc, watch};

use super::{
    blocks::{self, FoundBlock},
    ledger::{self, Stage},
    pplns,
};
use crate::{
    node::{
        NodeClient, NodeError,
        fleet::{Member, Standing, Status},
    },
    store::{Store, backup},
    telemetry::round::Round,
};

/// How often unanswered blocks are resubmitted while any remain.
const RETRY: Duration = Duration::from_secs(1);

/// The node's answer to a submission, as the submitter reads it.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// Accepted, or already known: null, duplicate, and inconclusive are all success.
    Success(String),
    /// A genuine reject.
    Reject(String),
}

/// Reads a submitblock outcome. An error that is not an answer, such as a refused connection or a timeout,
/// is returned as it is: the block is resubmitted.
fn read(outcome: Result<Option<String>, NodeError>) -> Result<Answer, NodeError> {
    match outcome {
        Ok(None) => Ok(Answer::Success("accepted".into())),
        Ok(Some(reason))
            if matches!(
                reason.as_str(),
                "duplicate" | "inconclusive" | "duplicate-inconclusive"
            ) =>
        {
            Ok(Answer::Success(reason))
        }
        Ok(Some(reason)) => Ok(Answer::Reject(reason)),
        Err(NodeError::Rpc { code, message }) => {
            Ok(Answer::Reject(format!("RPC error {code}: {message}")))
        }
        Err(e) => Err(e),
    }
}

/// One node's answer to a submission, as it arrives: the node, its outcome, and how long it took.
type Answered = (usize, Result<Option<String>, NodeError>, Duration);

/// Starts the submitter, sending each block to every node of `nodes` that `status` does not show
/// quarantined, and writing its hex to `dir`. Each answer recorded notifies `answered`, and each block a node
/// accepted starts the next `round`. It runs while any sender of the returned channel is alive.
pub fn spawn<N: NodeClient>(
    nodes: Arc<[Member<N>]>,
    status: watch::Receiver<Status>,
    store: Store,
    dir: PathBuf,
    answered: Arc<Notify>,
    round: Arc<Round>,
) -> mpsc::UnboundedSender<FoundBlock> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let submitter = Submitter {
        nodes,
        status,
        store,
        dir,
        answered,
        round,
        pending: Vec::new(),
    };
    tokio::spawn(submitter.run(receiver));
    sender
}

struct Submitter<N> {
    nodes: Arc<[Member<N>]>,
    status: watch::Receiver<Status>,
    store: Store,
    dir: PathBuf,
    answered: Arc<Notify>,
    round: Arc<Round>,
    /// Blocks awaiting an answer, oldest first.
    pending: Vec<Pending>,
}

struct Pending {
    hash: BlockHash,
    hex: Arc<str>,
}

impl<N: NodeClient> Submitter<N> {
    async fn run(mut self, mut found: mpsc::UnboundedReceiver<FoundBlock>) {
        self.load_unanswered().await;
        let mut retry = tokio::time::interval(RETRY);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                block = found.recv() => match block {
                    Some(block) => self.submit(block).await,
                    None => return,
                },
                _ = retry.tick(), if !self.pending.is_empty() => self.resubmit().await,
            }
        }
    }

    async fn submit(&mut self, block: FoundBlock) {
        tracing::info!(hash = %block.hash, height = block.height, pool = %block.pool, "submitting block");
        let hex: Arc<str> = block.hex.as_str().into();
        let start = Instant::now();
        // In flight to every node before the block is written and recorded.
        let answers = self.fan_out(&hex);
        self.persist(&block).await;
        match self.settle(block.hash, answers, false).await {
            Some(answer) => {
                if matches!(answer, Answer::Success(_)) {
                    crate::metrics::block_submitted(start.elapsed().as_secs_f64());
                }
                self.record(block.hash, answer).await;
            }
            None => {
                tracing::warn!(
                    hash = %block.hash,
                    "block submission unanswered by every node; resubmitting until one answers"
                );
                self.pending.push(Pending {
                    hash: block.hash,
                    hex,
                });
            }
        }
    }

    /// Sends `hex` to every node not quarantined, each call in a task of its own, and returns their answers
    /// as they arrive.
    fn fan_out(&self, hex: &Arc<str>) -> mpsc::UnboundedReceiver<Answered> {
        let quarantined: Vec<bool> = self
            .status
            .borrow()
            .nodes
            .iter()
            .map(|node| node.standing == Standing::Quarantined)
            .collect();
        let (sender, receiver) = mpsc::unbounded_channel();
        for (i, member) in self.nodes.iter().enumerate() {
            if quarantined.get(i) == Some(&true) {
                continue;
            }
            let (client, hex, sender) = (member.client.clone(), hex.clone(), sender.clone());
            tokio::spawn(async move {
                let start = Instant::now();
                let outcome = client.submitblock(&hex).await;
                let _ = sender.send((i, outcome, start.elapsed()));
            });
        }
        receiver
    }

    /// Reads the answers as they arrive, and returns at the first success, leaving the rest to be logged as
    /// they come. Without one, waits for every node: a reject if any rejected, or None if none answered.
    async fn settle(
        &self,
        hash: BlockHash,
        mut answers: mpsc::UnboundedReceiver<Answered>,
        resubmission: bool,
    ) -> Option<Answer> {
        let mut reject = None;
        while let Some((i, outcome, latency)) = answers.recv().await {
            let answer = read(outcome);
            log_answer(&self.nodes[i].name, hash, &answer, latency, resubmission);
            match answer {
                Ok(Answer::Success(result)) => {
                    let nodes = self.nodes.clone();
                    tokio::spawn(async move {
                        while let Some((i, outcome, latency)) = answers.recv().await {
                            log_answer(&nodes[i].name, hash, &read(outcome), latency, resubmission);
                        }
                    });
                    return Some(Answer::Success(result));
                }
                Ok(Answer::Reject(reason)) => {
                    reject.get_or_insert(reason);
                }
                Err(_) => {}
            }
        }
        reject.map(Answer::Reject)
    }

    /// Writes the block's hex to disk and records it, each failure logged: neither holds up submission.
    async fn persist(&self, block: &FoundBlock) {
        let path = self.path(block.hash);
        let hex = block.hex.clone();
        let write = tokio::task::spawn_blocking(move || write_atomically(&path, &hex));
        let found_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let row = FoundBlock {
            hex: String::new(),
            ..block.clone()
        };
        let record = self
            .store
            .commit(move |t| blocks::insert(t, &row, found_at));
        let (written, recorded) = tokio::join!(write, record);
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(hash = %block.hash, "cannot write the block's hex: {e}"),
            Err(e) => tracing::error!(hash = %block.hash, "cannot write the block's hex: {e}"),
        }
        if let Err(e) = recorded {
            tracing::error!(hash = %block.hash, "cannot record the block: {e}");
            return;
        }
        // A block with a pool output whose solution share was journaled first is allocated now, in a commit
        // of its own, so a failure here never costs the block's record.
        if block.pool_sats > Amount::ZERO {
            match self.store.commit(pplns::allocate_ready).await {
                Ok(allocated) if !allocated.is_empty() => backup::now(),
                Ok(_) => {}
                Err(e) => tracing::error!(hash = %block.hash, "cannot allocate the block: {e}"),
            }
        }
    }

    /// Resubmits the unanswered blocks, stopping at the first that is still unanswered.
    async fn resubmit(&mut self) {
        let mut queue = std::mem::take(&mut self.pending).into_iter();
        while let Some(pending) = queue.next() {
            let answers = self.fan_out(&pending.hex);
            match self.settle(pending.hash, answers, true).await {
                Some(answer) => {
                    tracing::info!(hash = %pending.hash, "block resubmitted");
                    self.record(pending.hash, answer).await;
                }
                None => {
                    tracing::debug!(hash = %pending.hash, "block resubmission unanswered");
                    self.pending.push(pending);
                    self.pending.extend(queue);
                    return;
                }
            }
        }
    }

    async fn record(&self, hash: BlockHash, answer: Answer) {
        // Each node's answer is logged as it arrives, a reject as critical.
        let (result, rejected) = match answer {
            Answer::Success(result) => {
                tracing::info!(%hash, %result, "block recorded as found");
                (result, false)
            }
            Answer::Reject(reason) => {
                tracing::error!(%hash, %reason, "every node that answered rejected the block");
                (reason, true)
            }
        };
        // A refused block's allocations, if it has any yet, are reversed with the refusal.
        let recorded = self
            .store
            .commit(move |t| {
                blocks::record_answer(t, hash, &result, rejected)?;
                if rejected {
                    ledger::settle(t, hash, Stage::Void)?;
                }
                blocks::placed(t, hash)
            })
            .await;
        match recorded {
            // The next round starts once the answer is recorded, so the solution share, accepted before it,
            // stays in the round it ends.
            Ok(Some((height, found_at))) if !rejected => self.round.found(height, found_at),
            Ok(_) => {}
            Err(e) => tracing::error!(%hash, "cannot record the node's answer: {e}"),
        }
        self.answered.notify_one();
    }

    /// Queues the blocks a previous run left unanswered, read back from their hex on disk.
    async fn load_unanswered(&mut self) {
        let hashes = match self.store.commit(blocks::unanswered).await {
            Ok(hashes) => hashes,
            Err(e) => {
                tracing::error!("cannot read unanswered blocks: {e}");
                return;
            }
        };
        for hash in hashes {
            match std::fs::read_to_string(self.path(hash)) {
                Ok(hex) => self.pending.push(Pending {
                    hash,
                    hex: hex.into(),
                }),
                Err(e) => tracing::error!(%hash, "cannot resubmit an unanswered block: {e}"),
            }
        }
        if !self.pending.is_empty() {
            tracing::info!(
                count = self.pending.len(),
                "resubmitting blocks the node never answered"
            );
        }
    }

    fn path(&self, hash: BlockHash) -> PathBuf {
        self.dir.join(format!("{hash}.hex"))
    }
}

/// Logs one node's answer and times it. A genuine reject is a critical incident, whatever the others answer.
fn log_answer(
    node: &str,
    hash: BlockHash,
    answer: &Result<Answer, NodeError>,
    latency: Duration,
    resubmission: bool,
) {
    let latency_ms = latency.as_secs_f64() * 1000.0;
    let label = match answer {
        Ok(Answer::Success(result)) => {
            tracing::info!(%hash, node, %result, latency_ms, "node answered the block");
            "success"
        }
        Ok(Answer::Reject(reason)) => {
            tracing::error!(
                %hash,
                node,
                %reason,
                latency_ms,
                "CRITICAL: a node rejected a block the pool found"
            );
            "reject"
        }
        Err(e) if resubmission => {
            tracing::debug!(%hash, node, latency_ms, "node did not answer the block: {e}");
            "unanswered"
        }
        Err(e) => {
            tracing::warn!(%hash, node, latency_ms, "node did not answer the block: {e}");
            "unanswered"
        }
    };
    crate::metrics::node_submission(node, label, latency.as_secs_f64());
}

/// Writes through a temporary file, so a crash never leaves a partial block for resubmission to read, and
/// syncs the file and then its directory, so the block survives a power loss as its synced row does.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let temporary = path.with_extension("hex.partial");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}

#[cfg(test)]
mod tests {
    use bitcoin::{Amount, hashes::Hash};
    use rusqlite::OptionalExtension;

    use super::*;
    use crate::{
        mining::pool::tests::id,
        node::{fake::FakeNode, fleet::NodeStatus},
        store::DATABASE,
        stratum::Transport,
    };

    struct Harness {
        /// The primary.
        node: Arc<FakeNode>,
        /// The secondaries.
        others: Vec<Arc<FakeNode>>,
        status: watch::Sender<Status>,
        store: Store,
        dir: PathBuf,
        answered: Arc<Notify>,
        round: Arc<Round>,
    }

    impl Harness {
        fn new(name: &str) -> Self {
            Harness::fleet(name, 1)
        }

        /// A harness of `n` nodes, every one fit.
        fn fleet(name: &str, n: usize) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "puntohunter-submitter-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let fit = NodeStatus {
                standing: Standing::Fit,
                height: None,
                lag: 0,
                latency: None,
            };
            Harness {
                node: Arc::new(FakeNode::regtest()),
                others: (1..n).map(|_| Arc::new(FakeNode::regtest())).collect(),
                status: watch::Sender::new(Status {
                    nodes: vec![fit; n],
                    source: Some(0),
                }),
                store: Store::open(&dir.join(DATABASE)).unwrap(),
                dir,
                answered: Arc::new(Notify::new()),
                round: Arc::new(Round::new()),
            }
        }

        fn spawn(&self) -> mpsc::UnboundedSender<FoundBlock> {
            let nodes: Vec<_> = std::iter::once(&self.node)
                .chain(&self.others)
                .enumerate()
                .map(|(i, node)| Member {
                    name: format!("node{i}"),
                    client: node.clone(),
                })
                .collect();
            spawn(
                nodes.into(),
                self.status.subscribe(),
                self.store.clone(),
                self.dir.clone(),
                self.answered.clone(),
                self.round.clone(),
            )
        }

        async fn answered(&self) {
            tokio::time::timeout(Duration::from_secs(10), self.answered.notified())
                .await
                .expect("no answer recorded within 10s");
        }

        /// The block's status and result, once it is recorded.
        async fn row(&self, hash: BlockHash) -> (String, Option<String>) {
            loop {
                let row = self
                    .store
                    .commit(move |t| {
                        t.query_row(
                            "SELECT status, result FROM blocks WHERE hash = ?1",
                            [hash.to_string()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                    })
                    .await
                    .unwrap();
                if let Some(row) = row {
                    return row;
                }
                tokio::task::yield_now().await;
            }
        }
    }

    fn block(n: u8) -> FoundBlock {
        FoundBlock {
            hash: BlockHash::from_byte_array([n; 32]),
            height: 1,
            pool: id("solo"),
            worker: "rig".into(),
            transport: Transport::V1,
            handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
            finder_sats: Amount::ZERO,
            pool_sats: Amount::ZERO,
            hex: format!("{n:02x}").repeat(100),
        }
    }

    #[test]
    fn reads_answers_as_b5_does() {
        let success = |s: &str| Ok(Answer::Success(s.into()));
        assert_eq!(read(Ok(None)), success("accepted"));
        for answer in ["duplicate", "inconclusive", "duplicate-inconclusive"] {
            assert_eq!(read(Ok(Some(answer.into()))), success(answer));
        }
        for answer in ["high-hash", "time-too-new", "duplicate-invalid", "rejected"] {
            assert_eq!(
                read(Ok(Some(answer.into()))),
                Ok(Answer::Reject(answer.into()))
            );
        }
        let decode = NodeError::Rpc {
            code: -22,
            message: "Block decode failed".into(),
        };
        assert!(matches!(read(Err(decode)), Ok(Answer::Reject(_))));
        let refused = NodeError::Refused("down".into());
        assert_eq!(read(Err(refused.clone())), Err(refused));
        let hung = NodeError::Timeout(Duration::from_secs(10));
        assert_eq!(read(Err(hung.clone())), Err(hung));
    }

    #[tokio::test]
    async fn submits_writes_and_records_a_found_block() {
        let harness = Harness::new("found");
        let blocks = harness.spawn();
        let found = block(1);
        blocks.send(found.clone()).unwrap();
        harness.answered().await;

        assert_eq!(harness.node.submitted(), std::slice::from_ref(&found.hex));
        assert_eq!(
            std::fs::read_to_string(harness.dir.join(format!("{}.hex", found.hash))).unwrap(),
            found.hex
        );
        assert_eq!(
            harness.row(found.hash).await,
            ("submitted".into(), Some("accepted".into()))
        );
    }

    #[tokio::test]
    async fn a_duplicate_is_success_and_a_genuine_reject_is_marked() {
        let harness = Harness::new("answers");
        let blocks = harness.spawn();
        harness.node.set_submit_answer(Some("duplicate"));
        blocks.send(block(1)).unwrap();
        harness.answered().await;
        assert_eq!(
            harness.row(block(1).hash).await,
            ("submitted".into(), Some("duplicate".into()))
        );
        assert_eq!(
            harness.round.start().height,
            Some(1),
            "a block a node accepted starts the next round"
        );

        harness.node.set_submit_answer(Some("time-too-new"));
        blocks
            .send(FoundBlock {
                height: 2,
                ..block(2)
            })
            .unwrap();
        harness.answered().await;
        assert_eq!(
            harness.row(block(2).hash).await,
            ("rejected".into(), Some("time-too-new".into()))
        );
        assert_eq!(
            harness.round.start().height,
            Some(1),
            "a refused block starts nothing"
        );
    }

    #[tokio::test]
    async fn resubmits_an_unanswered_block_when_the_node_returns() {
        let harness = Harness::new("resubmit");
        let blocks = harness.spawn();
        harness
            .node
            .set_failure(Some(NodeError::Timeout(Duration::from_secs(10))));
        blocks.send(block(1)).unwrap();
        // Recorded, with its hex on disk, while the node does not answer.
        assert_eq!(harness.row(block(1).hash).await, ("submitted".into(), None));
        let path = harness.dir.join(format!("{}.hex", block(1).hash));
        while !path.exists() {
            tokio::task::yield_now().await;
        }

        harness.node.set_failure(None);
        harness.answered().await;
        assert_eq!(harness.node.submitted(), [block(1).hex]);
        assert_eq!(
            harness.row(block(1).hash).await,
            ("submitted".into(), Some("accepted".into()))
        );
    }

    #[tokio::test]
    async fn resubmits_what_a_previous_run_left_unanswered() {
        // A previous run recorded the block and wrote its hex, but never heard back.
        let harness = Harness::new("restart");
        let found = block(1);
        std::fs::write(harness.dir.join(format!("{}.hex", found.hash)), &found.hex).unwrap();
        let row = found.clone();
        harness
            .store
            .commit(move |t| blocks::insert(t, &row, 0))
            .await
            .unwrap();

        // The block reached the node after all, so this run's resubmission is a duplicate.
        harness.node.set_submit_answer(Some("duplicate"));
        let _blocks = harness.spawn();
        harness.answered().await;
        assert_eq!(harness.node.submitted(), [found.hex]);
        assert_eq!(
            harness.row(found.hash).await,
            ("submitted".into(), Some("duplicate".into()))
        );
    }

    /// The block's status and result once its answer is recorded.
    async fn answer(harness: &Harness, hash: BlockHash) -> (String, Option<String>) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let row = harness.row(hash).await;
                if row.1.is_some() {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("no answer recorded within 10s")
    }

    #[tokio::test]
    async fn a_hung_node_delays_neither_the_others_nor_the_record() {
        let harness = Harness::fleet("hung", 3);
        harness.others[0].set_submit_hang(Some(Duration::from_secs(5)));
        let blocks = harness.spawn();
        let start = Instant::now();
        blocks.send(block(1)).unwrap();
        assert_eq!(
            answer(&harness, block(1).hash).await,
            ("submitted".into(), Some("accepted".into()))
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(harness.node.submitted(), [block(1).hex]);
        assert_eq!(harness.others[1].submitted(), [block(1).hex]);
    }

    #[tokio::test]
    async fn success_on_one_node_is_recorded_once_whatever_the_others_answer() {
        let harness = Harness::fleet("mixed", 3);
        harness.node.set_submit_answer(Some("duplicate"));
        harness.others[0].set_submit_answer(Some("high-hash"));
        let blocks = harness.spawn();
        blocks.send(block(1)).unwrap();
        let (status, result) = answer(&harness, block(1).hash).await;
        assert_eq!(
            status, "submitted",
            "a reject on one node leaves the block found"
        );
        assert!(
            matches!(result.as_deref(), Some("duplicate" | "accepted")),
            "{result:?}"
        );
        let rows = harness
            .store
            .commit(|t| {
                t.query_row("SELECT COUNT(*) FROM blocks", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn a_block_every_answering_node_rejects_is_rejected() {
        let harness = Harness::fleet("rejected", 3);
        harness.node.set_submit_answer(Some("bad-txns"));
        harness.others[0].set_submit_answer(Some("bad-txns"));
        harness.others[1].set_failure(Some(NodeError::Refused("down".into())));
        let blocks = harness.spawn();
        blocks.send(block(1)).unwrap();
        assert_eq!(
            answer(&harness, block(1).hash).await,
            ("rejected".into(), Some("bad-txns".into()))
        );
    }

    #[tokio::test]
    async fn a_block_no_node_answers_is_resubmitted_to_every_node() {
        let harness = Harness::fleet("unanswered", 2);
        for node in [&harness.node, &harness.others[0]] {
            node.set_failure(Some(NodeError::Refused("down".into())));
        }
        let blocks = harness.spawn();
        blocks.send(block(1)).unwrap();
        assert_eq!(harness.row(block(1).hash).await, ("submitted".into(), None));

        harness.others[0].set_failure(None);
        assert_eq!(
            answer(&harness, block(1).hash).await,
            ("submitted".into(), Some("accepted".into()))
        );
        assert_eq!(harness.others[0].submitted(), [block(1).hex]);
    }

    #[tokio::test]
    async fn a_quarantined_node_gets_nothing() {
        let harness = Harness::fleet("quarantine", 2);
        harness
            .status
            .send_modify(|status| status.nodes[1].standing = Standing::Quarantined);
        let blocks = harness.spawn();
        blocks.send(block(1)).unwrap();
        answer(&harness, block(1).hash).await;
        assert_eq!(harness.node.submitted(), [block(1).hex]);
        assert!(harness.others[0].submitted().is_empty());
    }
}
