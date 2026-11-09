//! The username lookup: the full username, sent in `POST /api/v1/lookup`'s body and parsed as
//! mining.authorize parses it, finds its address's fleet if that worker has mined here in any pool: the private
//! fleet of its address and fleet password, with one, or the address's open fleet. The username and the
//! password are never echoed or logged, and an address alone finds nothing.
//!
//! Lookups are limited per client address, in memory, to slow anyone sweeping addresses against common worker
//! names. Behind the trusted proxy the client address is the one it forwards (streams::client).

use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use super::Portal;
use crate::{
    mining::pool::PoolId,
    stratum::username,
    telemetry::{fleet, handle::Handle, rollup},
};

/// The window the limit counts over.
const MINUTE: Duration = Duration::from_secs(60);

/// The lookups each IP address made in the last minute.
pub struct Lookups {
    limit: usize,
    recent: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl Lookups {
    /// A limit of `per_minute` lookups an address.
    pub fn new(per_minute: u32) -> Self {
        Lookups {
            limit: per_minute as usize,
            recent: Mutex::new(HashMap::new()),
        }
    }

    /// Counts a lookup from `ip` at `now`, or, over the limit, says how long until the next is allowed.
    fn admit(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        let mut recent = self.recent.lock().unwrap();
        // Forgets the addresses quiet for a minute, so the map holds only the last minute's.
        recent.retain(|_, times| {
            while times
                .front()
                .is_some_and(|&at| now.saturating_duration_since(at) >= MINUTE)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        let times = recent.entry(ip).or_default();
        if times.len() >= self.limit {
            let oldest = times[times.len() - self.limit];
            return Err(MINUTE.saturating_sub(now.saturating_duration_since(oldest)));
        }
        times.push_back(now);
        Ok(())
    }
}

/// Why a lookup reaches no fleet.
pub enum Refusal {
    /// Not a username.
    Invalid,
    /// An address alone.
    WorkerNameRequired,
    /// No worker has mined under it here.
    NotFound,
    Failed,
}

impl Lookups {
    /// Counts a lookup from `ip`, or, over the limit, says how many whole seconds until the next is allowed.
    pub fn admit_now(&self, ip: IpAddr) -> Result<(), u64> {
        self.admit(ip, Instant::now())
            .map_err(|wait| (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1))
    }
}

impl Portal {
    /// The fleet of `username`'s address, if that worker has mined here in any pool, configured or not: the
    /// private fleet of its fleet secret, after a `#` or in `password` as a V1 device sets it, or
    /// its open fleet without one. The username is parsed as mining.authorize parses it, so an upper-case
    /// bech32 address matches, after a pasted suffix of options is cut off at its first `+`, well-formed or
    /// not: options are not identity. A wrong password names a worker that never mined, so it is not found.
    pub async fn find(&self, username: &str, password: &str) -> Result<Handle, Refusal> {
        let username = username.trim();
        let username = username
            .split_once('+')
            .map_or(username, |(identity, _)| identity);
        let username = username::parse(username, self.network)
            .and_then(|username| username.with_password(password))
            .map_err(|_| Refusal::Invalid)?;
        if username.worker.is_empty() {
            return Err(Refusal::WorkerNameRequired);
        }
        let failed = |e| {
            tracing::error!("cannot look a worker up: {e}");
            Refusal::Failed
        };
        let mut pools: Vec<PoolId> = self.pools.iter().map(|pool| pool.id.clone()).collect();
        let stored = self.reader.read(fleet::pools).await.map_err(failed)?;
        pools.extend(
            stored
                .into_iter()
                .filter(|id| self.configured(id).is_none()),
        );
        for pool in &pools {
            let handle = self.handles.handle(pool, &username);
            if self.knows(handle).await.map_err(failed)? {
                return Ok(self
                    .handles
                    .fleet(&username.address, username.secret.as_ref()));
            }
        }
        Err(Refusal::NotFound)
    }
}

impl Portal {
    /// Whether the worker has mined here: live since the pool started, or in the rollups, blocks, or fleets.
    pub async fn knows(&self, handle: Handle) -> Result<bool, crate::store::StoreError> {
        if self.telemetry.live(handle).is_some() {
            return Ok(true);
        }
        self.reader
            .read(move |connection| rollup::known(connection, handle))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_each_address_over_a_sliding_minute() {
        let lookups = Lookups::new(2);
        let (a, b): (IpAddr, IpAddr) = (
            "198.51.100.1".parse().unwrap(),
            "198.51.100.2".parse().unwrap(),
        );
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        assert_eq!(lookups.admit(a, at(0)), Ok(()));
        assert_eq!(lookups.admit(a, at(20)), Ok(()));
        assert_eq!(lookups.admit(a, at(30)), Err(Duration::from_secs(30)));
        assert_eq!(lookups.admit(b, at(30)), Ok(()), "another address");
        // A refusal is not counted: the first lookup's minute ends at 60.
        assert_eq!(lookups.admit(a, at(59)), Err(Duration::from_secs(1)));
        assert_eq!(lookups.admit(a, at(60)), Ok(()));
        assert_eq!(lookups.admit(a, at(61)), Err(Duration::from_secs(19)));
        // Quiet addresses are forgotten.
        assert_eq!(lookups.admit(b, at(200)), Ok(()));
        assert_eq!(lookups.recent.lock().unwrap().len(), 1);
    }
}
