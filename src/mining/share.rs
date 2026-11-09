//! The share check: hash the header a submission claims and compare it with a target, by double
//! SHA-256.

use bitcoin::{BlockHash, Target, block::Header};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checked {
    /// The header's double SHA-256, which also keys duplicates.
    pub hash: BlockHash,
    pub meets_target: bool,
    /// Whether the hash also meets the network target of the header's bits: the share is a block.
    pub is_block: bool,
}

pub fn check(header: &Header, target: Target) -> Checked {
    let hash = header.block_hash();
    Checked {
        hash,
        meets_target: target.is_met_by(hash),
        is_block: header.target().is_met_by(hash),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mining::{job::tests::mainnet_job, target},
        node::template::BIP320_MASK,
    };

    #[test]
    fn a_mainnet_block_meets_its_network_target() {
        let (job, submission, block) = mainnet_job();
        let header = job.header(&submission, BIP320_MASK as u32);
        let network = Target::from_compact(block.bits);

        let checked = check(&header, network);
        assert_eq!(checked.hash, block.hash);
        assert!(checked.meets_target);
        assert!(checked.is_block);

        let unmet = check(&header, target::from_difficulty(1e30));
        assert!(!unmet.meets_target);
        assert!(
            unmet.is_block,
            "the network target does not depend on the share target"
        );
    }

    #[test]
    fn a_wrong_nonce_misses_a_real_target() {
        let (job, mut submission, block) = mainnet_job();
        submission.nonce ^= 1;
        let checked = check(
            &job.header(&submission, BIP320_MASK as u32),
            target::from_difficulty(1.0),
        );
        assert_ne!(checked.hash, block.hash);
        assert!(!checked.is_block);
    }
}
