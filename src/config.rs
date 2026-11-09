//! The per-network TOML config passed to `puntohunter serve --config <file>`. Every value is validated at
//! startup, and a rejected value names its key.

use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use bitcoin::{Address, Amount, ScriptBuf, address::NetworkUnchecked};
use serde::Deserialize;

use crate::mining::{
    coinbase::{DEFAULT_TAG, MAX_TAG_LEN},
    pool::{Personal, Pool, PoolId},
};

#[derive(Debug)]
pub struct Config {
    pub network: Network,
    /// Holds the database and blocks/; data/<network> unless configured.
    pub data_dir: PathBuf,
    /// The primary node: the pool host's own, which holds the payout wallet.
    pub node: Node,
    /// The other nodes of the fleet, in config order; none makes the primary the whole fleet.
    pub secondary_nodes: Vec<Node>,
    /// How the fleet judges its nodes, every one alike.
    pub node_checks: NodeChecks,
    /// The pools, in config order, each built once and shared.
    pub pools: Vec<Arc<Pool>>,
    pub public: Public,
    pub stratum: Stratum,
    pub metrics: Metrics,
    pub portal: Portal,
    pub api: Api,
    pub vardiff: Vardiff,
    pub telemetry: Telemetry,
    pub addresses: Addresses,
    pub coinbase: Coinbase,
    /// The fee outputs every coinbase carries after the miner portion, in config order, unless its pool sets
    /// its own; none pays the miner portion the whole reward.
    pub fees: Vec<Fee>,
    pub accounting: Accounting,
    /// Pooled and hybrid payouts, when configured. Without them the payout worker does not run, and balances
    /// carry forward.
    pub payouts: Option<Payouts>,
    /// The other instances whose figures the overview adds to its own; None without `[group]`.
    pub group: Option<Group>,
    /// The IP-to-country database the stats page's countries come from; None without `[geo]`.
    pub geo: Option<Geo>,
    /// How many rows each list of the stats page shows.
    pub stats: Stats,
    /// The database's snapshots, which other hosts pull; None without `[backup]`.
    pub backup: Option<Backup>,
}

/// The database's snapshots: one every `interval`, and one at once after a block
/// with a pool output is allocated or a payout batch is broadcast, written into `dir`, the newest `keep` kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    pub dir: PathBuf,
    pub interval: Duration,
    pub keep: usize,
}

/// How many rows each list of the stats page and `GET /api/v1/stats` shows, each 10 by default.
/// The miner types and the countries past theirs fold into "Other" and "Elsewhere". A fleet's
/// ranks count every fleet, whatever is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Stats {
    pub best_shares_round: usize,
    pub best_shares_24h: usize,
    pub hash_rates: usize,
    pub miner_types: usize,
    pub countries: usize,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            best_shares_round: 10,
            best_shares_24h: 10,
            hash_rates: 10,
            miner_types: 10,
            countries: 10,
        }
    }
}

/// `stats`, each list showing at least one row.
fn stats(stats: Stats) -> Result<Stats, InvalidConfig> {
    for (key, rows) in [
        ("stats.best_shares_round", stats.best_shares_round),
        ("stats.best_shares_24h", stats.best_shares_24h),
        ("stats.hash_rates", stats.hash_rates),
        ("stats.miner_types", stats.miner_types),
        ("stats.countries", stats.countries),
    ] {
        if rows == 0 {
            return Err(invalid(key, "must be 1 or more"));
        }
    }
    Ok(stats)
}

/// The country database: a MaxMind DB file, such as DB-IP's IP to Country Lite.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geo {
    /// Reopened when it changes, so a monthly update needs no restart.
    pub database: PathBuf,
    /// The fewest connected workers a country is shown with; fewer fold into "Elsewhere". 1, the default,
    /// shows every country; more keeps a row from showing one worker's hashrate beside where it is.
    #[serde(default = "default_min_workers")]
    pub min_workers: usize,
}

fn default_min_workers() -> usize {
    1
}

/// A group of instances on hosts of their own, each listing the others.
#[derive(Debug)]
pub struct Group {
    /// The overview card's heading.
    pub name: String,
    /// How often each member is read.
    pub refresh: Duration,
    /// The other instances, in config order; this one is implicit.
    pub members: Vec<Member>,
    /// This instance's label, public text the card shows with `show_members`.
    pub label: Option<String>,
    /// Whether the overview's card and the group resource list each instance by its label.
    pub show_members: bool,
}

/// Another instance of the group.
#[derive(Debug)]
pub struct Member {
    /// Its name in logs and metric labels, never on a page or in the API.
    pub name: String,
    /// Its origin, such as https://east.example.com, read at `<url>/api/v1/pool`. Never shown: it can be a
    /// tunnel.
    pub url: String,
    /// Public text the card shows with `show_members`.
    pub label: Option<String>,
    /// The origin of its portal, which its label links to.
    pub link: Option<String>,
}

/// The P2TR dust limit: the largest dust limit of a standard output, so a payout at or above it is never dust
/// whatever address it pays.
pub const P2TR_DUST: Amount = Amount::from_sat(330);

/// The most outputs a payout batch may have. At 43 vbytes a P2TR output, a full batch stays well inside a
/// standard transaction's 100,000 vbytes with its inputs.
pub const MAX_BATCH_OUTPUTS: u32 = 1_000;

/// Batched on-chain payouts.
#[derive(Debug, Clone, PartialEq)]
pub struct Payouts {
    /// The payout wallet's name on the node, which holds the pool wallet's key and nothing else.
    pub wallet: String,
    /// The minimum payout, which an address's available balances in every pool together must reach.
    pub threshold: Amount,
    /// How often a batch is attempted.
    pub interval: Duration,
    pub outputs_per_batch: u32,
    /// Where a batch's fee rate comes from.
    pub rate: Rate,
    /// In sat/vB: a batch whose rate is higher is deferred.
    pub fee_rate_cap: f64,
}

/// Where a payout batch's fee rate comes from, raised to the node's minimum either way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rate {
    /// Regtest, which has no fee estimates: this many sat/vB.
    Fixed(f64),
    /// Mainnet: the node's `estimatesmartfee` for confirmation within this many blocks.
    Estimate(u16),
}

/// The confirmation targets `estimatesmartfee` takes, in blocks.
pub const CONFIRMATION_TARGETS: std::ops::RangeInclusive<u16> = 1..=1008;

/// The PPLNS window policy of pooled and hybrid accounting: the window covers the last
/// max(window_multiplier × network difficulty, window_min_work) of accepted work, in difficulty-1 units. The
/// spec calls the two k and W_min.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accounting {
    pub window_multiplier: f64,
    pub window_min_work: f64,
    /// Blocks beyond coinbase maturity before a block's allocations are available.
    pub maturity_buffer: u32,
}

/// A PPLNS window policy, each pool's own: k and W_min.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub window_multiplier: f64,
    pub window_min_work: f64,
}

impl Accounting {
    /// The global window policy.
    pub fn window(&self) -> Window {
        Window {
            window_multiplier: self.window_multiplier,
            window_min_work: self.window_min_work,
        }
    }
}

/// Parts per million: the unit percents are kept in, so the split is exact integer arithmetic.
pub const PPM: u64 = 1_000_000;

/// One fee output: who receives it, where, and how much of each block's reward.
#[derive(Debug, Clone, PartialEq)]
pub struct Fee {
    /// What the portal calls the receiver, such as "operator" or "partner".
    pub name: String,
    pub address: Address,
    pub portion: Portion,
}

/// How much of a block's reward an output takes: a percent of the whole reward, or a fixed amount. Either is
/// capped at what the outputs before it leave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Portion {
    /// Parts per million of the block's reward.
    Ppm(u64),
    Amount(Amount),
}

impl Portion {
    /// What it takes of `reward`, uncapped: floor(R × ppm / 10^6), or its fixed amount.
    pub fn of(self, reward: Amount) -> Amount {
        match self {
            Portion::Ppm(ppm) => Amount::from_sat(
                (u128::from(reward.to_sat()) * u128::from(ppm) / u128::from(PPM)) as u64,
            ),
            Portion::Amount(amount) => amount,
        }
    }
}

#[derive(Debug)]
pub struct Telemetry {
    /// The period each rollup row covers, in whole seconds: a minute live.
    pub rollup_secs: u64,
}

/// What every Stratum connection is held to, whatever its listener.
#[derive(Debug)]
pub struct Stratum {
    /// How long a connection may go without authorizing (V1) or opening a channel (V2) before it is
    /// closed.
    pub authorize_timeout: Duration,
}

/// What every coinbase's scriptSig says after the height.
#[derive(Debug, Clone)]
pub struct Coinbase {
    /// Shown by block explorers as the pool's name.
    pub tag: String,
    /// Whether the tag is followed by `Mined by <worker>/`, naming the worker each job is for: each pool's
    /// unless it sets its own.
    pub worker_names: bool,
    /// Whether it is, for a worker that asks with `+name`: each pool's unless it
    /// sets its own.
    pub worker_names_opt_in: bool,
}

/// The networks one puntohunter process can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Main,
    Regtest,
}

impl Network {
    pub fn bitcoin(self) -> bitcoin::Network {
        match self {
            Network::Main => bitcoin::Network::Bitcoin,
            Network::Regtest => bitcoin::Network::Regtest,
        }
    }

    /// The name getblockchaininfo reports as `chain`, and the name this config uses.
    pub fn chain(self) -> &'static str {
        match self {
            Network::Main => "main",
            Network::Regtest => "regtest",
        }
    }
}

/// The primary node's name in logs and metric labels.
pub const PRIMARY: &str = "primary";

#[derive(Debug)]
pub struct Node {
    /// The node's name in logs and metric labels: [`PRIMARY`] for `[node]`.
    pub name: String,
    pub rpc_url: String,
    pub auth: RpcAuth,
    /// ZMQ endpoint publishing hashblock.
    pub zmq_hashblock: String,
    /// Timeout of every RPC call except the long poll.
    pub rpc_timeout: Duration,
    /// Timeout of the long-polled getblocktemplate.
    pub longpoll_timeout: Duration,
}

/// How long a node may lag before it is stuck, and how long one out of a fault waits before it sources
/// templates again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeChecks {
    pub stuck: Duration,
    pub recovery: Duration,
}

#[derive(Clone)]
pub enum RpcAuth {
    CookieFile(PathBuf),
    UserPassword {
        user: String,
        password: String,
    },
    /// The password is read from the file, which [`Config::load`] refuses when others can read it.
    PasswordFile {
        user: String,
        path: PathBuf,
    },
}

impl fmt::Debug for RpcAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcAuth::CookieFile(path) => f.debug_tuple("CookieFile").field(path).finish(),
            RpcAuth::UserPassword { user, .. } => f
                .debug_struct("UserPassword")
                .field("user", user)
                .finish_non_exhaustive(),
            RpcAuth::PasswordFile { user, path } => f
                .debug_struct("PasswordFile")
                .field("user", user)
                .field("path", path)
                .finish(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metrics {
    /// Address the Prometheus endpoint listens on.
    pub bind: SocketAddr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Portal {
    /// Address the web portal listens on: plain HTTP, for the LAN.
    pub bind: SocketAddr,
    /// Username lookups each client address may make a minute.
    pub lookups_per_minute: u32,
    /// The API's event streams each client address may hold open.
    #[serde(default = "default_streams_per_ip")]
    pub streams_per_ip: u32,
    /// Event streams the pool holds open in all.
    #[serde(default = "default_max_streams")]
    pub max_streams: u32,
    /// The reverse proxy whose `X-Forwarded-For` names the client; unset without one.
    pub trusted_proxy: Option<IpAddr>,
    /// Days an offline device stays on its fleet's page.
    #[serde(default = "default_keep_offline_days")]
    pub keep_offline_days: u32,
    /// The portal's `site.json`, as `web/build.py --site` reads it: its name, description, and optional title
    /// and origin. Unset, `web/site.json`'s.
    pub site: Option<PathBuf>,
    /// A directory laid over `web/src/`, as `web/build.py --overlay` reads it: its files replace the app's of
    /// the same path, or are added.
    pub overlay: Option<PathBuf>,
}

/// The JSON API's settings.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Api {
    /// The origins whose browser pages may call `/api/v1`, each exactly, such as
    /// "https://dashboard.partner.example"; none by default, so the API is same-origin only.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

/// Whether `text` is one exact origin: http or https, a lower-case host, and a port, with no path, wildcard, or
/// credentials, as a browser sends it in `Origin`.
fn is_origin(text: &str) -> bool {
    let host = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"));
    host.is_some_and(|host| {
        !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.:[]".contains(&b))
    })
}

/// API reference section 1.8's defaults: a household's devices and browsers behind one address, and about
/// 120 MB of connections.
fn default_streams_per_ip() -> u32 {
    20
}

fn default_max_streams() -> u32 {
    2000
}

fn default_keep_offline_days() -> u32 {
    7
}

/// Where miners connect, as the overview's connection settings give it. A proxy, a port forward, or a domain
/// name can set it apart from the listeners; each pool sets its own ports.
#[derive(Debug, Clone)]
pub struct Public {
    /// A domain name or an IP address, an IPv6 one in brackets; the request's Host header when absent.
    pub host: Option<String>,
    /// The sites' domains, lower case: a request to one of them or to a name under it is given `stratum.`
    /// before that domain. Empty with `host`.
    pub domains: Vec<String>,
    /// The day, `YYYY-MM-DD`, the SV2 authority key changes to the next key in the data directory, which the
    /// overview shows with it ahead of the switch.
    pub authority_switch: Option<String>,
}

impl Public {
    /// The host the Stratum URLs name for a request that named `request`: `host`, else `stratum.` before the
    /// domain `request` is or is under, else `request` itself.
    pub fn stratum_host(&self, request: Option<&str>) -> String {
        if let Some(host) = &self.host {
            return host.clone();
        }
        let Some(request) = request else {
            return "host".into();
        };
        let request = request.to_ascii_lowercase();
        self.domains
            .iter()
            .filter(|domain| {
                request == **domain
                    || request
                        .strip_suffix(domain.as_str())
                        .is_some_and(|rest| rest.ends_with('.'))
            })
            .max_by_key(|domain| domain.len())
            .map_or(request.clone(), |domain| format!("stratum.{domain}"))
    }
}

/// Share difficulty bounds and aim. Difficulties may be below 1.
#[derive(Debug)]
pub struct Vardiff {
    pub floor: f64,
    pub ceiling: f64,
    pub share_interval: Duration,
}

/// The pool's own address, checked against the configured network.
#[derive(Debug)]
pub struct Addresses {
    /// Takes the miner portion of every pooled block.
    pub pool_wallet: Address,
}

/// A config that cannot be loaded. Every variant names the file's path.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {}: {source}", path.display())]
    Invalid {
        path: PathBuf,
        source: InvalidConfig,
    },
    #[error("node {node}'s rpc_password_file {}: {reason}", file.display())]
    PasswordFile {
        node: String,
        file: PathBuf,
        reason: String,
    },
    #[error("geo.database {}: {source}", file.display())]
    GeoDatabase {
        file: PathBuf,
        source: std::io::Error,
    },
    #[error("portal.site {}: {source}", file.display())]
    PortalSite {
        file: PathBuf,
        source: std::io::Error,
    },
    #[error("portal.overlay {}: not a directory", dir.display())]
    PortalOverlay { dir: PathBuf },
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidConfig {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error("{key}: {reason}")]
    Value { key: &'static str, reason: String },
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config: Config = text.parse().map_err(|source| ConfigError::Invalid {
            path: path.to_owned(),
            source,
        })?;
        for node in std::iter::once(&config.node).chain(&config.secondary_nodes) {
            if let RpcAuth::PasswordFile { path, .. } = &node.auth {
                check_password_file(path).map_err(|reason| ConfigError::PasswordFile {
                    node: node.name.clone(),
                    file: path.clone(),
                    reason,
                })?;
            }
        }
        if let Some(geo) = &config.geo {
            std::fs::File::open(&geo.database).map_err(|source| ConfigError::GeoDatabase {
                file: geo.database.clone(),
                source,
            })?;
        }
        if let Some(site) = &config.portal.site {
            std::fs::File::open(site).map_err(|source| ConfigError::PortalSite {
                file: site.clone(),
                source,
            })?;
        }
        if let Some(overlay) = &config.portal.overlay
            && !overlay.is_dir()
        {
            return Err(ConfigError::PortalOverlay {
                dir: overlay.clone(),
            });
        }
        Ok(config)
    }
}

/// A password file must be readable, hold a password, and be private to its owner, as a key is
/// (`secret.rs`).
fn check_password_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err("readable by others: chmod 600 it".into());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if text.trim().is_empty() {
        return Err("is empty".into());
    }
    Ok(())
}

impl FromStr for Config {
    type Err = InvalidConfig;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let raw: RawConfig = toml::from_str(text)?;
        let network = raw.network;

        let vardiff = raw.vardiff;
        if !(vardiff.floor.is_finite() && vardiff.floor > 0.0) {
            return Err(invalid("vardiff.floor", "must be a positive number"));
        }
        if !vardiff.ceiling.is_finite() {
            return Err(invalid("vardiff.ceiling", "must be a finite number"));
        }
        if vardiff.floor > vardiff.ceiling {
            return Err(invalid(
                "vardiff.floor",
                format!(
                    "{} is above vardiff.ceiling {}",
                    vardiff.floor, vardiff.ceiling
                ),
            ));
        }

        let portal = raw.portal;
        for (key, value) in [
            ("portal.lookups_per_minute", portal.lookups_per_minute),
            ("portal.streams_per_ip", portal.streams_per_ip),
            ("portal.max_streams", portal.max_streams),
            ("portal.keep_offline_days", portal.keep_offline_days),
        ] {
            if value == 0 {
                return Err(invalid(key, "must be positive"));
            }
        }
        if let Some(origin) = raw.api.allowed_origins.iter().find(|o| !is_origin(o)) {
            return Err(invalid(
                "api.allowed_origins",
                format!("{origin:?} is not an exact origin, such as https://dashboard.example"),
            ));
        }
        let fees = fees(raw.fees, network, "fees")?;
        let accounting = accounting(raw.accounting)?;
        let coinbase = Coinbase {
            tag: tag(raw.coinbase.tag)?,
            worker_names: raw.coinbase.worker_names,
            worker_names_opt_in: raw.coinbase.worker_names_opt_in,
        };
        let pools = pools(raw.pools, network, &fees, accounting.window(), &coinbase)?;
        let public = Public {
            host: raw
                .public
                .host
                .as_deref()
                .map(|host| public_host("public.host", host))
                .transpose()?,
            domains: raw
                .public
                .domains
                .iter()
                .map(|domain| match public_host("public.domains", domain)? {
                    domain if domain.parse::<IpAddr>().is_ok() || domain.starts_with('[') => Err(
                        invalid("public.domains", "must be domain names, not IP addresses"),
                    ),
                    domain => Ok(domain),
                })
                .collect::<Result<_, _>>()?,
            authority_switch: raw
                .public
                .authority_switch
                .as_deref()
                .map(day)
                .transpose()?,
        };
        if public.host.is_some() && !public.domains.is_empty() {
            return Err(invalid(
                "public.domains",
                "cannot be set beside public.host, which names one host for every site",
            ));
        }
        let node_checks = NodeChecks {
            stuck: seconds("node.stuck_secs", raw.node.stuck_secs)?,
            recovery: seconds("node.recovery_secs", raw.node.recovery_secs)?,
        };
        let primary = Node {
            name: PRIMARY.to_owned(),
            auth: auth(
                Section::Primary,
                raw.node.cookie_file,
                raw.node.rpc_user,
                raw.node.rpc_password,
                raw.node.rpc_password_file,
            )
            .map_err(|(key, reason)| invalid(key, reason))?,
            rpc_url: raw.node.rpc_url,
            zmq_hashblock: raw.node.zmq_hashblock,
            rpc_timeout: seconds("node.rpc_timeout_secs", raw.node.rpc_timeout_secs)?,
            longpoll_timeout: seconds(
                "node.longpoll_timeout_secs",
                raw.node.longpoll_timeout_secs,
            )?,
        };
        let secondary_nodes = secondary_nodes(raw.secondary_nodes, &primary)?;
        let data_dir = raw
            .data_dir
            .unwrap_or_else(|| Path::new("data").join(network.chain()));
        let backup = raw
            .backup
            .map(|backup| self::backup(backup, &data_dir))
            .transpose()?;

        Ok(Config {
            network,
            data_dir,
            node: primary,
            secondary_nodes,
            node_checks,
            pools,
            public,
            api: raw.api,
            stratum: Stratum {
                authorize_timeout: seconds(
                    "stratum.authorize_timeout_secs",
                    raw.stratum.authorize_timeout_secs,
                )?,
            },
            metrics: raw.metrics,
            portal,
            vardiff: Vardiff {
                floor: vardiff.floor,
                ceiling: vardiff.ceiling,
                share_interval: seconds(
                    "vardiff.share_interval_secs",
                    vardiff.share_interval_secs,
                )?,
            },
            telemetry: Telemetry {
                rollup_secs: match raw.telemetry.rollup_secs {
                    0 => return Err(invalid("telemetry.rollup_secs", "must be positive")),
                    secs => secs,
                },
            },
            addresses: Addresses {
                pool_wallet: address("addresses.pool_wallet", &raw.addresses.pool_wallet, network)?,
            },
            coinbase,
            fees,
            accounting,
            payouts: raw
                .payouts
                .map(|payouts| self::payouts(payouts, network))
                .transpose()?,
            group: raw.group.map(group).transpose()?,
            geo: match raw.geo {
                Some(geo) if geo.min_workers == 0 => {
                    return Err(invalid("geo.min_workers", "must be 1 or more"));
                }
                geo => geo,
            },
            stats: stats(raw.stats)?,
            backup,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackup {
    /// Relative to the data directory; `backups` in it by default.
    dir: Option<PathBuf>,
    #[serde(default = "default_backup_interval_secs")]
    interval_secs: u64,
    #[serde(default = "default_backup_keep")]
    keep: usize,
}

/// A quarter of an hour.
fn default_backup_interval_secs() -> u64 {
    900
}

/// A day of snapshots at the default interval.
fn default_backup_keep() -> usize {
    96
}

fn backup(raw: RawBackup, data_dir: &Path) -> Result<Backup, InvalidConfig> {
    if raw.interval_secs == 0 {
        return Err(invalid("backup.interval_secs", "must be positive"));
    }
    if raw.keep == 0 {
        return Err(invalid("backup.keep", "must be 1 or more"));
    }
    Ok(Backup {
        dir: data_dir.join(raw.dir.unwrap_or_else(|| "backups".into())),
        interval: Duration::from_secs(raw.interval_secs),
        keep: raw.keep,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    network: Network,
    data_dir: Option<PathBuf>,
    node: RawNode,
    #[serde(default)]
    secondary_nodes: Vec<RawSecondaryNode>,
    #[serde(default)]
    pools: Vec<RawPool>,
    #[serde(default)]
    public: RawPublic,
    stratum: RawStratum,
    metrics: Metrics,
    portal: Portal,
    #[serde(default)]
    api: Api,
    vardiff: RawVardiff,
    telemetry: RawTelemetry,
    addresses: RawAddresses,
    #[serde(default)]
    coinbase: RawCoinbase,
    #[serde(default)]
    fees: Vec<RawFee>,
    accounting: RawAccounting,
    payouts: Option<RawPayouts>,
    group: Option<RawGroup>,
    geo: Option<Geo>,
    #[serde(default)]
    stats: Stats,
    backup: Option<RawBackup>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    name: String,
    #[serde(default = "default_refresh_secs")]
    refresh_secs: f64,
    #[serde(default)]
    members: Vec<RawMember>,
    label: Option<String>,
    #[serde(default)]
    show_members: bool,
}

/// As the overview refreshes.
fn default_refresh_secs() -> f64 {
    30.0
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMember {
    name: String,
    url: String,
    label: Option<String>,
    link: Option<String>,
}

/// The most characters an instance's label takes.
const MAX_LABEL: usize = 32;

/// An instance's label, trimmed: 1 to [`MAX_LABEL`] characters, none a control character; required with
/// `show_members`.
fn label(
    key: &'static str,
    whose: &str,
    label: Option<String>,
    required: bool,
) -> Result<Option<String>, InvalidConfig> {
    let Some(label) = label else {
        return match required {
            true => Err(invalid(key, format!("{whose}: show_members takes a label"))),
            false => Ok(None),
        };
    };
    let label = label.trim().to_owned();
    if label.is_empty() || label.chars().count() > MAX_LABEL || label.chars().any(char::is_control)
    {
        return Err(invalid(
            key,
            format!("{whose}: takes 1 to {MAX_LABEL} printable characters"),
        ));
    }
    Ok(Some(label))
}

/// The group: a heading, a refresh, and at least one member, each named and placed uniquely.
fn group(raw: RawGroup) -> Result<Group, InvalidConfig> {
    let name = raw.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > MAX_POOL_NAME {
        return Err(invalid(
            "group.name",
            format!("takes 1 to {MAX_POOL_NAME} characters"),
        ));
    }
    if raw.members.is_empty() {
        return Err(invalid(
            "group.members",
            "list at least one other instance as [[group.members]]",
        ));
    }
    let show_members = raw.show_members;
    let mut members: Vec<Member> = Vec::with_capacity(raw.members.len());
    for (i, raw) in raw.members.into_iter().enumerate() {
        if !is_label(&raw.name) {
            return Err(invalid(
                "group.members.name",
                format!(
                    "entry {}: {:?} takes 1 to {MAX_NODE_NAME} lower-case letters, digits, and '-'",
                    i + 1,
                    raw.name
                ),
            ));
        }
        if !is_origin(&raw.url) {
            return Err(invalid(
                "group.members.url",
                format!(
                    "member {}: {:?} is not an exact origin, such as https://east.example.com",
                    raw.name, raw.url
                ),
            ));
        }
        if members.iter().any(|member| member.name == raw.name) {
            return Err(invalid(
                "group.members.name",
                format!("member {}: the name is used twice", raw.name),
            ));
        }
        if members.iter().any(|member| member.url == raw.url) {
            return Err(invalid(
                "group.members.url",
                format!("member {}: the url is used twice", raw.name),
            ));
        }
        if let Some(link) = &raw.link
            && !is_origin(link)
        {
            return Err(invalid(
                "group.members.link",
                format!(
                    "member {}: {link:?} is not an exact origin, such as https://east.example.com",
                    raw.name
                ),
            ));
        }
        let label = label(
            "group.members.label",
            &format!("member {}", raw.name),
            raw.label,
            show_members,
        )?;
        members.push(Member {
            name: raw.name,
            url: raw.url,
            label,
            link: raw.link,
        });
    }
    Ok(Group {
        name,
        refresh: seconds("group.refresh_secs", raw.refresh_secs)?,
        members,
        label: label("group.label", "this instance", raw.label, show_members)?,
        show_members,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAccounting {
    #[serde(default = "default_window_multiplier")]
    window_multiplier: f64,
    window_min_work: f64,
    #[serde(default)]
    maturity_buffer: u32,
}

/// The default PPLNS window: twice the network difficulty.
fn default_window_multiplier() -> f64 {
    2.0
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayouts {
    wallet: String,
    threshold_sats: u64,
    interval_secs: f64,
    outputs_per_batch: u32,
    fee_rate: Option<f64>,
    confirmation_target: Option<u16>,
    fee_rate_cap: f64,
}

fn payouts(raw: RawPayouts, network: Network) -> Result<Payouts, InvalidConfig> {
    // A name the wallet's URL path takes as it is.
    let name = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if raw.wallet.is_empty() || raw.wallet.len() > 64 || !raw.wallet.chars().all(name) {
        return Err(invalid(
            "payouts.wallet",
            "must be 1 to 64 letters, digits, '-', or '_'",
        ));
    }
    let threshold = Amount::from_sat(raw.threshold_sats);
    if threshold < P2TR_DUST {
        return Err(invalid(
            "payouts.threshold_sats",
            format!(
                "must be at least the P2TR dust limit, {} sats",
                P2TR_DUST.to_sat()
            ),
        ));
    }
    if !(1..=MAX_BATCH_OUTPUTS).contains(&raw.outputs_per_batch) {
        return Err(invalid(
            "payouts.outputs_per_batch",
            format!("must be 1 to {MAX_BATCH_OUTPUTS}"),
        ));
    }
    if !(raw.fee_rate_cap.is_finite() && raw.fee_rate_cap > 0.0) {
        return Err(invalid(
            "payouts.fee_rate_cap",
            "must be a positive number of sat/vB",
        ));
    }
    // Regtest has no fee estimates, so it pays a fixed rate; mainnet pays the node's estimate.
    let rate = match (network, raw.fee_rate, raw.confirmation_target) {
        (Network::Regtest, Some(rate), None) => {
            if !(rate.is_finite() && rate > 0.0 && rate <= raw.fee_rate_cap) {
                return Err(invalid(
                    "payouts.fee_rate",
                    "must be a positive number of sat/vB, at most fee_rate_cap",
                ));
            }
            Rate::Fixed(rate)
        }
        (Network::Regtest, None, _) => {
            return Err(invalid(
                "payouts.fee_rate",
                "regtest has no fee estimates: set the sat/vB a batch pays",
            ));
        }
        (Network::Regtest, Some(_), Some(_)) => {
            return Err(invalid(
                "payouts.confirmation_target",
                "regtest has no fee estimates: a batch pays fee_rate",
            ));
        }
        (Network::Main, None, Some(target)) => {
            if !CONFIRMATION_TARGETS.contains(&target) {
                return Err(invalid(
                    "payouts.confirmation_target",
                    format!(
                        "must be {} to {} blocks",
                        CONFIRMATION_TARGETS.start(),
                        CONFIRMATION_TARGETS.end()
                    ),
                ));
            }
            Rate::Estimate(target)
        }
        (Network::Main, Some(_), _) => {
            return Err(invalid(
                "payouts.fee_rate",
                "mainnet pays the node's fee estimate: set confirmation_target instead",
            ));
        }
        (Network::Main, None, None) => {
            return Err(invalid(
                "payouts.confirmation_target",
                "mainnet pays the node's fee estimate for confirmation within this many blocks",
            ));
        }
    };
    Ok(Payouts {
        wallet: raw.wallet,
        threshold,
        interval: seconds("payouts.interval_secs", raw.interval_secs)?,
        outputs_per_batch: raw.outputs_per_batch,
        rate,
        fee_rate_cap: raw.fee_rate_cap,
    })
}

fn accounting(raw: RawAccounting) -> Result<Accounting, InvalidConfig> {
    let keys = ["accounting.window_multiplier", "accounting.window_min_work"];
    let window = window(raw.window_multiplier, raw.window_min_work)
        .map_err(|(i, reason)| invalid(keys[i], reason))?;
    Ok(Accounting {
        window_multiplier: window.window_multiplier,
        window_min_work: window.window_min_work,
        maturity_buffer: raw.maturity_buffer,
    })
}

/// A window policy from k and W_min: k positive, and W_min zero or positive. A refusal names the key by its
/// index, 0 for k and 1 for W_min.
fn window(multiplier: f64, min_work: f64) -> Result<Window, (usize, &'static str)> {
    if !(multiplier.is_finite() && multiplier > 0.0) {
        return Err((0, "must be a positive number"));
    }
    if !(min_work.is_finite() && min_work >= 0.0) {
        return Err((1, "must be zero or a positive number"));
    }
    Ok(Window {
        window_multiplier: multiplier,
        window_min_work: min_work,
    })
}

/// A pool's own window policy (`[pools.accounting]`), each key falling back to the global `[accounting]`. A
/// global-only key, such as `maturity_buffer`, is refused as unknown.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    window_multiplier: Option<f64>,
    window_min_work: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNode {
    rpc_url: String,
    cookie_file: Option<PathBuf>,
    rpc_user: Option<String>,
    rpc_password: Option<String>,
    rpc_password_file: Option<PathBuf>,
    zmq_hashblock: String,
    rpc_timeout_secs: f64,
    longpoll_timeout_secs: f64,
    #[serde(default = "default_stuck_secs")]
    stuck_secs: f64,
    #[serde(default = "default_recovery_secs")]
    recovery_secs: f64,
}

/// Measured on mainnet: two nodes on separate networks reported each block within 1.3 s of each other.
fn default_stuck_secs() -> f64 {
    30.0
}

fn default_recovery_secs() -> f64 {
    60.0
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecondaryNode {
    name: String,
    rpc_url: String,
    cookie_file: Option<PathBuf>,
    rpc_user: Option<String>,
    rpc_password: Option<String>,
    rpc_password_file: Option<PathBuf>,
    zmq_hashblock: String,
    /// `[node]`'s unless set.
    rpc_timeout_secs: Option<f64>,
    longpoll_timeout_secs: Option<f64>,
}

/// The table a node's keys are in, for the key an error names.
#[derive(Clone, Copy)]
enum Section {
    Primary,
    Secondary,
}

impl Section {
    fn key(self, field: &str) -> &'static str {
        match (self, field) {
            (Section::Primary, "cookie_file") => "node.cookie_file",
            (Section::Primary, "rpc_user") => "node.rpc_user",
            (Section::Primary, "rpc_password") => "node.rpc_password",
            (Section::Secondary, "cookie_file") => "secondary_nodes.cookie_file",
            (Section::Secondary, "rpc_user") => "secondary_nodes.rpc_user",
            (Section::Secondary, "rpc_password") => "secondary_nodes.rpc_password",
            _ => unreachable!("no key {field}"),
        }
    }
}

/// A node's credentials: its cookie file, or a user with a password or a password file. Returns the key and
/// reason of a refusal.
fn auth(
    section: Section,
    cookie_file: Option<PathBuf>,
    user: Option<String>,
    password: Option<String>,
    password_file: Option<PathBuf>,
) -> Result<RpcAuth, (&'static str, &'static str)> {
    const ONE_FORM: &str =
        "set one of cookie_file, rpc_user and rpc_password, or rpc_user and rpc_password_file";
    match (cookie_file, user, password, password_file) {
        (Some(path), None, None, None) => Ok(RpcAuth::CookieFile(path)),
        (None, Some(user), Some(password), None) => Ok(RpcAuth::UserPassword { user, password }),
        (None, Some(user), None, Some(path)) => Ok(RpcAuth::PasswordFile { user, path }),
        (Some(_), ..) => Err((section.key("cookie_file"), ONE_FORM)),
        (None, Some(_), None, None) => Err((
            section.key("rpc_password"),
            "rpc_user takes rpc_password or rpc_password_file",
        )),
        (None, Some(_), Some(_), Some(_)) => Err((section.key("rpc_password"), ONE_FORM)),
        (None, None, ..) => Err((section.key("rpc_user"), ONE_FORM)),
    }
}

/// The secondary nodes, each named uniquely and never [`PRIMARY`], with `primary`'s timeouts unless set.
fn secondary_nodes(raw: Vec<RawSecondaryNode>, primary: &Node) -> Result<Vec<Node>, InvalidConfig> {
    let mut nodes: Vec<Node> = Vec::with_capacity(raw.len());
    for (i, raw) in raw.into_iter().enumerate() {
        if !is_label(&raw.name) {
            return Err(invalid(
                "secondary_nodes.name",
                format!(
                    "entry {}: {:?} takes 1 to {MAX_NODE_NAME} lower-case letters, digits, and '-'",
                    i + 1,
                    raw.name
                ),
            ));
        }
        let name = raw.name;
        let refuse = |key, reason: &str| invalid(key, format!("node {name}: {reason}"));
        if name == PRIMARY || nodes.iter().any(|node| node.name == name) {
            return Err(refuse(
                "secondary_nodes.name",
                "the name is taken; `primary` is [node]'s",
            ));
        }
        let auth = auth(
            Section::Secondary,
            raw.cookie_file,
            raw.rpc_user,
            raw.rpc_password,
            raw.rpc_password_file,
        )
        .map_err(|(key, reason)| refuse(key, reason))?;
        let timeout = |key, secs: Option<f64>, default| match secs {
            Some(secs) => {
                seconds(key, secs).map_err(|_| refuse(key, "must be a positive number of seconds"))
            }
            None => Ok(default),
        };
        nodes.push(Node {
            rpc_url: raw.rpc_url,
            auth,
            zmq_hashblock: raw.zmq_hashblock,
            rpc_timeout: timeout(
                "secondary_nodes.rpc_timeout_secs",
                raw.rpc_timeout_secs,
                primary.rpc_timeout,
            )?,
            longpoll_timeout: timeout(
                "secondary_nodes.longpoll_timeout_secs",
                raw.longpoll_timeout_secs,
                primary.longpoll_timeout,
            )?,
            name,
        });
    }
    Ok(nodes)
}

/// The longest node name, as a pool id's.
const MAX_NODE_NAME: usize = 32;

/// Whether `name` fits a node's or a group member's name, a metric label: 1 to [`MAX_NODE_NAME`] lower-case
/// letters, digits, and '-'.
fn is_label(name: &str) -> bool {
    (1..=MAX_NODE_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStratum {
    authorize_timeout_secs: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVardiff {
    floor: f64,
    ceiling: f64,
    share_interval_secs: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTelemetry {
    rollup_secs: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAddresses {
    pool_wallet: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCoinbase {
    tag: Option<String>,
    #[serde(default)]
    worker_names: bool,
    #[serde(default)]
    worker_names_opt_in: bool,
}

/// A pool's own `[pools.coinbase]`: either name flag, each falling back to `[coinbase]`'s. The tag stays global.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPoolCoinbase {
    worker_names: Option<bool>,
    worker_names_opt_in: Option<bool>,
}

/// The coinbase tag: printable ASCII, so every explorer shows it as written, and short enough that a worker's
/// name fits after it.
fn tag(tag: Option<String>) -> Result<String, InvalidConfig> {
    let tag = tag.unwrap_or_else(|| DEFAULT_TAG.to_owned());
    if tag.is_empty()
        || tag.len() > MAX_TAG_LEN
        || !tag.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
    {
        return Err(invalid(
            "coinbase.tag",
            format!("takes 1 to {MAX_TAG_LEN} printable ASCII characters"),
        ));
    }
    Ok(tag)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPublic {
    host: Option<String>,
    #[serde(default)]
    domains: Vec<String>,
    authority_switch: Option<String>,
}

/// `text` as `public.authority_switch`: a day, `YYYY-MM-DD`.
fn day(text: &str) -> Result<String, InvalidConfig> {
    let parts: Vec<&str> = text.split('-').collect();
    let number = |part: &str, len: usize, range: std::ops::RangeInclusive<u32>| {
        part.len() == len
            && part.chars().all(|c| c.is_ascii_digit())
            && part.parse().is_ok_and(|n| range.contains(&n))
    };
    match parts[..] {
        [year, month, day]
            if number(year, 4, 2000..=9999)
                && number(month, 2, 1..=12)
                && number(day, 2, 1..=31) =>
        {
            Ok(text.to_owned())
        }
        _ => Err(invalid(
            "public.authority_switch",
            "must be a day, YYYY-MM-DD",
        )),
    }
}

/// `text` as a Stratum URL's host: an IP address, an IPv6 one written in brackets, or a domain name of letters,
/// digits, and hyphens in dot-separated labels.
fn public_host(key: &'static str, text: &str) -> Result<String, InvalidConfig> {
    let unbracketed = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(text);
    match unbracketed.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if unbracketed == text => return Ok(ip.to_string()),
        Ok(IpAddr::V6(ip)) => return Ok(format!("[{ip}]")),
        _ => {}
    }
    let label = |label: &str| {
        (1..=63).contains(&label.len())
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    };
    if text.len() <= 253 && text.split('.').all(label) {
        Ok(text.to_ascii_lowercase())
    } else {
        Err(invalid(
            key,
            "must be a domain name or an IP address, with no scheme or port",
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFee {
    name: String,
    address: String,
    /// The fee's portion, as [`RawPortion`] takes it.
    percent: Option<f64>,
    btc: Option<f64>,
    sats: Option<f64>,
}

/// A portion's keys, of which exactly one is set: `percent` of each block's reward, or a fixed amount as `btc`
/// or as whole `sats`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPortion {
    pub percent: Option<f64>,
    pub btc: Option<f64>,
    pub sats: Option<f64>,
}

/// The longest fee name the portal shows.
const MAX_FEE_NAME: usize = 64;

/// The fee outputs under `key`, each named once, on the network, with an exact portion; their percents
/// together below the whole reward, and each fixed amount above its address's dust limit.
fn fees(raw: Vec<RawFee>, network: Network, key: &'static str) -> Result<Vec<Fee>, InvalidConfig> {
    let mut fees: Vec<Fee> = Vec::with_capacity(raw.len());
    for (i, fee) in raw.into_iter().enumerate() {
        let entry = |reason: String| invalid(key, format!("entry {}: {reason}", i + 1));
        let name = fee.name.trim().to_owned();
        if name.is_empty() || name.chars().count() > MAX_FEE_NAME {
            return Err(entry(format!("name takes 1 to {MAX_FEE_NAME} characters")));
        }
        if fees.iter().any(|other| other.name == name) {
            return Err(entry(format!("name {name:?} is used twice")));
        }
        let address =
            address("address", &fee.address, network).map_err(|e| entry(format!("{name}: {e}")))?;
        let raw = RawPortion {
            percent: fee.percent,
            btc: fee.btc,
            sats: fee.sats,
        };
        let portion = portion(raw, false).map_err(|e| entry(format!("{name}: {e}")))?;
        let dust = address.script_pubkey().minimal_non_dust();
        if matches!(portion, Portion::Amount(amount) if amount < dust) {
            return Err(entry(format!(
                "{name}: the amount is below its address's dust limit of {} sats",
                dust.to_sat()
            )));
        }
        fees.push(Fee {
            name,
            address,
            portion,
        });
    }
    let total = percents(&fees);
    if total >= PPM {
        return Err(invalid(
            key,
            format!(
                "percents total {}%, leaving the miner nothing",
                total as f64 / 1e4
            ),
        ));
    }
    Ok(fees)
}

/// The fees' percents together, in parts per million; fixed amounts aside.
fn percents(fees: &[Fee]) -> u64 {
    fees.iter()
        .filter_map(|fee| match fee.portion {
            Portion::Ppm(ppm) => Some(ppm),
            Portion::Amount(_) => None,
        })
        .sum()
}

/// One `[[pools]]` entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPool {
    id: String,
    name: Option<String>,
    v1: Option<SocketAddr>,
    v2: Option<SocketAddr>,
    public_v1: Option<u16>,
    public_v2: Option<u16>,
    finder: RawPortion,
    /// The range each worker may choose its own finder part in.
    personal: Option<RawPersonal>,
    /// The pool's own fee outputs, in place of the global `[[fees]]`; `fees = []` for none.
    fees: Option<Vec<RawFee>>,
    accounting: Option<RawWindow>,
    coinbase: Option<RawPoolCoinbase>,
}

/// A pool's `personal` range: each bound a portion, 0% and 100% when left out.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPersonal {
    min: Option<RawPortion>,
    max: Option<RawPortion>,
}

/// The longest pool name the portal shows.
const MAX_POOL_NAME: usize = 64;

/// The pools, in config order, by the pool rules, each with its own fees or `global`, and its own
/// window policy, each key its own or `global_window`'s.
fn pools(
    raw: Vec<RawPool>,
    network: Network,
    global: &[Fee],
    global_window: Window,
    global_coinbase: &Coinbase,
) -> Result<Vec<Arc<Pool>>, InvalidConfig> {
    if raw.is_empty() {
        return Err(invalid(
            "pools",
            "configure at least one pool, as a [[pools]] entry",
        ));
    }
    let mut pools: Vec<Arc<Pool>> = Vec::with_capacity(raw.len());
    for (i, raw) in raw.into_iter().enumerate() {
        let id: PoolId = raw
            .id
            .parse()
            .map_err(|e| invalid("pools.id", format!("entry {}: {:?} {e}", i + 1, raw.id)))?;
        let refuse = |key, reason: String| invalid(key, format!("pool {id}: {reason}"));
        if pools.iter().any(|pool| pool.id == id) {
            return Err(refuse("pools.id", "the id is used twice".into()));
        }
        let name = raw
            .name
            .as_deref()
            .map_or(id.as_str(), str::trim)
            .to_owned();
        if name.is_empty() || name.chars().count() > MAX_POOL_NAME {
            return Err(refuse(
                "pools.name",
                format!("takes 1 to {MAX_POOL_NAME} characters"),
            ));
        }
        if raw.v1.is_none() && raw.v2.is_none() {
            return Err(refuse(
                "pools.v1",
                "set v1, v2, or both: a pool needs a listener".into(),
            ));
        }
        for (key, bind) in [("pools.v1", raw.v1), ("pools.v2", raw.v2)] {
            let Some(bind) = bind else { continue };
            let other = pools
                .iter()
                .flat_map(|pool| [pool.v1, pool.v2])
                .chain([raw.v1.filter(|_| key == "pools.v2")])
                .flatten()
                .find(|other| shares_port(*other, bind));
            if let Some(other) = other {
                return Err(refuse(
                    key,
                    format!("{bind} takes the port of another listener, {other}"),
                ));
            }
        }
        let port = |key, port: Option<u16>, bind: Option<SocketAddr>| match (port, bind) {
            (Some(_), None) => Err(refuse(key, "is refused without its listener".into())),
            (Some(0), Some(_)) => Err(refuse(key, "must be a port number".into())),
            (Some(port), Some(_)) => Ok(Some(port)),
            (None, bind) => Ok(bind.map(|bind| bind.port())),
        };
        let public_v1 = port("pools.public_v1", raw.public_v1, raw.v1)?;
        let public_v2 = port("pools.public_v2", raw.public_v2, raw.v2)?;
        let fees = match raw.fees {
            Some(own) => self::fees(own, network, "pools.fees")
                .map_err(|e| refuse("pools.fees", e.to_string()))?,
            None => global.to_vec(),
        };
        let finder = finder(raw.finder, &fees).map_err(|e| refuse("pools.finder", e))?;
        let personal = raw
            .personal
            .map(|range| personal(range, finder))
            .transpose()
            .map_err(|(key, e)| refuse(key, e))?;
        let own = raw.accounting.unwrap_or(RawWindow {
            window_multiplier: None,
            window_min_work: None,
        });
        let window = self::window(
            own.window_multiplier
                .unwrap_or(global_window.window_multiplier),
            own.window_min_work.unwrap_or(global_window.window_min_work),
        )
        .map_err(|(i, reason)| {
            let keys = [
                "pools.accounting.window_multiplier",
                "pools.accounting.window_min_work",
            ];
            refuse(keys[i], reason.into())
        })?;
        let own = raw.coinbase.unwrap_or(RawPoolCoinbase {
            worker_names: None,
            worker_names_opt_in: None,
        });
        let worker_names = own.worker_names.unwrap_or(global_coinbase.worker_names);
        let worker_names_opt_in = own
            .worker_names_opt_in
            .unwrap_or(global_coinbase.worker_names_opt_in);
        if worker_names && worker_names_opt_in {
            return Err(refuse(
                "pools.coinbase.worker_names_opt_in",
                "worker_names already writes every worker's name; set worker_names = false on the pool to leave \
                 it to each worker"
                    .into(),
            ));
        }
        pools.push(Arc::new(Pool {
            id,
            name,
            v1: raw.v1,
            v2: raw.v2,
            public_v1,
            public_v2,
            finder,
            fees,
            window,
            personal,
            worker_names,
            worker_names_opt_in,
        }));
    }
    Ok(pools)
}

/// Whether listeners on `a` and `b` would take one port: the same port on the same address, or on any address
/// where either listens on every one.
fn shares_port(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

/// A pool's finder part: a portion that may be 0, a fixed amount 0 or at least the dust limit of P2TR, the
/// costliest common script a finder may mine to, and a percent either 100 or below 100% with the pool's
/// `fees`, so the pool either always pays a pool output or never does.
fn finder(raw: RawPortion, fees: &[Fee]) -> Result<Portion, String> {
    let finder = portion(raw, true)?;
    match finder {
        Portion::Ppm(PPM) => {}
        Portion::Ppm(ppm) => {
            let total = ppm + percents(fees);
            if total >= PPM {
                return Err(format!(
                    "percent and the fees' percents total {}%, which would never pay a pool output: set \
                     percent = 100 to pay the finder all of every block",
                    total as f64 / 1e4
                ));
            }
        }
        Portion::Amount(amount) => {
            let dust = p2tr_dust();
            if amount > Amount::ZERO && amount < dust {
                return Err(format!(
                    "a fixed amount above 0 is at least P2TR's dust limit of {} sats",
                    dust.to_sat()
                ));
            }
        }
    }
    Ok(finder)
}

/// A pool's `personal` range: each bound a portion that may be 0, a fixed one 0 or
/// at least P2TR's dust limit; bounds in one form in order, with a `finder` in their form between them. Bounds
/// in two forms are compared per job, where the clamp lets `max` win.
fn personal(raw: RawPersonal, finder: Portion) -> Result<Personal, (&'static str, String)> {
    let bound = |raw: Option<RawPortion>, key: &'static str, default: Portion| {
        let Some(raw) = raw else {
            return Ok(default);
        };
        let bound = portion(raw, true).map_err(|e| (key, e))?;
        let dust = p2tr_dust();
        match bound {
            Portion::Amount(amount) if amount > Amount::ZERO && amount < dust => Err((
                key,
                format!(
                    "a fixed amount above 0 is at least P2TR's dust limit of {} sats",
                    dust.to_sat()
                ),
            )),
            _ => Ok(bound),
        }
    };
    let min = bound(raw.min, "pools.personal.min", Portion::Ppm(0))?;
    let max = bound(raw.max, "pools.personal.max", Portion::Ppm(PPM))?;
    // A total order within one form: parts per million, or satoshis.
    let rank = |portion: Portion| match portion {
        Portion::Ppm(ppm) => (0, ppm),
        Portion::Amount(amount) => (1, amount.to_sat()),
    };
    let same = |a: Portion, b: Portion| rank(a).0 == rank(b).0;
    if same(min, max) && rank(min) > rank(max) {
        return Err(("pools.personal", "min is above max".into()));
    }
    let below = same(finder, min) && rank(finder) < rank(min);
    let above = same(finder, max) && rank(finder) > rank(max);
    if below || above {
        return Err((
            "pools.finder",
            "lies outside personal's range of min and max, compared in their form".into(),
        ));
    }
    Ok(Personal { min, max })
}

/// The dust limit of a P2TR output: a version-1 witness program of 32 bytes.
fn p2tr_dust() -> Amount {
    ScriptBuf::from_bytes([&[0x51, 0x20][..], &[0; 32]].concat()).minimal_non_dust()
}

/// A portion from its keys: exactly one of `percent`, a percentage of at most 100 to four decimal places;
/// `btc`, an amount to the satoshi; and `sats`, a whole number of satoshis. Either amount is at most 21
/// million BTC. A portion of 0 is refused unless `zero` allows it.
pub(crate) fn portion(raw: RawPortion, zero: bool) -> Result<Portion, String> {
    let portion = match (raw.percent, raw.btc, raw.sats) {
        (Some(percent), None, None) => {
            // A percentage to four decimal places is a whole number of parts per million.
            let ppm = percent * (PPM as f64 / 100.0);
            if !(ppm.is_finite()
                && (0.0..=PPM as f64).contains(&ppm)
                && (ppm - ppm.round()).abs() < 1e-6)
            {
                return Err(format!(
                    "percent must be from 0 to 100, to at most four decimal places, got {percent}"
                ));
            }
            Portion::Ppm(ppm.round() as u64)
        }
        (None, Some(btc), None) => match Amount::from_btc(btc) {
            Ok(amount) if amount <= Amount::MAX_MONEY => Portion::Amount(amount),
            _ => {
                return Err(format!(
                    "btc must be an amount of at most 21 million, to eight decimal places, got {btc}"
                ));
            }
        },
        (None, None, Some(sats)) => {
            if !(sats.is_finite()
                && sats >= 0.0
                && sats.fract() == 0.0
                && sats <= Amount::MAX_MONEY.to_sat() as f64)
            {
                return Err(format!(
                    "sats must be a whole number of satoshis, at most 21 million BTC's, got {sats}"
                ));
            }
            Portion::Amount(Amount::from_sat(sats as u64))
        }
        _ => return Err("takes exactly one of percent, btc, and sats".to_owned()),
    };
    if !zero && matches!(portion, Portion::Ppm(0) | Portion::Amount(Amount::ZERO)) {
        return Err("must be above 0".to_owned());
    }
    Ok(portion)
}

fn invalid(key: &'static str, reason: impl Into<String>) -> InvalidConfig {
    InvalidConfig::Value {
        key,
        reason: reason.into(),
    }
}

fn seconds(key: &'static str, secs: f64) -> Result<Duration, InvalidConfig> {
    match Duration::try_from_secs_f64(secs) {
        Ok(d) if !d.is_zero() => Ok(d),
        _ => Err(invalid(key, "must be a positive number of seconds")),
    }
}

fn address(key: &'static str, text: &str, network: Network) -> Result<Address, InvalidConfig> {
    let address: Address<NetworkUnchecked> = text
        .parse()
        .map_err(|e| invalid(key, format!("not a Bitcoin address: {e}")))?;
    address
        .require_network(network.bitcoin())
        .map_err(|_| invalid(key, format!("not an address on {}", network.chain())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checked_in(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config")
            .join(name)
    }

    fn table(name: &str) -> toml::Table {
        std::fs::read_to_string(checked_in(name))
            .unwrap()
            .parse()
            .unwrap()
    }

    fn section<'a>(table: &'a mut toml::Table, name: &str) -> &'a mut toml::Table {
        table[name].as_table_mut().unwrap()
    }

    fn parse(table: &toml::Table) -> Result<Config, InvalidConfig> {
        table.to_string().parse()
    }

    fn rejected_key(result: Result<Config, InvalidConfig>) -> &'static str {
        match result {
            Err(InvalidConfig::Value { key, .. }) => key,
            other => panic!("expected an invalid value, got {other:?}"),
        }
    }

    #[test]
    fn checked_in_configs_validate() {
        assert_eq!(
            Config::load(&checked_in("mainnet.toml")).unwrap().network,
            Network::Main
        );
        assert_eq!(
            Config::load(&checked_in("regtest.toml")).unwrap().network,
            Network::Regtest
        );
    }

    #[test]
    fn an_old_config_is_refused_as_unknown() {
        let mut config = table("regtest.toml");
        let listeners = toml::Table::from_iter([("solo_v1".to_owned(), "0.0.0.0:3333".into())]);
        config.insert("listeners".into(), listeners.into());
        assert!(matches!(parse(&config), Err(InvalidConfig::Toml(_))));

        let mut config = table("regtest.toml");
        let finder = toml::Table::from_iter([("share".to_owned(), 33.33.into())]);
        let hybrid = toml::Table::from_iter([("finder".to_owned(), finder.into())]);
        config.insert("hybrid".into(), hybrid.into());
        assert!(matches!(parse(&config), Err(InvalidConfig::Toml(_))));

        let mut config = table("regtest.toml");
        section(&mut config, "public").insert("solo_v1".into(), 443.into());
        assert!(matches!(parse(&config), Err(InvalidConfig::Toml(_))));

        let mut config = table("regtest.toml");
        fee_entries(&mut config)[0]
            .as_table_mut()
            .unwrap()
            .insert("share".into(), 0.9.into());
        assert!(matches!(parse(&config), Err(InvalidConfig::Toml(_))));
    }

    /// The config's fee entries, to edit.
    fn fee_entries(table: &mut toml::Table) -> &mut Vec<toml::Value> {
        table["fees"].as_array_mut().unwrap()
    }

    fn fee(name: &str, address: &toml::Value, percent: f64) -> toml::Value {
        toml::Value::Table(toml::Table::from_iter([
            ("name".to_owned(), name.into()),
            ("address".to_owned(), address.clone()),
            ("percent".to_owned(), percent.into()),
        ]))
    }

    /// A fee of a fixed `btc` per block.
    fn fixed_fee(name: &str, address: &toml::Value, btc: f64) -> toml::Value {
        toml::Value::Table(toml::Table::from_iter([
            ("name".to_owned(), name.into()),
            ("address".to_owned(), address.clone()),
            ("btc".to_owned(), btc.into()),
        ]))
    }

    #[test]
    fn rejects_address_for_other_network() {
        let mut mainnet = table("mainnet.toml");
        let mut regtest = table("regtest.toml");
        let regtest_fee = fee_entries(&mut regtest)[0].clone();
        let regtest_wallet = regtest["addresses"]["pool_wallet"].clone();

        mainnet.insert("fees".into(), vec![regtest_fee].into());
        assert_eq!(rejected_key(parse(&mainnet)), "fees");
        let mut mainnet = table("mainnet.toml");

        section(&mut mainnet, "addresses").insert("pool_wallet".into(), regtest_wallet);
        assert_eq!(rejected_key(parse(&mainnet)), "addresses.pool_wallet");
    }

    #[test]
    fn reads_the_checked_in_fees_in_order_as_percents_in_parts_per_million() {
        let config = Config::load(&checked_in("regtest.toml")).unwrap();
        let fees: Vec<(&str, Portion)> = config
            .fees
            .iter()
            .map(|fee| (fee.name.as_str(), fee.portion))
            .collect();
        assert_eq!(
            fees,
            [
                ("operator", Portion::Ppm(9_000)),
                ("partner", Portion::Ppm(6_000))
            ]
        );
    }

    #[test]
    fn takes_no_fees_or_many_and_checks_each() {
        let mut config = table("regtest.toml");
        let address = fee_entries(&mut config)[0]["address"].clone();
        config.remove("fees");
        assert!(
            parse(&config).unwrap().fees.is_empty(),
            "none: the miner takes it all"
        );

        let many: Vec<toml::Value> = (0..40)
            .map(|i| fee(&format!("receiver {i}"), &address, 0.0125))
            .collect();
        config.insert("fees".into(), many.into());
        let fees = parse(&config).unwrap().fees;
        assert_eq!(fees.len(), 40);
        assert!(fees.iter().all(|fee| fee.portion == Portion::Ppm(125)));

        for bad in [
            vec![fee("", &address, 1.0)],
            vec![fee("same", &address, 1.0), fee("same", &address, 1.0)],
            vec![fee("zero", &address, 0.0)],
            vec![fee("negative", &address, -1.0)],
            vec![fee("too fine", &address, 0.00001)],
            vec![fee("all", &address, 100.0)],
            vec![
                fee("half", &address, 50.0),
                fee("other half", &address, 50.0),
            ],
        ] {
            config.insert("fees".into(), bad.clone().into());
            assert_eq!(rejected_key(parse(&config)), "fees", "{bad:?}");
        }
    }

    #[test]
    fn takes_a_fee_as_a_percent_or_a_fixed_amount() {
        let mut config = table("regtest.toml");
        // The hybrid pool's finder percent would take the 99% fee past 100%.
        without_pool(&mut config, "hybrid");
        let address = fee_entries(&mut config)[0]["address"].clone();
        // Fixed amounts do not count toward the percents' 100%, which a fixed fee could not know the part of.
        let mixed = vec![
            fee("operator", &address, 99.0),
            fixed_fee("partner", &address, 0.01),
            fixed_fee("smallest", &address, 0.00000294),
            fixed_fee("all of it", &address, 21_000_000.0),
        ];
        config.insert("fees".into(), mixed.into());
        let portions: Vec<Portion> = parse(&config)
            .unwrap()
            .fees
            .iter()
            .map(|fee| fee.portion)
            .collect();
        assert_eq!(
            portions,
            [
                Portion::Ppm(990_000),
                Portion::Amount(Amount::from_sat(1_000_000)),
                Portion::Amount(Amount::from_sat(294)),
                Portion::Amount(Amount::MAX_MONEY),
            ]
        );

        let mut both = fee("both", &address, 1.0);
        both.as_table_mut()
            .unwrap()
            .insert("btc".into(), 0.01.into());
        let mut neither = fee("neither", &address, 1.0);
        neither.as_table_mut().unwrap().remove("percent");
        for bad in [
            both,
            neither,
            fixed_fee("zero", &address, 0.0),
            fixed_fee("negative", &address, -0.01),
            fixed_fee("too fine", &address, 0.000000001),
            fixed_fee("too much", &address, 21_000_000.01),
            // The regtest addresses are P2WPKH, whose outputs are dust below 294 sats.
            fixed_fee("dust", &address, 0.00000293),
        ] {
            config.insert("fees".into(), vec![bad.clone()].into());
            assert_eq!(rejected_key(parse(&config)), "fees", "{bad:?}");
        }
    }

    /// The config's pool entries, to edit.
    fn pool_entries(table: &mut toml::Table) -> &mut Vec<toml::Value> {
        table["pools"].as_array_mut().unwrap()
    }

    /// The entry of pool `id`, to edit.
    fn pool<'a>(table: &'a mut toml::Table, id: &str) -> &'a mut toml::Table {
        pool_entries(table)
            .iter_mut()
            .map(|entry| entry.as_table_mut().unwrap())
            .find(|entry| entry["id"].as_str() == Some(id))
            .unwrap()
    }

    fn without_pool(config: &mut toml::Table, id: &str) {
        pool_entries(config).retain(|entry| entry["id"].as_str() != Some(id));
    }

    /// Sets pool `id`'s finder to the one portion `key = value`.
    fn finder(config: &mut toml::Table, id: &str, key: &str, value: toml::Value) {
        let finder = toml::Table::from_iter([(key.to_owned(), value)]);
        pool(config, id).insert("finder".into(), finder.into());
    }

    #[test]
    fn the_checked_in_configs_hold_the_three_default_pools() {
        for name in ["mainnet.toml", "regtest.toml"] {
            let config = Config::load(&checked_in(name)).unwrap();
            let pools: Vec<_> = config
                .pools
                .iter()
                .map(|pool| {
                    (
                        pool.id.as_str(),
                        pool.name.as_str(),
                        pool.v1.map(|bind| bind.port()),
                        pool.v2.map(|bind| bind.port()),
                        (pool.public_v1, pool.public_v2),
                        pool.finder,
                    )
                })
                .collect();
            assert_eq!(
                pools,
                [
                    (
                        "solo",
                        "Solo",
                        Some(3333),
                        Some(3334),
                        (Some(3333), Some(3334)),
                        Portion::Ppm(PPM)
                    ),
                    (
                        "pooled",
                        "Pooled",
                        Some(4333),
                        Some(4334),
                        (Some(4333), Some(4334)),
                        Portion::Ppm(0)
                    ),
                    (
                        "hybrid",
                        "Hybrid",
                        Some(5333),
                        Some(5334),
                        (Some(5333), Some(5334)),
                        Portion::Ppm(333_300)
                    ),
                ],
                "{name}"
            );
            for pool in &config.pools {
                assert_eq!(pool.fees, config.fees, "{name}: the global fees");
                assert_eq!(pool.window, config.accounting.window(), "{name}");
            }
        }
    }

    #[test]
    fn at_least_one_pool_is_configured() {
        let mut config = table("regtest.toml");
        config.remove("pools");
        assert_eq!(rejected_key(parse(&config)), "pools");
        config.insert("pools".into(), toml::Value::Array(Vec::new()));
        assert_eq!(rejected_key(parse(&config)), "pools");
    }

    #[test]
    fn ids_are_unique_in_form_and_never_fleet() {
        for bad in ["fleet", "", "Solo", "my_pool", &"p".repeat(33)] {
            let mut config = table("regtest.toml");
            pool(&mut config, "hybrid").insert("id".into(), bad.into());
            assert_eq!(rejected_key(parse(&config)), "pools.id", "{bad:?}");
        }
        let mut config = table("regtest.toml");
        pool(&mut config, "hybrid").insert("id".into(), "solo".into());
        assert_eq!(rejected_key(parse(&config)), "pools.id", "a second solo");

        let mut config = table("regtest.toml");
        pool(&mut config, "hybrid").insert("id".into(), "my-bonus-2".into());
        let pools = parse(&config).unwrap().pools;
        assert_eq!(pools[2].id.as_str(), "my-bonus-2");
    }

    #[test]
    fn a_name_defaults_to_the_id() {
        let mut config = table("regtest.toml");
        pool(&mut config, "pooled").remove("name");
        assert_eq!(parse(&config).unwrap().pools[1].name, "pooled");
        for bad in ["", "  ", &"n".repeat(65)] {
            pool(&mut config, "pooled").insert("name".into(), bad.into());
            assert_eq!(rejected_key(parse(&config)), "pools.name", "{bad:?}");
        }
    }

    #[test]
    fn a_pool_listens_on_v1_v2_or_both_and_shares_no_address() {
        let mut config = table("regtest.toml");
        pool(&mut config, "pooled").remove("v1");
        pool(&mut config, "hybrid").remove("v2");
        let pools = parse(&config).unwrap().pools;
        let protocols: Vec<_> = pools
            .iter()
            .map(|pool| {
                (
                    pool.v1.is_some(),
                    pool.v2.is_some(),
                    pool.public_v1,
                    pool.public_v2,
                )
            })
            .collect();
        assert_eq!(
            protocols,
            [
                (true, true, Some(3333), Some(3334)),
                (false, true, None, Some(4334)),
                (true, false, Some(5333), None)
            ]
        );
        pool(&mut config, "pooled").remove("v2");
        assert_eq!(rejected_key(parse(&config)), "pools.v1", "no listener");

        for (id, key, bind, rejected) in [
            ("pooled", "v1", "0.0.0.0:3333", "pools.v1"),
            ("pooled", "v1", "127.0.0.1:3334", "pools.v1"),
            ("hybrid", "v2", "0.0.0.0:5333", "pools.v2"),
            ("hybrid", "v2", "192.0.2.5:4334", "pools.v2"),
        ] {
            let mut config = table("regtest.toml");
            pool(&mut config, id).insert(key.into(), bind.into());
            assert_eq!(
                rejected_key(parse(&config)),
                rejected,
                "{id}.{key} = {bind}"
            );
        }
        // One port on two specific addresses is two listeners.
        let mut config = table("regtest.toml");
        pool(&mut config, "solo").insert("v1".into(), "127.0.0.1:3333".into());
        pool(&mut config, "pooled").insert("v1".into(), "192.0.2.5:3333".into());
        assert!(parse(&config).is_ok());
    }

    #[test]
    fn a_public_port_comes_only_beside_its_listener() {
        let mut config = table("regtest.toml");
        pool(&mut config, "solo").insert("public_v1".into(), 443.into());
        pool(&mut config, "hybrid").insert("public_v2".into(), 8443.into());
        let pools = parse(&config).unwrap().pools;
        assert_eq!(
            pools
                .iter()
                .map(|pool| (pool.public_v1, pool.public_v2))
                .collect::<Vec<_>>(),
            [
                (Some(443), Some(3334)),
                (Some(4333), Some(4334)),
                (Some(5333), Some(8443))
            ]
        );
        pool(&mut config, "hybrid").remove("v2");
        assert_eq!(rejected_key(parse(&config)), "pools.public_v2");
        let mut config = table("regtest.toml");
        pool(&mut config, "solo").insert("public_v1".into(), 0.into());
        assert_eq!(rejected_key(parse(&config)), "pools.public_v1");
    }

    #[test]
    fn personal_bounds_take_any_portion_in_order_around_the_finder() {
        let portion = |key: &str, value: toml::Value| {
            toml::Value::Table(toml::Table::from_iter([(key.to_owned(), value)]))
        };
        let with = |min: Option<toml::Value>, max: Option<toml::Value>| {
            let mut config = table("regtest.toml");
            let mut range = toml::Table::new();
            if let Some(min) = min {
                range.insert("min".into(), min);
            }
            if let Some(max) = max {
                range.insert("max".into(), max);
            }
            pool(&mut config, "hybrid").insert("personal".into(), range.into());
            config
        };
        let sats = |n: u64| Portion::Amount(Amount::from_sat(n));
        let range = |config: &toml::Table| parse(config).unwrap().pools[2].personal.unwrap();

        // Unset bounds are 0% and 100%; every form parses, 0 included; and bounds in two forms parse.
        assert_eq!(
            range(&with(None, None)),
            Personal {
                min: Portion::Ppm(0),
                max: Portion::Ppm(PPM)
            }
        );
        let parsed = range(&with(
            Some(portion("sats", 100_000.into())),
            Some(portion("percent", 50.into())),
        ));
        assert_eq!(
            parsed,
            Personal {
                min: sats(100_000),
                max: Portion::Ppm(500_000)
            }
        );
        let fixed = range(&with(
            Some(portion("btc", 0.0.into())),
            Some(portion("btc", 1.0.into())),
        ));
        assert_eq!(
            fixed,
            Personal {
                min: sats(0),
                max: sats(100_000_000)
            }
        );
        // A fixed max below the finder's percent is compared per job, not here.
        assert!(parse(&with(None, Some(portion("sats", 330.into())))).is_ok());
        assert!(
            parse(&table("regtest.toml")).unwrap().pools[2]
                .personal
                .is_none()
        );

        for (min, max, key) in [
            // Out of order in one form.
            (
                Some(portion("percent", 50.into())),
                Some(portion("percent", 40.into())),
                "pools.personal",
            ),
            (
                Some(portion("sats", 2_000.into())),
                Some(portion("btc", 0.00001.into())),
                "pools.personal",
            ),
            // The finder's 33.33% outside a range of percents.
            (Some(portion("percent", 40.into())), None, "pools.finder"),
            (None, Some(portion("percent", 30.into())), "pools.finder"),
            // A fixed bound above 0 below P2TR's dust limit, and a percent above 100.
            (
                Some(portion("sats", 329.into())),
                None,
                "pools.personal.min",
            ),
            (
                None,
                Some(portion("percent", 101.into())),
                "pools.personal.max",
            ),
            // One portion key per bound.
            (
                Some(toml::Value::Table(toml::Table::from_iter([
                    ("percent".to_owned(), 1.into()),
                    ("sats".to_owned(), 1_000.into()),
                ]))),
                None,
                "pools.personal.min",
            ),
        ] {
            assert_eq!(
                rejected_key(parse(&with(min.clone(), max.clone()))),
                key,
                "{min:?} {max:?}"
            );
        }
        // A fixed finder outside a fixed range.
        let mut config = with(
            Some(portion("sats", 1_000.into())),
            Some(portion("sats", 2_000.into())),
        );
        pool(&mut config, "hybrid").insert("finder".into(), portion("sats", 3_000.into()));
        assert_eq!(rejected_key(parse(&config)), "pools.finder");

        let mut unknown = with(None, None);
        pool(&mut unknown, "hybrid")["personal"]
            .as_table_mut()
            .unwrap()
            .insert("default".into(), portion("percent", 10.into()));
        assert!(parse(&unknown).is_err(), "an unknown key under personal");
    }

    #[test]
    fn a_finder_takes_a_percent_btc_or_sats_and_may_be_0() {
        let mut config = table("regtest.toml");
        assert_eq!(p2tr_dust(), Amount::from_sat(330));
        let portion = |config: &toml::Table| parse(config).unwrap().pools[2].finder;
        // The fees take 1.5%, so a finder percent below 100 may reach 98.4999%.
        for (key, value, expected) in [
            ("percent", toml::Value::from(98.4999), Portion::Ppm(984_999)),
            ("percent", 0.0001.into(), Portion::Ppm(1)),
            ("percent", 100.into(), Portion::Ppm(PPM)),
            ("percent", 0.into(), Portion::Ppm(0)),
            (
                "btc",
                0.0000033.into(),
                Portion::Amount(Amount::from_sat(330)),
            ),
            ("sats", 330.into(), Portion::Amount(Amount::from_sat(330))),
            (
                "btc",
                0.5.into(),
                Portion::Amount(Amount::from_sat(50_000_000)),
            ),
            (
                "sats",
                50_000_000.into(),
                Portion::Amount(Amount::from_sat(50_000_000)),
            ),
            ("sats", 0.into(), Portion::Amount(Amount::ZERO)),
            ("btc", 0.0.into(), Portion::Amount(Amount::ZERO)),
            (
                "btc",
                21_000_000.0.into(),
                Portion::Amount(Amount::MAX_MONEY),
            ),
            (
                "sats",
                2_100_000_000_000_000_i64.into(),
                Portion::Amount(Amount::MAX_MONEY),
            ),
        ] {
            finder(&mut config, "hybrid", key, value.clone());
            assert_eq!(portion(&config), expected, "{key} = {value}");
        }

        for (key, value) in [
            ("percent", toml::Value::from(98.5)),
            ("percent", 99.into()),
            ("percent", 100.5.into()),
            ("percent", (-1.0).into()),
            ("percent", 0.00001.into()),
            ("btc", 0.0000032_9.into()),
            ("btc", 0.000000001.into()),
            ("btc", 21_000_000.01.into()),
            ("sats", 329.into()),
            ("sats", 1.5.into()),
            ("sats", (-1).into()),
            ("sats", 2_100_000_000_000_001_i64.into()),
        ] {
            finder(&mut config, "hybrid", key, value.clone());
            assert_eq!(
                rejected_key(parse(&config)),
                "pools.finder",
                "{key} = {value}"
            );
        }
        let both = toml::Table::from_iter([
            ("percent".to_owned(), 1.0.into()),
            ("sats".to_owned(), 1_000.into()),
        ]);
        for raw in [both, toml::Table::new()] {
            pool(&mut config, "hybrid").insert("finder".into(), raw.into());
            assert_eq!(rejected_key(parse(&config)), "pools.finder");
        }
        let unknown = toml::Table::from_iter([("share".to_owned(), 1.0.into())]);
        pool(&mut config, "hybrid").insert("finder".into(), unknown.into());
        assert!(matches!(parse(&config), Err(InvalidConfig::Toml(_))));
        pool(&mut config, "hybrid").remove("finder");
        assert!(
            matches!(parse(&config), Err(InvalidConfig::Toml(_))),
            "finder is required"
        );
    }

    #[test]
    fn a_finder_percent_that_would_never_pay_a_pool_output_is_refused_suggesting_100() {
        let mut config = table("regtest.toml");
        finder(&mut config, "hybrid", "percent", 99.0.into());
        match parse(&config) {
            Err(InvalidConfig::Value { key, reason }) => {
                assert_eq!(key, "pools.finder");
                assert!(reason.contains("100.5%"), "{reason}");
                assert!(reason.contains("percent = 100"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        // Without the fees, 99% leaves 1% to the pool output.
        pool(&mut config, "hybrid").insert("fees".into(), toml::Value::Array(Vec::new()));
        assert_eq!(
            parse(&config).unwrap().pools[2].finder,
            Portion::Ppm(990_000)
        );
    }

    #[test]
    fn a_btc_amount_and_the_same_in_sats_are_one_portion() {
        let mut config = table("regtest.toml");
        let address = fee_entries(&mut config)[0]["address"].clone();
        let mut sats = fee("fixed", &address, 1.0);
        let entry = sats.as_table_mut().unwrap();
        entry.remove("percent");
        entry.insert("sats".into(), 1_234_567.into());
        let btc = fixed_fee("fixed", &address, 0.01234567);
        for fees in [vec![sats], vec![btc]] {
            config.insert("fees".into(), fees.into());
            assert_eq!(
                parse(&config).unwrap().fees[0].portion,
                Portion::Amount(Amount::from_sat(1_234_567))
            );
        }
    }

    #[test]
    fn a_pools_fees_replace_the_global_ones_whole() {
        let mut config = table("regtest.toml");
        let address = fee_entries(&mut config)[0]["address"].clone();
        let mut own = fixed_fee("operator", &address, 0.01);
        own.as_table_mut().unwrap().remove("btc");
        own.as_table_mut()
            .unwrap()
            .insert("sats".into(), 1_000_000.into());
        pool(&mut config, "hybrid").insert("fees".into(), vec![own].into());
        pool(&mut config, "pooled").insert("fees".into(), toml::Value::Array(Vec::new()));
        let parsed = parse(&config).unwrap();
        let fees = |i: usize| -> Vec<(String, Portion)> {
            parsed.pools[i]
                .fees
                .iter()
                .map(|fee| (fee.name.clone(), fee.portion))
                .collect()
        };
        assert_eq!(
            fees(0),
            [
                ("operator".to_owned(), Portion::Ppm(9_000)),
                ("partner".to_owned(), Portion::Ppm(6_000))
            ],
            "the global fees"
        );
        assert_eq!(fees(1), [], "none");
        assert_eq!(
            fees(2),
            [(
                "operator".to_owned(),
                Portion::Amount(Amount::from_sat(1_000_000))
            )]
        );

        for bad in [
            vec![fee("all", &address, 100.0)],
            vec![fee("zero", &address, 0.0)],
            vec![fixed_fee("dust", &address, 0.00000293)],
            vec![fee("same", &address, 1.0), fee("same", &address, 1.0)],
        ] {
            pool(&mut config, "hybrid").insert("fees".into(), bad.clone().into());
            assert_eq!(rejected_key(parse(&config)), "pools.fees", "{bad:?}");
        }
        // A pool's own fees must leave its finder percent below 100% too.
        pool(&mut config, "hybrid").insert("fees".into(), vec![fee("big", &address, 70.0)].into());
        assert_eq!(rejected_key(parse(&config)), "pools.finder");
    }

    #[test]
    fn the_window_policy_takes_a_multiplier_of_2_by_default_and_checks_both() {
        let mut config = table("regtest.toml");
        section(&mut config, "accounting").remove("window_multiplier");
        assert_eq!(parse(&config).unwrap().accounting.window_multiplier, 2.0);
        for (key, value, rejected) in [
            ("window_multiplier", 0.0, "accounting.window_multiplier"),
            ("window_multiplier", -1.0, "accounting.window_multiplier"),
            (
                "window_multiplier",
                f64::INFINITY,
                "accounting.window_multiplier",
            ),
            ("window_min_work", -1.0, "accounting.window_min_work"),
            ("window_min_work", f64::NAN, "accounting.window_min_work"),
        ] {
            let mut bad = config.clone();
            section(&mut bad, "accounting").insert(key.into(), value.into());
            assert_eq!(rejected_key(parse(&bad)), rejected, "{key} = {value}");
        }
        section(&mut config, "accounting").remove("window_min_work");
        assert!(
            matches!(parse(&config), Err(InvalidConfig::Toml(_))),
            "window_min_work is required"
        );
    }

    #[test]
    fn a_pools_window_policy_falls_back_key_by_key() {
        let mut config = table("regtest.toml");
        let window = |config: &toml::Table, i: usize| {
            let window = parse(config).unwrap().pools[i].window;
            (window.window_multiplier, window.window_min_work)
        };
        let accounting = |pairs: &[(&str, f64)]| {
            toml::Value::Table(
                pairs
                    .iter()
                    .map(|(key, value)| (key.to_string(), (*value).into()))
                    .collect(),
            )
        };
        assert_eq!(window(&config, 2), (2.0, 1e6), "the global policy");
        pool(&mut config, "hybrid").insert(
            "accounting".into(),
            accounting(&[("window_multiplier", 1.33)]),
        );
        assert_eq!(
            window(&config, 2),
            (1.33, 1e6),
            "its own k, the global W_min"
        );
        pool(&mut config, "hybrid")
            .insert("accounting".into(), accounting(&[("window_min_work", 0.0)]));
        assert_eq!(
            window(&config, 2),
            (2.0, 0.0),
            "the global k, its own W_min"
        );
        pool(&mut config, "hybrid").insert(
            "accounting".into(),
            accounting(&[("window_multiplier", 5.0), ("window_min_work", 2e6)]),
        );
        assert_eq!(window(&config, 2), (5.0, 2e6));
        assert_eq!(
            window(&config, 1),
            (2.0, 1e6),
            "the other pools keep the global one"
        );
        pool(&mut config, "hybrid").insert("accounting".into(), toml::Table::new().into());
        assert_eq!(window(&config, 2), (2.0, 1e6));

        for (key, value, rejected) in [
            (
                "window_multiplier",
                0.0,
                "pools.accounting.window_multiplier",
            ),
            (
                "window_multiplier",
                f64::NAN,
                "pools.accounting.window_multiplier",
            ),
            ("window_min_work", -1.0, "pools.accounting.window_min_work"),
        ] {
            pool(&mut config, "hybrid").insert("accounting".into(), accounting(&[(key, value)]));
            assert_eq!(rejected_key(parse(&config)), rejected, "{key} = {value}");
        }
    }

    #[test]
    fn a_global_only_setting_under_a_pool_is_refused() {
        let mut config = table("regtest.toml");
        let buffer = toml::Table::from_iter([("maturity_buffer".to_owned(), 6.into())]);
        pool(&mut config, "hybrid").insert("accounting".into(), buffer.into());
        assert!(matches!(parse(&config), Err(InvalidConfig::Toml(_))));

        let mut config = table("regtest.toml");
        let payouts = config["payouts"].clone();
        pool(&mut config, "pooled").insert("payouts".into(), payouts);
        assert!(matches!(parse(&config), Err(InvalidConfig::Toml(_))));

        let mut config = table("regtest.toml");
        let tag = toml::Table::from_iter([("tag".to_owned(), "/mine/".into())]);
        pool(&mut config, "solo").insert("coinbase".into(), tag.into());
        assert!(matches!(parse(&config), Err(InvalidConfig::Toml(_))));
    }

    #[test]
    fn payouts_are_read_on_regtest_and_on_mainnet() {
        let mut config = table("regtest.toml");
        assert_eq!(
            parse(&config).unwrap().payouts,
            Some(Payouts {
                wallet: "payouts".into(),
                threshold: Amount::from_sat(100_000),
                interval: Duration::from_secs(60),
                outputs_per_batch: 100,
                rate: Rate::Fixed(2.0),
                fee_rate_cap: 10.0,
            })
        );
        let payouts = config["payouts"].clone();
        config.remove("payouts");
        assert_eq!(parse(&config).unwrap().payouts, None, "optional");

        let mut mainnet = table("mainnet.toml");
        assert_eq!(parse(&mainnet).unwrap().payouts, None);
        mainnet.insert("payouts".into(), payouts);
        section(&mut mainnet, "payouts").remove("fee_rate");
        section(&mut mainnet, "payouts").insert("confirmation_target".into(), 6.into());
        assert_eq!(
            parse(&mainnet).unwrap().payouts.map(|payouts| payouts.rate),
            Some(Rate::Estimate(6))
        );
    }

    #[test]
    fn regtest_pays_a_fixed_rate_and_mainnet_the_estimate_for_a_target() {
        let regtest = table("regtest.toml");
        let mut mainnet = table("mainnet.toml");
        mainnet.insert("payouts".into(), regtest["payouts"].clone());
        section(&mut mainnet, "payouts").remove("fee_rate");
        for (base, remove, add, rejected) in [
            (&regtest, "fee_rate", None, "payouts.fee_rate"),
            (
                &regtest,
                "",
                Some(("confirmation_target", 6)),
                "payouts.confirmation_target",
            ),
            (&mainnet, "", None, "payouts.confirmation_target"),
            (&mainnet, "", Some(("fee_rate", 2)), "payouts.fee_rate"),
            (
                &mainnet,
                "",
                Some(("confirmation_target", 0)),
                "payouts.confirmation_target",
            ),
            (
                &mainnet,
                "",
                Some(("confirmation_target", 1_009)),
                "payouts.confirmation_target",
            ),
        ] {
            let mut config = base.clone();
            section(&mut config, "payouts").remove(remove);
            if let Some((key, value)) = add {
                section(&mut config, "payouts").insert(key.into(), value.into());
            }
            assert_eq!(rejected_key(parse(&config)), rejected, "{remove} {add:?}");
        }
    }

    #[test]
    fn each_invalid_payout_setting_is_refused() {
        let config = table("regtest.toml");
        for (key, value, rejected) in [
            ("wallet", toml::Value::from(""), "payouts.wallet"),
            ("wallet", "pay outs".into(), "payouts.wallet"),
            ("wallet", "../payouts".into(), "payouts.wallet"),
            ("wallet", "w".repeat(65).into(), "payouts.wallet"),
            ("threshold_sats", 329.into(), "payouts.threshold_sats"),
            ("interval_secs", 0.0.into(), "payouts.interval_secs"),
            ("interval_secs", (-1.0).into(), "payouts.interval_secs"),
            ("outputs_per_batch", 0.into(), "payouts.outputs_per_batch"),
            (
                "outputs_per_batch",
                1_001.into(),
                "payouts.outputs_per_batch",
            ),
            ("fee_rate", 0.0.into(), "payouts.fee_rate"),
            ("fee_rate", 10.5.into(), "payouts.fee_rate"),
            ("fee_rate", f64::NAN.into(), "payouts.fee_rate"),
            ("fee_rate_cap", 0.0.into(), "payouts.fee_rate_cap"),
            ("fee_rate_cap", f64::INFINITY.into(), "payouts.fee_rate_cap"),
        ] {
            let mut bad = config.clone();
            section(&mut bad, "payouts").insert(key.into(), value.clone());
            assert_eq!(rejected_key(parse(&bad)), rejected, "{key} = {value}");
        }
        let mut edge = config.clone();
        for (key, value) in [
            ("threshold_sats", toml::Value::from(330)),
            ("outputs_per_batch", 1_000.into()),
            ("fee_rate", 10.0.into()),
        ] {
            section(&mut edge, "payouts").insert(key.into(), value);
        }
        assert!(parse(&edge).is_ok(), "the limits themselves are allowed");
        let mut unknown = config;
        section(&mut unknown, "payouts").insert("min_payout".into(), 1.into());
        assert!(matches!(parse(&unknown), Err(InvalidConfig::Toml(_))));
    }

    #[test]
    fn the_maturity_buffer_defaults_to_0() {
        let mut config = table("regtest.toml");
        section(&mut config, "accounting").insert("maturity_buffer".into(), 6.into());
        assert_eq!(parse(&config).unwrap().accounting.maturity_buffer, 6);
        section(&mut config, "accounting").remove("maturity_buffer");
        assert_eq!(parse(&config).unwrap().accounting.maturity_buffer, 0);
        section(&mut config, "accounting").insert("maturity_buffer".into(), (-1).into());
        assert!(matches!(parse(&config), Err(InvalidConfig::Toml(_))));
    }

    #[test]
    fn data_dir_defaults_to_the_network() {
        let mut config = table("regtest.toml");
        config.remove("data_dir");
        assert_eq!(parse(&config).unwrap().data_dir, Path::new("data/regtest"));
        config.insert("data_dir".into(), "/var/lib/puntohunter".into());
        assert_eq!(
            parse(&config).unwrap().data_dir,
            Path::new("/var/lib/puntohunter")
        );
    }

    #[test]
    fn rejects_floor_above_ceiling() {
        let mut config = table("regtest.toml");
        let vardiff = section(&mut config, "vardiff");
        let ceiling = vardiff["ceiling"].as_float().unwrap();
        vardiff.insert("floor".into(), (ceiling * 2.0).into());
        assert_eq!(rejected_key(parse(&config)), "vardiff.floor");
    }

    #[test]
    fn rejects_unknown_network() {
        let mut config = table("regtest.toml");
        config.insert("network".into(), "testnet".into());
        match parse(&config) {
            Err(InvalidConfig::Toml(e)) => {
                let message = e.to_string();
                assert!(message.contains("network = \"testnet\""), "{message}");
                assert!(message.contains("unknown variant"), "{message}");
            }
            other => panic!("expected a TOML error, got {other:?}"),
        }
    }

    #[test]
    fn accepts_floor_below_1() {
        let mut config = table("regtest.toml");
        let vardiff = section(&mut config, "vardiff");
        vardiff.insert("floor".into(), 2.5e-10.into());
        vardiff.insert("ceiling".into(), 2.5e-10.into());
        let config = parse(&config).unwrap();
        assert_eq!(config.vardiff.floor, 2.5e-10);
        assert_eq!(config.vardiff.ceiling, 2.5e-10);
    }

    #[test]
    fn the_public_host_is_optional_and_takes_a_domain_or_an_ip() {
        let mut config = table("regtest.toml");
        config.remove("public");
        assert_eq!(
            parse(&config).unwrap().public.host,
            None,
            "the request's Host header then"
        );

        config.insert("public".into(), toml::Table::new().into());
        for (host, shown) in [
            ("192.0.2.10", "192.0.2.10"),
            ("Pool.Example.com", "pool.example.com"),
            ("pool-1.lan", "pool-1.lan"),
            ("localhost", "localhost"),
            ("::1", "[::1]"),
            ("[fe80::1]", "[fe80::1]"),
        ] {
            section(&mut config, "public").insert("host".into(), host.into());
            let public = parse(&config).unwrap().public;
            assert_eq!(public.host.as_deref(), Some(shown), "{host}");
        }
        for host in [
            "",
            "http://pool.lan/",
            "pool.lan:3333",
            "pool..lan",
            "-pool.lan",
            "pool_1.lan",
            "[192.0.2.10]",
        ] {
            section(&mut config, "public").insert("host".into(), host.into());
            assert_eq!(rejected_key(parse(&config)), "public.host", "{host}");
        }
    }

    #[test]
    fn public_domains_take_domain_names_and_not_beside_the_host() {
        let mut config = table("regtest.toml");
        config.insert("public".into(), toml::Table::new().into());
        let domains = |list: &[&str]| {
            toml::Value::from(
                list.iter()
                    .map(|d| toml::Value::from(*d))
                    .collect::<Vec<_>>(),
            )
        };
        section(&mut config, "public")
            .insert("domains".into(), domains(&["Example.com", "example.org"]));
        assert_eq!(
            parse(&config).unwrap().public.domains,
            ["example.com", "example.org"]
        );
        for list in [&["192.0.2.10"][..], &["[::1]"], &["pool.lan:3333"], &[""]] {
            section(&mut config, "public").insert("domains".into(), domains(list));
            assert_eq!(rejected_key(parse(&config)), "public.domains", "{list:?}");
        }
        section(&mut config, "public").insert("domains".into(), domains(&["example.com"]));
        section(&mut config, "public").insert("host".into(), "stratum.example.com".into());
        assert_eq!(
            rejected_key(parse(&config)),
            "public.domains",
            "beside host"
        );
    }

    #[test]
    fn the_authority_switch_is_a_day() {
        let mut config = table("regtest.toml");
        config.insert("public".into(), toml::Table::new().into());
        assert_eq!(parse(&config).unwrap().public.authority_switch, None);
        section(&mut config, "public").insert("authority_switch".into(), "2026-11-15".into());
        assert_eq!(
            parse(&config).unwrap().public.authority_switch.as_deref(),
            Some("2026-11-15")
        );
        for text in [
            "",
            "2026-11",
            "2026-13-01",
            "2026-11-32",
            "26-11-15",
            "2026-1-15",
            "2026/11/15",
        ] {
            section(&mut config, "public").insert("authority_switch".into(), text.into());
            assert_eq!(
                rejected_key(parse(&config)),
                "public.authority_switch",
                "{text}"
            );
        }
    }

    #[test]
    fn the_stratum_host_is_the_set_host_else_stratum_before_the_site_domain_else_the_request() {
        let public = |host: Option<&str>, domains: &[&str]| Public {
            host: host.map(str::to_owned),
            domains: domains.iter().map(|d| d.to_string()).collect(),
            authority_switch: None,
        };
        let without = public(None, &[]);
        assert_eq!(without.stratum_host(Some("pool.lan")), "pool.lan");
        assert_eq!(without.stratum_host(Some("example.com")), "example.com");
        assert_eq!(without.stratum_host(None), "host");

        let with = public(None, &["example.com", "example.org", "eu.example.org"]);
        for (request, shown) in [
            ("example.com", "stratum.example.com"),
            ("portal.example.com", "stratum.example.com"),
            ("Portal.Example.com", "stratum.example.com"),
            ("example.org", "stratum.example.org"),
            ("eu.example.org", "stratum.eu.example.org"),
            ("api.eu.example.org", "stratum.eu.example.org"),
            ("notexample.com", "notexample.com"),
            ("203.0.113.5", "203.0.113.5"),
            ("[::1]", "[::1]"),
        ] {
            assert_eq!(with.stratum_host(Some(request)), shown, "{request}");
        }
        assert_eq!(with.stratum_host(None), "host");

        let set = public(Some("stratum.example.com"), &[]);
        assert_eq!(set.stratum_host(Some("example.org")), "stratum.example.com");
        assert_eq!(set.stratum_host(None), "stratum.example.com");
    }

    #[test]
    fn a_pool_may_set_either_name_flag_but_never_both() {
        let flags = |config: &toml::Table| {
            parse(config)
                .unwrap()
                .pools
                .iter()
                .map(|pool| (pool.worker_names, pool.worker_names_opt_in))
                .collect::<Vec<_>>()
        };
        let mut config = table("regtest.toml");
        assert_eq!(flags(&config), [(false, false); 3], "both off by default");

        // A pool's own flag; the others keep the global ones, each key falling back on its own.
        let own = |worker_names: Option<bool>, opt_in: Option<bool>| {
            let mut coinbase = toml::Table::new();
            if let Some(flag) = worker_names {
                coinbase.insert("worker_names".into(), flag.into());
            }
            if let Some(flag) = opt_in {
                coinbase.insert("worker_names_opt_in".into(), flag.into());
            }
            toml::Value::Table(coinbase)
        };
        pool(&mut config, "hybrid").insert("coinbase".into(), own(None, Some(true)));
        assert_eq!(
            flags(&config),
            [(false, false), (false, false), (false, true)]
        );
        section(&mut config, "coinbase").insert("worker_names_opt_in".into(), true.into());
        assert_eq!(flags(&config), [(false, true); 3]);

        // Both on, set or inherited, is refused with the pool's key; turning worker_names off on it is the way.
        let mut config = table("regtest.toml");
        section(&mut config, "coinbase").insert("worker_names".into(), true.into());
        pool(&mut config, "hybrid").insert("coinbase".into(), own(None, Some(true)));
        assert_eq!(
            rejected_key(parse(&config)),
            "pools.coinbase.worker_names_opt_in"
        );
        pool(&mut config, "hybrid").insert("coinbase".into(), own(Some(false), Some(true)));
        assert_eq!(
            flags(&config),
            [(true, false), (true, false), (false, true)]
        );
        section(&mut config, "coinbase").insert("worker_names_opt_in".into(), true.into());
        assert_eq!(
            rejected_key(parse(&config)),
            "pools.coinbase.worker_names_opt_in"
        );

        // The tag stays global.
        let mut config = table("regtest.toml");
        let mut tagged = toml::Table::new();
        tagged.insert("tag".into(), "/mine/".into());
        pool(&mut config, "hybrid").insert("coinbase".into(), tagged.into());
        assert!(parse(&config).is_err());
    }

    #[test]
    fn the_coinbase_tag_defaults_and_worker_names_are_off_unless_configured() {
        let mut config = table("regtest.toml");
        config.remove("coinbase");
        let coinbase = parse(&config).unwrap().coinbase;
        assert_eq!(
            (coinbase.tag.as_str(), coinbase.worker_names),
            (DEFAULT_TAG, false)
        );

        let mut coinbase = toml::Table::new();
        coinbase.insert("worker_names".into(), true.into());
        config.insert("coinbase".into(), coinbase.into());
        for tag in ["/Pool 1/", "x", &"t".repeat(MAX_TAG_LEN)] {
            section(&mut config, "coinbase").insert("tag".into(), tag.into());
            let coinbase = parse(&config).unwrap().coinbase;
            assert_eq!((coinbase.tag.as_str(), coinbase.worker_names), (tag, true));
        }
        for tag in [
            "",
            &"t".repeat(MAX_TAG_LEN + 1),
            "/caf\u{e9}/",
            "/tab\t/",
            "/nul\0/",
        ] {
            section(&mut config, "coinbase").insert("tag".into(), tag.into());
            assert_eq!(rejected_key(parse(&config)), "coinbase.tag", "{tag:?}");
        }
    }

    #[test]
    fn checks_the_portal_settings() {
        let portal = parse(&table("regtest.toml")).unwrap().portal;
        assert_eq!((portal.streams_per_ip, portal.max_streams), (20, 2000));
        assert_eq!(portal.trusted_proxy, None);
        assert_eq!(portal.keep_offline_days, 7);
        for key in [
            "lookups_per_minute",
            "streams_per_ip",
            "max_streams",
            "keep_offline_days",
        ] {
            let mut config = table("regtest.toml");
            section(&mut config, "portal").insert(key.into(), 0.into());
            assert_eq!(rejected_key(parse(&config)), format!("portal.{key}"));
        }

        let mut config = table("regtest.toml");
        section(&mut config, "portal").insert("trusted_proxy".into(), "127.0.0.1".into());
        let trusted = parse(&config).unwrap().portal.trusted_proxy;
        assert_eq!(trusted, Some(IpAddr::from([127, 0, 0, 1])));
        section(&mut config, "portal").insert("trusted_proxy".into(), "localhost".into());
        assert!(parse(&config).is_err(), "an address, not a name");
    }

    #[test]
    fn checks_the_api_origins() {
        assert!(
            parse(&table("regtest.toml"))
                .unwrap()
                .api
                .allowed_origins
                .is_empty()
        );
        let mut config = table("regtest.toml");
        config.insert("api".into(), toml::Table::new().into());
        let origins = [
            "https://dashboard.partner.example",
            "http://192.0.2.20:8000",
            "http://[::1]:3000",
        ];
        let list =
            |origins: &[&str]| toml::Value::Array(origins.iter().map(|&o| o.into()).collect());
        section(&mut config, "api").insert("allowed_origins".into(), list(&origins));
        assert_eq!(parse(&config).unwrap().api.allowed_origins, origins);
        for origin in [
            "*",
            "https://*.partner.example",
            "https://dashboard.partner.example/",
            "https://dashboard.partner.example/app",
            "https://Dashboard.partner.example",
            "dashboard.partner.example",
            "ftp://dashboard.partner.example",
            "https://user@dashboard.partner.example",
            "https://",
        ] {
            section(&mut config, "api").insert("allowed_origins".into(), list(&[origin]));
            assert_eq!(
                rejected_key(parse(&config)),
                "api.allowed_origins",
                "{origin}"
            );
        }
    }

    #[test]
    fn requires_exactly_one_auth_form() {
        let mut config = table("regtest.toml");
        section(&mut config, "node").insert("cookie_file".into(), "/tmp/.cookie".into());
        assert_eq!(rejected_key(parse(&config)), "node.cookie_file");

        let node = section(&mut config, "node");
        node.remove("cookie_file");
        node.remove("rpc_password");
        assert_eq!(rejected_key(parse(&config)), "node.rpc_password");

        let node = section(&mut config, "node");
        node.insert("rpc_password".into(), "secret".into());
        node.insert("rpc_password_file".into(), "/tmp/password".into());
        assert_eq!(rejected_key(parse(&config)), "node.rpc_password");

        section(&mut config, "node").remove("rpc_password");
        assert!(matches!(
            parse(&config).unwrap().node.auth,
            RpcAuth::PasswordFile { .. }
        ));
    }

    /// A secondary node named `name`, with a password file, as `[[secondary_nodes]]` takes it.
    fn secondary(name: &str) -> toml::Table {
        toml::Table::from_iter([
            ("name".to_owned(), name.into()),
            ("rpc_url".to_owned(), "http://198.51.100.2:8332".into()),
            ("rpc_user".to_owned(), "puntohunter".into()),
            ("rpc_password_file".to_owned(), "/tmp/password".into()),
            (
                "zmq_hashblock".to_owned(),
                "tcp://198.51.100.2:28332".into(),
            ),
        ])
    }

    fn with_secondaries(nodes: Vec<toml::Table>) -> toml::Table {
        let mut config = table("regtest.toml");
        config.insert(
            "secondary_nodes".into(),
            toml::Value::Array(nodes.into_iter().map(toml::Value::Table).collect()),
        );
        config
    }

    #[test]
    fn a_config_without_secondaries_is_a_fleet_of_one() {
        let config = parse(&table("regtest.toml")).unwrap();
        assert_eq!(config.node.name, PRIMARY);
        assert!(config.secondary_nodes.is_empty());
        assert_eq!(
            config.node_checks,
            NodeChecks {
                stuck: Duration::from_secs(30),
                recovery: Duration::from_secs(60),
            }
        );
    }

    #[test]
    fn secondaries_take_the_primarys_timeouts_unless_set() {
        let mut fast = secondary("fast");
        fast.insert("rpc_timeout_secs".into(), 2.5.into());
        let config = parse(&with_secondaries(vec![secondary("eu"), fast])).unwrap();
        let names: Vec<_> = config
            .secondary_nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(names, ["eu", "fast"]);
        let eu = &config.secondary_nodes[0];
        assert_eq!(eu.rpc_timeout, config.node.rpc_timeout);
        assert_eq!(eu.longpoll_timeout, config.node.longpoll_timeout);
        assert!(matches!(&eu.auth, RpcAuth::PasswordFile { user, .. } if user == "puntohunter"));
        assert_eq!(
            config.secondary_nodes[1].rpc_timeout,
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn refuses_a_bad_or_taken_node_name() {
        for name in ["", "EU", "eu_1", "primary", &"a".repeat(33)] {
            assert_eq!(
                rejected_key(parse(&with_secondaries(vec![secondary(name)]))),
                "secondary_nodes.name",
                "{name:?}"
            );
        }
        let twice = with_secondaries(vec![secondary("eu"), secondary("eu")]);
        assert_eq!(rejected_key(parse(&twice)), "secondary_nodes.name");
    }

    #[test]
    fn refuses_a_secondarys_bad_auth_or_timeout() {
        let mut both = secondary("eu");
        both.insert("rpc_password".into(), "secret".into());
        assert_eq!(
            rejected_key(parse(&with_secondaries(vec![both]))),
            "secondary_nodes.rpc_password"
        );

        let mut none = secondary("eu");
        none.remove("rpc_password_file");
        assert_eq!(
            rejected_key(parse(&with_secondaries(vec![none]))),
            "secondary_nodes.rpc_password"
        );

        let mut cookie = secondary("eu");
        cookie.insert("cookie_file".into(), "/tmp/.cookie".into());
        assert_eq!(
            rejected_key(parse(&with_secondaries(vec![cookie]))),
            "secondary_nodes.cookie_file"
        );

        let mut zero = secondary("eu");
        zero.insert("longpoll_timeout_secs".into(), 0.0.into());
        assert_eq!(
            rejected_key(parse(&with_secondaries(vec![zero]))),
            "secondary_nodes.longpoll_timeout_secs"
        );
    }

    #[test]
    fn refuses_zero_windows() {
        for key in ["stuck_secs", "recovery_secs"] {
            let mut config = table("regtest.toml");
            section(&mut config, "node").insert(key.into(), 0.0.into());
            assert_eq!(rejected_key(parse(&config)), format!("node.{key}"));
        }
    }

    #[test]
    fn load_refuses_a_password_file_others_can_read() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("puntohunter-password-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let password = dir.join("eu.password");
        let mut node = secondary("eu");
        node.insert(
            "rpc_password_file".into(),
            password.to_str().unwrap().into(),
        );
        let path = dir.join("config.toml");
        std::fs::write(&path, with_secondaries(vec![node]).to_string()).unwrap();
        let load = || Config::load(&path);

        assert!(
            matches!(load(), Err(ConfigError::PasswordFile { node, .. }) if node == "eu"),
            "a missing file"
        );
        std::fs::write(&password, "secret\n").unwrap();
        std::fs::set_permissions(&password, std::fs::Permissions::from_mode(0o640)).unwrap();
        let error = load().unwrap_err().to_string();
        assert!(error.contains("chmod 600"), "{error}");
        std::fs::set_permissions(&password, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load().unwrap().secondary_nodes.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A member named `name` at `url`, as `[[group.members]]` takes it.
    fn member(name: &str, url: &str) -> toml::Value {
        toml::Value::Table(toml::Table::from_iter([
            ("name".to_owned(), name.into()),
            ("url".to_owned(), url.into()),
        ]))
    }

    fn with_group(members: Vec<toml::Value>) -> toml::Table {
        let mut config = table("regtest.toml");
        config.insert(
            "group".into(),
            toml::Value::Table(toml::Table::from_iter([
                ("name".to_owned(), " PuntoHunter Group ".into()),
                ("members".to_owned(), toml::Value::Array(members)),
            ])),
        );
        config
    }

    #[test]
    fn a_config_without_a_group_has_none() {
        assert!(parse(&table("regtest.toml")).unwrap().group.is_none());
    }

    #[test]
    fn reads_a_group_and_its_members() {
        let mut config = with_group(vec![
            member("east", "https://east.example.com"),
            member("dev-host", "http://127.0.0.1:18080"),
        ]);
        let group = parse(&config).unwrap().group.unwrap();
        assert_eq!(group.name, "PuntoHunter Group");
        assert_eq!(group.refresh, Duration::from_secs(30));
        assert_eq!(
            group
                .members
                .iter()
                .map(|member| (member.name.as_str(), member.url.as_str()))
                .collect::<Vec<_>>(),
            [
                ("east", "https://east.example.com"),
                ("dev-host", "http://127.0.0.1:18080")
            ]
        );
        section(&mut config, "group").insert("refresh_secs".into(), 0.5.into());
        assert_eq!(
            parse(&config).unwrap().group.unwrap().refresh,
            Duration::from_millis(500)
        );
    }

    #[test]
    fn refuses_a_bad_group() {
        let east = || member("east", "https://east.example.com");
        assert_eq!(rejected_key(parse(&with_group(vec![]))), "group.members");
        for bad in ["", "  ", &"n".repeat(65)] {
            let mut config = with_group(vec![east()]);
            section(&mut config, "group").insert("name".into(), bad.into());
            assert_eq!(rejected_key(parse(&config)), "group.name", "{bad:?}");
        }
        for secs in [0.0, -1.0] {
            let mut config = with_group(vec![east()]);
            section(&mut config, "group").insert("refresh_secs".into(), secs.into());
            assert_eq!(rejected_key(parse(&config)), "group.refresh_secs");
        }
        for name in ["", "East", "east pool", &"n".repeat(33)] {
            let config = with_group(vec![member(name, "https://east.example.com")]);
            assert_eq!(
                rejected_key(parse(&config)),
                "group.members.name",
                "{name:?}"
            );
        }
        for url in [
            "east.example.com",
            "https://east.example.com/",
            "https://east.example.com/api/v1",
            "https://East.Example.com",
            "ftp://east.example.com",
            "https://user@east.example.com",
        ] {
            let config = with_group(vec![member("east", url)]);
            assert_eq!(rejected_key(parse(&config)), "group.members.url", "{url:?}");
        }
        let twice = with_group(vec![east(), member("east", "https://west.example.com")]);
        assert_eq!(rejected_key(parse(&twice)), "group.members.name");
        let same = with_group(vec![east(), member("west", "https://east.example.com")]);
        assert_eq!(rejected_key(parse(&same)), "group.members.url");
        let mut unknown = with_group(vec![east()]);
        section(&mut unknown, "group").insert("members_url".into(), "x".into());
        assert!(matches!(parse(&unknown), Err(InvalidConfig::Toml(_))));
    }

    /// `member` with `key` set to `value`.
    fn with(mut member: toml::Value, key: &str, value: &str) -> toml::Value {
        member
            .as_table_mut()
            .unwrap()
            .insert(key.into(), value.into());
        member
    }

    #[test]
    fn show_members_takes_a_label_on_every_instance() {
        let east = || member("east", "https://east.example.com");
        let labelled = || with(east(), "label", " US East ");
        // Without show_members, labels are allowed and the card shows none.
        let group = parse(&with_group(vec![labelled()])).unwrap().group.unwrap();
        assert!(!group.show_members);
        assert_eq!(group.members[0].label.as_deref(), Some("US East"));

        let show = |members: Vec<toml::Value>, own: Option<&str>| {
            let mut config = with_group(members);
            let group = section(&mut config, "group");
            group.insert("show_members".into(), true.into());
            if let Some(own) = own {
                group.insert("label".into(), own.into());
            }
            parse(&config)
        };
        let group = show(
            vec![with(labelled(), "link", "https://east.example.com")],
            Some("Türkiye"),
        )
        .unwrap()
        .group
        .unwrap();
        assert!(group.show_members);
        assert_eq!(group.label.as_deref(), Some("Türkiye"));
        assert_eq!(
            group.members[0].link.as_deref(),
            Some("https://east.example.com")
        );
        assert_eq!(rejected_key(show(vec![labelled()], None)), "group.label");
        assert_eq!(
            rejected_key(show(vec![east()], Some("Here"))),
            "group.members.label"
        );
        for bad in ["", "  ", &"x".repeat(33), "tab\there"] {
            assert_eq!(
                rejected_key(show(vec![labelled()], Some(bad))),
                "group.label",
                "{bad:?}"
            );
            assert_eq!(
                rejected_key(show(vec![with(east(), "label", bad)], Some("Here"))),
                "group.members.label",
                "{bad:?}"
            );
        }
        for link in ["east.example.com", "https://east.example.com/stats"] {
            assert_eq!(
                rejected_key(parse(&with_group(vec![with(east(), "link", link)]))),
                "group.members.link",
                "{link:?}"
            );
        }
    }

    #[test]
    fn each_stats_list_takes_its_own_length() {
        assert_eq!(
            parse(&table("regtest.toml")).unwrap().stats,
            Stats::default()
        );
        let mut config = table("regtest.toml");
        config.insert(
            "stats".into(),
            toml::Value::Table(toml::Table::from_iter([
                ("hash_rates".to_owned(), 5.into()),
                ("countries".to_owned(), 20.into()),
            ])),
        );
        let stats = parse(&config).unwrap().stats;
        assert_eq!(
            (stats.hash_rates, stats.countries, stats.best_shares_round),
            (5, 20, 10)
        );
        for key in [
            "best_shares_round",
            "best_shares_24h",
            "hash_rates",
            "miner_types",
            "countries",
        ] {
            let mut config = table("regtest.toml");
            config.insert(
                "stats".into(),
                toml::Value::Table(toml::Table::from_iter([(key.to_owned(), 0.into())])),
            );
            assert_eq!(rejected_key(parse(&config)), format!("stats.{key}"));
        }
    }

    #[test]
    fn backup_snapshots_into_the_data_directory_unless_told_otherwise() {
        let mut config = table("regtest.toml");
        assert!(
            parse(&config).unwrap().backup.is_none(),
            "off without [backup]"
        );
        config.insert("backup".into(), toml::Value::Table(toml::Table::new()));
        let parsed = parse(&config).unwrap();
        assert_eq!(
            parsed.backup.unwrap(),
            Backup {
                dir: parsed.data_dir.join("backups"),
                interval: Duration::from_secs(900),
                keep: 96,
            }
        );
        let backup = section(&mut config, "backup");
        backup.insert("dir".into(), "/var/backups/puntohunter".into());
        backup.insert("interval_secs".into(), 600.into());
        backup.insert("keep".into(), 10.into());
        assert_eq!(
            parse(&config).unwrap().backup.unwrap(),
            Backup {
                dir: "/var/backups/puntohunter".into(),
                interval: Duration::from_secs(600),
                keep: 10,
            }
        );
        for key in ["interval_secs", "keep"] {
            let mut config = config.clone();
            section(&mut config, "backup").insert(key.into(), 0.into());
            assert_eq!(rejected_key(parse(&config)), format!("backup.{key}"));
        }
    }

    #[test]
    fn load_refuses_a_missing_country_database() {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-geo-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let database = dir.join("country.mmdb");
        let mut config = table("regtest.toml");
        config.insert(
            "geo".into(),
            toml::Value::Table(toml::Table::from_iter([(
                "database".to_owned(),
                database.to_str().unwrap().into(),
            )])),
        );
        let path = dir.join("config.toml");
        std::fs::write(&path, config.to_string()).unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::GeoDatabase { .. })
        ));
        std::fs::write(&database, b"read when the pool starts").unwrap();
        let geo = Config::load(&path).unwrap().geo.unwrap();
        assert_eq!((geo.database, geo.min_workers), (database.clone(), 1));
        section(&mut config, "geo").insert("min_workers".into(), 3.into());
        assert_eq!(parse(&config).unwrap().geo.unwrap().min_workers, 3);
        section(&mut config, "geo").insert("min_workers".into(), 0.into());
        assert_eq!(rejected_key(parse(&config)), "geo.min_workers");
        assert!(parse(&table("regtest.toml")).unwrap().geo.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_refuses_a_missing_portal_site_and_an_overlay_that_is_no_directory() {
        let dir =
            std::env::temp_dir().join(format!("puntohunter-site-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let site = dir.join("site.json");
        let overlay = dir.join("overlay");
        let mut config = table("regtest.toml");
        section(&mut config, "portal").insert("site".into(), site.to_str().unwrap().into());
        section(&mut config, "portal").insert("overlay".into(), overlay.to_str().unwrap().into());
        let path = dir.join("config.toml");
        std::fs::write(&path, config.to_string()).unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::PortalSite { .. })
        ));
        std::fs::write(&site, b"read when the pool starts").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::PortalOverlay { .. })
        ));
        std::fs::create_dir(&overlay).unwrap();
        let portal = Config::load(&path).unwrap().portal;
        assert_eq!((portal.site, portal.overlay), (Some(site), Some(overlay)));
        let portal = parse(&table("regtest.toml")).unwrap().portal;
        assert_eq!((portal.site, portal.overlay), (None, None));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
