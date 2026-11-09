//! The Stratum front-ends and what they share: one context, one username parser, and one share check, so V1
//! and V2 are wire-format adapters over the same core.

pub mod username;
pub mod v1;
pub mod v2;

use std::{
    collections::HashSet,
    fmt,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use bitcoin::{Amount, BlockHash, ScriptBuf, Target, hashes::Hash};
use tokio::sync::{mpsc, watch};

use self::username::Username;
use crate::{
    config::{self, Network},
    geo::{Country, Geo},
    metrics,
    mining::{
        block,
        extranonce::ExtranonceAllocator,
        job::{Job, JobCache, Submission},
        pool::{Pool, PoolId},
        share::{self, Checked},
        target,
    },
    node::template::Work,
    pool::{
        blocks::FoundBlock,
        journal::{self, Entry, Journal},
    },
    telemetry::{
        self, Choices, Link, Listing, Reject, Telemetry,
        fleet::{self, Fleets, Member},
        handle::{Handle, HandleKey},
    },
};

/// What every session, on every listener of either protocol, shares.
pub struct Context {
    pub network: Network,
    pub vardiff: config::Vardiff,
    pub extranonces: Arc<ExtranonceAllocator>,
    pub work: watch::Receiver<Work>,
    pub jobs: JobCache,
    /// What a finder leaves of the miner portion pays it, and all of it in a pool that cannot pay a finder.
    pub pool_wallet: ScriptBuf,
    /// The submitter, which takes every block a share finds.
    pub blocks: mpsc::UnboundedSender<FoundBlock>,
    /// How long a connection may go without authorizing (V1) or opening a channel (V2) before it is closed.
    pub authorize_timeout: Duration,
    /// Derives each worker's handle.
    pub handles: HandleKey,
    pub telemetry: Arc<Telemetry>,
    /// Records each worker in its address's fleet.
    pub fleets: Fleets,
    /// Takes every accepted share of a pool that can pay a pool output before it is acknowledged.
    pub journal: Journal,
    /// Each connection's country, from its peer address; None without `[geo]`.
    pub geo: Option<Arc<Geo>>,
}

/// The protocol a session speaks, carried beside its pool for the portal and metrics to tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Transport {
    V1,
    V2Native,
}

impl Transport {
    /// Its user-facing label.
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::V1 => "V1",
            Transport::V2Native => "V2-native",
        }
    }

    /// The transport `as_str` names.
    pub fn parse(text: &str) -> Option<Self> {
        [Transport::V1, Transport::V2Native]
            .into_iter()
            .find(|transport| transport.as_str() == text)
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who a session mines for: set when a V1 session authorizes, and per SV2 channel. It holds the worker's
/// telemetry connection, which dropping it closes.
pub struct Worker {
    pub username: Username,
    pub pool: Arc<Pool>,
    pub transport: Transport,
    pub handle: Handle,
    pub link: Link,
}

/// Why the shared share check refused a share that matched one of its session's jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// A header hash the session already accepted, under whichever job, or one the journal already holds.
    Duplicate,
    LowDifficulty,
    /// A share the journal could not take: a commit failed, so none is credited until the pool restarts.
    Unavailable,
}

impl Context {
    /// The worker `username` names, mining in `pool` over `transport`, recorded in its fleet with the miner
    /// type `vendor`, if the miner named one, from a connection in `country`.
    pub fn worker(
        &self,
        username: Username,
        pool: &Arc<Pool>,
        transport: Transport,
        vendor: Option<String>,
        country: Option<Country>,
    ) -> Worker {
        let handle = self.handles.handle(&pool.id, &username);
        let fleet = self
            .handles
            .fleet(&username.address, username.secret.as_ref());
        let private = username.secret.is_some();
        self.fleets.record(
            fleet,
            username.address.to_string(),
            private,
            Member {
                handle,
                pool: pool.id.clone(),
                worker: username.worker.clone(),
                first_seen: telemetry::unix(SystemTime::now()),
                vendor: vendor.clone(),
            },
        );
        let link = self.telemetry.connect(handle, pool.id.clone(), transport);
        // The whole address stays out of telemetry: the stats lists hold its shortened form only, and none
        // for a private fleet.
        self.telemetry.describe(
            handle,
            Listing {
                fleet,
                short_address: (!private).then(|| fleet::shortened(&username.address.to_string())),
                miner: vendor,
                country,
            },
        );
        let options = &username.options;
        self.telemetry.choose(
            handle,
            Choices {
                part: options.part,
                asked_name: options.name,
                name_written: !username.worker.is_empty() && pool.writes_name(options.name),
            },
        );
        Worker {
            link,
            handle,
            username,
            pool: pool.clone(),
            transport,
        }
    }

    /// The country of a connection from `peer`, looked up when it opens; the address itself is kept nowhere.
    pub fn country(&self, peer: SocketAddr) -> Option<Country> {
        self.geo.as_ref()?.country(peer.ip())
    }

    /// Records a share refused as `class` on a listener of `pool` over `transport`, on `worker` when the
    /// share was for it.
    pub fn rejected(
        &self,
        pool: &PoolId,
        transport: Transport,
        worker: Option<&Worker>,
        class: Reject,
    ) {
        metrics::share_rejected(pool, transport, class);
        if let Some(worker) = worker {
            worker.link.rejected(class);
        }
    }

    /// The share check both front-ends run once a share has matched a job and passed the protocol's own
    /// checks: hash the header, refuse a duplicate or a miss of `target`, and send a block to the submitter
    /// before anything else. `seen` holds the header hashes the session accepted since its last clean
    /// job; a share that passes joins it. It is not yet accepted: [`Context::credit`] does that.
    pub fn check_share(
        &self,
        worker: &Worker,
        seen: &mut HashSet<BlockHash>,
        job: &Job,
        submission: &Submission,
        mask: u32,
        target: Target,
    ) -> Result<Checked, Rejection> {
        let checked = share::check(&job.header(submission, mask), target);
        if seen.contains(&checked.hash) {
            self.rejected(
                &worker.pool.id,
                worker.transport,
                Some(worker),
                Reject::Duplicate,
            );
            return Err(Rejection::Duplicate);
        }
        if !checked.meets_target {
            self.rejected(
                &worker.pool.id,
                worker.transport,
                Some(worker),
                Reject::LowDifficulty,
            );
            return Err(Rejection::LowDifficulty);
        }
        seen.insert(checked.hash);
        if checked.is_block {
            // Once: a resent copy is refused above as a duplicate.
            let (finder_sats, pool_sats) = job.paid();
            let found = FoundBlock {
                hash: checked.hash,
                height: job.height,
                pool: job.pool.clone(),
                worker: worker.username.worker.clone(),
                transport: worker.transport,
                handle: worker.handle,
                finder_sats,
                pool_sats,
                hex: block::assemble(job, submission, mask),
            };
            tracing::info!(
                hash = %found.hash,
                height = found.height,
                pool = %found.pool,
                worker = %found.worker,
                handle = %found.handle,
                transport = %found.transport,
                "share is a block"
            );
            if self.blocks.send(found).is_err() {
                tracing::error!(hash = %checked.hash, "CRITICAL: block lost: the submitter stopped");
            }
        }
        Ok(checked)
    }

    /// Accepts a share that passed [`Context::check_share`] on job `job_id`, at `target`: at once when its job
    /// pays no pool output, so it earns from no window, and otherwise once its
    /// journal commit is written, so it is in the journal before the miner hears it was accepted.
    pub async fn credit(
        &self,
        worker: &Worker,
        job: &Job,
        job_id: u64,
        target: Target,
        checked: &Checked,
    ) -> Result<(), Rejection> {
        if job.paid().1 > Amount::ZERO {
            let entry = Entry {
                pool: worker.pool.id.clone(),
                address: worker.username.address.to_string(),
                handle: worker.handle,
                template: job.template,
                job: job_id,
                bits: job.bits,
                miner_sats: job.split.miner,
                pool_sats: job.paid().1,
                target,
                proof: checked.hash,
                accepted_at: telemetry::unix(SystemTime::now()),
                block: checked.is_block,
            };
            match self.journal.record(entry).await {
                Ok(_) => {}
                Err(journal::Refusal::Duplicate) => {
                    self.rejected(
                        &worker.pool.id,
                        worker.transport,
                        Some(worker),
                        Reject::Duplicate,
                    );
                    return Err(Rejection::Duplicate);
                }
                Err(journal::Refusal::Failed) => return Err(Rejection::Unavailable),
            }
        }
        // Credited at the target, never at the hash's own difficulty.
        let reached = target::difficulty(Target::from_le_bytes(checked.hash.to_byte_array()));
        worker.link.accepted(
            target::work(target),
            reached,
            checked.is_block.then_some(job.height),
        );
        metrics::share_accepted(&worker.pool.id, worker.transport);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transports_carry_the_c1_labels() {
        assert_eq!(v1::TRANSPORT.as_str(), "V1");
        assert_eq!(v2::TRANSPORT.as_str(), "V2-native");
    }
}
