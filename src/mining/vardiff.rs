//! The per-session vardiff controller. It aims at the configured share interval and emits share targets,
//! clamped between the floor and the ceiling.
//!
//! It estimates the miner's rate from the work of its accepted shares, each at the difficulty its own job
//! assigned, so shares still arriving on older, easier jobs after a change do not skew the estimate.

use std::time::{Duration, Instant};

use bitcoin::Target;

use super::target;
use crate::config;

/// Retarget once the window holds this many shares' worth of work at the current difficulty, or once it
/// spans the time they were due in, so a miner that went quiet is eased. Either way the estimate rests on
/// about this many expected shares: on a live Bitaxe, a 3-interval window, 3 expected shares,
/// cut a Bitaxe's difficulty 4x whenever one or none arrived.
const RETARGET_SHARES: f64 = 10.0;
/// The most one retarget multiplies or divides the difficulty by. A step goes halfway, geometrically, to the
/// estimate, which halves the noise of a 10-share sample while a far-off start still ramps at the full step.
const MAX_STEP: f64 = 4.0;
/// Changes smaller than this fraction are not worth a set_difficulty.
const DEADBAND: f64 = 0.15;

#[derive(Debug)]
pub struct Vardiff {
    floor: f64,
    ceiling: f64,
    interval: Duration,
    difficulty: f64,
    window_start: Instant,
    /// The summed difficulty of the shares accepted since `window_start`.
    window_work: f64,
}

impl Vardiff {
    /// Starts at the hinted difficulty clamped to the bounds, or at the floor without a hint.
    pub fn new(config: &config::Vardiff, hint: Option<f64>, now: Instant) -> Self {
        let clamp = |d: f64| d.clamp(config.floor, config.ceiling);
        Vardiff {
            floor: config.floor,
            ceiling: config.ceiling,
            interval: config.share_interval,
            difficulty: hint.filter(|d| d.is_finite()).map_or(config.floor, clamp),
            window_start: now,
            window_work: 0.0,
        }
    }

    pub fn difficulty(&self) -> f64 {
        self.difficulty
    }

    pub fn target(&self) -> Target {
        target::from_difficulty(self.difficulty)
    }

    /// Records an accepted share at `share_difficulty`, the difficulty its job assigned. Returns the new
    /// target when the difficulty changes.
    pub fn on_share(&mut self, share_difficulty: f64, now: Instant) -> Option<Target> {
        self.window_work += share_difficulty;
        if self.window_work >= RETARGET_SHARES * self.difficulty || self.window_is_long(now) {
            self.retarget(now)
        } else {
            None
        }
    }

    /// Call periodically: eases the difficulty of a miner that submits too rarely to trigger a retarget
    /// itself. Returns the new target when the difficulty changes.
    pub fn on_tick(&mut self, now: Instant) -> Option<Target> {
        if self.window_is_long(now) {
            self.retarget(now)
        } else {
            None
        }
    }

    fn window_is_long(&self, now: Instant) -> bool {
        now.duration_since(self.window_start) >= self.interval.mul_f64(RETARGET_SHARES)
    }

    fn retarget(&mut self, now: Instant) -> Option<Target> {
        let elapsed = now.duration_since(self.window_start).as_secs_f64();
        // The difficulty at which the observed work rate yields one share per interval.
        let aimed = self.window_work / elapsed.max(1e-3) * self.interval.as_secs_f64();
        let ratio = aimed / self.difficulty;
        self.window_start = now;
        self.window_work = 0.0;
        if (ratio - 1.0).abs() < DEADBAND {
            return None;
        }
        let step = ratio.sqrt().clamp(1.0 / MAX_STEP, MAX_STEP);
        let next = (self.difficulty * step).clamp(self.floor, self.ceiling);
        if next == self.difficulty {
            return None;
        }
        self.difficulty = next;
        Some(self.target())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(floor: f64, ceiling: f64) -> config::Vardiff {
        config::Vardiff {
            floor,
            ceiling,
            share_interval: Duration::from_secs(10),
        }
    }

    /// A miner hashing at `hashrate`, submitting a share whenever its expected work at the current
    /// difficulty is done. Returns every target the controller emitted and the last share interval.
    fn simulate(vardiff: &mut Vardiff, hashrate: f64, shares: usize) -> (Vec<Target>, Duration) {
        let start = Instant::now();
        let mut now = start;
        let mut emitted = Vec::new();
        let mut last_interval = Duration::ZERO;
        for _ in 0..shares {
            let difficulty = vardiff.difficulty();
            last_interval = Duration::from_secs_f64(difficulty * 2f64.powi(32) / hashrate);
            now += last_interval;
            emitted.extend(vardiff.on_share(difficulty, now));
        }
        (emitted, last_interval)
    }

    #[test]
    fn a_steady_rate_converges_on_the_interval() {
        // About a Bitaxe: 1.2 TH/s aims at difficulty 2794 for one share per 10 s.
        let mut vardiff = Vardiff::new(&config(1.0, 1e6), None, Instant::now());
        let (emitted, interval) = simulate(&mut vardiff, 1.2e12, 200);
        assert!(!emitted.is_empty());
        let off = (interval.as_secs_f64() - 10.0).abs() / 10.0;
        assert!(off < DEADBAND, "last share interval {interval:?}");
    }

    #[test]
    fn never_leaves_the_bounds() {
        let (floor, ceiling) = (0.5, 64.0);
        for hashrate in [1e3, 1e9, 1e12, 1e15] {
            let mut vardiff = Vardiff::new(&config(floor, ceiling), None, Instant::now());
            let (emitted, _) = simulate(&mut vardiff, hashrate, 100);
            for target in emitted {
                assert!(target <= target::from_difficulty(floor), "{hashrate} H/s");
                assert!(target >= target::from_difficulty(ceiling), "{hashrate} H/s");
            }
        }
    }

    #[test]
    fn a_floor_equal_to_the_ceiling_pins_the_difficulty() {
        let pinned = 2.5e-10;
        let mut vardiff = Vardiff::new(&config(pinned, pinned), Some(1e6), Instant::now());
        assert_eq!(vardiff.difficulty(), pinned);
        for hashrate in [1e3, 1e12] {
            let (emitted, _) = simulate(&mut vardiff, hashrate, 100);
            assert!(emitted.is_empty(), "{hashrate} H/s");
        }
        assert_eq!(
            vardiff.on_tick(Instant::now() + Duration::from_secs(3600)),
            None
        );
        assert_eq!(vardiff.difficulty(), pinned);
    }

    #[test]
    fn clamps_an_out_of_range_hint() {
        let now = Instant::now();
        assert_eq!(
            Vardiff::new(&config(1.0, 1e6), Some(1e9), now).difficulty(),
            1e6
        );
        assert_eq!(
            Vardiff::new(&config(1.0, 1e6), Some(1e-20), now).difficulty(),
            1.0
        );
        assert_eq!(
            Vardiff::new(&config(1.0, 1e6), Some(512.0), now).difficulty(),
            512.0
        );
        assert_eq!(Vardiff::new(&config(1.0, 1e6), None, now).difficulty(), 1.0);
    }

    #[test]
    fn holds_steady_under_random_arrivals() {
        // Shares arrive as a Poisson process at a Bitaxe's rate, ticked every interval as a session
        // does. The difficulty should stay near its ideal instead of lurching on a few unlucky windows.
        let hashrate = 1.2e12;
        let ideal = hashrate * 10.0 / 2f64.powi(32);
        let start = Instant::now();
        let mut vardiff = Vardiff::new(&config(1.0, 1e6), Some(ideal), start);
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut uniform = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let (mut now, mut next_tick) = (start, start);
        let (mut low, mut total) = (0, 0);
        for _ in 0..5000 {
            let expected = vardiff.difficulty() * 2f64.powi(32) / hashrate;
            let arrival = now + Duration::from_secs_f64(-expected * (1.0 - uniform()).ln());
            while next_tick < arrival {
                vardiff.on_tick(next_tick);
                next_tick += Duration::from_secs(10);
            }
            now = arrival;
            let difficulty = vardiff.difficulty();
            vardiff.on_share(difficulty, now);
            total += 1;
            low += usize::from(vardiff.difficulty() < ideal / 2.0);
        }
        assert!(
            low * 20 < total,
            "below half the ideal for {low} of {total} shares"
        );
    }

    #[test]
    fn eases_a_miner_that_went_quiet() {
        let start = Instant::now();
        let mut vardiff = Vardiff::new(&config(1.0, 1e6), Some(1024.0), start);
        assert_eq!(vardiff.on_tick(start + Duration::from_secs(90)), None);
        assert!(vardiff.on_tick(start + Duration::from_secs(100)).is_some());
        assert_eq!(
            vardiff.difficulty(),
            256.0,
            "no shares at all: the full step down"
        );
    }
}
