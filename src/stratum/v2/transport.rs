//! The SV2 transport: the Noise NX handshake as responder, then SV2 frames with AEAD on every one.
//!
//! A reader task decodes frames into a channel, so the session can wait on frames beside its other events
//! without cancelling a half-read frame. Each frame carries when its last byte was read, so a share's response
//! time counts its wait in the queue. A malformed frame, a failed decryption, or a frame past the size cap
//! ends that task, and with it the one connection.

use std::time::Instant;

use binary_sv2::{GetSize, Serialize};
use codec_sv2::{
    Decoded, Decrypted, Handshake, NoiseDecoder, NoiseEncoder, SerializedFrame,
    TransportDecryptState, TransportEncryptState,
};
use framing_sv2::framing::MessageFrame;
use noise_sv2::{ELLSWIFT_ENCODING_SIZE, Responder};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::mpsc,
    task::JoinHandle,
};

use super::authority::Authority;

/// The most encrypted bytes one frame from a miner may take. The largest message a miner sends,
/// SetupConnection, is well under 2 KiB.
const MAX_FRAME: usize = 16 * 1024;
/// Frames decoded ahead of the session.
const QUEUE: usize = 64;

/// Why reading ended.
#[derive(Debug)]
pub enum ReadError {
    Closed,
    Io(std::io::Error),
    /// A frame that does not decrypt or does not parse as SV2 framing.
    Malformed(String),
    TooLong,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Closed => f.write_str("closed by the miner"),
            ReadError::Io(e) => write!(f, "read failed: {e}"),
            ReadError::Malformed(e) => write!(f, "malformed frame: {e}"),
            ReadError::TooLong => write!(f, "a frame past {MAX_FRAME} bytes"),
        }
    }
}

/// The frames a connection's reader task decodes, each with when it was read. Dropping it stops the task.
pub struct Frames {
    pub receiver: mpsc::Receiver<Result<(SerializedFrame, Instant), ReadError>>,
    task: JoinHandle<()>,
}

impl Drop for Frames {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Encrypts and writes frames.
pub struct Writer {
    half: OwnedWriteHalf,
    encoder: NoiseEncoder,
    state: TransportEncryptState,
}

impl Writer {
    pub async fn send<T: Serialize + GetSize>(
        &mut self,
        frame: MessageFrame<T>,
    ) -> std::io::Result<()> {
        let bytes = self
            .encoder
            .encode_transport(frame, &mut self.state)
            .map_err(|e| std::io::Error::other(format!("cannot encode a frame: {e}")))?;
        self.half.write_all(&bytes).await
    }
}

/// Completes the handshake as responder on `stream`, certified by `authority`, and starts its reader task.
pub async fn accept(
    stream: TcpStream,
    authority: &Authority,
) -> Result<(Frames, Writer), ReadError> {
    let (mut reader, mut half) = stream.into_split();
    let mut decoder = NoiseDecoder::new();
    let first = loop {
        match decoder
            .next_handshake_frame::<Responder>()
            .map_err(|e| ReadError::Malformed(e.to_string()))?
        {
            Decoded::Frame(frame) => break frame,
            Decoded::Incomplete(_) => read(&mut reader, decoder.writable()).await?,
        }
    };
    let first: [u8; ELLSWIFT_ENCODING_SIZE] = first
        .payload()
        .try_into()
        .expect("the decoder takes exactly the initiator's first message");
    let (reply, transport) = Handshake::responder(authority.responder())
        .step_1(first)
        .map_err(|e| ReadError::Malformed(format!("handshake: {e}")))?;
    let mut encoder = NoiseEncoder::new();
    half.write_all(&encoder.encode_handshake(reply))
        .await
        .map_err(ReadError::Io)?;
    let (encrypt, decrypt) = transport.split();
    let (sender, receiver) = mpsc::channel(QUEUE);
    let task = tokio::spawn(read_frames(reader, decoder, decrypt, sender));
    Ok((
        Frames { receiver, task },
        Writer {
            half,
            encoder,
            state: encrypt,
        },
    ))
}

async fn read_frames(
    mut reader: OwnedReadHalf,
    mut decoder: NoiseDecoder,
    mut state: TransportDecryptState,
    sender: mpsc::Sender<Result<(SerializedFrame, Instant), ReadError>>,
) {
    loop {
        match read_frame(&mut reader, &mut decoder, state).await {
            Ok((frame, next)) => {
                state = next;
                if sender.send(Ok((frame, Instant::now()))).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = sender.send(Err(e)).await;
                return;
            }
        }
    }
}

async fn read_frame(
    reader: &mut OwnedReadHalf,
    decoder: &mut NoiseDecoder,
    mut state: TransportDecryptState,
) -> Result<(SerializedFrame, TransportDecryptState), ReadError> {
    let mut taken = 0;
    loop {
        match decoder
            .next_transport_frame(state)
            .map_err(|e| ReadError::Malformed(e.to_string()))?
        {
            Decrypted::Frame(frame, next) => return Ok((frame, next)),
            Decrypted::Incomplete(len, next) => {
                state = next;
                // Zero means the bytes are already buffered.
                if len > 0 {
                    taken += len;
                    if taken > MAX_FRAME {
                        return Err(ReadError::TooLong);
                    }
                    read(reader, decoder.writable()).await?;
                }
            }
        }
    }
}

async fn read(reader: &mut OwnedReadHalf, buffer: &mut [u8]) -> Result<(), ReadError> {
    match reader.read_exact(buffer).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(ReadError::Closed),
        Err(e) => Err(ReadError::Io(e)),
    }
}
