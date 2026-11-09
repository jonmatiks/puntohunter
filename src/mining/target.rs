//! Share targets and Stratum difficulty. Difficulty d means the target diff1 / d, where diff1 is Stratum's
//! difficulty-1 target, 0xffff · 2^208. Difficulties below 1 give targets above diff1, up to 2^256 − 1.

use bitcoin::Target;

/// The Stratum difficulty a target stands for.
pub fn difficulty(target: Target) -> f64 {
    target.difficulty_float()
}

/// The target for Stratum difficulty `difficulty`, saturating at 2^256 − 1. `difficulty` must be positive.
pub fn from_difficulty(difficulty: f64) -> Target {
    assert!(
        difficulty.is_finite() && difficulty > 0.0,
        "difficulty must be positive, got {difficulty}"
    );
    // diff1 / d = (0xffff / d) · 2^208. Split 0xffff / d into a 53-bit integer mantissa and a power of two,
    // so the target is the mantissa shifted, with no 256-bit division.
    let (mantissa, exponent) = split(f64::from(0xffff_u32) / difficulty);
    Target::from_be_bytes(shifted(mantissa, exponent + 208))
}

/// The work a share at `target` stands for: the expected number of hashes
/// to meet it, 2^256 / (target + 1). In f64, which telemetry needs; accounting keeps it exact.
pub fn work(target: Target) -> f64 {
    let target = target
        .to_le_bytes()
        .iter()
        .rev()
        .fold(0.0, |n, &byte| n * 256.0 + f64::from(byte));
    2f64.powi(256) / (target + 1.0)
}

/// `x` as mantissa · 2^exponent, with an integer mantissa below 2^53.
fn split(x: f64) -> (u64, i32) {
    let bits = x.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1 << 52) - 1);
    if biased == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), biased - 1075)
    }
}

/// `value` · 2^shift as a big-endian 256-bit integer, rounded down, saturating at 2^256 − 1.
fn shifted(value: u64, shift: i32) -> [u8; 32] {
    let mut limbs = [0u64; 4]; // Little-endian 64-bit limbs.
    if shift < 0 {
        limbs[0] = value.checked_shr(shift.unsigned_abs()).unwrap_or(0);
    } else {
        let (index, bit) = ((shift / 64) as usize, shift % 64);
        let low = value << bit;
        let high = if bit == 0 { 0 } else { value >> (64 - bit) };
        if index >= 4 || (index == 3 && high != 0) {
            return [0xff; 32];
        }
        limbs[index] = low;
        if index < 3 {
            limbs[index + 1] = high;
        }
    }
    let mut bytes = [0u8; 32];
    for (i, limb) in limbs.iter().enumerate() {
        bytes[32 - 8 * (i + 1)..32 - 8 * i].copy_from_slice(&limb.to_be_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use bitcoin::CompactTarget;

    use super::*;

    fn close(a: f64, b: f64) -> bool {
        ((a - b) / b).abs() < 1e-12
    }

    #[test]
    fn difficulty_1_is_the_stratum_diff1_target() {
        assert_eq!(from_difficulty(1.0), Target::MAX);
        assert_eq!(difficulty(Target::MAX), 1.0);
    }

    #[test]
    fn round_trips_across_the_range() {
        for d in [
            2.5e-10,
            4.656542e-10,
            1e-3,
            0.5,
            1.0,
            1.5,
            512.0,
            2800.0,
            1e6,
            1.327e14,
            1e20,
        ] {
            assert!(close(difficulty(from_difficulty(d)), d), "difficulty {d}");
        }
    }

    #[test]
    fn a_higher_difficulty_is_a_lower_target() {
        assert!(from_difficulty(0.5) > from_difficulty(1.0));
        assert!(from_difficulty(1.0) > from_difficulty(2.0));
        assert_eq!(
            from_difficulty(2.0).to_be_bytes(),
            shifted(0xffff, 207),
            "diff1 halved exactly"
        );
    }

    #[test]
    fn matches_the_network_target_at_its_difficulty() {
        // Mainnet bits from the recorded template, and regtest's.
        for bits in [0x17021ec5, 0x207fffff] {
            let network = Target::from_compact(CompactTarget::from_consensus(bits));
            let rebuilt = from_difficulty(difficulty(network));
            assert!(
                close(difficulty(rebuilt), difficulty(network)),
                "bits {bits:#x}"
            );
        }
    }

    #[test]
    fn work_is_the_expected_hash_count() {
        // Difficulty 1 takes 2^48 / 0xffff hashes, just over 2^32.
        assert!(close(work(Target::MAX), 2f64.powi(48) / 65535.0));
        for d in [2.5e-10, 1.0, 2800.0, 1.327e14] {
            assert!(
                close(work(from_difficulty(d)), d * work(Target::MAX)),
                "{d}"
            );
        }
        assert_eq!(work(Target::from_le_bytes([0xff; 32])), 1.0, "every hash");
    }

    #[test]
    fn saturates_below_the_lowest_difficulty() {
        // diff1 / 2^256 is about 2.3e-10; anything lower asks for more than 256 bits.
        assert_eq!(from_difficulty(1e-12).to_be_bytes(), [0xff; 32]);
    }
}
