//! The ZMQ hashblock subscriber. It only wakes the template task, so a fault is logged
//! and the long poll carries on alone.

use std::time::Duration;

use bitcoin::{BlockHash, hashes::Hash};
use tokio::sync::watch;
use zeromq::{Socket, SocketRecv, SubSocket, ZmqMessage};

const RETRY: Duration = Duration::from_secs(5);

/// Subscribes to `endpoint` and publishes the hash of each new block.
pub fn spawn(endpoint: String) -> watch::Receiver<Option<BlockHash>> {
    let (tips, receiver) = watch::channel(None);
    tokio::spawn(run(endpoint, tips));
    receiver
}

async fn run(endpoint: String, tips: watch::Sender<Option<BlockHash>>) {
    let mut faulted = false;
    loop {
        let error = listen(&endpoint, &tips, &mut faulted).await;
        if tips.is_closed() {
            return;
        }
        if !faulted {
            tracing::warn!(
                "ZMQ hashblock on {endpoint} failed, retrying; the long poll carries on alone: {error}"
            );
            faulted = true;
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Listens until the socket fails or nobody is subscribed, and returns why.
async fn listen(
    endpoint: &str,
    tips: &watch::Sender<Option<BlockHash>>,
    faulted: &mut bool,
) -> String {
    let mut socket = SubSocket::new();
    if let Err(e) = socket.connect(endpoint).await {
        return e.to_string();
    }
    if let Err(e) = socket.subscribe("hashblock").await {
        return e.to_string();
    }
    if std::mem::take(faulted) {
        tracing::info!("ZMQ hashblock on {endpoint} subscribed again");
    } else {
        tracing::info!("ZMQ hashblock on {endpoint} subscribed");
    }
    loop {
        let message = match socket.recv().await {
            Ok(message) => message,
            Err(e) => return e.to_string(),
        };
        match block_hash(&message) {
            Some(hash) => {
                if tips.send(Some(hash)).is_err() {
                    return "no subscribers".into();
                }
            }
            None => tracing::warn!("ignoring a malformed ZMQ hashblock message"),
        }
    }
}

/// A hashblock message is the topic, the hash in display byte order, and a sequence number.
fn block_hash(message: &ZmqMessage) -> Option<BlockHash> {
    if message.get(0)?.as_ref() != b"hashblock" {
        return None;
    }
    let mut bytes: [u8; 32] = message.get(1)?.as_ref().try_into().ok()?;
    bytes.reverse();
    Some(BlockHash::from_byte_array(bytes))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bitcoin::hex::FromHex;

    use super::*;

    #[test]
    fn reads_the_hash_in_display_order() {
        let display = "0000000000000000000075dcebb46d61c09c92dc5253eb8121aee1a25e18d5e8";
        let mut message = ZmqMessage::from(b"hashblock".to_vec());
        message.push_back(Vec::<u8>::from_hex(display).unwrap().into());
        message.push_back(vec![1, 0, 0, 0].into());
        assert_eq!(
            block_hash(&message),
            Some(BlockHash::from_str(display).unwrap())
        );
    }
}
