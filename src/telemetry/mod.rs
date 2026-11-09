//! Worker telemetry: each worker's opaque handle, and its live values, kept in memory and
//! updated inline by the sessions that mine for it.
//!
//! A worker is known by its handle, so one username on several connections at once, as two Bitaxes sharing
//! one, is one worker: its hashrate sums theirs and its connections are counted. Each connection or channel
//! holds a [`Link`], and dropping the link is the disconnect.
//!
//! Beside the live values, each share is counted into its worker's current rollup period, which the rollup
//! task writes once the period ends.

pub mod fleet;
pub mod handle;
pub mod rollup;
pub mod round;

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use tokio::sync::broadcast;

use self::{handle::Handle, round::Round};
use crate::{config::Portion, geo::Country, mining::pool::PoolId, stratum::Transport};

/// How far back the hash-rate estimate looks. A Bitaxe at its vardiff aim sends about 60 shares in it.
pub const WINDOW: Duration = Duration::from_secs(600);
/// The short hash-rate window: quick to follow a change, but noisy, with about 6 shares in it at that aim.
pub const MINUTE: Duration = Duration::from_secs(60);
/// Changes held for a subscriber that falls behind. One that falls further behind misses some, and reads the
/// current values on the next.
const CHANGES: usize = 1024;

/// Unix seconds at `at`.
pub fn unix(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The rejection classes.
const COUNT: usize = 6;

/// A device's status: what its owner most needs to see at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Connected, with a share accepted in the last [`WINDOW`].
    Online,
    /// Connected, with none.
    Idle,
    /// Disconnected, last seen at these Unix seconds.
    Offline { last_seen: u64 },
}

impl Status {
    /// The status at Unix time `now` of a device that is `connected` or not, whose last accepted share and
    /// last activity were at those Unix times.
    pub fn new(connected: bool, last_share: Option<u64>, last_seen: u64, now: u64) -> Self {
        match last_share {
            _ if !connected => Status::Offline { last_seen },
            Some(at) if now.saturating_sub(at) <= WINDOW.as_secs() => Status::Online,
            _ => Status::Idle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Online => "online",
            Status::Idle => "idle",
            Status::Offline { .. } => "offline",
        }
    }
}

/// Why a share was refused: the rejection classes, plus a submission that does not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reject {
    Stale,
    Duplicate,
    LowDifficulty,
    BadVersion,
    Unauthorized,
    Invalid,
}

impl Reject {
    fn index(self) -> usize {
        self as usize
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Reject::Stale => "stale",
            Reject::Duplicate => "duplicate",
            Reject::LowDifficulty => "low_difficulty",
            Reject::BadVersion => "bad_version",
            Reject::Unauthorized => "unauthorized",
            Reject::Invalid => "invalid",
        }
    }
}

/// A worker's live values, current to the second.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub handle: Handle,
    pub pool: PoolId,
    /// The transports of the connections open now, or of the last one when none is.
    pub transports: Vec<Transport>,
    pub connections: usize,
    /// Hashes a second, estimated from the work accepted over the last [`WINDOW`].
    pub hash_rate: f64,
    /// Whether the estimate's window reaches back before the worker connected, so it is still filling.
    pub estimating: bool,
    /// The same over the last [`MINUTE`].
    pub minute_hash_rate: f64,
    pub minute_estimating: bool,
    pub last_share: Option<SystemTime>,
    /// The highest difficulty a share's hash reached, 0 before the first.
    pub best_share: f64,
    /// The difficulty of the target last assigned, while connected.
    pub difficulty: Option<f64>,
    /// When it last connected, disconnected, or had a share accepted or refused.
    pub last_seen: SystemTime,
    /// When its current session began: the first of the connections open now. None while disconnected.
    pub connected_since: Option<SystemTime>,
    /// What its last connection chose.
    pub choices: Choices,
    /// What the stats lists know of it; None until its connection describes it.
    pub listing: Option<Listing>,
}

/// What the stats lists know of a worker: its fleet, by handle and by its address's shortened
/// form, never the whole address, and its miner type.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub fleet: Handle,
    /// The address as [`fleet::shortened`] gives it; None in a private fleet, whose rows show no address.
    pub short_address: Option<String>,
    /// The miner type its connections named last; None while none has named one.
    pub miner: Option<String>,
    /// The country of its newest connection, from its peer address; None without `[geo]`, or when the
    /// database gives none.
    pub country: Option<Country>,
}

/// A connected worker as the stats lists count it: what they know of it, and its hashrate.
#[derive(Debug, Clone, PartialEq)]
pub struct Connected {
    pub listing: Option<Listing>,
    /// Over [`WINDOW`].
    pub hash_rate: f64,
    pub estimating: bool,
}

/// A worker's choices as its connection opened last bound them: the finder part it asked for, None for the
/// pool's, whether it asked for its name with `+name`, and whether its pool writes that name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Choices {
    pub part: Option<Portion>,
    pub asked_name: bool,
    pub name_written: bool,
}

/// One worker's shares over one rollup period on one transport.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Unix seconds at the period's start.
    pub start: u64,
    pub handle: Handle,
    pub pool: PoolId,
    pub transport: Transport,
    pub accepted: u64,
    /// The work of the targets the accepted shares were credited at.
    pub work: f64,
    rejected: [u64; COUNT],
    /// The highest difficulty an accepted share's hash reached, 0 when none was accepted.
    pub best: f64,
}

impl Row {
    pub fn rejected(&self, class: Reject) -> u64 {
        self.rejected[class.index()]
    }

    /// Its refused shares, every class together.
    pub fn refused(&self) -> u64 {
        self.rejected.iter().sum()
    }
}

/// The telemetry of every worker since the pool started, or since it was last active.
pub struct Telemetry {
    /// Seconds a rollup period lasts.
    period: u64,
    state: Mutex<State>,
    /// Each accepted share counts in it.
    round: Arc<Round>,
    changes: broadcast::Sender<Handle>,
}

#[derive(Default)]
struct State {
    workers: HashMap<Handle, Worker>,
    /// The rollup rows of the periods not yet written, by handle, transport, and period start.
    periods: HashMap<(Handle, Transport, u64), Row>,
    /// The rows last taken out for writing, with the boundary they were taken out before, so a worker's
    /// totals count them until the store holds them. They stay until the next are taken out, a period later,
    /// so a reader that read the store just before their write still counts them.
    writing: Option<(u64, Vec<Row>)>,
}

impl State {
    /// The row for `handle`'s shares over `transport` at Unix time `at`.
    fn row(
        &mut self,
        period: u64,
        handle: Handle,
        transport: Transport,
        at: SystemTime,
    ) -> Option<&mut Row> {
        let pool = self.workers.get(&handle)?.pool.clone();
        let unix = at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let start = unix - unix % period;
        Some(
            self.periods
                .entry((handle, transport, start))
                .or_insert_with(|| Row {
                    start,
                    handle,
                    pool,
                    transport,
                    accepted: 0,
                    work: 0.0,
                    rejected: [0; COUNT],
                    best: 0.0,
                }),
        )
    }
}

struct Worker {
    pool: PoolId,
    connections: BTreeMap<Transport, usize>,
    /// The transport of the connection opened last.
    transport: Transport,
    /// Where the hash-rate estimate starts: when the worker connected with no work in the window.
    since: Instant,
    /// Accepted shares in the window: when each arrived, and its work.
    shares: VecDeque<(Instant, f64)>,
    last_share: Option<SystemTime>,
    best_share: f64,
    difficulty: Option<f64>,
    /// When the worker last connected, disconnected, or had a share.
    active: Instant,
    /// The same, by the wall clock.
    seen: SystemTime,
    /// When the first of the connections open now opened.
    session: Option<SystemTime>,
    /// What the connection opened last chose.
    choices: Choices,
    listing: Option<Listing>,
}

impl Worker {
    fn new(pool: PoolId, transport: Transport, now: Instant, at: SystemTime) -> Self {
        Worker {
            pool,
            connections: BTreeMap::new(),
            transport,
            since: now,
            shares: VecDeque::new(),
            last_share: None,
            best_share: 0.0,
            difficulty: None,
            active: now,
            seen: at,
            session: None,
            choices: Choices::default(),
            listing: None,
        }
    }

    fn connections(&self) -> usize {
        self.connections.values().sum()
    }

    /// Drops the shares older than the window.
    fn prune(&mut self, now: Instant) {
        while let Some(&(at, _)) = self.shares.front() {
            if now.saturating_duration_since(at) <= WINDOW {
                break;
            }
            self.shares.pop_front();
        }
    }

    /// The work in `window`, at most [`WINDOW`], over the time it covers: at most the window, and at least a
    /// second, so the first share does not read as an absurd rate.
    fn hash_rate(&self, now: Instant, window: Duration) -> f64 {
        let work: f64 = self
            .shares
            .iter()
            .filter(|(at, _)| now.saturating_duration_since(*at) <= window)
            .map(|(_, work)| work)
            .sum();
        let span = now
            .saturating_duration_since(self.since)
            .clamp(Duration::from_secs(1), window);
        work / span.as_secs_f64()
    }

    fn live(&self, handle: Handle, now: Instant) -> Live {
        let open: Vec<Transport> = self
            .connections
            .iter()
            .filter(|(_, n)| **n > 0)
            .map(|(transport, _)| *transport)
            .collect();
        let connections = self.connections();
        Live {
            handle,
            pool: self.pool.clone(),
            transports: if open.is_empty() {
                vec![self.transport]
            } else {
                open
            },
            connections,
            hash_rate: self.hash_rate(now, WINDOW),
            estimating: now.saturating_duration_since(self.since) < WINDOW,
            minute_hash_rate: self.hash_rate(now, MINUTE),
            minute_estimating: now.saturating_duration_since(self.since) < MINUTE,
            last_share: self.last_share,
            best_share: self.best_share,
            difficulty: self.difficulty.filter(|_| connections > 0),
            last_seen: self.seen,
            connected_since: self.session,
            choices: self.choices,
            listing: self.listing.clone(),
        }
    }
}

impl Telemetry {
    /// Telemetry whose rollup periods last `period` seconds, with a round of its own.
    pub fn new(period: u64) -> Arc<Self> {
        Telemetry::with_round(period, Arc::new(Round::new()))
    }

    /// Telemetry whose rollup periods last `period` seconds, counting each accepted share in `round`.
    pub fn with_round(period: u64, round: Arc<Round>) -> Arc<Self> {
        assert!(period > 0, "a rollup period takes time");
        Arc::new(Telemetry {
            period,
            state: Mutex::new(State::default()),
            round,
            changes: broadcast::channel(CHANGES).0,
        })
    }

    pub fn round(&self) -> &Arc<Round> {
        &self.round
    }

    /// Opens a connection for `handle`, which a session holds for as long as it mines for that worker.
    pub fn connect(self: &Arc<Self>, handle: Handle, pool: PoolId, transport: Transport) -> Link {
        self.connect_at(handle, pool, transport, Instant::now(), SystemTime::now())
    }

    fn connect_at(
        self: &Arc<Self>,
        handle: Handle,
        pool: PoolId,
        transport: Transport,
        now: Instant,
        at: SystemTime,
    ) -> Link {
        self.update(handle, |state| {
            let worker = state
                .workers
                .entry(handle)
                .or_insert_with(|| Worker::new(pool, transport, now, at));
            worker.prune(now);
            if worker.connections() == 0 {
                worker.session = Some(at);
                if worker.shares.is_empty() {
                    worker.since = now;
                }
            }
            *worker.connections.entry(transport).or_default() += 1;
            worker.transport = transport;
            worker.active = now;
            worker.seen = at;
        });
        Link {
            telemetry: self.clone(),
            handle,
            transport,
        }
    }

    /// Records what `handle`'s connection, opened last, chose: one username may mine on several devices, each
    /// with its own choices, and its page shows the latest.
    pub fn choose(&self, handle: Handle, choices: Choices) {
        self.update(handle, |state| {
            if let Some(worker) = state.workers.get_mut(&handle) {
                worker.choices = choices;
            }
        });
    }

    /// Records what the stats lists know of `handle`, from its connection opened last. A connection that
    /// names no miner type keeps the one named before, as the fleet table does.
    pub fn describe(&self, handle: Handle, listing: Listing) {
        self.update(handle, |state| {
            if let Some(worker) = state.workers.get_mut(&handle) {
                let miner = listing
                    .miner
                    .clone()
                    .or_else(|| worker.listing.as_ref().and_then(|l| l.miner.clone()));
                worker.listing = Some(Listing { miner, ..listing });
            }
        });
    }

    /// Every worker with a connection open, as the stats lists count it.
    pub fn connected_workers(&self) -> Vec<Connected> {
        let now = Instant::now();
        let state = self.state.lock().unwrap();
        state
            .workers
            .values()
            .filter(|worker| worker.connections() > 0)
            .map(|worker| Connected {
                listing: worker.listing.clone(),
                hash_rate: worker.hash_rate(now, WINDOW),
                estimating: now.saturating_duration_since(worker.since) < WINDOW,
            })
            .collect()
    }

    /// The worker's live values, if it has connected since the pool started.
    pub fn live(&self, handle: Handle) -> Option<Live> {
        self.live_at(handle, Instant::now())
    }

    fn live_at(&self, handle: Handle, now: Instant) -> Option<Live> {
        let state = self.state.lock().unwrap();
        state.workers.get(&handle).map(|w| w.live(handle, now))
    }

    /// The workers with a connection open, by pool and transport.
    pub fn connected(&self) -> HashMap<(PoolId, Transport), usize> {
        let state = self.state.lock().unwrap();
        let mut counts = HashMap::new();
        for worker in state.workers.values() {
            for (transport, n) in &worker.connections {
                if *n > 0 {
                    *counts.entry((worker.pool.clone(), *transport)).or_default() += 1;
                }
            }
        }
        counts
    }

    /// The hashrate summed over every worker of `pool` it holds, and how many of them are connected.
    pub fn totals(&self, pool: &PoolId) -> (f64, usize) {
        let now = Instant::now();
        let state = self.state.lock().unwrap();
        state
            .workers
            .values()
            .filter(|worker| worker.pool == *pool)
            .fold((0.0, 0), |(rate, connected), worker| {
                (
                    rate + worker.hash_rate(now, WINDOW),
                    connected + usize::from(worker.connections() > 0),
                )
            })
    }

    /// The handle of each worker whose live values change, as they change.
    pub fn subscribe(&self) -> broadcast::Receiver<Handle> {
        self.changes.subscribe()
    }

    /// The rows of the periods that started before Unix time `before`, taken out: they are written once.
    pub fn finished(&self, before: u64) -> Vec<Row> {
        let mut state = self.state.lock().unwrap();
        let (done, open) = std::mem::take(&mut state.periods)
            .into_iter()
            .partition(|((_, _, start), _)| *start < before);
        state.periods = open;
        let mut rows: Vec<Row> = done.into_values().collect();
        rows.sort_by_key(|row| (row.start, row.handle, row.transport));
        state.writing = Some((before, rows.clone()));
        rows
    }

    /// The accepted and refused shares of `handle` that the store does not hold, whose totals it last added
    /// to with rows taken out before `through` (rollup::totals): the open periods', and the rows last taken
    /// out unless that write is the one the store holds.
    pub fn unflushed(&self, handle: Handle, through: u64) -> (u64, u64) {
        let state = self.state.lock().unwrap();
        let open = state.periods.values();
        let writing = state
            .writing
            .iter()
            .filter(|(boundary, _)| *boundary > through)
            .flat_map(|(_, rows)| rows);
        open.chain(writing)
            .filter(|row| row.handle == handle)
            .fold((0, 0), |(accepted, refused), row| {
                (accepted + row.accepted, refused + row.refused())
            })
    }

    /// Forgets the workers with no connection that have done nothing for a window, so a client cycling
    /// through usernames cannot grow the map without bound, and the round's hours that are over. Their
    /// history stays in the rollups.
    pub fn evict(&self, now: Instant) {
        self.round.prune(unix(SystemTime::now()));
        let mut state = self.state.lock().unwrap();
        state.workers.retain(|_, w| {
            w.connections() > 0 || now.saturating_duration_since(w.active) <= WINDOW
        });
    }

    /// Runs `f` on the workers and announces a change to `handle`.
    fn update(&self, handle: Handle, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap());
        // No subscriber is no error.
        let _ = self.changes.send(handle);
    }

    #[allow(clippy::too_many_arguments)]
    fn accept(
        &self,
        handle: Handle,
        transport: Transport,
        work: f64,
        difficulty: f64,
        block: Option<u32>,
        now: Instant,
        at: SystemTime,
    ) {
        let period = self.period;
        self.update(handle, |state| {
            let Some(worker) = state.workers.get_mut(&handle) else {
                return;
            };
            if let Some(listing) = &worker.listing {
                self.round
                    .share(listing, &worker.pool, difficulty, unix(at), block);
            }
            worker.prune(now);
            worker.shares.push_back((now, work));
            worker.last_share = Some(at);
            worker.best_share = worker.best_share.max(difficulty);
            worker.active = now;
            worker.seen = at;
            if let Some(row) = state.row(period, handle, transport, at) {
                row.accepted += 1;
                row.work += work;
                row.best = row.best.max(difficulty);
            }
        });
    }

    fn reject(
        &self,
        handle: Handle,
        transport: Transport,
        class: Reject,
        now: Instant,
        at: SystemTime,
    ) {
        let period = self.period;
        self.update(handle, |state| {
            let Some(worker) = state.workers.get_mut(&handle) else {
                return;
            };
            worker.active = now;
            worker.seen = at;
            if let Some(row) = state.row(period, handle, transport, at) {
                row.rejected[class.index()] += 1;
            }
        });
    }

    fn target(&self, handle: Handle, difficulty: f64) {
        let changed = {
            let mut state = self.state.lock().unwrap();
            match state.workers.get_mut(&handle) {
                Some(worker) if worker.difficulty != Some(difficulty) => {
                    worker.difficulty = Some(difficulty);
                    true
                }
                _ => false,
            }
        };
        if changed {
            let _ = self.changes.send(handle);
        }
    }

    fn disconnect(&self, handle: Handle, transport: Transport, at: SystemTime) {
        self.update(handle, |state| {
            if let Some(worker) = state.workers.get_mut(&handle)
                && let Some(n) = worker.connections.get_mut(&transport)
            {
                *n = n.saturating_sub(1);
                worker.active = Instant::now();
                worker.seen = at;
                if worker.connections() == 0 {
                    worker.session = None;
                }
            }
        });
    }
}

/// One connection's or channel's place in its worker's telemetry. Dropping it is the disconnect.
pub struct Link {
    telemetry: Arc<Telemetry>,
    handle: Handle,
    transport: Transport,
}

impl Link {
    /// Records an accepted share credited at a target of `work`, whose hash reached `difficulty`; `block` is
    /// the height of the block it found, if it found one.
    pub fn accepted(&self, work: f64, difficulty: f64, block: Option<u32>) {
        self.telemetry.accept(
            self.handle,
            self.transport,
            work,
            difficulty,
            block,
            Instant::now(),
            SystemTime::now(),
        );
    }

    pub fn rejected(&self, class: Reject) {
        self.telemetry.reject(
            self.handle,
            self.transport,
            class,
            Instant::now(),
            SystemTime::now(),
        );
    }

    /// Records the difficulty of the target the connection was assigned.
    pub fn target(&self, difficulty: f64) {
        self.telemetry.target(self.handle, difficulty);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.telemetry
            .disconnect(self.handle, self.transport, SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mining::pool::tests::id;

    fn handle(n: char) -> Handle {
        n.to_string().repeat(16).parse().unwrap()
    }

    #[test]
    fn the_hash_rate_converges_on_a_known_share_rate() {
        let telemetry = Telemetry::new(60);
        let start = Instant::now();
        let link = telemetry.connect_at(
            handle('a'),
            id("solo"),
            Transport::V1,
            start,
            SystemTime::now(),
        );
        // A share of work 1e12 every 2 s is 5e11 H/s.
        let work = 1e12;
        let rate = |at: Duration| {
            telemetry
                .live_at(handle('a'), start + at)
                .unwrap()
                .hash_rate
        };
        for n in 1..=1200 {
            let at = start + Duration::from_secs(2 * n);
            telemetry.accept(
                handle('a'),
                Transport::V1,
                work,
                1.0,
                None,
                at,
                SystemTime::now(),
            );
            if n == 30 {
                let early = rate(Duration::from_secs(60));
                assert!((early / 5e11 - 1.0).abs() < 0.05, "{early}");
            }
        }
        assert!(
            telemetry
                .live_at(handle('a'), start + WINDOW - Duration::from_secs(1))
                .unwrap()
                .estimating
        );
        assert!(
            !telemetry
                .live_at(handle('a'), start + WINDOW)
                .unwrap()
                .estimating
        );
        // Past the window, the estimate holds only the window's shares.
        let late = rate(Duration::from_secs(2400));
        assert!((late / 5e11 - 1.0).abs() < 0.01, "{late}");
        let live = telemetry
            .live_at(handle('a'), start + Duration::from_secs(2400))
            .unwrap();
        // The minute holds 30 shares, and the one on its edge.
        assert!(
            (live.minute_hash_rate / 5e11 - 1.0).abs() < 0.05,
            "{}",
            live.minute_hash_rate
        );
        assert!(!live.minute_estimating);
        drop(link);
    }

    #[test]
    fn one_handle_on_several_connections_sums_and_counts_them() {
        let telemetry = Telemetry::new(60);
        let start = Instant::now();
        let a = telemetry.connect_at(
            handle('a'),
            id("solo"),
            Transport::V1,
            start,
            SystemTime::now(),
        );
        let b = telemetry.connect_at(
            handle('a'),
            id("solo"),
            Transport::V2Native,
            start,
            SystemTime::now(),
        );
        for n in 1..=300 {
            let at = start + Duration::from_secs(n);
            // Two rigs of 1e9 H/s each, one share a second apiece.
            telemetry.accept(
                handle('a'),
                Transport::V1,
                1e9,
                1.0,
                None,
                at,
                SystemTime::now(),
            );
            telemetry.accept(
                handle('a'),
                Transport::V2Native,
                1e9,
                1.0,
                None,
                at,
                SystemTime::now(),
            );
        }
        let live = telemetry
            .live_at(handle('a'), start + Duration::from_secs(300))
            .unwrap();
        assert_eq!(live.connections, 2);
        assert_eq!(live.transports, [Transport::V1, Transport::V2Native]);
        let (rate, connected) = telemetry.totals(&id("solo"));
        assert_eq!(connected, 1, "one worker, on two connections");
        assert!(rate > 0.0);
        assert_eq!(telemetry.totals(&id("pooled")), (0.0, 0));
        assert!(
            (live.hash_rate / 2e9 - 1.0).abs() < 0.01,
            "{}",
            live.hash_rate
        );
        drop((a, b));
    }

    #[test]
    fn a_worker_walks_through_online_idle_and_offline() {
        let telemetry = Telemetry::new(60);
        let now = Instant::now();
        let t0 = 1_000_000;
        let at = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(t0 + secs);
        let link = telemetry.connect_at(handle('a'), id("solo"), Transport::V1, now, at(0));
        let status = |secs: u64| {
            let live = telemetry.live_at(handle('a'), now).unwrap();
            let last_share = live.last_share.map(unix);
            Status::new(
                live.connections > 0,
                last_share,
                unix(live.last_seen),
                t0 + secs,
            )
        };
        assert_eq!(status(0), Status::Idle, "connected, no share yet");
        telemetry.accept(handle('a'), Transport::V1, 1.0, 1.0, None, now, at(30));
        assert_eq!(status(30), Status::Online);
        assert_eq!(status(30 + WINDOW.as_secs()), Status::Online);
        assert_eq!(status(31 + WINDOW.as_secs()), Status::Idle);
        telemetry.disconnect(handle('a'), Transport::V1, at(700));
        // The link's own drop would disconnect it again, at the real time.
        std::mem::forget(link);
        assert_eq!(
            status(800),
            Status::Offline {
                last_seen: t0 + 700
            }
        );
    }

    #[test]
    fn best_share_keeps_the_maximum() {
        let telemetry = Telemetry::new(60);
        let now = Instant::now();
        let _link = telemetry.connect_at(
            handle('a'),
            id("pooled"),
            Transport::V1,
            now,
            SystemTime::now(),
        );
        for difficulty in [3.0, 9.0, 2.0] {
            telemetry.accept(
                handle('a'),
                Transport::V1,
                1.0,
                difficulty,
                None,
                now,
                SystemTime::now(),
            );
        }
        let live = telemetry.live_at(handle('a'), now).unwrap();
        assert_eq!(live.best_share, 9.0);
        assert_eq!(telemetry.unflushed(handle('a'), 0), (3, 0));
    }

    #[test]
    fn counts_rows_taken_out_until_their_write_is_over() {
        let telemetry = Telemetry::new(60);
        let now = Instant::now();
        let at = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let link = telemetry.connect_at(handle('a'), id("solo"), Transport::V1, now, at(600));
        telemetry.accept(handle('a'), Transport::V1, 1.0, 1.0, None, now, at(610));
        telemetry.reject(handle('a'), Transport::V1, Reject::Stale, now, at(615));
        telemetry.accept(handle('a'), Transport::V1, 1.0, 1.0, None, now, at(670));
        assert_eq!(telemetry.unflushed(handle('a'), 0), (2, 1));

        assert_eq!(telemetry.finished(660).len(), 1);
        assert_eq!(
            telemetry.unflushed(handle('a'), 600),
            (2, 1),
            "the store's totals do not hold the write yet"
        );
        assert_eq!(
            telemetry.unflushed(handle('a'), 660),
            (1, 0),
            "they hold it"
        );
        assert_eq!(telemetry.finished(720).len(), 1);
        assert_eq!(
            telemetry.unflushed(handle('a'), 660),
            (1, 0),
            "the next rows out replace the written ones"
        );
        assert_eq!(telemetry.unflushed(handle('b'), 0), (0, 0));
        drop(link);
    }

    #[test]
    fn a_session_starts_with_the_first_open_connection_and_ends_with_the_last() {
        let telemetry = Telemetry::new(60);
        let now = Instant::now();
        let at = |s: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        let since = || telemetry.live_at(handle('a'), now).unwrap().connected_since;
        let first = telemetry.connect_at(handle('a'), id("solo"), Transport::V1, now, at(100));
        let second =
            telemetry.connect_at(handle('a'), id("solo"), Transport::V2Native, now, at(200));
        assert_eq!(
            since(),
            Some(at(100)),
            "a second connection joins the session"
        );
        drop(first);
        assert_eq!(since(), Some(at(100)), "one connection is still open");
        drop(second);
        assert_eq!(since(), None);
        let again = telemetry.connect_at(handle('a'), id("solo"), Transport::V1, now, at(300));
        assert_eq!(since(), Some(at(300)), "a reconnect starts a new session");
        drop(again);
    }

    #[test]
    fn a_disconnect_marks_the_worker_disconnected_and_keeps_its_best_share() {
        let telemetry = Telemetry::new(60);
        let now = Instant::now();
        let link = telemetry.connect_at(
            handle('a'),
            id("solo"),
            Transport::V2Native,
            now,
            SystemTime::now(),
        );
        link.target(512.0);
        telemetry.accept(
            handle('a'),
            Transport::V2Native,
            1.0,
            700.0,
            None,
            now,
            SystemTime::now(),
        );
        let live = telemetry.live_at(handle('a'), now).unwrap();
        assert_eq!((live.connections, live.difficulty), (1, Some(512.0)));

        let mut changes = telemetry.subscribe();
        drop(link);
        assert_eq!(
            changes.try_recv(),
            Ok(handle('a')),
            "the disconnect is announced"
        );
        let live = telemetry.live_at(handle('a'), now).unwrap();
        assert_eq!(live.connections, 0);
        assert_eq!(live.difficulty, None);
        assert_eq!(live.transports, [Transport::V2Native]);
        assert_eq!(live.best_share, 700.0);
        assert!(telemetry.live_at(handle('b'), now).is_none());
    }

    #[test]
    fn counts_each_period_per_worker_and_transport_and_nothing_for_an_idle_one() {
        let telemetry = Telemetry::new(60);
        let now = Instant::now();
        let unix = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let _a = telemetry.connect_at(
            handle('a'),
            id("solo"),
            Transport::V1,
            now,
            SystemTime::now(),
        );
        let _b = telemetry.connect_at(
            handle('b'),
            id("pooled"),
            Transport::V2Native,
            now,
            SystemTime::now(),
        );
        let _idle = telemetry.connect_at(
            handle('c'),
            id("solo"),
            Transport::V1,
            now,
            SystemTime::now(),
        );
        // Worker a: two shares and a stale in the period at 600, one share in the period at 660.
        telemetry.accept(handle('a'), Transport::V1, 10.0, 5.0, None, now, unix(600));
        telemetry.accept(handle('a'), Transport::V1, 10.0, 8.0, None, now, unix(659));
        telemetry.reject(handle('a'), Transport::V1, Reject::Stale, now, unix(630));
        telemetry.accept(handle('a'), Transport::V1, 20.0, 3.0, None, now, unix(660));
        // Worker b: refused only, in the period at 600.
        telemetry.reject(
            handle('b'),
            Transport::V2Native,
            Reject::BadVersion,
            now,
            unix(601),
        );

        let rows = telemetry.finished(660);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let (a, b) = (&rows[0], &rows[1]);
        assert_eq!(
            (a.start, a.handle, a.pool.clone(), a.transport),
            (600, handle('a'), id("solo"), Transport::V1)
        );
        assert_eq!((a.accepted, a.work, a.best), (2, 20.0, 8.0));
        assert_eq!(a.rejected(Reject::Stale), 1);
        assert_eq!(
            (b.handle, b.pool.clone(), b.accepted, b.best),
            (handle('b'), id("pooled"), 0, 0.0)
        );
        assert_eq!(b.rejected(Reject::BadVersion), 1);

        assert!(telemetry.finished(660).is_empty(), "each row is taken once");
        let rows = telemetry.finished(720);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].start, rows[0].accepted, rows[0].work),
            (660, 1, 20.0)
        );
    }

    #[test]
    fn evicts_only_workers_gone_for_a_window() {
        let telemetry = Telemetry::new(60);
        let now = Instant::now();
        let gone = telemetry.connect_at(
            handle('a'),
            id("solo"),
            Transport::V1,
            now,
            SystemTime::now(),
        );
        let _held = telemetry.connect_at(
            handle('b'),
            id("solo"),
            Transport::V1,
            now,
            SystemTime::now(),
        );
        drop(gone);
        telemetry.evict(now + WINDOW / 2);
        assert!(
            telemetry.live_at(handle('a'), now).is_some(),
            "recently active"
        );
        telemetry.evict(Instant::now() + WINDOW + Duration::from_secs(1));
        assert!(telemetry.live_at(handle('a'), now).is_none());
        assert!(
            telemetry.live_at(handle('b'), now).is_some(),
            "still connected"
        );
    }

    #[test]
    fn a_described_worker_carries_its_fleet_and_type_into_the_round() {
        let telemetry = Telemetry::new(60);
        let link = telemetry.connect(handle('a'), id("solo"), Transport::V1);
        let address = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
        let listing = Listing {
            fleet: handle('f'),
            short_address: Some(fleet::shortened(address)),
            miner: Some("NerdQAxe++".into()),
            country: None,
        };
        telemetry.describe(handle('a'), listing.clone());
        // A later connection that names no type keeps the one named.
        telemetry.describe(
            handle('a'),
            Listing {
                miner: None,
                ..listing.clone()
            },
        );
        let live = telemetry.live(handle('a')).unwrap();
        assert_eq!(live.listing.as_ref(), Some(&listing));
        assert!(!format!("{live:?}").contains(address), "no whole address");
        link.accepted(1.0, 42.0, None);
        let round = telemetry.round().round();
        assert_eq!(round.len(), 1);
        assert_eq!(
            (
                round[0].fleet,
                round[0].best.best,
                round[0].best.miner.as_deref()
            ),
            (handle('f'), 42.0, Some("NerdQAxe++"))
        );
        let connected = telemetry.connected_workers();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].listing.as_ref(), Some(&listing));
        drop(link);
        assert!(telemetry.connected_workers().is_empty());
    }

    #[test]
    fn changes_are_broadcast_by_handle() {
        let telemetry = Telemetry::new(60);
        let mut changes = telemetry.subscribe();
        let link = telemetry.connect(handle('a'), id("solo"), Transport::V1);
        link.accepted(1.0, 1.0, None);
        link.rejected(Reject::Stale);
        link.target(2.0);
        link.target(2.0);
        for _ in 0..4 {
            assert_eq!(changes.try_recv(), Ok(handle('a')));
        }
        assert!(
            changes.try_recv().is_err(),
            "an unchanged target is no change"
        );
    }
}
