//! The country database: a peer address to its country, from an IP-to-country file
//! in the MaxMind DB format, such as DB-IP's IP to Country Lite or MaxMind's GeoLite2 Country. Only the
//! country is kept, never the address. The file is reopened when it changes, checked hourly, so a monthly
//! update needs no restart; a reopen that fails keeps the last good copy.

use std::{
    collections::BTreeMap,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime},
};

use maxminddb::Reader;
use serde::{Deserialize, Serialize};

/// How often the file is checked for a change.
const CHECK: Duration = Duration::from_secs(3600);

/// A country, by its ISO 3166-1 code and its English name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Country {
    pub code: String,
    pub name: String,
}

/// Who made the database, for the credit its licence asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    DbIp,
    MaxMind,
    Other,
}

#[derive(Debug, thiserror::Error)]
#[error("cannot open the country database {}: {reason}", path.display())]
pub struct GeoError {
    path: PathBuf,
    reason: String,
}

/// The open database, shared by every listener.
pub struct Geo {
    path: PathBuf,
    current: RwLock<Arc<Database>>,
    watch: Mutex<Watch>,
}

struct Database {
    reader: Reader<Vec<u8>>,
    source: Source,
}

/// What a lookup reads of a record: the country, as both databases lay it out.
#[derive(Deserialize)]
struct Record<'a> {
    #[serde(borrow)]
    country: Option<Named<'a>>,
}

#[derive(Deserialize)]
struct Named<'a> {
    iso_code: Option<&'a str>,
    #[serde(borrow)]
    names: Option<BTreeMap<&'a str, &'a str>>,
}

fn open(path: &Path) -> Result<Database, String> {
    let reader = Reader::open_readfile(path).map_err(|e| e.to_string())?;
    let kind = reader.metadata().database_type.to_ascii_lowercase();
    let source = if kind.contains("dbip") || kind.contains("db-ip") {
        Source::DbIp
    } else if kind.contains("geolite") || kind.contains("geoip") {
        Source::MaxMind
    } else {
        Source::Other
    };
    Ok(Database { reader, source })
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Geo {
    /// Opens the database at `path`; one that cannot be read stops startup.
    pub fn open(path: &Path) -> Result<Arc<Geo>, GeoError> {
        let seen = modified(path);
        let database = open(path).map_err(|reason| GeoError {
            path: path.to_owned(),
            reason,
        })?;
        Ok(Arc::new(Geo {
            path: path.to_owned(),
            current: RwLock::new(Arc::new(database)),
            watch: Mutex::new(Watch { seen: Some(seen) }),
        }))
    }

    /// The country of `ip`, if the database gives one.
    pub fn country(&self, ip: IpAddr) -> Option<Country> {
        let database = self.current.read().unwrap().clone();
        let found = database.reader.lookup(ip.to_canonical()).ok()?;
        let record: Record = found.decode().ok()??;
        let country = record.country?;
        let code = country.iso_code?;
        let name = country
            .names
            .as_ref()
            .and_then(|names| names.get("en").copied())
            .unwrap_or(code);
        Some(Country {
            code: code.to_owned(),
            name: name.to_owned(),
        })
    }

    pub fn source(&self) -> Source {
        self.current.read().unwrap().source
    }

    /// Checks the file hourly from now on, reopening it when it changes.
    pub fn spawn(self: &Arc<Self>) {
        let geo = self.clone();
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(CHECK);
            ticks.reset();
            loop {
                ticks.tick().await;
                let geo = geo.clone();
                // Reading a database of a few megabytes blocks.
                let _ = tokio::task::spawn_blocking(move || geo.check()).await;
            }
        });
    }

    /// Reopens the file if it changed since it was last looked at.
    fn check(&self) {
        let state = modified(&self.path);
        let outcome = self.watch.lock().unwrap().check(state, || open(&self.path));
        match outcome {
            Outcome::Unchanged => {}
            Outcome::Reopened(database) => {
                *self.current.write().unwrap() = Arc::new(database);
                tracing::info!(path = %self.path.display(), "country database reopened");
            }
            Outcome::Failed(reason) => tracing::error!(
                path = %self.path.display(),
                %reason,
                "cannot reopen the country database; keeping the last good copy"
            ),
        }
    }
}

/// The reopen rule, apart from the file format: a file is opened again when its modification time differs
/// from the last one looked at, and each state that fails to open is reported once.
struct Watch {
    /// The modification time last looked at, None for a file that could not be read; None before any.
    seen: Option<Option<SystemTime>>,
}

enum Outcome<T> {
    Unchanged,
    Reopened(T),
    Failed(String),
}

impl Watch {
    fn check<T>(
        &mut self,
        state: Option<SystemTime>,
        open: impl FnOnce() -> Result<T, String>,
    ) -> Outcome<T> {
        if self.seen == Some(state) {
            return Outcome::Unchanged;
        }
        self.seen = Some(state);
        if state.is_none() {
            return Outcome::Failed("the file cannot be read".into());
        }
        match open() {
            Ok(database) => Outcome::Reopened(database),
            Err(reason) => Outcome::Failed(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn reopens_a_changed_file_and_reports_each_failure_once() {
        let mut watch = Watch { seen: Some(at(1)) };
        let mut opened = 0;
        let mut open = |result: Result<u32, &str>| {
            opened += 1;
            result.map_err(str::to_owned)
        };
        assert!(matches!(
            watch.check(at(1), || open(Ok(1))),
            Outcome::Unchanged
        ));
        assert!(matches!(
            watch.check(at(2), || open(Ok(2))),
            Outcome::Reopened(2)
        ));
        assert!(matches!(
            watch.check(at(2), || open(Ok(3))),
            Outcome::Unchanged
        ));
        // A broken update is reported once, and the last good copy stays.
        assert!(matches!(
            watch.check(at(3), || open(Err("truncated"))),
            Outcome::Failed(reason) if reason == "truncated"
        ));
        assert!(matches!(
            watch.check(at(3), || open(Err("truncated"))),
            Outcome::Unchanged
        ));
        // So is a file that goes missing.
        assert!(matches!(
            watch.check(None, || open(Ok(4))),
            Outcome::Failed(_)
        ));
        assert!(matches!(
            watch.check(None, || open(Ok(4))),
            Outcome::Unchanged
        ));
        assert!(matches!(
            watch.check(at(4), || open(Ok(5))),
            Outcome::Reopened(5)
        ));
        assert_eq!(opened, 3, "an unchanged file is never opened");
    }

    #[test]
    fn refuses_a_file_that_is_not_a_database() {
        let path =
            std::env::temp_dir().join(format!("puntohunter-geo-{}.mmdb", std::process::id()));
        std::fs::write(&path, b"not a database").unwrap();
        assert!(Geo::open(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(Geo::open(&path).is_err(), "a missing file");
    }
}
