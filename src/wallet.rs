//! The payout wallet's client: JSON-RPC to the node's `/wallet/<name>` endpoint,
//! apart from `NodeClient`, which knows nothing of wallets. The wallet holds the pool wallet's key alone, so
//! what it can spend is the pool's coinbases and nothing that pays a miner or a fee address.

use bitcoin::{
    Address, Amount, BlockHash, OutPoint, Transaction, Txid,
    consensus::encode::{deserialize_hex, serialize_hex},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    config,
    node::{NodeError, RpcClient},
};

pub struct Wallet {
    rpc: RpcClient,
}

#[derive(Deserialize)]
struct Balances {
    mine: Mine,
}

#[derive(Deserialize)]
struct Mine {
    #[serde(with = "bitcoin::amount::serde::as_btc")]
    trusted: Amount,
    #[serde(with = "bitcoin::amount::serde::as_btc")]
    immature: Amount,
}

#[derive(Deserialize)]
struct MempoolInfo {
    /// In BTC/kvB.
    #[serde(with = "bitcoin::amount::serde::as_btc")]
    mempoolminfee: Amount,
}

#[derive(Deserialize)]
struct SmartFee {
    /// In BTC/kvB; absent when the node has no estimate.
    #[serde(default, with = "bitcoin::amount::serde::as_btc::opt")]
    feerate: Option<Amount>,
}

#[derive(Deserialize)]
struct FundedRaw {
    hex: String,
    #[serde(with = "bitcoin::amount::serde::as_btc")]
    fee: Amount,
}

/// A transaction the wallet funded, not yet signed.
pub struct Funded {
    pub tx: Transaction,
    pub fee: Amount,
}

/// Why a funded transaction was not signed.
#[derive(Debug, thiserror::Error)]
pub enum SignError {
    /// The wallet is encrypted and locked: signing waits for the operator to unlock it.
    #[error("the payout wallet is locked")]
    Locked,
    #[error(transparent)]
    Node(#[from] NodeError),
}

#[derive(Deserialize)]
struct Signed {
    hex: String,
    complete: bool,
}

#[derive(Deserialize)]
struct WalletTransaction {
    confirmations: i64,
}

#[derive(Deserialize)]
struct SinceBlock {
    transactions: Vec<Entry>,
}

/// One entry of a wallet transaction: a transaction has one for each output it sends or receives.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Entry {
    pub txid: Txid,
    /// `send`, `receive`, or, for a coinbase, `generate`, `immature`, or `orphan`.
    pub category: String,
    /// The block holding it, while one on the active chain does.
    pub blockhash: Option<BlockHash>,
    /// 0 in the mempool, negative if a conflict confirmed.
    pub confirmations: i64,
}

#[derive(Deserialize)]
struct Locked {
    txid: Txid,
    vout: u32,
}

/// bitcoind's RPC_INVALID_PARAMETER, which lockunspent answers for an output already spent.
const INVALID_PARAMETER: i64 = -8;
/// bitcoind's RPC_VERIFY_ALREADY_IN_UTXO_SET, which sendrawtransaction answers for a transaction already in a
/// block.
const ALREADY_IN_UTXO_SET: i64 = -27;
/// bitcoind's RPC_INVALID_ADDRESS_OR_KEY, which gettransaction answers for a transaction the wallet lacks.
const NOT_FOUND: i64 = -5;
/// bitcoind's RPC_WALLET_UNLOCK_NEEDED, which an encrypted wallet answers to signing while it is locked.
const UNLOCK_NEEDED: i64 = -13;

impl Wallet {
    pub fn new(node: &config::Node, name: &str) -> Self {
        Wallet {
            rpc: RpcClient::wallet(node, name),
        }
    }

    /// What the wallet may spend now: its confirmed outputs, mature coinbases among them, and its own
    /// unconfirmed change; never an immature coinbase.
    pub async fn spendable(&self) -> Result<Amount, NodeError> {
        let balances: Balances = self.call("getbalances", json!([])).await?;
        Ok(balances.mine.trusted)
    }

    /// What the wallet holds: what it may spend now, and its coinbases not yet mature.
    pub async fn held(&self) -> Result<Amount, NodeError> {
        let balances: Balances = self.call("getbalances", json!([])).await?;
        Ok(balances.mine.trusted + balances.mine.immature)
    }

    /// Every transaction the wallet holds, an entry for each output it sends or receives.
    pub async fn transactions(&self) -> Result<Vec<Entry>, NodeError> {
        let since: SinceBlock = self.call("listsinceblock", json!([])).await?;
        Ok(since.transactions)
    }

    /// The lowest fee rate the node's mempool takes now, in sat/kvB.
    pub async fn min_fee_rate(&self) -> Result<u64, NodeError> {
        let info: MempoolInfo = self.call("getmempoolinfo", json!([])).await?;
        Ok(info.mempoolminfee.to_sat())
    }

    /// The node's fee estimate for confirmation within `target` blocks, in sat/kvB; None while it has none,
    /// as on regtest or before it has seen enough blocks.
    pub async fn estimate_fee_rate(&self, target: u16) -> Result<Option<u64>, NodeError> {
        let estimate: SmartFee = self.call("estimatesmartfee", json!([target])).await?;
        Ok(estimate.feerate.map(Amount::to_sat))
    }

    /// Funds a transaction paying `outputs` in their order, at `sat_per_kvb`, from confirmed outputs the
    /// wallet has not locked, with its change to `change`.
    pub async fn fund(
        &self,
        outputs: &[(String, Amount)],
        change: &Address,
        sat_per_kvb: u64,
    ) -> Result<Funded, NodeError> {
        let outputs: Vec<Value> = outputs
            .iter()
            .map(|(address, amount)| json!({ address: amount.to_btc() }))
            .collect();
        let raw: String = self
            .call("createrawtransaction", json!([[], outputs]))
            .await?;
        let options = json!({
            "add_inputs": true,
            "minconf": 1,
            "changeAddress": change.to_string(),
            "fee_rate": sat_per_kvb as f64 / 1000.0,
            "lockUnspents": false,
        });
        let funded: FundedRaw = self
            .call("fundrawtransaction", json!([raw, options]))
            .await?;
        Ok(Funded {
            tx: decode("fundrawtransaction", &funded.hex)?,
            fee: funded.fee,
        })
    }

    /// Signs `tx`, which the wallet funded. Its txid does not change: every input the wallet holds is segwit.
    pub async fn sign(&self, tx: &Transaction) -> Result<Transaction, SignError> {
        let signed: Signed = match self
            .call("signrawtransactionwithwallet", json!([serialize_hex(tx)]))
            .await
        {
            Err(NodeError::Rpc {
                code: UNLOCK_NEEDED,
                ..
            }) => return Err(SignError::Locked),
            result => result?,
        };
        if !signed.complete {
            return Err(NodeError::Protocol(
                "signrawtransactionwithwallet: the wallet could not sign every input".into(),
            )
            .into());
        }
        Ok(decode("signrawtransactionwithwallet", &signed.hex)?)
    }

    /// Sends `tx` to the node, which relays it. Sending it again is harmless: the node already has it.
    pub async fn send(&self, tx: &Transaction) -> Result<(), NodeError> {
        match self
            .call::<Txid>("sendrawtransaction", json!([serialize_hex(tx)]))
            .await
        {
            Err(NodeError::Rpc {
                code: ALREADY_IN_UTXO_SET,
                ..
            }) => Ok(()),
            result => result.map(drop),
        }
    }

    /// How deep `txid` is: 0 in the mempool, negative if a conflict confirmed, None if the wallet lacks it.
    pub async fn confirmations(&self, txid: Txid) -> Result<Option<i64>, NodeError> {
        match self.call("gettransaction", json!([txid])).await {
            Err(NodeError::Rpc {
                code: NOT_FOUND, ..
            }) => Ok(None),
            result => result.map(|tx: WalletTransaction| Some(tx.confirmations)),
        }
    }

    /// The outputs locked against the wallet's own coin selection.
    pub async fn locked(&self) -> Result<Vec<OutPoint>, NodeError> {
        let locked: Vec<Locked> = self.call("listlockunspent", json!([])).await?;
        Ok(locked
            .into_iter()
            .map(|locked| OutPoint::new(locked.txid, locked.vout))
            .collect())
    }

    /// Locks `outpoint` against the wallet's coin selection, in the wallet's database, so the lock outlives a
    /// node restart. Returns false for an output already spent, which no selection can take.
    pub async fn lock(&self, outpoint: OutPoint) -> Result<bool, NodeError> {
        let outpoints = json!([{"txid": outpoint.txid, "vout": outpoint.vout}]);
        match self
            .call::<bool>("lockunspent", json!([false, outpoints, true]))
            .await
        {
            Err(NodeError::Rpc {
                code: INVALID_PARAMETER,
                ..
            }) => Ok(false),
            result => result,
        }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, NodeError> {
        self.rpc.call(method, params, self.rpc.timeout()).await
    }
}

fn decode(method: &str, hex: &str) -> Result<Transaction, NodeError> {
    deserialize_hex(hex)
        .map_err(|e| NodeError::Protocol(format!("{method}: cannot decode the transaction: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_trusted_and_immature_balances() {
        let body = r#"{"mine":{"trusted":50.00000000,"untrusted_pending":0.00000000,"immature":5000.12345678}}"#;
        let balances: Balances = serde_json::from_str(body).unwrap();
        assert_eq!(balances.mine.trusted, Amount::from_sat(5_000_000_000));
        assert_eq!(balances.mine.immature, Amount::from_sat(500_012_345_678));
    }

    #[test]
    fn reads_a_fee_estimate_or_its_absence() {
        let estimate: SmartFee =
            serde_json::from_str(r#"{"feerate":0.00012345,"blocks":6}"#).unwrap();
        assert_eq!(estimate.feerate.map(Amount::to_sat), Some(12_345));
        let none: SmartFee = serde_json::from_str(
            r#"{"errors":["Insufficient data or no feerate found"],"blocks":0}"#,
        )
        .unwrap();
        assert_eq!(none.feerate, None);
    }

    /// Asks a local mainnet node, `config/mainnet.toml`'s, for the estimate a mainnet batch pays.
    #[tokio::test]
    #[ignore = "asks a local mainnet node; run by hand"]
    async fn the_mainnet_node_estimates_a_fee_rate() {
        let config = config::Config::load(std::path::Path::new("config/mainnet.toml")).unwrap();
        let wallet = Wallet::new(&config.node, "payouts");
        let rate = wallet.estimate_fee_rate(6).await.unwrap();
        println!("estimatesmartfee 6: {rate:?} sat/kvB");
        assert!(rate.is_some_and(|rate| rate > 0), "{rate:?}");
    }

    #[test]
    fn reads_each_transaction_entry_with_its_block_if_any() {
        let body = r#"{"transactions":[
            {"address":"bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s","category":"orphan","amount":50.0,
             "generated":true,"confirmations":0,"trusted":false,
             "txid":"c7c80c6f23bd81bb64f923e3268a5996b3519f9536c81a14eb9ce75bef0f0994"},
            {"address":"bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s","category":"receive","amount":10.0,
             "confirmations":2,
             "blockhash":"19aaecf3ae9f566ebe84fb549e653af98c290c9fd0081f36191c6686458b2faf",
             "txid":"84544b9ab8e9c2c79542655646b0c82ffd383e8475c4deeb3c722c427d1076b7"}
        ],"removed":[],"lastblock":"7d1c0caca134d9ef8cc3b2d4528819686efa9c07eafebeb9d86d9d20c5344168"}"#;
        let since: SinceBlock = serde_json::from_str(body).unwrap();
        let [orphan, receive] = &since.transactions[..] else {
            panic!("two entries")
        };
        assert_eq!(
            (orphan.category.as_str(), orphan.blockhash),
            ("orphan", None)
        );
        assert_eq!(receive.confirmations, 2);
        assert_eq!(
            receive.blockhash.unwrap().to_string(),
            "19aaecf3ae9f566ebe84fb549e653af98c290c9fd0081f36191c6686458b2faf"
        );
    }
}
