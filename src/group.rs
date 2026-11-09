//! The group's members, read from the server. Each member's public
//! `GET /api/v1/pool` is fetched every refresh, and its last good answer counts in the group's totals for three
//! refreshes. Only a member's own figures are read, never its group's, so no figure counts twice. With
//! `show_members`, each member's label and last figures are listed too.

use std::{
    fmt, ops,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use reqwest::StatusCode;
use serde::Deserialize;
use tokio::time::MissedTickBehavior;

use crate::{config, metrics};

/// How long a member's fetch may take, its body included.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest answer read: a pool resource is a few kilobytes.
const MAX_BODY: usize = 64 * 1024;

/// How many refreshes a good answer counts for, so one missed poll changes nothing.
const FRESH_REFRESHES: u32 = 3;

/// An instance's figures, as its overview counts them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Figures {
    /// Each pool's hashrate summed, in H/s.
    pub hash_rate: f64,
    /// Each pool's workers with a live connection, summed.
    pub devices: u64,
    /// Blocks found, the node's refusals aside.
    pub blocks_found: u64,
}

impl ops::Add for Figures {
    type Output = Figures;

    fn add(self, other: Figures) -> Figures {
        Figures {
            hash_rate: self.hash_rate + other.hash_rate,
            devices: self.devices + other.devices,
            blocks_found: self.blocks_found + other.blocks_found,
        }
    }
}

/// A member's last good answer and the start of the poll that asked for it; None before its first.
type Answer = Option<(Figures, Instant)>;

/// The group's name and its members' last good answers, shared by the pollers and the portal.
#[derive(Clone)]
pub struct Members {
    /// The overview card's heading.
    pub name: String,
    /// With `show_members`, the labels the card lists the instances by; None without it.
    pub labels: Option<Arc<Labels>>,
    refresh: Duration,
    /// Each member's last good answer and when it arrived, in config order.
    slots: Arc<Mutex<Vec<Answer>>>,
}

/// The instances' labels, public text: this instance's, then each member's with its portal's origin, in
/// config order. A member's name and URL are never among them.
#[derive(Debug, PartialEq)]
pub struct Labels {
    pub own: String,
    pub members: Vec<(String, Option<String>)>,
}

/// A member as the card lists it: its label, its portal's origin, and its figures if they count now.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub label: String,
    pub link: Option<String>,
    pub figures: Option<Figures>,
}

impl Members {
    /// With `show_members`, each member in config order, with its figures while they count.
    pub fn listed(&self) -> Option<Vec<Listed>> {
        let labels = self.labels.as_ref()?;
        let now = Instant::now();
        let slots = self.slots.lock().unwrap();
        Some(
            labels
                .members
                .iter()
                .zip(slots.iter())
                .map(|((label, link), answer)| Listed {
                    label: label.clone(),
                    link: link.clone(),
                    figures: answer
                        .filter(|(_, at)| self.fresh(*at, now))
                        .map(|(figures, _)| figures),
                })
                .collect(),
        )
    }

    /// How many members are configured, and the figures of each one counted now.
    pub fn counted(&self) -> (usize, Vec<Figures>) {
        let now = Instant::now();
        let slots = self.slots.lock().unwrap();
        let counted = slots
            .iter()
            .flatten()
            .filter(|(_, at)| self.fresh(*at, now))
            .map(|(figures, _)| *figures)
            .collect();
        (slots.len(), counted)
    }

    /// Members holding `answers`, in config order, polled by no one.
    #[cfg(test)]
    pub(crate) fn holding(name: &str, refresh: Duration, answers: Vec<Answer>) -> Self {
        Members {
            name: name.to_owned(),
            labels: None,
            refresh,
            slots: Arc::new(Mutex::new(answers)),
        }
    }

    /// The same members, listed by `labels` as with `show_members`.
    #[cfg(test)]
    pub(crate) fn labelled(self, labels: Labels) -> Self {
        Members {
            labels: Some(Arc::new(labels)),
            ..self
        }
    }

    /// Whether an answer that arrived `at` still counts `now`.
    fn fresh(&self, at: Instant, now: Instant) -> bool {
        now.duration_since(at) < self.refresh * FRESH_REFRESHES
    }
}

/// Polls each of `group`'s members in the background, counting answers on `network`.
pub fn spawn(group: &config::Group, network: config::Network) -> Members {
    let members = Members {
        name: group.name.clone(),
        labels: group.show_members.then(|| {
            Arc::new(Labels {
                own: group.label.clone().unwrap_or_default(),
                members: group
                    .members
                    .iter()
                    .map(|member| {
                        (
                            member.label.clone().unwrap_or_default(),
                            member.link.clone(),
                        )
                    })
                    .collect(),
            })
        }),
        refresh: group.refresh,
        slots: Arc::new(Mutex::new(vec![None; group.members.len()])),
    };
    let client = client(FETCH_TIMEOUT);
    for (slot, member) in group.members.iter().enumerate() {
        metrics::set_group_member_up(&member.name, false);
        tokio::spawn(poll(
            client.clone(),
            member.name.clone(),
            format!("{}/api/v1/pool", member.url),
            network.chain(),
            members.clone(),
            slot,
        ));
    }
    members
}

/// The members' HTTP client, apart from the nodes' RPC client. It follows no redirect, so a member's URL names
/// its API itself.
fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("puntohunter/{}", crate::VERSION))
        .build()
        .expect("the HTTP client builds")
}

/// Fetches member `name` at `url` every refresh, a fetch waiting for the last, from now on. Its moves into and
/// out of the totals are logged once and measured.
async fn poll(
    client: reqwest::Client,
    name: String,
    url: String,
    network: &'static str,
    members: Members,
    slot: usize,
) {
    let mut ticks = tokio::time::interval(members.refresh);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut counted = None;
    loop {
        // An answer counts from the poll that asked for it, and each poll judges the member at its start, so
        // the log and the metric follow the totals, which judge when read, within a fetch's time.
        let polled = ticks.tick().await.into_std();
        let answer = fetch(&client, &url, network).await;
        let last = {
            let mut slots = members.slots.lock().unwrap();
            if let Ok(figures) = answer {
                slots[slot] = Some((figures, polled));
            }
            slots[slot]
        };
        let up = last.is_some_and(|(_, at)| members.fresh(at, polled));
        if counted == Some(up) {
            continue;
        }
        match &answer {
            Ok(_) => tracing::info!(member = %name, "group member counted in the totals"),
            Err(reason) => tracing::warn!(member = %name, %reason, "group member not counted"),
        }
        metrics::set_group_member_up(&name, up);
        counted = Some(up);
    }
}

/// Why a member's answer does not count.
#[derive(Debug, PartialEq)]
enum Refusal {
    Timeout,
    Unreachable,
    Failed,
    /// Any status but 200, a redirect's included.
    Status(u16),
    TooLarge,
    /// Not a pool resource.
    Malformed,
    /// A pool resource on another network.
    Network(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Refusal::Timeout => write!(f, "timed out"),
            Refusal::Unreachable => write!(f, "cannot connect"),
            Refusal::Failed => write!(f, "request failed"),
            Refusal::Status(status) => write!(f, "status {status}"),
            Refusal::TooLarge => write!(f, "answer over {MAX_BODY} bytes"),
            Refusal::Malformed => write!(f, "not a pool resource"),
            Refusal::Network(network) => write!(f, "on network {network:?}"),
        }
    }
}

impl From<reqwest::Error> for Refusal {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Refusal::Timeout
        } else if e.is_connect() {
            Refusal::Unreachable
        } else {
            Refusal::Failed
        }
    }
}

/// The parts of a member's pool resource the group adds up; the rest is ignored, so members on other versions
/// still read each other.
#[derive(Deserialize)]
struct PoolResource {
    network: String,
    pools: Vec<PoolFigures>,
    blocks_found: u64,
}

#[derive(Deserialize)]
struct PoolFigures {
    hash_rate: f64,
    devices: u64,
}

/// A member's figures from its pool resource at `url`, if it answers one on `network`.
async fn fetch(client: &reqwest::Client, url: &str, network: &str) -> Result<Figures, Refusal> {
    let mut response = client.get(url).send().await?;
    if response.status() != StatusCode::OK {
        return Err(Refusal::Status(response.status().as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY as u64)
    {
        return Err(Refusal::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(Refusal::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    let resource: PoolResource = serde_json::from_slice(&body).map_err(|_| Refusal::Malformed)?;
    if resource.network != network {
        return Err(Refusal::Network(resource.network));
    }
    let hash_rate: f64 = resource.pools.iter().map(|pool| pool.hash_rate).sum();
    if !(hash_rate.is_finite() && resource.pools.iter().all(|pool| pool.hash_rate >= 0.0)) {
        return Err(Refusal::Malformed);
    }
    Ok(Figures {
        hash_rate,
        devices: resource.pools.iter().map(|pool| pool.devices).sum(),
        blocks_found: resource.blocks_found,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::{
        Router,
        body::{Body, Bytes},
        http::{StatusCode, header},
        response::{IntoResponse, Redirect},
        routing::get,
    };
    use futures_util::stream;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    /// A pool resource as `GET /api/v1/pool` gives it, abridged, with a field the group does not read.
    fn resource(network: &str) -> serde_json::Value {
        json!({
            "network": network,
            "version": "v0.1.9+63b44726",
            "outage": false,
            "pools": [
                { "id": "solo", "hash_rate": 1.5e12, "devices": 2, "worker_names": false },
                { "id": "pooled", "hash_rate": 0.5e12, "devices": 1 }
            ],
            "payouts": null,
            "blocks_found": 4
        })
    }

    /// Serves `router` on a loopback port, returning its origin.
    async fn serve(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        origin
    }

    /// What a member answering with `router` at `/api/v1/pool` counts as on regtest, with a short timeout.
    async fn fetched(router: Router) -> Result<Figures, Refusal> {
        let origin = serve(router).await;
        let client = client(Duration::from_millis(300));
        fetch(&client, &format!("{origin}/api/v1/pool"), "regtest").await
    }

    fn answering(body: serde_json::Value) -> Router {
        Router::new().route(
            "/api/v1/pool",
            get(move || async move { axum::Json(body.clone()) }),
        )
    }

    #[tokio::test]
    async fn sums_a_members_pools_and_ignores_the_rest() {
        assert_eq!(
            fetched(answering(resource("regtest"))).await,
            Ok(Figures {
                hash_rate: 2e12,
                devices: 3,
                blocks_found: 4,
            })
        );
    }

    #[tokio::test]
    async fn refuses_what_is_not_a_pool_resource() {
        let mut missing = resource("regtest");
        missing["pools"][1]
            .as_object_mut()
            .unwrap()
            .remove("devices");
        assert_eq!(fetched(answering(missing)).await, Err(Refusal::Malformed));
        let mut negative = resource("regtest");
        negative["pools"][0]["hash_rate"] = json!(-1.0);
        assert_eq!(fetched(answering(negative)).await, Err(Refusal::Malformed));
        let html = Router::new().route("/api/v1/pool", get(|| async { "<html></html>" }));
        assert_eq!(fetched(html).await, Err(Refusal::Malformed));
        assert_eq!(
            fetched(answering(json!({ "error": "not_found" }))).await,
            Err(Refusal::Malformed)
        );
    }

    #[tokio::test]
    async fn refuses_another_network() {
        assert_eq!(
            fetched(answering(resource("main"))).await,
            Err(Refusal::Network("main".into()))
        );
    }

    #[tokio::test]
    async fn refuses_a_status_or_a_redirect() {
        let down = Router::new().route(
            "/api/v1/pool",
            get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        );
        assert_eq!(fetched(down).await, Err(Refusal::Status(503)));
        let moved = Router::new()
            .route(
                "/api/v1/pool",
                get(|| async { Redirect::temporary("/elsewhere") }),
            )
            .route(
                "/elsewhere",
                get(|| async { axum::Json(resource("regtest")) }),
            );
        assert_eq!(fetched(moved).await, Err(Refusal::Status(307)));
        assert_eq!(fetched(Router::new()).await, Err(Refusal::Status(404)));
    }

    #[tokio::test]
    async fn refuses_a_slow_or_unreachable_member() {
        let slow = Router::new().route(
            "/api/v1/pool",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                axum::Json(resource("regtest"))
            }),
        );
        assert_eq!(fetched(slow).await, Err(Refusal::Timeout));
        // A port nothing listens on any more.
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/v1/pool", closed.local_addr().unwrap());
        drop(closed);
        let client = client(Duration::from_millis(300));
        assert_eq!(
            fetch(&client, &url, "regtest").await,
            Err(Refusal::Unreachable)
        );
    }

    #[tokio::test]
    async fn refuses_too_large_an_answer_with_or_without_its_length() {
        let padded = || {
            let mut padded = resource("regtest");
            padded["padding"] = json!("x".repeat(MAX_BODY));
            padded.to_string()
        };
        let sized = Router::new().route("/api/v1/pool", get(move || async move { padded() }));
        assert_eq!(fetched(sized).await, Err(Refusal::TooLarge));
        // Chunked, so no Content-Length warns before the body is read.
        let streamed = Router::new().route(
            "/api/v1/pool",
            get(|| async {
                let chunks =
                    (0..20).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b' '; 8192])));
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    Body::from_stream(stream::iter(chunks)),
                )
                    .into_response()
            }),
        );
        assert_eq!(fetched(streamed).await, Err(Refusal::TooLarge));
    }

    #[tokio::test]
    async fn a_member_counts_for_three_refreshes_and_rejoins_at_its_next_answer() {
        let answering = Arc::new(AtomicBool::new(true));
        let switch = answering.clone();
        let origin = serve(Router::new().route(
            "/api/v1/pool",
            get(move || {
                let answering = switch.load(Ordering::SeqCst);
                async move {
                    if answering {
                        axum::Json(resource("regtest")).into_response()
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    }
                }
            }),
        ))
        .await;
        let refresh = Duration::from_millis(100);
        let group = config::Group {
            name: "Group".into(),
            refresh,
            members: vec![config::Member {
                name: "stub".into(),
                url: origin,
                label: None,
                link: None,
            }],
            label: None,
            show_members: false,
        };
        let members = spawn(&group, config::Network::Regtest);
        let counted = || members.counted().1.len();
        let wait_for = |n: usize, within: Duration| async move {
            let start = Instant::now();
            while counted() != n {
                assert!(start.elapsed() < within, "never {n} counted");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            start.elapsed()
        };

        wait_for(1, refresh * 2).await;
        assert_eq!(members.counted().0, 1);
        answering.store(false, Ordering::SeqCst);
        // Still counted through two missed polls, then gone once three refreshes have passed.
        let gone = wait_for(0, refresh * 6).await;
        assert!(gone >= refresh * 2, "left after {gone:?}");
        answering.store(true, Ordering::SeqCst);
        wait_for(1, refresh * 2).await;
    }
}
