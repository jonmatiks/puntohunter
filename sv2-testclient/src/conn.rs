//! The client's end of the SV2 transport: the Noise NX handshake as initiator, a reader thread that decodes
//! frames onto a channel, and a writer that encrypts them.

use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

use binary_sv2::{GetSize, Serialize};
use codec_sv2::{
    Decoded, Decrypted, Handshake, InitiatorSent, NoiseDecoder, NoiseEncoder,
    TransportDecryptState, TransportEncryptState,
};
use framing_sv2::{framing::MessageFrame, header::Header};
use noise_sv2::{INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE, Initiator};
use parsers_sv2::{AnyMessage, AnyMessageOwned};
use socket2::{Domain, Socket, Type};

/// What the reader thread delivers.
pub enum Incoming {
    Message(AnyMessageOwned),
    /// The connection ended, and why.
    Closed(String),
}

pub struct Conn {
    stream: TcpStream,
    encoder: NoiseEncoder,
    /// None when the handshake was deliberately left unfinished.
    state: Option<TransportEncryptState>,
    pub incoming: mpsc::Receiver<Incoming>,
}

/// An authority key as the pool prints it: SRI's base58check, or 64 hex characters.
pub fn authority_key(text: &str) -> Result<[u8; 32], String> {
    if text.len() == 64 {
        let mut key = [0; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
        }
        return Ok(key);
    }
    let decoded = bitcoin::base58::decode_check(text).map_err(|e| e.to_string())?;
    match decoded.split_first_chunk::<2>() {
        Some(([1, 0], key)) if key.len() == 32 => Ok(key.try_into().unwrap()),
        _ => Err(format!("{text} is not an SRI authority key")),
    }
}

impl Conn {
    /// Connects from `source`, when given, and completes the handshake, verifying the certificate against
    /// `pin` when given.
    pub fn connect(
        address: &str,
        source: Option<&str>,
        pin: Option<[u8; 32]>,
        timeout: Duration,
    ) -> Result<Conn, String> {
        let mut stream = open(address, source, timeout)?;
        let initiator = match pin {
            Some(key) => Initiator::from_raw_k(key).map_err(|e| format!("pin: {e:?}"))?,
            None => Initiator::without_pk().map_err(|e| format!("{e:?}"))?,
        };
        let mut encoder = NoiseEncoder::new();
        let (first, sent) = Handshake::initiator(initiator)
            .step_0()
            .map_err(|e| e.to_string())?;
        stream
            .write_all(&encoder.encode_handshake(first))
            .map_err(|e| e.to_string())?;
        let mut decoder = NoiseDecoder::new();
        let second = loop {
            match decoder
                .next_handshake_frame::<InitiatorSent>()
                .map_err(|e| e.to_string())?
            {
                Decoded::Frame(frame) => break frame,
                Decoded::Incomplete(_) => stream
                    .read_exact(decoder.writable())
                    .map_err(|e| format!("handshake: {e}"))?,
            }
        };
        let second: [u8; INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE] =
            second.payload().try_into().unwrap();
        let transport = sent
            .step_2(second)
            .map_err(|e| format!("handshake failed: {e}"))?;
        let (encrypt, decrypt) = transport.split();
        let incoming = spawn_reader(
            stream.try_clone().map_err(|e| e.to_string())?,
            decoder,
            decrypt,
        );
        Ok(Conn {
            stream,
            encoder,
            state: Some(encrypt),
            incoming,
        })
    }

    /// Connects and sends only the first `len` bytes of the handshake's first message.
    pub fn truncated(
        address: &str,
        source: Option<&str>,
        len: usize,
        timeout: Duration,
    ) -> Result<Conn, String> {
        let mut stream = open(address, source, timeout)?;
        let initiator = Initiator::without_pk().map_err(|e| format!("{e:?}"))?;
        let mut encoder = NoiseEncoder::new();
        let (first, _) = Handshake::initiator(initiator)
            .step_0()
            .map_err(|e| e.to_string())?;
        let first = encoder.encode_handshake(first);
        stream
            .write_all(&first[..len.min(first.len())])
            .map_err(|e| e.to_string())?;
        let (sender, incoming) = mpsc::channel();
        let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
        reader.set_read_timeout(None).map_err(|e| e.to_string())?;
        thread::spawn(move || {
            let mut byte = [0];
            let reason = match reader.read(&mut byte) {
                Ok(0) => "closed by the pool".to_owned(),
                Ok(_) => "the pool answered an unfinished handshake".to_owned(),
                Err(e) => e.to_string(),
            };
            let _ = sender.send(Incoming::Closed(reason));
        });
        Ok(Conn {
            stream,
            encoder,
            state: None,
            incoming,
        })
    }

    pub fn send<T: Serialize + GetSize>(&mut self, frame: MessageFrame<T>) -> Result<(), String> {
        let state = self
            .state
            .as_mut()
            .ok_or("no transport: the handshake is unfinished")?;
        let bytes = self
            .encoder
            .encode_transport(frame, state)
            .map_err(|e| e.to_string())?;
        self.write(&bytes)
    }

    /// Encrypts and sends a frame of `payload` under a header of the caller's choosing, whatever the
    /// payload holds.
    pub fn send_raw_frame(
        &mut self,
        msg_type: u8,
        ext_type: u16,
        payload: Vec<u8>,
    ) -> Result<(), String> {
        let frame = MessageFrame::from_message(Raw(payload), msg_type, ext_type, false)
            .map_err(|e| format!("{e:?}"))?;
        self.send(frame)
    }

    /// Writes `bytes` to the socket as they are, outside any frame or encryption.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream.write_all(bytes).map_err(|e| e.to_string())
    }

    pub fn close(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// A connection to `address`, from the IP address `source` when given.
fn open(address: &str, source: Option<&str>, timeout: Duration) -> Result<TcpStream, String> {
    let failed = |e: std::io::Error| format!("connect {address}: {e}");
    let stream = match source {
        None => TcpStream::connect(address).map_err(failed)?,
        Some(source) => {
            let to: SocketAddr = address.parse().map_err(|e| format!("{address}: {e}"))?;
            let from: IpAddr = source.parse().map_err(|e| format!("{source}: {e}"))?;
            let socket =
                Socket::new(Domain::for_address(to), Type::STREAM, None).map_err(failed)?;
            socket
                .bind(&SocketAddr::new(from, 0).into())
                .map_err(failed)?;
            socket.connect(&to.into()).map_err(failed)?;
            socket.into()
        }
    };
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    Ok(stream)
}

fn spawn_reader(
    mut stream: TcpStream,
    mut decoder: NoiseDecoder,
    mut state: TransportDecryptState,
) -> mpsc::Receiver<Incoming> {
    let (sender, receiver) = mpsc::channel();
    // The handshake's timeout no longer applies: a quiet pool is not a closed one.
    let _ = stream.set_read_timeout(None);
    thread::spawn(move || {
        loop {
            match decoder.next_transport_frame(state) {
                Ok(Decrypted::Frame(mut frame, next)) => {
                    state = next;
                    let header: Header = frame.header();
                    let message = match AnyMessage::try_from((header, frame.payload())) {
                        Ok(message) => Incoming::Message(message.into_owned()),
                        Err(e) => {
                            Incoming::Closed(format!("the pool sent a malformed message: {e:?}"))
                        }
                    };
                    if sender.send(message).is_err() {
                        return;
                    }
                }
                Ok(Decrypted::Incomplete(len, next)) => {
                    state = next;
                    if len > 0
                        && let Err(e) = stream.read_exact(decoder.writable())
                    {
                        let reason = match e.kind() {
                            std::io::ErrorKind::UnexpectedEof => "closed by the pool".to_owned(),
                            _ => e.to_string(),
                        };
                        let _ = sender.send(Incoming::Closed(reason));
                        return;
                    }
                }
                Err(e) => {
                    let _ = sender.send(Incoming::Closed(format!("undecryptable frame: {e}")));
                    return;
                }
            }
        }
    });
    receiver
}

/// Arbitrary payload bytes, framed as they are.
struct Raw(Vec<u8>);

impl GetSize for Raw {
    fn get_size(&self) -> usize {
        self.0.len()
    }
}

impl<'a> From<Raw> for binary_sv2::encodable::EncodableField<'a> {
    fn from(raw: Raw) -> Self {
        binary_sv2::encodable::EncodableField::Struct(
            raw.0
                .into_iter()
                .map(|byte| {
                    binary_sv2::encodable::EncodableField::Primitive(
                        binary_sv2::encodable::EncodablePrimitive::U8(byte),
                    )
                })
                .collect(),
        )
    }
}
