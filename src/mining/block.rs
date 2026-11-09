//! Block assembly: a share that meets the network target becomes a complete block, built by the pool alone
//! from its job, never held together by the node.

use bitcoin::{VarInt, consensus::Encodable, hex::DisplayHex};

use super::job::{Job, Submission};

/// The block `submission` completes on `job`, with version bits rolled within `mask`, in hex as submitblock
/// takes it: the header the share claims, the coinbase with its witness, then the template's transactions.
pub fn assemble(job: &Job, submission: &Submission, mask: u32) -> String {
    let mut bytes = Vec::new();
    job.header(submission, mask)
        .consensus_encode(&mut bytes)
        .expect("writing to a Vec cannot fail");
    VarInt(1 + job.transactions.count as u64)
        .consensus_encode(&mut bytes)
        .expect("writing to a Vec cannot fail");
    job.coinbase
        .transaction(&submission.extranonce)
        .consensus_encode(&mut bytes)
        .expect("writing to a Vec cannot fail");
    bytes.extend_from_slice(&job.transactions.bytes);
    bytes.to_lower_hex_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitcoin::{
        Amount, Block,
        consensus::{deserialize, serialize},
        hex::FromHex,
    };

    use super::*;
    use crate::{
        config::Config,
        mining::{
            coinbase::{CoinbaseParts, DEFAULT_TAG, EXTRANONCE_LEN, Rewards, Split},
            job::Shared,
            merkle, pool,
        },
        node::{
            BlockTemplate,
            recorded::{self, REGTEST_GETBLOCKTEMPLATE},
            template::BIP320_MASK,
        },
    };

    const MASK: u32 = BIP320_MASK as u32;

    /// Mainnet block 967934, recorded whole from the local mainnet node: 66 KB, 184 transactions, and a
    /// witness commitment.
    const MAINNET_BLOCK: &str = include_str!("../../tests/data/mining/mainnet-block-967934.hex");

    fn decode(hex: &str) -> Block {
        deserialize(&Vec::<u8>::from_hex(hex).unwrap()).unwrap()
    }

    #[test]
    fn assembles_a_block_from_the_recorded_regtest_template() {
        let config: Config =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/config/regtest.toml"))
                .unwrap()
                .parse()
                .unwrap();
        let template: BlockTemplate = recorded::result(REGTEST_GETBLOCKTEMPLATE);
        let miner = config.addresses.pool_wallet.script_pubkey();
        let [solo, ..] = &config.pools[..] else {
            panic!("the regtest config's solo pool")
        };
        let job = Job::new(
            &template,
            1,
            solo.id.clone(),
            DEFAULT_TAG,
            &miner,
            None,
            &Rewards::new(solo, miner.clone()),
            &Shared::new(&template),
        );
        let submission = Submission {
            extranonce: [7; EXTRANONCE_LEN],
            time: template.curtime + 30,
            nonce: 0x1234_5678,
            version_bits: 0x0000_6000,
        };

        let block = decode(&assemble(&job, &submission, MASK));
        assert_eq!(block.header, job.header(&submission, MASK));
        assert_eq!(block.header.time, submission.time, "the miner's ntime");
        assert_eq!(
            block.header.version.to_consensus(),
            0x2000_6000,
            "the rolled version"
        );
        assert!(block.check_merkle_root());
        assert!(block.check_witness_commitment());
        assert_eq!(
            block.txdata[1..]
                .iter()
                .map(|tx| tx.compute_txid())
                .collect::<Vec<_>>(),
            template
                .transactions
                .iter()
                .map(|tx| tx.txid)
                .collect::<Vec<_>>()
        );
        let coinbase = &block.txdata[0];
        assert_eq!(
            coinbase.output.iter().map(|o| o.value).sum::<Amount>(),
            Amount::from_sat(template.coinbasevalue)
        );
        assert_eq!(coinbase.output[0].script_pubkey, miner);
    }

    #[test]
    fn rebuilds_a_mainnet_block_byte_for_byte() {
        let block = decode(MAINNET_BLOCK);
        assert!(block.check_merkle_root());
        assert!(block.check_witness_commitment());

        // The parts a template and a submission would give: the coinbase split around 12 bytes of its
        // scriptSig, the version as a clean job version plus the miner's rolled bits, and each other
        // transaction in hex as getblocktemplate's data carries it.
        let coinbase = &block.txdata[0];
        assert_eq!(coinbase.input[0].witness.to_vec(), vec![vec![0u8; 32]]);
        let mut stripped = coinbase.clone();
        stripped.input[0].witness.clear();
        let bytes = serialize(&stripped);
        // Version, input count, outpoint, scriptSig length: the scriptSig starts at byte 42.
        let split = 42 + 8;
        let version = block.header.version.to_consensus();
        let job = Job {
            pool: pool::tests::id("solo"),
            template: 0,
            height: block.bip34_block_height().unwrap() as u32,
            prevhash: block.header.prev_blockhash,
            version: version & !BIP320_MASK,
            bits: block.header.bits,
            time: block.header.time,
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
            branch: merkle::branch(
                &block.txdata[1..]
                    .iter()
                    .map(|tx| tx.compute_txid())
                    .collect::<Vec<_>>(),
            ),
            transactions: Arc::new(block.txdata[1..].iter().map(serialize).collect()),
        };
        let submission = Submission {
            extranonce: bytes[split..split + EXTRANONCE_LEN].try_into().unwrap(),
            time: block.header.time,
            nonce: block.header.nonce,
            version_bits: version as u32 & MASK,
        };

        assert_eq!(job.height, 967_934);
        assert_eq!(
            job.header(&submission, MASK).block_hash(),
            block.block_hash()
        );
        assert_eq!(assemble(&job, &submission, MASK), MAINNET_BLOCK);
    }
}
