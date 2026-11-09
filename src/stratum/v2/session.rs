//! One SV2 connection: its task runs the handshake, negotiates SetupConnection, opens channels, issues jobs,
//! and acknowledges shares. A failure here ends this connection and no other.
//!
//! Acknowledgements are batched per burst: the session handles every frame already read, then sends one
//! SubmitSharesSuccess per channel for the shares it accepted. A share that arrives alone is acknowledged at
//! once; a rejection first flushes the shares accepted before it, so acknowledgements stay in sequence order.
//! Each share's response time runs from its frame read to the acknowledgement or error covering it written.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::Arc,
    time::{Instant, SystemTime},
};

use binary_sv2::GetSize;
use common_messages_sv2::{
    ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
    ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS,
    ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, Protocol, SetupConnectionErrorOwned,
    SetupConnectionOwned, SetupConnectionSuccessOwned,
};
use mining_sv2::{
    ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED,
    ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE, ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID,
    ERROR_CODE_UPDATE_CHANNEL_INVALID_CHANNEL_ID, OpenMiningChannelErrorOwned,
    SubmitSharesErrorOwned, UpdateChannelErrorOwned,
};
use parsers_sv2::{AnyMessageOwned, CommonMessagesOwned, MiningOwned};
use tokio::{net::TcpStream, sync::watch};

use super::{
    TRANSPORT,
    authority::Authority,
    channel::{Channel, Kind, Open, Passed, Share},
    message,
    transport::{self, Frames, ReadError, Writer},
};
use crate::{
    failpoint,
    geo::Country,
    metrics,
    mining::pool::Pool,
    node::template::Work,
    stratum::{Context, Rejection, username},
    telemetry::{Reject, fleet},
};

/// The one SV2 protocol version.
const VERSION: u16 = 2;
/// SetupConnection flags of the mining protocol a miner may set: REQUIRES_STANDARD_JOBS (this pool never
/// sends extended jobs to a standard channel) and REQUIRES_VERSION_ROLLING (it always allows BIP320's bits).
/// REQUIRES_WORK_SELECTION, custom jobs, is Job Declaration, which is deferred.
const SUPPORTED_FLAGS: u32 = 0b101;
/// Channels one connection may hold. Each holds an extranonce prefix.
const MAX_CHANNELS: usize = 64;
/// Frames handled before the acknowledgements they earned are sent.
const MAX_BURST: usize = 64;
/// Refused channel opens a connection may ask for before it is closed. Each is logged.
const MAX_REFUSED_OPENS: u32 = 16;
/// Submits naming no open channel a connection may send before it is closed.
const MAX_UNKNOWN_CHANNEL_SUBMITS: u32 = 16;
/// The error code of a share refused because the journal failed: not one of SV2's own, which
/// name faults of the share.
const ACCOUNTING_UNAVAILABLE: &str = "accounting-unavailable";

/// Why a session ended.
struct Close(String);

struct Session {
    context: Arc<Context>,
    peer: SocketAddr,
    /// Bound by the listener that accepted the connection; switching pools means reconnecting.
    pool: Arc<Pool>,
    writer: Writer,
    work: watch::Receiver<Work>,
    set_up: bool,
    /// The miner type SetupConnection named, without a version.
    vendor: Option<String>,
    /// The peer's country, looked up when the connection opened.
    country: Option<Country>,
    channels: BTreeMap<u32, Channel>,
    /// Whether a channel was ever opened, which stops the open deadline.
    opened: bool,
    last_channel_id: u32,
    refused_opens: u32,
    unknown_channel_submits: u32,
}

pub async fn run(
    stream: TcpStream,
    peer: SocketAddr,
    pool: Arc<Pool>,
    context: Arc<Context>,
    authority: Arc<Authority>,
) {
    tracing::info!(%peer, pool = %pool.id, transport = %TRANSPORT, "V2 connection");
    // One deadline from accept to the first open channel, over the handshake too, so a connection that
    // stalls anywhere before mining cannot hold its task forever.
    let timeout = context.authorize_timeout;
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let accepted = tokio::select! {
        accepted = transport::accept(stream, &authority) => accepted.map_err(|e| e.to_string()),
        _ = &mut deadline => Err(format!("no handshake within {timeout:?}")),
    };
    let (frames, writer) = match accepted {
        Ok(accepted) => accepted,
        Err(reason) => {
            tracing::info!(%peer, pool = %pool.id, transport = %TRANSPORT, %reason, "V2 handshake failed");
            return;
        }
    };
    let country = context.country(peer);
    let mut session = Session {
        work: context.work.clone(),
        context,
        peer,
        pool,
        writer,
        set_up: false,
        vendor: None,
        country,
        channels: BTreeMap::new(),
        opened: false,
        last_channel_id: 0,
        refused_opens: 0,
        unknown_channel_submits: 0,
    };
    let Close(reason) = session.run(frames, deadline).await;
    tracing::info!(%peer, pool = %session.pool.id, transport = %TRANSPORT, %reason, "V2 disconnected");
}

impl Session {
    async fn run(
        &mut self,
        mut frames: Frames,
        mut deadline: std::pin::Pin<&mut tokio::time::Sleep>,
    ) -> Close {
        let timeout = self.context.authorize_timeout;
        let mut vardiff_tick = tokio::time::interval(self.context.vardiff.share_interval);
        loop {
            let result = tokio::select! {
                frame = frames.receiver.recv() => match frame {
                    Some(Ok((frame, read))) => self.on_frames(frame, read, &mut frames).await,
                    Some(Err(e)) => {
                        self.warn_if_untrusted(&e);
                        Err(Close(e.to_string()))
                    }
                    None => Err(Close("the reader stopped".into())),
                },
                changed = self.work.changed(), if !self.channels.is_empty() => match changed {
                    Ok(()) => self.on_work().await,
                    Err(_) => Err(Close("the template task stopped".into())),
                },
                _ = vardiff_tick.tick(), if !self.channels.is_empty() => self.on_tick().await,
                _ = &mut deadline, if !self.opened => {
                    Err(Close(format!("no channel opened within {timeout:?}")))
                }
            };
            if let Err(close) = result {
                return close;
            }
        }
    }

    /// Warns when the miner hangs up between the handshake and SetupConnection. That is what a miner does on
    /// refusing the pool's certificate, as a Bitaxe pinning another network's key
    /// does, and it would otherwise log as an ordinary disconnect.
    fn warn_if_untrusted(&self, e: &ReadError) {
        if !self.set_up && matches!(e, ReadError::Closed | ReadError::Io(_)) {
            tracing::warn!(
                peer = %self.peer,
                pool = %self.pool.id,
                transport = %TRANSPORT,
                "V2 miner closed the connection before SetupConnection: it may not trust the pool's \
                 authority key"
            );
        }
    }

    /// Handles `first`, read at `read`, and every frame already read behind it, then acknowledges what they
    /// earned.
    async fn on_frames(
        &mut self,
        first: codec_sv2::SerializedFrame,
        read: Instant,
        frames: &mut Frames,
    ) -> Result<(), Close> {
        self.on_frame(first, read).await?;
        for _ in 1..MAX_BURST {
            match frames.receiver.try_recv() {
                Ok(Ok((frame, read))) => self.on_frame(frame, read).await?,
                Ok(Err(e)) => return Err(Close(e.to_string())),
                Err(_) => break,
            }
        }
        let ids: Vec<u32> = self.channels.keys().copied().collect();
        for id in ids {
            self.acknowledge(id).await?;
        }
        Ok(())
    }

    async fn on_frame(
        &mut self,
        mut frame: codec_sv2::SerializedFrame,
        read: Instant,
    ) -> Result<(), Close> {
        let message =
            message::parse(&mut frame).map_err(|e| Close(format!("malformed message: {e}")))?;
        // A channel open carries the user identity, which may hold a fleet secret.
        match &message {
            AnyMessageOwned::Mining(
                MiningOwned::OpenStandardMiningChannel(_)
                | MiningOwned::OpenExtendedMiningChannel(_),
            ) => tracing::trace!("V2 received a channel open"),
            _ => tracing::trace!(%message, "V2 received"),
        }
        if !self.set_up {
            return match message {
                AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(setup)) => {
                    self.setup(&setup).await
                }
                other => Err(Close(format!("{other} before SetupConnection"))),
            };
        }
        match message {
            AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(_)) => Err(Close(
                "SetupConnection on a connection already set up".into(),
            )),
            AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(open)) => {
                self.open(Open {
                    kind: Kind::Extended,
                    request_id: open.request_id,
                    user_identity: message::text(&open.user_identity),
                    nominal_hash_rate: open.nominal_hash_rate,
                    max_target: message::target(&open.max_target),
                    min_extranonce_size: open.min_extranonce_size,
                    vendor: self.vendor.clone(),
                    country: self.country.clone(),
                })
                .await
            }
            AnyMessageOwned::Mining(MiningOwned::OpenStandardMiningChannel(open)) => {
                self.open(Open {
                    kind: Kind::Standard,
                    request_id: open.request_id,
                    user_identity: message::text(&open.user_identity),
                    nominal_hash_rate: open.nominal_hash_rate,
                    max_target: message::target(&open.max_target),
                    min_extranonce_size: 0,
                    vendor: self.vendor.clone(),
                    country: self.country.clone(),
                })
                .await
            }
            AnyMessageOwned::Mining(MiningOwned::SubmitSharesExtended(share)) => {
                let channel = share.channel_id;
                let share = Share {
                    sequence_number: share.sequence_number,
                    job_id: share.job_id,
                    nonce: share.nonce,
                    ntime: share.ntime,
                    version: share.version,
                    extranonce: share.extranonce.as_bytes().to_vec(),
                };
                self.submit(channel, share, read).await
            }
            other => self.on_mining(other, read).await,
        }
    }

    /// The rest of the mining messages a miner sends, read at `read`.
    async fn on_mining(&mut self, message: AnyMessageOwned, read: Instant) -> Result<(), Close> {
        match message {
            AnyMessageOwned::Mining(MiningOwned::SubmitSharesStandard(share)) => {
                let channel = share.channel_id;
                let share = Share {
                    sequence_number: share.sequence_number,
                    job_id: share.job_id,
                    nonce: share.nonce,
                    ntime: share.ntime,
                    version: share.version,
                    extranonce: Vec::new(),
                };
                self.submit(channel, share, read).await
            }
            AnyMessageOwned::Mining(MiningOwned::UpdateChannel(update)) => {
                let now = Instant::now();
                let max_target = message::target(&update.maximum_target);
                let result = match self.channels.get_mut(&update.channel_id) {
                    None => Err(ERROR_CODE_UPDATE_CHANNEL_INVALID_CHANNEL_ID),
                    Some(channel) => channel.update(
                        &self.context.vardiff,
                        update.nominal_hash_rate,
                        max_target,
                        now,
                    ),
                };
                match result {
                    Ok(true) => self.retarget(update.channel_id).await,
                    Ok(false) => Ok(()),
                    Err(error_code) => {
                        self.send(MiningOwned::UpdateChannelError(UpdateChannelErrorOwned {
                            channel_id: update.channel_id,
                            error_code: message::str0255(error_code),
                        }))
                        .await
                    }
                }
            }
            AnyMessageOwned::Mining(MiningOwned::CloseChannel(close)) => {
                if self.channels.remove(&close.channel_id).is_some() {
                    tracing::info!(peer = %self.peer, channel = close.channel_id, "V2 channel closed");
                }
                Ok(())
            }
            other => {
                // Well-formed but nothing a miner sends to a pool: ignore it rather than guess.
                tracing::debug!(peer = %self.peer, %other, "V2 message ignored");
                Ok(())
            }
        }
    }

    /// Answers SetupConnection: success within the supported version and flags, otherwise an error that
    /// keeps the socket open for another attempt.
    async fn setup(&mut self, setup: &SetupConnectionOwned) -> Result<(), Close> {
        let refusal = negotiate(setup).err();
        let vendor = message::text(&setup.vendor);
        let Some((error_code, flags)) = refusal else {
            tracing::info!(
                peer = %self.peer,
                pool = %self.pool.id,
                transport = %TRANSPORT,
                %vendor,
                flags = format_args!("{:#x}", setup.flags),
                "V2 connection set up"
            );
            self.set_up = true;
            self.vendor = fleet::vendor(&vendor);
            return self
                .send_common(CommonMessagesOwned::SetupConnectionSuccess(
                    SetupConnectionSuccessOwned {
                        used_version: VERSION,
                        flags: 0,
                    },
                ))
                .await;
        };
        tracing::warn!(
            peer = %self.peer,
            pool = %self.pool.id,
            %vendor,
            min_version = setup.min_version,
            max_version = setup.max_version,
            flags = format_args!("{:#x}", setup.flags),
            %error_code,
            "V2 SetupConnection refused"
        );
        self.send_common(CommonMessagesOwned::SetupConnectionError(
            SetupConnectionErrorOwned {
                flags,
                error_code: message::str0255(error_code),
            },
        ))
        .await
    }

    async fn open(&mut self, open: Open) -> Result<(), Close> {
        let id = self.last_channel_id + 1;
        let opened = if self.channels.len() >= MAX_CHANNELS {
            Err(super::channel::Refusal {
                error_code: ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED,
                reason: format!("{MAX_CHANNELS} channels already open"),
            })
        } else {
            Channel::open(&self.context, &self.pool, id, &open, Instant::now())
        };
        let (channel, success) = match opened {
            Ok(opened) => opened,
            Err(refusal) => {
                tracing::warn!(
                    peer = %self.peer,
                    pool = %self.pool.id,
                    transport = %TRANSPORT,
                    user_identity = username::redacted(&open.user_identity),
                    reason = %refusal.reason,
                    error_code = %refusal.error_code,
                    "V2 channel open refused"
                );
                self.refused_opens += 1;
                if self.refused_opens > MAX_REFUSED_OPENS {
                    return Err(Close("too many refused channel opens".into()));
                }
                return self
                    .send(MiningOwned::OpenMiningChannelError(
                        OpenMiningChannelErrorOwned {
                            request_id: open.request_id,
                            error_code: message::str0255(refusal.error_code),
                        },
                    ))
                    .await;
            }
        };
        tracing::info!(
            address = %channel.worker.username.address,
            worker = %channel.worker.username.worker,
            handle = %channel.worker.handle,
            pool = %self.pool.id,
            transport = %TRANSPORT,
            channel = id,
            kind = %channel.kind.as_str(),
            "V2 channel opened"
        );
        self.last_channel_id = id;
        self.opened = true;
        self.channels.insert(id, channel);
        self.send(success).await?;
        // The current work, without marking it seen: the other channels' jobs follow the watch.
        let work = self.work.borrow().clone();
        self.issue(id, &work, false).await
    }

    /// Checks and credits `share`, read at `read`, and answers it at once if it was refused or retargets.
    async fn submit(&mut self, channel_id: u32, share: Share, read: Instant) -> Result<(), Close> {
        let sequence_number = share.sequence_number;
        let result = match self.channels.get_mut(&channel_id) {
            None => {
                self.unknown_channel_submits += 1;
                if self.unknown_channel_submits > MAX_UNKNOWN_CHANNEL_SUBMITS {
                    return Err(Close("too many submits to no open channel".into()));
                }
                self.context
                    .rejected(&self.pool.id, TRANSPORT, None, Reject::Invalid);
                Err(ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID)
            }
            Some(channel) => channel.submit(&self.context, &share, Instant::now()),
        };
        let result = match result {
            Ok(passed) => self.credit(channel_id, passed, read).await,
            Err(error_code) => Err(error_code),
        };
        match result {
            Ok(false) => Ok(()),
            Ok(true) => {
                self.acknowledge(channel_id).await?;
                self.retarget(channel_id).await
            }
            Err(error_code) => {
                tracing::debug!(
                    channel = channel_id,
                    sequence_number,
                    error_code,
                    "V2 share rejected"
                );
                if self.channels.contains_key(&channel_id) {
                    self.acknowledge(channel_id).await?;
                }
                self.send(MiningOwned::SubmitSharesError(SubmitSharesErrorOwned {
                    channel_id,
                    sequence_number,
                    error_code: message::str0255(error_code),
                }))
                .await?;
                metrics::share_answered(&self.pool.id, TRANSPORT, read);
                Ok(())
            }
        }
    }

    /// Credits a share that passed channel `id`'s checks, once it is journaled in a pool that journals, and
    /// only then adds it to the acknowledgement batch, so a SubmitSharesSuccess covers committed shares
    /// alone. Returns whether vardiff changed the target, or the SV2 error code.
    async fn credit(
        &mut self,
        id: u32,
        passed: Passed,
        read: Instant,
    ) -> Result<bool, &'static str> {
        let Some(channel) = self.channels.get(&id) else {
            return Err(ERROR_CODE_SUBMIT_SHARES_INVALID_CHANNEL_ID);
        };
        let credited = self
            .context
            .credit(
                &channel.worker,
                &passed.job,
                passed.job_id.into(),
                passed.target,
                &passed.checked,
            )
            .await;
        match credited {
            Ok(()) => {}
            Err(Rejection::Duplicate) => return Err(ERROR_CODE_SUBMIT_SHARES_DUPLICATE_SHARE),
            Err(Rejection::Unavailable) => return Err(ACCOUNTING_UNAVAILABLE),
            Err(Rejection::LowDifficulty) => unreachable!("only the check refuses so"),
        }
        if self.pool.can_pay_pool() {
            failpoint::exit_if(failpoint::BEFORE_ACKNOWLEDGEMENT);
        }
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.accept(&passed, read);
        }
        Ok(passed.retarget)
    }

    /// Sends channel `id`'s pending acknowledgement, if it has one, answering each share it covers.
    async fn acknowledge(&mut self, id: u32) -> Result<(), Close> {
        let Some((ack, read)) = self
            .channels
            .get_mut(&id)
            .and_then(Channel::acknowledgement)
        else {
            return Ok(());
        };
        self.send(ack).await?;
        for read in read {
            metrics::share_answered(&self.pool.id, TRANSPORT, read);
        }
        Ok(())
    }

    /// Announces channel `id`'s new target, then a job carrying it on the current work.
    async fn retarget(&mut self, id: u32) -> Result<(), Close> {
        let Some(channel) = self.channels.get(&id) else {
            return Ok(());
        };
        let set_target = channel.set_target();
        self.send(set_target).await?;
        let work = self.work.borrow().clone();
        self.issue(id, &work, true).await
    }

    /// Issues every channel a job on the new work. No work sends nothing and keeps the connection.
    async fn on_work(&mut self) -> Result<(), Close> {
        let work = self.work.borrow_and_update().clone();
        let ids: Vec<u32> = self.channels.keys().copied().collect();
        for id in ids {
            self.issue(id, &work, false).await?;
        }
        Ok(())
    }

    async fn on_tick(&mut self) -> Result<(), Close> {
        let now = Instant::now();
        let eased: Vec<u32> = self
            .channels
            .values_mut()
            .filter_map(|channel| channel.on_tick(now).then_some(channel.id))
            .collect();
        for id in eased {
            self.retarget(id).await?;
        }
        Ok(())
    }

    async fn issue(&mut self, id: u32, work: &Work, retarget: bool) -> Result<(), Close> {
        let (Work::Template(template), Some(channel)) = (work, self.channels.get_mut(&id)) else {
            return Ok(());
        };
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        for message in channel.issue(&self.context, template, now, retarget) {
            self.send(message).await?;
        }
        Ok(())
    }

    async fn send(&mut self, message: MiningOwned) -> Result<(), Close> {
        tracing::trace!(%message, "V2 sent");
        self.write(message::mining(message)).await
    }

    async fn send_common(&mut self, message: CommonMessagesOwned) -> Result<(), Close> {
        self.write(message::common(message)).await
    }

    async fn write<T: binary_sv2::Serialize + GetSize>(
        &mut self,
        frame: message::Frame<T>,
    ) -> Result<(), Close> {
        self.writer
            .send(frame)
            .await
            .map_err(|e| Close(format!("write failed: {e}")))
    }
}

/// SetupConnection's answer: success within the supported version and flags, or the SV2 error code and the
/// flags it refuses.
fn negotiate(setup: &SetupConnectionOwned) -> Result<(), (&'static str, u32)> {
    if setup.protocol != Protocol::MiningProtocol {
        Err((ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, 0))
    } else if !(setup.min_version..=setup.max_version).contains(&VERSION) {
        Err((ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH, 0))
    } else if setup.flags & !SUPPORTED_FLAGS != 0 {
        Err((
            ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS,
            setup.flags & !SUPPORTED_FLAGS,
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(
        protocol: Protocol,
        min_version: u16,
        max_version: u16,
        flags: u32,
    ) -> SetupConnectionOwned {
        SetupConnectionOwned {
            protocol,
            min_version,
            max_version,
            flags,
            endpoint_host: message::str0255("127.0.0.1"),
            endpoint_port: 3334,
            vendor: message::str0255("test"),
            hardware_version: message::str0255(""),
            firmware: message::str0255(""),
            device_id: message::str0255(""),
        }
    }

    #[test]
    fn accepts_a_range_holding_version_2() {
        assert_eq!(negotiate(&setup(Protocol::MiningProtocol, 2, 2, 0)), Ok(()));
        assert_eq!(negotiate(&setup(Protocol::MiningProtocol, 1, 3, 0)), Ok(()));
    }

    #[test]
    fn accepts_the_flags_esp_miner_and_puntominer_send() {
        // REQUIRES_VERSION_ROLLING always, with REQUIRES_STANDARD_JOBS on a standard channel.
        for flags in [0b000, 0b100, 0b101] {
            assert_eq!(
                negotiate(&setup(Protocol::MiningProtocol, 2, 2, flags)),
                Ok(())
            );
        }
    }

    #[test]
    fn refuses_an_out_of_range_version() {
        for (min, max) in [(3, 5), (1, 1)] {
            assert_eq!(
                negotiate(&setup(Protocol::MiningProtocol, min, max, 0)),
                Err((ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH, 0))
            );
        }
    }

    #[test]
    fn refuses_another_protocol() {
        assert_eq!(
            negotiate(&setup(Protocol::JobDeclarationProtocol, 2, 2, 0)),
            Err((ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, 0))
        );
    }

    #[test]
    fn refuses_work_selection_and_unknown_flags_naming_them() {
        // REQUIRES_WORK_SELECTION is Job Declaration's, deferred.
        assert_eq!(
            negotiate(&setup(Protocol::MiningProtocol, 2, 2, 0b110)),
            Err((ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS, 0b010))
        );
        assert_eq!(
            negotiate(&setup(Protocol::MiningProtocol, 2, 2, 1 << 31)),
            Err((
                ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS,
                1 << 31
            ))
        );
    }
}
