//! The Prometheus endpoint, which the e2e harness also polls for readiness.
//!
//! It carries each node's state and standing in the fleet, tip, lag, chainwork, RPC latency, whether it sources
//! the work, and moves of the work between nodes; the tip and age of the current template; shares and refusals
//! by pool and transport; how long a share waits for its answer, by pool and transport; block submission
//! latency, overall and per node; connected workers by pool and transport, a series for each configured
//! pool; and whether each group member counts in the group's totals. No metric has a per-worker label: the
//! portal owns worker stats, and a label is where an address would leak. Values that change with the clock
//! or the tip are read when the endpoint is scraped, so a tip hash never outlives its tip as a series.

use std::{
    fmt::Write,
    net::SocketAddr,
    sync::Arc,
    time::{Instant, SystemTime},
};

use axum::{Router, extract::State, routing::get};
use metrics::SetRecorderError;
use metrics_exporter_prometheus::{
    BuildError, Matcher, PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
};
use tokio::{net::TcpListener, sync::watch};

use crate::{
    mining::pool::PoolId,
    node::template::Work,
    pool::ledger,
    store::{Reader, SyncStatus},
    stratum::Transport,
    telemetry::{Reject, Telemetry, unix},
};

const READY: &str = "puntohunter_ready";
const NODE_STATE: &str = "puntohunter_node_state";
const NODE_STANDING: &str = "puntohunter_node_standing";
const NODE_TIP_HEIGHT: &str = "puntohunter_node_tip_height";
const NODE_LAG: &str = "puntohunter_node_lag_blocks";
const NODE_CHAINWORK: &str = "puntohunter_node_chainwork_log2";
const NODE_RPC: &str = "puntohunter_node_rpc_seconds";
const NODE_SELECTED: &str = "puntohunter_node_selected";
const NODE_SOURCE_CHANGES: &str = "puntohunter_node_source_changes_total";
const TIP_HEIGHT: &str = "puntohunter_tip_height";
const TIP: &str = "puntohunter_tip";
const TEMPLATE_AGE: &str = "puntohunter_template_age_seconds";
const SHARES_ACCEPTED: &str = "puntohunter_shares_accepted_total";
const SHARES_REJECTED: &str = "puntohunter_shares_rejected_total";
const SHARE_RESPONSE: &str = "puntohunter_share_response_seconds";
const SUBMISSION: &str = "puntohunter_block_submission_seconds";
const NODE_SUBMISSION: &str = "puntohunter_node_submission_seconds";
const WORKERS: &str = "puntohunter_workers_connected";
const JOURNAL_COMMIT: &str = "puntohunter_journal_commit_seconds";
const JOURNAL_FAILED: &str = "puntohunter_journal_failed";
const JOURNAL_UNSYNCED: &str = "puntohunter_journal_unsynced_seconds";
const PAYOUTS_PAUSED: &str = "puntohunter_payouts_paused";
const PAYOUTS_DEFERRED: &str = "puntohunter_payouts_deferred";
const PAYOUTS_HALTED: &str = "puntohunter_payouts_halted";
const PAYOUTS_LOCKED: &str = "puntohunter_payouts_locked";
const WALLET_HELD: &str = "puntohunter_wallet_held_sats";
const LIABILITIES: &str = "puntohunter_liabilities_sats";
const FEE_RESERVE: &str = "puntohunter_fee_reserve_sats";
const ACCOUNTING_BACKLOG: &str = "puntohunter_accounting_backlog_blocks";
const BATCHES_UNCONFIRMED: &str = "puntohunter_payout_batches_unconfirmed";
const BATCH_AGE: &str = "puntohunter_payout_batch_oldest_unconfirmed_seconds";
const WALLET_COVERS: &str = "puntohunter_wallet_covers_liabilities";
const GROUP_MEMBER_UP: &str = "puntohunter_group_member_up";
const BACKUP_LAST: &str = "puntohunter_backup_last_seconds";
const BACKUPS_FAILED: &str = "puntohunter_backups_failed_total";

/// The standings in the fleet NODE_STANDING reports, one series each.
pub const NODE_STANDINGS: [&str; 10] = [
    "fit",
    "recovering",
    "stuck",
    "isolated",
    "starting",
    "ibd",
    "refused",
    "hung",
    "failed",
    "quarantined",
];

/// The node states NODE_STATE reports, one series each.
pub const NODE_STATES: [&str; 7] = [
    "starting",
    "healthy",
    "ibd",
    "refused",
    "hung",
    "failed",
    "wrong_chain",
];

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("cannot start metrics endpoint on {bind}: {source}")]
    Bind {
        bind: SocketAddr,
        source: std::io::Error,
    },
    #[error("cannot configure metrics: {0}")]
    Build(#[from] BuildError),
    #[error("cannot install metrics recorder: {0}")]
    Install(#[from] SetRecorderError<PrometheusRecorder>),
}

/// The bound endpoint, not yet serving.
pub struct Endpoint {
    listener: TcpListener,
    handle: PrometheusHandle,
}

/// Binds the metrics listener and installs the global recorder, so what happens from here on is counted.
pub async fn install(bind: SocketAddr) -> Result<Endpoint, MetricsError> {
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| MetricsError::Bind { bind, source })?;
    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(SUBMISSION.into()),
            &[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0],
        )?
        .set_buckets_for_metric(
            Matcher::Full(NODE_SUBMISSION.into()),
            &[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0],
        )?
        // Fine below the 50 ms share response bound, which is a bucket's edge, so a quantile against it is exact.
        .set_buckets_for_metric(
            Matcher::Full(SHARE_RESPONSE.into()),
            &[
                0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0,
            ],
        )?
        .set_buckets_for_metric(
            Matcher::Full(JOURNAL_COMMIT.into()),
            &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0],
        )?
        .build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder)?;
    metrics::describe_gauge!(READY, "1 once every listener is bound");
    metrics::describe_gauge!(
        NODE_STATE,
        "1 for each node's current state, 0 for the others, by node"
    );
    metrics::describe_gauge!(
        NODE_STANDING,
        "1 for each node's standing in the fleet, 0 for the others, by node: fit, a soft fault (recovering, \
         stuck, isolated), or a hard fault"
    );
    metrics::describe_gauge!(NODE_TIP_HEIGHT, "Height of each node's tip, by node");
    metrics::describe_gauge!(
        NODE_LAG,
        "Blocks each node's tip is behind the fleet's most-work tip, by node"
    );
    metrics::describe_gauge!(
        NODE_CHAINWORK,
        "log2 of the work of each node's chain up to its tip, by node"
    );
    metrics::describe_gauge!(
        NODE_RPC,
        "Seconds each node's last getblockchaininfo took, by node"
    );
    metrics::describe_gauge!(NODE_SELECTED, "1 for the node the current work comes from");
    metrics::describe_counter!(
        NODE_SOURCE_CHANGES,
        "Moves of template sourcing from one node to another, by the node left and the node taken: a failover, \
         or a new tip served from the first node to report it and the move back once the source catches up"
    );
    metrics::describe_gauge!(
        TIP_HEIGHT,
        "Height of the tip the current template builds on"
    );
    metrics::describe_gauge!(
        TEMPLATE_AGE,
        "Seconds since the node made the current template"
    );
    metrics::describe_counter!(SHARES_ACCEPTED, "Shares accepted, by pool and transport");
    metrics::describe_counter!(
        SHARES_REJECTED,
        "Shares refused, by pool, transport, and class"
    );
    metrics::describe_histogram!(
        SHARE_RESPONSE,
        "Seconds from a submit read to its answer written, V1's response or the SV2 acknowledgement or error \
         covering it, by pool and transport"
    );
    metrics::describe_histogram!(
        SUBMISSION,
        "Seconds from a found block's submission to the first node's success"
    );
    metrics::describe_histogram!(
        NODE_SUBMISSION,
        "Seconds each node took to answer a found block, by node and answer: success, reject, or unanswered"
    );
    metrics::describe_gauge!(
        WORKERS,
        "Workers with a connection open, by pool and transport"
    );
    metrics::describe_histogram!(
        JOURNAL_COMMIT,
        "Seconds a share journal commit took, from its batch's start to written"
    );
    metrics::describe_gauge!(
        JOURNAL_FAILED,
        "1 once a journal commit failed: pooled and hybrid shares are refused until a restart"
    );
    metrics::describe_gauge!(
        JOURNAL_UNSYNCED,
        "Seconds the oldest journal commit not yet synced to disk has waited; 0 when all are synced"
    );
    metrics::describe_gauge!(
        PAYOUTS_PAUSED,
        "1 while payouts are paused for reconciliation, as when a block left the chain after its \
         allocations were reserved or paid"
    );
    metrics::describe_gauge!(
        PAYOUTS_DEFERRED,
        "1 while the payout batch due is deferred, as when the fee rate is above the cap or the fee reserve \
         is short; the log says why"
    );
    metrics::describe_gauge!(
        BACKUP_LAST,
        "Unix seconds when the last database snapshot was written; absent before the first"
    );
    metrics::describe_counter!(BACKUPS_FAILED, "Database snapshots that failed");
    metrics::describe_gauge!(
        PAYOUTS_HALTED,
        "1 while payouts are halted because startup reconciliation found the books, the payout wallet, or \
         the chain disagree; the log says where"
    );
    metrics::describe_gauge!(
        PAYOUTS_LOCKED,
        "1 while a reserved payout batch waits for the operator to unlock the payout wallet to sign it"
    );
    metrics::describe_gauge!(
        LIABILITIES,
        "Satoshis miners are owed: every pending, available, and reserved balance"
    );
    metrics::describe_gauge!(FEE_RESERVE, "Satoshis the fee reserve holds");
    metrics::describe_gauge!(
        ACCOUNTING_BACKLOG,
        "Blocks with a pool output recorded and not yet allocated"
    );
    metrics::describe_gauge!(
        BATCHES_UNCONFIRMED,
        "Payout batches reserved and not yet confirmed"
    );
    metrics::describe_gauge!(
        BATCH_AGE,
        "Seconds since the oldest unconfirmed payout batch was reserved, 0 when none is; a stuck batch \
         keeps it growing"
    );
    metrics::describe_gauge!(
        WALLET_HELD,
        "Satoshis the payout wallet holds, spendable or immature, at the payout worker's last interval"
    );
    metrics::describe_gauge!(
        WALLET_COVERS,
        "1 while the payout wallet holds at least what miners are owed and the fee reserve, less what \
         batches already sent"
    );
    metrics::describe_gauge!(
        GROUP_MEMBER_UP,
        "1 while the group member counts in the group's totals, by member"
    );
    tracing::info!(%bind, "metrics endpoint listening");
    Ok(Endpoint { listener, handle })
}

/// Serves the endpoint in the background, reading the tip and template from `work` and the workers from
/// `telemetry` at each scrape, with a series of connected workers for each of `pools` on each transport it
/// serves.
pub fn serve(
    endpoint: Endpoint,
    pools: Vec<(PoolId, Vec<Transport>)>,
    work: watch::Receiver<Work>,
    telemetry: Arc<Telemetry>,
    sync: Arc<SyncStatus>,
    reader: Reader,
) {
    let app = Router::new()
        .route("/metrics", get(scrape))
        .with_state(Arc::new(Sources {
            handle: endpoint.handle,
            pools,
            work,
            telemetry,
            sync,
            reader,
        }));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(endpoint.listener, app).await {
            tracing::error!("metrics endpoint stopped: {e}");
        }
    });
}

struct Sources {
    handle: PrometheusHandle,
    /// The configured pools and the transports each serves, each counted whether or not a worker is connected.
    pools: Vec<(PoolId, Vec<Transport>)>,
    work: watch::Receiver<Work>,
    telemetry: Arc<Telemetry>,
    /// The store's syncs, for the journal's unsynced age.
    sync: Arc<SyncStatus>,
    /// The books, for the accounting figures.
    reader: Reader,
}

async fn scrape(State(sources): State<Arc<Sources>>) -> String {
    let work = sources.work.borrow().clone();
    if let Work::Template(template) = &work {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let age = now.saturating_sub(template.block.curtime.into());
        metrics::gauge!(TEMPLATE_AGE).set(age as f64);
    }
    metrics::gauge!(JOURNAL_UNSYNCED)
        .set(sources.sync.unsynced_for(SystemTime::now()).as_secs_f64());
    match sources.reader.read(ledger::figures).await {
        Ok(figures) => {
            metrics::gauge!(LIABILITIES).set(figures.owed.to_sat() as f64);
            metrics::gauge!(FEE_RESERVE).set(figures.reserve.to_sat() as f64);
            metrics::gauge!(ACCOUNTING_BACKLOG).set(figures.backlog as f64);
            metrics::gauge!(BATCHES_UNCONFIRMED).set(figures.unconfirmed as f64);
            let age = figures
                .oldest_unconfirmed
                .map_or(0, |at| unix(SystemTime::now()).saturating_sub(at));
            metrics::gauge!(BATCH_AGE).set(age as f64);
        }
        Err(e) => tracing::error!("cannot read the accounting figures: {e}"),
    }
    let connected = sources.telemetry.connected();
    for (pool, transports) in &sources.pools {
        for &transport in transports {
            let count = connected
                .get(&(pool.clone(), transport))
                .copied()
                .unwrap_or(0);
            metrics::gauge!(WORKERS, "pool" => pool.to_string(), "transport" => transport.as_str())
                .set(count as f64);
        }
    }
    let mut text = sources.handle.render();
    // The tip's hash as an info metric of its own, present only while there is work on it.
    if let Work::Template(template) = &work {
        let _ = write!(
            text,
            "# HELP {TIP} The tip the current template builds on\n# TYPE {TIP} gauge\n\
             {TIP}{{hash=\"{}\"}} 1\n",
            template.block.previousblockhash
        );
    }
    text
}

/// Reports readiness; call once every listener is bound.
pub fn set_ready() {
    metrics::gauge!(READY).set(1.0);
}

/// Reports a node's state, one of [`NODE_STATES`].
pub fn set_node_state(node: &str, state: &'static str) {
    for name in NODE_STATES {
        metrics::gauge!(NODE_STATE, "node" => node.to_owned(), "state" => name)
            .set(if name == state { 1.0 } else { 0.0 });
    }
}

/// What the fleet makes of a node at a selection round.
pub struct NodeFigures<'a> {
    pub node: &'a str,
    /// One of [`NODE_STANDINGS`].
    pub standing: &'static str,
    pub height: Option<u32>,
    pub lag: u32,
    pub chainwork_log2: Option<f64>,
    pub rpc: Option<std::time::Duration>,
    pub selected: bool,
}

pub fn set_node_figures(figures: NodeFigures) {
    let node = figures.node;
    for name in NODE_STANDINGS {
        metrics::gauge!(NODE_STANDING, "node" => node.to_owned(), "standing" => name)
            .set(if name == figures.standing { 1.0 } else { 0.0 });
    }
    if let Some(height) = figures.height {
        metrics::gauge!(NODE_TIP_HEIGHT, "node" => node.to_owned()).set(f64::from(height));
    }
    metrics::gauge!(NODE_LAG, "node" => node.to_owned()).set(f64::from(figures.lag));
    if let Some(chainwork) = figures.chainwork_log2 {
        metrics::gauge!(NODE_CHAINWORK, "node" => node.to_owned()).set(chainwork);
    }
    if let Some(rpc) = figures.rpc {
        metrics::gauge!(NODE_RPC, "node" => node.to_owned()).set(rpc.as_secs_f64());
    }
    metrics::gauge!(NODE_SELECTED, "node" => node.to_owned()).set(if figures.selected {
        1.0
    } else {
        0.0
    });
}

/// Reports whether group member `member` counts in the group's totals.
pub fn set_group_member_up(member: &str, up: bool) {
    metrics::gauge!(GROUP_MEMBER_UP, "member" => member.to_owned()).set(if up { 1.0 } else { 0.0 });
}

/// Counts a move of template sourcing from node `from` to node `to`.
pub fn node_source_changed(from: &str, to: &str) {
    metrics::counter!(NODE_SOURCE_CHANGES, "from" => from.to_owned(), "to" => to.to_owned())
        .increment(1);
}

pub fn set_tip_height(height: u32) {
    metrics::gauge!(TIP_HEIGHT).set(f64::from(height));
}

pub fn share_accepted(pool: &PoolId, transport: Transport) {
    metrics::counter!(SHARES_ACCEPTED, "pool" => pool.to_string(), "transport" => transport.as_str())
        .increment(1);
}

pub fn share_rejected(pool: &PoolId, transport: Transport, class: Reject) {
    metrics::counter!(
        SHARES_REJECTED,
        "pool" => pool.to_string(),
        "transport" => transport.as_str(),
        "class" => class.as_str()
    )
    .increment(1);
}

/// Records a submit answered, from `read`, when it was read.
pub fn share_answered(pool: &PoolId, transport: Transport, read: Instant) {
    metrics::histogram!(SHARE_RESPONSE, "pool" => pool.to_string(), "transport" => transport.as_str())
        .record(read.elapsed().as_secs_f64());
}

pub fn block_submitted(seconds: f64) {
    metrics::histogram!(SUBMISSION).record(seconds);
}

/// Records one node's answer to a found block: `answer` is success, reject, or unanswered.
pub fn node_submission(node: &str, answer: &'static str, seconds: f64) {
    metrics::histogram!(NODE_SUBMISSION, "node" => node.to_owned(), "answer" => answer)
        .record(seconds);
}

pub fn journal_committed(seconds: f64) {
    metrics::histogram!(JOURNAL_COMMIT).record(seconds);
}

pub fn set_journal_failed(failed: bool) {
    metrics::gauge!(JOURNAL_FAILED).set(if failed { 1.0 } else { 0.0 });
}

/// A database snapshot written at `at`.
pub fn backup_written(at: SystemTime) {
    let secs = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    metrics::gauge!(BACKUP_LAST).set(secs);
}

/// A database snapshot that failed.
pub fn backup_failed() {
    metrics::counter!(BACKUPS_FAILED).increment(1);
}

pub fn set_payouts_paused(paused: bool) {
    metrics::gauge!(PAYOUTS_PAUSED).set(if paused { 1.0 } else { 0.0 });
}

pub fn set_payouts_deferred(deferred: bool) {
    metrics::gauge!(PAYOUTS_DEFERRED).set(if deferred { 1.0 } else { 0.0 });
}

pub fn set_payouts_halted(halted: bool) {
    metrics::gauge!(PAYOUTS_HALTED).set(if halted { 1.0 } else { 0.0 });
}

pub fn set_payouts_locked(locked: bool) {
    metrics::gauge!(PAYOUTS_LOCKED).set(if locked { 1.0 } else { 0.0 });
}

/// What the payout wallet holds, and whether that covers the liabilities and the fee reserve.
pub fn set_wallet(held: bitcoin::Amount, covers: bool) {
    metrics::gauge!(WALLET_HELD).set(held.to_sat() as f64);
    metrics::gauge!(WALLET_COVERS).set(if covers { 1.0 } else { 0.0 });
}
