//! SV2 wire messages through SRI's codec: incoming frames parse into owned messages, and outgoing messages
//! frame with the type and channel bit their own definitions give.

use binary_sv2::{B032Owned, Str0255Owned, U256Owned};
use bitcoin::Target;
use codec_sv2::SerializedFrame;
use framing_sv2::framing::MessageFrame;
use parsers_sv2::{AnyMessage, AnyMessageOwned, CommonMessagesOwned, IsSv2Message, MiningOwned};

pub type Frame<T> = MessageFrame<T>;

/// The message a frame carries, or why it does not carry one.
pub fn parse(frame: &mut SerializedFrame) -> Result<AnyMessageOwned, String> {
    let header = frame.header();
    AnyMessage::try_from((header, frame.payload()))
        .map(AnyMessage::into_owned)
        .map_err(|e| format!("message type {:#04x}: {e:?}", header.msg_type()))
}

pub fn mining(message: MiningOwned) -> Frame<MiningOwned> {
    let (message_type, channel_bit) = (message.message_type(), message.channel_bit());
    MessageFrame::from_message(message, message_type, 0, channel_bit)
        .expect("the pool's messages fit a frame")
}

pub fn common(message: CommonMessagesOwned) -> Frame<CommonMessagesOwned> {
    let (message_type, channel_bit) = (message.message_type(), message.channel_bit());
    MessageFrame::from_message(message, message_type, 0, channel_bit)
        .expect("the pool's messages fit a frame")
}

/// A target as SV2 carries it: a 256-bit little-endian number.
pub fn u256(target: Target) -> U256Owned {
    target.to_le_bytes().into()
}

pub fn target(value: &U256Owned) -> Target {
    Target::from_le_bytes(value.to_array())
}

pub fn str0255(text: &str) -> Str0255Owned {
    text.try_into().expect("the pool's strings are short")
}

pub fn b032(bytes: &[u8]) -> B032Owned {
    B032Owned::new(bytes.to_vec()).expect("at most 32 bytes")
}

/// A string field as text, lossily: user identities are logged and parsed, never echoed as bytes.
pub fn text(value: &Str0255Owned) -> String {
    String::from_utf8_lossy(value.as_bytes()).into_owned()
}
