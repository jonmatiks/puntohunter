//! The private balance view: what one address is owed in each pool, pending, available, reserved,
//! and paid, behind the balance token its owner holds, which `GET /api/v1/balance` takes as a bearer token, so
//! it never sits in a URL or a browser's history. It names no address, may not be cached, and nothing here
//! logs a token.

use axum::http::{HeaderMap, header};
use rusqlite::Connection;
use serde::Serialize;

use super::Portal;
use crate::{mining::pool::PoolId, store::StoreError, tokens};

/// An address's balance in each pool it holds one in, in satoshis. The configured pools come in config order,
/// then any no longer configured, by id.
#[derive(Debug, Default)]
pub struct Balances {
    pools: Vec<PoolBalance>,
}

#[derive(Debug)]
struct PoolBalance {
    pool: PoolId,
    balance: Balance,
}

/// The balance resource of `GET /api/v1/balance` (API reference section 2.9): the same balances, each named
/// by its unit.
#[derive(Debug, Serialize)]
pub struct BalanceResource {
    pools: Vec<BalanceSats>,
}

#[derive(Debug, Serialize)]
struct BalanceSats {
    pool: PoolId,
    pending_sats: u64,
    available_sats: u64,
    reserved_sats: u64,
    paid_sats: u64,
}

impl From<&PoolBalance> for BalanceSats {
    fn from(PoolBalance { pool, balance }: &PoolBalance) -> Self {
        BalanceSats {
            pool: pool.clone(),
            pending_sats: balance.pending,
            available_sats: balance.available,
            reserved_sats: balance.reserved,
            paid_sats: balance.paid,
        }
    }
}

impl From<Balances> for BalanceResource {
    fn from(balances: Balances) -> Self {
        BalanceResource {
            pools: balances.pools.iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Default)]
struct Balance {
    pending: u64,
    available: u64,
    reserved: u64,
    paid: u64,
}

/// The balances of the address `token` opens, by pool, or None for a token that opens none.
fn balances(connection: &Connection, token: &str) -> rusqlite::Result<Option<Vec<PoolBalance>>> {
    let Some(address) = tokens::address(connection, token)? else {
        return Ok(None);
    };
    let mut rows = connection.prepare_cached(
        "SELECT pool, pending, available, reserved, paid FROM balances WHERE address = ?1",
    )?;
    rows.query_map([&address], |row| {
        Ok(PoolBalance {
            pool: row.get(0)?,
            balance: Balance {
                pending: row.get::<_, i64>(1)? as u64,
                available: row.get::<_, i64>(2)? as u64,
                reserved: row.get::<_, i64>(3)? as u64,
                paid: row.get::<_, i64>(4)? as u64,
            },
        })
    })?
    .collect::<rusqlite::Result<_>>()
    .map(Some)
}

impl Portal {
    pub async fn balances(&self, token: &str) -> Result<Option<Balances>, StoreError> {
        let token = token.to_owned();
        let Some(mut pools) = self
            .reader
            .read(move |connection| balances(connection, &token))
            .await?
        else {
            return Ok(None);
        };
        // The configured pools in config order, then any no longer configured, by id.
        let order = |id: &PoolId| self.pools.iter().position(|pool| pool.id == *id);
        pools.sort_by(|a, b| match (order(&a.pool), order(&b.pool)) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.pool.cmp(&b.pool),
        });
        Ok(Some(Balances { pools }))
    }
}

/// The bearer token `Authorization` carries, if any.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}
