pub mod config;
pub mod failpoint;
pub mod geo;
pub mod group;
pub mod metrics;
pub mod mining;
pub mod node;
pub mod pool;
pub mod portal;
pub mod secret;
pub mod store;
pub mod stratum;
pub mod telemetry;
pub mod tokens;
pub mod wallet;

/// The running version: the package's and the commit it is built from, such as 0.1.5+fb8d1c55, marked
/// -dirty when built from uncommitted sources (build.rs).
pub const VERSION: &str = env!("PUNTOHUNTER_VERSION");
