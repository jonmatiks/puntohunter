//! The opaque handles: public URLs and responses key on them, never on the address. A worker
//! handle is a truncated HMAC-SHA256 of the pool's id and canonical Stratum username, and a fleet handle one
//! of the canonical address, both under one secret in the data directory, so they are stable across restarts
//! and cannot be reversed or derived without the secret. A device with a fleet secret has a private fleet's
//! handles: the same HMACs with the secret taken in, each under a tag of its own.

use std::{fmt, path::Path, str::FromStr};

use bitcoin::{
    Address,
    hashes::{Hash, HashEngine, hmac, sha256},
    hex::{DisplayHex, FromHex},
    secp256k1::rand::{self, RngCore},
};

use crate::{
    mining::pool::PoolId,
    secret::{self, SecretError},
    stratum::username::{Secret, Username},
};

/// The secret's file name in the data directory.
pub const FILE: &str = "handle.key";
/// The HMAC bytes a handle keeps: 80 bits, 16 base32 characters.
const BYTES: usize = 10;
/// The encoded length: 5 bits a character.
const LEN: usize = BYTES * 8 / 5;
/// RFC 4648's base32 alphabet, lower-cased for URLs.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// A worker's or a fleet's handle: 16 lower-case base32 characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Handle([u8; BYTES]);

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bits = self.0.iter().fold(0u128, |n, &b| n << 8 | u128::from(b));
        for i in (0..LEN).rev() {
            let digit = (bits >> (5 * i)) & 0x1f;
            write!(f, "{}", ALPHABET[digit as usize] as char)?;
        }
        Ok(())
    }
}

impl FromStr for Handle {
    type Err = ();

    /// Parses a handle as `Display` writes it, and nothing else.
    fn from_str(text: &str) -> Result<Self, ()> {
        if text.len() != LEN {
            return Err(());
        }
        let mut bits = 0u128;
        for c in text.bytes() {
            let digit = ALPHABET.iter().position(|&a| a == c).ok_or(())?;
            bits = bits << 5 | digit as u128;
        }
        Ok(Handle(bits.to_be_bytes()[16 - BYTES..].try_into().unwrap()))
    }
}

/// The handle secret.
#[derive(Clone)]
pub struct HandleKey([u8; 32]);

impl HandleKey {
    /// The secret at `path`, 32 hex-encoded bytes, generated there with 0600 permissions if the file does not
    /// exist.
    pub fn load_or_generate(path: &Path) -> Result<Self, SecretError> {
        secret::load_or_generate(
            path,
            "handle secret",
            |text| <[u8; 32]>::from_hex(text).ok().map(HandleKey),
            || {
                let mut bytes = [0; 32];
                rand::thread_rng().fill_bytes(&mut bytes);
                (HandleKey(bytes), bytes.to_lower_hex_string())
            },
        )
    }

    /// A key of `byte` repeated, for tests.
    #[cfg(test)]
    pub fn fixed(byte: u8) -> Self {
        HandleKey([byte; 32])
    }

    /// The handle of `username` mining in `pool`. With a secret, its tag is `<pool>#private` and its text
    /// the username with `#<secret>` after it: no pool id holds `#`, nor does a canonical username, so a
    /// device without the secret never shares the handle.
    pub fn handle(&self, pool: &PoolId, username: &Username) -> Handle {
        match &username.secret {
            None => self.mac(pool.as_str(), &canonical(username)),
            Some(secret) => self.mac(
                &format!("{pool}#private"),
                &format!("{}#{}", canonical(username), secret.expose()),
            ),
        }
    }

    /// The handle of the fleet mining to `address`: every worker on it, in any pool, that has the same
    /// `secret`, or none for the address's open fleet. Its input starts with `fleet:`, or `fleet#private:`
    /// with a secret, and a worker handle's with its pool's id, which is never `fleet` and holds no `#`, so
    /// no two share an input.
    pub fn fleet(&self, address: &Address, secret: Option<&Secret>) -> Handle {
        match secret {
            None => self.mac("fleet", &address.to_string()),
            Some(secret) => self.mac("fleet#private", &format!("{address}#{}", secret.expose())),
        }
    }

    /// The truncated HMAC of `tag:text`.
    fn mac(&self, tag: &str, text: &str) -> Handle {
        let mut engine = hmac::HmacEngine::<sha256::Hash>::new(&self.0);
        engine.input(tag.as_bytes());
        engine.input(b":");
        engine.input(text.as_bytes());
        let mac = hmac::Hmac::<sha256::Hash>::from_engine(engine).to_byte_array();
        Handle(mac[..BYTES].try_into().unwrap())
    }
}

/// The username re-encoded from its parts: the address in its canonical form, so a bech32 address typed in
/// upper case names the same worker.
fn canonical(username: &Username) -> String {
    match username.worker.as_str() {
        "" => username.address.to_string(),
        worker => format!("{}.{worker}", username.address),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use super::*;
    use crate::{
        config::{Config, Network},
        mining::pool::tests::id,
        stratum::username,
    };

    /// A username on the regtest config's pool wallet, standing in for a miner's.
    fn username(worker: &str) -> Username {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/regtest.toml");
        let config: Config = fs::read_to_string(path).unwrap().parse().unwrap();
        let text = format!("{}.{worker}", config.addresses.pool_wallet);
        username::parse(&text, Network::Regtest).unwrap()
    }

    fn fresh(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("puntohunter-handle-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn is_stable_for_one_secret_and_differs_by_pool_worker_and_secret() {
        let rig = username("rig");
        let handle = HandleKey::fixed(1).handle(&id("solo"), &rig);
        assert_eq!(handle, HandleKey::fixed(1).handle(&id("solo"), &rig));
        assert_ne!(handle, HandleKey::fixed(1).handle(&id("pooled"), &rig));
        assert_ne!(
            handle,
            HandleKey::fixed(1).handle(&id("solo"), &username("other"))
        );
        assert_ne!(handle, HandleKey::fixed(2).handle(&id("solo"), &rig));
    }

    #[test]
    fn a_solo_handle_stays_what_it_was_recorded() {
        // Recorded on 2026-10-15, before pools replaced modes: a default pool's id is its mode's name, so
        // every handle a miner already has stays the same.
        let rig = username::parse(
            "bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s.rig",
            Network::Regtest,
        )
        .unwrap();
        assert_eq!(
            HandleKey::fixed(7).handle(&id("solo"), &rig).to_string(),
            "nmy6ibibjreoxu5o"
        );
    }

    #[test]
    fn leaves_a_workers_options_out() {
        // A worker that changes its part or asks for its name keeps its handle.
        let rig = username("rig");
        for suffix in ["+50", "+name", "+0.5btc+name"] {
            let chose = username(&format!("rig{suffix}"));
            assert_ne!(chose.options, rig.options, "{suffix}");
            assert_eq!(
                HandleKey::fixed(1).handle(&id("solo"), &chose),
                HandleKey::fixed(1).handle(&id("solo"), &rig),
                "{suffix}"
            );
        }
        let bare = username::parse(&format!("{}+50", rig.address), Network::Regtest).unwrap();
        let plain = username::parse(&rig.address.to_string(), Network::Regtest).unwrap();
        assert_eq!(
            HandleKey::fixed(1).handle(&id("solo"), &bare),
            HandleKey::fixed(1).handle(&id("solo"), &plain)
        );
    }

    #[test]
    fn matches_for_upper_and_lower_case_bech32() {
        let lower = username("rig");
        let typed = format!("{}.rig", lower.address.to_string().to_uppercase());
        let upper = username::parse(&typed, Network::Regtest).unwrap();
        assert_eq!(
            HandleKey::fixed(1).handle(&id("solo"), &upper),
            HandleKey::fixed(1).handle(&id("solo"), &lower)
        );
    }

    #[test]
    fn a_fleet_handle_is_stable_case_blind_apart_from_worker_handles_and_by_secret() {
        let rig = username("rig");
        let fleet = HandleKey::fixed(1).fleet(&rig.address, None);
        assert_eq!(fleet, HandleKey::fixed(1).fleet(&rig.address, None));
        let typed = rig.address.to_string().to_uppercase();
        let upper = username::parse(&typed, Network::Regtest).unwrap();
        assert_eq!(HandleKey::fixed(1).fleet(&upper.address, None), fleet);
        for worker in ["", "rig", "other"] {
            for pool in [id("solo"), id("pooled")] {
                assert_ne!(HandleKey::fixed(1).handle(&pool, &username(worker)), fleet);
            }
        }
        assert_ne!(HandleKey::fixed(2).fleet(&rig.address, None), fleet);
    }

    #[test]
    fn an_open_fleets_handles_stay_what_they_were_recorded() {
        // Recorded on 2026-10-26, before private fleets: a device without a secret keeps its links.
        let rig = username::parse(
            "bcrt1q7f0pjwhc3jzzv0w4uurm589506glv2dggfan4s.rig",
            Network::Regtest,
        )
        .unwrap();
        assert_eq!(
            HandleKey::fixed(7).fleet(&rig.address, None).to_string(),
            "id56qilhjhxlqg6r"
        );
    }

    #[test]
    fn a_secret_makes_handles_of_its_own() {
        let key = HandleKey::fixed(1);
        let rig = username("rig");
        let private = username("rig#garage-pass");
        let other = username("rig#other-pass");
        let secret = private.secret.as_ref();
        for pool in [id("solo"), id("pooled")] {
            let open = key.handle(&pool, &rig);
            assert_ne!(key.handle(&pool, &private), open, "a protected worker");
            assert_ne!(key.handle(&pool, &private), key.handle(&pool, &other));
            assert_eq!(
                key.handle(&pool, &private),
                key.handle(&pool, &username("rig#garage-pass+50+name")),
                "options stay out"
            );
        }
        let open = key.fleet(&rig.address, None);
        let fleet = key.fleet(&rig.address, secret);
        assert_ne!(fleet, open, "a private fleet");
        assert_ne!(fleet, key.fleet(&rig.address, other.secret.as_ref()));
        assert_eq!(fleet, HandleKey::fixed(1).fleet(&rig.address, secret));
        assert_ne!(fleet, HandleKey::fixed(2).fleet(&rig.address, secret));
        for pool in [id("solo"), id("pooled")] {
            assert_ne!(key.handle(&pool, &private), fleet);
        }
    }

    #[test]
    fn encodes_16_base32_characters_and_parses_back() {
        let handle = HandleKey::fixed(1).handle(&id("solo"), &username("rig"));
        let text = handle.to_string();
        assert_eq!(text.len(), 16);
        assert!(text.bytes().all(|c| ALPHABET.contains(&c)), "{text}");
        assert_eq!(text.parse(), Ok(handle));
        assert_eq!(Handle([0; BYTES]).to_string(), "a".repeat(16));
        assert_eq!(Handle([0xff; BYTES]).to_string(), "7".repeat(16));
        for bad in ["", "abc", &"A".repeat(16), &"1".repeat(16), &"a".repeat(17)] {
            assert_eq!(bad.parse::<Handle>(), Err(()), "{bad}");
        }
    }

    #[test]
    fn generates_the_secret_once_with_0600_and_refuses_one_others_can_read() {
        let path = fresh("handle-generate.key");
        let first = HandleKey::load_or_generate(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let rig = username("rig");
        let again = HandleKey::load_or_generate(&path).unwrap();
        assert_eq!(
            first.handle(&id("solo"), &rig),
            again.handle(&id("solo"), &rig)
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            HandleKey::load_or_generate(&path),
            Err(SecretError::Exposed { .. })
        ));
        fs::write(&path, "not hex\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            HandleKey::load_or_generate(&path),
            Err(SecretError::Invalid { .. })
        ));
    }
}
