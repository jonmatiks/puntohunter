//! Whether the pool is serving (API reference section 2.10): a coarse status and state codes, for
//! uptime monitors and the portal's outage banner. It reads memory only, never the store, and never carries
//! error text, which can name internal hosts, ports, and paths; the operator reads the detail from
//! Prometheus.

use std::time::SystemTime;

use serde::Serialize;

use super::{Portal, VERSION};
use crate::{
    node::{
        fleet::{Standing, Status},
        health::State,
    },
    telemetry::unix,
};

/// The health resource.
#[derive(Debug, Serialize)]
pub struct Health {
    /// `ok`, `degraded`, or `unavailable`.
    pub status: &'static str,
    pub version: &'static str,
    pub uptime_secs: u64,
    /// The tip the current template builds on; None without one.
    pub tip_height: Option<u32>,
    pub checks: Checks,
}

#[derive(Debug, Serialize)]
pub struct Checks {
    /// `starting`, `healthy`, `degraded`, `ibd`, `outage`, or `wrong_chain`. With secondary nodes it is the
    /// fleet's: `healthy` while every node is fit, `degraded` while work continues with a node out, and the
    /// primary's code while there is no work.
    pub node: &'static str,
    /// Whether miners get work now.
    pub work: bool,
    /// `accepting` or `stopped`; None without a pool that can pay a pool output.
    pub pooled_credit: Option<&'static str>,
    /// Where payouts stand; None without `[payouts]`.
    pub payouts: Option<&'static str>,
    /// With secondary nodes, how many are fit of how many are configured; never which.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nodes: Option<NodeCounts>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct NodeCounts {
    pub fit: usize,
    pub configured: usize,
}

impl Health {
    /// Whether miners get no work, which a monitor reads from the HTTP status alone.
    pub fn unavailable(&self) -> bool {
        self.status == "unavailable"
    }
}

impl Portal {
    pub fn health(&self) -> Health {
        let chain = self.chain();
        let (node, nodes) = node_check(&self.node.borrow(), &self.nodes.borrow(), chain.is_some());
        let pooled = self.pools.iter().any(|pool| pool.can_pay_pool());
        let pooled_credit = pooled.then(|| {
            if self.journal.failed() {
                "stopped"
            } else {
                "accepting"
            }
        });
        let payouts = self
            .payouts
            .as_ref()
            .map(|terms| super::pool::StandingView::from(&*terms.standing.borrow()).state);
        let status = if chain.is_none() {
            "unavailable"
        } else if node == "degraded"
            || pooled_credit == Some("stopped")
            || matches!(payouts, Some("paused" | "halted"))
        {
            "degraded"
        } else {
            "ok"
        };
        Health {
            status,
            version: VERSION,
            uptime_secs: unix(SystemTime::now()).saturating_sub(unix(self.started)),
            // The tip is one below the block the template builds.
            tip_height: chain.as_ref().map(|chain| chain.height.saturating_sub(1)),
            checks: Checks {
                node,
                work: chain.is_some(),
                pooled_credit,
                payouts,
                nodes,
            },
        }
    }
}

/// The node's code and, with secondary nodes, the fleet's counts, given the primary's state, every node's
/// standing, and whether miners get work.
fn node_check(primary: &State, fleet: &Status, work: bool) -> (&'static str, Option<NodeCounts>) {
    let code = match primary {
        State::Starting => "starting",
        State::Healthy => "healthy",
        State::InIbd => "ibd",
        State::Outage(_) => "outage",
        State::WrongChain { .. } => "wrong_chain",
    };
    if fleet.nodes.len() < 2 {
        return (code, None);
    }
    let counts = NodeCounts {
        fit: fleet
            .nodes
            .iter()
            .filter(|node| node.standing == Standing::Fit)
            .count(),
        configured: fleet.nodes.len(),
    };
    let code = match work {
        true if counts.fit == counts.configured => "healthy",
        true => "degraded",
        false => code,
    };
    (code, Some(counts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{fleet::NodeStatus, health::Outage};

    fn fleet(standings: &[Standing]) -> Status {
        Status {
            nodes: standings
                .iter()
                .map(|&standing| NodeStatus {
                    standing,
                    height: Some(100),
                    lag: 0,
                    latency: None,
                })
                .collect(),
            source: Some(0),
        }
    }

    #[test]
    fn one_node_gives_its_own_state_and_no_counts() {
        let lone = fleet(&[Standing::Isolated]);
        assert_eq!(node_check(&State::Healthy, &lone, true), ("healthy", None));
        let down = State::Outage(Outage::Refused);
        assert_eq!(node_check(&down, &lone, false), ("outage", None));
    }

    #[test]
    fn a_fleet_is_healthy_only_with_every_node_fit() {
        let all = fleet(&[Standing::Fit, Standing::Fit, Standing::Fit]);
        let counts = |fit| Some(NodeCounts { fit, configured: 3 });
        assert_eq!(
            node_check(&State::Healthy, &all, true),
            ("healthy", counts(3))
        );

        let one_out = fleet(&[
            Standing::Down(Outage::Hung),
            Standing::Fit,
            Standing::Recovering,
        ]);
        let hung = State::Outage(Outage::Hung);
        assert_eq!(node_check(&hung, &one_out, true), ("degraded", counts(1)));

        let none = fleet(&[
            Standing::Down(Outage::Hung),
            Standing::Ibd,
            Standing::Quarantined,
        ]);
        assert_eq!(node_check(&hung, &none, false), ("outage", counts(0)));
    }
}
