#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

mod network;
mod concurrency;
mod fetch_chain;

use std::path::{Path};
use std::sync::{Arc,Mutex};
use std::str::FromStr;
use std::{env, io};

use anyhow::{anyhow,Context};
use clap::{AppSettings, Clap};
use core::future::Future;
use futures::future::FutureExt;
use futures::task::SpawnExt;
use tokio::time;
use actix::{Actor, Arbiter};
use openssl_probe;
use tracing::metadata::LevelFilter;
use tracing::{info,error};
use tracing_subscriber::EnvFilter;

use concurrency::{Ctx,AnyhowCast};
use network::{ClientActor,Network};

use near_chain::ChainGenesis;
use near_client::{start_view_client};
use near_network::routing::start_routing_table_actor;
use near_network::test_utils::NetworkRecipient;
use near_network::PeerManagerActor;
use near_primitives::network::PeerId;
use near_store::{Store,db};
use nearcore::config::NearConfig;
use nearcore::NightshadeRuntime;
use near_crypto::{Signer};
use near_primitives::version;
use near_chain_configs::{ClientConfig,GenesisValidationMode};
use nearcore::config;
use near_primitives::hash::CryptoHash;
use near_network::types::{NetworkClientMessages,NetworkRequests};
use near_network_primitives::types::{
    AccountIdOrPeerTrackingShard,
    PartialEncodedChunkRequestMsg,
};

pub fn start_with_config(home_dir: &Path, config: NearConfig) -> anyhow::Result<Arc<Network>> {
    config.network_config.verify().context("start_with_config")?;
    let node_id = PeerId::new(config.network_config.public_key.clone());
    let chain_genesis = ChainGenesis::from(&config.genesis);
    let store = Store::new(Arc::new(db::TestDB::new()));

    let runtime = Arc::new(NightshadeRuntime::with_config(
        home_dir,
        store.clone(),
        &config,
        config.client_config.trie_viewer_state_size_limit,
        config.client_config.max_gas_burnt_view,
    ));

    let network_adapter = Arc::new(NetworkRecipient::default());

    let view_client = start_view_client(
        config.validator_signer.as_ref().map(|signer| signer.validator_id().clone()),
        chain_genesis.clone(),
        runtime.clone(),
        network_adapter.clone(),
        config.client_config.clone(),
    ).recipient();
    let network = Network::new(config,network_adapter.clone()); 
    let client_actor = {
        let network = network.clone();
        ClientActor::start_in_arbiter(&Arbiter::new().handle(), move |_ctx| {
            ClientActor::new(network)
        }).recipient()
    };

    let routing_table_addr = start_routing_table_actor(node_id, store.clone());
    let network_actor = PeerManagerActor::start_in_arbiter(&Arbiter::new().handle(), move |_ctx| {
        PeerManagerActor::new(
            store,
            config.network_config,
            client_actor,
            view_client,
            routing_table_addr,
        )
        .unwrap()
    }).recipient();
    network_adapter.set_recipient(network_actor);
    return Ok(network)
}

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
    pub start_block_hash : String,
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
        // Dropping Runtime is blocking, while futures should never be blocking.
        // Tokio has a runtime check which panics if you drop tokio Runtime from a future executed
        // on another Tokio runtime.
        // To avoid that, we create a runtime within the synchronous code and pass just an Arc
        // inside of it.
        let rt_ = Arc::new(tokio::runtime::Runtime::new()?);
        let rt = rt_.clone();
        return actix::System::new().block_on(async move {
            let chain_sync = Arc::new(ChainSync{
                parts_per_chunk: near_config.genesis.config.num_block_producer_seats,
                network: start_with_config(
                    home_dir,
                    near_config,
                ).context("start_with_config")?,
            });
            // We execute the chain_sync on a totally separate set of system threads to minimize
            // the interaction with actix.
            let (ctx,cancel) = Ctx::background().with_cancel();
            let handle = rt.spawn(chain_sync.run(ctx,start_block_hash));

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
            cancel();
            handle.await??;
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
