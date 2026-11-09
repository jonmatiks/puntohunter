//! An in-memory node behind [`NodeClient`], for core tests.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use bitcoin::{BlockHash, Work};
use tokio::sync::{Notify, watch};

use super::{
    BlockHeaderInfo, BlockTemplate, BlockchainInfo, NetworkInfo, NodeClient, NodeError,
    recorded::{self, REGTEST_GETBLOCKCHAININFO, REGTEST_GETBLOCKTEMPLATE, REGTEST_GETNETWORKINFO},
};

pub struct FakeNode {
    state: Mutex<State>,
    changed: Notify,
    tips: watch::Sender<Option<BlockHash>>,
}

struct State {
    info: BlockchainInfo,
    network: NetworkInfo,
    template: BlockTemplate,
    templates_set: u64,
    /// The work of the chain the template builds on, which getblockheader gives for its prevhash.
    template_chainwork: Work,
    /// How long getblockchaininfo takes to answer.
    delay: Duration,
    /// Fails getblockchaininfo alone, leaving a waiting long poll waiting, as a node that hangs mid-poll.
    health_failure: Option<NodeError>,
    /// Fails getblocktemplate alone, as mainnet's does while the node has no peers.
    template_failure: Option<NodeError>,
    failure: Option<NodeError>,
    /// What submitblock answers.
    submit_answer: Option<String>,
    /// How long submitblock hangs before it times out, when set.
    submit_hang: Option<Duration>,
    /// Every block submitted while the node answered, in hex.
    submitted: Vec<String>,
    headers: HashMap<BlockHash, BlockHeaderInfo>,
}

impl FakeNode {
    /// A healthy regtest node serving the recorded regtest template.
    pub fn regtest() -> Self {
        let info: BlockchainInfo = recorded::result(REGTEST_GETBLOCKCHAININFO);
        FakeNode {
            state: Mutex::new(State {
                template_chainwork: info.chainwork,
                info,
                network: recorded::result(REGTEST_GETNETWORKINFO),
                template: recorded::result(REGTEST_GETBLOCKTEMPLATE),
                templates_set: 0,
                delay: Duration::ZERO,
                health_failure: None,
                template_failure: None,
                failure: None,
                submit_answer: None,
                submit_hang: None,
                submitted: Vec::new(),
                headers: HashMap::new(),
            }),
            changed: Notify::new(),
            tips: watch::Sender::new(None),
        }
    }

    pub fn info(&self) -> BlockchainInfo {
        self.state.lock().unwrap().info.clone()
    }

    pub fn template(&self) -> BlockTemplate {
        self.state.lock().unwrap().template.clone()
    }

    pub fn set_info(&self, info: BlockchainInfo) {
        self.state.lock().unwrap().info = info;
    }

    pub fn set_network(&self, network: NetworkInfo) {
        self.state.lock().unwrap().network = network;
    }

    /// Serves `template` under a fresh longpollid, returning any long poll waiting on the old one.
    pub fn set_template(&self, template: BlockTemplate) {
        self.set_template_quietly(template);
        self.changed.notify_waiters();
    }

    /// Serves `template` under a fresh longpollid, but leaves a waiting long poll waiting, as if it had
    /// missed the change.
    pub fn set_template_quietly(&self, mut template: BlockTemplate) {
        let mut state = self.state.lock().unwrap();
        state.templates_set += 1;
        template.longpollid = format!("fake-{}", state.templates_set);
        state.template = template;
    }

    /// Moves the node onto tip `hash` at `height` with `chainwork`: getblockchaininfo reports it, and a new
    /// template builds on it, returning any waiting long poll.
    pub fn set_tip(&self, hash: BlockHash, height: u32, chainwork: Work) {
        let mut template = {
            let mut state = self.state.lock().unwrap();
            state.info.bestblockhash = hash;
            state.info.blocks = height;
            state.info.chainwork = chainwork;
            state.template_chainwork = chainwork;
            state.template.clone()
        };
        template.previousblockhash = hash;
        template.height = height + 1;
        self.set_template(template);
    }

    /// Sets the peers getnetworkinfo reports.
    pub fn set_peers(&self, connections: u32) {
        self.state.lock().unwrap().network.connections = connections;
    }

    /// Fails getblockchaininfo alone with `failure` until it is cleared with None.
    pub fn set_health_failure(&self, failure: Option<NodeError>) {
        self.state.lock().unwrap().health_failure = failure;
    }

    /// Makes getblockchaininfo take `delay` to answer.
    pub fn set_delay(&self, delay: Duration) {
        self.state.lock().unwrap().delay = delay;
    }

    /// Announces a new block, as ZMQ hashblock does.
    pub fn announce_tip(&self, hash: BlockHash) {
        self.tips.send_replace(Some(hash));
    }

    /// Fails every call with `failure` until it is cleared with None. A waiting long poll fails too.
    pub fn set_failure(&self, failure: Option<NodeError>) {
        self.state.lock().unwrap().failure = failure;
        self.changed.notify_waiters();
    }

    /// Fails getblocktemplate alone with `failure` until it is cleared with None. A waiting long poll fails too.
    pub fn set_template_failure(&self, failure: Option<NodeError>) {
        self.state.lock().unwrap().template_failure = failure;
        self.changed.notify_waiters();
    }

    /// Makes submitblock answer `answer`: None for accepted, or a reason.
    pub fn set_submit_answer(&self, answer: Option<&str>) {
        self.state.lock().unwrap().submit_answer = answer.map(str::to_owned);
    }

    /// Makes submitblock hang for `hang`, then time out, or answer again with None.
    pub fn set_submit_hang(&self, hang: Option<Duration>) {
        self.state.lock().unwrap().submit_hang = hang;
    }

    /// The blocks submitted while the node answered, in hex.
    pub fn submitted(&self) -> Vec<String> {
        self.state.lock().unwrap().submitted.clone()
    }

    /// Serves `header` for block `hash`, or forgets the block.
    pub fn set_header(&self, hash: BlockHash, header: Option<BlockHeaderInfo>) {
        let mut state = self.state.lock().unwrap();
        match header {
            Some(header) => state.headers.insert(hash, header),
            None => state.headers.remove(&hash),
        };
    }
}

impl NodeClient for FakeNode {
    async fn getblockchaininfo(&self) -> Result<BlockchainInfo, NodeError> {
        let delay = self.state.lock().unwrap().delay;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let state = self.state.lock().unwrap();
        match state.failure.as_ref().or(state.health_failure.as_ref()) {
            Some(failure) => Err(failure.clone()),
            None => Ok(state.info.clone()),
        }
    }

    async fn getnetworkinfo(&self) -> Result<NetworkInfo, NodeError> {
        let state = self.state.lock().unwrap();
        match &state.failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(state.network.clone()),
        }
    }

    async fn getblocktemplate(&self, longpollid: Option<&str>) -> Result<BlockTemplate, NodeError> {
        loop {
            // Registered before the state is read, so a change in between still wakes this call.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock().unwrap();
                if let Some(failure) = state.failure.as_ref().or(state.template_failure.as_ref()) {
                    return Err(failure.clone());
                }
                if longpollid != Some(state.template.longpollid.as_str()) {
                    return Ok(state.template.clone());
                }
            }
            changed.await;
        }
    }

    async fn submitblock(&self, hex: &str) -> Result<Option<String>, NodeError> {
        let hang = self.state.lock().unwrap().submit_hang;
        if let Some(hang) = hang {
            tokio::time::sleep(hang).await;
            return Err(NodeError::Timeout(hang));
        }
        let mut state = self.state.lock().unwrap();
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        state.submitted.push(hex.to_owned());
        Ok(state.submit_answer.clone())
    }

    async fn getblockheader(&self, hash: BlockHash) -> Result<Option<BlockHeaderInfo>, NodeError> {
        let state = self.state.lock().unwrap();
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        // A header set by a test, or else the one the template builds on.
        Ok(state.headers.get(&hash).cloned().or_else(|| {
            (hash == state.template.previousblockhash).then(|| BlockHeaderInfo {
                height: state.template.height - 1,
                confirmations: 1,
                chainwork: state.template_chainwork,
            })
        }))
    }

    fn tip_events(&self) -> watch::Receiver<Option<BlockHash>> {
        self.tips.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn long_poll_returns_on_a_new_template() {
        let node = std::sync::Arc::new(FakeNode::regtest());
        let current = node.getblocktemplate(None).await.unwrap();

        let poll = tokio::spawn({
            let node = node.clone();
            async move { node.getblocktemplate(Some(&current.longpollid)).await }
        });
        tokio::task::yield_now().await;
        assert!(!poll.is_finished());

        let mut next = node.template();
        next.curtime += 1;
        node.set_template(next);
        let polled = tokio::time::timeout(Duration::from_secs(5), poll)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(polled.longpollid, node.template().longpollid);
    }

    #[tokio::test]
    async fn fails_every_call_until_cleared() {
        let node = FakeNode::regtest();
        node.set_failure(Some(NodeError::Refused("down".into())));
        assert!(node.getblockchaininfo().await.is_err());
        assert!(node.getblocktemplate(None).await.is_err());
        node.set_failure(None);
        assert!(node.getblockchaininfo().await.is_ok());
    }
}
