//! The database's snapshots: one every interval, and one at once
//! after a block with a pool output is allocated or a payout batch is broadcast, so the records that move money
//! wait for no interval. `VACUUM INTO` copies the database as one read transaction sees it, on a connection of
//! its own, so neither the writer nor the portal waits. A snapshot is written under a hidden temporary name
//! and renamed into place, so another host's pull never copies half of one, and the oldest beyond `keep` are
//! removed. A snapshot that fails is logged and counted, and mining and the journal carry on.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use rusqlite::{Connection, OpenFlags};
use tokio::sync::Notify;

use crate::{config::Backup, metrics};

/// A snapshot's name: `puntohunter-<Unix milliseconds>.sqlite3`, so names sort by when they were taken.
const PREFIX: &str = "puntohunter-";
const SUFFIX: &str = ".sqlite3";

/// A snapshot asked for out of turn. Without `[backup]` nothing takes it.
static NOW: Notify = Notify::const_new();

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Asks for a snapshot at once: a block with a pool output was allocated, or a payout batch broadcast.
pub fn now() {
    NOW.notify_one();
}

/// Starts the snapshots of `database` that `settings` asks for. It runs for the life of the process, the
/// first snapshot at once.
pub fn spawn(database: PathBuf, settings: Backup) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(settings.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                () = NOW.notified() => {}
            }
            let (database, dir, keep) = (database.clone(), settings.dir.clone(), settings.keep);
            let taken = tokio::task::spawn_blocking(move || {
                snapshot(&database, &dir, keep, SystemTime::now())
            })
            .await;
            match taken {
                Ok(Ok(path)) => {
                    tracing::debug!(snapshot = %path.display(), "database snapshot written");
                    metrics::backup_written(SystemTime::now());
                }
                Ok(Err(e)) => {
                    tracing::error!("cannot snapshot the database: {e}");
                    metrics::backup_failed();
                }
                Err(e) => {
                    tracing::error!("cannot snapshot the database: {e}");
                    metrics::backup_failed();
                }
            }
        }
    });
}

/// Writes a snapshot of `database` into `dir`, named for `at`, and removes the oldest beyond `keep`.
pub fn snapshot(
    database: &Path,
    dir: &Path,
    keep: usize,
    at: SystemTime,
) -> Result<PathBuf, BackupError> {
    fs::create_dir_all(dir)?;
    let millis = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let name = format!("{PREFIX}{millis}{SUFFIX}");
    let path = dir.join(&name);
    let temporary = dir.join(format!(".{name}.tmp"));
    // A file left by a snapshot that failed halfway; VACUUM INTO refuses to overwrite one.
    let _ = fs::remove_file(&temporary);
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.execute("VACUUM INTO ?1", [temporary.to_string_lossy()])?;
    drop(connection);
    fs::rename(&temporary, &path)?;
    prune(dir, keep)?;
    Ok(path)
}

/// Removes the oldest snapshots in `dir` beyond `keep`; any other file is left alone.
fn prune(dir: &Path, keep: usize) -> io::Result<()> {
    let mut snapshots: Vec<(u128, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let millis = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| {
                name.strip_prefix(PREFIX)?
                    .strip_suffix(SUFFIX)?
                    .parse()
                    .ok()
            });
        if let Some(millis) = millis {
            snapshots.push((millis, path));
        }
    }
    snapshots.sort();
    let excess = snapshots.len().saturating_sub(keep);
    for (_, path) in &snapshots[..excess] {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::store::{DATABASE, Store};

    fn fresh(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-backup-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn at(millis: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(millis)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn a_snapshot_opens_as_the_database_and_the_newest_are_kept() {
        let dir = fresh("snapshot");
        let database = dir.join(DATABASE);
        let store = Store::open(&database).unwrap();
        store
            .commit(|t| {
                t.execute(
                    "INSERT INTO totals (handle, accepted, rejected, through) \
                     VALUES ('aaaaaaaaaaaaaaaa', 7, 1, 60)",
                    [],
                )
            })
            .await
            .unwrap();
        let backups = dir.join("backups");
        let first = snapshot(&database, &backups, 2, at(1_000)).unwrap();
        assert_eq!(first, backups.join("puntohunter-1000.sqlite3"));
        let copy = Connection::open(&first).unwrap();
        let accepted: i64 = copy
            .query_row("SELECT accepted FROM totals", [], |row| row.get(0))
            .unwrap();
        assert_eq!(accepted, 7, "the committed rows");
        drop(copy);
        let restored = dir.join("restored.sqlite3");
        fs::copy(&first, &restored).unwrap();
        assert!(
            Store::open(&restored).is_ok(),
            "a store opens on a copy, as a restore does"
        );

        fs::write(backups.join("notes.txt"), "kept").unwrap();
        snapshot(&database, &backups, 2, at(2_000)).unwrap();
        snapshot(&database, &backups, 2, at(3_000)).unwrap();
        assert_eq!(
            names(&backups),
            [
                "notes.txt",
                "puntohunter-2000.sqlite3",
                "puntohunter-3000.sqlite3"
            ],
            "the newest two, and nothing else touched"
        );
    }

    #[test]
    fn a_failed_snapshot_leaves_no_file_behind_its_name() {
        let dir = fresh("failed");
        let backups = dir.join("backups");
        assert!(snapshot(&dir.join("missing.sqlite3"), &backups, 2, at(1_000)).is_err());
        assert!(
            names(&backups).iter().all(|name| !name.starts_with(PREFIX)),
            "{:?}",
            names(&backups)
        );
    }

    /// Waits until `dir` holds `count` snapshots.
    async fn until(dir: &Path, count: usize) {
        let snapshots = || {
            fs::read_dir(dir).map_or(0, |entries| {
                entries
                    .filter(|entry| {
                        let name = entry.as_ref().unwrap().file_name();
                        name.to_string_lossy().starts_with(PREFIX)
                    })
                    .count()
            })
        };
        for _ in 0..200 {
            if snapshots() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{count} snapshots, found {}", snapshots());
    }

    #[tokio::test]
    async fn takes_one_at_once_and_one_when_asked_out_of_turn() {
        let dir = fresh("trigger");
        let database = dir.join(DATABASE);
        let _store = Store::open(&database).unwrap();
        let backups = dir.join("backups");
        spawn(
            database,
            Backup {
                dir: backups.clone(),
                interval: Duration::from_secs(3600),
                keep: 10,
            },
        );
        until(&backups, 1).await;
        // A name of its own: snapshots are named to the millisecond.
        tokio::time::sleep(Duration::from_millis(5)).await;
        now();
        until(&backups, 2).await;
    }
}
