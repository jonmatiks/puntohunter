//! The SQLite store. One writer thread owns the database and answers each commit
//! over a oneshot channel, so persist-before-acknowledge is a plain await.
//!
//! A commit comes in two kinds. A journal commit returns once it is written, which survives a crash
//! of the pool process, without waiting for a disk sync. Every other commit syncs the write-ahead log before
//! it returns, which makes every journal commit before it durable too. While a journal commit is unsynced, the
//! writer syncs within [`SYNC_AFTER`] of it, so a power loss costs at most the last second of journal commits
//! and never a commit that moves money.

pub mod backup;

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant, SystemTime},
};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use tokio::sync::oneshot;

/// The database file inside the data directory.
pub const DATABASE: &str = "puntohunter.sqlite3";

/// How long a journal commit may stay unsynced: half a second, so the sync lands well within a
/// second.
pub const SYNC_AFTER: Duration = Duration::from_millis(500);

/// Numbered migrations, applied in order against PRAGMA user_version. Today the one migration is the whole
/// schema, which a schema change edits in place; its CHANGELOG entry gives the SQL that brings an existing
/// database up to date. Since mainnet payouts opened the database holds real balances and is never deleted.
const MIGRATIONS: &[&str] = &[include_str!("migrations/0001_schema.sql")];

type Request = Box<dyn FnOnce(&mut Writer) + Send>;

/// A handle to the writer thread, which runs while any handle is alive.
#[derive(Clone)]
pub struct Store {
    requests: mpsc::Sender<Request>,
    status: Arc<SyncStatus>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot open database {}: {source}", path.display())]
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    #[error(
        "database {} is at version {found}, above this binary's {known}: it was written by a newer binary, or \
         predates the squashed schema, in which case delete it, keeping handle.key beside it",
        path.display()
    )]
    Newer {
        path: PathBuf,
        found: usize,
        known: usize,
    },
    #[error("cannot start the store thread: {0}")]
    Thread(io::Error),
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot sync the database: {0}")]
    Sync(io::Error),
    #[error("the store thread stopped")]
    Stopped,
}

/// Whether a commit returns once written or once synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Journal,
    Synced,
}

/// The writer thread's state.
struct Writer {
    connection: Connection,
    /// The write-ahead log, which a sync flushes to disk.
    wal: PathBuf,
    unsynced: Unsynced,
    status: Arc<SyncStatus>,
}

impl Writer {
    /// Runs `f` in a transaction, syncing after it unless it is a journal commit. An error from `f` rolls
    /// the transaction back. The transaction takes the write lock as it begins, so one that reads and then
    /// writes never meets another process's commit between the two, as `puntohunter reserve credit` makes while
    /// the pool runs; the connection's busy timeout, five seconds, waits for the other's commit to end.
    fn run<T>(
        &mut self,
        kind: Kind,
        f: impl FnOnce(&Transaction) -> rusqlite::Result<T>,
    ) -> Result<T, StoreError> {
        let value = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .and_then(|transaction| {
                let value = f(&transaction)?;
                transaction.commit()?;
                Ok(value)
            })?;
        match kind {
            Kind::Journal => {
                self.unsynced.written(Instant::now());
                self.status.written(SystemTime::now());
            }
            Kind::Synced => self.sync().map_err(StoreError::Sync)?,
        }
        Ok(value)
    }

    /// Flushes the write-ahead log, and with it every commit written so far, to disk. SQLite writes each
    /// commit's frames to the log with plain writes, so fsync on a handle of our own reaches them all.
    fn sync(&mut self) -> io::Result<()> {
        match File::open(&self.wal) {
            Ok(wal) => wal.sync_data()?,
            // Checkpointed away: SQLite syncs the log before a checkpoint.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.unsynced.synced();
        self.status.synced();
        Ok(())
    }

    /// Serves `queue` until every handle is dropped, syncing each journal commit within [`SYNC_AFTER`].
    fn serve(mut self, queue: mpsc::Receiver<Request>) {
        loop {
            let request = match self.unsynced.deadline() {
                None => match queue.recv() {
                    Ok(request) => Some(request),
                    Err(_) => break,
                },
                Some(deadline) => {
                    match queue.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(request) => Some(request),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            };
            if let Some(request) = request {
                request(&mut self);
            }
            let now = Instant::now();
            if self.unsynced.due(now)
                && let Err(e) = self.sync()
            {
                tracing::error!("cannot sync the journal: {e}");
                // Try again after another interval rather than at once.
                self.unsynced.retry(now);
            }
        }
        if self.unsynced.deadline().is_some()
            && let Err(e) = self.sync()
        {
            tracing::error!("cannot sync the journal: {e}");
        }
    }
}

/// When the first journal commit not yet synced was written; the writer syncs by [`SYNC_AFTER`] after it.
/// Kept apart from the clock, so tests set the time.
#[derive(Debug, Default)]
struct Unsynced {
    since: Option<Instant>,
}

impl Unsynced {
    fn written(&mut self, now: Instant) {
        self.since.get_or_insert(now);
    }

    fn deadline(&self) -> Option<Instant> {
        self.since.map(|since| since + SYNC_AFTER)
    }

    fn due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| deadline <= now)
    }

    fn synced(&mut self) {
        self.since = None;
    }

    /// A failed sync: the next attempt comes an interval after `now`.
    fn retry(&mut self, now: Instant) {
        self.since = Some(now);
    }
}

/// The writer's syncs, as the metrics read them.
#[derive(Debug, Default)]
pub struct SyncStatus {
    /// Unix milliseconds of the first journal commit not yet synced; 0 when every one is.
    unsynced_since: AtomicU64,
    syncs: AtomicU64,
}

impl SyncStatus {
    fn written(&self, now: SystemTime) {
        let _ = self.unsynced_since.compare_exchange(
            0,
            unix_millis(now).max(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    fn synced(&self) {
        self.unsynced_since.store(0, Ordering::Relaxed);
        self.syncs.fetch_add(1, Ordering::Relaxed);
    }

    /// How long the oldest journal commit not yet synced has waited; zero when every one is synced.
    pub fn unsynced_for(&self, now: SystemTime) -> Duration {
        match self.unsynced_since.load(Ordering::Relaxed) {
            0 => Duration::ZERO,
            since => Duration::from_millis(unix_millis(now).saturating_sub(since)),
        }
    }

    /// Syncs since the store opened.
    pub fn syncs(&self) -> u64 {
        self.syncs.load(Ordering::Relaxed)
    }
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Store {
    /// Opens the database at `path`, creating it if need be, applies the migrations it lacks, and starts the
    /// writer thread.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        let open_error = |source| StoreError::Open {
            path: path.to_owned(),
            source,
        };
        let mut connection = Connection::open(path).map_err(open_error)?;
        configure(&connection).map_err(open_error)?;
        let found = version(&connection).map_err(open_error)?;
        if found > MIGRATIONS.len() {
            return Err(StoreError::Newer {
                path: path.to_owned(),
                found,
                known: MIGRATIONS.len(),
            });
        }
        migrate(&mut connection, found).map_err(open_error)?;
        let status = Arc::new(SyncStatus::default());
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        let writer = Writer {
            connection,
            wal: wal.into(),
            unsynced: Unsynced::default(),
            status: status.clone(),
        };
        let (requests, queue) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name("store".into())
            .spawn(move || writer.serve(queue))
            .map_err(StoreError::Thread)?;
        Ok(Store { requests, status })
    }

    /// Runs `f` in a transaction on the writer thread, returning its value once the transaction has
    /// committed and the write-ahead log is synced to disk, so the value is an acknowledgement that the write,
    /// and every journal commit before it, is durable. An error from `f` rolls the transaction back.
    pub async fn commit<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction) -> rusqlite::Result<T> + Send + 'static,
    {
        self.request(Kind::Synced, f).await
    }

    /// Runs `f` in a transaction on the writer thread, returning its value once the transaction is written:
    /// it survives a crash of the pool process, and is synced to disk within [`SYNC_AFTER`] or by the next
    /// commit, whichever comes first. For the share journal alone.
    pub async fn journal<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction) -> rusqlite::Result<T> + Send + 'static,
    {
        self.request(Kind::Journal, f).await
    }

    async fn request<T, F>(&self, kind: Kind, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction) -> rusqlite::Result<T> + Send + 'static,
    {
        let (reply, answer) = oneshot::channel();
        let request: Request = Box::new(move |writer| {
            let _ = reply.send(writer.run(kind, f));
        });
        self.requests
            .send(request)
            .map_err(|_| StoreError::Stopped)?;
        answer.await.map_err(|_| StoreError::Stopped)?
    }

    /// Runs `f` in a transaction on the writer thread without waiting for it, for a write nothing waits on.
    /// It is synced as [`Store::commit`] is. A failure is logged with `what`, the write's name.
    pub fn write<F>(&self, what: &'static str, f: F)
    where
        F: FnOnce(&Transaction) -> rusqlite::Result<()> + Send + 'static,
    {
        let request: Request = Box::new(move |writer| {
            if let Err(e) = writer.run(Kind::Synced, f) {
                tracing::error!("cannot write {what}: {e}");
            }
        });
        if self.requests.send(request).is_err() {
            tracing::error!("cannot write {what}: the store thread stopped");
        }
    }

    /// The writer's syncs, for the metrics.
    pub fn sync_status(&self) -> Arc<SyncStatus> {
        self.status.clone()
    }
}

/// A read-only connection beside the writer, for the portal: its reads never queue behind a commit.
#[derive(Clone)]
pub struct Reader {
    connection: Arc<Mutex<Connection>>,
}

impl Reader {
    /// Opens the database at `path` read-only. Open the [`Store`] first, so it exists and is migrated.
    pub fn open(path: &Path) -> Result<Reader, StoreError> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| StoreError::Open {
            path: path.to_owned(),
            source,
        })?;
        Ok(Reader {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Runs `f` on a blocking thread and returns its value.
    pub async fn read<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || f(&connection.lock().unwrap()))
            .await
            .map_err(|_| StoreError::Stopped)?
            .map_err(StoreError::from)
    }
}

/// WAL lets readers run beside the writer. synchronous=NORMAL writes a commit without syncing it, and the
/// writer syncs the log itself (see the module docs); SQLite still syncs it before each checkpoint.
fn configure(connection: &Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "journal_mode", "wal")?;
    connection.pragma_update(None, "synchronous", "normal")?;
    // The schema's REFERENCES clauses hold whatever SQLite's compiled-in default is.
    connection.pragma_update(None, "foreign_keys", "on")
}

fn version(connection: &Connection) -> rusqlite::Result<usize> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(version as usize)
}

/// Applies the migrations after the first `applied`, each in its own transaction with its version.
fn migrate(connection: &mut Connection, applied: usize) -> rusqlite::Result<()> {
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", (index + 1) as i64)?;
        transaction.commit()?;
        tracing::info!(version = index + 1, "database migrated");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A fresh path for a test database.
    fn path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(DATABASE)
    }

    /// The schema as SQLite records it, and the version.
    fn schema(path: &Path) -> (Vec<String>, usize) {
        let connection = Connection::open(path).unwrap();
        let mut statement = connection
            .prepare("SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")
            .unwrap();
        let sql = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        (sql, version(&connection).unwrap())
    }

    fn insert_block(transaction: &Transaction, hash: &str) -> rusqlite::Result<usize> {
        transaction.execute(
            "INSERT INTO blocks (hash, height, pool, status, found_at) \
             VALUES (?1, 1, 'solo', 'submitted', 0)",
            [hash],
        )
    }

    fn count(path: &Path, hash: &str) -> u32 {
        Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM blocks WHERE hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn migrates_a_fresh_database_then_leaves_it_alone() {
        let path = path("migrate");
        drop(Store::open(&path).unwrap());
        let migrated = schema(&path);
        assert_eq!(migrated.1, MIGRATIONS.len());
        assert!(
            migrated
                .0
                .iter()
                .any(|sql| sql.contains("CREATE TABLE blocks"))
        );

        drop(Store::open(&path).unwrap());
        assert_eq!(schema(&path), migrated);
    }

    #[test]
    fn refuses_a_database_from_a_newer_binary() {
        let path = path("newer");
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap();
        drop(connection);
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::Newer { found, .. }) if found == MIGRATIONS.len() + 1
        ));
    }

    #[tokio::test]
    async fn commits_in_wal_mode_and_syncs_the_log_itself() {
        let store = Store::open(&path("pragmas")).unwrap();
        let (mode, synchronous) = store
            .commit(|transaction| {
                let mode: String =
                    transaction.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
                let synchronous: i64 =
                    transaction.pragma_query_value(None, "synchronous", |row| row.get(0))?;
                Ok((mode, synchronous))
            })
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(synchronous, 1, "NORMAL");
    }

    #[test]
    fn an_unsynced_journal_commit_falls_due_by_sync_after_the_first() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut unsynced = Unsynced::default();
        assert_eq!(unsynced.deadline(), None);
        assert!(!unsynced.due(at(10_000)), "nothing written, nothing due");

        unsynced.written(at(0));
        unsynced.written(at(300));
        assert_eq!(
            unsynced.deadline(),
            Some(at(0) + SYNC_AFTER),
            "from the first"
        );
        assert!(!unsynced.due(at(0) + SYNC_AFTER - Duration::from_millis(1)));
        assert!(unsynced.due(at(0) + SYNC_AFTER));
        assert!(SYNC_AFTER < Duration::from_secs(1), "within the second");

        unsynced.synced();
        assert_eq!(unsynced.deadline(), None);
        unsynced.retry(at(2_000));
        assert_eq!(unsynced.deadline(), Some(at(2_000) + SYNC_AFTER));
    }

    #[tokio::test]
    async fn a_journal_commit_returns_unsynced_and_the_next_commit_syncs_it() {
        let store = Store::open(&path("kinds")).unwrap();
        let status = store.sync_status();
        store.commit(|t| insert_block(t, "a")).await.unwrap();
        let syncs = status.syncs();
        assert_eq!(status.unsynced_for(SystemTime::now()), Duration::ZERO);

        store.journal(|t| insert_block(t, "b")).await.unwrap();
        let later = SystemTime::now() + Duration::from_millis(100);
        assert!(status.unsynced_for(later) >= Duration::from_millis(100));
        assert_eq!(status.syncs(), syncs, "written, not synced");

        store.commit(|t| insert_block(t, "c")).await.unwrap();
        assert_eq!(status.syncs(), syncs + 1);
        assert_eq!(status.unsynced_for(later), Duration::ZERO);
    }

    #[tokio::test]
    async fn the_writer_syncs_a_lone_journal_commit_within_the_second() {
        let store = Store::open(&path("deadline")).unwrap();
        let status = store.sync_status();
        let syncs = status.syncs();
        let written = Instant::now();
        store.journal(|t| insert_block(t, "a")).await.unwrap();
        while status.syncs() == syncs {
            assert!(
                written.elapsed() < Duration::from_secs(1),
                "not synced within the second"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(written.elapsed() >= SYNC_AFTER - Duration::from_millis(10));
        assert_eq!(status.unsynced_for(SystemTime::now()), Duration::ZERO);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acknowledges_a_commit_only_once_it_is_durable() {
        let path = path("ack");
        let store = Store::open(&path).unwrap();
        let (inserted, wait_inserted) = mpsc::channel();
        let (release, wait_release) = mpsc::channel::<()>();
        let commit = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .commit(move |transaction| {
                        insert_block(transaction, "a")?;
                        inserted.send(()).unwrap();
                        wait_release.recv().unwrap();
                        Ok(())
                    })
                    .await
            }
        });

        // The row is written but its transaction is open: no acknowledgement, and nothing another reader,
        // or a restart, would find.
        wait_inserted.recv_timeout(Duration::from_secs(10)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!commit.is_finished());
        assert_eq!(count(&path, "a"), 0);

        release.send(()).unwrap();
        commit.await.unwrap().unwrap();
        drop(store);
        assert_eq!(count(&path, "a"), 1, "acknowledged, so committed");
    }

    #[tokio::test]
    async fn a_reader_sees_what_is_committed_and_cannot_write() {
        let path = path("reader");
        let store = Store::open(&path).unwrap();
        let reader = Reader::open(&path).unwrap();
        store.commit(|t| insert_block(t, "c")).await.unwrap();
        let count: u32 = reader
            .read(|c| c.query_row("SELECT COUNT(*) FROM blocks", [], |row| row.get(0)))
            .await
            .unwrap();
        assert_eq!(count, 1);
        let written = reader.read(|c| c.execute("DELETE FROM blocks", [])).await;
        assert!(matches!(written, Err(StoreError::Sqlite(_))), "{written:?}");
    }

    #[tokio::test]
    async fn a_failed_commit_writes_nothing_and_reports_the_error() {
        let path = path("rollback");
        let store = Store::open(&path).unwrap();
        let result = store
            .commit(|transaction| {
                insert_block(transaction, "b")?;
                insert_block(transaction, "b")
            })
            .await;
        assert!(matches!(result, Err(StoreError::Sqlite(_))), "{result:?}");
        assert_eq!(count(&path, "b"), 0);
    }

    /// Every table and view, with its columns.
    fn columns(connection: &Connection) -> Vec<(String, Vec<String>)> {
        let names: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type IN ('table', 'view') ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        names
            .into_iter()
            .map(|name| {
                let columns = connection
                    .prepare(&format!("SELECT name FROM pragma_table_info('{name}')"))
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                (name, columns)
            })
            .collect()
    }

    const HANDLE: &str = "aaaaaaaaaaaaaaaa";
    const ADDRESS: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";

    /// A row naming `pool` in each table with a pool column, in order, each with what it references.
    fn rows(pool: &str) -> Vec<String> {
        vec![
            format!(
                "INSERT INTO blocks (hash, height, pool, status, found_at) \
                 VALUES ('b', 1, '{pool}', 'submitted', 0)"
            ),
            format!(
                "INSERT INTO rollups (start, handle, pool, transport, accepted, work, stale, duplicate, \
                 low_difficulty, bad_version, invalid, best) \
                 VALUES (0, '{HANDLE}', '{pool}', 'V1', 1, 1.0, 0, 0, 0, 0, 0, 1.0)"
            ),
            format!(
                "INSERT INTO best_shares (handle, pool, day, best) VALUES ('{HANDLE}', '{pool}', 0, 1.0)"
            ),
            format!(
                "INSERT INTO fleets (fleet, handle, pool, worker, first_seen) \
                 VALUES ('{HANDLE}', '{HANDLE}', '{pool}', 'w', 0)"
            ),
            "INSERT INTO policies (id, window_multiplier, window_min_work, version) VALUES (1, 2.0, 0.0, 1)"
                .into(),
            format!(
                "INSERT INTO journal (pool, address, handle, policy, template, job, bits, miner_sats, \
                 pool_sats, target, proof, accepted_at) \
                 VALUES ('{pool}', '{ADDRESS}', '{HANDLE}', 1, 1, 1, 1, 10, 4, zeroblob(32), zeroblob(32), 0)"
            ),
            format!(
                "INSERT INTO snapshots (block, pool, cutoff, first, boundary, size, total, amount) \
                 VALUES ('b', '{pool}', 1, 1, '1', '1', '1', 4)"
            ),
            format!(
                "INSERT INTO allocations (block, pool, address, sats) VALUES ('b', '{pool}', '{ADDRESS}', 4)"
            ),
            format!(
                "INSERT INTO ledger (kind, address, pool, sats, block) \
                 VALUES ('pending', '{ADDRESS}', '{pool}', 4, 'b')"
            ),
        ]
    }

    /// Writes `rows` in one transaction, rolled back, and returns the index of the first SQLite refused.
    fn first_refused(connection: &mut Connection, rows: &[String]) -> Option<usize> {
        let transaction = connection.transaction().unwrap();
        rows.iter()
            .position(|sql| transaction.execute(sql, []).is_err())
    }

    #[test]
    fn every_table_names_the_pool_by_its_id_and_none_a_mode() {
        let path = path("pools");
        drop(Store::open(&path).unwrap());
        let mut connection = Connection::open(&path).unwrap();
        let columns = columns(&connection);
        let with_pool: Vec<&str> = columns
            .iter()
            .filter(|(_, columns)| columns.iter().any(|c| c == "pool"))
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            with_pool,
            [
                "allocations",
                "balances",
                "best_shares",
                "blocks",
                "fleets",
                "journal",
                "ledger",
                "rollups",
                "snapshots"
            ]
        );
        for (name, columns) in &columns {
            assert!(!columns.iter().any(|c| c == "mode"), "{name}: {columns:?}");
        }

        for pool in ["solo", "pooled", "hybrid", "my-pool-2", &"p".repeat(32)] {
            assert_eq!(first_refused(&mut connection, &rows(pool)), None, "{pool}");
        }
        // Each pool column refuses an id outside the form, one table at a time.
        for bad in [
            "",
            &"p".repeat(33),
            "Solo",
            "my_pool",
            "my pool",
            "pool!",
            "pool.1",
        ] {
            for table in 0..rows(bad).len() {
                if table == 4 {
                    continue; // policies has no pool
                }
                let mut written = rows("solo");
                written[table] = rows(bad)[table].clone();
                assert_eq!(
                    first_refused(&mut connection, &written),
                    Some(table),
                    "{bad:?}: {}",
                    written[table]
                );
            }
        }
    }

    #[test]
    fn the_journal_keeps_its_jobs_split() {
        let path = path("split");
        drop(Store::open(&path).unwrap());
        let mut connection = Connection::open(&path).unwrap();
        let journal = |miner: &str, pool: &str| {
            let mut written = rows("pooled");
            written[5] = format!(
                "INSERT INTO journal (pool, address, handle, policy, template, job, bits, miner_sats, \
                 pool_sats, target, proof, accepted_at) \
                 VALUES ('pooled', '{ADDRESS}', '{HANDLE}', 1, 1, 1, 1, {miner}, {pool}, zeroblob(32), \
                 zeroblob(32), 0)"
            );
            written
        };
        for (miner, pool) in [("10", "0"), ("10", "10"), ("0", "0")] {
            assert_eq!(
                first_refused(&mut connection, &journal(miner, pool)),
                None,
                "{miner} {pool}"
            );
        }
        for (miner, pool) in [
            ("10", "11"),
            ("-1", "0"),
            ("10", "-1"),
            ("NULL", "0"),
            ("10", "NULL"),
            ("1.5", "1"),
        ] {
            assert_eq!(
                first_refused(&mut connection, &journal(miner, pool)),
                Some(5),
                "{miner} {pool}"
            );
        }
    }

    #[test]
    fn refuses_a_database_from_before_the_squashed_schema() {
        let path = path("thirteen");
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 13).unwrap();
        drop(connection);
        let refused = Store::open(&path);
        assert!(
            matches!(
                &refused,
                Err(StoreError::Newer {
                    found: 13,
                    known: 1,
                    ..
                })
            ),
            "{:?}",
            refused.as_ref().err()
        );
        let message = refused.err().unwrap().to_string();
        assert!(message.contains("handle.key"), "{message}");
    }
}
