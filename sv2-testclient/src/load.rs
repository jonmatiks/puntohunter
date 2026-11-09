//! The load generator of the load suite: many sessions in one process, each a [`Client`] on a thread of its own,
//! sending a valid share every `interval` and noting when each new prevhash reached it.
//!
//! ```text
//! -> {"cmd":"load","port":4334,"pin":"9bXi...","users":["bcrt1q....w1",...],"interval":2.5,"standard":false}
//! <- {"event":"loading","ready":250,"failed":[]}
//! -> {"cmd":"report"}
//! <- {"event":"report","ready":250,"submitted":...,"accepted":...,"errors":{},"closed":[],"tips":{...}}
//! -> {"cmd":"stop"}
//! <- {"event":"stopped"}
//! ```

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use parsers_sv2::{AnyMessageOwned, MiningOwned};
use serde_json::{Value, json};

use super::{
    Client,
    channel::{Kind, Wanted, hex},
    frame, text, timeout,
};

pub struct Load {
    stats: Arc<Mutex<Stats>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

#[derive(Default)]
struct Stats {
    ready: usize,
    /// Sessions that could not connect, set up, or open their channel, and why.
    failed: Vec<String>,
    /// Sessions the pool closed after they were ready, and why.
    closed: Vec<String>,
    submitted: u64,
    accepted: u64,
    errors: BTreeMap<String, u64>,
    /// When each session first held a job on each prevhash, by block hash in display order: Unix seconds.
    tips: HashMap<String, Vec<f64>>,
}

impl Load {
    /// Starts a session for each of `users`, returning once each is ready, holding a job, or has failed.
    pub fn start(command: &Value) -> Result<(Load, Value), String> {
        let users: Vec<String> = command["users"]
            .as_array()
            .ok_or("no users")?
            .iter()
            .map(|user| user.as_str().map(str::to_owned).ok_or("a user is a string"))
            .collect::<Result<_, _>>()?;
        let interval = Duration::from_secs_f64(command["interval"].as_f64().ok_or("no interval")?);
        let stats = Arc::new(Mutex::new(Stats::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let count = users.len();
        let threads = users
            .into_iter()
            .enumerate()
            .map(|(i, user)| {
                let (command, stats, stop) = (command.clone(), stats.clone(), stop.clone());
                // Spread over the interval, so the shares do not arrive in step.
                let offset = interval.mul_f64(i as f64 / count as f64);
                thread::spawn(move || run(&command, &user, interval, offset, &stats, &stop))
            })
            .collect();
        let load = Load {
            stats,
            stop,
            threads,
        };
        let deadline = Instant::now() + timeout(command);
        loop {
            let (ready, failed) = {
                let stats = load.stats.lock().unwrap();
                (stats.ready, stats.failed.clone())
            };
            if ready + failed.len() == count {
                return Ok((
                    load,
                    json!({"event": "loading", "ready": ready, "failed": failed}),
                ));
            }
            if Instant::now() >= deadline {
                load.stop();
                return Err(format!(
                    "{ready} of {count} sessions ready, {failed:?} failed"
                ));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn report(&self) -> Value {
        let stats = self.stats.lock().unwrap();
        json!({
            "event": "report",
            "ready": stats.ready,
            "failed": stats.failed,
            "closed": stats.closed,
            "submitted": stats.submitted,
            "accepted": stats.accepted,
            "errors": stats.errors,
            "tips": stats.tips,
        })
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads {
            let _ = thread.join();
        }
    }
}

/// One session: connect, set up, open a channel, then a share every `interval`, starting after `offset`.
fn run(
    command: &Value,
    user: &str,
    interval: Duration,
    offset: Duration,
    stats: &Mutex<Stats>,
    stop: &AtomicBool,
) {
    let mut client = Client::new();
    let opened = client
        .connect(command)
        .and_then(|_| client.setup(command))
        .and_then(|reply| match reply["event"].as_str() {
            Some("setup_success") => client.open(
                &json!({"user": user, "timeout": command["timeout"]}),
                match command["standard"].as_bool() {
                    Some(true) => Kind::Standard,
                    _ => Kind::Extended,
                },
            ),
            _ => Err(format!("setup refused: {reply}")),
        })
        .and_then(|reply| match reply["channel_id"].as_u64() {
            Some(id) => Ok(id as u32),
            None => Err(format!("channel refused: {reply}")),
        });
    let channel_id = match opened.and_then(|id| {
        client
            .job(&json!({"channel_id": id, "timeout": command["timeout"]}))
            .map(|_| id)
    }) {
        Ok(id) => id,
        Err(reason) => {
            stats
                .lock()
                .unwrap()
                .failed
                .push(format!("{user}: {reason}"));
            return;
        }
    };
    stats.lock().unwrap().ready += 1;
    let mut seen = HashSet::new();
    let mut next = Instant::now() + offset;
    while !stop.load(Ordering::Relaxed) {
        let wait = next
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        client.drain(wait);
        let at = unix_now();
        let mut held = stats.lock().unwrap();
        for message in client.queue.drain(..) {
            match message {
                AnyMessageOwned::Mining(MiningOwned::SetNewPrevHash(prevhash)) => {
                    let mut hash = prevhash.prev_hash.to_array();
                    hash.reverse();
                    if seen.insert(hash) {
                        held.tips.entry(hex(&hash)).or_default().push(at);
                    }
                }
                AnyMessageOwned::Mining(MiningOwned::SubmitSharesSuccess(success)) => {
                    held.accepted += u64::from(success.new_submits_accepted_count);
                }
                AnyMessageOwned::Mining(MiningOwned::SubmitSharesError(error)) => {
                    *held.errors.entry(text(&error.error_code)).or_default() += 1;
                }
                _ => {}
            }
        }
        if let Some(reason) = &client.closed {
            held.closed.push(format!("{user}: {reason}"));
            return;
        }
        drop(held);
        if Instant::now() < next {
            continue;
        }
        next += interval;
        let channel = client.channels.get_mut(&channel_id).unwrap();
        let Ok(share) = channel.grind(None, Wanted::Valid) else {
            continue;
        };
        if client
            .conn
            .as_mut()
            .unwrap()
            .send(frame(share.message))
            .is_ok()
        {
            stats.lock().unwrap().submitted += 1;
        }
    }
    if let Some(conn) = client.conn.take() {
        conn.close();
    }
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}
