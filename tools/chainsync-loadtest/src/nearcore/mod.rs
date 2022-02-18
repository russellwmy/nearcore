pub mod append_only_map;
pub mod config;
pub mod migrations;
mod runtime;
mod shard_tracker;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rand::{Rng};
use actix::{Actor, Arbiter};
use anyhow::Context;
// use tracing::{info};

use near_chain::ChainGenesis;
use crate::near_client::{ClientActor};

use near_client::{start_view_client};
use near_network::routing::start_routing_table_actor;
use near_network::test_utils::NetworkRecipient;
use near_network::PeerManagerActor;
use near_primitives::network::PeerId;
use near_primitives::epoch_manager::RngSeed;
use near_store::{Store,db};

pub use crate::nearcore::config::{
    init_configs, load_config, load_test_config, NearConfig, NEAR_BASE,
};
pub use crate::nearcore::runtime::NightshadeRuntime;
pub use crate::nearcore::shard_tracker::TrackedConfig;

pub fn get_default_home() -> PathBuf {
    if let Ok(near_home) = std::env::var("NEAR_HOME") {
        return near_home.into();
    }

    if let Some(mut home) = dirs::home_dir() {
        home.push(".near");
        return home;
    }

    PathBuf::default()
}

/// Returns random seed sampled from the current thread
pub fn random_seed_from_thread() -> RngSeed {
    let mut rng_seed: RngSeed = [0; 32];
    rand::thread_rng().fill(&mut rng_seed);
    rng_seed
}

pub fn start_with_config(home_dir: &Path, config: NearConfig) -> Result<(), anyhow::Error> {
    config.network_config.verify().with_context(|| "start_with_config")?;
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
    let client_actor = {
        let config = config.clone();
        let chain_genesis = chain_genesis.clone();
        let runtime = runtime.clone();
        let node_id = node_id.clone();
        let network_adapter = network_adapter.clone();
        ClientActor::start_in_arbiter(&Arbiter::current(), move |_ctx| {
            ClientActor::new(
                config.client_config,
                chain_genesis,
                runtime,
                node_id,
                network_adapter,
                true,
                random_seed_from_thread(),
            )
            .unwrap()
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
    return Ok(())
}
