use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::SystemTime,
};

use bitcoin::{Amount, address::NetworkUnchecked};
use clap::{Parser, Subcommand};
use puntohunter::{
    config::{Config, ConfigError},
    geo::{Geo, GeoError},
    metrics::MetricsError,
    mining::{extranonce::ExtranonceAllocator, job::JobCache},
    node::{Bitcoind, NodeError, fleet},
    pool::{
        journal::{self, Journal},
        ledger::{self, Unbacked},
        lifecycle, payouts, pplns, submitter,
    },
    portal::{
        self, FleetSnapshots, Lookups, NextAuthorityKey, PayoutTerms, Portal, Site, SiteError,
        StreamCaps, Windows, WorkerSnapshots,
    },
    secret::SecretError,
    store::{self, Reader, Store, StoreError},
    stratum::{
        self, v1,
        v2::{
            self,
            authority::{self, Authority},
        },
    },
    telemetry::{
        self, Telemetry,
        fleet::Fleets,
        handle::{self, HandleKey},
        rollup,
        round::Round,
    },
    tokens,
    wallet::Wallet,
};
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version = puntohunter::VERSION, about = "Solo and pooled Bitcoin mining engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the pool.
    Serve {
        /// Path to the network's TOML config.
        #[arg(long)]
        config: PathBuf,
    },
    /// The operator's fee reserve, from which payout batches pay their transaction fees.
    Reserve {
        #[command(subcommand)]
        command: ReserveCommand,
    },
    /// Balance tokens, each of which opens one address's private balance view.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// The SV2 authority key miners pin.
    Authority {
        #[command(subcommand)]
        command: AuthorityCommand,
    },
    /// Payout batches.
    Payouts {
        #[command(subcommand)]
        command: PayoutsCommand,
    },
}

#[derive(Subcommand)]
enum PayoutsCommand {
    /// Lift a pause of payouts, once you have settled what caused it, such as a block that left the chain
    /// after its balances were paid. A running pool resumes at its next payout interval.
    Resume {
        /// Path to the network's TOML config.
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum AuthorityCommand {
    /// Make the next key, for a rotation, beside the current one in the data directory, and print its public
    /// key. A next key already there is kept, and its public key printed again.
    Next {
        /// Path to the network's TOML config.
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Issue a token for an address and print it, the one time it is shown. Only its hash is kept.
    Issue {
        /// The payout address whose balance the token opens.
        address: String,
        /// Path to the network's TOML config.
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum ReserveCommand {
    /// Credit the fee reserve with funds the operator has put in the payout wallet. Refused beyond the
    /// wallet's spendable funds less what miners are owed and what the reserve already holds.
    Credit {
        /// Satoshis to credit.
        #[arg(value_parser = clap::value_parser!(u64).range(1..))]
        sats: u64,
        /// Path to the network's TOML config.
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Metrics(#[from] MetricsError),
    #[error("cannot create data directory {}: {source}", path.display())]
    DataDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Geo(#[from] GeoError),
    #[error(transparent)]
    Site(#[from] SiteError),
    #[error("cannot listen on {bind}: {source}")]
    Listen {
        bind: std::net::SocketAddr,
        source: std::io::Error,
    },
    #[error("cannot wait for the shutdown signal: {0}")]
    Signal(std::io::Error),
    #[error("payouts are not configured: the config has no [payouts] section")]
    NoPayouts,
    #[error("cannot read the payout wallet {wallet}: {source}")]
    Wallet { wallet: String, source: NodeError },
    #[error(transparent)]
    Unbacked(#[from] Unbacked),
    #[error("{address} is not a {network} address")]
    Address {
        address: String,
        network: &'static str,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            // The zeromq crate warns at each reconnect to a node that is down, which its health logs once.
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,zeromq=error")),
        )
        .with_writer(std::io::stdout)
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let result = match Cli::parse().command {
        Command::Serve { config } => serve(&config).await,
        Command::Reserve {
            command: ReserveCommand::Credit { sats, config },
        } => credit_reserve(&config, Amount::from_sat(sats)).await,
        Command::Token {
            command: TokenCommand::Issue { address, config },
        } => issue_token(&config, &address).await,
        Command::Authority {
            command: AuthorityCommand::Next { config },
        } => next_authority_key(&config),
        Command::Payouts {
            command: PayoutsCommand::Resume { config },
        } => resume_payouts(&config).await,
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn credit_reserve(config: &Path, sats: Amount) -> Result<(), Error> {
    let config = Config::load(config)?;
    let payouts = config.payouts.as_ref().ok_or(Error::NoPayouts)?;
    let spendable = Wallet::new(&config.node, &payouts.wallet)
        .spendable()
        .await
        .map_err(|source| Error::Wallet {
            wallet: payouts.wallet.clone(),
            source,
        })?;
    std::fs::create_dir_all(&config.data_dir).map_err(|source| Error::DataDir {
        path: config.data_dir.clone(),
        source,
    })?;
    let store = Store::open(&config.data_dir.join(store::DATABASE))?;
    let reserve = store
        .commit(move |t| ledger::credit_reserve(t, sats, spendable))
        .await??;
    println!(
        "fee reserve credited {} sats; it holds {} sats",
        sats.to_sat(),
        reserve.to_sat()
    );
    Ok(())
}

async fn resume_payouts(config: &Path) -> Result<(), Error> {
    let config = Config::load(config)?;
    std::fs::create_dir_all(&config.data_dir).map_err(|source| Error::DataDir {
        path: config.data_dir.clone(),
        source,
    })?;
    let store = Store::open(&config.data_dir.join(store::DATABASE))?;
    let now = telemetry::unix(SystemTime::now());
    let lifted = store.commit(move |t| ledger::resume(t, now)).await?;
    if lifted.is_empty() {
        println!("payouts were not paused");
        return Ok(());
    }
    for pause in &lifted {
        match &pause.block {
            Some(block) => println!("lifted: {} (block {block})", pause.reason),
            None => println!("lifted: {}", pause.reason),
        }
    }
    println!("Payouts resume at the pool's next payout interval; a running pool needs no restart.");
    Ok(())
}

fn next_authority_key(config: &Path) -> Result<(), Error> {
    let config = Config::load(config)?;
    std::fs::create_dir_all(&config.data_dir).map_err(|source| Error::DataDir {
        path: config.data_dir.clone(),
        source,
    })?;
    let path = config.data_dir.join(authority::NEXT_FILE);
    let made = !path.exists();
    let next = Authority::load_or_generate(&path)?;
    println!(
        "{} SV2 authority key {}: {}",
        if made {
            "made the next"
        } else {
            "kept the next"
        },
        path.display(),
        next.public_key()
    );
    println!(
        "Set [public] authority_switch to the day it takes over and restart to announce it; at the switch, \
         move it over {}.",
        authority::FILE
    );
    Ok(())
}

async fn issue_token(config: &Path, address: &str) -> Result<(), Error> {
    let config = Config::load(config)?;
    let invalid = || Error::Address {
        address: address.to_owned(),
        network: config.network.chain(),
    };
    let address = address
        .parse::<bitcoin::Address<NetworkUnchecked>>()
        .map_err(|_| invalid())?
        .require_network(config.network.bitcoin())
        .map_err(|_| invalid())?;
    std::fs::create_dir_all(&config.data_dir).map_err(|source| Error::DataDir {
        path: config.data_dir.clone(),
        source,
    })?;
    let store = Store::open(&config.data_dir.join(store::DATABASE))?;
    let shown = address.to_string();
    let token = store.commit(move |t| tokens::issue(t, &address)).await?;
    // Printed, never logged: this is the one time the token is seen.
    println!("balance token for {shown}: {token}");
    println!(
        "It opens this address's balance at /balance.html. Only its hash is kept, so it cannot be shown again."
    );
    Ok(())
}

async fn serve(config: &Path) -> Result<(), Error> {
    let started = SystemTime::now();
    tracing::info!(version = puntohunter::VERSION, "starting");
    let config = Config::load(config)?;
    tracing::info!(network = config.network.chain(), "config loaded");
    let pools = config.pools.clone();
    let metrics = puntohunter::metrics::install(config.metrics.bind).await?;
    let blocks_dir = config.data_dir.join("blocks");
    std::fs::create_dir_all(&blocks_dir).map_err(|source| Error::DataDir {
        path: blocks_dir.clone(),
        source,
    })?;
    let database = config.data_dir.join(store::DATABASE);
    let store = Store::open(&database)?;
    let reader = Reader::open(&database)?;
    tracing::info!(data_dir = %config.data_dir.display(), "store open");
    // Without [backup], no snapshot is taken.
    if let Some(settings) = config.backup.clone() {
        tracing::info!(
            dir = %settings.dir.display(),
            interval = ?settings.interval,
            keep = settings.keep,
            "database snapshots"
        );
        store::backup::spawn(database.clone(), settings);
    }
    let authority = Arc::new(Authority::load_or_generate(
        &config.data_dir.join(authority::FILE),
    )?);
    // Printed for pinning: puntominer's stratum2+tcp://host:port/<key>, and AxeOS's authority-key field.
    tracing::info!(authority_key = %authority.public_key(), "SV2 authority key");
    // A rotation is announced by the next key and the day it takes over, together.
    let next_path = config.data_dir.join(authority::NEXT_FILE);
    let next_authority_key = match (next_path.exists(), &config.public.authority_switch) {
        (true, Some(day)) => {
            let next = Authority::load_or_generate(&next_path)?;
            tracing::info!(
                next_key = %next.public_key(),
                switch_on = %day,
                "SV2 authority key rotation announced"
            );
            Some(NextAuthorityKey {
                key: next.public_key(),
                switch_on: day.clone(),
            })
        }
        (true, None) => {
            tracing::warn!(
                path = %next_path.display(),
                "a next SV2 authority key without [public] authority_switch is not announced"
            );
            None
        }
        (false, Some(day)) => {
            tracing::warn!(
                switch_on = %day,
                path = %next_path.display(),
                "[public] authority_switch is set without a next SV2 authority key; nothing is announced"
            );
            None
        }
        (false, None) => None,
    };
    let handles = HandleKey::load_or_generate(&config.data_dir.join(handle::FILE))?;

    // The primary first.
    let nodes = std::iter::once(&config.node)
        .chain(&config.secondary_nodes)
        .map(|node| (node.name.clone(), Arc::new(Bitcoind::start(node))))
        .collect();
    let fleet = fleet::spawn(nodes, config.network, config.node_checks);
    if !config.secondary_nodes.is_empty() {
        tracing::info!(
            nodes = config.secondary_nodes.len() + 1,
            stuck = ?config.node_checks.stuck,
            recovery = ?config.node_checks.recovery,
            "node fleet"
        );
    }
    let (work, node_state) = (fleet.work.clone(), fleet.primary_state.clone());
    // The round, seeded before the submitter can start the next one and before any share counts in it.
    let rollup_secs = config.telemetry.rollup_secs;
    let round = Arc::new(
        reader
            .read(move |connection| {
                Round::seed(connection, telemetry::unix(SystemTime::now()), rollup_secs)
            })
            .await?,
    );
    let start = round.start();
    tracing::info!(
        since_block = ?start.height,
        started_at = ?start.at,
        fleets = round.round().len(),
        "round seeded"
    );
    let answered = Arc::new(Notify::new());
    let blocks = submitter::spawn(
        fleet.members.clone(),
        fleet.status.clone(),
        store.clone(),
        blocks_dir,
        answered.clone(),
        round.clone(),
    );
    lifecycle::spawn(
        fleet.members.clone(),
        fleet.status.clone(),
        store.clone(),
        work.clone(),
        answered,
        config.accounting.maturity_buffer,
    );
    let telemetry = Telemetry::with_round(config.telemetry.rollup_secs, round);
    rollup::spawn(
        telemetry.clone(),
        store.clone(),
        config.telemetry.rollup_secs,
    );
    puntohunter::metrics::serve(
        metrics,
        pools
            .iter()
            .map(|pool| (pool.id.clone(), pool.transports()))
            .collect(),
        work.clone(),
        telemetry.clone(),
        store.sync_status(),
        reader.clone(),
    );
    // Each pool's window policy, recorded once; pools with one policy share its row.
    let mut policies = journal::Policies::new();
    for pool in pools.iter().filter(|pool| pool.can_pay_pool()) {
        let policy = journal::policy(&store, pool.window).await?;
        tracing::info!(
            pool = %pool.id,
            window_multiplier = pool.window.window_multiplier,
            window_min_work = pool.window.window_min_work,
            policy,
            "accounting policy"
        );
        policies.insert(pool.id.clone(), policy);
    }
    // Blocks whose solution share was never journaled, found while the journal could not write or just before
    // a crash: no such share can arrive now, before the journal opens, so each takes its pool's window as it
    // stood when the block was found.
    let cut = store.commit(pplns::cut_off_unjournaled).await?;
    if !cut.is_empty() {
        tracing::warn!(
            blocks = ?cut,
            "blocks whose solution share was never journaled take their pool's window up to its last journaled share"
        );
    }
    // Blocks whose solution was journaled but whose allocation was not, before a crash.
    let allocated = store.commit(pplns::allocate_ready).await?;
    if !allocated.is_empty() {
        tracing::info!(
            blocks = allocated.len(),
            "allocated blocks left unallocated"
        );
    }
    let paused = store.commit(ledger::paused).await?;
    if paused {
        tracing::error!("CRITICAL: payouts are paused for reconciliation");
    }
    puntohunter::metrics::set_payouts_paused(paused);
    // Without [payouts], balances accrue and nothing is paid.
    let payout_terms = config.payouts.clone().map(|settings| {
        tracing::info!(
            wallet = settings.wallet,
            interval = ?settings.interval,
            "payout worker started"
        );
        let standing = payouts::spawn(
            Wallet::new(&config.node, &settings.wallet),
            store.clone(),
            settings.clone(),
            config.addresses.pool_wallet.clone(),
        );
        PayoutTerms { settings, standing }
    });
    let journal = Journal::spawn(store.clone(), policies);
    // Without [geo], no connection is given a country and the stats page has no countries card.
    let geo = match &config.geo {
        Some(settings) => {
            let geo = Geo::open(&settings.database)?;
            geo.spawn();
            tracing::info!(database = %settings.database.display(), "country database open");
            Some(geo)
        }
        None => None,
    };
    // The portal's pages, assembled before any listener opens, so a site that cannot be built stops the pool
    // before a miner connects.
    let site = Site::build(
        config.portal.site.as_deref(),
        config.portal.overlay.as_deref(),
    )?;
    tracing::info!(
        name = site.name(),
        files = site.file_count(),
        "portal site built"
    );
    let pool_wallet = config.addresses.pool_wallet.script_pubkey();
    let context = Arc::new(stratum::Context {
        network: config.network,
        vardiff: config.vardiff,
        extranonces: Arc::new(ExtranonceAllocator::default()),
        work: work.clone(),
        jobs: JobCache::new(&pools, &pool_wallet, &config.coinbase.tag),
        pool_wallet: pool_wallet.clone(),
        blocks,
        authorize_timeout: config.stratum.authorize_timeout,
        handles,
        telemetry: telemetry.clone(),
        fleets: Fleets::new(store),
        journal: journal.clone(),
        geo: geo.clone(),
    });
    // Each pool's listeners, a protocol it leaves out having none.
    for pool in &pools {
        if let Some(bind) = pool.v1 {
            v1::listen(bind, pool.clone(), context.clone())
                .await
                .map_err(|source| Error::Listen { bind, source })?;
        }
        if let Some(bind) = pool.v2 {
            v2::listen(bind, pool.clone(), context.clone(), authority.clone())
                .await
                .map_err(|source| Error::Listen { bind, source })?;
        }
    }
    // What the overview tells miners to connect to: each pool's public ports, as solo_v1=3333.
    let ports: Vec<String> = pools
        .iter()
        .flat_map(|pool| {
            [("v1", pool.public_v1), ("v2", pool.public_v2)]
                .into_iter()
                .filter_map(|(protocol, port)| Some(format!("{}_{protocol}={}", pool.id, port?)))
        })
        .collect();
    tracing::info!(
        host = config.public.host.as_deref().unwrap_or("the browser's"),
        ports = %ports.join(" "),
        "public Stratum endpoint"
    );
    // Without [group], the overview shows this instance alone.
    let group = config.group.as_ref().map(|group| {
        tracing::info!(
            group = group.name,
            members = group.members.len(),
            "group polling started"
        );
        puntohunter::group::spawn(group, config.network)
    });
    let bind = config.portal.bind;
    portal::listen(
        bind,
        Portal {
            network: config.network,
            pools,
            public: config.public,
            authority_key: authority.public_key(),
            next_authority_key,
            handles: context.handles.clone(),
            telemetry,
            reader,
            fees: config.fees.iter().map(Into::into).collect(),
            rollup_secs: config.telemetry.rollup_secs,
            work,
            node: node_state,
            nodes: fleet.status.clone(),
            journal,
            lookups: Lookups::new(config.portal.lookups_per_minute),
            streams: StreamCaps::new(config.portal.streams_per_ip, config.portal.max_streams),
            trusted_proxy: config.portal.trusted_proxy,
            keep_offline_days: config.portal.keep_offline_days,
            allowed_origins: config.api.allowed_origins,
            started,
            pool_wallet,
            accounting: config.accounting,
            payouts: payout_terms,
            windows: Windows::default(),
            workers: WorkerSnapshots::default(),
            fleets: FleetSnapshots::default(),
            group,
            geo,
            country_min_workers: config.geo.as_ref().map_or(1, |geo| geo.min_workers),
            stats_limits: config.stats,
            site,
        },
    )
    .await
    .map_err(|source| Error::Listen { bind, source })?;
    puntohunter::metrics::set_ready();
    tracing::info!("ready");
    shutdown_signal().await?;
    tracing::info!("shutting down");
    Ok(())
}

/// Resolves on Ctrl-C, or on SIGTERM, which systemd sends to stop the service, so either stop is logged alike.
async fn shutdown_signal() -> Result<(), Error> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(Error::Signal)?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.map_err(Error::Signal),
        _ = terminate.recv() => Ok(()),
    }
}
