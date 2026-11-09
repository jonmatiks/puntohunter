//! A job: the work one template yields for one pool and miner payout script. It
//! rebuilds the header a submission claims, and serializes itself as a V1 mining.notify.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use bitcoin::{
    Amount, BlockHash, CompactTarget, Script, ScriptBuf,
    block::{Header, Version},
    hashes::{Hash, sha256d},
    hex::DisplayHex,
};
use serde_json::{Value, json};

use super::{
    coinbase::{self, CoinbaseParts, EXTRANONCE_LEN, Rewards, Signature, Split},
    merkle,
    pool::{Pool, PoolId},
};
use crate::{
    config::Portion,
    node::{BlockTemplate, template::Template},
    stratum::username::Options,
};

#[derive(Debug, Clone)]
pub struct Job {
    pub pool: PoolId,
    /// The sequence of the template it was built from, which the journal records with each share.
    pub template: u64,
    pub height: u32,
    pub prevhash: BlockHash,
    /// The template's version, normalized so no bit inside BIP320's mask is set unless required.
    pub version: i32,
    pub bits: CompactTarget,
    /// The template's curtime, the earliest ntime a notify may announce.
    pub time: u32,
    pub coinbase: CoinbaseParts,
    /// What the coinbase's reward outputs pay.
    pub split: Split,
    pub branch: Vec<sha256d::Hash>,
    pub transactions: Arc<Transactions>,
}

/// A template's transactions after the coinbase, serialized back to back as a block carries them. One
/// allocation per template, not one per transaction: each session keeps the jobs of up to 16 templates, and
/// thousands of small allocations freed on other threads fragment glibc's per-thread arenas until each holds
/// its own copy of that working set.
#[derive(Debug, Default)]
pub struct Transactions {
    pub count: usize,
    pub bytes: Vec<u8>,
}

impl<T: AsRef<[u8]>> FromIterator<T> for Transactions {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let all: Vec<T> = iter.into_iter().collect();
        let mut bytes = Vec::with_capacity(all.iter().map(|tx| tx.as_ref().len()).sum());
        for tx in &all {
            bytes.extend_from_slice(tx.as_ref());
        }
        Transactions {
            count: all.len(),
            bytes,
        }
    }
}

/// What every job on one template shares, whatever it pays.
#[derive(Debug, Clone, Default)]
pub struct Shared {
    pub branch: Vec<sha256d::Hash>,
    pub transactions: Arc<Transactions>,
}

impl Shared {
    pub fn new(block: &BlockTemplate) -> Self {
        let txids: Vec<_> = block.transactions.iter().map(|tx| tx.txid).collect();
        Shared {
            branch: merkle::branch(&txids),
            transactions: Arc::new(block.transactions.iter().map(|tx| &tx.data).collect()),
        }
    }
}

/// What a miner returns for a job.
#[derive(Debug, Clone, Copy)]
pub struct Submission {
    /// The pool's extranonce prefix followed by the miner's extranonce.
    pub extranonce: [u8; EXTRANONCE_LEN],
    pub time: u32,
    pub nonce: u32,
    /// The rolled version bits; only the bits inside the negotiated mask count.
    pub version_bits: u32,
}

/// BIP310's reconstruction: the job's bits outside the mask, the miner's inside it.
pub fn rolled_version(job_version: i32, version_bits: u32, mask: u32) -> i32 {
    ((job_version as u32 & !mask) | (version_bits & mask)) as i32
}

impl Job {
    /// The job `block` yields in `pool`, whose coinbases pay `rewards`, signed `signature`, for a finder's
    /// part, as the worker asked `part`, paying `miner`, given what the template's jobs share.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        block: &BlockTemplate,
        template: u64,
        pool: PoolId,
        signature: &str,
        miner: &Script,
        part: Option<Portion>,
        rewards: &Rewards,
        shared: &Shared,
    ) -> Self {
        let (split, outputs) = rewards.pay(Amount::from_sat(block.coinbasevalue), miner, part);
        Job {
            pool,
            template,
            height: block.height,
            prevhash: block.previousblockhash,
            version: block.version,
            bits: block.bits,
            time: block.curtime,
            coinbase: coinbase::build(
                block.height,
                signature,
                &outputs,
                block.default_witness_commitment.as_deref(),
            ),
            split,
            branch: shared.branch.clone(),
            transactions: shared.transactions.clone(),
        }
    }

    /// What a block found on this job pays its finder's address and the pool wallet.
    pub fn paid(&self) -> (Amount, Amount) {
        (self.split.finder, self.split.pool)
    }

    /// The header `submission` claims, with version bits rolled within `mask`.
    pub fn header(&self, submission: &Submission, mask: u32) -> Header {
        Header {
            version: Version::from_consensus(rolled_version(
                self.version,
                submission.version_bits,
                mask,
            )),
            prev_blockhash: self.prevhash,
            merkle_root: merkle::root(self.coinbase.txid(&submission.extranonce), &self.branch),
            time: submission.time,
            bits: self.bits,
            nonce: submission.nonce,
        }
    }

    /// The params of a V1 mining.notify announcing `time` as ntime.
    pub fn notify(&self, job_id: &str, time: u32, clean: bool) -> Value {
        json!([
            job_id,
            stratum_prevhash(self.prevhash),
            self.coinbase.coinb1.to_lower_hex_string(),
            self.coinbase.coinb2.to_lower_hex_string(),
            self.branch
                .iter()
                .map(|hash| hash.as_byte_array().to_lower_hex_string())
                .collect::<Vec<_>>(),
            format!("{:08x}", self.version as u32),
            format!("{:08x}", self.bits.to_consensus()),
            format!("{time:08x}"),
            clean,
        ])
    }
}

/// Jobs for the current template, one per pool, coinbase signature, miner payout script, and worker's choice,
/// shared by every session. A pool that can pay a finder builds each finder's own; one
/// that cannot pays the pool wallet alone, so its workers share one unless their names are written.
pub struct JobCache {
    /// What each configured pool's coinbases pay, and what its scriptSigs say.
    rewards: HashMap<PoolId, (Rewards, Signature)>,
    state: Mutex<CacheState>,
}

/// A job's key: its pool, signature, miner payout script, and the worker's own finder part as asked, None for
/// a worker that asked none or on a pool without `personal`, where a part changes nothing. A worker on the default so shares its address's jobs.
type Key = (PoolId, String, ScriptBuf, Option<Portion>);

#[derive(Default)]
struct CacheState {
    /// The template the cache holds jobs for.
    seq: u64,
    shared: Shared,
    jobs: HashMap<Key, Arc<Job>>,
}

impl JobCache {
    /// Jobs for `pools`, which pay what their finders leave to `pool_wallet`, signed with `tag`.
    pub fn new(pools: &[Arc<Pool>], pool_wallet: &ScriptBuf, tag: &str) -> Self {
        JobCache {
            rewards: pools
                .iter()
                .map(|pool| {
                    let rewards = Rewards::new(pool, pool_wallet.clone());
                    (pool.id.clone(), (rewards, Signature::new(tag, pool)))
                })
                .collect(),
            state: Mutex::default(),
        }
    }

    /// The job for `worker`, mining in `pool` with its `options`, for a finder's part paying `miner`.
    pub fn job(
        &self,
        template: &Template,
        pool: &PoolId,
        worker: &str,
        miner: &Script,
        options: &Options,
    ) -> Arc<Job> {
        let (rewards, signature) = self.rewards.get(pool).expect("a configured pool");
        let signature = signature.text(worker, options.name);
        let part = options.part.filter(|_| rewards.personal.is_some());
        let build = |shared: &Shared| {
            Arc::new(Job::new(
                &template.block,
                template.seq,
                pool.clone(),
                &signature,
                miner,
                part,
                rewards,
                shared,
            ))
        };
        let mut state = self.state.lock().unwrap();
        if template.seq < state.seq {
            // A session still catching up on an older template: build without evicting the newer one.
            return build(&Shared::new(&template.block));
        }
        if template.seq > state.seq {
            state.seq = template.seq;
            state.shared = Shared::new(&template.block);
            state.jobs.clear();
        }
        let CacheState { shared, jobs, .. } = &mut *state;
        jobs.entry((pool.clone(), signature.clone(), miner.to_owned(), part))
            .or_insert_with(|| build(shared))
            .clone()
    }
}

/// Stratum's prevhash: the header's bytes with each 4-byte word reversed.
fn stratum_prevhash(prevhash: BlockHash) -> String {
    let mut bytes = *prevhash.as_byte_array();
    for word in bytes.chunks_mut(4) {
        word.reverse();
    }
    bytes.to_lower_hex_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use bitcoin::{
        Transaction,
        consensus::{deserialize, serialize},
        hashes::HashEngine,
        hex::FromHex,
    };

    use super::*;
    use crate::{
        mining::{pool, recorded::MainnetBlock},
        node::{
            recorded::{self, REGTEST_GETBLOCKTEMPLATE},
            template::BIP320_MASK,
        },
    };

    const MASK: u32 = BIP320_MASK as u32;

    /// The XOR reading of BIP310 that some pools use.
    fn xor_version(job_version: i32, version_bits: u32) -> i32 {
        (job_version as u32 ^ version_bits) as i32
    }

    #[test]
    fn or_and_xor_readings_disagree_only_on_in_mask_job_bits() {
        let rolled = 0x0123_4000 & MASK;
        let clean_job = 0x2000_0000;
        assert_eq!(
            rolled_version(clean_job, rolled, MASK),
            xor_version(clean_job, rolled)
        );

        let job_with_bit_28 = 0x3000_0000;
        assert_ne!(
            rolled_version(job_with_bit_28, rolled, MASK),
            xor_version(job_with_bit_28, rolled)
        );
    }

    #[test]
    fn ignores_version_bits_outside_the_mask() {
        assert_eq!(rolled_version(0x2000_0000, 0xffff_ffff, 0), 0x2000_0000);
        assert_eq!(
            rolled_version(0x2000_0000, 0xffff_ffff, MASK),
            0x2000_0000 | MASK as i32
        );
    }

    /// Mainnet block 967926 as a job: its coinbase split around 12 bytes of its scriptSig, and its rolled
    /// version as a clean job version plus the miner's bits. Returns the job, the submission that found the
    /// block, and the block.
    pub fn mainnet_job() -> (Job, Submission, MainnetBlock) {
        let block = MainnetBlock::load();
        let mut coinbase: Transaction =
            deserialize(&Vec::<u8>::from_hex(&block.coinbase_hex).unwrap()).unwrap();
        coinbase.input[0].witness.clear();
        let bytes = serialize(&coinbase);
        // Version, input count, outpoint, scriptSig length: the scriptSig starts at byte 42.
        let split = 42 + 8;
        let job_version = block.version & !BIP320_MASK;
        let job = Job {
            pool: pool::tests::id("solo"),
            template: 0,
            height: block.height,
            prevhash: block.previousblockhash,
            version: job_version,
            bits: block.bits,
            time: block.time,
            coinbase: CoinbaseParts {
                coinb1: bytes[..split].to_vec(),
                coinb2: bytes[split + EXTRANONCE_LEN..].to_vec(),
                witness: true,
            },
            // The recording's one reward output: its finder's.
            split: Split {
                miner: coinbase.output[0].value,
                finder: coinbase.output[0].value,
                pool: Amount::ZERO,
                fees: Vec::new(),
            },
            branch: merkle::branch(&block.txids[1..]),
            // The recording keeps txids, not transactions.
            transactions: Arc::default(),
        };
        let submission = Submission {
            extranonce: bytes[split..split + EXTRANONCE_LEN].try_into().unwrap(),
            time: block.time,
            nonce: block.nonce,
            version_bits: block.version as u32 & MASK,
        };
        (job, submission, block)
    }

    /// The header a V1 miner builds from notify params, written from the miner's side.
    fn miner_header(notify: &Value, extranonce: &[u8], nonce: u32, version_bits: u32) -> Vec<u8> {
        let field = |i: usize| notify[i].as_str().unwrap();
        let hex = |s: &str| Vec::<u8>::from_hex(s).unwrap();
        let word = |s: &str| u32::from_str_radix(s, 16).unwrap();

        let coinbase = [hex(field(2)), extranonce.to_vec(), hex(field(3))].concat();
        let mut root = sha256d::Hash::hash(&coinbase).to_byte_array();
        for sibling in notify[4].as_array().unwrap() {
            let mut engine = sha256d::Hash::engine();
            engine.input(&root);
            engine.input(&hex(sibling.as_str().unwrap()));
            root = sha256d::Hash::from_engine(engine).to_byte_array();
        }
        let mut prevhash = hex(field(1));
        for w in prevhash.chunks_mut(4) {
            w.reverse();
        }
        let version = (word(field(5)) & !MASK) | (version_bits & MASK);
        [
            &version.to_le_bytes()[..],
            &prevhash,
            &root,
            &word(field(7)).to_le_bytes(),
            &word(field(6)).to_le_bytes(),
            &nonce.to_le_bytes(),
        ]
        .concat()
    }

    #[test]
    fn rebuilds_a_mainnet_header_from_its_notify_fields() {
        let (job, submission, block) = mainnet_job();

        let header = job.header(&submission, MASK);
        assert_eq!(header.merkle_root, block.merkleroot);
        assert_eq!(header.version.to_consensus(), block.version);
        assert_eq!(header.block_hash(), block.hash);

        let notify = job.notify("1", job.time, true);
        let bytes = miner_header(
            &notify,
            &submission.extranonce,
            submission.nonce,
            submission.version_bits,
        );
        assert_eq!(bytes, serialize(&header));
        assert_eq!(
            BlockHash::from_raw_hash(sha256d::Hash::hash(&bytes)),
            block.hash
        );
    }

    #[test]
    fn serializes_notify_fields() {
        let (job, _, block) = mainnet_job();
        let notify = job.notify("abc", block.time, false);
        assert_eq!(notify[0], "abc");
        assert_eq!(notify[5], format!("{:08x}", block.version as u32 & !MASK));
        assert_eq!(notify[6], "17021ec5");
        assert_eq!(notify[7], format!("{:08x}", block.time));
        assert_eq!(notify[8], false);
        // Stratum's word order ends with the display hash's leading zero words.
        assert!(notify[1].as_str().unwrap().ends_with("00000000"));
    }

    /// The three default pools, each writing every worker's name or each that asks as `worker_names` and
    /// `opt_in` say.
    fn named(worker_names: bool, opt_in: bool) -> Vec<Arc<Pool>> {
        pools()
            .into_iter()
            .map(|pool| {
                Arc::new(Pool {
                    worker_names,
                    worker_names_opt_in: opt_in,
                    ..(*pool).clone()
                })
            })
            .collect()
    }

    /// The three default pools.
    fn pools() -> Vec<Arc<Pool>> {
        [
            ("solo", Portion::Ppm(1_000_000)),
            ("pooled", Portion::Ppm(0)),
            ("hybrid", Portion::Ppm(333_300)),
        ]
        .into_iter()
        .map(|(id, finder)| Arc::new(pool::tests::paying(id, finder)))
        .collect()
    }

    #[test]
    fn a_personal_part_keys_jobs_of_its_own_and_pays_as_clamped() {
        let template = Template {
            seq: 1,
            clean: true,
            block: Arc::new(recorded::result(REGTEST_GETBLOCKTEMPLATE)),
        };
        let wallet = ScriptBuf::from(vec![0x51]);
        let finder = ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([1; 20]));
        let personal = Pool {
            personal: Some(pool::Personal {
                min: Portion::Ppm(0),
                max: Portion::Ppm(500_000),
            }),
            ..pool::tests::paying("split", Portion::Ppm(333_300))
        };
        let mut pools = pools();
        pools.push(Arc::new(personal));
        let cache = JobCache::new(&pools, &wallet, "/pool/");
        let (split, hybrid) = (pool::tests::id("split"), pool::tests::id("hybrid"));
        let reward = template.block.coinbasevalue;
        let part = |ppm| Options {
            part: Some(Portion::Ppm(ppm)),
            name: false,
        };

        // The default is shared by the address's workers; each part asked gets jobs of its own.
        let default = cache.job(&template, &split, "a", &finder, &Options::default());
        assert!(Arc::ptr_eq(
            &default,
            &cache.job(&template, &split, "b", &finder, &Options::default())
        ));
        let ten = cache.job(&template, &split, "a", &finder, &part(100_000));
        assert!(!Arc::ptr_eq(&default, &ten));
        assert!(Arc::ptr_eq(
            &ten,
            &cache.job(&template, &split, "b", &finder, &part(100_000))
        ));
        assert_eq!(
            default.paid().0,
            Amount::from_sat(reward * 333_300 / 1_000_000)
        );
        assert_eq!(ten.paid().0, Amount::from_sat(reward / 10));
        // Above max, clamped to it; at 0, the whole miner portion to the pool wallet.
        let all = cache.job(&template, &split, "a", &finder, &part(1_000_000));
        assert_eq!(
            all.paid(),
            (
                Amount::from_sat(reward / 2),
                Amount::from_sat(reward - reward / 2)
            )
        );
        let none = cache.job(&template, &split, "a", &finder, &part(0));
        assert_eq!(none.paid(), (Amount::ZERO, Amount::from_sat(reward)));

        // On a pool without personal, a part changes nothing: the pool's finder, and the shared job.
        let asked = cache.job(&template, &hybrid, "a", &finder, &part(100_000));
        assert!(Arc::ptr_eq(
            &asked,
            &cache.job(&template, &hybrid, "a", &finder, &Options::default())
        ));
    }

    #[test]
    fn pooled_workers_share_a_job_unless_each_is_named() {
        let template = Template {
            seq: 1,
            clean: true,
            block: Arc::new(recorded::result(REGTEST_GETBLOCKTEMPLATE)),
        };
        let pool = ScriptBuf::from(vec![0x51]);
        let pooled = pool::tests::id("pooled");
        let cache = |worker_names| JobCache::new(&named(worker_names, false), &pool, "/pool/");

        let unnamed = cache(false);
        let a = unnamed.job(&template, &pooled, "a", &pool, &Options::default());
        assert!(Arc::ptr_eq(
            &a,
            &unnamed.job(&template, &pooled, "b", &pool, &Options::default())
        ));

        let named = cache(true);
        let a = named.job(&template, &pooled, "a", &pool, &Options::default());
        let b = named.job(&template, &pooled, "b", &pool, &Options::default());
        assert_ne!(a.coinbase, b.coinbase);
        assert!(Arc::ptr_eq(
            &a,
            &named.job(&template, &pooled, "a", &pool, &Options::default())
        ));
        let signed =
            |job: &Job, text: &[u8]| job.coinbase.coinb1.windows(text.len()).any(|w| w == text);
        assert!(signed(&a, b"/pool/Mined by a/"));
        assert!(signed(&b, b"/pool/Mined by b/"));
    }

    #[test]
    fn under_opt_in_a_worker_asking_for_its_name_gets_jobs_of_its_own() {
        let template = Template {
            seq: 1,
            clean: true,
            block: Arc::new(recorded::result(REGTEST_GETBLOCKTEMPLATE)),
        };
        let pool = ScriptBuf::from(vec![0x51]);
        let pooled = pool::tests::id("pooled");
        let cache = JobCache::new(&named(false, true), &pool, "/pool/");
        let asks = Options {
            part: None,
            name: true,
        };
        let quiet = Options::default();
        let a = cache.job(&template, &pooled, "a", &pool, &quiet);
        assert!(Arc::ptr_eq(
            &a,
            &cache.job(&template, &pooled, "b", &pool, &quiet)
        ));
        let named = cache.job(&template, &pooled, "a", &pool, &asks);
        assert!(!Arc::ptr_eq(&a, &named));
        let signed =
            |job: &Job, text: &[u8]| job.coinbase.coinb1.windows(text.len()).any(|w| w == text);
        assert!(signed(&named, b"/pool/Mined by a/"));
        assert!(!signed(&a, b"Mined by"));
    }

    #[test]
    fn hybrid_workers_share_a_job_per_address_which_pays_its_finder_then_the_pool() {
        let template = Template {
            seq: 1,
            clean: true,
            block: Arc::new(recorded::result(REGTEST_GETBLOCKTEMPLATE)),
        };
        let pool = ScriptBuf::from(vec![0x51]);
        let (finder, other) = (ScriptBuf::from(vec![0x52]), ScriptBuf::from(vec![0x53]));
        let cache = JobCache::new(&pools(), &pool, "/pool/");
        let [solo, pooled, hybrid] = ["solo", "pooled", "hybrid"].map(pool::tests::id);

        let a = cache.job(&template, &hybrid, "a", &finder, &Options::default());
        assert_eq!(a.pool, hybrid);
        assert!(Arc::ptr_eq(
            &a,
            &cache.job(&template, &hybrid, "b", &finder, &Options::default())
        ));
        let elsewhere = cache.job(&template, &hybrid, "a", &other, &Options::default());
        assert!(!Arc::ptr_eq(&a, &elsewhere));
        assert_ne!(a.coinbase, elsewhere.coinbase);
        let solo = cache.job(&template, &solo, "a", &finder, &Options::default());
        assert!(!Arc::ptr_eq(&a, &solo), "never another pool's job");

        // A third of the reward to the finder, the rest to the pool: in the outputs and as recorded paid.
        let reward = template.block.coinbasevalue;
        let third = Amount::from_sat(reward * 333_300 / 1_000_000);
        let outputs = a.coinbase.transaction(&[0; EXTRANONCE_LEN]).output;
        assert_eq!(
            outputs[..2]
                .iter()
                .map(|o| (o.script_pubkey.clone(), o.value))
                .collect::<Vec<_>>(),
            [
                (finder.clone(), third),
                (pool.clone(), Amount::from_sat(reward) - third)
            ]
        );
        assert_eq!(a.paid(), (third, Amount::from_sat(reward) - third));
        assert_eq!(solo.paid(), (Amount::from_sat(reward), Amount::ZERO));
        let pooled = cache.job(&template, &pooled, "a", &pool, &Options::default());
        assert_eq!(pooled.paid(), (Amount::ZERO, Amount::from_sat(reward)));
    }
}
