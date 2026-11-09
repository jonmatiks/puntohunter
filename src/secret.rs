//! Secrets kept as files in the data directory: the SV2 authority key and the handle
//! secret. Each is generated on first run with 0600 permissions, and refused at startup when others can read
//! it or it does not parse.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("cannot read the {what} {}: {source}", path.display())]
    Read {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("cannot write the {what} {}: {source}", path.display())]
    Write {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("the {what} {} does not parse", path.display())]
    Invalid { what: &'static str, path: PathBuf },
    #[error("the {what} {} is readable by others: chmod 600 it", path.display())]
    Exposed { what: &'static str, path: PathBuf },
}

/// The secret `what` at `path`, read with `parse`. If the file does not exist, `generate` makes the secret
/// and the text to write, which is written there with 0600 permissions.
pub fn load_or_generate<T>(
    path: &Path,
    what: &'static str,
    parse: impl FnOnce(&str) -> Option<T>,
    generate: impl FnOnce() -> (T, String),
) -> Result<T, SecretError> {
    let read = |source| SecretError::Read {
        what,
        path: path.to_owned(),
        source,
    };
    match fs::read_to_string(path) {
        Ok(text) => {
            let mode = fs::metadata(path).map_err(read)?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(SecretError::Exposed {
                    what,
                    path: path.to_owned(),
                });
            }
            parse(text.trim()).ok_or_else(|| SecretError::Invalid {
                what,
                path: path.to_owned(),
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let (secret, text) = generate();
            let write = |source| SecretError::Write {
                what,
                path: path.to_owned(),
                source,
            };
            // create_new: never overwrite a secret that appeared since the read.
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(write)?;
            writeln!(file, "{text}").map_err(write)?;
            file.sync_all().map_err(write)?;
            tracing::info!(path = %path.display(), "{what} generated");
            Ok(secret)
        }
        Err(source) => Err(read(source)),
    }
}
