//! The odds of a block at a hashrate: the chances over a day, a month, and a year, and the waits,
//! as raw values the API sends and the portal words as odds, never as a forecast.

/// Seconds in a day, and in the month and year the odds are quoted over.
const DAY: f64 = 86_400.0;
const MONTH: f64 = 30.0 * DAY;
const YEAR: f64 = 365.0 * DAY;

/// The chance of a block over a day, a month, and a year at one hashrate, and the waits for one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Odds {
    pub day: f64,
    pub month: f64,
    pub year: f64,
    /// The average wait, in seconds: D·2^32 / H.
    pub average: f64,
    /// The median wait, ln 2 times the average.
    pub median: f64,
}

impl Odds {
    /// The odds at `hash_rate` hashes a second against network difficulty `difficulty`: a block takes D·2^32
    /// hashes on average, so the chance over t seconds is 1 − e^(−H·t / (D·2^32)). None without a hashrate.
    pub fn new(hash_rate: f64, difficulty: f64) -> Option<Odds> {
        if !(hash_rate > 0.0 && difficulty > 0.0) {
            return None;
        }
        let average = difficulty * 2f64.powi(32) / hash_rate;
        let chance = |t: f64| -(-t / average).exp_m1();
        Some(Odds {
            day: chance(DAY),
            month: chance(MONTH),
            year: chance(YEAR),
            average,
            median: std::f64::consts::LN_2 * average,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a / b - 1.0).abs() < 1e-9
    }

    #[test]
    fn the_odds_match_hand_computed_values() {
        // A Bitaxe's 1 TH/s against difficulty 100 T: a block takes 100e12 · 2^32 = 4.295e23 hashes, an
        // average wait of 4.295e11 s, about 13,600 years.
        let odds = Odds::new(1e12, 100e12).unwrap();
        let average = 100e12 * 4_294_967_296.0 / 1e12;
        assert!(close(odds.average, average));
        assert!(close(odds.median, std::f64::consts::LN_2 * average));
        // Far below 1, the chance over t is t / average, less a relative (t / average) / 2.
        assert!((odds.day / (86_400.0 / average) - 1.0).abs() < 1e-6);
        assert!((odds.year / (365.0 * 86_400.0 / average) - 1.0).abs() < 1e-4);

        // At an even chance per day: 1 − e^(−1) a day, 1 − e^(−30) a month.
        let even = Odds::new(1.0, 86_400.0 / 4_294_967_296.0).unwrap();
        assert!(close(even.day, 1.0 - (-1f64).exp()));
        assert!(close(even.month, 1.0 - (-30f64).exp()));
        assert!(close(even.average, 86_400.0));
    }

    #[test]
    fn there_are_no_odds_without_a_hash_rate_or_a_difficulty() {
        assert_eq!(Odds::new(0.0, 100e12), None);
        assert_eq!(Odds::new(1e12, 0.0), None);
        assert_eq!(Odds::new(f64::NAN, 100e12), None);
    }
}
