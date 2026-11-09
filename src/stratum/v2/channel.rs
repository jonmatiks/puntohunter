//! One SV2 mining channel on the shared core: its extranonce prefix from the V1 allocator, its jobs
//! from the shared JobCache, and its shares through the shared check. It knows nothing of sockets; each call
//! returns the messages to send, in order.
//!
//! Each job records the target assigned with it, as a V1 session's jobs do. A share is checked against the
//! easier of its job's target and the channel's current one, and credited at that target. After a raise, a
//! miner that filters shares by the target it held when the job arrived (ESP-Miner does) still has its
//! in-flight shares accepted; after a drop, a miner that applies SetTarget at once is honoured on its next
//! share. A retarget sends SetTarget followed by a job carrying the new target, as V1 sends
//! set_difficulty before a notify.

use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Instant,
};

use binary_sv2::{B064KOwned, Seq0255Owned, Sv2OptionOwned, U256Owned};
use bitcoin::{BlockHash, ScriptBuf, Target, hashes::Hash};
use mining_sv2::{
    ERROR_CODE_OPEN_MINING_CHANNEL_INVALID_NOMINAL_HASHRATE,
    ERROR_CODE_OPEN_MINING_CHANNEL_MAX_TARGET_OUT_OF_RANGE,
    ERROR_CODE_OPEN_MINING_CHANNEL_MIN_EXTRANONCE_SIZE_TOO_LARGE,
    ERROR_CODE_OPEN_MINING_CHANNEL_UNKNOWN_USER, ERROR_CODE_SUBMIT_SHARES_BAD_EXTRANONCE_SIZE,
    ERROR_CODE_SUBMIT_SHARES_DIFFICULTY_TOO_LOW, ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE,
    ERROR_CODE_SUBMIT_SHARES_INVALID_JOB_ID,
    ERROR_CODE_SUBMIT_SHARES_INVALID_NON_ROLLABLE_VERSION_BIT,
    ERROR_CODE_SUBMIT_SHARES_STALE_SHARE, NewExtendedMiningJobOwned, NewMiningJobOwned,
    OpenExtendedMiningChannelSuccessOwned, OpenStandardMiningChannelSuccessOwned,
    SetNewPrevHashOwned, SetTargetOwned, SubmitSharesSuccessOwned,
};
use parsers_sv2::MiningOwned;

use super::{TRANSPORT, message};
use crate::{
    config,
    geo::Country,
    mining::{
        coinbase::{EXTRANONCE_LEN, EXTRANONCE_PREFIX_LEN, MINER_EXTRANONCE_LEN},
        extranonce::ExtranoncePrefix,
        job::{Job, Submission},
        merkle,
        pool::Pool,
        share::Checked,
        target,
        vardiff::Vardiff,
    },
    node::template::{BIP320_MASK, Template},
    stratum::{Context, Rejection, Worker, username},
    telemetry::Reject,
};

/// Jobs kept per channel. A new prevhash drops them all; refreshes and retargets add to them.
const MAX_JOBS: usize = 16;
/// Version bits a miner may roll: BIP320's, as V1 grants.
const VERSION_MASK: u32 = BIP320_MASK as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The miner rolls 8 extranonce bytes after the pool's 4-byte prefix.
    Extended,
    /// Header-only: the pool fills all 12 extranonce bytes and sends the channel's own merkle root.
    Standard,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Extended => "extended",
            Kind::Standard => "standard",
        }
    }
}

/// An OpenStandardMiningChannel or OpenExtendedMiningChannel.
#[derive(Debug, Clone)]
pub struct Open {
    pub kind: Kind,
    pub request_id: u32,
    pub user_identity: String,
    pub nominal_hash_rate: f32,
    pub max_target: Target,
    /// Extended channels only.
    pub min_extranonce_size: u16,
    /// The miner type the connection's SetupConnection named.
    pub vendor: Option<String>,
    /// The connection's country.
    pub country: Option<Country>,
}

/// A SubmitSharesStandard or SubmitSharesExtended.
#[derive(Debug, Clone)]
pub struct Share {
    pub sequence_number: u32,
    pub job_id: u32,
    pub nonce: u32,
    pub ntime: u32,
    pub version: u32,
    /// The miner's extranonce: extended channels only.
    pub extranonce: Vec<u8>,
}

/// Why a channel open was refused: the SV2 error code and a reason for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub error_code: &'static str,
    pub reason: String,
}

/// A job as this channel issued it, with the target assigned alongside it.
struct IssuedJob {
    id: u32,
    job: Arc<Job>,
    target: Target,
    difficulty: f64,
}

/// A share that passed the channel's checks and the shared one, to credit before it joins the batch.
pub struct Passed {
    pub sequence_number: u32,
    pub job: Arc<Job>,
    pub job_id: u32,
    /// The target it is credited at, and its difficulty.
    pub target: Target,
    pub difficulty: f64,
    pub checked: Checked,
    /// Whether vardiff changed the target.
    pub retarget: bool,
}

/// Accepted shares not yet acknowledged.
#[derive(Default)]
struct Batch {
    last_sequence_number: u32,
    accepted: u32,
    difficulty: f64,
    /// When each was read.
    read: Vec<Instant>,
}

pub struct Channel {
    pub id: u32,
    pub kind: Kind,
    pub worker: Worker,
    extranonce: ExtranoncePrefix,
    /// The script the finder's part of this channel's jobs pays.
    miner: ScriptBuf,
    vardiff: Vardiff,
    max_target: Target,
    jobs: VecDeque<IssuedJob>,
    /// The last job id issued; ids count up from 1.
    last_job_id: u32,
    /// The prevhash the last SetNewPrevHash activated.
    prevhash: Option<BlockHash>,
    /// The template of the last job issued, so each template is issued once.
    template_seq: Option<u64>,
    /// The ntime of the last job. Each job announces a later one, so a job issued again on the same template
    /// gives the miner fresh headers rather than the ones it already searched.
    last_ntime: u32,
    /// Header hashes accepted since the last new prevhash, whichever job they arrived under.
    shares: HashSet<BlockHash>,
    batch: Batch,
}

impl Channel {
    /// Opens channel `id` for `open` in `pool`, answering with its Success message, or refuses it.
    pub fn open(
        context: &Context,
        pool: &Arc<Pool>,
        id: u32,
        open: &Open,
        now: Instant,
    ) -> Result<(Channel, MiningOwned), Refusal> {
        let refuse = |error_code, reason: String| Refusal { error_code, reason };
        let username = username::parse(&open.user_identity, context.network)
            .map_err(|e| refuse(ERROR_CODE_OPEN_MINING_CHANNEL_UNKNOWN_USER, e.to_string()))?;
        if open.kind == Kind::Extended
            && usize::from(open.min_extranonce_size) > MINER_EXTRANONCE_LEN
        {
            return Err(refuse(
                ERROR_CODE_OPEN_MINING_CHANNEL_MIN_EXTRANONCE_SIZE_TOO_LARGE,
                format!(
                    "{} extranonce bytes asked for, {MINER_EXTRANONCE_LEN} offered",
                    open.min_extranonce_size
                ),
            ));
        }
        let bounds = bounds(&context.vardiff, open.max_target).map_err(|reason| {
            refuse(
                ERROR_CODE_OPEN_MINING_CHANNEL_MAX_TARGET_OUT_OF_RANGE,
                reason,
            )
        })?;
        let hint = hint(open.nominal_hash_rate, &context.vardiff).map_err(|reason| {
            refuse(
                ERROR_CODE_OPEN_MINING_CHANNEL_INVALID_NOMINAL_HASHRATE,
                reason,
            )
        })?;
        // A job pays its finder's part of the miner portion to the worker's address, and the pool wallet the
        // rest; in a pool that cannot pay a finder, all of it to the pool wallet, so its channels share jobs.
        let miner = if pool.can_pay_finder() {
            username.address.script_pubkey()
        } else {
            context.pool_wallet.clone()
        };
        let channel = Channel {
            id,
            kind: open.kind,
            worker: context.worker(
                username,
                pool,
                TRANSPORT,
                open.vendor.clone(),
                open.country.clone(),
            ),
            extranonce: context.extranonces.allocate(),
            miner,
            vardiff: Vardiff::new(&bounds, hint, now),
            max_target: open.max_target,
            jobs: VecDeque::new(),
            last_job_id: 0,
            prevhash: None,
            template_seq: None,
            last_ntime: 0,
            shares: HashSet::new(),
            batch: Batch::default(),
        };
        let target = message::u256(channel.assigned().0);
        let success = match open.kind {
            Kind::Extended => MiningOwned::OpenExtendedMiningChannelSuccess(
                OpenExtendedMiningChannelSuccessOwned {
                    request_id: open.request_id,
                    channel_id: id,
                    target,
                    extranonce_size: MINER_EXTRANONCE_LEN as u16,
                    extranonce_prefix: message::b032(&channel.extranonce.bytes()),
                    group_channel_id: 0,
                },
            ),
            Kind::Standard => MiningOwned::OpenStandardMiningChannelSuccess(
                OpenStandardMiningChannelSuccessOwned {
                    request_id: open.request_id,
                    channel_id: id,
                    target,
                    extranonce_prefix: message::b032(&channel.full_extranonce(&[])),
                    group_channel_id: 0,
                },
            ),
        };
        Ok((channel, success))
    }

    /// The target new work is assigned, and its difficulty: vardiff's, never easier than the miner's
    /// max_target.
    fn assigned(&self) -> (Target, f64) {
        let target = self.vardiff.target();
        if target > self.max_target {
            (self.max_target, target::difficulty(self.max_target))
        } else {
            (target, self.vardiff.difficulty())
        }
    }

    /// The coinbase extranonce: the pool's prefix, then the miner's bytes on an extended channel or zeros
    /// on a standard one.
    fn full_extranonce(&self, miner: &[u8]) -> [u8; EXTRANONCE_LEN] {
        let mut extranonce = [0; EXTRANONCE_LEN];
        extranonce[..EXTRANONCE_PREFIX_LEN].copy_from_slice(&self.extranonce.bytes());
        extranonce[EXTRANONCE_PREFIX_LEN..EXTRANONCE_PREFIX_LEN + miner.len()]
            .copy_from_slice(miner);
        extranonce
    }

    /// Issues a job on `template`, once per template unless `retarget` asks for one at a new target. A new
    /// prevhash sends the job as a future job and activates it with SetNewPrevHash, dropping the older jobs;
    /// otherwise the job is active at once on the current prevhash. `now` is the current Unix time.
    pub fn issue(
        &mut self,
        context: &Context,
        template: &Template,
        now: u32,
        retarget: bool,
    ) -> Vec<MiningOwned> {
        if !retarget && self.template_seq == Some(template.seq) {
            return Vec::new();
        }
        self.template_seq = Some(template.seq);
        let job = context.jobs.job(
            template,
            &self.worker.pool.id,
            &self.worker.username.worker,
            &self.miner,
            &self.worker.username.options,
        );
        let future = self.prevhash != Some(job.prevhash);
        if future {
            self.jobs.clear();
            self.shares.clear();
        }
        self.last_job_id += 1;
        let id = self.last_job_id;
        let ntime = job.time.max(now).max(self.last_ntime.saturating_add(1));
        self.last_ntime = ntime;
        let (target, difficulty) = self.assigned();
        self.worker.link.target(difficulty);
        tracing::debug!(channel = self.id, job = id, difficulty, future, "V2 job");
        let min_ntime = Sv2OptionOwned::new((!future).then_some(ntime));
        let mut messages = vec![match self.kind {
            Kind::Extended => MiningOwned::NewExtendedMiningJob(NewExtendedMiningJobOwned {
                channel_id: self.id,
                job_id: id,
                min_ntime,
                version: job.version as u32,
                version_rolling_allowed: true,
                merkle_path: Seq0255Owned::new(
                    job.branch
                        .iter()
                        .map(|hash| U256Owned::from(hash.to_byte_array()))
                        .collect(),
                )
                .expect("a merkle branch is at most 255 hashes"),
                coinbase_tx_prefix: B064KOwned::new(job.coinbase.coinb1.clone())
                    .expect("a coinbase is under 64 KiB"),
                coinbase_tx_suffix: B064KOwned::new(job.coinbase.coinb2.clone())
                    .expect("a coinbase is under 64 KiB"),
            }),
            Kind::Standard => {
                let root = merkle::root(job.coinbase.txid(&self.full_extranonce(&[])), &job.branch);
                MiningOwned::NewMiningJob(NewMiningJobOwned {
                    channel_id: self.id,
                    job_id: id,
                    min_ntime,
                    version: job.version as u32,
                    merkle_root: U256Owned::from(root.to_byte_array()),
                })
            }
        }];
        if future {
            self.prevhash = Some(job.prevhash);
            messages.push(MiningOwned::SetNewPrevHash(SetNewPrevHashOwned {
                channel_id: self.id,
                job_id: id,
                prev_hash: U256Owned::from(job.prevhash.to_byte_array()),
                min_ntime: ntime,
                nbits: job.bits.to_consensus(),
            }));
        }
        if self.jobs.len() == MAX_JOBS {
            self.jobs.pop_front();
        }
        self.jobs.push_back(IssuedJob {
            id,
            job,
            target,
            difficulty,
        });
        messages
    }

    /// The SetTarget announcing the current target.
    pub fn set_target(&self) -> MiningOwned {
        MiningOwned::SetTarget(SetTargetOwned {
            channel_id: self.id,
            maximum_target: message::u256(self.assigned().0),
        })
    }

    /// Checks `share` through the shared core. A share that passes is credited by the session, then joins
    /// the acknowledgement batch through [`Channel::accept`]; a rejection is its SV2 error code.
    pub fn submit(
        &mut self,
        context: &Context,
        share: &Share,
        now: Instant,
    ) -> Result<Passed, &'static str> {
        let result = self.check(context, share, now);
        // The shared check records its own refusals, a duplicate and a miss of the target.
        if let Err(error_code) = result
            && let Some(class) = class(error_code)
        {
            context.rejected(&self.worker.pool.id, TRANSPORT, Some(&self.worker), class);
        }
        result
    }

    fn check(
        &mut self,
        context: &Context,
        share: &Share,
        now: Instant,
    ) -> Result<Passed, &'static str> {
        let current = self.assigned();
        let Some(issued) = self.jobs.iter().find(|job| job.id == share.job_id) else {
            return Err(if (1..=self.last_job_id).contains(&share.job_id) {
                ERROR_CODE_SUBMIT_SHARES_STALE_SHARE
            } else {
                ERROR_CODE_SUBMIT_SHARES_INVALID_JOB_ID
            });
        };
        let extranonce = match self.kind {
            Kind::Extended if share.extranonce.len() != MINER_EXTRANONCE_LEN => {
                return Err(ERROR_CODE_SUBMIT_SHARES_BAD_EXTRANONCE_SIZE);
            }
            Kind::Extended => self.full_extranonce(&share.extranonce),
            Kind::Standard => self.full_extranonce(&[]),
        };
        if (share.version ^ issued.job.version as u32) & !VERSION_MASK != 0 {
            return Err(ERROR_CODE_SUBMIT_SHARES_INVALID_NON_ROLLABLE_VERSION_BIT);
        }
        let submission = Submission {
            extranonce,
            time: share.ntime,
            nonce: share.nonce,
            version_bits: share.version,
        };
        let (target, difficulty) = if issued.target >= current.0 {
            (issued.target, issued.difficulty)
        } else {
            current
        };
        let job = issued.job.clone();
        let checked = context
            .check_share(
                &self.worker,
                &mut self.shares,
                &job,
                &submission,
                VERSION_MASK,
                target,
            )
            .map_err(|rejection| match rejection {
                Rejection::Duplicate => ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE,
                Rejection::LowDifficulty => ERROR_CODE_SUBMIT_SHARES_DIFFICULTY_TOO_LOW,
                Rejection::Unavailable => unreachable!("only crediting refuses so"),
            })?;
        Ok(Passed {
            sequence_number: share.sequence_number,
            job,
            job_id: share.job_id,
            target,
            difficulty,
            checked,
            retarget: self.vardiff.on_share(difficulty, now).is_some(),
        })
    }

    /// Adds a credited share, read at `read`, to the acknowledgement batch.
    pub fn accept(&mut self, passed: &Passed, read: Instant) {
        tracing::debug!(channel = self.id, hash = %passed.checked.hash, difficulty = passed.difficulty, "V2 share accepted");
        self.batch.last_sequence_number = passed.sequence_number;
        self.batch.accepted += 1;
        self.batch.difficulty += passed.difficulty;
        self.batch.read.push(read);
    }

    /// The SubmitSharesSuccess acknowledging every share accepted since the last one, if any were, with when
    /// each was read. Its new_shares_sum is the batch's summed difficulty, truncated, as SRI reports it.
    pub fn acknowledgement(&mut self) -> Option<(MiningOwned, Vec<Instant>)> {
        let batch = std::mem::take(&mut self.batch);
        (batch.accepted > 0).then_some((
            MiningOwned::SubmitSharesSuccess(SubmitSharesSuccessOwned {
                channel_id: self.id,
                last_sequence_number: batch.last_sequence_number,
                new_submits_accepted_count: batch.accepted,
                new_shares_sum: batch.difficulty as u64,
            }),
            batch.read,
        ))
    }

    /// Lets vardiff ease a miner that submits too rarely. Returns whether the target changed.
    pub fn on_tick(&mut self, now: Instant) -> bool {
        self.vardiff.on_tick(now).is_some()
    }

    /// Honours an UpdateChannel: re-seeds vardiff from the claimed hashrate within the new max_target.
    /// Returns whether the target changed, or the SV2 error code.
    pub fn update(
        &mut self,
        config: &config::Vardiff,
        nominal_hash_rate: f32,
        max_target: Target,
        now: Instant,
    ) -> Result<bool, &'static str> {
        let bounds = bounds(config, max_target)
            .map_err(|_| mining_sv2::ERROR_CODE_UPDATE_CHANNEL_MAX_TARGET_OUT_OF_RANGE)?;
        let hint = hint(nominal_hash_rate, config)
            .map_err(|_| mining_sv2::ERROR_CODE_UPDATE_CHANNEL_INVALID_NOMINAL_HASHRATE)?;
        let before = self.assigned().0;
        self.max_target = max_target;
        self.vardiff = Vardiff::new(&bounds, hint, now);
        Ok(self.assigned().0 != before)
    }
}

/// The class of a refusal the channel makes before the shared check; None for the check's own.
fn class(error_code: &str) -> Option<Reject> {
    match error_code {
        ERROR_CODE_SUBMIT_SHARES_STALE_SHARE => Some(Reject::Stale),
        ERROR_CODE_SUBMIT_SHARES_INVALID_NON_ROLLABLE_VERSION_BIT => Some(Reject::BadVersion),
        ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE | ERROR_CODE_SUBMIT_SHARES_DIFFICULTY_TOO_LOW => {
            None
        }
        _ => Some(Reject::Invalid),
    }
}

/// The configured vardiff bounds with the floor raised to the difficulty of the miner's max_target, the
/// easiest target it takes; or why no difficulty is left between the two.
fn bounds(config: &config::Vardiff, max_target: Target) -> Result<config::Vardiff, String> {
    let easiest = if max_target == Target::ZERO {
        f64::INFINITY
    } else {
        target::difficulty(max_target)
    };
    let floor = config.floor.max(easiest);
    if floor > config.ceiling {
        return Err(format!(
            "max_target asks for difficulty {easiest} or more, above the ceiling {}",
            config.ceiling
        ));
    }
    Ok(config::Vardiff {
        floor,
        ceiling: config.ceiling,
        share_interval: config.share_interval,
    })
}

/// The starting difficulty a claimed hashrate asks for: one share per interval. None when the miner claims
/// nothing, so vardiff starts at the floor.
fn hint(nominal_hash_rate: f32, config: &config::Vardiff) -> Result<Option<f64>, String> {
    let rate = f64::from(nominal_hash_rate);
    if !rate.is_finite() || rate < 0.0 {
        return Err(format!("nominal hashrate {nominal_hash_rate}"));
    }
    Ok((rate > 0.0).then(|| rate * config.share_interval.as_secs_f64() / 2f64.powi(32)))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bitcoin::block::Header;
    use tokio::sync::{mpsc, watch};

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{
        config::Config,
        mining::{extranonce::ExtranonceAllocator, job::JobCache, share},
        node::{
            BlockTemplate,
            recorded::{self, REGTEST_GETBLOCKTEMPLATE},
            template::Work,
        },
        pool::{blocks::FoundBlock, journal::Journal},
        store::{DATABASE, Store},
        telemetry::{Telemetry, fleet::Fleets, handle::HandleKey},
    };

    /// Below regtest's network difficulty, so shares take a few hashes and need not be blocks.
    const PINNED: f64 = 3e-10;

    struct Fixture {
        context: Context,
        /// The regtest config's solo pool.
        solo: Arc<Pool>,
        template: Template,
        _blocks: mpsc::UnboundedReceiver<FoundBlock>,
    }

    /// A store of the fixture's own, for the fleet rows channels write.
    fn store() -> Store {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("puntohunter-channel-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join(DATABASE)).unwrap()
    }

    /// Submits `share` and credits it as the session does, returning what the session acts on: whether the
    /// target changed, or the error code.
    async fn submit(
        fixture: &Fixture,
        channel: &mut Channel,
        share: &Share,
        now: Instant,
    ) -> Result<bool, &'static str> {
        let passed = channel.submit(&fixture.context, share, now)?;
        fixture
            .context
            .credit(
                &channel.worker,
                &passed.job,
                passed.job_id.into(),
                passed.target,
                &passed.checked,
            )
            .await
            .map_err(|_| "refused by the journal")?;
        channel.accept(&passed, now);
        Ok(passed.retarget)
    }

    fn fixture(floor: f64, ceiling: f64) -> Fixture {
        let config: Config =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config/regtest.toml"))
                .unwrap()
                .parse()
                .unwrap();
        let (blocks, receiver) = mpsc::unbounded_channel();
        let (_, work) = watch::channel(Work::None);
        let store = store();
        let pools = config.pools.clone();
        let context = Context {
            network: config.network,
            vardiff: config::Vardiff {
                floor,
                ceiling,
                share_interval: Duration::from_secs(10),
            },
            extranonces: Arc::new(ExtranonceAllocator::default()),
            work,
            jobs: JobCache::new(
                &pools,
                &config.addresses.pool_wallet.script_pubkey(),
                &config.coinbase.tag,
            ),
            pool_wallet: config.addresses.pool_wallet.script_pubkey(),
            blocks,
            authorize_timeout: Duration::from_secs(60),
            handles: HandleKey::fixed(1),
            telemetry: Telemetry::new(60),
            fleets: Fleets::new(store.clone()),
            journal: Journal::spawn(
                store,
                pools.iter().map(|pool| (pool.id.clone(), 1)).collect(),
            ),
            geo: None,
        };
        let template = Template {
            seq: 1,
            clean: true,
            block: Arc::new(recorded::result(REGTEST_GETBLOCKTEMPLATE)),
        };
        Fixture {
            context,
            solo: pools[0].clone(),
            template,
            _blocks: receiver,
        }
    }

    /// A username on the test config's pool wallet address, standing in for a miner's.
    fn user(fixture: &Fixture) -> String {
        let context = &fixture.context;
        let address =
            bitcoin::Address::from_script(&context.pool_wallet, context.network.bitcoin()).unwrap();
        format!("{address}.rig")
    }

    fn open(kind: Kind, user_identity: String) -> Open {
        Open {
            kind,
            request_id: 7,
            user_identity,
            nominal_hash_rate: 0.0,
            max_target: Target::from_le_bytes([0xff; 32]),
            min_extranonce_size: 2,
            vendor: None,
            country: None,
        }
    }

    fn opened(fixture: &Fixture, kind: Kind) -> Channel {
        let (channel, _) = Channel::open(
            &fixture.context,
            &fixture.solo,
            1,
            &open(kind, user(fixture)),
            Instant::now(),
        )
        .unwrap();
        channel
    }

    /// A template on another tip: the next block.
    fn next_block(template: &Template) -> Template {
        let mut block: BlockTemplate = (*template.block).clone();
        block.previousblockhash = BlockHash::from_byte_array([7; 32]);
        block.height += 1;
        Template {
            seq: template.seq + 1,
            clean: true,
            block: Arc::new(block),
        }
    }

    fn refresh(template: &Template) -> Template {
        Template {
            seq: template.seq + 1,
            clean: false,
            block: template.block.clone(),
        }
    }

    /// A share on the channel's newest job whose hash meets `wanted`, for `extranonce`. Each call takes a
    /// fresh nonce range from `from`.
    fn grind(
        channel: &Channel,
        extranonce: [u8; MINER_EXTRANONCE_LEN],
        from: u32,
        wanted: impl Fn(&share::Checked) -> bool,
        target: Target,
    ) -> Share {
        let issued = channel.jobs.back().unwrap();
        let full = match channel.kind {
            Kind::Extended => channel.full_extranonce(&extranonce),
            Kind::Standard => channel.full_extranonce(&[]),
        };
        for nonce in from.. {
            let submission = Submission {
                extranonce: full,
                time: issued.job.time,
                nonce,
                version_bits: issued.job.version as u32,
            };
            let header: Header = issued.job.header(&submission, VERSION_MASK);
            if wanted(&share::check(&header, target)) {
                return Share {
                    sequence_number: nonce,
                    job_id: issued.id,
                    nonce,
                    ntime: issued.job.time,
                    version: issued.job.version as u32,
                    extranonce: match channel.kind {
                        Kind::Extended => extranonce.to_vec(),
                        Kind::Standard => Vec::new(),
                    },
                };
            }
        }
        unreachable!()
    }

    /// A share meeting the channel's target that is not a block.
    fn valid(channel: &Channel, from: u32) -> Share {
        let target = channel.assigned().0;
        grind(
            channel,
            [1; 8],
            from,
            |c| c.meets_target && !c.is_block,
            target,
        )
    }

    #[tokio::test]
    async fn sizes_the_first_target_from_the_claimed_rate_within_the_bounds() {
        let fixture = fixture(1.0, 1e6);
        let mut request = open(Kind::Extended, user(&fixture));
        // About a Bitaxe: 1.2 TH/s aims at difficulty 2794 for one share per 10 s.
        request.nominal_hash_rate = 1.2e12;
        let (channel, success) =
            Channel::open(&fixture.context, &fixture.solo, 1, &request, Instant::now()).unwrap();
        let expected = f64::from(request.nominal_hash_rate) * 10.0 / 2f64.powi(32);
        assert!((channel.assigned().1 - expected).abs() < 1e-6);
        let MiningOwned::OpenExtendedMiningChannelSuccess(success) = success else {
            panic!("{success:?}")
        };
        assert_eq!(
            message::target(&success.target),
            target::from_difficulty(expected)
        );
        assert_eq!(success.extranonce_size, 8);
        assert_eq!(
            success.extranonce_prefix.as_bytes(),
            channel.extranonce.bytes()
        );

        request.nominal_hash_rate = 1e20;
        let (channel, _) =
            Channel::open(&fixture.context, &fixture.solo, 2, &request, Instant::now()).unwrap();
        assert_eq!(channel.assigned().1, 1e6, "clamped to the ceiling");
    }

    #[tokio::test]
    async fn channels_take_distinct_prefixes_from_the_shared_allocator() {
        let fixture = fixture(1.0, 1e6);
        let v1 = fixture.context.extranonces.allocate();
        let a = opened(&fixture, Kind::Extended);
        let b = opened(&fixture, Kind::Standard);
        let prefixes: HashSet<_> = [v1.bytes(), a.extranonce.bytes(), b.extranonce.bytes()].into();
        assert_eq!(prefixes.len(), 3);
    }

    #[tokio::test]
    async fn refuses_an_open_with_its_reason() {
        let fixture = fixture(1.0, 1e6);
        let refused = |request: Open| {
            Channel::open(&fixture.context, &fixture.solo, 1, &request, Instant::now())
                .err()
                .unwrap()
                .error_code
        };
        assert_eq!(
            refused(open(Kind::Extended, "worker1".into())),
            ERROR_CODE_OPEN_MINING_CHANNEL_UNKNOWN_USER
        );
        let mut request = open(Kind::Extended, user(&fixture));
        request.min_extranonce_size = 9;
        assert_eq!(
            refused(request),
            ERROR_CODE_OPEN_MINING_CHANNEL_MIN_EXTRANONCE_SIZE_TOO_LARGE
        );
        let mut request = open(Kind::Standard, user(&fixture));
        request.max_target = target::from_difficulty(1e7);
        assert_eq!(
            refused(request),
            ERROR_CODE_OPEN_MINING_CHANNEL_MAX_TARGET_OUT_OF_RANGE
        );
        let mut request = open(Kind::Standard, user(&fixture));
        request.nominal_hash_rate = f32::NAN;
        assert_eq!(
            refused(request),
            ERROR_CODE_OPEN_MINING_CHANNEL_INVALID_NOMINAL_HASHRATE
        );
    }

    #[tokio::test]
    async fn never_assigns_a_target_easier_than_max_target() {
        let fixture = fixture(1.0, 1e6);
        let mut request = open(Kind::Extended, user(&fixture));
        request.max_target = target::from_difficulty(512.0);
        let (channel, _) =
            Channel::open(&fixture.context, &fixture.solo, 1, &request, Instant::now()).unwrap();
        assert!(channel.assigned().0 <= request.max_target);
        assert!((channel.assigned().1 - 512.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn a_new_prevhash_sends_a_future_job_then_activates_it() {
        let fixture = fixture(PINNED, PINNED);
        let mut channel = opened(&fixture, Kind::Extended);
        let messages = channel.issue(&fixture.context, &fixture.template, 0, false);
        let [
            MiningOwned::NewExtendedMiningJob(job),
            MiningOwned::SetNewPrevHash(prevhash),
        ] = &messages[..]
        else {
            panic!("{messages:?}")
        };
        assert_eq!(job.min_ntime.clone().into_inner(), None, "a future job");
        assert_eq!(prevhash.job_id, job.job_id);
        assert_eq!(
            prevhash.prev_hash.to_array(),
            fixture.template.block.previousblockhash.to_byte_array()
        );
        assert_eq!(prevhash.nbits, fixture.template.block.bits.to_consensus());
        assert!(prevhash.min_ntime >= fixture.template.block.curtime);

        // The same coinbase a V1 session paying the same script gets.
        let v1 = fixture.context.jobs.job(
            &fixture.template,
            &fixture.solo.id,
            &channel.worker.username.worker,
            &channel.miner,
            &Default::default(),
        );
        assert_eq!(job.coinbase_tx_prefix.as_bytes(), v1.coinbase.coinb1);
        assert_eq!(job.coinbase_tx_suffix.as_bytes(), v1.coinbase.coinb2);
        assert_eq!(job.version, v1.version as u32);
        assert!(job.version_rolling_allowed);

        assert!(
            channel
                .issue(&fixture.context, &fixture.template, 0, false)
                .is_empty(),
            "each template once"
        );
    }

    #[tokio::test]
    async fn a_refresh_is_a_job_on_the_current_prevhash() {
        let fixture = fixture(PINNED, PINNED);
        let mut channel = opened(&fixture, Kind::Extended);
        channel.issue(&fixture.context, &fixture.template, 0, false);
        let first_ntime = channel.last_ntime;
        let messages = channel.issue(&fixture.context, &refresh(&fixture.template), 0, false);
        let [MiningOwned::NewExtendedMiningJob(job)] = &messages[..] else {
            panic!("{messages:?}")
        };
        assert!(
            job.min_ntime.clone().into_inner().unwrap() > first_ntime,
            "active at once, with a later ntime"
        );
        assert_eq!(channel.jobs.len(), 2);
    }

    #[tokio::test]
    async fn a_standard_job_carries_the_channels_own_merkle_root() {
        let fixture = fixture(PINNED, PINNED);
        let mut channel = opened(&fixture, Kind::Standard);
        let messages = channel.issue(&fixture.context, &fixture.template, 0, false);
        let [
            MiningOwned::NewMiningJob(job),
            MiningOwned::SetNewPrevHash(_),
        ] = &messages[..]
        else {
            panic!("{messages:?}")
        };
        let issued = channel.jobs.back().unwrap();
        let extranonce = channel.full_extranonce(&[]);
        assert_eq!(extranonce[..4], channel.extranonce.bytes());
        assert_eq!(extranonce[4..], [0; 8], "the pool fills all 12 bytes");
        let root = merkle::root(issued.job.coinbase.txid(&extranonce), &issued.job.branch);
        assert_eq!(job.merkle_root.to_array(), root.to_byte_array());
    }

    #[tokio::test]
    async fn accepted_shares_batch_into_one_acknowledgement() {
        let fixture = fixture(PINNED, PINNED);
        for kind in [Kind::Extended, Kind::Standard] {
            let mut channel = opened(&fixture, kind);
            channel.issue(&fixture.context, &fixture.template, 0, false);
            let mut from = 0;
            let mut last = 0;
            for _ in 0..5 {
                let share = valid(&channel, from);
                from = share.nonce + 1;
                last = share.sequence_number;
                assert_eq!(
                    submit(&fixture, &mut channel, &share, Instant::now()).await,
                    Ok(false)
                );
            }
            let Some((MiningOwned::SubmitSharesSuccess(ack), read)) = channel.acknowledgement()
            else {
                panic!("an acknowledgement")
            };
            assert_eq!(
                (ack.last_sequence_number, ack.new_submits_accepted_count),
                (last, 5)
            );
            assert_eq!(read.len(), 5, "when each share was read");
            assert_eq!(ack.new_shares_sum, (5.0 * PINNED) as u64);
            assert!(
                channel.acknowledgement().is_none(),
                "nothing left to acknowledge"
            );
        }
    }

    #[tokio::test]
    async fn each_rejection_has_its_own_error_code() {
        let fixture = fixture(PINNED, PINNED);
        let mut channel = opened(&fixture, Kind::Extended);
        channel.issue(&fixture.context, &fixture.template, 0, false);
        let now = Instant::now();

        let share = valid(&channel, 0);
        assert_eq!(submit(&fixture, &mut channel, &share, now).await, Ok(false));
        let mut again = share.clone();
        again.sequence_number += 1000;
        assert_eq!(
            submit(&fixture, &mut channel, &again, now).await,
            Err(ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE)
        );

        let target = channel.assigned().0;
        let low = grind(&channel, [2; 8], 0, |c| !c.meets_target, target);
        assert_eq!(
            submit(&fixture, &mut channel, &low, now).await,
            Err(ERROR_CODE_SUBMIT_SHARES_DIFFICULTY_TOO_LOW)
        );

        let mut outside = valid(&channel, share.nonce + 1);
        outside.version ^= 1;
        assert_eq!(
            submit(&fixture, &mut channel, &outside, now).await,
            Err(ERROR_CODE_SUBMIT_SHARES_INVALID_NON_ROLLABLE_VERSION_BIT)
        );

        let mut short = valid(&channel, share.nonce + 1);
        short.extranonce.pop();
        assert_eq!(
            submit(&fixture, &mut channel, &short, now).await,
            Err(ERROR_CODE_SUBMIT_SHARES_BAD_EXTRANONCE_SIZE)
        );

        let mut unknown = valid(&channel, share.nonce + 1);
        unknown.job_id = 99;
        assert_eq!(
            submit(&fixture, &mut channel, &unknown, now).await,
            Err(ERROR_CODE_SUBMIT_SHARES_INVALID_JOB_ID)
        );

        let stale = valid(&channel, share.nonce + 1);
        channel.issue(&fixture.context, &next_block(&fixture.template), 0, false);
        assert_eq!(
            submit(&fixture, &mut channel, &stale, now).await,
            Err(ERROR_CODE_SUBMIT_SHARES_STALE_SHARE)
        );

        // Each refusal reaches telemetry once, whether the channel or the shared check made it.
        assert_eq!(
            fixture
                .context
                .telemetry
                .unflushed(channel.worker.handle, 0),
            (1, 6)
        );
    }

    #[tokio::test]
    async fn a_share_is_credited_at_the_easier_of_its_jobs_target_and_the_current_one() {
        let fixture = fixture(PINNED, 4.5e-10);
        let mut channel = opened(&fixture, Kind::Extended);
        channel.issue(&fixture.context, &fixture.template, 0, false);
        let (easy, _) = channel.assigned();

        // A raise: the old job keeps its easier target, so shares in flight on it still count ...
        channel.vardiff = Vardiff::new(&fixture.context.vardiff, Some(4.5e-10), Instant::now());
        let (hard, _) = channel.assigned();
        assert!(hard < easy);
        let in_flight = grind(
            &channel,
            [3; 8],
            0,
            |c| c.meets_target && !c.is_block && !hard.is_met_by(c.hash),
            easy,
        );
        assert!(
            submit(&fixture, &mut channel, &in_flight, Instant::now())
                .await
                .is_ok()
        );
        // ... while the job issued with the new target holds shares to it.
        channel.issue(&fixture.context, &fixture.template, 0, true);
        let only_easy = grind(
            &channel,
            [4; 8],
            0,
            |c| c.meets_target && !c.is_block && !hard.is_met_by(c.hash),
            easy,
        );
        assert_eq!(
            submit(&fixture, &mut channel, &only_easy, Instant::now()).await,
            Err(ERROR_CODE_SUBMIT_SHARES_DIFFICULTY_TOO_LOW)
        );

        // A drop applies at once, to the older jobs too.
        channel.vardiff = Vardiff::new(&fixture.context.vardiff, None, Instant::now());
        assert!(
            submit(&fixture, &mut channel, &only_easy, Instant::now())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn an_update_reseeds_the_target_within_the_new_max_target() {
        let fixture = fixture(1.0, 1e6);
        let mut channel = opened(&fixture, Kind::Extended);
        let now = Instant::now();
        let config = &fixture.context.vardiff;
        assert_eq!(
            channel.update(config, 1.2e12, channel.max_target, now),
            Ok(true)
        );
        let expected = f64::from(1.2e12_f32) * 10.0 / 2f64.powi(32);
        assert!((channel.assigned().1 - expected).abs() < 1e-6);
        assert_eq!(
            channel.update(config, 1.2e12, target::from_difficulty(1e4), now),
            Ok(true)
        );
        assert!(
            (channel.assigned().1 - 1e4).abs() < 1e-6,
            "raised to max_target"
        );
        assert_eq!(
            channel.update(config, 1.2e12, target::from_difficulty(1e7), now),
            Err(mining_sv2::ERROR_CODE_UPDATE_CHANNEL_MAX_TARGET_OUT_OF_RANGE)
        );
    }
}
