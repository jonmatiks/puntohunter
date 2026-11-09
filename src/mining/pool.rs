//! Pools: [`PoolId`], the identity every block, share, ledger entry, worker handle,
//! and metric label carries, and [`Pool`], the parameters a configured pool mines by.

use std::{fmt, net::SocketAddr, str::FromStr, sync::Arc};

use bitcoin::Amount;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Serialize, Serializer};

use crate::{
    config::{Fee, PPM, Portion, Window},
    stratum::Transport,
};

/// The longest pool id, in characters.
pub const MAX_ID_LEN: usize = 32;

/// A pool's id: 1 to [`MAX_ID_LEN`] of `a–z`, `0–9`, and `-`, and never `fleet`. Cheap to clone. A row read
/// from the store parses to one whatever the config holds, so a pool no longer configured still shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PoolId(Arc<str>);

/// Why a text is not a pool id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidId {
    #[error("takes 1 to {MAX_ID_LEN} lower-case letters, digits, and '-'")]
    Form,
    /// A worker handle hashes `<id>:<username>` and a fleet handle `fleet:<address>`, so a pool with this id
    /// would give a bare-address worker its fleet's handle.
    #[error("\"fleet\" is reserved for fleet handles")]
    Fleet,
}

impl PoolId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PoolId {
    type Err = InvalidId;

    fn from_str(text: &str) -> Result<Self, InvalidId> {
        let form = (1..=MAX_ID_LEN).contains(&text.len())
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        match text {
            _ if !form => Err(InvalidId::Form),
            "fleet" => Err(InvalidId::Fleet),
            _ => Ok(PoolId(text.into())),
        }
    }
}

impl fmt::Display for PoolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for PoolId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl ToSql for PoolId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for PoolId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value
            .as_str()?
            .parse()
            .map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

/// A configured pool: what it is called, where miners reach it, and how its blocks pay. Built once from the
/// config and shared as an `Arc`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pool {
    pub id: PoolId,
    /// What the portal calls it.
    pub name: String,
    /// The Stratum V1 and V2 listen addresses; a protocol left out has no listener.
    pub v1: Option<SocketAddr>,
    pub v2: Option<SocketAddr>,
    /// The ports miners reach, each its listener's unless configured; None where the protocol is off.
    pub public_v1: Option<u16>,
    pub public_v2: Option<u16>,
    /// What a block's finder is paid of its reward, capped at the miner portion.
    pub finder: Portion,
    /// The fee outputs every coinbase of the pool carries after the miner portion, in config order.
    pub fees: Vec<Fee>,
    /// The PPLNS window policy of its blocks.
    pub window: Window,
    /// The range each worker may choose its own finder part in; None where every worker gets `finder`.
    pub personal: Option<Personal>,
    /// Whether its coinbases name every worker, or each worker that asks with `+name`; never
    /// both.
    pub worker_names: bool,
    pub worker_names_opt_in: bool,
}

/// A pool's `personal` range: each bound a portion, as `finder` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Personal {
    pub min: Portion,
    pub max: Portion,
}

impl Personal {
    /// The finder's part of `reward` for a worker asking `part`: in satoshis of this reward, raised to `min`,
    /// then lowered to `max`, so `max` wins where a percent bound and a fixed one cross.
    pub fn clamp(&self, reward: Amount, part: Portion) -> Amount {
        part.of(reward)
            .max(self.min.of(reward))
            .min(self.max.of(reward))
    }
}

/// What a pool's `finder` makes it, which only the portal's wording reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A finder of 100%: each block pays its finder the whole miner portion.
    Solo,
    /// A finder of 0: each block pays the pool wallet, shared by work.
    Pooled,
    /// A percent in between, or a fixed amount: a part to the finder, the rest shared.
    Hybrid,
    /// Each worker's own part, within the pool's `personal` range.
    Personal,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Solo => "solo",
            Kind::Pooled => "pooled",
            Kind::Hybrid => "hybrid",
            Kind::Personal => "personal",
        }
    }
}

impl Pool {
    /// Whether a block can pay a finder output: unless its `finder` is 0, or with `personal`, unless its
    /// `max` is, since every part is clamped to the range.
    pub fn can_pay_finder(&self) -> bool {
        let most = self.personal.map_or(self.finder, |range| range.max);
        !matches!(most, Portion::Ppm(0) | Portion::Amount(Amount::ZERO))
    }

    /// Whether a block can pay a pool output, so its shares are journaled: unless its `finder` is 100%, or
    /// with `personal`, unless its `min` is. A fixed amount can pay both.
    pub fn can_pay_pool(&self) -> bool {
        self.personal.map_or(self.finder, |range| range.min) != Portion::Ppm(PPM)
    }

    /// Whether its coinbases name a worker that `asked` with `+name` or not: always under `worker_names`, and
    /// under `worker_names_opt_in` when it asked.
    pub fn writes_name(&self, asked: bool) -> bool {
        self.worker_names || (self.worker_names_opt_in && asked)
    }

    /// The transports it serves: V1, V2, or both, as its listeners say.
    pub fn transports(&self) -> Vec<Transport> {
        [(self.v1, Transport::V1), (self.v2, Transport::V2Native)]
            .into_iter()
            .filter_map(|(listener, transport)| listener.map(|_| transport))
            .collect()
    }

    pub fn kind(&self) -> Kind {
        if self.personal.is_some() {
            return Kind::Personal;
        }
        match (self.can_pay_finder(), self.can_pay_pool()) {
            (true, false) => Kind::Solo,
            (false, _) => Kind::Pooled,
            (true, true) => Kind::Hybrid,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The pool id `text`, for tests.
    pub fn id(text: &str) -> PoolId {
        text.parse().unwrap()
    }

    /// A pool paying its finder `finder`, with no listeners or fees.
    pub fn paying(id: &str, finder: Portion) -> Pool {
        Pool {
            id: self::id(id),
            name: id.to_owned(),
            v1: None,
            v2: None,
            public_v1: None,
            public_v2: None,
            finder,
            fees: Vec::new(),
            window: Window {
                window_multiplier: 2.0,
                window_min_work: 0.0,
            },
            personal: None,
            worker_names: false,
            worker_names_opt_in: false,
        }
    }

    #[test]
    fn an_id_takes_lower_case_letters_digits_and_hyphens_and_never_fleet() {
        for good in [
            "solo",
            "pooled",
            "hybrid",
            "my-bonus",
            "p2",
            "-",
            &"a".repeat(32),
        ] {
            assert_eq!(good.parse::<PoolId>().unwrap().as_str(), good);
        }
        for bad in [
            "",
            &"a".repeat(33),
            "Solo",
            "my_pool",
            "my pool",
            "pool.1",
            "café",
        ] {
            assert_eq!(bad.parse::<PoolId>(), Err(InvalidId::Form), "{bad:?}");
        }
        assert_eq!("fleet".parse::<PoolId>(), Err(InvalidId::Fleet));
        assert!("fleets".parse::<PoolId>().is_ok());
    }

    #[test]
    fn the_finder_decides_what_a_pool_can_pay_and_its_kind() {
        let fixed = |sats| Portion::Amount(Amount::from_sat(sats));
        for (finder, finder_paid, pool_paid, kind) in [
            (Portion::Ppm(PPM), true, false, Kind::Solo),
            (Portion::Ppm(0), false, true, Kind::Pooled),
            (fixed(0), false, true, Kind::Pooled),
            (Portion::Ppm(333_300), true, true, Kind::Hybrid),
            (Portion::Ppm(1), true, true, Kind::Hybrid),
            (fixed(50_000_000), true, true, Kind::Hybrid),
            // A fixed amount above any reward pays the finder all of every block, yet stays a hybrid pool.
            (fixed(21_000_000 * 100_000_000), true, true, Kind::Hybrid),
        ] {
            let pool = paying("p", finder);
            assert_eq!(
                (pool.can_pay_finder(), pool.can_pay_pool(), pool.kind()),
                (finder_paid, pool_paid, kind),
                "{finder:?}"
            );
        }
    }

    #[test]
    fn personal_bounds_decide_what_a_pool_can_pay() {
        let fixed = |sats| Portion::Amount(Amount::from_sat(sats));
        for (min, max, finder_paid, pool_paid) in [
            (Portion::Ppm(0), Portion::Ppm(500_000), true, true),
            (Portion::Ppm(0), Portion::Ppm(0), false, true),
            (Portion::Ppm(0), fixed(0), false, true),
            (Portion::Ppm(PPM), Portion::Ppm(PPM), true, false),
            (fixed(100_000), Portion::Ppm(PPM), true, true),
            (Portion::Ppm(0), Portion::Ppm(PPM), true, true),
        ] {
            let pool = Pool {
                personal: Some(Personal { min, max }),
                ..paying("p", Portion::Ppm(0))
            };
            assert_eq!(
                (pool.can_pay_finder(), pool.can_pay_pool(), pool.kind()),
                (finder_paid, pool_paid, Kind::Personal),
                "{min:?} {max:?}"
            );
        }
    }

    #[test]
    fn a_part_is_clamped_in_satoshis_of_each_reward_with_max_winning() {
        let fixed = |sats| Portion::Amount(Amount::from_sat(sats));
        let range = Personal {
            min: fixed(50_000_000),
            max: Portion::Ppm(100_000),
        };
        let reward = |btc: u64| Amount::from_sat(btc * 100_000_000);
        // At 10 BTC the range is 0.5 to 1 BTC: a part inside it stands, one outside goes to the nearer bound.
        assert_eq!(
            range.clamp(reward(10), fixed(70_000_000)),
            Amount::from_sat(70_000_000)
        );
        assert_eq!(
            range.clamp(reward(10), Portion::Ppm(0)),
            Amount::from_sat(50_000_000)
        );
        assert_eq!(
            range.clamp(reward(10), Portion::Ppm(PPM)),
            Amount::from_sat(100_000_000)
        );
        // At 3.2 BTC the bounds cross, 0.5 BTC above 10% of it, and max wins.
        assert_eq!(
            range.clamp(reward(3) + Amount::from_sat(20_000_000), Portion::Ppm(0)),
            Amount::from_sat(32_000_000)
        );
        let percents = Personal {
            min: Portion::Ppm(0),
            max: Portion::Ppm(500_000),
        };
        assert_eq!(
            percents.clamp(Amount::from_sat(1_001), Portion::Ppm(333_300)),
            Amount::from_sat(333)
        );
    }
}
