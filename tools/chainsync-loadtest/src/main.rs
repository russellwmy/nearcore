mod cli;
pub mod nearcore;
pub mod near_client;

use near_performance_metrics::process;
use near_primitives::version;
use once_cell::sync;
use openssl_probe;
use std::path;
use std::time;

static NEARD_VERSION: sync::Lazy<version::Version> = sync::Lazy::new(|| version::Version {
    version: "trunk".to_string(),
    build: "unknown".to_string(),
});
static DEFAULT_HOME: sync::Lazy<path::PathBuf> = sync::Lazy::new(nearcore::get_default_home);

fn main() {
    openssl_probe::init_ssl_cert_env_vars();
    process::schedule_printing_performance_stats(time::Duration::from_secs(60));
    cli::Cmd::parse_and_run();
}
