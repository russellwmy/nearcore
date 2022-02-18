#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

mod start;
mod near_client;

use near_crypto::{Signer};
use anyhow::{anyhow};
use near_primitives::version;
use openssl_probe;
use clap::{AppSettings, Clap};
use futures::future::FutureExt;
use near_chain_configs::GenesisValidationMode;
use nearcore::config;
use std::path::Path;
use std::{env, io};
use tracing::metadata::LevelFilter;
use tracing::{info,error};
use tracing_subscriber::EnvFilter;



fn download_configs(chain_id :&str, dir :&std::path::Path) -> anyhow::Result<()> {
    // Always fetch the config.
    std::fs::create_dir_all(dir)?;
    let url = config::get_config_url(chain_id);
    let config_path = &dir.join(config::CONFIG_FILENAME);
    config::download_config(&url,config_path)?;
    let config = config::Config::from_file(config_path)?;

    // Fetch genesis file if not cached.
    let genesis_path = dir.join(config.genesis_file);
    if !genesis_path.exists() {
        let url = config::get_genesis_url(chain_id);
        config::download_config(&url, &genesis_path)?;
    }

    // Generate node key if missing.
    let node_key_path = dir.join(config.node_key_file);
    if !node_key_path.exists() {
        let account_id = "node".parse().unwrap();
        let node_signer = near_crypto::InMemorySigner::from_random(account_id, near_crypto::KeyType::ED25519);
        node_signer.write_to_file(&node_key_path)?;
    }
    return Ok(());
}

#[derive(Clap, Debug)]
struct Cmd {
    #[clap(long)]
    pub chain_id : String,
    #[clap(long)]
    pub start_block_height : usize,
    #[clap(long)]
    pub start_block_hash : String,
}

impl Cmd {
    fn parse_and_run() {
        let cmd = Self::parse();
    
        let mut cache_dir = dirs::cache_dir().unwrap();
        cache_dir.push("near_configs");
        cache_dir.push(&cmd.chain_id);

        info!("downloading configs for chain {}",cmd.chain_id);
        let home_dir = cache_dir.as_path();
        if let Err(e) = download_configs(&cmd.chain_id,home_dir) {
            error!("Failed to initialize configs: {:#}", e);
        }

        info!("load_config({})",cmd.chain_id);
        // Load configs from home.
        let genesis_validation = GenesisValidationMode::UnsafeFast;
        let mut near_config = nearcore::config::load_config(home_dir, genesis_validation);
        
        // Set current version in client config.
        near_config.client_config.version = version::Version{
            version: "trunk".to_string(),
            build: "unknown".to_string(),
        };
        info!("#boot nodes = {}",near_config.network_config.boot_nodes.len());
        return;

        let sys = actix::System::new();
        sys.block_on(async move {
            start::start_with_config(home_dir, near_config).expect("start_with_config");

            let sig = if cfg!(unix) {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigint = signal(SignalKind::interrupt()).unwrap();
                let mut sigterm = signal(SignalKind::terminate()).unwrap();
                futures::select! {
                    _ = sigint .recv().fuse() => "SIGINT",
                    _ = sigterm.recv().fuse() => "SIGTERM"
                }
            } else {
                tokio::signal::ctrl_c().await.unwrap();
                "Ctrl+C"
            };
            info!(target: "neard", "Got {}, stopping...", sig);
            actix::System::current().stop();
        });
        sys.run().unwrap();
    }
}

fn init_logging() {
    let env_filter = EnvFilter::from_default_env().add_directive(LevelFilter::INFO.into());
    tracing_subscriber::fmt::Subscriber::builder()
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::ENTER
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        )
        .with_env_filter(env_filter)
        .with_writer(io::stderr)
        .init();
}

fn main() {
    init_logging();
    openssl_probe::init_ssl_cert_env_vars();
    Cmd::parse_and_run();
}
