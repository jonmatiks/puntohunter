//! Coinbase parts: the serialized coinbase split around its 12 extranonce bytes.
//!
//! The scriptSig is the BIP34 height, a push of the signature, and a 12-byte push holding the extranonce: a
//! 4-byte prefix unique per connection, then the 8 bytes the miner rolls. The signature is the configured
//! tag, followed by `Mined by <worker>/` when worker names are configured. The outputs are the reward
//! outputs, the miner portion's two parts (the finder's, then the pool's, each left out when 0) and then each
//! configured fee in order, then the witness commitment when the template carries one.

use bitcoin::{
    Amount, Script, ScriptBuf, Transaction, TxOut, Txid, VarInt, Witness,
    consensus::{Encodable, deserialize},
    hashes::{Hash, sha256d},
    script::Builder,
};

use super::pool::{Personal, Pool};
use crate::config::{self, Portion};

/// The tag when the config sets none.
pub const DEFAULT_TAG: &str = "/puntohunter/";
/// The longest configured tag, in bytes.
pub const MAX_TAG_LEN: usize = 32;
const MINED_BY: &str = "Mined by ";
/// The longest worker name a signature carries, in bytes; a longer one is cut to this.
pub const MAX_WORKER_NAME_LEN: usize = 32;
/// The pool's part of the extranonce, unique per live connection or channel.
pub const EXTRANONCE_PREFIX_LEN: usize = 4;
/// The miner's part: V1 extranonce2, or the SV2 extended-channel extranonce.
pub const MINER_EXTRANONCE_LEN: usize = 8;
pub const EXTRANONCE_LEN: usize = EXTRANONCE_PREFIX_LEN + MINER_EXTRANONCE_LEN;

const VERSION: i32 = 2;

// The longest signature takes a single-byte push (opcodes 1 to 75), and with a 5-byte height and the
// extranonce push the scriptSig stays within consensus's 100 bytes.
const _: () = assert!(MAX_TAG_LEN + MINED_BY.len() + MAX_WORKER_NAME_LEN + "/".len() <= 75);
const _: () = assert!(5 + 1 + 75 + 1 + EXTRANONCE_LEN <= 100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoinbaseParts {
    /// The coinbase serialized without witness, up to the extranonce.
    pub coinb1: Vec<u8>,
    /// The coinbase serialized without witness, after the extranonce.
    pub coinb2: Vec<u8>,
    /// Whether the coinbase carries the zero witness reserved value, as it must when the block commits to
    /// witnesses.
    pub witness: bool,
}

/// The fee outputs every coinbase pays after the miner portion, in config order: each script and its portion.
/// Taken from the config, never from code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fees(Vec<(ScriptBuf, Portion)>);

impl Fees {
    pub fn new(fees: &[config::Fee]) -> Self {
        Fees(
            fees.iter()
                .map(|fee| (fee.address.script_pubkey(), fee.portion))
                .collect(),
        )
    }

    pub fn portions(&self) -> Vec<Portion> {
        self.0.iter().map(|(_, portion)| *portion).collect()
    }
}

/// What every coinbase of one pool pays: its fees, the pool wallet, and its finder's part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewards {
    pub fees: Fees,
    /// Takes what the finder leaves of the miner portion.
    pub pool: ScriptBuf,
    pub finder: Portion,
    /// The range a worker's own part is clamped to; None where every worker gets `finder`.
    pub personal: Option<Personal>,
}

impl Rewards {
    pub fn new(pool: &Pool, pool_wallet: ScriptBuf) -> Self {
        Rewards {
            fees: Fees::new(&pool.fees),
            pool: pool_wallet,
            finder: pool.finder,
            personal: pool.personal,
        }
    }

    /// The finder's part of `reward` for a worker that asked `part`: on a pool with `personal`, its part, or
    /// `finder` when it asked none, clamped in satoshis of this reward; on any other
    /// pool, `finder`.
    fn finder(&self, reward: Amount, part: Option<Portion>) -> Portion {
        match self.personal {
            Some(range) => Portion::Amount(range.clamp(reward, part.unwrap_or(self.finder))),
            None => self.finder,
        }
    }

    /// The split of `reward`, and the reward outputs paying it: the finder's part, as the worker asked `part`,
    /// to `miner`, and the rest to the pool wallet.
    pub fn pay(
        &self,
        reward: Amount,
        miner: &Script,
        part: Option<Portion>,
    ) -> (Split, Vec<TxOut>) {
        let split = split(
            reward,
            &self.fees.portions(),
            self.finder(reward, part),
            self.pool.minimal_non_dust(),
            miner.minimal_non_dust(),
        );
        let outputs = split.outputs(miner, &self.pool, &self.fees);
        (split, outputs)
    }

    /// The miner portion of `reward`: what its fees leave, which the finder's part and the pool output
    /// divide.
    pub fn miner(&self, reward: Amount) -> Amount {
        split(
            reward,
            &self.fees.portions(),
            Portion::Ppm(0),
            Amount::ZERO,
            Amount::ZERO,
        )
        .miner
    }
}

/// What a pool's scriptSigs say after the height: the configured tag, then the worker's name where the pool
/// writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    tag: String,
    /// Every worker's name.
    worker_names: bool,
    /// The name of each worker that asks with `+name`.
    worker_names_opt_in: bool,
}

impl Signature {
    /// The signature of `pool`'s coinbases, after `tag`.
    pub fn new(tag: &str, pool: &Pool) -> Self {
        Signature {
            tag: tag.to_owned(),
            worker_names: pool.worker_names,
            worker_names_opt_in: pool.worker_names_opt_in,
        }
    }

    /// Whether a worker's name is written, as [`Pool::writes_name`] says.
    fn writes(&self, asked: bool) -> bool {
        self.worker_names || (self.worker_names_opt_in && asked)
    }

    /// The signature of a coinbase `worker` mines, which `asked` for its name or not: the tag alone when its
    /// name is not written or it has none, else `<tag>Mined by <worker>/` with the name cut to
    /// [`MAX_WORKER_NAME_LEN`] bytes.
    pub fn text(&self, worker: &str, asked: bool) -> String {
        if !self.writes(asked) || worker.is_empty() {
            return self.tag.clone();
        }
        // Worker names are ASCII (username::parse), so any byte is a character boundary.
        let name = &worker[..worker.len().min(MAX_WORKER_NAME_LEN)];
        format!("{}{MINED_BY}{name}/", self.tag)
    }
}

/// A reward split among the reward outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    /// The miner portion: the reward less the fees.
    pub miner: Amount,
    /// The miner portion's two parts, either of which may be 0: the finder's, paid to its own address, and
    /// the pool output, paid to the pool wallet.
    pub finder: Amount,
    pub pool: Amount,
    pub fees: Vec<Amount>,
}

/// The split of reward R: each fee in order takes floor(R × share / 10^6), its share in parts per million,
/// or its fixed amount, capped at what the fees before it leave; the miner portion takes the rest, rounding
/// remainders included. With no fees the miner portion is R.
///
/// The finder takes its `finder` portion of R, as a fee would, capped at the miner portion, and the pool
/// output the rest. Then, only when both would be paid: a pool output below
/// `pool_dust`, the pool wallet's dust limit, goes to the finder; else a finder's part below `finder_dust`,
/// its script's, goes to the pool output. An output that takes the whole miner portion is paid even below
/// its limit.
pub fn split(
    reward: Amount,
    portions: &[Portion],
    finder: Portion,
    pool_dust: Amount,
    finder_dust: Amount,
) -> Split {
    let take = |portion: Portion, left: Amount| portion.of(reward).min(left);
    let mut left = reward;
    let fees = portions
        .iter()
        .map(|portion| {
            let fee = take(*portion, left);
            left -= fee;
            fee
        })
        .collect();
    let miner = left;
    let mut finder = take(finder, miner);
    let mut pool = miner - finder;
    if finder > Amount::ZERO && pool < pool_dust {
        (finder, pool) = (miner, Amount::ZERO);
    } else if pool > Amount::ZERO && finder < finder_dust {
        (finder, pool) = (Amount::ZERO, miner);
    }
    Split {
        miner,
        finder,
        pool,
        fees,
    }
}

impl Split {
    /// The reward outputs in order: the finder's part to `finder`, then the pool output to `pool`, each
    /// left out when 0; then each fee to its script.
    pub fn outputs(&self, finder: &Script, pool: &Script, fees: &Fees) -> Vec<TxOut> {
        let output = |value, script: &Script| TxOut {
            value,
            script_pubkey: script.to_owned(),
        };
        [(self.finder, finder), (self.pool, pool)]
            .into_iter()
            .filter(|(value, _)| *value > Amount::ZERO)
            .map(|(value, script)| output(value, script))
            .chain(
                fees.0
                    .iter()
                    .zip(&self.fees)
                    .map(|((script, _), value)| output(*value, script)),
            )
            .collect()
    }
}

/// Builds the parts for a block at `height` signed `signature`, whose coinbase pays `outputs`, the reward
/// outputs in order. Identical inputs give identical bytes.
pub fn build(
    height: u32,
    signature: &str,
    outputs: &[TxOut],
    witness_commitment: Option<&Script>,
) -> CoinbaseParts {
    let mut script_sig = Builder::new()
        .push_int(i64::from(height))
        .into_script()
        .into_bytes();
    assert!(
        signature.len() <= 75,
        "a signature takes a single-byte push"
    );
    script_sig.push(signature.len() as u8);
    script_sig.extend_from_slice(signature.as_bytes());
    script_sig.push(EXTRANONCE_LEN as u8);

    let mut coinb1 = Vec::new();
    write(&mut coinb1, &VERSION);
    write(&mut coinb1, &VarInt(1));
    coinb1.extend_from_slice(&[0; 32]); // The null outpoint: no previous transaction ...
    write(&mut coinb1, &u32::MAX); // ... and index 0xffffffff.
    write(
        &mut coinb1,
        &VarInt((script_sig.len() + EXTRANONCE_LEN) as u64),
    );
    coinb1.extend_from_slice(&script_sig);

    let mut outputs = outputs.to_vec();
    if let Some(commitment) = witness_commitment {
        outputs.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: commitment.to_owned(),
        });
    }
    let mut coinb2 = Vec::new();
    write(&mut coinb2, &u32::MAX); // Sequence.
    write(&mut coinb2, &outputs);
    write(&mut coinb2, &0u32); // Lock time.

    CoinbaseParts {
        coinb1,
        coinb2,
        witness: witness_commitment.is_some(),
    }
}

impl CoinbaseParts {
    /// The coinbase serialized without witness, as its txid hashes it.
    pub fn serialize(&self, extranonce: &[u8; EXTRANONCE_LEN]) -> Vec<u8> {
        [&self.coinb1[..], extranonce, &self.coinb2[..]].concat()
    }

    pub fn txid(&self, extranonce: &[u8; EXTRANONCE_LEN]) -> Txid {
        Txid::from_raw_hash(sha256d::Hash::hash(&self.serialize(extranonce)))
    }

    /// The complete coinbase, witness included.
    pub fn transaction(&self, extranonce: &[u8; EXTRANONCE_LEN]) -> Transaction {
        let mut transaction: Transaction = deserialize(&self.serialize(extranonce))
            .expect("coinbase parts serialize a transaction");
        if self.witness {
            transaction.input[0].witness = Witness::from_slice(&[[0u8; 32]]);
        }
        transaction
    }
}

fn write(buffer: &mut Vec<u8>, value: &impl Encodable) {
    value
        .consensus_encode(buffer)
        .expect("writing to a Vec cannot fail");
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bitcoin::{
        Address, Network,
        consensus::serialize,
        hex::{DisplayHex, FromHex},
        opcodes::all::OP_PUSHNUM_5,
    };
    use proptest::prelude::*;

    use super::*;
    use crate::{
        config::{Config, PPM},
        mining::{
            job::{Job, Shared},
            pool,
        },
        node::{
            BlockTemplate,
            recorded::{self, MAINNET_GETBLOCKTEMPLATE, REGTEST_GETBLOCKTEMPLATE},
        },
    };

    const EXTRANONCE: [u8; EXTRANONCE_LEN] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    /// The 0.9% and 0.6% of the checked-in configs, in parts per million.
    const SHARES: [Portion; 2] = [Portion::Ppm(9_000), Portion::Ppm(6_000)];

    /// The recorded regtest template, the fees of config/regtest.toml, and its pool wallet standing in for
    /// the miner's address.
    fn inputs() -> (BlockTemplate, ScriptBuf, Fees) {
        let config: Config =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config/regtest.toml"))
                .unwrap()
                .parse()
                .unwrap();
        (
            recorded::result(REGTEST_GETBLOCKTEMPLATE),
            config.addresses.pool_wallet.script_pubkey(),
            Fees::new(&config.fees),
        )
    }

    /// A finder of 100%: solo's.
    const SOLO: Portion = Portion::Ppm(PPM);

    /// Solo's reward outputs: the whole miner portion to `miner`, then the fees.
    fn outputs(reward: Amount, miner: &Script, fees: &Fees) -> Vec<TxOut> {
        split(reward, &fees.portions(), SOLO, Amount::ZERO, Amount::ZERO)
            .outputs(miner, miner, fees)
    }

    fn parts(template: &BlockTemplate, miner: &Script, fees: &Fees) -> CoinbaseParts {
        build(
            template.height,
            DEFAULT_TAG,
            &outputs(Amount::from_sat(template.coinbasevalue), miner, fees),
            template.default_witness_commitment.as_deref(),
        )
    }

    fn sats(split: &Split) -> Vec<u64> {
        [split.miner]
            .iter()
            .chain(&split.fees)
            .map(|amount| amount.to_sat())
            .collect()
    }

    #[test]
    fn splits_with_b4_rounding() {
        let mainnet: BlockTemplate = recorded::result(MAINNET_GETBLOCKTEMPLATE);
        for (reward, expected) in [
            (0, [0, 0, 0]),
            (1, [1, 0, 0]),
            (999, [986, 8, 5]),
            (1000, [985, 9, 6]),
            // The recorded mainnet template's coinbasevalue: subsidy plus fees.
            (mainnet.coinbasevalue, [311_872_866, 2_849_599, 1_899_733]),
        ] {
            let split = split(
                Amount::from_sat(reward),
                &SHARES,
                SOLO,
                Amount::ZERO,
                Amount::ZERO,
            );
            assert_eq!(sats(&split), expected, "reward {reward}");
            assert_eq!(sats(&split).iter().sum::<u64>(), reward);
        }
    }

    #[test]
    fn splits_among_any_number_of_fees_or_none() {
        let reward = Amount::from_sat(5_000_000_123);
        assert_eq!(
            sats(&split(reward, &[], SOLO, Amount::ZERO, Amount::ZERO)),
            [5_000_000_123],
            "no fees: the miner takes it all"
        );
        // Ten fees of 0.0125% each: floor(5,000,000,123 × 125 / 10^6) = 625,000 apiece.
        let split = split(
            reward,
            &[Portion::Ppm(125); 10],
            SOLO,
            Amount::ZERO,
            Amount::ZERO,
        );
        assert_eq!(split.fees, vec![Amount::from_sat(625_000); 10]);
        assert_eq!(split.miner, Amount::from_sat(5_000_000_123 - 6_250_000));
    }

    #[test]
    fn the_miner_portion_is_what_the_fees_leave_whatever_the_finder() {
        let rewards = |finder| Rewards {
            fees: Fees(vec![(ScriptBuf::new(), Portion::Ppm(15_000))]),
            pool: ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([1; 20])),
            finder,
            personal: None,
        };
        let reward = Amount::from_sat(1_000_000);
        for finder in [SOLO, Portion::Ppm(0), Portion::Ppm(333_300)] {
            assert_eq!(rewards(finder).miner(reward), Amount::from_sat(985_000));
        }
    }

    #[test]
    fn a_personal_part_below_its_scripts_dust_limit_goes_to_the_pool_output() {
        let p2pkh = ScriptBuf::new_p2pkh(&bitcoin::PubkeyHash::from_byte_array([1; 20]));
        let p2tr = ScriptBuf::from_bytes([&[0x51, 0x20][..], &[1; 32]].concat());
        let rewards = Rewards {
            fees: Fees(Vec::new()),
            pool: ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([2; 20])),
            finder: Portion::Ppm(333_300),
            personal: Some(Personal {
                min: Portion::Ppm(0),
                max: Portion::Ppm(PPM),
            }),
        };
        let reward = Amount::from_sat(1_000_000);
        let part = Some(Portion::Amount(Amount::from_sat(400)));
        let (paid, _) = rewards.pay(reward, &p2pkh, part);
        assert_eq!(
            (paid.finder, paid.pool),
            (Amount::ZERO, reward),
            "546 is P2PKH's limit"
        );
        let (paid, outputs) = rewards.pay(reward, &p2tr, part);
        assert_eq!(paid.finder, Amount::from_sat(400), "330 is P2TR's");
        assert_eq!(outputs[0].script_pubkey, p2tr);
    }

    #[test]
    fn takes_fixed_fees_capped_at_what_is_left() {
        let fixed = |sats| Portion::Amount(Amount::from_sat(sats));
        let share = Portion::Ppm(9_000);
        for (reward, portions, expected) in [
            // A share is of the whole reward wherever it stands; a fixed amount is the same at any reward.
            (1_000_000, [fixed(10_000), share], [981_000, 10_000, 9_000]),
            (
                2_000_000,
                [fixed(10_000), share],
                [1_972_000, 10_000, 18_000],
            ),
            // A reward too small for them all: each fee in order takes what is left, the miner going first.
            (10_000, [fixed(10_000), share], [0, 10_000, 0]),
            (15_000, [fixed(10_000), fixed(10_000)], [0, 10_000, 5_000]),
            (15_000, [share, fixed(20_000)], [0, 135, 14_865]),
        ] {
            let split = split(
                Amount::from_sat(reward),
                &portions,
                SOLO,
                Amount::ZERO,
                Amount::ZERO,
            );
            assert_eq!(sats(&split), expected, "reward {reward}, {portions:?}");
        }
    }

    #[test]
    fn outputs_sum_exactly_to_the_reward() {
        let (template, miner, fees) = inputs();
        for reward in [
            0,
            1,
            999,
            1000,
            1001,
            5_000_000_000,
            21_000_000 * 100_000_000,
        ] {
            let transaction = build(
                template.height,
                DEFAULT_TAG,
                &outputs(Amount::from_sat(reward), &miner, &fees),
                None,
            )
            .transaction(&EXTRANONCE);
            let total: u64 = transaction.output.iter().map(|o| o.value.to_sat()).sum();
            assert_eq!(total, reward);
        }
    }

    #[test]
    fn pays_each_fee_in_order_after_the_miner_or_none() {
        let (template, miner, _) = inputs();
        let receivers: Vec<ScriptBuf> = (0..5u8)
            .map(|i| Address::p2wsh(&ScriptBuf::from(vec![i]), Network::Regtest).script_pubkey())
            .collect();
        let fees = Fees(
            receivers
                .iter()
                .map(|s| (s.clone(), Portion::Ppm(1_000)))
                .collect(),
        );
        let transaction = build(
            template.height,
            DEFAULT_TAG,
            &outputs(Amount::from_sat(1_000_000), &miner, &fees),
            None,
        )
        .transaction(&EXTRANONCE);
        let scripts: Vec<_> = transaction
            .output
            .iter()
            .map(|o| o.script_pubkey.clone())
            .collect();
        assert_eq!(scripts, [vec![miner.clone()], receivers].concat());
        assert!(
            transaction.output[1..]
                .iter()
                .all(|o| o.value == Amount::from_sat(1_000))
        );

        let alone = build(
            template.height,
            DEFAULT_TAG,
            &outputs(Amount::from_sat(7), &miner, &Fees(Vec::new())),
            None,
        )
        .transaction(&EXTRANONCE);
        assert_eq!(alone.output.len(), 1);
        assert_eq!(alone.output[0].value, Amount::from_sat(7));
    }

    #[test]
    fn solo_and_pooled_coinbases_are_unchanged_by_hybrid() {
        // Txids recorded on 2026-10-06 from the builder before hybrid, so the refactor that let one
        // builder take hybrid's outputs changed no other mode's bytes. Re-recorded whenever the default tag
        // changes, last on 2026-10-16 for /puntohunter/.
        let (template, miner, fees) = inputs();
        let mainnet: BlockTemplate = recorded::result(MAINNET_GETBLOCKTEMPLATE);
        let fixed = Fees(vec![
            (
                fees.0[0].0.clone(),
                Portion::Amount(Amount::from_sat(1_000_000)),
            ),
            fees.0[1].clone(),
        ]);
        for (template, fees, txid) in [
            (
                &template,
                &fees,
                "55c33bfd75a92c27267ef3d5d7616d03f253eb94b974bde8f541908760125d15",
            ),
            (
                &mainnet,
                &fees,
                "fe525eab53c27b108a95fb08520bdc04c605cc5ac071ab155c52a407ccb366d3",
            ),
            (
                &template,
                &fixed,
                "62266eeae5d6e0a1e87fddb144100e6e98466c56bdc3288ad64b2b3f29f9c551",
            ),
            (
                &mainnet,
                &fixed,
                "2460bdf78c68e71ec4669ec8d0a27077ab694327d3ce4b6cfcb23fbe5760b079",
            ),
        ] {
            assert_eq!(
                parts(template, &miner, fees).txid(&EXTRANONCE).to_string(),
                txid
            );
        }
    }

    /// The dust limit of the regtest pool wallet's P2WPKH script.
    const DUST: Amount = Amount::from_sat(294);

    #[test]
    fn splits_hybrid_as_the_design_tabulates() {
        // A worked example: a 33.33% finder share at fees of 0.9% and 0.6%, 0.025 BTC of transaction fees.
        let (_, pool, _) = inputs();
        assert_eq!(pool.minimal_non_dust(), DUST);
        for (reward, finder, rest) in [
            (315_000_000, 104_989_500, 205_285_500),
            (158_750_000, 52_911_375, 103_457_375),
            (80_625_000, 26_872_312, 52_543_313),
        ] {
            let split = split(
                Amount::from_sat(reward),
                &SHARES,
                Portion::Ppm(333_300),
                DUST,
                DUST,
            );
            assert_eq!(
                (split.finder, split.pool),
                (Amount::from_sat(finder), Amount::from_sat(rest)),
                "reward {reward}"
            );
        }
    }

    #[test]
    fn caps_the_finder_at_the_miner_portion_and_gives_it_dust() {
        // With no fees the miner portion is the reward, 10,000 sats.
        let fixed = |sats| Portion::Amount(Amount::from_sat(sats));
        for (finder, expected) in [
            (fixed(9_706), (9_706, 294)), // the pool output at the dust limit: kept
            (fixed(9_707), (10_000, 0)),  // one satoshi below it: the finder's
            (fixed(10_000), (10_000, 0)), // N = M
            (fixed(50_000), (10_000, 0)), // N > M
            (fixed(0), (0, 10_000)),      // nothing for the finder
            (Portion::Ppm(970_600), (9_706, 294)),
            (Portion::Ppm(970_700), (10_000, 0)),
        ] {
            let split = split(Amount::from_sat(10_000), &[], finder, DUST, DUST);
            assert_eq!(
                (split.finder.to_sat(), split.pool.to_sat()),
                expected,
                "{finder:?}"
            );
        }
    }

    /// The dust limits of the three finder scripts the design names.
    fn finder_scripts() -> [(&'static str, ScriptBuf, u64); 3] {
        [
            (
                "P2PKH",
                ScriptBuf::new_p2pkh(&bitcoin::PubkeyHash::from_byte_array([1; 20])),
                546,
            ),
            (
                "P2WPKH",
                ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([1; 20])),
                294,
            ),
            (
                "P2TR",
                ScriptBuf::from_bytes([&[0x51, 0x20][..], &[1; 32]].concat()),
                330,
            ),
        ]
    }

    #[test]
    fn a_finder_part_below_its_scripts_dust_limit_goes_to_the_pool_output() {
        let reward = Amount::from_sat(1_000_000);
        for (name, script, dust) in finder_scripts() {
            assert_eq!(script.minimal_non_dust().to_sat(), dust, "{name}");
            let rewards = Rewards {
                fees: Fees(Vec::new()),
                pool: ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([2; 20])),
                finder: Portion::Amount(Amount::from_sat(dust - 1)),
                personal: None,
            };
            let (below, outputs) = rewards.pay(reward, &script, None);
            assert_eq!((below.finder, below.pool), (Amount::ZERO, reward), "{name}");
            assert_eq!(outputs.len(), 1, "{name}: the finder's output left out");
            assert_eq!(outputs[0].script_pubkey, rewards.pool, "{name}");

            let at = Rewards {
                finder: Portion::Amount(Amount::from_sat(dust)),
                ..rewards
            };
            let (split, outputs) = at.pay(reward, &script, None);
            assert_eq!(split.finder.to_sat(), dust, "{name}: at the limit, paid");
            assert_eq!(outputs[0].script_pubkey, script, "{name}");
        }
    }

    #[test]
    fn an_output_taking_the_whole_miner_portion_is_paid_below_its_dust_limit() {
        // A miner portion of 200 sats, below every limit, after fees of 0.9% and 0.6% of a reward of 202.
        let reward = Amount::from_sat(202);
        let solo = split(reward, &SHARES, SOLO, DUST, DUST);
        assert_eq!(
            (solo.finder, solo.pool),
            (Amount::from_sat(200), Amount::ZERO)
        );
        let pooled = split(reward, &SHARES, Portion::Ppm(0), DUST, DUST);
        assert_eq!(
            (pooled.finder, pooled.pool),
            (Amount::ZERO, Amount::from_sat(200)),
            "no finder paid: the pool wallet's, allocated"
        );
        // Below both limits, with both parts above 0: the pool rule runs first, so the finder takes it.
        let hybrid = split(reward, &SHARES, Portion::Ppm(333_300), DUST, DUST);
        assert_eq!(
            (hybrid.finder, hybrid.pool),
            (Amount::from_sat(200), Amount::ZERO)
        );
        let fixed = split(
            reward,
            &SHARES,
            Portion::Amount(Amount::from_sat(1)),
            DUST,
            DUST,
        );
        assert_eq!(
            (fixed.finder, fixed.pool),
            (Amount::from_sat(200), Amount::ZERO)
        );
    }

    #[test]
    fn a_pool_output_below_the_pool_wallets_dust_limit_goes_to_the_finder() {
        let reward = Amount::from_sat(10_000);
        for (name, script, dust) in finder_scripts() {
            let split = split(
                reward,
                &[],
                Portion::Amount(Amount::from_sat(10_000 - 293)),
                DUST,
                Amount::from_sat(dust),
            );
            assert_eq!((split.finder, split.pool), (reward, Amount::ZERO), "{name}");
            let outputs = split.outputs(&script, &ScriptBuf::new(), &Fees(Vec::new()));
            assert_eq!(outputs.len(), 1, "{name}: the pool output left out");
        }
    }

    proptest! {
        #[test]
        fn finder_pool_and_fees_always_sum_to_the_reward(
            reward in 0..=21_000_000 * 100_000_000u64,
            finder in prop_oneof![
                (0..=PPM).prop_map(Portion::Ppm),
                (0..=21_000_000 * 100_000_000u64).prop_map(|sats| Portion::Amount(Amount::from_sat(sats))),
            ],
            fees in proptest::collection::vec(
                prop_oneof![
                    (1..100_000u64).prop_map(Portion::Ppm),
                    (294..1_000_000_000u64).prop_map(|sats| Portion::Amount(Amount::from_sat(sats))),
                ],
                0..5,
            ),
            pool_dust in 0..1_000u64,
            finder_dust in 0..1_000u64,
        ) {
            let (pool_dust, finder_dust) = (Amount::from_sat(pool_dust), Amount::from_sat(finder_dust));
            let r = Amount::from_sat(reward);
            let split = split(r, &fees, finder, pool_dust, finder_dust);
            let total = split.finder + split.pool + split.fees.iter().copied().sum::<Amount>();
            prop_assert_eq!(total, r);
            prop_assert_eq!(split.finder + split.pool, split.miner);
            // Both paid only above their limits, and then the finder its part exactly.
            if split.finder > Amount::ZERO && split.pool > Amount::ZERO {
                prop_assert!(split.pool >= pool_dust && split.finder >= finder_dust);
                let n = match finder {
                    Portion::Ppm(ppm) => Amount::from_sat(
                        (u128::from(reward) * u128::from(ppm) / u128::from(PPM)) as u64,
                    ),
                    Portion::Amount(amount) => amount,
                };
                prop_assert_eq!(split.finder, n);
            }
        }
    }

    /// A hybrid coinbase on the recorded regtest template: a finder's script of its own beside the pool
    /// wallet.
    fn hybrid_parts(
        template: &BlockTemplate,
        finder: &Script,
        portion: Portion,
        fees: &Fees,
    ) -> CoinbaseParts {
        let (_, pool, _) = inputs();
        let split = split(
            Amount::from_sat(template.coinbasevalue),
            &fees.portions(),
            portion,
            pool.minimal_non_dust(),
            finder.minimal_non_dust(),
        );
        build(
            template.height,
            DEFAULT_TAG,
            &split.outputs(finder, &pool, fees),
            template.default_witness_commitment.as_deref(),
        )
    }

    #[test]
    fn lays_out_a_hybrid_coinbase_finder_then_pool_then_solos_fees() {
        let (template, pool, fees) = inputs();
        let finder = Address::p2wsh(Script::new(), Network::Regtest).script_pubkey();
        let reward = Amount::from_sat(template.coinbasevalue);
        let commitment = template.default_witness_commitment.clone().unwrap();
        let solo = parts(&template, &finder, &fees).transaction(&EXTRANONCE);

        let transaction =
            hybrid_parts(&template, &finder, Portion::Ppm(333_300), &fees).transaction(&EXTRANONCE);
        let split = split(
            reward,
            &SHARES,
            Portion::Ppm(333_300),
            DUST,
            finder.minimal_non_dust(),
        );
        let (f, p) = (split.finder, split.pool);
        assert!(f > Amount::ZERO && p > Amount::ZERO);
        let scripts: Vec<_> = transaction
            .output
            .iter()
            .map(|o| o.script_pubkey.clone())
            .collect();
        assert_eq!(
            scripts,
            [
                finder.clone(),
                pool.clone(),
                fees.0[0].0.clone(),
                fees.0[1].0.clone(),
                commitment.clone()
            ]
        );
        assert_eq!(transaction.output[0].value, f);
        assert_eq!(transaction.output[1].value, p);
        assert_eq!(
            transaction.output.iter().map(|o| o.value).sum::<Amount>(),
            reward
        );
        assert_eq!(
            serialize(&transaction.output[2..].to_vec()),
            serialize(&solo.output[1..].to_vec()),
            "the fees and the commitment as solo's, byte for byte"
        );
        assert_eq!(transaction.input, solo.input);

        // A finder's part above the miner portion leaves one finder output and no pool output.
        let alone = hybrid_parts(
            &template,
            &finder,
            Portion::Amount(Amount::from_sat(21_000_000 * 100_000_000)),
            &fees,
        )
        .transaction(&EXTRANONCE);
        assert_eq!(alone.output.len(), solo.output.len());
        assert_eq!(
            alone.output, solo.output,
            "all of it to the finder, as solo"
        );
    }

    #[test]
    fn identical_hybrid_inputs_give_identical_bytes() {
        let (template, pool, fees) = inputs();
        let finder = Address::p2wsh(Script::new(), Network::Regtest).script_pubkey();
        let share = Portion::Ppm(333_300);
        assert_eq!(
            hybrid_parts(&template, &finder, share, &fees),
            hybrid_parts(&template, &finder, share, &fees)
        );
        assert_ne!(
            hybrid_parts(&template, &finder, share, &fees),
            hybrid_parts(&template, &pool, share, &fees)
        );
        assert_ne!(
            hybrid_parts(&template, &finder, share, &fees),
            hybrid_parts(&template, &finder, Portion::Ppm(333_301), &fees)
        );
    }

    #[test]
    fn identical_inputs_give_identical_bytes() {
        let (template, miner, fees) = inputs();
        assert_eq!(
            parts(&template, &miner, &fees),
            parts(&template, &miner, &fees)
        );

        let other = Address::p2wsh(Script::new(), Network::Regtest).script_pubkey();
        assert_ne!(
            parts(&template, &miner, &fees),
            parts(&template, &other, &fees)
        );
        let swapped = Fees(fees.0.iter().rev().cloned().collect());
        assert_ne!(
            parts(&template, &miner, &fees),
            parts(&template, &miner, &swapped)
        );
    }

    #[test]
    fn lays_out_the_coinbase() {
        let (template, miner, fees) = inputs();
        let parts = parts(&template, &miner, &fees);
        let transaction = parts.transaction(&EXTRANONCE);

        assert!(transaction.is_coinbase());
        let script_sig = transaction.input[0].script_sig.as_bytes();
        let height = Builder::new()
            .push_int(i64::from(template.height))
            .into_script();
        assert!(
            script_sig.starts_with(height.as_bytes()),
            "BIP34 height first"
        );
        let rest = &script_sig[height.len()..];
        let tag = DEFAULT_TAG.as_bytes();
        assert_eq!(rest[0] as usize, tag.len());
        assert_eq!(&rest[1..1 + tag.len()], tag);
        assert_eq!(&rest[1 + tag.len()..], [&[12u8][..], &EXTRANONCE].concat());

        let split = split(
            Amount::from_sat(template.coinbasevalue),
            &SHARES,
            SOLO,
            Amount::ZERO,
            Amount::ZERO,
        );
        let outputs: Vec<_> = transaction
            .output
            .iter()
            .map(|o| (o.value, o.script_pubkey.clone()))
            .collect();
        assert_eq!(
            outputs,
            [
                (split.finder, miner),
                (split.fees[0], fees.0[0].0.clone()),
                (split.fees[1], fees.0[1].0.clone()),
                (
                    Amount::ZERO,
                    template.default_witness_commitment.clone().unwrap()
                ),
            ]
        );
        assert_eq!(transaction.input[0].witness.to_vec(), vec![vec![0u8; 32]]);
        assert_eq!(parts.txid(&EXTRANONCE), transaction.compute_txid());
    }

    #[test]
    fn leaves_out_the_commitment_and_witness_without_one() {
        let (template, miner, fees) = inputs();
        let parts = build(
            template.height,
            DEFAULT_TAG,
            &outputs(Amount::from_sat(1), &miner, &fees),
            None,
        );
        let transaction = parts.transaction(&EXTRANONCE);
        assert_eq!(transaction.output.len(), 3);
        assert!(transaction.input[0].witness.is_empty());
    }

    #[test]
    fn encodes_a_low_height_as_bip34_does() {
        let (_, miner, fees) = inputs();
        let transaction = build(
            5,
            DEFAULT_TAG,
            &outputs(Amount::from_sat(1), &miner, &fees),
            None,
        )
        .transaction(&EXTRANONCE);
        assert_eq!(
            transaction.input[0].script_sig.as_bytes()[0],
            OP_PUSHNUM_5.to_u8()
        );
    }

    fn signature(tag: &str, worker_names: bool) -> Signature {
        Signature {
            tag: tag.to_owned(),
            worker_names,
            worker_names_opt_in: false,
        }
    }

    #[test]
    fn signs_with_the_tag_and_the_worker_only_when_configured() {
        let off = signature("/pool/", false);
        assert_eq!(off.text("bitaxe", false), "/pool/");
        let on = signature("/pool/", true);
        assert_eq!(on.text("bitaxe", false), "/pool/Mined by bitaxe/");
        assert_eq!(on.text("rack.1", false), "/pool/Mined by rack.1/");
        assert_eq!(
            on.text("", false),
            "/pool/",
            "an unnamed worker: the tag alone"
        );

        let long = "w".repeat(64);
        assert_eq!(
            on.text(&long, false),
            format!("/pool/Mined by {}/", &long[..MAX_WORKER_NAME_LEN])
        );
    }

    #[test]
    fn writes_a_name_exactly_as_the_two_flags_and_the_workers_ask_say() {
        // Every combination of the two flags and the worker's ask; both flags on is refused at startup, so it is not here.
        for (worker_names, opt_in, asked, written) in [
            (false, false, false, false),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, true),
            (true, false, false, true),
            (true, false, true, true),
        ] {
            let signature = Signature {
                tag: "/pool/".to_owned(),
                worker_names,
                worker_names_opt_in: opt_in,
            };
            let expected = if written {
                "/pool/Mined by rig/"
            } else {
                "/pool/"
            };
            assert_eq!(
                signature.text("rig", asked),
                expected,
                "{worker_names} {opt_in} {asked}"
            );
            assert_eq!(
                signature.text("", asked),
                "/pool/",
                "a bare address has no name"
            );
        }
    }

    #[test]
    fn the_longest_signature_fits_the_script_sig() {
        let (_, miner, fees) = inputs();
        let longest = signature(&"t".repeat(MAX_TAG_LEN), true).text(&"w".repeat(64), false);
        // The largest height a 4-byte push holds, so the height push is the 5 bytes a later one would take.
        let transaction = build(
            0x7fff_ffff,
            &longest,
            &outputs(Amount::from_sat(1), &miner, &fees),
            None,
        )
        .transaction(&EXTRANONCE);
        let script_sig = transaction.input[0].script_sig.as_bytes();
        assert!(script_sig.len() <= 100, "{} bytes", script_sig.len());
        let pushes: Vec<_> = transaction.input[0]
            .script_sig
            .instructions()
            .map(|i| i.unwrap().push_bytes().map(|b| b.as_bytes().to_vec()))
            .collect();
        assert_eq!(pushes[1].as_deref(), Some(longest.as_bytes()));
        assert_eq!(pushes[2].as_deref(), Some(&EXTRANONCE[..]));
    }

    /// The recorded coinbases: what the code built before configurable pools.
    const VECTORS: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/mining/coinbases.json"
    );

    /// One recorded coinbase and the inputs it was built from. Scripts, the extranonce, and the coinbase are
    /// in hex.
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct Vector {
        name: String,
        /// A recorded getblocktemplate in tests/data/node/.
        template: String,
        mode: String,
        /// Hybrid's finder portion; none in solo and pooled.
        finder: Option<VectorPortion>,
        fees: Vec<VectorFee>,
        pool_wallet: String,
        /// The worker's address: the finder's script.
        miner: String,
        signature: String,
        extranonce: String,
        /// The coinbase transaction, witness included.
        coinbase: String,
    }

    #[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "lowercase")]
    enum VectorPortion {
        Ppm(u64),
        Sats(u64),
    }

    impl From<VectorPortion> for Portion {
        fn from(portion: VectorPortion) -> Self {
            match portion {
                VectorPortion::Ppm(ppm) => Portion::Ppm(ppm),
                VectorPortion::Sats(sats) => Portion::Amount(Amount::from_sat(sats)),
            }
        }
    }

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct VectorFee {
        script: String,
        portion: VectorPortion,
    }

    fn script(hex: &str) -> ScriptBuf {
        ScriptBuf::from_hex(hex).unwrap()
    }

    /// The coinbase today's code builds from a vector's inputs, in hex: a job on the template, paying the
    /// worker's address where the pool can pay a finder and the pool wallet where it cannot, as sessions
    /// choose.
    fn rebuild(vector: &Vector) -> String {
        let block: BlockTemplate = recorded::result(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/data/node")
                    .join(&vector.template),
            )
            .unwrap(),
        );
        // Each mode as the default pool of its name: solo's finder takes 100%, pooled's 0.
        let finder = match vector.mode.as_str() {
            "solo" => SOLO,
            "pooled" => Portion::Ppm(0),
            _ => vector.finder.unwrap().into(),
        };
        let pool = pool::tests::paying(&vector.mode, finder);
        let rewards = Rewards {
            fees: Fees(
                vector
                    .fees
                    .iter()
                    .map(|fee| (script(&fee.script), fee.portion.into()))
                    .collect(),
            ),
            pool: script(&vector.pool_wallet),
            finder,
            personal: None,
        };
        let miner = match pool.can_pay_finder() {
            true => script(&vector.miner),
            false => rewards.pool.clone(),
        };
        let job = Job::new(
            &block,
            0,
            pool.id,
            &vector.signature,
            &miner,
            None,
            &rewards,
            &Shared::new(&block),
        );
        let extranonce = <[u8; EXTRANONCE_LEN]>::from_hex(&vector.extranonce).unwrap();
        serialize(&job.coinbase.transaction(&extranonce)).to_lower_hex_string()
    }

    /// The recorded cases: each mode, hybrid at a percent and at a fixed amount, with no fees, the checked-in
    /// percent fees, and a fixed fee, to P2WPKH, P2TR, and P2PKH finders, over the recorded mainnet and
    /// regtest templates. No finder's part is below its script's dust limit.
    fn cases() -> Vec<Vector> {
        let regtest: Config =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config/regtest.toml"))
                .unwrap()
                .parse()
                .unwrap();
        let mainnet: Config =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config/mainnet.toml"))
                .unwrap()
                .parse()
                .unwrap();
        let hex = |script: ScriptBuf| script.to_hex_string();
        let percent: Vec<VectorFee> = regtest
            .fees
            .iter()
            .map(|fee| VectorFee {
                script: hex(fee.address.script_pubkey()),
                portion: match fee.portion {
                    Portion::Ppm(ppm) => VectorPortion::Ppm(ppm),
                    Portion::Amount(amount) => VectorPortion::Sats(amount.to_sat()),
                },
            })
            .collect();
        let fixed = vec![
            VectorFee {
                portion: VectorPortion::Sats(1_000_000),
                ..percent[0].clone()
            },
            percent[1].clone(),
        ];
        let fee_lists = [
            ("no fees", vec![]),
            ("percent fees", percent),
            ("a fixed fee", fixed),
        ];
        let finders = [
            (
                "P2WPKH",
                ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([0x11; 20])),
                DEFAULT_TAG.to_owned(),
            ),
            (
                "P2TR",
                ScriptBuf::from_bytes([&[0x51, 0x20][..], &[0x22; 32]].concat()),
                format!("{DEFAULT_TAG}Mined by rig-1/"),
            ),
            (
                "P2PKH",
                ScriptBuf::new_p2pkh(&bitcoin::PubkeyHash::from_byte_array([0x33; 20])),
                DEFAULT_TAG.to_owned(),
            ),
        ];
        let modes = [
            ("solo", None),
            ("pooled", None),
            ("hybrid 33.33%", Some(VectorPortion::Ppm(333_300))),
            ("hybrid 0.5 BTC", Some(VectorPortion::Sats(50_000_000))),
        ];
        let templates = [
            ("mainnet", "mainnet-getblocktemplate.json", &mainnet),
            ("regtest", "regtest-getblocktemplate.json", &regtest),
        ];
        let mut cases = Vec::new();
        for (network, template, config) in templates {
            for (mode, finder) in modes {
                for (fees_name, fees) in &fee_lists {
                    for (script_name, miner, signature) in &finders {
                        let n = cases.len() as u8;
                        cases.push(Vector {
                            name: format!("{network} {mode}, {fees_name}, {script_name} finder"),
                            template: template.to_owned(),
                            mode: mode.split(' ').next().unwrap().to_owned(),
                            finder,
                            fees: fees.clone(),
                            pool_wallet: hex(config.addresses.pool_wallet.script_pubkey()),
                            miner: hex(miner.clone()),
                            signature: signature.clone(),
                            extranonce: [[n; 4], [0xa5, n, 0x5a, n]].concat().repeat(2)
                                [..EXTRANONCE_LEN]
                                .to_lower_hex_string(),
                            coinbase: String::new(),
                        });
                    }
                }
            }
        }
        cases
    }

    /// Writes tests/data/mining/coinbases.json from today's code. Recorded once, before configurable pools
    /// moved any code; the file must not change after, and running this again must rewrite it unchanged.
    #[test]
    #[ignore = "records the vectors; run by hand"]
    fn record_coinbase_vectors() {
        let vectors: Vec<Vector> = cases()
            .into_iter()
            .map(|mut vector| {
                vector.coinbase = rebuild(&vector);
                vector
            })
            .collect();
        std::fs::write(
            VECTORS,
            serde_json::to_string_pretty(&vectors).unwrap() + "\n",
        )
        .unwrap();
    }

    #[test]
    fn rebuilds_every_recorded_coinbase_byte_for_byte() {
        let vectors: Vec<Vector> =
            serde_json::from_str(&std::fs::read_to_string(VECTORS).unwrap()).unwrap();
        assert_eq!(vectors.len(), 72);
        for vector in &vectors {
            assert_eq!(rebuild(vector), vector.coinbase, "{}", vector.name);
        }
    }
}
