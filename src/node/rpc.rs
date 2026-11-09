//! A thin async JSON-RPC client for bitcoind over reqwest.

use std::{error::Error as _, time::Duration};

use bitcoin::BlockHash;
use reqwest::StatusCode;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json, value::RawValue};

use super::{BlockHeaderInfo, BlockTemplate, BlockchainInfo, NetworkInfo, NodeError};
use crate::config::{self, RpcAuth};

pub struct RpcClient {
    http: reqwest::Client,
    url: String,
    auth: RpcAuth,
    timeout: Duration,
    longpoll_timeout: Duration,
}

impl RpcClient {
    pub fn new(node: &config::Node) -> Self {
        RpcClient {
            http: reqwest::Client::new(),
            url: node.rpc_url.clone(),
            auth: node.auth.clone(),
            timeout: node.rpc_timeout,
            longpoll_timeout: node.longpoll_timeout,
        }
    }

    /// A client of the node's wallet `name`, at its own endpoint: a wallet call never reaches another wallet.
    pub fn wallet(node: &config::Node, name: &str) -> Self {
        RpcClient {
            url: format!("{}/wallet/{name}", node.rpc_url.trim_end_matches('/')),
            ..RpcClient::new(node)
        }
    }

    /// The timeout of every call but the long poll.
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<T, NodeError> {
        let request = self.request(method, params, timeout)?;
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| transport_error(e, timeout))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| transport_error(e, timeout))?;
        parse_response(method, status, &body)
    }

    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<reqwest::Request, NodeError> {
        // The cookie is read on every call, because bitcoind writes a new one each time it starts.
        let (user, password) = credentials(&self.auth)?;
        self.http
            .post(&self.url)
            .basic_auth(user, Some(password))
            .timeout(timeout)
            .json(&json!({"jsonrpc": "2.0", "id": 0, "method": method, "params": params}))
            .build()
            .map_err(|e| NodeError::Protocol(error_chain(&e)))
    }
}

impl RpcClient {
    pub async fn getblockchaininfo(&self) -> Result<BlockchainInfo, NodeError> {
        self.call("getblockchaininfo", json!([]), self.timeout)
            .await
    }

    pub async fn getnetworkinfo(&self) -> Result<NetworkInfo, NodeError> {
        self.call("getnetworkinfo", json!([]), self.timeout).await
    }

    /// With a longpollid, the call takes the long-poll timeout.
    pub async fn getblocktemplate(
        &self,
        longpollid: Option<&str>,
    ) -> Result<BlockTemplate, NodeError> {
        let (request, timeout) = match longpollid {
            Some(id) => (
                json!({"rules": ["segwit"], "longpollid": id}),
                self.longpoll_timeout,
            ),
            None => (json!({"rules": ["segwit"]}), self.timeout),
        };
        self.call("getblocktemplate", json!([request]), timeout)
            .await
    }

    pub async fn submitblock(&self, hex: &str) -> Result<Option<String>, NodeError> {
        self.call("submitblock", json!([hex]), self.timeout).await
    }

    pub async fn getblockheader(
        &self,
        hash: BlockHash,
    ) -> Result<Option<BlockHeaderInfo>, NodeError> {
        match self
            .call("getblockheader", json!([hash, true]), self.timeout)
            .await
        {
            Err(NodeError::Rpc {
                code: BLOCK_NOT_FOUND,
                ..
            }) => Ok(None),
            result => result.map(Some),
        }
    }
}

/// bitcoind's RPC_INVALID_ADDRESS_OR_KEY, which getblockheader answers for an unknown block.
const BLOCK_NOT_FOUND: i64 = -5;

fn credentials(auth: &RpcAuth) -> Result<(String, String), NodeError> {
    match auth {
        RpcAuth::CookieFile(path) => {
            let cookie_error = |reason: String| NodeError::Credentials {
                path: path.clone(),
                reason,
            };
            let text = std::fs::read_to_string(path).map_err(|e| cookie_error(e.to_string()))?;
            let (user, password) = text
                .trim()
                .split_once(':')
                .ok_or_else(|| cookie_error("no ':' between user and password".into()))?;
            Ok((user.into(), password.into()))
        }
        RpcAuth::UserPassword { user, password } => Ok((user.clone(), password.clone())),
        // Read on every call too, so a password changed on both sides needs no restart.
        RpcAuth::PasswordFile { user, path } => {
            let password = std::fs::read_to_string(path).map_err(|e| NodeError::Credentials {
                path: path.clone(),
                reason: e.to_string(),
            })?;
            Ok((user.clone(), password.trim().to_owned()))
        }
    }
}

/// The result is left unparsed until its type is known: a template parsed into a `Value` first would be thousands of
/// short-lived allocations more.
#[derive(Deserialize)]
struct Response<'a> {
    #[serde(borrow, default)]
    result: Option<&'a RawValue>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

/// Reads a JSON-RPC 2.0 body. bitcoind answers an RPC error with an error object and a failed
/// authentication with a bare HTTP status.
fn parse_response<T: DeserializeOwned>(
    method: &str,
    status: StatusCode,
    body: &[u8],
) -> Result<T, NodeError> {
    let response: Response = serde_json::from_slice(body).map_err(|e| {
        NodeError::Protocol(format!(
            "{method}: HTTP {status}, body is not JSON-RPC: {e}"
        ))
    })?;
    if let Some(error) = response.error {
        return Err(NodeError::Rpc {
            code: error.code,
            message: error.message,
        });
    }
    serde_json::from_str(response.result.map_or("null", RawValue::get))
        .map_err(|e| NodeError::Protocol(format!("{method}: cannot decode result: {e}")))
}

fn transport_error(e: reqwest::Error, timeout: Duration) -> NodeError {
    if e.is_timeout() {
        NodeError::Timeout(timeout)
    } else if e.is_connect() {
        NodeError::Refused(error_chain(&e))
    } else {
        NodeError::Protocol(error_chain(&e))
    }
}

/// An error with its sources, since reqwest's own message leaves out the cause.
fn error_chain(e: &reqwest::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, path::PathBuf};

    use bitcoin::Work;

    use super::*;
    use crate::node::recorded::{
        MAINNET_GETBLOCKCHAININFO, MAINNET_GETBLOCKTEMPLATE, MAINNET_GETNETWORKINFO,
        REGTEST_GETBLOCKCHAININFO, REGTEST_GETBLOCKTEMPLATE, REGTEST_GETNETWORKINFO,
    };

    fn client(url: String, auth: RpcAuth, timeout: Duration) -> RpcClient {
        RpcClient::new(&config::Node {
            name: config::PRIMARY.into(),
            rpc_url: url,
            auth,
            zmq_hashblock: String::new(),
            rpc_timeout: timeout,
            longpoll_timeout: timeout,
        })
    }

    fn authorization(auth: RpcAuth) -> String {
        let request = client("http://127.0.0.1:1".into(), auth, Duration::from_secs(1))
            .request("getblockchaininfo", json!([]), Duration::from_secs(1))
            .unwrap();
        request.headers()["authorization"]
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn parse<T: DeserializeOwned>(body: &str) -> T {
        parse_response("test", StatusCode::OK, body.as_bytes()).unwrap()
    }

    #[test]
    fn parses_recorded_mainnet_responses() {
        let info: BlockchainInfo = parse(MAINNET_GETBLOCKCHAININFO);
        assert_eq!(info.chain, "main");
        assert!(!info.initialblockdownload);
        assert_eq!(
            info.chainwork,
            Work::from_unprefixed_hex(
                "00000000000000000000000000000000000000014a5aa1366f2acb539345f8f2"
            )
            .unwrap()
        );

        let network: NetworkInfo = parse(MAINNET_GETNETWORKINFO);
        assert_eq!(
            network,
            NetworkInfo {
                connections: 10,
                networkactive: true
            }
        );

        let template: BlockTemplate = parse(MAINNET_GETBLOCKTEMPLATE);
        assert_eq!(template.height, info.blocks + 1);
        assert_eq!(template.previousblockhash, info.bestblockhash);
        assert_eq!(template.bits.to_consensus(), 0x17021ec5);
        assert_eq!(template.transactions.len(), 3);
        assert!(template.rules.iter().any(|r| r == "!segwit"));
        assert!(template.default_witness_commitment.is_some_and(|script| {
            script
                .as_bytes()
                .starts_with(&[0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed])
        }));
    }

    #[test]
    fn parses_recorded_regtest_responses() {
        let info: BlockchainInfo = parse(REGTEST_GETBLOCKCHAININFO);
        assert_eq!(info.chain, "regtest");
        // Two units of work per regtest block, the genesis block's included.
        assert_eq!(
            info.chainwork,
            Work::from_be_bytes({
                let mut bytes = [0; 32];
                bytes[31] = 2 * (info.blocks as u8 + 1);
                bytes
            })
        );

        let network: NetworkInfo = parse(REGTEST_GETNETWORKINFO);
        assert_eq!(
            network,
            NetworkInfo {
                connections: 0,
                networkactive: true
            },
            "the Compose node has no peers"
        );

        let template: BlockTemplate = parse(REGTEST_GETBLOCKTEMPLATE);
        assert_eq!(template.height, info.blocks + 1);
        assert_eq!(template.bits.to_consensus(), 0x207fffff);
        assert_eq!(template.transactions.len(), 1);
        assert!(template.default_witness_commitment.is_some());
    }

    #[test]
    fn reads_submitblock_answers() {
        let accepted: Option<String> = parse(r#"{"jsonrpc":"2.0","result":null,"id":0}"#);
        assert_eq!(accepted, None);
        let duplicate: Option<String> = parse(r#"{"jsonrpc":"2.0","result":"duplicate","id":0}"#);
        assert_eq!(duplicate.as_deref(), Some("duplicate"));
    }

    #[test]
    fn reads_a_block_header() {
        let body = concat!(
            r#"{"jsonrpc":"2.0","result":{"hash":"00","confirmations":-1,"height":7,"version":1,"#,
            r#""chainwork":"0000000000000000000000000000000000000000000000000000000000000010"},"id":0}"#,
        );
        let header: BlockHeaderInfo = parse(body);
        assert_eq!(
            header,
            BlockHeaderInfo {
                height: 7,
                confirmations: -1,
                chainwork: Work::from_be_bytes({
                    let mut bytes = [0; 32];
                    bytes[31] = 0x10;
                    bytes
                }),
            }
        );
    }

    #[test]
    fn a_wallet_client_calls_its_own_endpoint() {
        for url in ["http://127.0.0.1:18443", "http://127.0.0.1:18443/"] {
            let node = config::Node {
                name: config::PRIMARY.into(),
                rpc_url: url.into(),
                auth: RpcAuth::CookieFile(PathBuf::new()),
                zmq_hashblock: String::new(),
                rpc_timeout: Duration::from_secs(1),
                longpoll_timeout: Duration::from_secs(1),
            };
            let wallet = RpcClient::wallet(&node, "payouts");
            assert_eq!(wallet.url, "http://127.0.0.1:18443/wallet/payouts");
        }
    }

    #[test]
    fn keeps_an_rpc_error() {
        let body = br#"{"jsonrpc":"2.0","error":{"code":-8,"message":"bad"},"id":0}"#;
        let result: Result<Value, _> = parse_response("test", StatusCode::OK, body);
        assert_eq!(
            result.unwrap_err(),
            NodeError::Rpc {
                code: -8,
                message: "bad".into()
            }
        );
    }

    #[test]
    fn reports_a_failed_authentication() {
        let result: Result<Value, _> = parse_response("test", StatusCode::UNAUTHORIZED, b"");
        assert!(matches!(result, Err(NodeError::Protocol(m)) if m.contains("401")));
    }

    #[test]
    fn authenticates_with_a_cookie_file() {
        let path: PathBuf =
            std::env::temp_dir().join(format!("puntohunter-cookie-{}", std::process::id()));
        std::fs::write(&path, "__cookie__:secret\n").unwrap();
        let header = authorization(RpcAuth::CookieFile(path.clone()));
        std::fs::remove_file(&path).unwrap();
        // base64("__cookie__:secret")
        assert_eq!(header, "Basic X19jb29raWVfXzpzZWNyZXQ=");
    }

    #[test]
    fn authenticates_with_a_user_and_password() {
        let header = authorization(RpcAuth::UserPassword {
            user: "puntohunter".into(),
            password: "puntohunter".into(),
        });
        // base64("puntohunter:puntohunter")
        assert_eq!(header, "Basic cHVudG9odW50ZXI6cHVudG9odW50ZXI=");
    }

    #[test]
    fn reports_a_missing_cookie_file() {
        let path = PathBuf::from("/nonexistent/.cookie");
        let result = credentials(&RpcAuth::CookieFile(path.clone()));
        assert!(matches!(result, Err(NodeError::Credentials { path: p, .. }) if p == path));
    }

    #[tokio::test]
    async fn tells_a_refused_connection_from_a_timeout() {
        let auth = || RpcAuth::UserPassword {
            user: "u".into(),
            password: "p".into(),
        };
        let timeout = Duration::from_millis(200);

        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let refused = client(closed_url, auth(), timeout)
            .getblockchaininfo()
            .await;
        assert!(matches!(refused, Err(NodeError::Refused(_))), "{refused:?}");

        // The kernel completes the handshake, but nothing ever answers.
        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let silent_url = format!("http://{}", silent.local_addr().unwrap());
        let hung = client(silent_url, auth(), timeout)
            .getblockchaininfo()
            .await;
        assert_eq!(hung.unwrap_err(), NodeError::Timeout(timeout));
    }
}
