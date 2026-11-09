//! The SV2 authority keypair: a secret in the data directory, generated on first
//! run with 0600 permissions. Each connection's Noise certificate is signed with it, and miners pin its public
//! key, printed at startup in SRI's base58check form.

use std::{path::Path, time::Duration};

use bitcoin::{
    hex::{DisplayHex, FromHex},
    secp256k1::{Keypair, Secp256k1, SecretKey, rand},
};
use noise_sv2::Responder;

use crate::secret::{self, SecretError};

/// The key's file name in the data directory.
pub const FILE: &str = "sv2-authority.key";
/// The next key's, made by `puntohunter authority next` ahead of a rotation and moved over [`FILE`] at the
/// switch. The pool signs nothing with it; the overview shows its public key.
pub const NEXT_FILE: &str = "sv2-authority.next.key";
/// How long each connection's certificate is valid. A miner checks it once, during the handshake.
const CERTIFICATE_VALIDITY: Duration = Duration::from_secs(3600);
/// The little-endian u16 version SRI prefixes to a base58check public key.
const KEY_VERSION: [u8; 2] = [1, 0];

pub struct Authority {
    keypair: Keypair,
}

impl Authority {
    /// The key at `path`, a hex secp256k1 secret key, generated there with 0600 permissions if the file does
    /// not exist.
    pub fn load_or_generate(path: &Path) -> Result<Self, SecretError> {
        let secp = Secp256k1::new();
        let keypair = secret::load_or_generate(
            path,
            "SV2 authority key",
            |text| {
                let bytes = <[u8; 32]>::from_hex(text).ok()?;
                let secret = SecretKey::from_slice(&bytes).ok()?;
                Some(Keypair::from_secret_key(&secp, &secret))
            },
            || {
                let keypair = Keypair::new(&secp, &mut rand::thread_rng());
                let text = keypair.secret_bytes().to_lower_hex_string();
                (keypair, text)
            },
        )?;
        Ok(Authority { keypair })
    }

    /// The public key miners pin, in SRI's base58check form: puntominer's `stratum2+tcp://host:port/<key>`
    /// and AxeOS's authority-key field both take it.
    pub fn public_key(&self) -> String {
        encode(&self.keypair.x_only_public_key().0.serialize())
    }

    /// A Noise NX responder for one connection, with a fresh static key certified by this authority.
    pub fn responder(&self) -> Box<Responder> {
        Responder::from_authority_kp(
            &self.keypair.x_only_public_key().0.serialize(),
            &self.keypair.secret_bytes(),
            CERTIFICATE_VALIDITY,
        )
        .expect("the public key is the secret key's own")
    }
}

/// An x-only public key in SRI's base58check form: the version, then the key.
fn encode(xonly: &[u8; 32]) -> String {
    bitcoin::base58::encode_check(&[&KEY_VERSION[..], xonly].concat())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use super::*;

    fn fresh(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-authority-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn generates_once_with_0600_and_loads_the_same_key() {
        let path = fresh("authority-generate.key");
        let first = Authority::load_or_generate(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let again = Authority::load_or_generate(&path).unwrap();
        assert_eq!(first.public_key(), again.public_key());
    }

    #[test]
    fn prints_the_key_as_sri_base58check() {
        let path = fresh("authority-b58.key");
        let authority = Authority::load_or_generate(&path).unwrap();
        let decoded = bitcoin::base58::decode_check(&authority.public_key()).unwrap();
        assert_eq!(decoded[..2], KEY_VERSION);
        assert_eq!(
            decoded[2..],
            authority.keypair.x_only_public_key().0.serialize()
        );
    }

    #[test]
    fn encodes_the_spec_vector() {
        // SV2 spec 4.7.1's example authority key, as raw x-only bytes and in base58check.
        let raw = [
            118, 99, 112, 0, 151, 156, 28, 17, 175, 12, 48, 11, 205, 140, 127, 228, 134, 16, 252,
            233, 185, 193, 30, 61, 174, 227, 90, 224, 176, 138, 116, 85,
        ];
        assert_eq!(
            encode(&raw),
            "9bXiEd8boQVhq7WddEcERUL5tyyJVFYdU8th3HfbNXK3Yw6GRXh"
        );
    }

    #[test]
    fn refuses_a_key_others_can_read_or_one_that_is_not_a_key() {
        let path = fresh("authority-exposed.key");
        Authority::load_or_generate(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Authority::load_or_generate(&path),
            Err(SecretError::Exposed { .. })
        ));

        let path = fresh("authority-invalid.key");
        fs::write(&path, "not a key\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            Authority::load_or_generate(&path),
            Err(SecretError::Invalid { .. })
        ));
    }
}
