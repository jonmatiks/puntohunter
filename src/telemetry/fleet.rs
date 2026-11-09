//! Fleets: every worker mining to one address, recorded under the address's fleet handle with its
//! worker handle, pool, worker name, miner type, and the address, which the fleet page and its workers' pages
//! show. A worker joins its fleet when it authorizes over V1 or opens a channel over V2.

use std::{collections::HashMap, sync::Mutex};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::handle::Handle;
use crate::{mining::pool::PoolId, store::Store};

/// A worker in a fleet.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub handle: Handle,
    pub pool: PoolId,
    /// Empty for a worker mining with the bare address.
    pub worker: String,
    /// Unix seconds.
    pub first_seen: u64,
    /// The miner type, from [`vendor`]; None when the miner named none.
    pub vendor: Option<String>,
}

/// The miner type in `text`, a V2 vendor or a V1 user agent such as "bitaxe/BM1370/v2.15.3": its first word,
/// without a version at its end, in the letters, digits, '.', '_', '-', and '+' the fleet table admits, at
/// most 24 of them and at least one a letter or digit. None when nothing is left. A version would let a rare
/// firmware be followed across pools.
pub fn vendor(text: &str) -> Option<String> {
    let word = text.split(['/', ' ', '(', ';']).next()?;
    let word = word.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let word = word.strip_suffix(['v', 'V']).unwrap_or(word);
    let word = word.trim_end_matches(['-', '_', '.']);
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+');
    (word.len() <= 24 && word.chars().all(ok) && word.chars().any(|c| c.is_ascii_alphanumeric()))
        .then(|| word.to_owned())
}

/// `address` as the stats lists show it: its first 6 and last 4 characters, `bc1qw5…f3t4`, too few
/// to find the address from.
pub fn shortened(address: &str) -> String {
    let length = address.chars().count();
    if length <= 10 {
        return "…".into();
    }
    let head: String = address.chars().take(6).collect();
    let tail: String = address.chars().skip(length - 4).collect();
    format!("{head}…{tail}")
}

/// Records each worker in its fleet through the store's writer thread.
pub struct Fleets {
    store: Store,
    /// The workers recorded since the pool started, with the miner type recorded, so a reconnect from the
    /// same miner writes nothing.
    recorded: Mutex<HashMap<Handle, Option<String>>>,
}

impl Fleets {
    pub fn new(store: Store) -> Self {
        Fleets {
            store,
            recorded: Mutex::new(HashMap::new()),
        }
    }

    /// Records `member` in `fleet`, the fleet of `address`, private or open, once per worker and pool, and
    /// again when it names another miner type, without waiting for the write.
    pub fn record(&self, fleet: Handle, address: String, private: bool, member: Member) {
        {
            let mut recorded = self.recorded.lock().unwrap();
            match recorded.get(&member.handle) {
                Some(vendor) if member.vendor.is_none() || *vendor == member.vendor => return,
                _ => {
                    recorded.insert(member.handle, member.vendor.clone());
                }
            }
        }
        self.store.write("a fleet row", move |t| {
            insert(t, fleet, &address, private, &member)
        });
    }
}

/// Adds `member` to `fleet`, the fleet of `address`, private or open, keeping the row of a worker already
/// recorded but filling in its address if it was recorded before addresses were, and its miner type if this
/// one names one.
pub fn insert(
    transaction: &Transaction,
    fleet: Handle,
    address: &str,
    private: bool,
    member: &Member,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO fleets (fleet, handle, pool, worker, first_seen, address, vendor, private) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (handle) DO UPDATE SET address = coalesce(address, excluded.address), \
         vendor = coalesce(excluded.vendor, vendor)",
        params![
            fleet.to_string(),
            member.handle.to_string(),
            member.pool,
            member.worker,
            member.first_seen as i64,
            address,
            member.vendor,
            private
        ],
    )?;
    Ok(())
}

/// Whether `fleet` is a private fleet, keyed on a secret as well as its address.
pub fn private(connection: &Connection, fleet: Handle) -> rusqlite::Result<bool> {
    Ok(connection
        .query_row(
            "SELECT private FROM fleets WHERE fleet = ?1 LIMIT 1",
            [fleet.to_string()],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// The payout address of `fleet`; none for a fleet recorded only before addresses were.
pub fn address(connection: &Connection, fleet: Handle) -> rusqlite::Result<Option<String>> {
    connection
        .query_row(
            "SELECT address FROM fleets WHERE fleet = ?1 AND address IS NOT NULL LIMIT 1",
            [fleet.to_string()],
            |row| row.get(0),
        )
        .optional()
}

/// The payout address of the fleet the worker `handle` mines in.
pub fn worker_address(connection: &Connection, handle: Handle) -> rusqlite::Result<Option<String>> {
    let fleet: Option<String> = connection
        .query_row(
            "SELECT fleet FROM fleets WHERE handle = ?1",
            [handle.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    match fleet.and_then(|fleet| fleet.parse().ok()) {
        Some(fleet) => address(connection, fleet),
        None => Ok(None),
    }
}

/// The miner type of the worker `handle`, if it named one.
pub fn worker_vendor(connection: &Connection, handle: Handle) -> rusqlite::Result<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT vendor FROM fleets WHERE handle = ?1",
            [handle.to_string()],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

/// Every pool a worker has mined in, by id.
pub fn pools(connection: &Connection) -> rusqlite::Result<Vec<PoolId>> {
    connection
        .prepare("SELECT DISTINCT pool FROM fleets ORDER BY pool")?
        .query_map([], |row| row.get(0))?
        .collect()
}

/// The workers of `fleet`, oldest first; none for a fleet never recorded.
pub fn members(connection: &Connection, fleet: Handle) -> rusqlite::Result<Vec<Member>> {
    let mut statement = connection.prepare(
        "SELECT handle, pool, worker, first_seen, vendor FROM fleets WHERE fleet = ?1 \
         ORDER BY first_seen, handle",
    )?;
    let rows = statement.query_map([fleet.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, PoolId>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut members = Vec::new();
    for row in rows {
        let (handle, pool, worker, first_seen, vendor) = row?;
        // The table's checks admit nothing else.
        if let Ok(handle) = handle.parse() {
            members.push(Member {
                handle,
                pool,
                worker,
                first_seen: first_seen as u64,
                vendor,
            });
        }
    }
    Ok(members)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{mining::pool::tests::id, store::DATABASE};

    /// Address-shaped text; the table checks shape, not network.
    const ADDRESS: &str = "bcrt1qfleetaddressaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "bcrt1qotheraddressaaaaaaaaaaaaaaaaaaaaaaa";

    fn store(name: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-fleet-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(&dir.join(DATABASE)).unwrap()
    }

    fn handle(c: char) -> Handle {
        c.to_string().repeat(16).parse().unwrap()
    }

    fn member(c: char, pool: PoolId, worker: &str, first_seen: u64) -> Member {
        Member {
            handle: handle(c),
            pool,
            worker: worker.into(),
            first_seen,
            vendor: None,
        }
    }

    #[test]
    fn a_listed_address_keeps_six_and_four() {
        assert_eq!(
            shortened("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"),
            "bc1qw5…f3t4"
        );
        assert_eq!(
            shortened("1BoatSLRHtKNngkdXEeobR76b53LETtpyT"),
            "1BoatS…tpyT"
        );
    }

    #[test]
    fn a_miner_type_is_its_first_word_without_a_version() {
        for (text, expected) in [
            ("bitaxe/BM1370/v2.15.3", Some("bitaxe")),
            ("bitaxe", Some("bitaxe")),
            ("PuntoMiner", Some("PuntoMiner")),
            ("cgminer/4.10.0", Some("cgminer")),
            ("NerdMiner_v2.1", Some("NerdMiner")),
            ("bitaxe-v2.15", Some("bitaxe")),
            ("puntohunter-e2e/1.0", Some("puntohunter-e2e")),
            ("sv2-testclient", Some("sv2-testclient")),
            ("Braiins OS 24.1", Some("Braiins")),
            ("NerdQAxe++/BM1370/v1.0", Some("NerdQAxe++")),
            ("NerdQAxe+", Some("NerdQAxe+")),
            ("+", None),
            ("++/1.0", None),
            ("v1.2", None),
            ("", None),
            ("/1.0", None),
            ("<script>", None),
            ("bcrt1qfleetaddressaaaaaaaaaaaaaaaaaaaaaaa", None),
        ] {
            assert_eq!(vendor(text).as_deref(), expected, "{text:?}");
        }
    }

    #[tokio::test]
    async fn a_miner_type_is_kept_until_another_is_named() {
        let store = store("vendor");
        let fleets = Fleets::new(store.clone());
        for (named, kept) in [
            (None, None),
            (Some("bitaxe"), Some("bitaxe")),
            (None, Some("bitaxe")),
            (Some("PuntoMiner"), Some("PuntoMiner")),
        ] {
            let member = Member {
                vendor: named.map(str::to_owned),
                ..member('a', id("solo"), "rig", 1)
            };
            fleets.record(handle('f'), ADDRESS.into(), false, member);
            let mut found = None;
            for _ in 0..100 {
                found = store
                    .commit(|t| worker_vendor(t, handle('a')))
                    .await
                    .unwrap();
                if found.as_deref() == kept {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(found.as_deref(), kept, "after {named:?}");
        }
        assert_eq!(
            read(&store, handle('f')).await[0].vendor.as_deref(),
            Some("PuntoMiner")
        );
    }

    async fn read(store: &Store, fleet: Handle) -> Vec<Member> {
        store.commit(move |t| members(t, fleet)).await.unwrap()
    }

    #[tokio::test]
    async fn records_each_worker_once_and_keeps_its_first_row() {
        let store = store("record");
        let fleets = Fleets::new(store.clone());
        let f = || ADDRESS.to_owned();
        fleets.record(handle('f'), f(), false, member('a', id("solo"), "rig", 10));
        fleets.record(handle('f'), f(), false, member('b', id("pooled"), "", 20));
        fleets.record(handle('f'), f(), false, member('a', id("solo"), "rig", 30));
        fleets.record(
            handle('g'),
            OTHER.into(),
            false,
            member('c', id("solo"), "other", 40),
        );
        // A restart forgets what was recorded; the table keeps the first row.
        Fleets::new(store.clone()).record(
            handle('f'),
            f(),
            false,
            member('a', id("solo"), "rig", 50),
        );

        let mut found = Vec::new();
        for _ in 0..100 {
            found = read(&store, handle('f')).await;
            if found.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            found,
            [
                member('a', id("solo"), "rig", 10),
                member('b', id("pooled"), "", 20)
            ]
        );
        assert_eq!(read(&store, handle('g')).await.len(), 1);
        assert!(read(&store, handle('h')).await.is_empty());
        let addresses = store
            .commit(|t| {
                Ok((
                    address(t, handle('f'))?,
                    worker_address(t, handle('b'))?,
                    worker_address(t, handle('c'))?,
                    address(t, handle('h'))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            addresses,
            (
                Some(ADDRESS.into()),
                Some(ADDRESS.into()),
                Some(OTHER.into()),
                None
            )
        );
    }

    #[tokio::test]
    async fn a_row_recorded_before_addresses_takes_its_address_once() {
        let store = store("backfill");
        store
            .commit(|t| {
                t.execute(
                    "INSERT INTO fleets (fleet, handle, pool, worker, first_seen) \
                     VALUES ('ffffffffffffffff', 'aaaaaaaaaaaaaaaa', 'solo', 'rig', 1)",
                    [],
                )
            })
            .await
            .unwrap();
        assert_eq!(
            store.commit(|t| address(t, handle('f'))).await.unwrap(),
            None
        );
        for filled in [ADDRESS, OTHER] {
            let member = member('a', id("solo"), "rig", 9);
            store
                .commit(move |t| insert(t, handle('f'), filled, false, &member))
                .await
                .unwrap();
        }
        assert_eq!(
            store.commit(|t| address(t, handle('f'))).await.unwrap(),
            Some(ADDRESS.into()),
            "filled once, then kept"
        );
        assert_eq!(read(&store, handle('f')).await[0].first_seen, 1);
    }

    #[tokio::test]
    async fn the_table_admits_only_handles_names_addresses_and_miner_types_where_they_belong() {
        let store = store("checks");
        let ok = member('a', id("solo"), "rack.1-x_Y", 1);
        store
            .commit(move |t| insert(t, handle('f'), ADDRESS, false, &ok))
            .await
            .unwrap();
        for (fleet, handle_text, worker, address) in [
            ("bcrt1qxxxxxxxxxx", "aaaaaaaaaaaaaaab", "rig", ADDRESS),
            ("ffffffffffffffff", "bcrt1qxxxxxxxxxx", "rig", ADDRESS),
            ("ffffffffffffffff", "aaaaaaaaaaaaaaac", "a b", ADDRESS),
            ("ffffffffffffffff", "aaaaaaaaaaaaaaad", "<b>", ADDRESS),
            (
                "ffffffffffffffff",
                "aaaaaaaaaaaaaaae",
                &"w".repeat(65),
                ADDRESS,
            ),
            (
                "ffffffffffffffff",
                "aaaaaaaaaaaaaaaf",
                "rig",
                "<script>alert(1)</script>",
            ),
            ("ffffffffffffffff", "aaaaaaaaaaaaaaag", "rig", "short"),
        ] {
            let (fleet, handle_text, worker, address) = (
                fleet.to_owned(),
                handle_text.to_owned(),
                worker.to_owned(),
                address.to_owned(),
            );
            let result = store
                .commit(move |t| {
                    t.execute(
                        "INSERT INTO fleets (fleet, handle, pool, worker, first_seen, address) \
                         VALUES (?1, ?2, 'solo', ?3, 0, ?4)",
                        params![fleet, handle_text, worker, address],
                    )
                })
                .await;
            assert!(result.is_err(), "{result:?}");
        }
        let plus = member('i', id("solo"), "rig", 1);
        let plus = Member {
            vendor: Some("NerdQAxe++".into()),
            ..plus
        };
        store
            .commit(move |t| insert(t, handle('f'), ADDRESS, false, &plus))
            .await
            .unwrap();
        assert_eq!(
            store
                .commit(|t| worker_vendor(t, handle('i')))
                .await
                .unwrap()
                .as_deref(),
            Some("NerdQAxe++")
        );
        for vendor in [
            "x".repeat(25),
            "a b".into(),
            "<b>".into(),
            String::new(),
            "++".into(),
        ] {
            let result = store
                .commit(move |t| {
                    t.execute(
                        "INSERT INTO fleets (fleet, handle, pool, worker, first_seen, address, vendor) \
                         VALUES ('ffffffffffffffff', 'aaaaaaaaaaaaaaah', 'solo', 'rig', 0, ?1, ?2)",
                        params![ADDRESS, vendor],
                    )
                })
                .await;
            assert!(result.is_err(), "{result:?}");
        }
    }
}
