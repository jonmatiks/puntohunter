//! sv2-testclient: the SV2 side of the e2e harness. pytest drives it over JSON lines: one command
//! per line on stdin, one reply per command on stdout. Messages the pool sends between commands are queued,
//! and `wait` and `messages` read them.
//!
//! ```text
//! -> {"cmd":"connect","port":3334,"pin":"9bXi...","source":"127.0.0.2"}
//! <- {"event":"connected"}
//! -> {"cmd":"setup"}
//! <- {"event":"setup_success","used_version":2,"flags":0}
//! -> {"cmd":"open_extended_channel","user":"bcrt1q....worker1"}
//! <- {"event":"channel_opened","channel_id":1,"target":"0000ffff...","extranonce_prefix":"00000001",...}
//! -> {"cmd":"submit","channel_id":1,"count":50}
//! <- {"event":"submitted","first_sequence_number":0,"last_sequence_number":49,"acks":[...],"errors":[]}
//! ```
//!
//! Misbehaviour is a command like any other: an out-of-range version through `setup`, a wrong `pin`, a
//! `truncate_handshake`, `raw_frame` for arbitrary bytes once encrypted, and `raw_bytes` for bytes outside
//! any frame. `load` runs many sessions at once for the load suite, and `report` and `stop` read and
//! end them.

mod channel;
mod conn;
mod load;

use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, Write},
    time::{Duration, Instant},
};

use binary_sv2::{Str0255Owned, U256Owned};
use bitcoin::Target;
use common_messages_sv2::{Protocol, SetupConnectionOwned};
use framing_sv2::framing::MessageFrame;
use mining_sv2::{
    OpenExtendedMiningChannelOwned, OpenStandardMiningChannelOwned, UpdateChannelOwned,
};
use parsers_sv2::{AnyMessageOwned, CommonMessagesOwned, IsSv2Message, MiningOwned};
use serde_json::{Value, json};

use self::{
    channel::{Channel, Kind, Wanted, hex, target_hex},
    conn::{Conn, Incoming},
    load::Load,
};

const DEFAULT_TIMEOUT: f64 = 10.0;

struct Client {
    conn: Option<Conn>,
    /// Messages received and not yet read by `wait` or `messages`.
    queue: VecDeque<AnyMessageOwned>,
    channels: HashMap<u32, Channel>,
    closed: Option<String>,
    next_request_id: u32,
}

fn main() {
    let mut client = Client::new();
    let mut load = None;
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(command) => match command["cmd"].as_str() {
                Some("load") => Load::start(&command).map(|(started, reply)| {
                    load = Some(started);
                    reply
                }),
                Some("report") => load.as_ref().map(Load::report).ok_or("no load".into()),
                Some("stop") => load
                    .take()
                    .map(|stopped| {
                        stopped.stop();
                        json!({"event": "stopped"})
                    })
                    .ok_or("no load".into()),
                _ => client.run(&command),
            }
            .unwrap_or_else(|reason: String| json!({"event": "error", "reason": reason})),
            Err(e) => json!({"event": "error", "reason": format!("not JSON: {e}")}),
        };
        let mut out = stdout.lock();
        let _ = writeln!(out, "{reply}");
        let _ = out.flush();
    }
}

fn timeout(command: &Value) -> Duration {
    Duration::from_secs_f64(command["timeout"].as_f64().unwrap_or(DEFAULT_TIMEOUT))
}

fn str0255(text: &str) -> Str0255Owned {
    text.try_into().expect("a short string")
}

/// A target from big-endian hex, as this client writes them.
fn parse_target(value: &Value) -> Result<Target, String> {
    let Some(text) = value.as_str() else {
        return Ok(Target::from_be_bytes([0xff; 32]));
    };
    let bytes = from_hex(text)?;
    let array: [u8; 32] = bytes.try_into().map_err(|_| "a target is 32 bytes")?;
    Ok(Target::from_be_bytes(array))
}

fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    (0..text.len())
        .step_by(2)
        .map(|i| {
            text.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or(format!("not hex: {text}"))
        })
        .collect()
}

fn frame(message: MiningOwned) -> MessageFrame<MiningOwned> {
    let (message_type, channel_bit) = (message.message_type(), message.channel_bit());
    MessageFrame::from_message(message, message_type, 0, channel_bit).unwrap()
}

impl Client {
    fn new() -> Client {
        Client {
            conn: None,
            queue: VecDeque::new(),
            channels: HashMap::new(),
            closed: None,
            next_request_id: 1,
        }
    }

    fn run(&mut self, command: &Value) -> Result<Value, String> {
        let name = command["cmd"].as_str().ok_or("no cmd")?;
        match name {
            "connect" => self.connect(command),
            "setup" => self.setup(command),
            "open_extended_channel" => self.open(command, Kind::Extended),
            "open_standard_channel" => self.open(command, Kind::Standard),
            "job" => self.job(command),
            "submit" => self.submit(command),
            "update_channel" => self.update_channel(command),
            "wait" => self.wait(command),
            "messages" => {
                self.drain(Duration::ZERO);
                let messages: Vec<Value> = self.queue.drain(..).map(|m| describe(&m)).collect();
                Ok(json!({"event": "messages", "messages": messages}))
            }
            "raw_frame" => {
                let payload = from_hex(command["payload"].as_str().unwrap_or(""))?;
                let msg_type = command["msg_type"].as_u64().ok_or("no msg_type")? as u8;
                let ext_type = command["ext_type"].as_u64().unwrap_or(0) as u16;
                self.conn()?.send_raw_frame(msg_type, ext_type, payload)?;
                Ok(json!({"event": "sent"}))
            }
            "raw_bytes" => {
                let bytes = from_hex(command["hex"].as_str().ok_or("no hex")?)?;
                self.conn()?.write(&bytes)?;
                Ok(json!({"event": "sent"}))
            }
            "closed" => {
                let deadline = Instant::now() + timeout(command);
                while self.closed.is_none() && Instant::now() < deadline {
                    self.drain(deadline.saturating_duration_since(Instant::now()));
                }
                Ok(
                    json!({"event": "closed", "closed": self.closed.is_some(), "reason": self.closed}),
                )
            }
            "close" => {
                if let Some(conn) = self.conn.take() {
                    conn.close();
                }
                Ok(json!({"event": "closed", "closed": true}))
            }
            other => Err(format!("unknown cmd {other}")),
        }
    }

    fn conn(&mut self) -> Result<&mut Conn, String> {
        self.conn.as_mut().ok_or_else(|| "not connected".to_owned())
    }

    fn connect(&mut self, command: &Value) -> Result<Value, String> {
        let host = command["host"].as_str().unwrap_or("127.0.0.1");
        let port = command["port"].as_u64().ok_or("no port")?;
        let address = format!("{host}:{port}");
        let source = command["source"].as_str();
        self.queue.clear();
        self.channels.clear();
        self.closed = None;
        if let Some(len) = command["truncate_handshake"].as_u64() {
            self.conn = Some(Conn::truncated(
                &address,
                source,
                len as usize,
                timeout(command),
            )?);
            return Ok(json!({"event": "truncated"}));
        }
        let pin = match command["pin"].as_str() {
            Some(key) => Some(conn::authority_key(key)?),
            None => None,
        };
        self.conn = Some(Conn::connect(&address, source, pin, timeout(command))?);
        Ok(json!({"event": "connected"}))
    }

    fn setup(&mut self, command: &Value) -> Result<Value, String> {
        let setup = SetupConnectionOwned {
            protocol: match command["protocol"].as_u64().unwrap_or(0) {
                0 => Protocol::MiningProtocol,
                1 => Protocol::JobDeclarationProtocol,
                _ => Protocol::TemplateDistributionProtocol,
            },
            min_version: command["min_version"].as_u64().unwrap_or(2) as u16,
            max_version: command["max_version"].as_u64().unwrap_or(2) as u16,
            flags: command["flags"].as_u64().unwrap_or(0) as u32,
            endpoint_host: str0255("127.0.0.1"),
            endpoint_port: command["port"].as_u64().unwrap_or(0) as u16,
            vendor: str0255(command["vendor"].as_str().unwrap_or("sv2-testclient")),
            hardware_version: str0255(""),
            firmware: str0255(env!("CARGO_PKG_VERSION")),
            device_id: str0255(""),
        };
        let message = CommonMessagesOwned::SetupConnection(setup);
        let (message_type, channel_bit) = (message.message_type(), message.channel_bit());
        let frame = MessageFrame::from_message(message, message_type, 0, channel_bit).unwrap();
        self.conn()?.send(frame)?;
        let reply = self.expect(timeout(command), |m| {
            matches!(
                m,
                AnyMessageOwned::Common(
                    CommonMessagesOwned::SetupConnectionSuccess(_)
                        | CommonMessagesOwned::SetupConnectionError(_)
                )
            )
        })?;
        Ok(match reply {
            AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionSuccess(success)) => {
                json!({"event": "setup_success", "used_version": success.used_version, "flags": success.flags})
            }
            AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionError(error)) => json!({
                "event": "setup_error",
                "error_code": text(&error.error_code),
                "flags": error.flags,
            }),
            _ => unreachable!(),
        })
    }

    fn open(&mut self, command: &Value, kind: Kind) -> Result<Value, String> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        let user = str0255(command["user"].as_str().ok_or("no user")?);
        let nominal_hash_rate = command["nominal_hash_rate"].as_f64().unwrap_or(0.0) as f32;
        let max_target = U256Owned::from(parse_target(&command["max_target"])?.to_le_bytes());
        let message = match kind {
            Kind::Extended => {
                MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
                    request_id,
                    user_identity: user,
                    nominal_hash_rate,
                    max_target,
                    min_extranonce_size: command["min_extranonce_size"].as_u64().unwrap_or(8)
                        as u16,
                })
            }
            Kind::Standard => {
                MiningOwned::OpenStandardMiningChannel(OpenStandardMiningChannelOwned {
                    request_id,
                    user_identity: user,
                    nominal_hash_rate,
                    max_target,
                })
            }
        };
        self.conn()?.send(frame(message))?;
        let reply = self.expect(timeout(command), |m| match m {
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(s)) => {
                s.request_id == request_id
            }
            AnyMessageOwned::Mining(MiningOwned::OpenStandardMiningChannelSuccess(s)) => {
                s.request_id == request_id
            }
            AnyMessageOwned::Mining(MiningOwned::OpenMiningChannelError(e)) => {
                e.request_id == request_id
            }
            _ => false,
        })?;
        let (channel_id, target, prefix, size, group) = match reply {
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(s)) => (
                s.channel_id,
                s.target.to_array(),
                s.extranonce_prefix.as_bytes().to_vec(),
                usize::from(s.extranonce_size),
                s.group_channel_id,
            ),
            AnyMessageOwned::Mining(MiningOwned::OpenStandardMiningChannelSuccess(s)) => (
                s.channel_id,
                s.target.to_array(),
                s.extranonce_prefix.as_bytes().to_vec(),
                0,
                s.group_channel_id,
            ),
            AnyMessageOwned::Mining(MiningOwned::OpenMiningChannelError(e)) => {
                return Ok(json!({"event": "channel_error", "error_code": text(&e.error_code)}));
            }
            _ => unreachable!(),
        };
        let target = Target::from_le_bytes(target);
        self.channels.insert(
            channel_id,
            Channel::new(channel_id, kind, target, prefix.clone(), size),
        );
        // Jobs that raced ahead of this reply were queued before the channel existed.
        let queued: Vec<AnyMessageOwned> = self.queue.iter().cloned().collect();
        for message in &queued {
            self.track(message);
        }
        Ok(json!({
            "event": "channel_opened",
            "channel_id": channel_id,
            "target": target_hex(target),
            "difficulty": target.difficulty_float(),
            "extranonce_prefix": hex(&prefix),
            "extranonce_size": size,
            "group_channel_id": group,
        }))
    }

    /// Waits for channel `channel_id` to have an active job, newer than `after` if given, and describes it.
    fn job(&mut self, command: &Value) -> Result<Value, String> {
        let id = command["channel_id"].as_u64().ok_or("no channel_id")? as u32;
        let after = command["after"].as_u64().map(|a| a as u32);
        let deadline = Instant::now() + timeout(command);
        loop {
            self.drain(Duration::ZERO);
            let channel = self.channels.get(&id).ok_or(format!("no channel {id}"))?;
            if let Some(active) = channel.active
                && after.is_none_or(|after| active > after)
            {
                let job = channel.job(active).unwrap();
                let mut value = channel.describe(job);
                value["event"] = "job".into();
                value["target"] = target_hex(channel.target).into();
                value["difficulty"] = channel.target.difficulty_float().into();
                return Ok(value);
            }
            if let Some(reason) = &self.closed {
                return Err(format!("connection closed: {reason}"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("no active job on channel {id}"));
            }
            self.drain(left.min(Duration::from_millis(50)));
        }
    }

    /// Grinds `count` shares of `kind` on channel `channel_id`, sends them in one write, and waits until
    /// the pool has answered every one. `kind` is valid, block, low_difficulty, between (meeting the
    /// easier target `meets` but not the channel's), bad_version, bad_extranonce, or duplicate. The reply
    /// carries each share's fields, so a test can send the same work again another way.
    fn submit(&mut self, command: &Value) -> Result<Value, String> {
        let id = command["channel_id"].as_u64().ok_or("no channel_id")? as u32;
        let count = command["count"].as_u64().unwrap_or(1) as usize;
        let job_id = command["job_id"].as_u64().map(|j| j as u32);
        let kind = command["kind"].as_str().unwrap_or("valid");
        self.drain(Duration::ZERO);
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(format!("no channel {id}"))?;
        let difficulty = channel.target.difficulty_float();
        let first = channel.next_sequence;
        let mut messages = Vec::new();
        let mut hashes = Vec::new();
        let mut shares = Vec::new();
        for _ in 0..count {
            let message = if kind == "duplicate" {
                channel.duplicate()?
            } else {
                let wanted = match kind {
                    "valid" => Wanted::Valid,
                    "block" => Wanted::Block,
                    "low_difficulty" => Wanted::LowDifficulty,
                    "between" => Wanted::Between(parse_target(&command["meets"])?),
                    "bad_version" => Wanted::BadVersion,
                    "bad_extranonce" => Wanted::BadExtranonce,
                    other => return Err(format!("unknown kind {other}")),
                };
                let ground = channel.grind(job_id, wanted)?;
                hashes.push(ground.hash.to_string());
                channel.last = Some(ground.message.clone());
                ground.message
            };
            shares.push(share_fields(&message));
            messages.push(message);
        }
        let last = channel.next_sequence - 1;
        let job_id = match messages.last() {
            Some(MiningOwned::SubmitSharesExtended(s)) => s.job_id,
            Some(MiningOwned::SubmitSharesStandard(s)) => s.job_id,
            _ => 0,
        };
        let conn = self.conn()?;
        for message in messages {
            conn.send(frame(message))?;
        }
        // Resolved as ESP-Miner resolves them: an acknowledgement covers up to its last_sequence_number, and
        // an error its own sequence_number.
        let (mut acks, mut errors) = (Vec::new(), Vec::new());
        let mut resolved: Option<u32> = None;
        let deadline = Instant::now() + timeout(command);
        while resolved.is_none_or(|r| r < last) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "shares {first}..={last} unanswered past {resolved:?}: acks {acks:?} errors {errors:?}"
                ));
            }
            let reply = self.expect(left, |m| match m {
                AnyMessageOwned::Mining(MiningOwned::SubmitSharesSuccess(s)) => s.channel_id == id,
                AnyMessageOwned::Mining(MiningOwned::SubmitSharesError(e)) => e.channel_id == id,
                _ => false,
            })?;
            match reply {
                AnyMessageOwned::Mining(MiningOwned::SubmitSharesSuccess(s)) => {
                    acks.push(json!({
                        "last_sequence_number": s.last_sequence_number,
                        "accepted": s.new_submits_accepted_count,
                        "sum": s.new_shares_sum,
                    }));
                    resolved = resolved.max(Some(s.last_sequence_number));
                }
                AnyMessageOwned::Mining(MiningOwned::SubmitSharesError(e)) => {
                    errors.push(json!({
                        "sequence_number": e.sequence_number,
                        "error_code": text(&e.error_code),
                    }));
                    resolved = resolved.max(Some(e.sequence_number));
                }
                _ => unreachable!(),
            }
        }
        Ok(json!({
            "event": "submitted",
            "job_id": job_id,
            "first_sequence_number": first,
            "last_sequence_number": last,
            "count": count,
            "difficulty": difficulty,
            "hashes": hashes,
            "shares": shares,
            "acks": acks,
            "errors": errors,
        }))
    }

    fn update_channel(&mut self, command: &Value) -> Result<Value, String> {
        let message = MiningOwned::UpdateChannel(UpdateChannelOwned {
            channel_id: command["channel_id"].as_u64().ok_or("no channel_id")? as u32,
            nominal_hash_rate: command["nominal_hash_rate"].as_f64().unwrap_or(0.0) as f32,
            maximum_target: U256Owned::from(parse_target(&command["max_target"])?.to_le_bytes()),
        });
        self.conn()?.send(frame(message))?;
        Ok(json!({"event": "sent"}))
    }

    /// The next queued message named `message`, for `channel_id` if given.
    fn wait(&mut self, command: &Value) -> Result<Value, String> {
        let name = command["message"].as_str().ok_or("no message")?.to_owned();
        let channel = command["channel_id"].as_u64();
        let found = self.expect(timeout(command), |m| {
            let described = describe(m);
            described["message"] == name.as_str()
                && channel.is_none_or(|c| described["channel_id"].as_u64() == Some(c))
        })?;
        let mut value = describe(&found);
        value["event"] = "message".into();
        Ok(value)
    }

    /// Takes the first queued message `wanted` matches, waiting up to `timeout` for one.
    fn expect(
        &mut self,
        timeout: Duration,
        wanted: impl Fn(&AnyMessageOwned) -> bool,
    ) -> Result<AnyMessageOwned, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(index) = self.queue.iter().position(&wanted) {
                return Ok(self.queue.remove(index).unwrap());
            }
            if let Some(reason) = &self.closed {
                return Err(format!("connection closed: {reason}"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("timed out".into());
            }
            self.drain(left.min(Duration::from_millis(50)));
        }
    }

    /// Queues what the reader delivered, waiting up to `wait` for the first of it.
    fn drain(&mut self, wait: Duration) {
        let Some(conn) = &self.conn else { return };
        let mut received = Vec::new();
        let first = if wait.is_zero() {
            conn.incoming.try_recv().ok()
        } else {
            conn.incoming.recv_timeout(wait).ok()
        };
        if let Some(first) = first {
            received.push(first);
            received.extend(conn.incoming.try_iter());
        }
        for incoming in received {
            match incoming {
                Incoming::Message(message) => {
                    self.track(&message);
                    self.queue.push_back(message);
                }
                Incoming::Closed(reason) => {
                    self.closed.get_or_insert(reason);
                }
            }
        }
    }

    /// Updates the channels from a message the pool sent.
    fn track(&mut self, message: &AnyMessageOwned) {
        let AnyMessageOwned::Mining(message) = message else {
            return;
        };
        match message {
            MiningOwned::NewExtendedMiningJob(job) => {
                if let Some(channel) = self.channels.get_mut(&job.channel_id) {
                    channel.on_extended_job(job);
                }
            }
            MiningOwned::NewMiningJob(job) => {
                if let Some(channel) = self.channels.get_mut(&job.channel_id) {
                    channel.on_standard_job(job);
                }
            }
            MiningOwned::SetNewPrevHash(prevhash) => {
                if let Some(channel) = self.channels.get_mut(&prevhash.channel_id) {
                    channel.on_prevhash(prevhash);
                }
            }
            MiningOwned::SetTarget(set) => {
                if let Some(channel) = self.channels.get_mut(&set.channel_id) {
                    channel.target = Target::from_le_bytes(set.maximum_target.to_array());
                }
            }
            _ => {}
        }
    }
}

fn text(value: &Str0255Owned) -> String {
    String::from_utf8_lossy(value.as_bytes()).into_owned()
}

/// A message as JSON: its name, and the fields tests read.
/// A submitted share's fields: the job, the miner's extranonce in hex (empty on a standard channel), and the
/// header fields it rolled.
fn share_fields(message: &MiningOwned) -> Value {
    match message {
        MiningOwned::SubmitSharesExtended(s) => json!({
            "sequence_number": s.sequence_number,
            "job_id": s.job_id,
            "extranonce": hex(s.extranonce.as_ref()),
            "ntime": s.ntime,
            "nonce": s.nonce,
            "version": s.version,
        }),
        MiningOwned::SubmitSharesStandard(s) => json!({
            "sequence_number": s.sequence_number,
            "job_id": s.job_id,
            "extranonce": "",
            "ntime": s.ntime,
            "nonce": s.nonce,
            "version": s.version,
        }),
        _ => Value::Null,
    }
}

fn describe(message: &AnyMessageOwned) -> Value {
    match message {
        AnyMessageOwned::Mining(message) => match message {
            MiningOwned::NewExtendedMiningJob(job) => json!({
                "message": "NewExtendedMiningJob",
                "channel_id": job.channel_id,
                "job_id": job.job_id,
                "min_ntime": job.min_ntime.as_ref(),
                "version": job.version,
                "version_rolling_allowed": job.version_rolling_allowed,
                "merkle_path": job.merkle_path.clone().into_inner().iter().map(|h| hex(&h.to_array())).collect::<Vec<_>>(),
                "coinbase_prefix": hex(job.coinbase_tx_prefix.as_bytes()),
                "coinbase_suffix": hex(job.coinbase_tx_suffix.as_bytes()),
            }),
            MiningOwned::NewMiningJob(job) => json!({
                "message": "NewMiningJob",
                "channel_id": job.channel_id,
                "job_id": job.job_id,
                "min_ntime": job.min_ntime.as_ref(),
                "version": job.version,
                "merkle_root": hex(&job.merkle_root.to_array()),
            }),
            MiningOwned::SetNewPrevHash(prevhash) => json!({
                "message": "SetNewPrevHash",
                "channel_id": prevhash.channel_id,
                "job_id": prevhash.job_id,
                "prev_hash": hex(&prevhash.prev_hash.to_array()),
                "min_ntime": prevhash.min_ntime,
                "nbits": prevhash.nbits,
            }),
            MiningOwned::SetTarget(set) => {
                let target = Target::from_le_bytes(set.maximum_target.to_array());
                json!({
                    "message": "SetTarget",
                    "channel_id": set.channel_id,
                    "target": target_hex(target),
                    "difficulty": target.difficulty_float(),
                })
            }
            MiningOwned::SubmitSharesSuccess(s) => json!({
                "message": "SubmitSharesSuccess",
                "channel_id": s.channel_id,
                "last_sequence_number": s.last_sequence_number,
                "accepted": s.new_submits_accepted_count,
                "sum": s.new_shares_sum,
            }),
            MiningOwned::SubmitSharesError(e) => json!({
                "message": "SubmitSharesError",
                "channel_id": e.channel_id,
                "sequence_number": e.sequence_number,
                "error_code": text(&e.error_code),
            }),
            MiningOwned::UpdateChannelError(e) => json!({
                "message": "UpdateChannelError",
                "channel_id": e.channel_id,
                "error_code": text(&e.error_code),
            }),
            other => json!({"message": other.to_string()}),
        },
        other => json!({"message": other.to_string()}),
    }
}
