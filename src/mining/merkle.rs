//! The merkle branch for the coinbase position. It depends only on the template, so every job built from one
//! template shares it.

use bitcoin::{
    TxMerkleNode, Txid,
    hashes::{Hash, sha256d},
};

/// The sibling hashes from the coinbase up to the root, given the template's transactions in block order,
/// coinbase excluded.
pub fn branch(txids: &[Txid]) -> Vec<sha256d::Hash> {
    // Each level keeps a placeholder for the coinbase's path in position 0.
    let mut level: Vec<sha256d::Hash> = Vec::with_capacity(txids.len() + 2);
    level.push(sha256d::Hash::all_zeros());
    level.extend(txids.iter().map(|txid| txid.to_raw_hash()));
    let mut branch = Vec::new();
    while level.len() > 1 {
        branch.push(level[1]);
        if level.len() % 2 == 1 {
            level.push(*level.last().unwrap());
        }
        let mut next = Vec::with_capacity(level.len() / 2);
        next.push(sha256d::Hash::all_zeros());
        next.extend(level[2..].chunks(2).map(|pair| pair_hash(pair[0], pair[1])));
        level = next;
    }
    branch
}

/// The merkle root of a block whose coinbase has `coinbase` as txid.
pub fn root(coinbase: Txid, branch: &[sha256d::Hash]) -> TxMerkleNode {
    let root = branch.iter().fold(coinbase.to_raw_hash(), |node, sibling| {
        pair_hash(node, *sibling)
    });
    TxMerkleNode::from_raw_hash(root)
}

fn pair_hash(left: sha256d::Hash, right: sha256d::Hash) -> sha256d::Hash {
    let mut engine = sha256d::Hash::engine();
    bitcoin::hashes::HashEngine::input(&mut engine, left.as_byte_array());
    bitcoin::hashes::HashEngine::input(&mut engine, right.as_byte_array());
    sha256d::Hash::from_engine(engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mining::recorded::MainnetBlock;

    #[test]
    fn rebuilds_a_mainnet_merkle_root() {
        let block = MainnetBlock::load();
        let branch = branch(&block.txids[1..]);
        assert_eq!(root(block.txids[0], &branch), block.merkleroot);
    }

    #[test]
    fn a_lone_coinbase_is_its_own_root() {
        let coinbase = Txid::from_byte_array([7; 32]);
        assert!(branch(&[]).is_empty());
        assert_eq!(root(coinbase, &[]).to_raw_hash(), coinbase.to_raw_hash());
    }

    #[test]
    fn agrees_with_rust_bitcoin_for_every_small_count() {
        for count in 1..=17u8 {
            let txids: Vec<Txid> = (0..count).map(|i| Txid::from_byte_array([i; 32])).collect();
            let expected =
                bitcoin::merkle_tree::calculate_root(txids.iter().map(|t| t.to_raw_hash()))
                    .unwrap();
            assert_eq!(
                root(txids[0], &branch(&txids[1..])).to_raw_hash(),
                expected,
                "{count} transactions"
            );
        }
    }
}
