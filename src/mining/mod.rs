//! The job and share core shared by every front-end. Named `mining`, not `core`, to stay
//! clear of Rust's `core` crate.

pub mod block;
pub mod coinbase;
pub mod extranonce;
pub mod job;
pub mod merkle;
pub mod pool;
pub mod share;
pub mod target;
pub mod vardiff;

/// A real mainnet block, recorded from the local mainnet node, for fixed-vector tests.
#[cfg(test)]
pub(crate) mod recorded {
    use bitcoin::{BlockHash, CompactTarget, TxMerkleNode, Txid};
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct MainnetBlock {
        pub hash: BlockHash,
        pub height: u32,
        pub version: i32,
        pub previousblockhash: BlockHash,
        pub merkleroot: TxMerkleNode,
        pub time: u32,
        #[serde(deserialize_with = "crate::node::compact_target")]
        pub bits: CompactTarget,
        pub nonce: u32,
        /// The coinbase, witness included, in hex.
        pub coinbase_hex: String,
        /// Every transaction in block order, coinbase first.
        pub txids: Vec<Txid>,
    }

    impl MainnetBlock {
        pub fn load() -> Self {
            serde_json::from_str(include_str!(
                "../../tests/data/mining/mainnet-block-967926.json"
            ))
            .unwrap()
        }
    }
}
