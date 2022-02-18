//! Client actor orchestrates Client and facilitates network connection.
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

use std::time;

use crate::near_client::client::Client;
use crate::near_client::StatusResponse;
use actix::{Actor, Context, Handler};
use log::{error, info, warn};
use near_chain::chain::{
    ApplyStatePartsResponse, 
    BlockCatchUpResponse, StateSplitResponse,
};
use near_chain::test_utils::format_hash;
use near_chain::{ChainGenesis,RuntimeAdapter};
use near_chain_configs::ClientConfig;
use near_client_primitives::types::{
    Error, GetNetworkInfo, NetworkInfoResponse, Status,
    StatusError, StatusSyncInfo, 
};
use near_network::types::{
    NetworkClientMessages, NetworkClientResponses, NetworkInfo, 
    PeerManagerAdapter, 
};
use near_performance_metrics;
use near_performance_metrics_macros::{perf, perf_with_debug};
use near_primitives::epoch_manager::RngSeed;
use near_primitives::network::{PeerId};
use near_primitives::time::{Utc};
use near_primitives::utils::{from_timestamp};
use near_primitives::validator_signer::ValidatorSigner;
use near_primitives::version::PROTOCOL_VERSION;
use near_primitives::views::ValidatorInfo;
use rand::seq::SliceRandom;
use rand::{thread_rng};
use std::sync::Arc;
use std::time::{Duration};

pub struct ClientActor {
    client: Client,
    network_info: NetworkInfo,
    /// Identity that represents this Client at the network level.
    /// It is used as part of the messages that identify this client.
    node_id: PeerId,
    sync_started: bool,

    #[cfg(feature = "sandbox")]
    fastforward_delta: Option<near_primitives::types::BlockHeightDelta>,
}

impl ClientActor {
    pub fn new(
        config: ClientConfig,
        chain_genesis: ChainGenesis,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        node_id: PeerId,
        network_adapter: Arc<dyn PeerManagerAdapter>,
        enable_doomslug: bool,
        rng_seed: RngSeed,
    ) -> Result<Self, Error> {
        let client = Client::new(
            config,
            chain_genesis,
            runtime_adapter,
            network_adapter.clone(),
            enable_doomslug,
            rng_seed,
        )?;

        Ok(ClientActor {
            client,
            node_id,
            network_info: NetworkInfo {
                connected_peers: vec![],
                num_connected_peers: 0,
                peer_max_count: 0,
                highest_height_peers: vec![],
                received_bytes_per_sec: 0,
                sent_bytes_per_sec: 0,
                known_producers: vec![],
                peer_counter: 0,
            },
            sync_started: false,
        })
    }
}

impl Actor for ClientActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Start syncing job.
        self.sync_loop(ctx);
    }
}

impl Handler<NetworkClientMessages> for ClientActor {
    type Result = NetworkClientResponses;

    #[perf_with_debug]
    fn handle(&mut self, msg: NetworkClientMessages, _ctx: &mut Context<Self>) -> Self::Result {
        match msg {
            NetworkClientMessages::Block(_block, _peer_id, _was_requested) => {
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::BlockHeaders(_headers, _peer_id) => {
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::Transaction{..} => {NetworkClientResponses::NoResponse}
            NetworkClientMessages::BlockApproval(_approval, _peer_id) => {NetworkClientResponses::NoResponse}
            NetworkClientMessages::StateResponse(_state_response_info) => {NetworkClientResponses::NoResponse}
            NetworkClientMessages::EpochSyncResponse(_peer_id, _response) => {NetworkClientResponses::NoResponse}
            NetworkClientMessages::EpochSyncFinalizationResponse(_peer_id, _response) => {NetworkClientResponses::NoResponse}
            NetworkClientMessages::PartialEncodedChunkRequest(_part_request_msg, _route_back) => {NetworkClientResponses::NoResponse}
            NetworkClientMessages::PartialEncodedChunkResponse(_response) => {
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::PartialEncodedChunk(_partial_encoded_chunk) => {
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::PartialEncodedChunkForward(_forward) => {NetworkClientResponses::NoResponse}
            NetworkClientMessages::Challenge(_challenge) => {NetworkClientResponses::NoResponse}
            NetworkClientMessages::NetworkInfo(network_info) => {
                self.network_info = network_info;
                NetworkClientResponses::NoResponse
            }
        }
    }
}

impl Handler<Status> for ClientActor {
    type Result = Result<StatusResponse, StatusError>;
    fn handle(&mut self, msg: Status, _ctx: &mut Context<Self>) -> Self::Result {
        return Err(StatusError::NodeIsSyncing);
    }
}

impl Handler<GetNetworkInfo> for ClientActor {
    type Result = Result<NetworkInfoResponse, String>;

    #[perf]
    fn handle(&mut self, _msg: GetNetworkInfo, _ctx: &mut Context<Self>) -> Self::Result {
        Ok(NetworkInfoResponse {
            connected_peers: (self.network_info.connected_peers.iter())
                .map(|fpi| fpi.peer_info.clone())
                .collect(),
            num_connected_peers: self.network_info.num_connected_peers,
            peer_max_count: self.network_info.peer_max_count,
            sent_bytes_per_sec: self.network_info.sent_bytes_per_sec,
            received_bytes_per_sec: self.network_info.received_bytes_per_sec,
            known_producers: self.network_info.known_producers.clone(),
        })
    }
}

impl ClientActor {
    /// Starts syncing and then switches to either syncing or regular mode.
    fn sync_loop(&mut self, ctx: &mut Context<ClientActor>) {
        let wait_period = match self.sync() {
            Ok(wait_period) => wait_period,
            Err(err) => {
                error!(target: "sync", "Sync: Unexpected error: {}", err);
                self.client.config.sync_step_period
            }
        };
        near_performance_metrics::actix::run_later(ctx, wait_period, |act, ctx| act.sync_loop(ctx));
    }

    /// Main syncing job responsible for syncing client with other peers.
    /// Runs itself iff it was not ran as reaction for message with results of
    /// finishing state part job
    fn sync(&mut self) -> anyhow::Result<time::Duration> {
        if !self.sync_started {
            // Wait for connections reach at least minimum peers unless skipping sync.
            if self.network_info.num_connected_peers < self.client.config.min_num_peers && !self.client.config.skip_sync_wait {
                return Ok(self.client.config.sync_step_period); // TODO: make it an error
            }
            self.sync_started = true;
        }
        let head = self.client.chain.head()?;
        let peer = self.network_info.highest_height_peers.choose(&mut thread_rng()).ok_or::<Error>("no peers".to_string().into())?;
        let highest_height = peer.chain_info.height;

        // Run each step of syncing separately.
        // header_sync.run() fails only if header_head() fails.
        // returns Ok whether or not headers are synced or requested for next headers
        self.client.header_sync.run(
            &mut self.client.chain,
            highest_height,
            &self.network_info.highest_height_peers
        )?;
        // Returns true if state_sync is needed (we gave up on block syncing).
        self.client.block_sync.run(
            &mut self.client.chain,
            highest_height,
            &self.network_info.highest_height_peers
        )?;
        return Ok(self.client.config.sync_step_period);
    }
}

impl Handler<ApplyStatePartsResponse> for ClientActor {
    type Result = ();
    fn handle(&mut self, msg: ApplyStatePartsResponse, _: &mut Self::Context) -> Self::Result {}
}

impl Handler<BlockCatchUpResponse> for ClientActor {
    type Result = ();
    fn handle(&mut self, msg: BlockCatchUpResponse, _: &mut Self::Context) -> Self::Result {}
}

impl Handler<StateSplitResponse> for ClientActor {
    type Result = ();
    fn handle(&mut self, msg: StateSplitResponse, _: &mut Self::Context) -> Self::Result {}
}
