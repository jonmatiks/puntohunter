//! A channel as the client sees it: the jobs, prevhash, and target the pool sent, and shares ground from
//! them. The header is built here from the messages alone, never from the pool's code.

use std::collections::HashMap;

use bitcoin::{
    BlockHash, CompactTarget, Target,
    hashes::{Hash, sha256d},
};
use mining_sv2::{
    NewExtendedMiningJobOwned, NewMiningJobOwned, SetNewPrevHashOwned, SubmitSharesExtendedOwned,
    SubmitSharesStandardOwned,
};
use parsers_sv2::MiningOwned;

/// Version bits outside BIP320's, which no channel may roll.
const OUTSIDE_BIP320: u32 = 1;
/// Hashes tried before a share is given up on.
const MAX_TRIES: u32 = 50_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Extended,
    Standard,
}

#[derive(Clone)]
enum Work {
    Extended {
        prefix: Vec<u8>,
        suffix: Vec<u8>,
        merkle_path: Vec<[u8; 32]>,
    },
    Standard {
        merkle_root: [u8; 32],
    },
}

#[derive(Clone)]
pub struct Job {
    pub id: u32,
    version: u32,
    work: Work,
    /// Set once the job is active: the prevhash, nbits, and ntime it is mined on.
    chain: Option<([u8; 32], u32, u32)>,
}

/// What a submit asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// Meets the target and is not a block.
    Valid,
    /// Meets the target and the network's.
    Block,
    /// Misses the target.
    LowDifficulty,
    /// Meets this easier target but misses the channel's, and is not a block.
    Between(Target),
    /// A valid share, with a version bit outside BIP320's.
    BadVersion,
    /// A valid share, with a 7-byte extranonce.
    BadExtranonce,
}

/// A share ready to send, and what the client knows of it.
pub struct Ground {
    pub message: MiningOwned,
    pub hash: BlockHash,
}

pub struct Channel {
    pub id: u32,
    pub kind: Kind,
    pub target: Target,
    pub extranonce_prefix: Vec<u8>,
    pub extranonce_size: usize,
    jobs: HashMap<u32, Job>,
    /// The newest active job.
    pub active: Option<u32>,
    prevhash: Option<([u8; 32], u32, u32)>,
    pub next_sequence: u32,
    extranonce_counter: u64,
    nonce_cursor: u32,
    /// The last share sent, for resending as a duplicate.
    pub last: Option<MiningOwned>,
}

impl Channel {
    pub fn new(
        id: u32,
        kind: Kind,
        target: Target,
        extranonce_prefix: Vec<u8>,
        extranonce_size: usize,
    ) -> Self {
        Channel {
            id,
            kind,
            target,
            extranonce_prefix,
            extranonce_size,
            jobs: HashMap::new(),
            active: None,
            prevhash: None,
            next_sequence: 0,
            extranonce_counter: 0,
            nonce_cursor: 0,
            last: None,
        }
    }

    pub fn on_extended_job(&mut self, job: &NewExtendedMiningJobOwned) {
        let work = Work::Extended {
            prefix: job.coinbase_tx_prefix.as_bytes().to_vec(),
            suffix: job.coinbase_tx_suffix.as_bytes().to_vec(),
            merkle_path: job
                .merkle_path
                .clone()
                .into_inner()
                .iter()
                .map(|hash| hash.to_array())
                .collect(),
        };
        self.on_job(
            job.job_id,
            job.version,
            job.min_ntime.as_ref().copied(),
            work,
        );
    }

    pub fn on_standard_job(&mut self, job: &NewMiningJobOwned) {
        let work = Work::Standard {
            merkle_root: job.merkle_root.to_array(),
        };
        self.on_job(
            job.job_id,
            job.version,
            job.min_ntime.as_ref().copied(),
            work,
        );
    }

    fn on_job(&mut self, id: u32, version: u32, min_ntime: Option<u32>, work: Work) {
        let mut job = Job {
            id,
            version,
            work,
            chain: None,
        };
        // A job with min_ntime is active at once on the current prevhash; one without waits for its own.
        if let (Some(ntime), Some((prevhash, nbits, _))) = (min_ntime, self.prevhash) {
            job.chain = Some((prevhash, nbits, ntime));
            self.active = Some(id);
        }
        self.jobs.insert(id, job);
    }

    pub fn on_prevhash(&mut self, message: &SetNewPrevHashOwned) {
        let chain = (
            message.prev_hash.to_array(),
            message.nbits,
            message.min_ntime,
        );
        self.prevhash = Some(chain);
        if let Some(job) = self.jobs.get_mut(&message.job_id) {
            job.chain = Some(chain);
            self.active = Some(message.job_id);
        }
    }

    pub fn job(&self, id: u32) -> Option<&Job> {
        self.jobs.get(&id)
    }

    /// The header fields a job yields, as JSON.
    pub fn describe(&self, job: &Job) -> serde_json::Value {
        let (prevhash, nbits, ntime) = job.chain.unwrap_or_default();
        let mut value = serde_json::json!({
            "job_id": job.id,
            "version": job.version,
            "active": job.chain.is_some(),
            "prev_hash": hex(&prevhash),
            "nbits": nbits,
            "ntime": ntime,
        });
        match &job.work {
            Work::Extended {
                prefix,
                suffix,
                merkle_path,
            } => {
                value["coinbase_prefix"] = hex(prefix).into();
                value["coinbase_suffix"] = hex(suffix).into();
                value["merkle_path"] = merkle_path.iter().map(|h| hex(h)).collect();
            }
            Work::Standard { merkle_root } => value["merkle_root"] = hex(merkle_root).into(),
        }
        value
    }

    /// Grinds a share on job `id` (the active one if none) that is what `wanted` asks for.
    pub fn grind(&mut self, id: Option<u32>, wanted: Wanted) -> Result<Ground, String> {
        let id = id.or(self.active).ok_or("no active job")?;
        let job = self.jobs.get(&id).ok_or(format!("no job {id}"))?.clone();
        let (prevhash, nbits, ntime) = job.chain.ok_or(format!("job {id} is not active"))?;
        let network = Target::from_compact(CompactTarget::from_consensus(nbits));
        if matches!(
            wanted,
            Wanted::Valid | Wanted::BadVersion | Wanted::BadExtranonce
        ) && self.target <= network
        {
            return Err("every share at this target is a block".into());
        }
        self.extranonce_counter += 1;
        let miner = self.extranonce_counter.to_be_bytes();
        let root = match &job.work {
            Work::Extended {
                prefix,
                suffix,
                merkle_path,
            } => {
                let coinbase = [
                    &prefix[..],
                    &self.extranonce_prefix,
                    &miner[..self.extranonce_size.min(8)],
                    &vec![0; self.extranonce_size.saturating_sub(8)],
                    suffix,
                ]
                .concat();
                let mut root = sha256d::Hash::hash(&coinbase).to_byte_array();
                for sibling in merkle_path {
                    root = sha256d::Hash::hash(&[root, *sibling].concat()).to_byte_array();
                }
                root
            }
            Work::Standard { merkle_root } => *merkle_root,
        };
        let version = match wanted {
            Wanted::BadVersion => job.version ^ OUTSIDE_BIP320,
            _ => job.version,
        };
        let meets = |hash: BlockHash| match wanted {
            Wanted::Valid | Wanted::BadVersion | Wanted::BadExtranonce => {
                self.target.is_met_by(hash) && !network.is_met_by(hash)
            }
            Wanted::Block => self.target.is_met_by(hash) && network.is_met_by(hash),
            Wanted::LowDifficulty => !self.target.is_met_by(hash),
            Wanted::Between(easier) => {
                easier.is_met_by(hash) && !self.target.is_met_by(hash) && !network.is_met_by(hash)
            }
        };
        for _ in 0..MAX_TRIES {
            let nonce = self.nonce_cursor;
            self.nonce_cursor = self.nonce_cursor.wrapping_add(1);
            let header = [
                &version.to_le_bytes()[..],
                &prevhash,
                &root,
                &ntime.to_le_bytes(),
                &nbits.to_le_bytes(),
                &nonce.to_le_bytes(),
            ]
            .concat();
            let hash = BlockHash::from_raw_hash(sha256d::Hash::hash(&header));
            if !meets(hash) {
                continue;
            }
            let sequence_number = self.next_sequence;
            self.next_sequence += 1;
            let message = match self.kind {
                Kind::Extended => {
                    let mut extranonce = miner[..self.extranonce_size.min(8)].to_vec();
                    extranonce.resize(self.extranonce_size, 0);
                    if wanted == Wanted::BadExtranonce {
                        extranonce.pop();
                    }
                    MiningOwned::SubmitSharesExtended(SubmitSharesExtendedOwned {
                        channel_id: self.id,
                        sequence_number,
                        job_id: id,
                        nonce,
                        ntime,
                        version,
                        extranonce: binary_sv2::B032Owned::new(extranonce).unwrap(),
                    })
                }
                Kind::Standard => MiningOwned::SubmitSharesStandard(SubmitSharesStandardOwned {
                    channel_id: self.id,
                    sequence_number,
                    job_id: id,
                    nonce,
                    ntime,
                    version,
                }),
            };
            return Ok(Ground { message, hash });
        }
        Err(format!("no share after {MAX_TRIES} hashes"))
    }

    /// The last share sent again, under the next sequence number.
    pub fn duplicate(&mut self) -> Result<MiningOwned, String> {
        let sequence_number = self.next_sequence;
        let mut message = self.last.clone().ok_or("no share sent yet")?;
        match &mut message {
            MiningOwned::SubmitSharesExtended(share) => share.sequence_number = sequence_number,
            MiningOwned::SubmitSharesStandard(share) => share.sequence_number = sequence_number,
            _ => unreachable!(),
        }
        self.next_sequence += 1;
        Ok(message)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A target as big-endian hex, the way it reads as a number.
pub fn target_hex(target: Target) -> String {
    hex(&target.to_be_bytes())
}
