#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

mod start;
mod near_client;
mod async_ctx;
mod dispatcher;

use futures::future::FutureExt;
use futures::task::SpawnExt;
use core::future::Future;
use tokio::time;
use std::sync::{Arc,Mutex};
use std::str::FromStr;
use near_crypto::{Signer};
use anyhow::{anyhow,Context};
use near_primitives::version;
use openssl_probe;
use clap::{AppSettings, Clap};
use near_chain_configs::{ClientConfig,GenesisValidationMode};
use nearcore::config;
use std::path::Path;
use std::{env, io};
use tracing::metadata::LevelFilter;
use tracing::{info,error};
use tracing_subscriber::EnvFilter;
use near_primitives::hash::CryptoHash;
use near_primitives::network::{PeerId};
use near_network::types::{NetworkClientMessages,NetworkRequests};
use near_network_primitives::types::{
    AccountIdOrPeerTrackingShard,
    PartialEncodedChunkRequestMsg,
};
use async_ctx::{Ctx,AnyhowCast};

struct ChainSync {
    network: Arc<near_client::Network>,
    // client_config.min_num_peers
    min_num_peers : usize,
    // Currently it is equivalent to genesis_config.num_block_producer_seats,
    // (see https://cs.github.com/near/nearcore/blob/dae9553670de13c279d3ebd55f17da13d94fa691/nearcore/src/runtime/mod.rs#L1114).
    // AFAICT eventually it will change dynamically (I guess it will be provided in the Block).
    parts_per_chunk : u64,

    request_timeout : tokio::time::Duration,
}


impl ChainSync {
    async fn retry<'a,F,T>(&self,ctx:Ctx,make_future:impl Fn(Ctx,PeerId) -> F) -> anyhow::Result<T> where
        F : Future<Output=anyhow::Result<T>> + 'a
    {
        loop {
            for peer in &self.network.info(ctx.clone(),self.min_num_peers).await?.connected_peers {
                let res = make_future(ctx.with_timeout(self.request_timeout),peer.peer_info.id.clone()).await;
                if !res.matches(&async_ctx::Err::DeadlineExceeded) {
                    return res;
                }
                info!("SYNC deadline exceeded, retrying");
            }
        }
    }
    
    async fn run(self:Arc<ChainSync>, ctx:Ctx, start_block_hash: CryptoHash) -> anyhow::Result<()> {
        info!("SYNC start");
        let mut next_block_hash = start_block_hash;
        let peers = self.network.info(ctx.clone(),self.min_num_peers).await?;
        let peer = &peers.highest_height_peers[0];
        let target_height = peer.chain_info.height;
        info!("SYNC target_height = {}",target_height);
        let mut headers = self.retry(ctx.clone(),|ctx,peer_id|
            self.network.fetch_block_headers(ctx.clone(),peer_id,next_block_hash)
        ).await?;
        headers.sort_by_key(|h|h.height());
        let start_height = headers[0].height();
        info!("SYNC start_height = {}, {} blocks to process",start_height,target_height-start_height);
        next_block_hash = headers.last().context("no headers")?.hash().clone();
        ctx.scope(|ctx,s|async move {
            let self_ = self;
            for h in headers.drain(0..) {
                let self_ = self_.clone();
                s.spawn(|ctx|async move {
                    info!("SYNC requesting block #{}",h.height());
                    let block = self_.retry(ctx.clone(),|ctx,peer_id|
                        self_.network.fetch_block(ctx.clone(),peer_id,h.hash().clone())
                    ).await?;
                    info!("SYNC got block #{}, it has {} chunks",block.header().height(),block.chunks().len());
                    for chunk_header in block.chunks().iter() {
                        info!("SYNC requesting chunk {} of block #{} ({})",chunk_header.shard_id(),block.header().height(),chunk_header.chunk_hash().0);
                        let chunk = self_.network.fetch_chunk(ctx.clone(),chunk_header,(0..self_.parts_per_chunk).collect()).await?;
                        info!("SYNC got chunk {}, it has {} parts",chunk.chunk_hash.0,chunk.parts.len());
                    }
                    return anyhow::Ok(());
                });
            }
            return anyhow::Ok(());
        }).await
    }
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
                min_num_peers: near_config.client_config.min_num_peers,
                parts_per_chunk: near_config.genesis.config.num_block_producer_seats,
                network: start::start_with_config(
                    home_dir,
                    near_config,
                    start_block_hash,
                ).context("start_with_config")?,
                request_timeout: time::Duration::from_secs(2),
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
