//! The JSON API under `/api/v1` ([API reference](../../docs/guide/api.md) sections 1 and 2): the
//! pool's data in raw values, which the portal's pages read in the browser, as any other web app may. Each
//! resource and its stream are built from one model, so the two cannot disagree.
//!
//! Errors are `{"error", "message"}` with a stable code, and never echo the username, token, or address they
//! were given. Nothing here logs a request.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{
        ConnectInfo, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tower_http::cors::{AllowOrigin, CorsLayer};

use super::{
    Portal,
    balance::{self, BalanceResource},
    blocks::{self, Blocks, LIMIT},
    fleet, host_of,
    lookup::Refusal,
    streams,
    worker::{self, Range, Unresolved},
};
use crate::{config, pool::blocks::Finders, store::StoreError, telemetry::rollup::Rate};

/// The `/api/v1` routes, nested into the portal's router, open to the browser pages of `origins`.
pub fn router(origins: &[String]) -> Router<Arc<Portal>> {
    let router = Router::new()
        .route("/pool", get(pool))
        .route("/lookup", post(lookup))
        .route("/balance", get(balance))
        .route("/health", get(health))
        .route("/group", get(group))
        .route("/stats", get(stats))
        .route("/blocks", get(pool_blocks))
        .route("/workers/{handle}", get(worker))
        .route("/workers/{handle}/events", get(worker_events))
        .route("/workers/{handle}/blocks", get(worker_blocks))
        .route("/workers/{handle}/series", get(worker_series))
        .route("/fleets/{fleet}", get(fleet))
        .route("/fleets/{fleet}/events", get(fleet_events))
        .route("/fleets/{fleet}/blocks", get(fleet_blocks))
        .route("/fleets/{fleet}/series", get(fleet_series))
        .fallback(async || ApiError::NotFound("no such resource"));
    if origins.is_empty() {
        return router;
    }
    router
        .layer(cors(origins))
        .layer(middleware::map_response(unlisted))
}

/// Drops what tower-http exposes to an origin it did not admit, so an unlisted origin gets no CORS header.
async fn unlisted(mut response: Response) -> Response {
    let headers = response.headers_mut();
    if !headers.contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN) {
        headers.remove(header::ACCESS_CONTROL_EXPOSE_HEADERS);
    }
    response
}

/// The CORS layer for `origins`: exact origins, no credentials, since the API uses no
/// cookies, and a preflight kept 10 minutes.
fn cors(origins: &[String]) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins.iter().filter_map(|origin| {
            // Checked at startup, so each is a valid header value.
            HeaderValue::from_str(origin).ok()
        })))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .expose_headers([header::RETRY_AFTER])
        .max_age(Duration::from_secs(600))
}

/// An API error: its status, its code, and a message for developers.
#[derive(Debug)]
pub enum ApiError {
    /// Malformed JSON, or a query parameter out of range.
    BadRequest(&'static str),
    /// The lookup's username does not parse.
    InvalidUsername,
    /// The lookup's username is a bare address.
    WorkerNameRequired,
    /// The balance token is missing, or opens no balance.
    Unauthorized(&'static str),
    NotFound(&'static str),
    /// Too many lookups: seconds until the next is allowed.
    RateLimited(u64),
    /// The client address holds as many streams as it may: seconds to wait.
    TooManyStreams(u64),
    /// The pool holds as many streams as it may: seconds to wait.
    StreamCapacity(u64),
    /// The store could not be read.
    Internal,
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        tracing::error!("cannot read the store: {e}");
        ApiError::Internal
    }
}

impl From<Unresolved> for ApiError {
    fn from(unresolved: Unresolved) -> Self {
        match unresolved {
            Unresolved::NotFound => ApiError::NotFound("no worker or fleet has that handle"),
            Unresolved::Failed => ApiError::Internal,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, "bad_request", *message),
            ApiError::InvalidUsername => (
                StatusCode::BAD_REQUEST,
                "invalid_username",
                "the username is not an address, a dot, and a worker name",
            ),
            ApiError::WorkerNameRequired => (
                StatusCode::BAD_REQUEST,
                "worker_name_required",
                "an address alone finds nothing: send the full username, worker name included",
            ),
            ApiError::Unauthorized(message) => (StatusCode::UNAUTHORIZED, "unauthorized", *message),
            ApiError::NotFound(message) => (StatusCode::NOT_FOUND, "not_found", *message),
            ApiError::RateLimited(_) => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "too many lookups from this address in the last minute",
            ),
            ApiError::TooManyStreams(_) => (
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_streams",
                "this address holds as many open streams as it may",
            ),
            ApiError::StreamCapacity(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "stream_capacity",
                "the pool holds as many open streams as it may",
            ),
            ApiError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "the pool could not read its store",
            ),
        };
        let mut body = json!({ "error": code, "message": message });
        let mut response = match self {
            ApiError::RateLimited(seconds) => {
                body["retry_after_secs"] = seconds.into();
                let mut response = (status, Json(body)).into_response();
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.into());
                response
            }
            ApiError::TooManyStreams(seconds) | ApiError::StreamCapacity(seconds) => {
                let mut response = (status, Json(body)).into_response();
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, seconds.into());
                response
            }
            ApiError::Unauthorized(_) => {
                let mut response = (status, Json(body)).into_response();
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                response
            }
            _ => (status, Json(body)).into_response(),
        };
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

/// How a response may be cached (API reference section 1.6).
#[derive(Debug, Clone, Copy)]
pub enum Cache {
    /// Public figures: revalidated on every use.
    Public,
    /// Anything carrying an address, a balance, or the pool's health.
    Private,
}

/// `value` as JSON, with its `Cache-Control`.
pub fn json(cache: Cache, value: &impl Serialize) -> Response {
    let mut response = Json(value).into_response();
    let cache = match cache {
        Cache::Public => "no-cache",
        Cache::Private => "no-store",
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}

/// A hashrate over 1 minute, 10 minutes, 1 hour, and 24 hours, keyed by window (API reference section 1.3).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Rates {
    #[serde(rename = "1m")]
    pub minute: Rate,
    #[serde(rename = "10m")]
    pub ten_minutes: Rate,
    #[serde(rename = "1h")]
    pub hour: Rate,
    #[serde(rename = "24h")]
    pub day: Rate,
}

/// A figure that may be missing, and why (API reference section 1.4): the figure, or
/// `{"unavailable": <reason>}`.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Figure<T> {
    Known(T),
    Unavailable { unavailable: Lacking },
}

impl<T> Figure<T> {
    pub fn missing(reason: Lacking) -> Self {
        Figure::Unavailable {
            unavailable: reason,
        }
    }
}

/// Why a figure is missing, sent as its `no_*` code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Lacking {
    /// The node gives no work right now.
    #[serde(rename = "no_template")]
    Template,
    /// No 24-hour hashrate yet, so no odds.
    #[serde(rename = "no_hash_rate")]
    HashRate,
    /// The pool's window holds no shares yet.
    #[serde(rename = "no_shares")]
    Shares,
}

/// A fee or reward portion: `{"ppm": n}` of the reward, or `{"sats": n}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Portion {
    Ppm(u64),
    Sats(u64),
}

impl From<config::Portion> for Portion {
    fn from(portion: config::Portion) -> Self {
        match portion {
            config::Portion::Ppm(ppm) => Portion::Ppm(ppm),
            config::Portion::Amount(amount) => Portion::Sats(amount.to_sat()),
        }
    }
}

async fn pool(State(portal): State<Arc<Portal>>, headers: HeaderMap) -> Result<Response, ApiError> {
    let pool = portal.overview(host_of(&headers)).await?;
    Ok(json(Cache::Public, &pool))
}

/// The group's totals (API reference section 2.11), naming no member.
async fn group(State(portal): State<Arc<Portal>>) -> Result<Response, ApiError> {
    match portal.group_totals().await? {
        Some(totals) => Ok(json(Cache::Public, &totals)),
        None => Err(ApiError::NotFound("no group is configured")),
    }
}

/// The pool stats (API reference section 2.12): each fleet by its shortened address only.
async fn stats(State(portal): State<Arc<Portal>>) -> Response {
    json(Cache::Public, &portal.stats())
}

/// A worker. It carries no address, worker name, or fleet handle: worker handles are public.
async fn worker(
    State(portal): State<Arc<Portal>>,
    Path(text): Path<String>,
) -> Result<Response, ApiError> {
    let device = worker::resolve(&portal, &text).await?;
    Ok(json(Cache::Private, &portal.worker(&device).await?))
}

async fn worker_events(
    State(portal): State<Arc<Portal>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(text): Path<String>,
) -> Result<Response, ApiError> {
    let handle = worker::resolve(&portal, &text).await?.handle;
    let guard = portal
        .streams
        .take(streams::client(peer, &headers, portal.trusted_proxy))?;
    Ok(worker::live_stream(portal, handle, guard))
}

/// A fleet, with its own address, so its owner can check which address the devices pay.
async fn fleet(
    State(portal): State<Arc<Portal>>,
    Path(text): Path<String>,
) -> Result<Response, ApiError> {
    Ok(json(Cache::Private, &portal.fleet_resource(&text).await?))
}

async fn fleet_events(
    State(portal): State<Arc<Portal>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(text): Path<String>,
) -> Result<Response, ApiError> {
    let (handle, _) = portal.members(&text).await?;
    let guard = portal
        .streams
        .take(streams::client(peer, &headers, portal.trusted_proxy))?;
    Ok(fleet::fleet_stream(portal, handle, guard))
}

/// A block list's `?limit=`, from 1 to [`LIMIT`].
#[derive(Deserialize)]
struct Limit {
    limit: Option<String>,
}

impl Limit {
    fn of(query: Result<Query<Limit>, QueryRejection>) -> Result<u32, ApiError> {
        let refused = ApiError::BadRequest("limit must be a whole number from 1 to 200");
        let Ok(Query(Limit { limit })) = query else {
            return Err(refused);
        };
        match limit {
            None => Ok(LIMIT),
            Some(text) => text
                .parse()
                .ok()
                .filter(|limit| (1..=LIMIT).contains(limit))
                .ok_or(refused),
        }
    }
}

async fn pool_blocks(
    State(portal): State<Arc<Portal>>,
    query: Result<Query<Limit>, QueryRejection>,
) -> Result<Response, ApiError> {
    let limit = Limit::of(query)?;
    let blocks = portal.blocks(Finders::All, limit).await?;
    Ok(json(Cache::Public, &Blocks { blocks }))
}

async fn worker_blocks(
    State(portal): State<Arc<Portal>>,
    Path(text): Path<String>,
    query: Result<Query<Limit>, QueryRejection>,
) -> Result<Response, ApiError> {
    let handle = worker::resolve(&portal, &text).await?.handle;
    let limit = Limit::of(query)?;
    let blocks = portal.blocks(Finders::Worker(handle), limit).await?;
    Ok(json(Cache::Public, &Blocks { blocks }))
}

async fn fleet_blocks(
    State(portal): State<Arc<Portal>>,
    Path(text): Path<String>,
    query: Result<Query<Limit>, QueryRejection>,
) -> Result<Response, ApiError> {
    let (handle, members) = portal.members(&text).await?;
    let limit = Limit::of(query)?;
    let mut blocks = portal.blocks(Finders::Fleet(handle), limit).await?;
    blocks::name_finders(&mut blocks, &members);
    Ok(json(Cache::Public, &Blocks { blocks }))
}

/// The span of a chart's `?range=`.
fn span(
    portal: &Portal,
    query: Result<Query<Range>, QueryRejection>,
) -> Result<(u64, u64, u64), ApiError> {
    query
        .ok()
        .and_then(|Query(range)| range.span(portal.rollup_secs))
        .ok_or(ApiError::BadRequest("range must be 1h, 24h, or 7d"))
}

async fn worker_series(
    State(portal): State<Arc<Portal>>,
    Path(text): Path<String>,
    query: Result<Query<Range>, QueryRejection>,
) -> Result<Response, ApiError> {
    let handle = worker::resolve(&portal, &text).await?.handle;
    let series = portal.worker_series(handle, span(&portal, query)?).await?;
    Ok(json(Cache::Public, &series))
}

async fn fleet_series(
    State(portal): State<Arc<Portal>>,
    Path(text): Path<String>,
    query: Result<Query<Range>, QueryRejection>,
) -> Result<Response, ApiError> {
    let (_, members) = portal.members(&text).await?;
    let series = portal.fleet_series(members, span(&portal, query)?).await?;
    Ok(json(Cache::Public, &series))
}

/// The fleet a full username opens (API reference section 2.2), under the lookup limit.
#[derive(Serialize)]
struct Found {
    fleet: String,
}

async fn lookup(
    State(portal): State<Arc<Portal>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<Map<String, Value>>, JsonRejection>,
) -> Result<Response, ApiError> {
    let client = streams::client(peer, &headers, portal.trusted_proxy);
    portal
        .lookups
        .admit_now(client)
        .map_err(ApiError::RateLimited)?;
    // An object with a username, and nothing else serde would take for one, such as an array, and an
    // optional fleet password.
    let fields = body.ok().and_then(|Json(body)| {
        let username = body.get("username")?.as_str()?.to_owned();
        let password = match body.get("password") {
            None | Some(Value::Null) => String::new(),
            Some(password) => password.as_str()?.to_owned(),
        };
        Some((username, password))
    });
    let Some((username, password)) = fields else {
        return Err(ApiError::BadRequest(
            "send a JSON body {\"username\": \"<address>.<worker>\"}, with \"password\" for a private fleet",
        ));
    };
    match portal.find(&username, &password).await {
        Ok(fleet) => Ok(json(
            Cache::Private,
            &Found {
                fleet: fleet.to_string(),
            },
        )),
        Err(Refusal::Invalid) => Err(ApiError::InvalidUsername),
        Err(Refusal::WorkerNameRequired) => Err(ApiError::WorkerNameRequired),
        Err(Refusal::NotFound) => Err(ApiError::NotFound(
            "no worker has mined under that username",
        )),
        Err(Refusal::Failed) => Err(ApiError::Internal),
    }
}

async fn balance(
    State(portal): State<Arc<Portal>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let token = balance::bearer(&headers).ok_or(ApiError::Unauthorized(
        "send the balance token as Authorization: Bearer <token>",
    ))?;
    match portal.balances(token).await? {
        Some(balances) => Ok(json(Cache::Private, &BalanceResource::from(balances))),
        None => Err(ApiError::Unauthorized("the token opens no balance")),
    }
}

/// The health check: 503 when miners get no work, with the health body rather than an error.
async fn health(State(portal): State<Arc<Portal>>) -> Response {
    let health = portal.health();
    let status = if health.unavailable() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    (status, json(Cache::Private, &health)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_serialize_as_the_proposal_sets() {
        let rate = |hash_rate| Rate {
            hash_rate,
            estimating: false,
        };
        let rates = Rates {
            minute: rate(1.0),
            ten_minutes: rate(2.0),
            hour: rate(3.0),
            day: Rate {
                hash_rate: 4.0,
                estimating: true,
            },
        };
        assert_eq!(
            serde_json::to_value(rates).unwrap(),
            json!({
                "1m": { "hash_rate": 1.0, "estimating": false },
                "10m": { "hash_rate": 2.0, "estimating": false },
                "1h": { "hash_rate": 3.0, "estimating": false },
                "24h": { "hash_rate": 4.0, "estimating": true },
            })
        );
        let missing: Figure<f64> = Figure::missing(Lacking::Template);
        assert_eq!(
            serde_json::to_value(missing).unwrap(),
            json!({ "unavailable": "no_template" })
        );
        assert_eq!(
            serde_json::to_value(Figure::Known(0.5)).unwrap(),
            json!(0.5)
        );
        assert_eq!(
            serde_json::to_value(Portion::Ppm(9000)).unwrap(),
            json!({ "ppm": 9000 })
        );
        assert_eq!(
            serde_json::to_value(Portion::Sats(1_000_000)).unwrap(),
            json!({ "sats": 1_000_000 })
        );
    }
}
