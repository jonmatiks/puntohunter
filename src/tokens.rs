//! Balance tokens: each opens one address's private balance view.
//! The operator issues one with `puntohunter token issue`, which prints it once; only its SHA-256 is kept
//! (the schema's `tokens` table). A token is 32 random bytes in hex, so it cannot be guessed, and it is never
//! logged.

use std::time::SystemTime;

use bitcoin::{
    Address,
    hashes::{Hash, sha256},
    hex::DisplayHex,
    secp256k1::rand::{self, RngCore},
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::telemetry::unix;

/// Issues a token for `address`, records its hash, and returns it: the one time it is seen.
pub fn issue(connection: &Connection, address: &Address) -> rusqlite::Result<String> {
    let mut bytes = [0; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = bytes.to_lower_hex_string();
    connection.execute(
        "INSERT INTO tokens (hash, address, issued) VALUES (?1, ?2, ?3)",
        params![
            hash(&token),
            address.to_string(),
            unix(SystemTime::now()) as i64
        ],
    )?;
    Ok(token)
}

/// The address `token` opens, if it opens one.
pub fn address(connection: &Connection, token: &str) -> rusqlite::Result<Option<String>> {
    connection
        .query_row(
            "SELECT address FROM tokens WHERE hash = ?1",
            [hash(token)],
            |row| row.get(0),
        )
        .optional()
}

fn hash(token: &str) -> [u8; 32] {
    sha256::Hash::hash(token.as_bytes()).to_byte_array()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{DATABASE, Store};

    #[tokio::test]
    async fn a_token_opens_its_address_alone_and_only_its_hash_is_kept() {
        let dir = std::env::temp_dir().join(format!("puntohunter-tokens-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join(DATABASE)).unwrap();
        let owner: Address = "bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s"
            .parse::<Address<_>>()
            .unwrap()
            .assume_checked();
        let (first, second, opened, kept) = store
            .commit(move |t| {
                let first = issue(t, &owner)?;
                let second = issue(t, &owner)?;
                let opened = [
                    address(t, &first)?,
                    address(t, &second)?,
                    address(t, &first.to_uppercase())?,
                    address(t, "")?,
                ];
                let kept: Vec<Vec<u8>> = t
                    .prepare("SELECT hash FROM tokens ORDER BY rowid")?
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok((first, second, opened, kept))
            })
            .await
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        let opens = Some("bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s".to_owned());
        assert_eq!(opened, [opens.clone(), opens, None, None]);
        assert_eq!(kept, [hash(&first).to_vec(), hash(&second).to_vec()]);
    }
}
