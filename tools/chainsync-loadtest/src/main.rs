#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

mod start;
mod near_client;

use std::sync::{Arc,Mutex};
use std::str::FromStr;
use near_crypto::{Signer};
use anyhow::{anyhow,Context};
use near_primitives::version;
use openssl_probe;
use clap::{AppSettings, Clap};
use futures::future::FutureExt;
use futures::task::SpawnExt;
use near_chain_configs::{ClientConfig,GenesisValidationMode};
use nearcore::config;
use std::path::Path;
use std::{env, io};
use tracing::metadata::LevelFilter;
use tracing::{info,error};
use tracing_subscriber::EnvFilter;
use near_primitives::hash::CryptoHash;
use near_network::types::{NetworkClientMessages,NetworkRequests};

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

async fn sync(pool: futures::executor::ThreadPool, config : ClientConfig, network: near_client::Network,  start_block_hash: CryptoHash) -> anyhow::Result<()> {
    info!("SYNC waiting for peers");
    let peers = network.info(config.min_num_peers).await?;
    info!("SYNC start");
    let mut next_block_hash = start_block_hash;
    let peer = &peers.highest_height_peers[0];
    let target_height = peer.chain_info.height;
    info!("SYNC target_height = {}",target_height);
    let msg = network.call(NetworkRequests::BlockHeadersRequest{
        hashes: vec![next_block_hash],
        peer_id: peer.peer_info.id.clone(),
    }).await?;
    let mut headers = if let NetworkClientMessages::BlockHeaders(headers,_) = msg { headers } else { panic!("unexpected message"); };
    headers.sort_by_key(|h|h.height());
    if headers.len()==0 { return Err(anyhow!("invalid response: no headers")); }
    let start_height = headers[0].height();
    info!("SYNC start_height = {}, {} blocks to process",target_height,target_height-start_height);
    next_block_hash = headers.last().context("no headers")?.hash().clone();
    return anyhow::Ok(());
}

impl Cmd {
    fn parse_and_run() -> anyhow::Result<()> {
        let cmd = Self::parse();
        let start_block_hash = cmd.start_block_hash.parse::<CryptoHash>().map_err(|x|anyhow!(x.to_string()))?;
    
        let mut cache_dir = dirs::cache_dir().context("dirs::cache_dir() = None")?;
        cache_dir.push("near_configs");
        cache_dir.push(&cmd.chain_id);

        info!("downloading configs for chain {}",cmd.chain_id);
        let home_dir = cache_dir.as_path();
        download_configs(&cmd.chain_id,home_dir).context("Failed to initialize configs")?;

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
        return actix::System::new().block_on(async move {
            let config = near_config.client_config.clone(); 
            let network = start::start_with_config(
                home_dir,
                near_config,
                start_block_hash,
            ).context("start_with_config")?;

            let pool = futures::executor::ThreadPool::new()?;
            let handle = pool.spawn_with_handle(sync(pool.clone(),config,network,start_block_hash));

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
            // TODO: inform the handled funtion that we are stopping + handle.join();
            return Ok(()); 
        });
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
    if let Err(e) = Cmd::parse_and_run() {
        error!("Cmd::parse_and_run(): {:#}", e);
    }
}
