//! Startup reconciliation. On every start, before the payout worker's
//! first batch, the books must agree with themselves, with the payout wallet, and with the chain. Any
//! mismatch halts payouts until a start finds none; mining and the journal carry on. A restored backup that
//! lags the wallet fails here, so it is safe without being detected as a restore.
//!
//! [`audit`] checks the books alone. [`books`] reads what they expect of the wallet, and [`compare`] holds
//! that against what the wallet reports.

use std::collections::{HashMap, HashSet};

use bitcoin::{Amount, BlockHash, OutPoint, Txid};
use rusqlite::Transaction;

use super::{ledger, payouts};
use crate::wallet::Entry;

/// What the books expect of the wallet.
#[derive(Debug, Default)]
pub struct Books {
    pub batches: Vec<Batch>,
    /// The inputs of the batches not yet confirmed: the only inputs the wallet may have locked.
    pub inputs: HashSet<OutPoint>,
    /// Every block the pool found: the only blocks whose coinbase the wallet may hold.
    pub blocks: HashSet<BlockHash>,
    pub owed: Amount,
    pub reserve: Amount,
}

#[derive(Debug)]
pub struct Batch {
    pub id: i64,
    pub state: String,
    /// The signed transaction's, or, while the batch is reserved, the funded one's, which signing keeps.
    pub txid: Txid,
    /// What it reserved: its payees' balances and its fee.
    pub reserved: Amount,
}

/// What the wallet reports.
#[derive(Debug, Default)]
pub struct Holdings {
    pub locked: Vec<OutPoint>,
    pub transactions: Vec<Entry>,
    /// What it holds, spendable or immature.
    pub held: Amount,
}

/// Checks the books against themselves: each block's allocations sum to the amount it allocated; each
/// allocation is credited in full while its block may be on the chain and not at all once it is not, blocks
/// paused for reconciliation aside; no balance is below zero; and each batch reserved its payees and its fee,
/// and paid them once it was confirmed and not before. Returns each mismatch found.
pub fn audit(t: &Transaction) -> rusqlite::Result<Vec<String>> {
    let mut mismatches = Vec::new();
    let mut rows = t.prepare(
        "SELECT s.block, s.amount, (SELECT COALESCE(SUM(sats), 0) FROM allocations WHERE block = s.block) \
         FROM snapshots s ORDER BY s.block",
    )?;
    for row in rows.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))? {
        let (block, amount, allocated): (String, i64, i64) = row?;
        if allocated != amount {
            mismatches.push(format!(
                "block {block}'s allocations sum to {allocated} sats, not the {amount} it allocated"
            ));
        }
    }

    let mut rows = t.prepare(
        "SELECT a.block, a.address, a.sats, b.status, \
         COALESCE(SUM(CASE l.kind WHEN 'pending' THEN l.sats WHEN 'reversal' THEN -l.sats ELSE 0 END), 0), \
         COALESCE(SUM(l.sats != a.sats OR l.pool != a.pool), 0) \
         FROM allocations a JOIN blocks b ON b.hash = a.block \
         LEFT JOIN ledger l ON l.block = a.block AND l.address = a.address \
         WHERE a.block NOT IN (SELECT block FROM pauses WHERE block IS NOT NULL) \
         GROUP BY a.block, a.address ORDER BY a.block, a.address",
    )?;
    let allocations = rows.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })?;
    for row in allocations {
        let (block, address, sats, status, credited, odd): (String, String, i64, String, i64, i64) =
            row?;
        let expected = if matches!(status.as_str(), "rejected" | "orphaned") {
            0
        } else {
            sats
        };
        if credited != expected {
            mismatches.push(format!(
                "the ledger credits {address} {credited} sats for block {block}, which is {status}, where \
                 its allocation calls for {expected}"
            ));
        }
        if odd > 0 {
            mismatches.push(format!(
                "the ledger has entries for {address}'s allocation in block {block} with another amount \
                 or pool"
            ));
        }
    }

    let mut rows = t.prepare(
        "SELECT address, pool FROM balances WHERE pending < 0 OR available < 0 OR reserved < 0 \
         ORDER BY address, pool",
    )?;
    for row in rows.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))? {
        let (address, pool): (String, String) = row?;
        mismatches.push(format!("{address}'s {pool} balance is below zero"));
    }
    let overdrawn: bool = t.query_row(
        "SELECT COALESCE(SUM(CASE kind WHEN 'credit' THEN sats WHEN 'reserved' THEN -sats ELSE 0 END), 0) \
         < 0 FROM ledger WHERE address IS NULL",
        [],
        |row| row.get(0),
    )?;
    if overdrawn {
        mismatches.push("the fee reserve reserved more than its credits".into());
    }

    let mut rows = t.prepare(
        "SELECT b.id, b.state, b.fee, \
         COALESCE(SUM(CASE WHEN l.kind = 'reserved' AND l.address IS NOT NULL THEN l.sats END), 0), \
         COALESCE(SUM(CASE WHEN l.kind = 'reserved' AND l.address IS NULL THEN l.sats END), 0), \
         COALESCE(SUM(CASE WHEN l.kind = 'paid' THEN l.sats END), 0) \
         FROM batches b LEFT JOIN ledger l ON l.batch = b.id GROUP BY b.id ORDER BY b.id",
    )?;
    let batches = rows.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })?;
    for row in batches {
        let (id, state, fee, payees, fee_reserved, paid): (i64, String, i64, i64, i64, i64) = row?;
        if payees == 0 || fee_reserved != fee {
            mismatches.push(format!(
                "batch {id} reserved {payees} sats for its payees and {fee_reserved} for its fee of {fee}"
            ));
        }
        let reserved = payees + fee_reserved;
        if paid != if state == "confirmed" { reserved } else { 0 } {
            mismatches.push(format!(
                "batch {id} is {state}, but the ledger paid {paid} of the {reserved} sats it reserved"
            ));
        }
    }
    let mut rows = t.prepare(
        "SELECT DISTINCT batch FROM ledger WHERE batch IS NOT NULL AND batch NOT IN (SELECT id FROM batches) \
         ORDER BY batch",
    )?;
    for batch in rows.query_map([], |row| row.get::<_, i64>(0))? {
        mismatches.push(format!(
            "the ledger has entries for batch {}, which does not exist",
            batch?
        ));
    }
    Ok(mismatches)
}

/// Reads what the books expect of the wallet.
pub fn books(t: &Transaction) -> rusqlite::Result<Books> {
    let batches = t
        .prepare(
            "SELECT b.id, b.state, b.funded, b.txid, \
             (SELECT COALESCE(SUM(sats), 0) FROM ledger WHERE kind = 'reserved' AND batch = b.id) \
             FROM batches b ORDER BY b.id",
        )?
        .query_map([], |row| {
            let txid = match row.get::<_, Option<String>>(3)? {
                Some(txid) => txid.parse().map_err(|e| invalid(3, e))?,
                None => payouts::decode(2, &row.get::<_, String>(2)?)?.compute_txid(),
            };
            Ok(Batch {
                id: row.get(0)?,
                state: row.get(1)?,
                txid,
                reserved: Amount::from_sat(row.get::<_, i64>(4)? as u64),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let inputs = t
        .prepare(
            "SELECT i.txid, i.vout FROM batch_inputs i JOIN batches b ON b.id = i.batch \
             WHERE b.state != 'confirmed'",
        )?
        .query_map([], |row| {
            let txid: String = row.get(0)?;
            Ok(OutPoint::new(
                txid.parse().map_err(|e| invalid(0, e))?,
                row.get(1)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let blocks = t
        .prepare("SELECT hash FROM blocks")?
        .query_map([], |row| {
            let hash: String = row.get(0)?;
            hash.parse().map_err(|e| invalid(0, e))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Books {
        batches,
        inputs,
        blocks,
        owed: ledger::owed(t)?,
        reserve: ledger::reserve(t)?,
    })
}

fn invalid(column: usize, e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(e))
}

/// Holds `books` against what the wallet reports: each locked input belongs to a batch not yet confirmed;
/// each batch's transaction stands in the wallet and on the chain where its state says; every payout
/// transaction and coinbase the wallet holds is one the books know; and the wallet holds at least what miners
/// are owed and the fee reserve, less what batches already sent. Returns each mismatch found.
pub fn compare(books: &Books, wallet: &Holdings) -> Vec<String> {
    let mut mismatches = Vec::new();
    for outpoint in wallet
        .locked
        .iter()
        .filter(|outpoint| !books.inputs.contains(outpoint))
    {
        mismatches.push(format!(
            "the wallet has input {outpoint} locked, which no unconfirmed batch holds"
        ));
    }

    let depths = depths(wallet);
    for batch in &books.batches {
        let depth = depths.get(&batch.txid).copied();
        let problem = match (batch.state.as_str(), depth) {
            ("reserved", Some(_)) => {
                Some("the wallet holds its transaction, which was never signed")
            }
            ("signed" | "broadcast", Some(depth)) if depth < 0 => {
                Some("a transaction conflicting with its own confirmed")
            }
            ("broadcast", None) => Some("the wallet does not hold its transaction"),
            ("confirmed", depth) if depth.is_none_or(|depth| depth < 1) => {
                Some("its transaction is not on the chain")
            }
            _ => None,
        };
        if let Some(problem) = problem {
            mismatches.push(format!(
                "batch {} is {}, but {problem}",
                batch.id, batch.state
            ));
        }
    }

    let txids: HashSet<Txid> = books.batches.iter().map(|batch| batch.txid).collect();
    let mut reported = HashSet::new();
    for entry in &wallet.transactions {
        let unknown = match (entry.category.as_str(), entry.blockhash) {
            ("send", _) if !txids.contains(&entry.txid) => {
                format!(
                    "the wallet sent transaction {}, which no batch made",
                    entry.txid
                )
            }
            ("generate" | "immature", Some(hash)) if !books.blocks.contains(&hash) => {
                format!(
                    "the wallet holds the coinbase of block {hash}, which the pool did not record"
                )
            }
            _ => continue,
        };
        if reported.insert(entry.txid) {
            mismatches.push(unknown);
        }
    }

    if wallet.held < due(books, wallet) {
        mismatches.push(format!(
            "the wallet holds {} sats, short of the {} owed to miners and the {} in the fee reserve, less \
             the {} batches already sent",
            wallet.held.to_sat(),
            books.owed.to_sat(),
            books.reserve.to_sat(),
            sent(books, wallet).to_sat()
        ));
    }
    mismatches
}

/// What the wallet should hold: what miners are owed and the fee reserve, less what batches already sent.
pub fn due(books: &Books, wallet: &Holdings) -> Amount {
    (books.owed + books.reserve)
        .checked_sub(sent(books, wallet))
        .unwrap_or(Amount::ZERO)
}

/// What the batches whose transaction the wallet sent reserved, their payees' balances and fees, which have
/// left the wallet though the ledger has not yet paid them.
fn sent(books: &Books, wallet: &Holdings) -> Amount {
    let depths = depths(wallet);
    books
        .batches
        .iter()
        .filter(|batch| {
            matches!(batch.state.as_str(), "signed" | "broadcast")
                && depths.get(&batch.txid).is_some_and(|&depth| depth >= 0)
        })
        .map(|batch| batch.reserved)
        .sum()
}

fn depths(wallet: &Holdings) -> HashMap<Txid, i64> {
    wallet
        .transactions
        .iter()
        .map(|entry| (entry.txid, entry.confirmations))
        .collect()
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash;
    use rusqlite::params;

    use super::*;
    use crate::{
        mining::pool::tests::id,
        pool::{
            blocks::{self, FoundBlock},
            ledger::Stage,
        },
        store::{DATABASE, Store},
        stratum::Transport,
    };

    const A: &str = "bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s";
    const B: &str = "bcrt1q3jeqwzg70pfkc9k4pvynlmfjlrrghp0cd705hx";

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!(
            "puntohunter-reconcile-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join(DATABASE)).unwrap()
    }

    fn hash(n: u8) -> BlockHash {
        BlockHash::from_byte_array([n; 32])
    }

    /// Records block `n` as found, with a snapshot of `amount` and `allocations`, settled to `stage`.
    fn allocated(
        t: &Transaction,
        n: u8,
        amount: i64,
        allocations: &[(&str, i64)],
        stage: Stage,
    ) -> rusqlite::Result<()> {
        let block = FoundBlock {
            hash: hash(n),
            height: 100,
            pool: id("pooled"),
            worker: "rig".into(),
            transport: Transport::V1,
            handle: "aaaaaaaaaaaaaaaa".parse().unwrap(),
            finder_sats: Amount::ZERO,
            pool_sats: Amount::from_sat(amount as u64),
            hex: String::new(),
        };
        blocks::insert(t, &block, 0)?;
        t.execute(
            "INSERT INTO snapshots (block, pool, cutoff, first, boundary, size, total, amount) \
             VALUES (?1, 'pooled', 1, 1, '1', '1', '1', ?2)",
            params![hash(n).to_string(), amount],
        )?;
        for (address, sats) in allocations {
            t.execute(
                "INSERT INTO allocations (block, pool, address, sats) VALUES (?1, 'pooled', ?2, ?3)",
                params![hash(n).to_string(), address, sats],
            )?;
        }
        ledger::settle(t, hash(n), stage).map(drop)
    }

    #[tokio::test]
    async fn the_books_agree_with_themselves_until_an_entry_or_a_state_disagrees() {
        let store = store("audit");
        let clean = store
            .commit(|t| {
                allocated(t, 1, 1_000, &[(A, 600), (B, 400)], Stage::Available)?;
                t.execute("INSERT INTO ledger (kind, sats) VALUES ('credit', 500)", [])?;
                t.execute(
                    "INSERT INTO batches (state, funded, fee, fee_rate, tx, txid) \
                     VALUES ('broadcast', '00', 100, 1.0, '00', 'txid')",
                    [],
                )?;
                t.execute(
                    "INSERT INTO ledger (kind, address, pool, sats, batch) \
                     VALUES ('reserved', ?1, 'pooled', 600, 1)",
                    [A],
                )?;
                t.execute(
                    "INSERT INTO ledger (kind, sats, batch) VALUES ('reserved', 100, 1)",
                    [],
                )?;
                audit(t)
            })
            .await
            .unwrap();
        assert_eq!(clean, Vec::<String>::new());

        let confirmed = store
            .commit(|t| {
                t.execute("UPDATE batches SET state = 'confirmed'", [])?;
                let found = audit(t);
                t.execute("UPDATE batches SET state = 'broadcast'", [])?;
                found
            })
            .await
            .unwrap();
        assert_eq!(
            confirmed,
            ["batch 1 is confirmed, but the ledger paid 0 of the 700 sats it reserved"]
        );

        let short = store
            .commit(|t| {
                allocated(t, 2, 1_000, &[(A, 900)], Stage::Pending)?;
                audit(t)
            })
            .await
            .unwrap();
        let block2 = hash(2);
        assert_eq!(
            short,
            [format!(
                "block {block2}'s allocations sum to 900 sats, not the 1000 it allocated"
            )]
        );

        // Orphaned without its reversals: each allocation is still credited.
        let orphaned = store
            .commit(move |t| {
                t.execute(
                    "UPDATE blocks SET status = 'orphaned' WHERE hash = ?1",
                    [block2.to_string()],
                )?;
                audit(t)
            })
            .await
            .unwrap();
        assert_eq!(orphaned.len(), 2, "{orphaned:?}");
        assert_eq!(
            orphaned[1],
            format!(
                "the ledger credits {A} 900 sats for block {block2}, which is orphaned, where its allocation \
                 calls for 0"
            )
        );
    }

    fn txid(n: u8) -> Txid {
        Txid::from_byte_array([n; 32])
    }

    fn entry(
        txid: Txid,
        category: &str,
        blockhash: Option<BlockHash>,
        confirmations: i64,
    ) -> Entry {
        Entry {
            txid,
            category: category.into(),
            blockhash,
            confirmations,
        }
    }

    /// Books with a confirmed batch, one broadcast and reserving 700 sats, and one reserved, owing miners
    /// 1,000 sats beside a fee reserve of 500; and a wallet that agrees, the broadcast batch's 700 sats gone.
    fn agreeing() -> (Books, Holdings) {
        let batch = |id, state: &str, n, reserved| Batch {
            id,
            state: state.into(),
            txid: txid(n),
            reserved: Amount::from_sat(reserved),
        };
        let books = Books {
            batches: vec![
                batch(1, "confirmed", 1, 300),
                batch(2, "broadcast", 2, 700),
                batch(3, "reserved", 3, 200),
            ],
            inputs: [OutPoint::new(txid(9), 0), OutPoint::new(txid(9), 1)].into(),
            blocks: [hash(1)].into(),
            owed: Amount::from_sat(1_000),
            reserve: Amount::from_sat(500),
        };
        let wallet = Holdings {
            // The broadcast batch's input is spent, and so no longer locked.
            locked: vec![OutPoint::new(txid(9), 1)],
            transactions: vec![
                entry(txid(1), "send", Some(hash(5)), 6),
                entry(txid(2), "send", None, 0),
                entry(txid(2), "receive", None, 0),
                entry(txid(4), "immature", Some(hash(1)), 3),
                // The operator's funding, and a coinbase no longer on the chain.
                entry(txid(5), "receive", Some(hash(6)), 9),
                entry(txid(6), "orphan", None, 0),
            ],
            held: Amount::from_sat(800),
        };
        (books, wallet)
    }

    #[test]
    fn the_wallet_agrees_with_the_books_until_it_holds_what_they_do_not_know() {
        let (books, wallet) = agreeing();
        assert_eq!(compare(&books, &wallet), Vec::<String>::new());

        let (books, mut wallet) = agreeing();
        wallet.locked.push(OutPoint::new(txid(8), 0));
        wallet
            .transactions
            .push(entry(txid(7), "send", Some(hash(5)), 2));
        wallet
            .transactions
            .push(entry(txid(7), "send", Some(hash(5)), 2));
        wallet
            .transactions
            .push(entry(txid(8), "generate", Some(hash(7)), 200));
        wallet.held = Amount::from_sat(799);
        assert_eq!(
            compare(&books, &wallet),
            [
                format!("the wallet has input {}:0 locked, which no unconfirmed batch holds", txid(8)),
                format!("the wallet sent transaction {}, which no batch made", txid(7)),
                format!(
                    "the wallet holds the coinbase of block {}, which the pool did not record",
                    hash(7)
                ),
                "the wallet holds 799 sats, short of the 1000 owed to miners and the 500 in the fee reserve, \
                 less the 700 batches already sent"
                    .into(),
            ]
        );
    }

    #[test]
    fn each_batch_stands_where_its_state_says() {
        let (books, mut wallet) = agreeing();
        // The broadcast batch's transaction is gone, so its 700 sats count as held; the reserved batch's was
        // sent; the confirmed batch's is back in the mempool.
        wallet.transactions.retain(|entry| entry.txid != txid(2));
        wallet.transactions.push(entry(txid(3), "send", None, 0));
        wallet.transactions[0].confirmations = 0;
        wallet.held = Amount::from_sat(1_500);
        assert_eq!(
            compare(&books, &wallet),
            [
                "batch 1 is confirmed, but its transaction is not on the chain",
                "batch 2 is broadcast, but the wallet does not hold its transaction",
                "batch 3 is reserved, but the wallet holds its transaction, which was never signed",
            ]
        );

        let (books, mut wallet) = agreeing();
        wallet.transactions[1].confirmations = -1;
        wallet.transactions[2].confirmations = -1;
        wallet.held = Amount::from_sat(1_500);
        assert_eq!(
            compare(&books, &wallet),
            ["batch 2 is broadcast, but a transaction conflicting with its own confirmed"]
        );
    }
}
