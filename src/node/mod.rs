//! The node adapter: the one trait the core sees, its JSON-RPC client, and an in-memory
//! fake behind the same trait for core tests.

#[cfg(test)]
pub mod fake;
pub mod fleet;
pub mod health;
mod rpc;
pub mod template;
mod zmq;

use std::{future::Future, path::PathBuf, time::Duration};

use bitcoin::{BlockHash, CompactTarget, ScriptBuf, Txid, Work, Wtxid, hex::FromHex};
use serde::{Deserialize, Deserializer};
use tokio::sync::watch;

pub use health::NodeHealth;
pub use rpc::RpcClient;

use crate::config;

pub trait NodeClient: Send + Sync + 'static {
    fn getblockchaininfo(&self) -> impl Future<Output = Result<BlockchainInfo, NodeError>> + Send;

    fn getnetworkinfo(&self) -> impl Future<Output = Result<NetworkInfo, NodeError>> + Send;

    /// A segwit template. Given the longpollid of the current template, the call returns once the node has
    /// a newer one.
    fn getblocktemplate(
        &self,
        longpollid: Option<&str>,
    ) -> impl Future<Output = Result<BlockTemplate, NodeError>> + Send;

    /// Submits a block in hex. Returns the node's answer: None for accepted, otherwise its reason, such as
    /// duplicate or high-hash.
    fn submitblock(
        &self,
        hex: &str,
    ) -> impl Future<Output = Result<Option<String>, NodeError>> + Send;

    /// The header of block `hash` as the node sees it, or None if the node does not know the block.
    fn getblockheader(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<BlockHeaderInfo>, NodeError>> + Send;

    /// The hash of each new block the node announces, latest first. It only wakes the node's watcher.
    fn tip_events(&self) -> watch::Receiver<Option<BlockHash>>;
}

/// The local node: JSON-RPC, plus ZMQ hashblock for tip events.
pub struct Bitcoind {
    rpc: RpcClient,
    tips: watch::Receiver<Option<BlockHash>>,
}

impl Bitcoind {
    /// Starts the ZMQ subscriber. Nothing is called on the node until the template task runs.
    pub fn start(node: &config::Node) -> Self {
        Bitcoind {
            rpc: RpcClient::new(node),
            tips: zmq::spawn(node.zmq_hashblock.clone()),
        }
    }
}

impl NodeClient for Bitcoind {
    async fn getblockchaininfo(&self) -> Result<BlockchainInfo, NodeError> {
        self.rpc.getblockchaininfo().await
    }

    async fn getnetworkinfo(&self) -> Result<NetworkInfo, NodeError> {
        self.rpc.getnetworkinfo().await
    }

    async fn getblocktemplate(&self, longpollid: Option<&str>) -> Result<BlockTemplate, NodeError> {
        self.rpc.getblocktemplate(longpollid).await
    }

    async fn submitblock(&self, hex: &str) -> Result<Option<String>, NodeError> {
        self.rpc.submitblock(hex).await
    }

    async fn getblockheader(&self, hash: BlockHash) -> Result<Option<BlockHeaderInfo>, NodeError> {
        self.rpc.getblockheader(hash).await
    }

    fn tip_events(&self) -> watch::Receiver<Option<BlockHash>> {
        self.tips.clone()
    }
}

/// The part of getblockchaininfo the pool reads.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockchainInfo {
    pub chain: String,
    pub blocks: u32,
    pub bestblockhash: BlockHash,
    /// The work of the node's chain up to its tip, which the selector selects on.
    #[serde(deserialize_with = "work")]
    pub chainwork: Work,
    pub initialblockdownload: bool,
}

/// The part of getnetworkinfo the pool reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NetworkInfo {
    /// Peers connected, in and out.
    pub connections: u32,
    /// False while P2P is turned off with setnetworkactive.
    pub networkactive: bool,
}

/// The part of getblocktemplate the pool reads.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockTemplate {
    pub version: i32,
    pub rules: Vec<String>,
    pub vbrequired: u32,
    pub previousblockhash: BlockHash,
    pub transactions: Vec<TemplateTransaction>,
    pub coinbasevalue: u64,
    pub longpollid: String,
    pub mintime: u32,
    pub curtime: u32,
    #[serde(deserialize_with = "compact_target")]
    pub bits: CompactTarget,
    pub height: u32,
    pub default_witness_commitment: Option<ScriptBuf>,
}

/// The part of getblockheader the pool reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BlockHeaderInfo {
    pub height: u32,
    /// Blocks on the node's active chain from this one to the tip, this one included; -1 when the block is
    /// not on the active chain.
    pub confirmations: i64,
    /// The work of the chain up to this block, this one included.
    #[serde(deserialize_with = "work")]
    pub chainwork: Work,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TemplateTransaction {
    /// The serialized transaction, decoded from getblocktemplate's hex.
    #[serde(deserialize_with = "hex_bytes")]
    pub data: Vec<u8>,
    pub txid: Txid,
    #[serde(rename = "hash")]
    pub wtxid: Wtxid,
    pub fee: u64,
    pub weight: u64,
}

/// Why a node call failed. A refused connection, a timeout, and an RPC error stay apart, because a hung call
/// counts as an outage.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NodeError {
    #[error("node refused the connection: {0}")]
    Refused(String),
    #[error("node did not answer within {0:?}")]
    Timeout(Duration),
    #[error("RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("unexpected node response: {0}")]
    Protocol(String),
    /// The cookie or password file cannot be read.
    #[error("cannot read credentials file {}: {reason}", path.display())]
    Credentials { path: PathBuf, reason: String },
}

pub(crate) fn compact_target<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<CompactTarget, D::Error> {
    let hex = String::deserialize(deserializer)?;
    CompactTarget::from_unprefixed_hex(&hex).map_err(serde::de::Error::custom)
}

fn work<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Work, D::Error> {
    let hex = String::deserialize(deserializer)?;
    Work::from_unprefixed_hex(&hex).map_err(serde::de::Error::custom)
}

fn hex_bytes<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let hex = String::deserialize(deserializer)?;
    Vec::from_hex(&hex).map_err(serde::de::Error::custom)
}

/// Responses recorded from the local mainnet node and the Compose regtest node, as full JSON-RPC 2.0
/// bodies. The mainnet template keeps only its first three transactions.
#[cfg(test)]
pub(crate) mod recorded {
    pub const MAINNET_GETBLOCKCHAININFO: &str =
        include_str!("../../tests/data/node/mainnet-getblockchaininfo.json");
    pub const MAINNET_GETBLOCKTEMPLATE: &str =
        include_str!("../../tests/data/node/mainnet-getblocktemplate.json");
    pub const REGTEST_GETBLOCKCHAININFO: &str =
        include_str!("../../tests/data/node/regtest-getblockchaininfo.json");
    pub const REGTEST_GETBLOCKTEMPLATE: &str =
        include_str!("../../tests/data/node/regtest-getblocktemplate.json");
    pub const MAINNET_GETNETWORKINFO: &str =
        include_str!("../../tests/data/node/mainnet-getnetworkinfo.json");
    pub const REGTEST_GETNETWORKINFO: &str =
        include_str!("../../tests/data/node/regtest-getnetworkinfo.json");

    /// The `result` of a recorded body.
    pub fn result<T: serde::de::DeserializeOwned>(body: &str) -> T {
        let mut body: serde_json::Value = serde_json::from_str(body).unwrap();
        serde_json::from_value(body["result"].take()).unwrap()
    }
}
