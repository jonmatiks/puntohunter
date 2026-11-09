//! The selector: over every node's watcher, it judges where each node stands and publishes the work of the
//! fittest. With one node it publishes that node's work.

use std::{sync::Arc, time::Duration};

use bitcoin::BlockHash;
use tokio::{
    sync::{Notify, watch},
    time::Instant,
};

use super::{
    BlockTemplate, NodeClient,
    health::{Outage, State},
    template::{self, Sourced, Template, View, Work},
};
use crate::config::{Network, NodeChecks};

/// A node of the fleet.
pub struct Member<N> {
    pub name: String,
    pub client: Arc<N>,
}

/// The running fleet.
pub struct Fleet<N> {
    /// In config order, the primary first.
    pub members: Arc<[Member<N>]>,
    /// The work of the selected node, or none.
    pub work: watch::Receiver<Work>,
    pub status: watch::Receiver<Status>,
    /// The primary's own state, as the health check reads it with one node.
    pub primary_state: watch::Receiver<State>,
}

/// Where every node stands, and which one sources the work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// In config order, the primary first.
    pub nodes: Vec<NodeStatus>,
    /// The node the current work comes from.
    pub source: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    pub standing: Standing,
    /// Its tip's height; None before its first answer.
    pub height: Option<u32>,
    /// Blocks its tip is behind the fleet's best.
    pub lag: u32,
    /// How long its last health check took.
    pub latency: Option<Duration>,
}

/// Where a node stands, as the selector judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Free of faults.
    Fit,
    // Soft faults, which keep a node from sourcing templates only while another node is fit.
    /// Out of a fault, and not yet through its recovery window.
    Recovering,
    /// Behind the fleet's best tip for longer than the stuck window.
    Stuck,
    /// Without peers.
    Isolated,
    // Hard faults, which always keep a node from sourcing templates.
    /// No answer or no template yet.
    Starting,
    Ibd,
    Down(Outage),
    /// On another chain, for the life of the process.
    Quarantined,
}

impl Standing {
    pub fn hard(self) -> bool {
        matches!(
            self,
            Standing::Starting | Standing::Ibd | Standing::Down(_) | Standing::Quarantined
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Standing::Fit => "fit",
            Standing::Recovering => "recovering",
            Standing::Stuck => "stuck",
            Standing::Isolated => "isolated",
            Standing::Starting => "starting",
            Standing::Ibd => "ibd",
            Standing::Down(Outage::Refused) => "refused",
            Standing::Down(Outage::Hung) => "hung",
            Standing::Down(Outage::Failed) => "failed",
            Standing::Quarantined => "quarantined",
        }
    }
}

/// Starts a watcher for each of `nodes`, the primary first, and the selector over them. It runs while any
/// receiver of the work is alive.
pub fn spawn<N: NodeClient>(
    nodes: Vec<(String, Arc<N>)>,
    network: Network,
    checks: NodeChecks,
) -> Fleet<N> {
    assert!(!nodes.is_empty(), "a fleet has a primary");
    let changed = Arc::new(Notify::new());
    let mut members = Vec::with_capacity(nodes.len());
    let mut views = Vec::with_capacity(nodes.len());
    let mut states = Vec::with_capacity(nodes.len());
    for (name, client) in nodes {
        let (view, state) = template::watch(name.clone(), client.clone(), network, changed.clone());
        views.push(view);
        states.push(state);
        members.push(Member { name, client });
    }
    let starting = NodeStatus {
        standing: Standing::Starting,
        height: None,
        lag: 0,
        latency: None,
    };
    let (work, work_receiver) = watch::channel(Work::None);
    let (status, status_receiver) = watch::channel(Status {
        nodes: vec![starting; members.len()],
        source: None,
    });
    let selector = Selector {
        names: members.iter().map(|member| member.name.clone()).collect(),
        books: vec![Book::default(); views.len()],
        views,
        checks,
        changed,
        work,
        status,
        seq: 0,
        prevhash: None,
        source: None,
        published: None,
        disagreement: Vec::new(),
    };
    tokio::spawn(selector.run());
    Fleet {
        members: members.into(),
        work: work_receiver,
        status: status_receiver,
        primary_state: states.swap_remove(0),
    }
}

/// What the selector remembers of a node between rounds.
#[derive(Debug, Default, Clone)]
struct Book {
    /// Whether the node has been fit or sourced templates: its first admission waits for no window.
    admitted: bool,
    /// Whether the node was free of every fault but recovery at the last round.
    clear: bool,
    /// The end of its recovery window, while it is in one.
    recovering_until: Option<Instant>,
    /// Since when its tip has had less work than the fleet's best.
    behind_since: Option<Instant>,
    /// The standing last logged.
    logged: Option<Standing>,
}

struct Selector {
    names: Vec<String>,
    views: Vec<watch::Receiver<View>>,
    books: Vec<Book>,
    checks: NodeChecks,
    /// Notified on every change to a view.
    changed: Arc<Notify>,
    work: watch::Sender<Work>,
    status: watch::Sender<Status>,
    seq: u64,
    /// The prevhash of the last template published, kept through no work so work resuming on the same tip is
    /// not clean.
    prevhash: Option<BlockHash>,
    source: Option<usize>,
    /// The template last published, to tell a new one from the same.
    published: Option<Arc<BlockTemplate>>,
    /// The tips last seen disagreeing on equal work, so a fork is logged once.
    disagreement: Vec<BlockHash>,
}

impl Selector {
    async fn run(mut self) {
        loop {
            let deadline = self.select(Instant::now());
            let expiry = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = self.changed.notified() => {}
                _ = expiry => {}
                _ = self.work.closed() => return,
            }
        }
    }

    /// Judges every node at `now` and publishes the work of the one selected. Returns when a window next
    /// ends, if one is running.
    fn select(&mut self, now: Instant) -> Option<Instant> {
        let views: Vec<View> = self
            .views
            .iter()
            .map(|view| view.borrow().clone())
            .collect();
        let mut deadline: Option<Instant> = None;
        let mut wake = |at: Instant| deadline = Some(deadline.map_or(at, |d| d.min(at)));

        // The fleet's best tip, among the nodes answering on the right chain.
        let healthy = |view: &&View| view.state == State::Healthy;
        let best = views
            .iter()
            .filter(healthy)
            .filter_map(|view| view.tip)
            .max_by_key(|tip| (tip.chainwork, tip.height));
        self.log_disagreement(&views, best.map(|tip| tip.chainwork));

        let mut standings = Vec::with_capacity(views.len());
        let mut nodes = Vec::with_capacity(views.len());
        for (view, book) in views.iter().zip(&mut self.books) {
            let behind = match (view.tip, best) {
                (Some(tip), Some(best)) if view.state == State::Healthy => {
                    (tip.chainwork < best.chainwork).then(|| best.height.saturating_sub(tip.height))
                }
                _ => None,
            };
            if behind.is_some() {
                let since = *book.behind_since.get_or_insert(now);
                if now < since + self.checks.stuck {
                    wake(since + self.checks.stuck);
                }
            } else {
                book.behind_since = None;
            }
            let stuck = book
                .behind_since
                .is_some_and(|since| now >= since + self.checks.stuck);
            let hard = match &view.state {
                State::Healthy if view.template.is_some() => None,
                State::Healthy | State::Starting => Some(Standing::Starting),
                State::InIbd => Some(Standing::Ibd),
                State::Outage(outage) => Some(Standing::Down(*outage)),
                State::WrongChain { .. } => Some(Standing::Quarantined),
            };
            let isolated = view.peers == Some(0);
            let clear = hard.is_none() && !stuck && !isolated;
            if clear && !book.clear && book.admitted {
                book.recovering_until = Some(now + self.checks.recovery);
            }
            if !clear {
                book.recovering_until = None;
            }
            book.clear = clear;
            let standing = match hard {
                Some(hard) => hard,
                None if stuck => Standing::Stuck,
                None if isolated => Standing::Isolated,
                None => match book.recovering_until {
                    Some(until) if now < until => {
                        wake(until);
                        Standing::Recovering
                    }
                    _ => {
                        book.recovering_until = None;
                        book.admitted = true;
                        Standing::Fit
                    }
                },
            };
            standings.push(standing);
            nodes.push(NodeStatus {
                standing,
                height: view.tip.map(|tip| tip.height),
                lag: behind.unwrap_or(0),
                latency: view.latency,
            });
        }
        for (i, &standing) in standings.iter().enumerate() {
            self.log_standing(i, standing);
        }

        // Soft faults count only while another node has none.
        let fit = standings.contains(&Standing::Fit);
        let candidates: Vec<usize> = (0..views.len())
            .filter(|&i| !standings[i].hard() && (!fit || standings[i] == Standing::Fit))
            .collect();
        let template = |i: usize| {
            views[i]
                .template
                .as_ref()
                .expect("a candidate has a template")
        };
        // The tip most nodes share counts each node without a hard fault whose template builds on it.
        let shared = |prevhash: BlockHash| {
            (0..views.len())
                .filter(|&j| !standings[j].hard())
                .filter(|&j| template(j).block.previousblockhash == prevhash)
                .count()
        };
        let key = |i: usize| {
            let sourced = template(i);
            (
                sourced.chainwork,
                shared(sourced.block.previousblockhash),
                i == 0,
            )
        };
        let selected = candidates.iter().map(|&i| key(i)).max().map(|top| {
            // The current source stays while nothing ranks above it; latency only picks a new one.
            match self.source {
                Some(source) if candidates.contains(&source) && key(source) == top => source,
                _ => candidates
                    .iter()
                    .copied()
                    .filter(|&i| key(i) == top)
                    .min_by_key(|&i| views[i].latency.unwrap_or(Duration::MAX))
                    .expect("the top key is a candidate's"),
            }
        });
        match selected {
            Some(i) => {
                // Taken with no fit node to defer to, a recovering node is readmitted: its window is over.
                if standings[i] == Standing::Recovering {
                    self.books[i].recovering_until = None;
                    nodes[i].standing = Standing::Fit;
                    self.log_standing(i, Standing::Fit);
                }
                self.books[i].admitted = true;
                let sourced = template(i).clone();
                let same = self.source == Some(i)
                    && self
                        .published
                        .as_ref()
                        .is_some_and(|published| Arc::ptr_eq(published, &sourced.block));
                if !same {
                    self.publish(i, sourced);
                }
            }
            None => self.publish_none(),
        }
        for (i, (node, view)) in nodes.iter().zip(&views).enumerate() {
            crate::metrics::set_node_figures(crate::metrics::NodeFigures {
                node: &self.names[i],
                standing: node.standing.label(),
                height: node.height,
                lag: node.lag,
                chainwork_log2: view.tip.map(|tip| tip.chainwork.log2()),
                rpc: node.latency,
                selected: self.source == Some(i),
            });
        }
        let status = Status {
            nodes,
            source: self.source,
        };
        self.status.send_if_modified(|current| {
            let changed = *current != status;
            *current = status;
            changed
        });
        deadline
    }

    fn publish(&mut self, source: usize, sourced: Sourced) {
        let block = sourced.block;
        let node = &self.names[source];
        if self.source != Some(source) && self.names.len() > 1 {
            let from = self.source.map_or("none", |from| self.names[from].as_str());
            tracing::info!(from, to = %node, "template source changed");
            if self.source.is_some() {
                crate::metrics::node_source_changed(from, node);
            }
        }
        let clean = self.prevhash != Some(block.previousblockhash);
        if clean {
            tracing::info!(
                node = %node,
                height = block.height,
                prevhash = %block.previousblockhash,
                transactions = block.transactions.len(),
                "new template, clean"
            );
        } else {
            tracing::debug!(
                node = %node,
                height = block.height,
                transactions = block.transactions.len(),
                "template refresh"
            );
        }
        self.seq += 1;
        self.prevhash = Some(block.previousblockhash);
        self.source = Some(source);
        self.published = Some(block.clone());
        crate::metrics::set_tip_height(block.height.saturating_sub(1));
        self.work.send_replace(Work::Template(Arc::new(Template {
            seq: self.seq,
            clean,
            block,
        })));
    }

    fn publish_none(&mut self) {
        if self.source.take().is_some() && self.names.len() > 1 {
            tracing::warn!("no node may source templates; issuing no work");
        }
        self.published = None;
        self.work.send_if_modified(|work| {
            let changed = matches!(work, Work::Template(_));
            *work = Work::None;
            changed
        });
    }

    /// Logs a node's move to a soft fault, or back to fit from one. Hard faults are logged by its health.
    fn log_standing(&mut self, i: usize, standing: Standing) {
        let book = &mut self.books[i];
        let previous = book.logged.replace(standing);
        if previous == Some(standing) {
            return;
        }
        let node = &self.names[i];
        match standing {
            Standing::Stuck => tracing::warn!(
                node,
                stuck = ?self.checks.stuck,
                "node stuck: its tip has had less work than the fleet's best for the stuck window"
            ),
            Standing::Isolated => tracing::warn!(node, "node has no peers"),
            Standing::Recovering => tracing::info!(
                node,
                recovery = ?self.checks.recovery,
                "node recovering: it may source templates again after its recovery window"
            ),
            Standing::Fit if previous.is_some_and(|p| !p.hard()) => {
                tracing::info!(node, "node fit")
            }
            _ => {}
        }
    }

    /// Logs once each set of tips that disagree at the fleet's greatest work: a real fork, or a node on a
    /// minority chain. Work follows the majority, then the primary.
    fn log_disagreement(&mut self, views: &[View], best: Option<bitcoin::Work>) {
        let mut tips: Vec<BlockHash> = views
            .iter()
            .filter(|view| view.state == State::Healthy)
            .filter_map(|view| view.tip)
            .filter(|tip| Some(tip.chainwork) == best)
            .map(|tip| tip.hash)
            .collect();
        tips.sort();
        tips.dedup();
        if tips.len() < 2 {
            self.disagreement.clear();
        } else if tips != self.disagreement {
            tracing::warn!(
                tips = ?tips,
                "nodes disagree on the tip at equal work; work follows the majority, then the primary"
            );
            self.disagreement = tips;
        }
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash;

    use super::*;
    use crate::node::{NodeError, fake::FakeNode};

    const STUCK: Duration = Duration::from_secs(30);
    const RECOVERY: Duration = Duration::from_secs(60);
    const RPC_TIMEOUT: Duration = Duration::from_secs(10);

    fn hash(n: u8) -> BlockHash {
        BlockHash::from_byte_array([n; 32])
    }

    fn chainwork(n: u64) -> bitcoin::Work {
        let mut bytes = [0; 32];
        bytes[24..].copy_from_slice(&n.to_be_bytes());
        bitcoin::Work::from_be_bytes(bytes)
    }

    struct Harness {
        nodes: Vec<Arc<FakeNode>>,
        fleet: Fleet<FakeNode>,
    }

    /// `n` nodes with peers on the recorded regtest tip, the primary first.
    fn fleet(n: usize) -> Harness {
        let nodes: Vec<_> = (0..n)
            .map(|_| {
                let node = Arc::new(FakeNode::regtest());
                node.set_peers(8);
                node
            })
            .collect();
        let fleet = spawn(
            nodes
                .iter()
                .enumerate()
                .map(|(i, node)| (["primary", "b", "c"][i].to_owned(), node.clone()))
                .collect(),
            Network::Regtest,
            NodeChecks {
                stuck: STUCK,
                recovery: RECOVERY,
            },
        );
        Harness { nodes, fleet }
    }

    impl Harness {
        /// Waits until the status and work satisfy `wanted`, and returns them.
        async fn until(
            &mut self,
            what: &str,
            wanted: impl Fn(&Status, &Work) -> bool,
        ) -> (Status, Work) {
            let wait = async {
                loop {
                    {
                        let status = self.fleet.status.borrow_and_update().clone();
                        let work = self.fleet.work.borrow().clone();
                        if wanted(&status, &work) {
                            return (status, work);
                        }
                    }
                    tokio::select! {
                        _ = self.fleet.status.changed() => {}
                        _ = self.fleet.work.changed() => {}
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(600), wait)
                .await
                .unwrap_or_else(|_| panic!("not {what} within 600s"))
        }

        /// Waits until node `source` sources the work, and returns its template.
        async fn sourced_by(&mut self, source: usize) -> Arc<Template> {
            let (_, work) = self
                .until(&format!("sourced by {source}"), |status, work| {
                    status.source == Some(source) && matches!(work, Work::Template(_))
                })
                .await;
            match work {
                Work::Template(template) => template,
                Work::None => unreachable!(),
            }
        }

        fn source(&self) -> Option<usize> {
            self.fleet.status.borrow().source
        }

        fn standing(&self, i: usize) -> Standing {
            self.fleet.status.borrow().nodes[i].standing
        }
    }

    #[tokio::test(start_paused = true)]
    async fn starts_on_the_primary_and_fails_over_on_a_refused_call_within_a_health_round() {
        let mut h = fleet(2);
        let start = Instant::now();
        let first = h.sourced_by(0).await;
        assert!(first.clean);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "no window at startup"
        );

        let failed = Instant::now();
        h.nodes[0].set_failure(Some(NodeError::Refused("down".into())));
        let template = h.sourced_by(1).await;
        assert!(failed.elapsed() <= template::HEALTH_INTERVAL);
        assert!(
            !template.clean,
            "another node on the same tip leaves jobs valid"
        );
        assert!(template.seq > first.seq);
        assert_eq!(h.standing(0), Standing::Down(Outage::Refused));
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_primary_hands_over_within_its_rpc_timeout() {
        let mut h = fleet(2);
        h.sourced_by(0).await;

        let hung = Instant::now();
        h.nodes[0].set_delay(RPC_TIMEOUT);
        h.nodes[0].set_failure(Some(NodeError::Timeout(RPC_TIMEOUT)));
        h.sourced_by(1).await;
        let elapsed = hung.elapsed();
        assert!(
            elapsed <= RPC_TIMEOUT + template::HEALTH_INTERVAL,
            "{elapsed:?}"
        );
        assert_eq!(h.standing(0), Standing::Down(Outage::Hung));
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_tip_is_served_from_the_first_node_with_it() {
        let mut h = fleet(2);
        let first = h.sourced_by(0).await;

        let found = Instant::now();
        h.nodes[1].set_tip(hash(2), 102, chainwork(300));
        let template = h.sourced_by(1).await;
        assert_eq!(found.elapsed(), Duration::ZERO, "served at once");
        assert!(template.clean);
        assert_eq!(template.block.previousblockhash, hash(2));

        // The primary catches up within the stuck window and takes sourcing back, the jobs still good.
        tokio::time::sleep(Duration::from_secs(2)).await;
        h.nodes[0].set_tip(hash(2), 102, chainwork(300));
        let back = h.sourced_by(0).await;
        assert!(!back.clean);
        assert!(back.seq > template.seq && template.seq > first.seq);
    }

    #[tokio::test(start_paused = true)]
    async fn the_taller_tip_with_less_work_loses() {
        let mut h = fleet(2);
        h.sourced_by(0).await;
        h.nodes[0].set_tip(hash(3), 103, chainwork(250));
        h.nodes[1].set_tip(hash(4), 102, chainwork(300));
        let template = h.sourced_by(1).await;
        assert_eq!(template.block.previousblockhash, hash(4));

        h.until("the taller primary stuck", |status, _| {
            status.nodes[0].standing == Standing::Stuck
        })
        .await;
        assert_eq!(h.source(), Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn equal_work_goes_to_the_majority_then_the_primary() {
        let mut h = fleet(3);
        h.sourced_by(0).await;
        h.nodes[0].set_tip(hash(5), 102, chainwork(300));
        h.nodes[1].set_tip(hash(6), 102, chainwork(300));
        h.nodes[2].set_tip(hash(6), 102, chainwork(300));
        h.until(
            "on the majority's tip",
            |_, work| matches!(work, Work::Template(t) if t.block.previousblockhash == hash(6)),
        )
        .await;
        assert_ne!(h.source(), Some(0));

        let mut h = fleet(2);
        h.sourced_by(0).await;
        h.nodes[1].set_tip(hash(6), 102, chainwork(300));
        h.sourced_by(1).await;
        h.nodes[0].set_tip(hash(5), 102, chainwork(300));
        let template = h.sourced_by(0).await;
        assert_eq!(
            template.block.previousblockhash,
            hash(5),
            "no majority: the primary's"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn among_equals_the_lowest_latency_is_taken() {
        let mut h = fleet(3);
        h.sourced_by(0).await;
        h.nodes[1].set_delay(Duration::from_millis(80));
        tokio::time::sleep(Duration::from_secs(2)).await;
        h.nodes[0].set_failure(Some(NodeError::Refused("down".into())));
        h.sourced_by(2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_stuck_node_recovers_only_after_its_window() {
        let mut h = fleet(2);
        h.sourced_by(0).await;
        h.nodes[1].set_tip(hash(2), 102, chainwork(300));
        h.sourced_by(1).await;
        h.until("the primary stuck", |status, _| {
            status.nodes[0].standing == Standing::Stuck && status.nodes[0].lag == 1
        })
        .await;

        let caught_up = Instant::now();
        h.nodes[0].set_tip(hash(2), 102, chainwork(300));
        h.until("the primary recovering", |status, _| {
            status.nodes[0].standing == Standing::Recovering
        })
        .await;
        assert_eq!(h.source(), Some(1), "equal work, but the primary waits");
        h.sourced_by(0).await;
        let waited = caught_up.elapsed();
        assert!(
            waited >= RECOVERY && waited <= RECOVERY + template::HEALTH_INTERVAL,
            "{waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_node_without_peers_is_passed_over_only_while_another_is_fit() {
        let mut h = fleet(2);
        h.sourced_by(0).await;
        h.nodes[0].set_peers(0);
        h.sourced_by(1).await;
        assert_eq!(h.standing(0), Standing::Isolated);

        h.nodes[1].set_failure(Some(NodeError::Refused("down".into())));
        let template = h.sourced_by(0).await;
        assert!(!template.clean);
    }

    #[tokio::test(start_paused = true)]
    async fn a_recovering_node_is_taken_at_once_when_no_other_is_up() {
        let mut h = fleet(2);
        h.sourced_by(0).await;
        h.nodes[0].set_failure(Some(NodeError::Refused("down".into())));
        h.sourced_by(1).await;
        h.nodes[1].set_failure(Some(NodeError::Refused("down".into())));
        h.until("no work", |status, work| {
            status.source.is_none() && matches!(work, Work::None)
        })
        .await;

        let back = Instant::now();
        h.nodes[0].set_failure(None);
        h.sourced_by(0).await;
        assert!(back.elapsed() <= template::HEALTH_INTERVAL * 2);
        assert_eq!(h.standing(0), Standing::Fit, "no other node to defer to");
    }

    #[tokio::test(start_paused = true)]
    async fn a_node_on_the_wrong_chain_never_sources_templates() {
        let wrong = Arc::new(FakeNode::regtest());
        let mut info = wrong.info();
        info.chain = "main".into();
        wrong.set_info(info);
        wrong.set_tip(hash(9), 500, chainwork(10_000));
        wrong.set_peers(8);
        let primary = Arc::new(FakeNode::regtest());
        primary.set_peers(8);
        let mut h = Harness {
            fleet: spawn(
                vec![
                    ("primary".to_owned(), primary.clone()),
                    ("b".to_owned(), wrong.clone()),
                ],
                Network::Regtest,
                NodeChecks {
                    stuck: STUCK,
                    recovery: RECOVERY,
                },
            ),
            nodes: vec![primary, wrong],
        };
        h.sourced_by(0).await;
        h.until("b quarantined", |status, _| {
            status.nodes[1].standing == Standing::Quarantined
        })
        .await;

        h.nodes[0].set_failure(Some(NodeError::Refused("down".into())));
        h.until("no work", |_, work| matches!(work, Work::None))
            .await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(matches!(*h.fleet.work.borrow(), Work::None));
        assert_eq!(h.standing(1), Standing::Quarantined);
    }
}
