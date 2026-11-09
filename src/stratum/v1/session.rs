//! One V1 connection: its task reads lines, answers the handshake, issues jobs, and closes on hostile input.
//! A failure here ends this connection and no other.

use std::{
    collections::{HashSet, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::{Instant, SystemTime},
};

use bitcoin::{BlockHash, Target, hashes::Hash, hex::FromHex};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpStream, tcp::OwnedWriteHalf},
};
use tokio_util::codec::{FramedRead, LinesCodec};

use super::{
    TRANSPORT,
    message::{self, Request, StratumError},
};
use crate::{
    failpoint,
    geo::Country,
    metrics,
    mining::{
        coinbase::{EXTRANONCE_LEN, EXTRANONCE_PREFIX_LEN, MINER_EXTRANONCE_LEN},
        extranonce::ExtranoncePrefix,
        job::{Job, Submission},
        pool::Pool,
        target,
        vardiff::Vardiff,
    },
    node::template::{BIP320_MASK, Work},
    stratum::{Context, Rejection, Worker, username},
    telemetry::{Reject, fleet},
};

/// The longest line a miner may send. Real messages are well under 1 KiB.
pub const MAX_LINE: usize = 16 * 1024;
/// Jobs kept per session. A clean template drops them all; mempool refreshes add to them.
const MAX_JOBS: usize = 16;
/// Unauthorized submits a connection may send before it is closed.
const MAX_UNAUTHORIZED_SUBMITS: u32 = 16;
/// Refused authorizations a connection may ask for before it is closed. Each is logged, so without a bound
/// a hostile client could fill the log.
const MAX_REFUSED_AUTHORIZATIONS: u32 = 16;

/// Why a session ended.
struct Close(String);

/// A job as this session issued it, with the target assigned alongside it.
struct IssuedJob {
    id: String,
    job: Arc<Job>,
    target: Target,
    difficulty: f64,
}

struct Session {
    context: Arc<Context>,
    peer: SocketAddr,
    /// Bound by the listener that accepted the connection; switching pools means reconnecting.
    pool: Arc<Pool>,
    writer: OwnedWriteHalf,
    extranonce: ExtranoncePrefix,
    subscribed: bool,
    /// The miner type from mining.subscribe's user agent, without a version.
    vendor: Option<String>,
    /// The peer's country, looked up when the connection opened.
    country: Option<Country>,
    /// The authorized username as the miner sent it, and the worker it names.
    worker: Option<(String, Worker)>,
    /// The version-rolling mask negotiated through mining.configure; none until then.
    version_mask: u32,
    /// A starting difficulty from mining.suggest_difficulty or a d=N password.
    hint: Option<f64>,
    /// Created at authorization.
    vardiff: Option<Vardiff>,
    jobs: VecDeque<IssuedJob>,
    /// Header hashes of the shares accepted on the jobs since the last clean one. A header hash names the
    /// work itself, so the same header is refused whichever job id it arrives under.
    shares: HashSet<BlockHash>,
    next_job_id: u64,
    /// The ntime of the last notify. Each notify announces a later one, so a job issued again on the same
    /// template gives the miner fresh headers rather than the ones it already searched.
    last_ntime: u32,
    /// The template of the last job issued, so a clean template clears the jobs only once.
    template_seq: Option<u64>,
    unauthorized_submits: u32,
    refused_authorizations: u32,
}

pub async fn run(stream: TcpStream, peer: SocketAddr, pool: Arc<Pool>, context: Arc<Context>) {
    let (reader, writer) = stream.into_split();
    let mut lines = FramedRead::new(reader, LinesCodec::new_with_max_length(MAX_LINE));
    let mut work = context.work.clone();
    let country = context.country(peer);
    let mut session = Session {
        extranonce: context.extranonces.allocate(),
        context,
        peer,
        pool,
        writer,
        subscribed: false,
        vendor: None,
        country,
        worker: None,
        version_mask: 0,
        hint: None,
        vardiff: None,
        jobs: VecDeque::new(),
        shares: HashSet::new(),
        next_job_id: 0,
        last_ntime: 0,
        template_seq: None,
        unauthorized_submits: 0,
        refused_authorizations: 0,
    };
    let mut vardiff_tick = tokio::time::interval(session.context.vardiff.share_interval);
    // A connection that never authorizes would otherwise hold its task and extranonce prefix forever, as
    // a Bitaxe refused for an address on the other network.
    let authorize_timeout = session.context.authorize_timeout;
    let authorize_deadline = tokio::time::sleep(authorize_timeout);
    tokio::pin!(authorize_deadline);
    tracing::info!(%peer, pool = %session.pool.id, transport = %TRANSPORT, "V1 connection");
    let Close(reason) = loop {
        let result = tokio::select! {
            line = lines.next() => match line {
                Some(Ok(line)) if line.trim().is_empty() => Ok(()),
                Some(Ok(line)) => session.handle(&line, Instant::now(), &mut work).await,
                Some(Err(e)) => Err(Close(e.to_string())),
                None => Err(Close("closed by the miner".into())),
            },
            changed = work.changed(), if session.vardiff.is_some() => match changed {
                Ok(()) => session.issue(&mut work, false).await,
                Err(_) => Err(Close("the template task stopped".into())),
            },
            _ = vardiff_tick.tick(), if session.vardiff.is_some() => session.on_tick(&mut work).await,
            _ = &mut authorize_deadline, if session.worker.is_none() => {
                Err(Close(format!("not authorized within {authorize_timeout:?}")))
            }
        };
        if let Err(close) = result {
            break close;
        }
    };
    tracing::info!(%peer, pool = %session.pool.id, transport = %TRANSPORT, %reason, "V1 disconnected");
}

impl Session {
    /// Answers `line`, read at `read`.
    async fn handle(
        &mut self,
        line: &str,
        read: Instant,
        work: &mut tokio::sync::watch::Receiver<Work>,
    ) -> Result<(), Close> {
        // An authorize carries the username and password, which may hold a fleet secret.
        if line.contains("mining.authorize") {
            tracing::trace!("V1 received mining.authorize");
        } else {
            tracing::trace!(line, "V1 received");
        }
        let request: Request =
            serde_json::from_str(line).map_err(|e| Close(format!("malformed JSON: {e}")))?;
        match request.method.as_str() {
            "mining.subscribe" => {
                let reply = self.subscribe(&request);
                self.send(reply).await
            }
            "mining.authorize" => {
                let reply = match self.authorize(&request) {
                    Ok(()) => message::result(&request.id, json!(true)),
                    Err((error, reason)) => {
                        tracing::warn!(
                            peer = %self.peer,
                            pool = %self.pool.id,
                            username = username::redacted(request.param(0).as_str().unwrap_or_default()),
                            %reason,
                            "V1 authorization refused"
                        );
                        self.refused_authorizations += 1;
                        if self.refused_authorizations > MAX_REFUSED_AUTHORIZATIONS {
                            return Err(Close("too many refused authorizations".into()));
                        }
                        message::error(&request.id, error, &reason)
                    }
                };
                self.send(reply).await?;
                if self.vardiff.is_some() {
                    self.issue(work, true).await?;
                }
                Ok(())
            }
            "mining.configure" => {
                let reply = self.configure(&request);
                self.send(reply).await
            }
            "mining.extranonce.subscribe" => {
                self.send(message::result(&request.id, json!(true))).await
            }
            "mining.submit" => {
                let (reply, retarget) = self.submit(&request).await?;
                self.send(reply).await?;
                metrics::share_answered(&self.pool.id, TRANSPORT, read);
                // A new target takes effect on the next job, which goes out at once.
                if retarget {
                    self.issue(work, false).await?;
                }
                Ok(())
            }
            "mining.suggest_difficulty" => {
                self.send(message::result(&request.id, json!(true))).await?;
                if let Some(hint) = difficulty(request.param(0)) {
                    self.hint = Some(hint);
                    // Once work has started, the hint re-seeds vardiff and applies from the next job.
                    if self.vardiff.is_some() {
                        self.vardiff = Some(self.new_vardiff());
                        self.issue(work, true).await?;
                    }
                }
                Ok(())
            }
            _ => {
                let reply = message::error(&request.id, StratumError::Other, "Unknown method");
                self.send(reply).await
            }
        }
    }

    fn subscribe(&mut self, request: &Request) -> Value {
        self.subscribed = true;
        self.vendor = request.param(0).as_str().and_then(fleet::vendor);
        let extranonce1 = hex(&self.extranonce.bytes());
        message::result(
            &request.id,
            json!([
                [
                    ["mining.set_difficulty", extranonce1],
                    ["mining.notify", extranonce1]
                ],
                extranonce1,
                MINER_EXTRANONCE_LEN,
            ]),
        )
    }

    /// Authorizes the worker and starts vardiff. On refusal, returns the error and its reason.
    fn authorize(&mut self, request: &Request) -> Result<(), (StratumError, String)> {
        if !self.subscribed {
            return Err((StratumError::NotSubscribed, "Not subscribed".into()));
        }
        let Some(name) = request.param(0).as_str() else {
            return Err((StratumError::Unauthorized, "No username".into()));
        };
        let password = request.param(1).as_str().unwrap_or_default();
        let username = username::parse(name, self.context.network)
            .and_then(|username| username.with_password(password))
            .map_err(|e| {
                (
                    StratumError::Unauthorized,
                    format!("Unauthorized worker: {e}"),
                )
            })?;
        if let Some(hint) = request.param(1).as_str().and_then(password_difficulty) {
            self.hint = Some(hint);
        }
        let worker = self.context.worker(
            username,
            &self.pool,
            TRANSPORT,
            self.vendor.clone(),
            self.country.clone(),
        );
        tracing::info!(
            address = %worker.username.address,
            worker = %worker.username.worker,
            handle = %worker.handle,
            pool = %self.pool.id,
            transport = %TRANSPORT,
            "V1 authorized"
        );
        self.worker = Some((name.to_owned(), worker));
        if self.vardiff.is_none() {
            self.vardiff = Some(self.new_vardiff());
        }
        Ok(())
    }

    fn new_vardiff(&self) -> Vardiff {
        Vardiff::new(&self.context.vardiff, self.hint, Instant::now())
    }

    /// BIP310: grants version rolling within BIP320's bits, and declines every other extension.
    fn configure(&mut self, request: &Request) -> Value {
        let options = request.param(1);
        let mut granted = Map::new();
        let extensions = request.param(0).as_array().into_iter().flatten();
        for extension in extensions.filter_map(Value::as_str) {
            if extension != "version-rolling" {
                granted.insert(extension.into(), json!(false));
                continue;
            }
            let requested = options["version-rolling.mask"]
                .as_str()
                .and_then(|mask| u32::from_str_radix(mask, 16).ok())
                .unwrap_or(u32::MAX);
            let min_bits = options["version-rolling.min-bit-count"]
                .as_u64()
                .unwrap_or(0);
            let mask = requested & BIP320_MASK as u32;
            if u64::from(mask.count_ones()) >= min_bits {
                self.version_mask = mask;
                granted.insert("version-rolling".into(), json!(true));
                granted.insert("version-rolling.mask".into(), json!(format!("{mask:08x}")));
            } else {
                granted.insert("version-rolling".into(), json!(false));
            }
        }
        message::result(&request.id, Value::Object(granted))
    }

    /// Issues a job on the current work, with set_difficulty ahead of the notify it governs. The first job
    /// on a clean template sets clean_jobs and drops the session's older jobs; `restart` does the same on
    /// any template, as a session's first job needs. No work sends nothing and keeps the socket open.
    async fn issue(
        &mut self,
        work: &mut tokio::sync::watch::Receiver<Work>,
        restart: bool,
    ) -> Result<(), Close> {
        let Work::Template(template) = work.borrow_and_update().clone() else {
            return Ok(());
        };
        let (Some((_, worker)), Some(vardiff)) = (&self.worker, &self.vardiff) else {
            return Ok(());
        };
        // A job pays its finder's part of the miner portion to the worker's address, and the pool wallet the
        // rest; in a pool that cannot pay a finder, all of it to the pool wallet, so its workers share jobs.
        let miner = if self.pool.can_pay_finder() {
            worker.username.address.script_pubkey()
        } else {
            self.context.pool_wallet.clone()
        };
        let job = self.context.jobs.job(
            &template,
            &self.pool.id,
            &worker.username.worker,
            &miner,
            &worker.username.options,
        );
        let (target, difficulty) = (vardiff.target(), vardiff.difficulty());
        worker.link.target(difficulty);
        let clean = restart || (template.clean && self.template_seq != Some(template.seq));
        self.template_seq = Some(template.seq);
        if clean {
            self.jobs.clear();
            self.shares.clear();
        }
        self.next_job_id += 1;
        let id = format!("{:x}", self.next_job_id);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        let ntime = job.time.max(now).max(self.last_ntime.saturating_add(1));
        self.last_ntime = ntime;
        tracing::debug!(job = %id, difficulty, clean, "V1 job");
        let notify = job.notify(&id, ntime, clean);
        if self.jobs.len() == MAX_JOBS {
            self.jobs.pop_front();
        }
        self.jobs.push_back(IssuedJob {
            id,
            job,
            target,
            difficulty,
        });
        self.send(message::notification(
            "mining.set_difficulty",
            json!([difficulty]),
        ))
        .await?;
        self.send(message::notification("mining.notify", notify))
            .await
    }

    /// Checks a share against the target recorded with its job and credits it, once it is journaled in a pool
    /// that can pay a pool output. Returns the reply, and whether vardiff changed the target.
    async fn submit(&mut self, request: &Request) -> Result<(Value, bool), Close> {
        let refuse = |error, text: &str| {
            tracing::debug!(reason = text, "V1 share rejected");
            Ok((message::error(&request.id, error, text), false))
        };
        // The refusals the share check does not record itself.
        let (context, pool) = (&self.context, &self.pool);
        let reject = |worker, class, error, text: &str| {
            context.rejected(&pool.id, TRANSPORT, worker, class);
            refuse(error, text)
        };
        let worker = match &self.worker {
            Some((name, worker)) if request.param(0).as_str() == Some(name.as_str()) => worker,
            _ => {
                self.unauthorized_submits += 1;
                if self.unauthorized_submits > MAX_UNAUTHORIZED_SUBMITS {
                    return Err(Close("too many unauthorized submits".into()));
                }
                return reject(
                    None,
                    Reject::Unauthorized,
                    StratumError::Unauthorized,
                    "Unauthorized worker",
                );
            }
        };
        let prefix = self.extranonce.bytes();
        let job_id = request.param(1).as_str();
        let Some(issued) = self.jobs.iter().find(|j| Some(j.id.as_str()) == job_id) else {
            return reject(
                Some(worker),
                Reject::Stale,
                StratumError::StaleJob,
                "Stale job",
            );
        };
        let Some(submission) = parse_submission(request, prefix) else {
            return reject(
                Some(worker),
                Reject::Invalid,
                StratumError::Other,
                "Invalid submit params",
            );
        };
        if submission.version_bits & !self.version_mask != 0 {
            return reject(
                Some(worker),
                Reject::BadVersion,
                StratumError::Other,
                "Version bits outside the negotiated mask",
            );
        }
        let checked = match context.check_share(
            worker,
            &mut self.shares,
            &issued.job,
            &submission,
            self.version_mask,
            issued.target,
        ) {
            Ok(checked) => checked,
            Err(Rejection::Duplicate) => return refuse(StratumError::Duplicate, "Duplicate share"),
            Err(Rejection::LowDifficulty) => {
                return refuse(StratumError::LowDifficulty, "Low difficulty share");
            }
            Err(Rejection::Unavailable) => unreachable!("only crediting refuses so"),
        };
        // The id this session gave the job, as a number for the journal.
        let job_number = u64::from_str_radix(&issued.id, 16).expect("a hex job id");
        let (job, target, difficulty) = (issued.job.clone(), issued.target, issued.difficulty);
        match context
            .credit(worker, &job, job_number, target, &checked)
            .await
        {
            Ok(()) => {}
            Err(Rejection::Duplicate) => return refuse(StratumError::Duplicate, "Duplicate share"),
            Err(Rejection::Unavailable) => {
                return refuse(StratumError::Other, "Pool accounting unavailable");
            }
            Err(Rejection::LowDifficulty) => unreachable!("only the check refuses so"),
        }
        if pool.can_pay_pool() {
            failpoint::exit_if(failpoint::BEFORE_ACKNOWLEDGEMENT);
        }
        tracing::debug!(
            hash = %checked.hash,
            target_difficulty = difficulty,
            share_difficulty = target::difficulty(Target::from_le_bytes(checked.hash.to_byte_array())),
            "V1 share accepted"
        );
        let retarget = self
            .vardiff
            .as_mut()
            .and_then(|vardiff| vardiff.on_share(difficulty, Instant::now()))
            .is_some();
        Ok((message::result(&request.id, json!(true)), retarget))
    }

    /// Lets vardiff ease a miner that submits too rarely, issuing a job at the new target.
    async fn on_tick(
        &mut self,
        work: &mut tokio::sync::watch::Receiver<Work>,
    ) -> Result<(), Close> {
        let retarget = self
            .vardiff
            .as_mut()
            .and_then(|vardiff| vardiff.on_tick(Instant::now()));
        match retarget {
            Some(_) => self.issue(work, false).await,
            None => Ok(()),
        }
    }

    async fn send(&mut self, message: Value) -> Result<(), Close> {
        let mut line = message.to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| Close(format!("write failed: {e}")))
    }
}

/// mining.submit's params: worker, job id, extranonce2, ntime, nonce, and optionally version bits.
fn parse_submission(request: &Request, prefix: [u8; EXTRANONCE_PREFIX_LEN]) -> Option<Submission> {
    let word = |index: usize| -> Option<u32> {
        u32::from_str_radix(request.param(index).as_str()?, 16).ok()
    };
    let miner: [u8; MINER_EXTRANONCE_LEN] = Vec::<u8>::from_hex(request.param(2).as_str()?)
        .ok()?
        .try_into()
        .ok()?;
    let mut extranonce = [0; EXTRANONCE_LEN];
    extranonce[..EXTRANONCE_PREFIX_LEN].copy_from_slice(&prefix);
    extranonce[EXTRANONCE_PREFIX_LEN..].copy_from_slice(&miner);
    let version_bits = match request.param(5) {
        Value::Null => 0,
        _ => word(5)?,
    };
    Some(Submission {
        extranonce,
        time: word(3)?,
        nonce: word(4)?,
        version_bits,
    })
}

/// A positive difficulty, sent as a number or a numeric string.
fn difficulty(value: &Value) -> Option<f64> {
    let difficulty = match value {
        Value::String(s) => s.parse().ok()?,
        other => other.as_f64()?,
    };
    (difficulty.is_finite() && difficulty > 0.0).then_some(difficulty)
}

/// The d=N starting difficulty in a password such as `x`, `d=512`, or `x,d=512`.
fn password_difficulty(password: &str) -> Option<f64> {
    password
        .split(',')
        .find_map(|part| part.trim().strip_prefix("d="))
        .and_then(|d| difficulty(&Value::String(d.into())))
}

fn hex(bytes: &[u8]) -> String {
    bitcoin::hex::DisplayHex::to_lower_hex_string(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_difficulty_from_the_password() {
        assert_eq!(password_difficulty("d=512"), Some(512.0));
        assert_eq!(password_difficulty("x, d=0.001"), Some(0.001));
        assert_eq!(password_difficulty("x"), None);
        assert_eq!(password_difficulty("d=-1"), None);
        assert_eq!(password_difficulty("d=abc"), None);
    }
}
