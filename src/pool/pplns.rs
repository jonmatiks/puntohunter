//! Difficulty-weighted PPLNS: a block's pool output
//! divided by work over a window of its pool's journal, fixed when the block's solution share is journaled.
//!
//! The window ends at the solution share, inclusive, and walks back W = max(window_multiplier × D,
//! window_min_work) of work, D the solved job's network difficulty, crediting the oldest share only the
//! fraction of its work W still needs. A window holding less than W divides by what it holds. Each address
//! gets floor(amount × work / total) satoshis, and what the floors leave goes a satoshi at a time by largest
//! fractional part, ties to the lower address. Work is the expected hash count of a share's assigned target,
//! held exactly as an integer in units of 2^-16 hashes, 2^256 / (⌊target / 2^16⌋ + 1): whole hashes would round every share of the e2e harness, below difficulty 1e-9, to one or
//! two. A window at today's mainnet difficulty is about 2^96 units, so a u128 total has room for difficulty
//! to grow four billion times. The products take 192 bits.
//!
//! Under accounting policy 2, each share counts its work weighted by its job's
//! contribution, floor(work × Pₛ / Mₛ), Pₛ the pool output of the job's miner portion Mₛ, both journaled with
//! the share; W and every credit are then weighted work. Policy 1, plain work, still walks the windows of
//! blocks whose solution share it holds.

use std::collections::{BTreeMap, HashMap};

use bitcoin::{BlockHash, CompactTarget, Target};
use rusqlite::{Connection, Transaction, params};

use super::ledger::{self, Stage};
use crate::mining::pool::PoolId;

/// A journaled share, as the window walks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub sequence: u64,
    pub address: String,
    /// In 2^-16 hashes, see [`work`]; weighted by [`weigh`] under policy 2.
    pub work: u128,
}

/// The accounting policy that weighs each share's work by its job's contribution.
pub const WEIGHTED: i64 = 2;

/// A share's weighted work under policy 2: floor(`work` × `pool` / `miner`), its job's pool output over its
/// miner portion, exactly. Its 192-bit product splits as work = q × miner + r, so the result is
/// q × pool + floor(r × pool / miner), each term within 128 bits since pool ≤ miner.
pub fn weigh(work: u128, miner: u64, pool: u64) -> u128 {
    assert!(
        pool <= miner && miner > 0,
        "a pool output is part of its miner portion"
    );
    let (miner, pool) = (u128::from(miner), u128::from(pool));
    (work / miner) * pool + (work % miner) * pool / miner
}

/// The work a share's journal row credits under `version`: plain, or weighted.
fn credited(target: &[u8], miner: i64, pool: i64, version: i64) -> rusqlite::Result<u128> {
    let target = <[u8; 32]>::try_from(target).map_err(|_| {
        rusqlite::Error::InvalidColumnType(0, "target".into(), rusqlite::types::Type::Blob)
    })?;
    let work = work(Target::from_be_bytes(target));
    Ok(if version >= WEIGHTED {
        weigh(work, miner as u64, pool as u64)
    } else {
        work
    })
}

/// The bits by which work is finer than a hash.
const WORK_SCALE_BITS: u32 = 16;

/// A block's window: whose work it credits, and how much.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// The oldest share in the window.
    pub first: u64,
    /// The work credited to that oldest share: all of its work, or the fraction W still needed.
    pub boundary: u128,
    /// The work credited in all: W, or less when the journal holds less.
    pub total: u128,
    /// The credited work per address, workers paying one address together.
    pub credits: BTreeMap<String, u128>,
}

/// The work of a share at `target`: its expected hashes in units of 2^-16 hashes, 2^256 / (⌊target / 2^16⌋ +
/// 1), which bitcoin's own work arithmetic computes for the target shifted down.
pub fn work(target: Target) -> u128 {
    let shifted = Target::from_be_bytes(shift_right(target.to_be_bytes(), WORK_SCALE_BITS));
    let bytes = shifted.to_work().to_be_bytes();
    let (high, low) = bytes.split_at(16);
    assert!(high.iter().all(|&b| b == 0), "no share target is that hard");
    u128::from_be_bytes(low.try_into().expect("16 bytes"))
}

/// `bytes`, a 256-bit big-endian integer, shifted right by `bits`, from 1 to 127.
fn shift_right(bytes: [u8; 32], bits: u32) -> [u8; 32] {
    let (high, low) = bytes.split_at(16);
    let high = u128::from_be_bytes(high.try_into().expect("16 bytes"));
    let low = u128::from_be_bytes(low.try_into().expect("16 bytes"));
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(&(high >> bits).to_be_bytes());
    out[16..].copy_from_slice(&((low >> bits) | (high << (128 - bits))).to_be_bytes());
    out
}

/// W, in work units: `multiplier` times the work of the network difficulty `bits` sets, or `min_work`
/// difficulty-1 shares' worth if that is more.
pub fn size(multiplier: f64, min_work: f64, bits: CompactTarget) -> u128 {
    let network = work(Target::from_compact(bits)) as f64;
    let difficulty_1 = work(Target::MAX) as f64;
    // Float products, rounded down: W only places the window's boundary, and the same inputs always give the
    // same W. The allocation itself is exact.
    ((multiplier * network) as u128)
        .max((min_work * difficulty_1) as u128)
        .max(1)
}

/// Walks `newest_first`, the journal of one pool ending at a solution share, back until `size` work is
/// credited. None when it holds no share.
pub fn window(newest_first: impl IntoIterator<Item = Share>, size: u128) -> Option<Window> {
    let mut credits = BTreeMap::new();
    let mut total = 0u128;
    let mut oldest = None;
    for share in newest_first {
        let credited = share.work.min(size - total);
        *credits.entry(share.address).or_insert(0) += credited;
        total += credited;
        oldest = Some((share.sequence, credited));
        if total == size {
            break;
        }
    }
    let (first, boundary) = oldest?;
    Some(Window {
        first,
        boundary,
        total,
        credits,
    })
}

/// Divides `amount` satoshis by `window`'s credits: each address its floor, then the satoshis the floors
/// leave, one each, by largest remainder, ties to the lower address. Addresses allocated nothing are left
/// out; the amounts sum to `amount` exactly.
pub fn allocate(amount: u64, window: &Window) -> Vec<(String, u64)> {
    let mut shares: Vec<(&String, u64, u128)> = window
        .credits
        .iter()
        .map(|(address, &work)| {
            let (sats, remainder) = mul_div(amount, work, window.total);
            (address, sats, remainder)
        })
        .collect();
    let left = amount - shares.iter().map(|(_, sats, _)| sats).sum::<u64>();
    let mut order: Vec<usize> = (0..shares.len()).collect();
    // Stable, so equal remainders keep address order.
    order.sort_by(|&a, &b| shares[b].2.cmp(&shares[a].2));
    for &i in order.iter().take(left as usize) {
        shares[i].1 += 1;
    }
    shares
        .into_iter()
        .filter(|(_, sats, _)| *sats > 0)
        .map(|(address, sats, _)| (address.clone(), sats))
        .collect()
}

/// floor(a × b / c) and its remainder, exactly: the product takes up to 192 bits. The quotient fits 64 bits
/// because b ≤ c.
fn mul_div(a: u64, b: u128, c: u128) -> (u64, u128) {
    assert!(b <= c && c > 0, "a credit is part of its window");
    const LOW: u128 = u64::MAX as u128;
    let a = u128::from(a);
    let low = a * (b & LOW);
    let high = a * (b >> 64);
    // The product is high × 2^64 + low, as two 128-bit halves.
    let (product_low, carry) = low.overflowing_add(high << 64);
    let product_high = (high >> 64) + u128::from(carry);
    let (mut quotient, mut remainder) = (0u128, 0u128);
    for bit in (0..256).rev() {
        let next = if bit >= 128 {
            (product_high >> (bit - 128)) & 1
        } else {
            (product_low >> bit) & 1
        };
        // remainder < c, so doubling it overflows at most into a 129th bit, which c cannot reach.
        let overflow = remainder >> 127 == 1;
        remainder = (remainder << 1) | next;
        quotient <<= 1;
        if overflow || remainder >= c {
            remainder = remainder.wrapping_sub(c);
            quotient |= 1;
        }
    }
    (
        u64::try_from(quotient).expect("b ≤ c keeps the quotient under a"),
        remainder,
    )
}

/// A pool's window as a block found now would take it: each worker's
/// credited weighted work, by handle, the total credited, and the plain work it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub total: u128,
    pub credits: HashMap<String, u128>,
    /// The plain work of the shares credited, the oldest's in proportion to its credit: total over plain is
    /// the window's mean contribution.
    pub plain: u128,
}

impl Current {
    /// The part of the window worker `handle` holds, from 0 to 1.
    pub fn part(&self, handle: &str) -> f64 {
        self.credits
            .get(handle)
            .map_or(0.0, |&work| work as f64 / self.total as f64)
    }
}

/// `pool`'s current window: its journal walked back from the newest share until `size` weighted work is
/// credited, as [`window`] walks a block's under policy 2. None while the journal holds no share of the pool.
pub fn current(
    connection: &Connection,
    pool: &PoolId,
    size: u128,
) -> rusqlite::Result<Option<Current>> {
    let mut statement = connection.prepare_cached(
        "SELECT handle, target, miner_sats, pool_sats FROM journal WHERE pool = ?1 \
         ORDER BY sequence DESC",
    )?;
    let mut rows = statement.query([pool])?;
    let mut credits = HashMap::new();
    let (mut total, mut plain) = (0u128, 0u128);
    while total < size
        && let Some(row) = rows.next()?
    {
        let target: Vec<u8> = row.get(1)?;
        let weighted = self::credited(&target, row.get(2)?, row.get(3)?, WEIGHTED)?;
        let credited = weighted.min(size - total);
        *credits.entry(row.get(0)?).or_insert(0) += credited;
        total += credited;
        if weighted > 0 {
            // Policy 1's count: the share's plain work, its amounts unread.
            let work = self::credited(&target, 0, 0, 1)?;
            plain += (work as f64 * credited as f64 / weighted as f64) as u128;
        }
    }
    Ok((total > 0).then_some(Current {
        total,
        credits,
        plain,
    }))
}

/// A block whose solution share is journaled and that has no snapshot yet.
struct Ready {
    hash: String,
    /// The solution share's pool, whose window the block takes.
    pool: PoolId,
    cutoff: u64,
    amount: u64,
    bits: CompactTarget,
    multiplier: f64,
    min_work: f64,
    /// The solution share's policy version: plain work, or weighted.
    version: i64,
    /// Whether the node refused the block or it left the chain before it was allocated: its allocations are
    /// recorded, and credited only if it returns.
    off_chain: bool,
}

/// Gives a window cutoff to every block with a pool output whose solution share was never journaled: found
/// while the journal could not write, or just before a crash, so its miner was never told the share was
/// accepted. Called at startup only, before the journal opens, when no such share can still arrive. The
/// block's window then ends at its pool's newest share journaled when it was found, which is all the pool
/// credited before it, so its pool output is allocated rather than left in the pool wallet. A block whose pool
/// had journaled nothing by then keeps no cutoff. Returns the blocks given one.
pub fn cut_off_unjournaled(t: &Transaction) -> rusqlite::Result<Vec<String>> {
    t.prepare(
        "UPDATE blocks SET cutoff = \
             (SELECT MAX(j.sequence) FROM journal j \
              WHERE j.pool = blocks.pool AND j.accepted_at <= blocks.found_at) \
         WHERE pool_sats > 0 AND cutoff IS NULL \
         AND EXISTS (SELECT 1 FROM journal j \
                     WHERE j.pool = blocks.pool AND j.accepted_at <= blocks.found_at) \
         RETURNING hash",
    )?
    .query_map([], |row| row.get(0))?
    .collect()
}

/// Snapshots and allocates every pooled or hybrid block that is recorded, whose solution share is journaled,
/// and that has no snapshot yet, crediting each allocation pending in the ledger: called in the commit that
/// completes either half, and at startup, so a crash between them loses no allocation. Returns the blocks
/// allocated.
pub fn allocate_ready(t: &Transaction) -> rusqlite::Result<Vec<BlockHash>> {
    let ready: Vec<Ready> = t
        .prepare_cached(
            "SELECT b.hash, j.pool, b.cutoff, b.pool_sats, j.bits, p.window_multiplier, p.window_min_work, \
             b.status IN ('rejected', 'orphaned'), p.version \
             FROM blocks b JOIN journal j ON j.sequence = b.cutoff JOIN policies p ON p.id = j.policy \
             WHERE b.pool_sats IS NOT NULL \
             AND NOT EXISTS (SELECT 1 FROM snapshots s WHERE s.block = b.hash) \
             ORDER BY b.cutoff",
        )?
        .query_map([], |row| {
            Ok(Ready {
                hash: row.get(0)?,
                pool: row.get(1)?,
                cutoff: row.get::<_, i64>(2)? as u64,
                amount: row.get::<_, i64>(3)? as u64,
                bits: CompactTarget::from_consensus(row.get(4)?),
                multiplier: row.get(5)?,
                min_work: row.get(6)?,
                off_chain: row.get(7)?,
                version: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut allocated = Vec::with_capacity(ready.len());
    for block in ready {
        snapshot(t, &block)?;
        let hash = block.hash.parse().expect("a stored block hash");
        let stage = if block.off_chain {
            Stage::Void
        } else {
            Stage::Pending
        };
        ledger::settle(t, hash, stage)?;
        allocated.push(hash);
    }
    Ok(allocated)
}

fn snapshot(t: &Transaction, block: &Ready) -> rusqlite::Result<()> {
    let size = size(block.multiplier, block.min_work, block.bits);
    let mut statement = t.prepare_cached(
        "SELECT sequence, address, target, miner_sats, pool_sats FROM journal \
         WHERE pool = ?1 AND sequence <= ?2 ORDER BY sequence DESC",
    )?;
    let mut rows = statement.query(params![block.pool, block.cutoff as i64])?;
    let mut error = None;
    let shares = std::iter::from_fn(|| match rows.next() {
        Ok(Some(row)) => {
            let read = || -> rusqlite::Result<Share> {
                let target: Vec<u8> = row.get(2)?;
                Ok(Share {
                    sequence: row.get::<_, i64>(0)? as u64,
                    address: row.get(1)?,
                    work: credited(&target, row.get(3)?, row.get(4)?, block.version)?,
                })
            };
            read().map_err(|e| error = Some(e)).ok()
        }
        Ok(None) => None,
        Err(e) => {
            error = Some(e);
            None
        }
    });
    let window = window(shares, size);
    drop(rows);
    if let Some(e) = error {
        return Err(e);
    }
    // The walk starts at the solution share, so the window holds at least it.
    let window = window.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    t.execute(
        "INSERT INTO snapshots (block, pool, cutoff, first, boundary, size, total, amount) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            block.hash,
            block.pool,
            block.cutoff as i64,
            window.first as i64,
            window.boundary.to_string(),
            size.to_string(),
            window.total.to_string(),
            block.amount as i64,
        ],
    )?;
    let allocations = allocate(block.amount, &window);
    for (address, sats) in &allocations {
        t.execute(
            "INSERT INTO allocations (block, pool, address, sats) VALUES (?1, ?2, ?3, ?4)",
            params![block.hash, block.pool, address, *sats as i64],
        )?;
    }
    tracing::info!(
        hash = block.hash,
        pool = %block.pool,
        cutoff = block.cutoff,
        first = window.first,
        sats = block.amount,
        addresses = allocations.len(),
        "block allocated"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn share(sequence: u64, address: &str, work: u128) -> Share {
        Share {
            sequence,
            address: address.into(),
            work,
        }
    }

    /// x × y as two 128-bit halves, high first.
    fn wide(x: u128, y: u128) -> (u128, u128) {
        const LOW: u128 = u64::MAX as u128;
        let (x_high, x_low, y_high, y_low) = (x >> 64, x & LOW, y >> 64, y & LOW);
        let low = x_low * y_low;
        let middle = x_low * y_high;
        let (middle, carry) = middle.overflowing_add(x_high * y_low);
        let (low, carry_low) = low.overflowing_add(middle << 64);
        let high =
            x_high * y_high + (middle >> 64) + (u128::from(carry) << 64) + u128::from(carry_low);
        (high, low)
    }

    #[test]
    fn the_work_of_a_target_is_its_expected_hash_count() {
        // Difficulty 1: about 2^32 hashes, in units of 2^-16 hashes.
        assert_eq!(work(Target::MAX), 281_479_271_743_489);
        // Regtest's network target, 0x7fffff × 2^232: two hashes.
        let regtest = CompactTarget::from_consensus(0x207f_ffff);
        assert_eq!(work(Target::from_compact(regtest)), 2 << 16);
        assert_eq!(
            work(Target::from_be_bytes([0xff; 32])),
            1 << 16,
            "the easiest: one hash"
        );
        assert_eq!(size(2.0, 0.0, regtest), 4 << 16, "k × D");
        assert_eq!(
            size(2.0, 1.0, regtest),
            281_479_271_743_489,
            "W_min when it is more"
        );
        // A share below difficulty 1 keeps its proportion: 2.4e-10 and 4e-10 are 1.03 and 1.72 hashes.
        let at = |difficulty| work(crate::mining::target::from_difficulty(difficulty)) as f64;
        assert!((at(4e-10) / at(2.4e-10) - 4.0 / 2.4).abs() < 1e-4);
        assert_eq!(
            shift_right([0xff; 32], 16)[..3],
            [0, 0, 0xff],
            "zeros in from the top"
        );
    }

    #[test]
    fn the_window_credits_only_what_the_oldest_share_still_needs() {
        let shares = [
            share(9, "b", 40),
            share(8, "a", 30),
            share(7, "c", 50),
            share(6, "a", 99),
        ];
        let window = window(shares.clone(), 100).unwrap();
        assert_eq!((window.first, window.boundary, window.total), (7, 30, 100));
        assert_eq!(
            window.credits,
            BTreeMap::from([("a".into(), 30), ("b".into(), 40), ("c".into(), 30)]),
            "share 6 is outside, and c gets 30 of its 50"
        );

        let exact = super::window(shares.clone(), 70).unwrap();
        assert_eq!((exact.first, exact.boundary, exact.total), (8, 30, 70));

        // A journal holding less than W: the window takes all of it and divides by that.
        let short = super::window(shares, 1_000).unwrap();
        assert_eq!((short.first, short.boundary, short.total), (6, 99, 219));
        assert_eq!(short.credits["a"], 129, "one address's workers together");
        assert_eq!(super::window(Vec::new(), 100), None);
    }

    #[test]
    fn a_shares_weighted_work_is_its_work_times_its_contribution_exactly() {
        assert_eq!(
            weigh(1_000, 300, 300),
            1_000,
            "a contribution of 1: work itself"
        );
        assert_eq!(weigh(1_000, 300, 0), 0, "a contribution of 0: nothing");
        assert_eq!(weigh(1_000, 300, 100), 333, "floor(1000 / 3)");
        // At today's network difficulty a share is about 2^95 of work, and the product about 2^123 and beyond.
        let work = (1u128 << 100) + 12_345;
        let (miner, pool) = (312_500_000_u64, 208_354_166_u64);
        let (high, low) = wide(work, u128::from(pool));
        assert_eq!(high, 0, "2^100 × 2^28 still fits 128 bits");
        assert_eq!(weigh(work, miner, pool), low / u128::from(miner));
        assert_eq!(
            weigh(u128::MAX, u64::MAX, u64::MAX - 1),
            u128::MAX - u128::MAX / u128::from(u64::MAX)
        );
    }

    #[test]
    fn work_alone_pays_the_keeper_twice_and_weighted_work_pays_each_its_worth() {
        // Two workers of equal hashrate, M = 1 BTC; A keeps 100%, B keeps 0%. Each
        // finds one block. A's block pays A its finder output, M; B's pays the pool output, M, over the window.
        let m: u64 = 100_000_000;
        let work = 1u128 << 32;
        let shares = |a_work: u128| [share(2, "b", work), share(1, "a", a_work)];
        let paid = |a_work: u128| {
            let window = window(shares(a_work), 2 * work).unwrap();
            let allocated: BTreeMap<String, u64> = allocate(m, &window).into_iter().collect();
            (
                m + allocated.get("a").copied().unwrap_or(0),
                allocated.get("b").copied().unwrap_or(0),
            )
        };
        assert_eq!(paid(work), (m + m / 2, m / 2), "work alone: A 1.5, B 0.5");
        // Weighted, A's shares count weigh(work, m, 0) = 0 and are not journaled at all.
        assert_eq!(weigh(work, m, 0), 0);
        let window = window([share(2, "b", weigh(work, m, m))], 2 * work).unwrap();
        assert_eq!(allocate(m, &window), [("b".to_owned(), m)]);
        assert_eq!((m, m), (m, allocate(m, &window)[0].1), "A 1.0, B 1.0");
    }

    #[test]
    fn one_contribution_throughout_allocates_as_work_alone() {
        let plain = [
            share(4, "a", 500),
            share(3, "b", 300),
            share(2, "c", 700),
            share(1, "a", 900),
        ];
        // A contribution of a half, which every work here divides exactly, so no floor moves the boundary.
        let (miner, pool) = (1_000, 500);
        let weighted = plain.clone().map(|s| Share {
            work: weigh(s.work, miner, pool),
            ..s
        });
        let plain_window = window(plain, 1_800).unwrap();
        let weighted_window = window(weighted, weigh(1_800, miner, pool)).unwrap();
        assert_eq!(
            allocate(1_000_000, &plain_window),
            allocate(1_000_000, &weighted_window)
        );
    }

    proptest! {
        #[test]
        fn weighted_allocations_sum_to_the_pool_output(
            shares in prop::collection::vec((0u8..6, 1u64..1_000_000_000_000, 1u64..=1_000_000), 1..80),
            amount in 0u64..2_100_000_000_000_000,
            size in 1u128..(1 << 50),
        ) {
            let miner = 1_000_000;
            let journal = shares.iter().enumerate().rev().map(|(i, &(address, work, pool))| Share {
                sequence: i as u64,
                address: address.to_string(),
                work: weigh(u128::from(work), miner, pool),
            });
            if let Some(window) = window(journal, size) {
                prop_assume!(window.total > 0);
                let sum: u64 = allocate(amount, &window).iter().map(|(_, sats)| sats).sum();
                prop_assert_eq!(sum, amount);
            }
        }
    }

    #[test]
    fn every_part_earns_its_works_worth_in_expectation() {
        use proptest::test_runner::{RngAlgorithm, TestRng};

        // Workers of unequal hashrate, each keeping a seeded random part of
        // M (one 0% and one 100% among them), mine equal shares, one in S a block. A block pays its finder its
        // part and allocates the pool output over the weighted window of W = k × D, k = 2. Over 100,000 blocks
        // each is paid within 3% of its work × M / D: no part pays more than another.
        const BLOCKS: u64 = 100_000;
        const S: u64 = 16;
        let mut rng = TestRng::from_seed(RngAlgorithm::ChaCha, &[7; 32]);
        let m: u64 = 312_500_000;
        let d: u128 = 1 << 16;
        let rates: [u64; 5] = [9, 5, 3, 2, 1];
        let mut parts = [0u64, 1_000_000, 0, 0, 0];
        for part in &mut parts[2..] {
            *part = rng.next_u64() % 1_000_001;
        }
        let pool: Vec<u64> = parts.iter().map(|&ppm| m - m * ppm / 1_000_000).collect();
        let size = 2 * u128::from(S) * d;
        let total_rate: u64 = rates.iter().sum();

        let mut journal: std::collections::VecDeque<Share> = Default::default();
        let (mut paid, mut mined) = ([0u64; 5], [0u64; 5]);
        let (mut sequence, mut blocks) = (0u64, 0u64);
        while blocks < BLOCKS {
            let mut pick = rng.next_u64() % total_rate;
            let i = rates
                .iter()
                .position(|&rate| match pick.checked_sub(rate) {
                    Some(rest) => {
                        pick = rest;
                        false
                    }
                    None => true,
                })
                .unwrap();
            mined[i] += 1;
            if pool[i] > 0 {
                journal.push_back(share(sequence, &i.to_string(), weigh(d, m, pool[i])));
                if journal.len() > 4_096 {
                    journal.pop_front();
                }
            }
            sequence += 1;
            if rng.next_u64().is_multiple_of(S) {
                blocks += 1;
                paid[i] += m - pool[i];
                if pool[i] > 0 {
                    let window = window(journal.iter().rev().cloned(), size).unwrap();
                    for (address, sats) in allocate(pool[i], &window) {
                        paid[address.parse::<usize>().unwrap()] += sats;
                    }
                }
            }
        }
        for i in 0..rates.len() {
            let worth = mined[i] as f64 / S as f64 * m as f64;
            let ratio = paid[i] as f64 / worth;
            assert!(
                (ratio - 1.0).abs() < 0.03,
                "worker {i}, keeping {} ppm: paid {ratio:.4} of its worth",
                parts[i]
            );
        }
    }

    #[test]
    fn leftover_satoshis_go_by_largest_remainder_then_address_order() {
        let window = |credits: &[(&str, u128)]| Window {
            first: 1,
            boundary: 1,
            total: credits.iter().map(|(_, w)| w).sum(),
            credits: credits.iter().map(|(a, w)| ((*a).into(), *w)).collect(),
        };
        // 10 sats over 1/3, 1/3, 1/3: floors 3 each, the leftover one to the first address.
        assert_eq!(
            allocate(10, &window(&[("c", 1), ("a", 1), ("b", 1)])),
            [("a".into(), 4), ("b".into(), 3), ("c".into(), 3)]
        );
        // 10 sats over 0.55, 0.30, 0.15: floors 5, 3, 1 leave one, and a and c tie at 0.5: a, first.
        assert_eq!(
            allocate(10, &window(&[("a", 55), ("b", 30), ("c", 15)])),
            [("a".into(), 6), ("b".into(), 3), ("c".into(), 1)]
        );
        // An address the amount is too small to reach is left out.
        assert_eq!(
            allocate(1, &window(&[("a", 2), ("b", 1)])),
            [("a".into(), 1)]
        );
        assert_eq!(allocate(0, &window(&[("a", 1)])), []);
    }

    #[test]
    fn products_beyond_128_bits_divide_exactly() {
        // Mainnet's whole supply over a window of 2^100 hashes, a third credited to one address.
        let total = 3u128 << 98;
        let (sats, remainder) = mul_div(2_100_000_000_000_000, 1u128 << 98, total);
        assert_eq!((sats, remainder), (700_000_000_000_000, 0));
        assert_eq!(mul_div(u64::MAX, u128::MAX, u128::MAX), (u64::MAX, 0));
        assert_eq!(mul_div(7, u128::MAX - 1, u128::MAX), (6, u128::MAX - 7));
    }

    mod stored {
        use bitcoin::{Amount, hashes::Hash};

        use super::*;
        use crate::{
            config::Window,
            mining::pool::tests::id,
            pool::{
                blocks::{self, FoundBlock},
                journal::{self, Entry, Journal},
            },
            store::{DATABASE, Store},
            stratum::Transport,
        };

        const POOLED: &str = "bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s";
        const HYBRID: &str = "bcrt1q3jeqwzg70pfkc9k4pvynlmfjlrrghp0cd705hx";

        async fn open(name: &str) -> (Store, Journal) {
            let dir = std::env::temp_dir()
                .join(format!("puntohunter-pplns-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store = Store::open(&dir.join(DATABASE)).unwrap();
            let window = Window {
                window_multiplier: 2.0,
                window_min_work: 1.0,
            };
            let policy = journal::policy(&store, window).await.unwrap();
            let policies = [(id("pooled"), policy), (id("hybrid"), policy)].into();
            (store.clone(), Journal::spawn(store, policies))
        }

        /// A share of `pool` paying `address`, credited at `target`.
        fn entry(n: u8, pool: &str, address: &str, target: Target, block: bool) -> Entry {
            Entry {
                pool: id(pool),
                address: address.into(),
                handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
                template: 1,
                job: 1,
                bits: CompactTarget::from_consensus(0x207f_ffff),
                miner_sats: Amount::from_sat(1_000),
                pool_sats: Amount::from_sat(1_000),
                target,
                proof: BlockHash::from_byte_array([n; 32]),
                accepted_at: 0,
                block,
            }
        }

        fn found(n: u8, pool: &str, pool_sats: u64) -> FoundBlock {
            FoundBlock {
                hash: BlockHash::from_byte_array([n; 32]),
                height: 1,
                pool: id(pool),
                worker: "rig".into(),
                transport: Transport::V1,
                handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
                finder_sats: Amount::ZERO,
                pool_sats: Amount::from_sat(pool_sats),
                hex: String::new(),
            }
        }

        async fn allocations(store: &Store, n: u8) -> Vec<(String, i64)> {
            let hash = BlockHash::from_byte_array([n; 32]).to_string();
            store
                .commit(move |t| {
                    t.prepare(
                        "SELECT address, sats FROM allocations WHERE block = ?1 ORDER BY address",
                    )?
                    .query_map([hash], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect()
                })
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn each_mode_allocates_its_own_window_once_and_for_good() {
            let (store, journal) = open("windows").await;
            // Two hashes and one.
            let mut half = [0xff; 32];
            half[0] = 0x7f;
            let (hard, easy) = (
                Target::from_be_bytes(half),
                Target::from_be_bytes([0xff; 32]),
            );
            assert_eq!((work(hard), work(easy)), (2 << 16, 1 << 16));
            for e in [
                entry(1, "pooled", POOLED, hard, false),
                entry(2, "hybrid", HYBRID, easy, false),
                entry(3, "hybrid", POOLED, easy, false),
                entry(4, "pooled", HYBRID, easy, false),
            ] {
                journal.record(e).await.unwrap();
            }
            // The hybrid block's solution, journaled before the block is recorded.
            journal
                .record(entry(5, "hybrid", HYBRID, easy, true))
                .await
                .unwrap();
            let block = found(5, "hybrid", 1_000);
            store
                .commit(move |t| {
                    blocks::insert(t, &block, 0)?;
                    allocate_ready(t)
                })
                .await
                .unwrap();
            // Hybrid work only: HYBRID 1 + 1, POOLED 1, of 3.
            assert_eq!(
                allocations(&store, 5).await,
                [(HYBRID.into(), 667), (POOLED.into(), 333)]
            );

            // The pooled block recorded first; its share after it, which allocates it.
            let block = found(6, "pooled", 900);
            store
                .commit(move |t| blocks::insert(t, &block, 0))
                .await
                .unwrap();
            journal
                .record(entry(6, "pooled", POOLED, hard, true))
                .await
                .unwrap();
            let pooled = loop {
                let rows = allocations(&store, 6).await;
                if !rows.is_empty() {
                    break rows;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            };
            // Pooled work only: POOLED 2 + 2, HYBRID 1, of 5.
            assert_eq!(pooled, [(HYBRID.into(), 180), (POOLED.into(), 720)]);

            // Later shares, and another pass, change neither.
            journal
                .record(entry(7, "hybrid", HYBRID, hard, false))
                .await
                .unwrap();
            assert_eq!(store.commit(allocate_ready).await.unwrap(), []);
            assert_eq!(allocations(&store, 5).await[1], (POOLED.into(), 333));
            assert_eq!(allocations(&store, 6).await, pooled);
        }

        #[tokio::test]
        async fn a_block_whose_solution_share_was_never_journaled_takes_the_window_before_it() {
            let (store, journal) = open("unjournaled").await;
            let easy = Target::from_be_bytes([0xff; 32]);
            for e in [
                entry(1, "pooled", POOLED, easy, false),
                entry(2, "pooled", HYBRID, easy, false),
            ] {
                journal.record(e).await.unwrap();
            }
            // Found while the journal could not write: its own share never reached it. Beside it, a solo
            // block, and a block of a pool that had journaled nothing.
            let (lost, solo, empty) = (
                found(9, "pooled", 1_000),
                found(10, "pooled", 0),
                found(11, "hybrid", 500),
            );
            store
                .commit(move |t| {
                    blocks::insert(t, &lost, 0)?;
                    blocks::insert(t, &solo, 0)?;
                    blocks::insert(t, &empty, 0)
                })
                .await
                .unwrap();
            assert_eq!(store.commit(allocate_ready).await.unwrap(), []);

            let cut = store.commit(cut_off_unjournaled).await.unwrap();
            assert_eq!(cut, [BlockHash::from_byte_array([9; 32]).to_string()]);
            assert_eq!(store.commit(allocate_ready).await.unwrap().len(), 1);
            // The window up to the last journaled share: one share each.
            assert_eq!(
                allocations(&store, 9).await,
                [(HYBRID.into(), 500), (POOLED.into(), 500)]
            );
            // A second pass changes nothing.
            assert_eq!(
                store.commit(cut_off_unjournaled).await.unwrap(),
                Vec::<String>::new()
            );
            assert_eq!(store.commit(allocate_ready).await.unwrap(), []);
        }

        #[tokio::test]
        async fn the_current_window_credits_each_worker_back_from_the_newest_share() {
            let (store, journal) = open("current").await;
            let mut half = [0xff; 32];
            half[0] = 0x7f;
            let (hard, easy) = (
                Target::from_be_bytes(half),
                Target::from_be_bytes([0xff; 32]),
            );
            let by = |n, handle: &str, target| Entry {
                handle: handle.parse().unwrap(),
                ..entry(n, "pooled", POOLED, target, false)
            };
            let (a, b) = ("aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb");
            for e in [by(1, a, hard), by(2, b, easy), by(3, a, easy)] {
                journal.record(e).await.unwrap();
            }
            let current = |pool, size| store.commit(move |t| current(t, &id(pool), size));
            assert_eq!(current("hybrid", u128::MAX).await.unwrap(), None);
            // Less than W journaled: the window is all of it, A's 2 + 1 and B's 1.
            let all = current("pooled", u128::MAX).await.unwrap().unwrap();
            assert_eq!(all.total, 4 << 16);
            assert_eq!(
                (all.part(a), all.part(b), all.part("cccccccccccccccc")),
                (0.75, 0.25, 0.0)
            );
            // W of two hashes: the newest two shares, one each.
            let two = current("pooled", 2 << 16).await.unwrap().unwrap();
            assert_eq!(two.total, 2 << 16);
            assert_eq!((two.part(a), two.part(b)), (0.5, 0.5));
            // W of two and a half: A's hard share is credited half its work.
            let boundary = current("pooled", 5 << 15).await.unwrap().unwrap();
            assert_eq!(boundary.credits[a], (1 << 16) + (1 << 15));
        }

        #[tokio::test]
        async fn each_allocation_is_credited_pending_unless_the_node_refused_the_block() {
            let (store, journal) = open("ledger").await;
            let easy = Target::from_be_bytes([0xff; 32]);
            journal
                .record(entry(1, "pooled", POOLED, easy, false))
                .await
                .unwrap();
            journal
                .record(entry(2, "pooled", HYBRID, easy, true))
                .await
                .unwrap();
            let block = found(2, "pooled", 1_001);
            store
                .commit(move |t| {
                    blocks::insert(t, &block, 0)?;
                    allocate_ready(t)
                })
                .await
                .unwrap();
            // Refused before its solution share is journaled.
            let block = found(3, "pooled", 500);
            store
                .commit(move |t| {
                    blocks::insert(t, &block, 0)?;
                    blocks::record_answer(t, block.hash, "high-hash", true)
                })
                .await
                .unwrap();
            journal
                .record(entry(3, "pooled", POOLED, easy, true))
                .await
                .unwrap();
            while allocations(&store, 3).await.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }

            let ledger: Vec<(String, String, String, i64)> = store
                .commit(|t| {
                    t.prepare("SELECT block, address, kind, sats FROM ledger ORDER BY id")?
                        .query_map([], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                        })?
                        .collect()
                })
                .await
                .unwrap();
            let block = BlockHash::from_byte_array([2; 32]).to_string();
            assert_eq!(
                ledger,
                [
                    (block.clone(), HYBRID.into(), "pending".into(), 501),
                    (block, POOLED.into(), "pending".into(), 500),
                ],
                "block 2's allocations, and none of block 3's"
            );
            assert_eq!(
                allocations(&store, 3).await.len(),
                2,
                "block 3 is allocated all the same"
            );
        }
    }

    proptest! {
        #[test]
        fn mul_div_is_exact(a: u64, b: u128, c: u128) {
            let (b, c) = if b <= c { (b, c) } else { (c, b) };
            prop_assume!(c > 0);
            let (q, r) = mul_div(a, b, c);
            prop_assert!(r < c);
            // q × c + r == a × b, in 256 bits.
            let (high, low) = wide(u128::from(q), c);
            let (low, carry) = low.overflowing_add(r);
            prop_assert_eq!((high + u128::from(carry), low), wide(u128::from(a), b));
        }

        #[test]
        fn allocations_sum_exactly_to_the_amount(
            amount in 0u64..=2_100_000_000_000_000,
            works in prop::collection::vec((0u8..20, 1u128..=1u128 << 90), 1..60),
            size in 1u128..=1u128 << 95,
        ) {
            let shares = works
                .iter()
                .enumerate()
                .rev()
                .map(|(i, (address, work))| share(i as u64, &format!("addr{address:02}"), *work));
            let window = window(shares, size).unwrap();
            prop_assert!(window.total <= size);
            prop_assert_eq!(window.credits.values().sum::<u128>(), window.total);
            let allocations = allocate(amount, &window);
            prop_assert_eq!(allocations.iter().map(|(_, sats)| sats).sum::<u64>(), amount);
            // No address is more than a satoshi off its exact share.
            for (address, sats) in &allocations {
                let (floor, _) = mul_div(amount, window.credits[address], window.total);
                prop_assert!(*sats == floor || *sats == floor + 1);
            }
        }

        #[test]
        fn credit_follows_assigned_targets_not_share_count(
            amount in 0u64..=2_100_000_000_000_000,
            work in 1u128..=1u128 << 80,
            count in 1usize..20,
        ) {
            // a sends `count` shares at twice the work of each of b's 2 × `count`, interleaved.
            let mut shares = Vec::new();
            for i in 0..count as u64 {
                shares.push(share(3 * i, "a", 2 * work));
                shares.push(share(3 * i + 1, "b", work));
                shares.push(share(3 * i + 2, "b", work));
            }
            shares.reverse();
            let window = window(shares, u128::MAX).unwrap();
            prop_assert_eq!(window.credits["a"], window.credits["b"]);
            let allocations = allocate(amount, &window);
            let paid = |address: &str| {
                allocations.iter().find(|(a, _)| a == address).map_or(0, |(_, sats)| *sats)
            };
            // Equal work: equal halves, the odd satoshi to the lower address.
            prop_assert_eq!(paid("a"), amount.div_ceil(2));
            prop_assert_eq!(paid("b"), amount / 2);
        }

        #[test]
        fn a_short_window_divides_everything_it_holds(
            amount in 0u64..=2_100_000_000_000_000,
            works in prop::collection::vec(1u128..=1u128 << 60, 1..40),
        ) {
            let held: u128 = works.iter().sum();
            let shares = works
                .iter()
                .enumerate()
                .rev()
                .map(|(i, work)| share(i as u64, if i % 2 == 0 { "even" } else { "odd" }, *work));
            let window = window(shares, held + 1).unwrap();
            prop_assert_eq!((window.total, window.first), (held, 0));
            let allocations = allocate(amount, &window);
            prop_assert_eq!(allocations.iter().map(|(_, sats)| sats).sum::<u64>(), amount);
        }
    }
}
