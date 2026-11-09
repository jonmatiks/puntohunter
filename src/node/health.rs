//! Whether a node may source templates, from getblockchaininfo and the outcome of every call to it (a node on
//! another chain is quarantined, and a hard fault takes it out). The selector (`fleet.rs`) weighs the nodes against each other.

use tokio::sync::watch;

use super::{BlockchainInfo, NodeClient, NodeError};
use crate::config::Network;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// No getblockchaininfo answer yet, so the chain is unchecked.
    Starting,
    Healthy,
    InIbd,
    Outage(Outage),
    /// The node is on another chain. Only a restart with the right config clears it.
    WrongChain {
        chain: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outage {
    Refused,
    /// A call timed out: a hung node counts as down.
    Hung,
    /// The node answered, but not with a usable result.
    Failed,
}

pub struct NodeHealth {
    /// The node's name, for the log.
    name: String,
    network: Network,
    state: State,
    /// The state, published for the portal's health check.
    published: watch::Sender<State>,
}

impl NodeHealth {
    pub fn new(name: String, network: Network) -> Self {
        crate::metrics::set_node_state(&name, State::Starting.label());
        NodeHealth {
            name,
            network,
            state: State::Starting,
            published: watch::Sender::new(State::Starting),
        }
    }

    /// The state as it changes.
    pub fn watch(&self) -> watch::Receiver<State> {
        self.published.subscribe()
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn may_issue_work(&self) -> bool {
        self.state == State::Healthy
    }

    /// Calls getblockchaininfo and feeds its outcome. Returns whether the state changed.
    pub async fn check(&mut self, node: &impl NodeClient) -> bool {
        let info = node.getblockchaininfo().await;
        self.observe_info(&info)
    }

    /// Feeds a getblockchaininfo outcome. Returns whether the state changed.
    pub fn observe_info(&mut self, info: &Result<BlockchainInfo, NodeError>) -> bool {
        match info {
            Ok(info) if info.chain != self.network.chain() => self.set(
                State::WrongChain {
                    chain: info.chain.clone(),
                },
                None,
            ),
            Ok(info) if info.initialblockdownload => self.set(State::InIbd, None),
            Ok(_) => self.set(State::Healthy, None),
            Err(e) => self.observe_error(e),
        }
    }

    /// Feeds the outcome of any other call. Only a failure changes the state: recovery is for
    /// getblockchaininfo to show. Returns whether the state changed.
    pub fn observe_call<T>(&mut self, result: &Result<T, NodeError>) -> bool {
        match result {
            Ok(_) => false,
            Err(e) => self.observe_error(e),
        }
    }

    fn observe_error(&mut self, e: &NodeError) -> bool {
        let outage = match e {
            NodeError::Refused(_) => Outage::Refused,
            NodeError::Timeout(_) => Outage::Hung,
            NodeError::Rpc { .. } | NodeError::Protocol(_) | NodeError::Credentials { .. } => {
                Outage::Failed
            }
        };
        self.set(State::Outage(outage), Some(e))
    }

    fn set(&mut self, next: State, cause: Option<&NodeError>) -> bool {
        if self.state == next || matches!(self.state, State::WrongChain { .. }) {
            return false;
        }
        let node = &self.name;
        match &next {
            State::Starting => {}
            State::Healthy => tracing::info!(node, "node healthy"),
            State::InIbd => tracing::warn!(node, "node in initial block download"),
            State::Outage(Outage::Refused) => {
                tracing::warn!(node, "node refused the connection: {}", describe(cause))
            }
            State::Outage(Outage::Hung) => {
                tracing::warn!(node, "node hung: {}", describe(cause))
            }
            State::Outage(Outage::Failed) => {
                tracing::warn!(node, "node call failed: {}", describe(cause))
            }
            State::WrongChain { chain } => tracing::error!(
                node,
                "node is on chain {chain}, but the config names {}; quarantined until restarted with \
                 the right config",
                self.network.chain()
            ),
        }
        crate::metrics::set_node_state(&self.name, next.label());
        self.published.send_replace(next.clone());
        self.state = next;
        true
    }
}

impl State {
    /// The state's name on the metrics endpoint, one of [`crate::metrics::NODE_STATES`].
    pub fn label(&self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Healthy => "healthy",
            State::InIbd => "ibd",
            State::Outage(Outage::Refused) => "refused",
            State::Outage(Outage::Hung) => "hung",
            State::Outage(Outage::Failed) => "failed",
            State::WrongChain { .. } => "wrong_chain",
        }
    }
}

fn describe(cause: Option<&NodeError>) -> String {
    cause.map(ToString::to_string).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{config::PRIMARY, node::fake::FakeNode};

    fn healthy() -> (FakeNode, NodeHealth) {
        let node = FakeNode::regtest();
        let health = NodeHealth::new(PRIMARY.into(), Network::Regtest);
        assert!(!health.may_issue_work(), "no work before the chain check");
        (node, health)
    }

    async fn checked(node: &FakeNode, health: &mut NodeHealth) -> bool {
        health.check(node).await;
        health.may_issue_work()
    }

    #[tokio::test]
    async fn issues_work_on_a_synced_node_of_the_right_chain() {
        let (node, mut health) = healthy();
        assert!(checked(&node, &mut health).await);
    }

    #[tokio::test]
    async fn wrong_chain_issues_no_work_for_the_life_of_the_process() {
        let (node, _) = healthy();
        let mut health = NodeHealth::new(PRIMARY.into(), Network::Main);
        assert!(!checked(&node, &mut health).await);
        assert_eq!(
            health.state(),
            &State::WrongChain {
                chain: "regtest".into()
            }
        );

        let mut main = node.info();
        main.chain = "main".into();
        node.set_info(main);
        assert!(
            !checked(&node, &mut health).await,
            "a matching chain later does not clear it"
        );
    }

    #[tokio::test]
    async fn ibd_issues_no_work_until_it_ends() {
        let (node, mut health) = healthy();
        let mut info = node.info();
        info.initialblockdownload = true;
        node.set_info(info.clone());
        assert!(!checked(&node, &mut health).await);
        assert_eq!(health.state(), &State::InIbd);

        info.initialblockdownload = false;
        node.set_info(info);
        assert!(checked(&node, &mut health).await);
    }

    #[tokio::test]
    async fn refused_and_hung_calls_issue_no_work_until_recovery() {
        for (failure, outage) in [
            (
                NodeError::Refused("connection refused".into()),
                Outage::Refused,
            ),
            (NodeError::Timeout(Duration::from_secs(10)), Outage::Hung),
        ] {
            let (node, mut health) = healthy();
            assert!(checked(&node, &mut health).await);

            node.set_failure(Some(failure));
            assert!(!checked(&node, &mut health).await);
            assert_eq!(health.state(), &State::Outage(outage));

            node.set_failure(None);
            assert!(checked(&node, &mut health).await);
        }
    }

    #[tokio::test]
    async fn a_failed_template_call_is_an_outage() {
        let (node, mut health) = healthy();
        assert!(checked(&node, &mut health).await);
        node.set_failure(Some(NodeError::Timeout(Duration::from_secs(120))));
        let template = crate::node::NodeClient::getblocktemplate(&node, None).await;
        assert!(health.observe_call(&template));
        assert!(!health.may_issue_work());
    }

    #[tokio::test]
    async fn reports_each_transition_once() {
        let (node, mut health) = healthy();
        assert!(health.check(&node).await);
        assert!(!health.check(&node).await);

        node.set_failure(Some(NodeError::Refused("first".into())));
        assert!(health.check(&node).await);
        node.set_failure(Some(NodeError::Refused("second".into())));
        assert!(
            !health.check(&node).await,
            "the same outage again is no transition"
        );
        node.set_failure(Some(NodeError::Timeout(Duration::from_secs(1))));
        assert!(
            health.check(&node).await,
            "a hung node is a different outage"
        );
    }
}
