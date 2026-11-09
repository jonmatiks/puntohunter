//! The node watcher: one per node, and the only caller of its node. It keeps the node's health, tip, peers,
//! and latest template live on a watch channel whether or not the node is selected, so failover is a pointer
//! swap. The selector (`fleet.rs`) publishes the one current work from the watchers' templates.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use bitcoin::BlockHash;
use tokio::{
    sync::{Notify, watch},
    time::{Instant, MissedTickBehavior},
};

use super::{BlockTemplate, NodeClient, NodeError, NodeHealth, fleet, health::State};
use crate::config::{Network, NodeChecks, PRIMARY};

/// How long an unusable node waits before the next template call, unless a new block wakes it first.
const RETRY: Duration = Duration::from_secs(1);

/// How often each node's health is checked.
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(1);

/// BIP320's version-rolling bits, 13 to 28.
pub const BIP320_MASK: i32 = 0x1fff_e000;

#[derive(Debug, Clone)]
pub enum Work {
    /// Nothing may be mined: no node may source templates.
    None,
    Template(Arc<Template>),
}

#[derive(Debug)]
pub struct Template {
    /// Counts the templates published, from whichever node, so jobs can be cached per template.
    pub seq: u64,
    /// A new prevhash: every earlier job is stale. Work resuming after an outage, or from another node, on the
    /// same tip is not clean, since nothing has shown the jobs on that tip stale.
    pub clean: bool,
    /// The node's template, its version normalized (`normalized_version`).
    pub block: Arc<BlockTemplate>,
}

/// Clears the in-mask bits vbrequired does not set, so a V1 client reading BIP310 as OR and one reading it
/// as XOR build the same header.
pub fn normalized_version(version: i32, vbrequired: u32) -> i32 {
    version & !(BIP320_MASK & !(vbrequired as i32))
}

/// Starts a fleet of one, `node`, and returns its work and the node's state as they change. It runs
/// while any receiver of the work is alive.
pub fn spawn<N: NodeClient>(
    node: Arc<N>,
    network: Network,
) -> (watch::Receiver<Work>, watch::Receiver<State>) {
    // A lone node has no other to defer to, so neither window ever applies.
    let checks = NodeChecks {
        stuck: Duration::from_secs(30),
        recovery: Duration::from_secs(60),
    };
    let fleet = fleet::spawn(vec![(PRIMARY.to_owned(), node)], network, checks);
    (fleet.work, fleet.primary_state)
}

/// A node as its watcher last saw it.
#[derive(Debug, Clone)]
pub struct View {
    pub state: State,
    /// The tip of the last getblockchaininfo answer.
    pub tip: Option<Tip>,
    /// Peers connected, 0 while P2P is off; None before the first answer.
    pub peers: Option<u32>,
    /// How long the last getblockchaininfo took.
    pub latency: Option<Duration>,
    /// The latest template, while the node is healthy.
    pub template: Option<Sourced>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tip {
    pub hash: BlockHash,
    pub height: u32,
    pub chainwork: bitcoin::Work,
}

/// A template, with the work of the chain it builds on, which the selector selects by.
#[derive(Debug, Clone)]
pub struct Sourced {
    pub block: Arc<BlockTemplate>,
    pub chainwork: bitcoin::Work,
}

/// Starts the watcher of node `name`, and returns its view and its state as they change. Each change to the
/// view notifies `changed`. It runs while any receiver of the view is alive.
pub(super) fn watch<N: NodeClient>(
    name: String,
    node: Arc<N>,
    network: Network,
    changed: Arc<Notify>,
) -> (watch::Receiver<View>, watch::Receiver<State>) {
    let health = NodeHealth::new(name.clone(), network);
    let state = health.watch();
    let (view, receiver) = watch::channel(View {
        state: State::Starting,
        tip: None,
        peers: None,
        latency: None,
        template: None,
    });
    let watcher = Arc::new(Watcher {
        name,
        node,
        health: Mutex::new(health),
        view,
        best: watch::Sender::new(None),
        changed,
    });
    tokio::spawn(watcher.clone().check_health());
    tokio::spawn(watcher.poll_templates());
    (receiver, state)
}

struct Watcher<N> {
    name: String,
    node: Arc<N>,
    health: Mutex<NodeHealth>,
    /// Read by the selector alone, so it closes when the selector stops.
    view: watch::Sender<View>,
    /// The tip of the last getblockchaininfo answer, which wakes the template loop when the long poll and ZMQ
    /// both missed a block.
    best: watch::Sender<Option<BlockHash>>,
    changed: Arc<Notify>,
}

impl<N: NodeClient> Watcher<N> {
    async fn check_health(self: Arc<Self>) {
        let mut interval = tokio::time::interval(HEALTH_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        while !self.view.is_closed() {
            interval.tick().await;
            self.check().await;
        }
    }

    /// Calls getblockchaininfo, then getnetworkinfo, and publishes what they show.
    async fn check(&self) {
        let start = Instant::now();
        let info = self.node.getblockchaininfo().await;
        let latency = start.elapsed();
        let network = match &info {
            Ok(_) => Some(self.node.getnetworkinfo().await),
            Err(_) => None,
        };
        let state = {
            let mut health = self.health.lock().unwrap();
            match &network {
                Some(Err(e)) => health.observe_call::<()>(&Err(e.clone())),
                _ => health.observe_info(&info),
            };
            health.state().clone()
        };
        let healthy = state == State::Healthy;
        self.view.send_modify(|view| {
            view.state = state;
            if let Ok(info) = &info {
                view.tip = Some(Tip {
                    hash: info.bestblockhash,
                    height: info.blocks,
                    chainwork: info.chainwork,
                });
                view.latency = Some(latency);
            }
            if let Some(Ok(network)) = &network {
                view.peers = Some(if network.networkactive {
                    network.connections
                } else {
                    0
                });
            }
            if !healthy {
                view.template = None;
            }
        });
        if let Ok(info) = &info {
            self.best.send_if_modified(|best| {
                let changed = *best != Some(info.bestblockhash);
                *best = Some(info.bestblockhash);
                changed
            });
        }
        self.changed.notify_one();
    }

    async fn poll_templates(self: Arc<Self>) {
        let mut tips = self.node.tip_events();
        let mut best = self.best.subscribe();
        let mut state = self.health.lock().unwrap().watch();
        let mut longpollid: Option<String> = None;
        // The prevhash of the last template, kept through an outage.
        let mut prevhash: Option<BlockHash> = None;
        while !self.view.is_closed() {
            if *state.borrow_and_update() != State::Healthy {
                longpollid = None;
                tokio::select! {
                    _ = state.changed() => {}
                    _ = tokio::time::sleep(RETRY) => {}
                }
                continue;
            }

            let node = self.node.clone();
            let polled = longpollid.clone();
            let result = tokio::select! {
                result = node.getblocktemplate(polled.as_deref()) => result,
                // A node that hangs or fails mid-poll may answer that poll only at its next block once back,
                // so the poll is dropped, and a fresh call made once the node is healthy again.
                () = unhealthy(&mut state) => continue,
                hash = new_tip(&mut tips, prevhash) => {
                    tracing::debug!(node = %self.name, %hash, "hashblock before the long poll returned");
                    node.getblocktemplate(None).await
                }
                hash = new_tip(&mut best, prevhash) => {
                    tracing::debug!(node = %self.name, %hash, "new tip before the long poll returned");
                    node.getblocktemplate(None).await
                }
            };
            let result = match result {
                Ok(block) => self.source(block).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(sourced) => {
                    longpollid = Some(sourced.block.longpollid.clone());
                    prevhash = Some(sourced.block.previousblockhash);
                    self.view.send_if_modified(|view| {
                        // A template that arrives as the node falls ill is not kept.
                        let healthy = view.state == State::Healthy;
                        if healthy {
                            view.template = Some(sourced);
                        }
                        healthy
                    });
                    self.changed.notify_one();
                }
                // A long poll can outlast its timeout on a quiet chain; a health check tells whether the node is
                // hung before the next one.
                Err(NodeError::Timeout(_)) if polled.is_some() => self.check().await,
                // Mainnet's getblocktemplate refuses while the node has no peers. The health check's peer count
                // shows that once, a soft fault; as an outage it would flap with every check. A node shutting
                // down answers -9 too, which the next check's getblockchaininfo shows.
                Err(NodeError::Rpc { code: -9, .. }) => {
                    longpollid = None;
                    tokio::select! {
                        _ = tokio::time::sleep(RETRY) => {}
                        _ = new_tip(&mut tips, None) => {}
                    }
                }
                Err(e) => {
                    self.fail(e);
                    longpollid = None;
                    tokio::select! {
                        _ = tokio::time::sleep(RETRY) => {}
                        _ = new_tip(&mut tips, None) => {}
                    }
                }
            }
        }
    }

    /// The template, normalized, with the work of the chain it builds on: known from the tip or the last
    /// template, or else asked of the node, since a template can arrive before the health check sees its tip.
    async fn source(&self, mut block: BlockTemplate) -> Result<Sourced, NodeError> {
        block.version = normalized_version(block.version, block.vbrequired);
        let known = {
            let view = self.view.borrow();
            let tip = view
                .tip
                .filter(|tip| tip.hash == block.previousblockhash)
                .map(|tip| tip.chainwork);
            tip.or_else(|| {
                view.template
                    .as_ref()
                    .filter(|t| t.block.previousblockhash == block.previousblockhash)
                    .map(|t| t.chainwork)
            })
        };
        let chainwork = match known {
            Some(chainwork) => chainwork,
            None => match self.node.getblockheader(block.previousblockhash).await? {
                Some(header) => header.chainwork,
                None => {
                    return Err(NodeError::Protocol(format!(
                        "the template builds on {}, which the node does not know",
                        block.previousblockhash
                    )));
                }
            },
        };
        tracing::debug!(
            node = %self.name,
            height = block.height,
            transactions = block.transactions.len(),
            "template"
        );
        Ok(Sourced {
            block: Arc::new(block),
            chainwork,
        })
    }

    /// Records a failed call: the node is out until a health check shows it back.
    fn fail(&self, e: NodeError) {
        let state = {
            let mut health = self.health.lock().unwrap();
            health.observe_call::<()>(&Err(e));
            health.state().clone()
        };
        self.view.send_modify(|view| {
            view.state = state;
            view.template = None;
        });
        self.changed.notify_one();
    }
}

/// Waits until the node is no longer healthy.
async fn unhealthy(state: &mut watch::Receiver<State>) {
    if state
        .wait_for(|state| *state != State::Healthy)
        .await
        .is_err()
    {
        std::future::pending::<()>().await;
    }
}

/// Waits for a block hash other than `current` on `tips`. Without a sender it waits forever, leaving the
/// other wake-ups to work alone.
async fn new_tip(
    tips: &mut watch::Receiver<Option<BlockHash>>,
    current: Option<BlockHash>,
) -> BlockHash {
    loop {
        if tips.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if let Some(hash) = *tips.borrow_and_update()
            && Some(hash) != current
        {
            return hash;
        }
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash;

    use super::*;
    use crate::node::fake::FakeNode;

    async fn next(work: &mut watch::Receiver<Work>) -> Work {
        tokio::time::timeout(Duration::from_secs(60), work.changed())
            .await
            .expect("no new work within 60s")
            .unwrap();
        work.borrow_and_update().clone()
    }

    async fn next_template(work: &mut watch::Receiver<Work>) -> Arc<Template> {
        match next(work).await {
            Work::Template(template) => template,
            Work::None => panic!("expected a template, got no work"),
        }
    }

    fn started() -> (Arc<FakeNode>, watch::Receiver<Work>) {
        let node = Arc::new(FakeNode::regtest());
        let (work, _) = spawn(node.clone(), Network::Regtest);
        (node, work)
    }

    /// The fake's template on a new tip.
    fn on_new_tip(node: &FakeNode, n: u8) -> BlockTemplate {
        let mut block = node.template();
        block.previousblockhash = BlockHash::from_byte_array([n; 32]);
        block.height += 1;
        block
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_prevhash_is_clean() {
        let (node, mut work) = started();
        assert!(next_template(&mut work).await.clean, "the first template");

        node.set_template(on_new_tip(&node, 1));
        let template = next_template(&mut work).await;
        assert!(template.clean);
        assert_eq!(
            template.block.previousblockhash,
            node.template().previousblockhash
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_mempool_refresh_is_not_clean() {
        let (node, mut work) = started();
        let first = next_template(&mut work).await;

        let mut refresh = node.template();
        refresh.transactions.clear();
        refresh.curtime += 30;
        node.set_template(refresh);
        let template = next_template(&mut work).await;
        assert!(!template.clean);
        assert_eq!(
            template.block.previousblockhash,
            first.block.previousblockhash
        );
        assert!(template.seq > first.seq);
    }

    #[tokio::test(start_paused = true)]
    async fn an_outage_issues_no_work_then_resumes_on_the_same_tip_unclean() {
        let (node, mut work) = started();
        let before = next_template(&mut work).await;

        node.set_failure(Some(NodeError::Refused("connection refused".into())));
        assert!(matches!(next(&mut work).await, Work::None));

        node.set_failure(None);
        let template = next_template(&mut work).await;
        assert_eq!(
            template.block.previousblockhash,
            before.block.previousblockhash
        );
        assert!(!template.clean, "jobs on the tip are still good");
    }

    #[tokio::test(start_paused = true)]
    async fn a_tip_that_moved_during_an_outage_is_clean() {
        let (node, mut work) = started();
        next_template(&mut work).await;

        node.set_failure(Some(NodeError::Timeout(Duration::from_secs(10))));
        assert!(matches!(next(&mut work).await, Work::None));

        node.set_template(on_new_tip(&node, 3));
        node.set_failure(None);
        let template = next_template(&mut work).await;
        assert!(template.clean, "jobs from before the outage are stale");
    }

    #[tokio::test(start_paused = true)]
    async fn a_template_refused_for_want_of_peers_is_no_outage() {
        let node = Arc::new(FakeNode::regtest());
        let (mut work, mut state) = spawn(node.clone(), Network::Regtest);
        next_template(&mut work).await;
        assert_eq!(*state.borrow_and_update(), State::Healthy);

        // Mainnet's getblocktemplate refuses while the node has no peers, which the peer count shows.
        node.set_peers(0);
        node.set_template_failure(Some(NodeError::Rpc {
            code: -9,
            message: "Bitcoin Core is not connected!".into(),
        }));
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(
            !state.has_changed().unwrap(),
            "the state flapped: {:?}",
            *state.borrow()
        );

        node.set_template_failure(None);
        node.set_peers(8);
        node.set_template(on_new_tip(&node, 5));
        assert!(next_template(&mut work).await.clean);
    }

    #[tokio::test(start_paused = true)]
    async fn hashblock_refreshes_what_the_long_poll_missed() {
        let (node, mut work) = started();
        next_template(&mut work).await;

        let block = on_new_tip(&node, 2);
        let prevhash = block.previousblockhash;
        node.set_template_quietly(block);
        node.announce_tip(prevhash);
        let template = next_template(&mut work).await;
        assert!(template.clean);
        assert_eq!(template.block.previousblockhash, prevhash);
    }

    #[tokio::test(start_paused = true)]
    async fn publishes_the_normalized_version() {
        let node = Arc::new(FakeNode::regtest());
        let mut block = node.template();
        block.version = 0x3000_0000;
        node.set_template(block);
        let (mut work, _) = spawn(node, Network::Regtest);
        assert_eq!(next_template(&mut work).await.block.version, 0x2000_0000);
    }

    #[tokio::test(start_paused = true)]
    async fn a_node_that_hung_mid_poll_serves_again_without_a_new_template() {
        let (node, mut work) = started();
        let before = next_template(&mut work).await;

        // The long poll stays waiting while the health check finds the node hung, as a paused node's does.
        node.set_health_failure(Some(NodeError::Timeout(Duration::from_secs(10))));
        assert!(matches!(next(&mut work).await, Work::None));

        // Back, with no new template to end that long poll: a fresh call serves it.
        node.set_health_failure(None);
        let template = next_template(&mut work).await;
        assert_eq!(
            template.block.previousblockhash,
            before.block.previousblockhash
        );
    }

    #[test]
    fn normalization_clears_unrequired_bits_inside_the_mask() {
        // Bit 28, the testdummy signal older regtest nodes set, goes; a vbrequired bit stays.
        assert_eq!(normalized_version(0x3000_0000, 0), 0x2000_0000);
        assert_eq!(normalized_version(0x3010_0000, 1 << 20), 0x2010_0000);
        // Bits outside BIP320's range are left alone.
        assert_eq!(normalized_version(0x2000_0001, 0), 0x2000_0001);
    }
}
