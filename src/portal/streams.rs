//! What the live streams share (API reference section 1.8): one snapshot of each
//! watched fleet or worker, computed at most once a second and shared by every subscriber of its stream. Store
//! reads then grow with the number of fleets and workers watched, not with their viewers and shares.
//!
//! Open streams are the costliest thing the portal serves, so they are capped per client address and in all.
//! The client address is the peer's, or behind the configured reverse proxy the one it
//! forwards; the lookup limit keys on it too.

use std::{
    collections::HashMap,
    hash::Hash,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::http::HeaderMap;
use tokio::{
    sync::broadcast::{self, error::RecvError},
    time::{Instant, Interval},
};

use super::api::ApiError;
use crate::telemetry::handle::Handle;

/// How long a refused stream's client should wait before it tries again.
const RETRY_SECS: u64 = 10;

/// The client a request came from: the peer, or, when the peer is `trusted_proxy`, the last address its
/// `X-Forwarded-For` names, the one the proxy added. From any other peer the header is ignored, so a client
/// cannot pick its own address.
pub fn client(peer: SocketAddr, headers: &HeaderMap, trusted_proxy: Option<IpAddr>) -> IpAddr {
    let peer = peer.ip().to_canonical();
    if trusted_proxy.map(|proxy| proxy.to_canonical()) != Some(peer) {
        return peer;
    }
    headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .and_then(|last| last.trim().parse::<IpAddr>().ok())
        .map_or(peer, |forwarded| forwarded.to_canonical())
}

/// The open streams, in all and by client address.
pub struct StreamCaps {
    per_ip: u32,
    max: u32,
    open: Mutex<(u32, HashMap<IpAddr, u32>)>,
}

/// A stream's place under the caps, given back when the stream is dropped.
pub struct StreamGuard {
    caps: Arc<StreamCaps>,
    client: IpAddr,
}

impl StreamCaps {
    pub fn new(per_ip: u32, max: u32) -> Arc<Self> {
        Arc::new(StreamCaps {
            per_ip,
            max,
            open: Mutex::new((0, HashMap::new())),
        })
    }

    /// A place for one more stream from `client`, or why there is none.
    pub fn take(self: &Arc<Self>, client: IpAddr) -> Result<StreamGuard, ApiError> {
        let mut open = self.open.lock().unwrap();
        let (total, by_client) = &mut *open;
        let mine = by_client.entry(client).or_default();
        if *mine >= self.per_ip {
            if *mine == 0 {
                by_client.remove(&client);
            }
            return Err(ApiError::TooManyStreams(RETRY_SECS));
        }
        if *total >= self.max {
            if *mine == 0 {
                by_client.remove(&client);
            }
            return Err(ApiError::StreamCapacity(RETRY_SECS));
        }
        *mine += 1;
        *total += 1;
        Ok(StreamGuard {
            caps: self.clone(),
            client,
        })
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let mut open = self.caps.open.lock().unwrap();
        let (total, by_client) = &mut *open;
        *total -= 1;
        if let Some(mine) = by_client.get_mut(&self.client) {
            *mine -= 1;
            if *mine == 0 {
                by_client.remove(&self.client);
            }
        }
    }
}

/// The least time between two computations of one snapshot. A change waits at most this long to show, inside
/// the 2 s the stream tests hold.
pub const COALESCE: Duration = Duration::from_secs(1);

/// How long a snapshot nobody asked for is kept before it is forgotten.
const KEPT: Duration = Duration::from_secs(60);

/// The latest snapshot of each key, and when it was computed.
pub struct Snapshots<K, V> {
    entries: Mutex<HashMap<K, Entry<V>>>,
}

/// A key's snapshot and when it was computed, locked while it is computed again.
type Entry<V> = Arc<tokio::sync::Mutex<Option<(Instant, Arc<V>)>>>;

impl<K, V> Default for Snapshots<K, V> {
    fn default() -> Self {
        Snapshots {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash + Clone, V> Snapshots<K, V> {
    /// The snapshot of `key` computed at `not_before` or later, from `compute`. One computed earlier is
    /// computed again, but no sooner than [`COALESCE`] after the last, and callers waiting meanwhile share
    /// that computation.
    pub async fn get<F, Fut>(&self, key: K, not_before: Instant, compute: F) -> Arc<V>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V>,
    {
        let entry = {
            let mut entries = self.entries.lock().unwrap();
            let now = Instant::now();
            // Forgets the keys nobody has asked for lately; one being computed is kept.
            entries.retain(|_, entry| match entry.try_lock() {
                Ok(snapshot) => snapshot.as_ref().is_some_and(|(at, _)| now - *at < KEPT),
                Err(_) => true,
            });
            entries.entry(key).or_default().clone()
        };
        let mut snapshot = entry.lock().await;
        if let Some((at, value)) = &*snapshot {
            if *at >= not_before {
                return value.clone();
            }
            tokio::time::sleep_until(*at + COALESCE).await;
        }
        let at = Instant::now();
        let value = Arc::new(compute().await);
        *snapshot = Some((at, value.clone()));
        value
    }
}

/// The `not_before` of a stream's periodic or first event: a snapshot up to [`COALESCE`] old will do.
pub fn recent() -> Instant {
    let now = Instant::now();
    now.checked_sub(COALESCE).unwrap_or(now)
}

/// Waits for a stream's next event: a change to a device `ours` accepts, or the stream's refresh. Returns the
/// `not_before` its snapshot needs, or None once telemetry is gone.
pub async fn next(
    changes: &mut broadcast::Receiver<Handle>,
    refresh: &mut Interval,
    ours: impl Fn(Handle) -> bool,
) -> Option<Instant> {
    loop {
        tokio::select! {
            change = changes.recv() => match change {
                Ok(changed) if ours(changed) => return Some(Instant::now()),
                Ok(_) => continue,
                // Behind: some change was missed, so send the current values.
                Err(RecvError::Lagged(_)) => return Some(Instant::now()),
                Err(RecvError::Closed) => return None,
            },
            _ = refresh.tick() => return Some(recent()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::http::HeaderValue;

    use super::*;

    fn peer(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 40_000)
    }

    #[test]
    fn the_forwarded_address_counts_only_from_the_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.66, 198.51.100.7"),
        );
        let proxy = Some("127.0.0.1".parse().unwrap());
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        assert_eq!(
            client(peer("127.0.0.1"), &headers, proxy),
            ip("198.51.100.7"),
            "the entry the proxy added"
        );
        assert_eq!(
            client(peer("192.0.2.9"), &headers, proxy),
            ip("192.0.2.9"),
            "another peer's is ignored"
        );
        assert_eq!(
            client(peer("127.0.0.1"), &headers, None),
            ip("127.0.0.1"),
            "no proxy configured"
        );
        assert_eq!(
            client(peer("::ffff:127.0.0.1"), &headers, proxy),
            ip("198.51.100.7"),
            "a mapped address"
        );
        assert_eq!(
            client(peer("127.0.0.1"), &HeaderMap::new(), proxy),
            ip("127.0.0.1"),
            "no header"
        );
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("not an address"),
        );
        assert_eq!(
            client(peer("127.0.0.1"), &headers, proxy),
            ip("127.0.0.1"),
            "an unreadable header"
        );
    }

    #[test]
    fn streams_are_capped_per_client_and_in_all_and_a_closed_one_frees_its_place() {
        let caps = StreamCaps::new(2, 3);
        let (a, b) = (
            "198.51.100.1".parse().unwrap(),
            "198.51.100.2".parse().unwrap(),
        );
        let first = caps.take(a).unwrap();
        let second = caps.take(a).unwrap();
        assert!(matches!(
            caps.take(a),
            Err(ApiError::TooManyStreams(RETRY_SECS))
        ));
        drop(first);
        let third = caps.take(a).unwrap();
        let fourth = caps.take(b).unwrap();
        assert!(matches!(
            caps.take(b),
            Err(ApiError::StreamCapacity(RETRY_SECS))
        ));
        assert_eq!(caps.open.lock().unwrap().0, 3);
        drop(fourth);
        assert!(
            !caps.open.lock().unwrap().1.contains_key(&b),
            "a client with none open is forgotten"
        );
        drop((second, third));
        assert_eq!(caps.open.lock().unwrap().0, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn subscribers_within_a_second_share_one_computation() {
        let snapshots = Arc::new(Snapshots::<u8, usize>::default());
        let computed = Arc::new(AtomicUsize::new(0));
        let compute = |computed: Arc<AtomicUsize>| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            computed.fetch_add(1, Ordering::SeqCst)
        };
        let mut subscribers = Vec::new();
        for i in 0..10 {
            let (snapshots, computed) = (snapshots.clone(), computed.clone());
            subscribers.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(i * 90)).await;
                *snapshots.get(1, recent(), || compute(computed)).await
            }));
        }
        for subscriber in subscribers {
            assert_eq!(
                subscriber.await.unwrap(),
                0,
                "the first computation, shared"
            );
        }
        assert_eq!(computed.load(Ordering::SeqCst), 1);

        // Another key is its own.
        assert_eq!(
            *snapshots
                .get(2, recent(), || compute(computed.clone()))
                .await,
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_is_computed_again_but_no_sooner_than_a_second_after_the_last() {
        let snapshots = Snapshots::<u8, Instant>::default();
        let first = *snapshots.get(1, recent(), async || Instant::now()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        // A change after the snapshot needs a new one, a second after the last.
        let changed = Instant::now();
        let second = *snapshots.get(1, changed, async || Instant::now()).await;
        assert_eq!(second - first, COALESCE);
        // Asked for before the change, the snapshot after it will do.
        assert_eq!(
            *snapshots.get(1, changed, async || Instant::now()).await,
            second
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        let third = *snapshots
            .get(1, Instant::now(), async || Instant::now())
            .await;
        assert_eq!(
            third - second,
            Duration::from_secs(5),
            "at once, a second having passed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_snapshot_nobody_asks_for_is_forgotten() {
        let snapshots = Snapshots::<u8, u8>::default();
        snapshots.get(1, recent(), async || 0).await;
        tokio::time::sleep(KEPT + Duration::from_secs(1)).await;
        snapshots.get(2, recent(), async || 0).await;
        let entries = snapshots.entries.lock().unwrap();
        assert!(!entries.contains_key(&1) && entries.contains_key(&2));
    }
}
