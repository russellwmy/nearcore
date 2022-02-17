//! Client actor orchestrates Client and facilitates network connection.

use std::time;

use crate::near_client::client::Client;
use crate::near_client::StatusResponse;
use actix::{Actor, Context, Handler};
use log::{debug, error, info, warn};
use near_chain::chain::{
    ApplyStatePartsResponse, 
    BlockCatchUpResponse, StateSplitResponse,
};
use near_chain::test_utils::format_hash;
use near_chain::{ChainGenesis,RuntimeAdapter};
use near_chain_configs::ClientConfig;
use near_client_primitives::types::{
    Error, GetNetworkInfo, NetworkInfoResponse, Status,
    StatusError, StatusSyncInfo, SyncStatus,
};
use near_network::types::{
    NetworkClientMessages, NetworkClientResponses, NetworkInfo, NetworkRequests,
    PeerManagerAdapter, PeerManagerMessageRequest,
};
use near_performance_metrics;
use near_performance_metrics_macros::{perf, perf_with_debug};
use near_primitives::epoch_manager::RngSeed;
use near_primitives::hash::CryptoHash;
use near_primitives::network::{AnnounceAccount, PeerId};
use near_primitives::time::{Clock, Utc};
use near_primitives::unwrap_or_return;
use near_primitives::utils::{from_timestamp};
use near_primitives::validator_signer::ValidatorSigner;
use near_primitives::version::PROTOCOL_VERSION;
use near_primitives::views::ValidatorInfo;
use rand::seq::SliceRandom;
use rand::{thread_rng};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Multiplier on `max_block_time` to wait until deciding that chain stalled.
const STATUS_WAIT_TIME_MULTIPLIER: u64 = 10;

pub struct ClientActor {
    client: Client,
    network_adapter: Arc<dyn PeerManagerAdapter>,
    network_info: NetworkInfo,
    /// Identity that represents this Client at the network level.
    /// It is used as part of the messages that identify this client.
    node_id: PeerId,
    /// Last time we announced our accounts as validators.
    last_validator_announce_time: Option<Instant>,
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
        validator_signer: Option<Arc<dyn ValidatorSigner>>,
        enable_doomslug: bool,
        rng_seed: RngSeed,
    ) -> Result<Self, Error> {
        if let Some(vs) = &validator_signer {
            info!(target: "client", "Starting validator node: {}", vs.validator_id());
        }
        let client = Client::new(
            config,
            chain_genesis,
            runtime_adapter,
            network_adapter.clone(),
            validator_signer,
            enable_doomslug,
            rng_seed,
        )?;

        Ok(ClientActor {
            client,
            network_adapter,
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
            last_validator_announce_time: None,
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

    #[perf]
    fn handle(&mut self, msg: Status, _ctx: &mut Context<Self>) -> Self::Result {
        let head = self.client.chain.head()?;
        let head_header = self.client.chain.get_block_header(&head.last_block_hash)?;
        let latest_block_time = head_header.raw_timestamp();
        let latest_state_root = (*head_header.prev_state_root()).into();
        if msg.is_health_check {
            let now = Utc::now();
            let block_timestamp = from_timestamp(latest_block_time);
            if now > block_timestamp {
                let elapsed = (now - block_timestamp).to_std().unwrap();
                if elapsed
                    > Duration::from_millis(
                        self.client.config.max_block_production_delay.as_millis() as u64
                            * STATUS_WAIT_TIME_MULTIPLIER,
                    )
                {
                    return Err(StatusError::NoNewBlocks { elapsed });
                }
            }

            if self.client.sync_status.is_syncing() {
                return Err(StatusError::NodeIsSyncing);
            }
        }
        let validators = self
            .client
            .runtime_adapter
            .get_epoch_block_producers_ordered(&head.epoch_id, &head.last_block_hash)?
            .into_iter()
            .map(|(validator_stake, is_slashed)| ValidatorInfo {
                account_id: validator_stake.take_account_id(),
                is_slashed,
            })
            .collect();

        let protocol_version =
            self.client.runtime_adapter.get_epoch_protocol_version(&head.epoch_id)?;

        let validator_account_id =
            self.client.validator_signer.as_ref().map(|vs| vs.validator_id()).cloned();

        let mut earliest_block_hash = None;
        let mut earliest_block_height = None;
        let mut earliest_block_time = None;
        if let Some(earliest_block_hash_value) = self.client.chain.get_earliest_block_hash()? {
            earliest_block_hash = Some(earliest_block_hash_value);
            if let Ok(earliest_block) =
                self.client.chain.get_block_header(&earliest_block_hash_value)
            {
                earliest_block_height = Some(earliest_block.height());
                earliest_block_time = Some(earliest_block.timestamp());
            }
        }
        Ok(StatusResponse {
            version: self.client.config.version.clone(),
            protocol_version,
            latest_protocol_version: PROTOCOL_VERSION,
            chain_id: self.client.config.chain_id.clone(),
            rpc_addr: self.client.config.rpc_addr.clone(),
            validators,
            sync_info: StatusSyncInfo {
                latest_block_hash: head.last_block_hash.into(),
                latest_block_height: head.height,
                latest_state_root,
                latest_block_time: from_timestamp(latest_block_time),
                syncing: self.client.sync_status.is_syncing(),
                earliest_block_hash,
                earliest_block_height,
                earliest_block_time,
            },
            validator_account_id,
        })
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
    /// Check if client Account Id should be sent and send it.
    /// Account Id is sent when is not current a validator but are becoming a validator soon.
    fn check_send_announce_account(&mut self, prev_block_hash: CryptoHash) {
        // If no peers, there is no one to announce to.
        if self.network_info.num_connected_peers == 0 {
            debug!(target: "client", "No peers: skip account announce");
            return;
        }

        // First check that we currently have an AccountId
        let validator_signer = match self.client.validator_signer.as_ref() {
            None => return,
            Some(signer) => signer,
        };

        let now = Clock::instant();
        // Check that we haven't announced it too recently
        if let Some(last_validator_announce_time) = self.last_validator_announce_time {
            // Don't make announcement if have passed less than half of the time in which other peers
            // should remove our Account Id from their Routing Tables.
            if 2 * (now - last_validator_announce_time) < self.client.config.ttl_account_id_router {
                return;
            }
        }

        debug!(target: "client", "Check announce account for {}, last announce time {:?}", validator_signer.validator_id(), self.last_validator_announce_time);

        // Announce AccountId if client is becoming a validator soon.
        let next_epoch_id = unwrap_or_return!(self
            .client
            .runtime_adapter
            .get_next_epoch_id_from_prev_block(&prev_block_hash));

        // Check client is part of the futures validators
        if self.client.is_validator(&next_epoch_id, &prev_block_hash) {
            debug!(target: "client", "Sending announce account for {}", validator_signer.validator_id());
            self.last_validator_announce_time = Some(now);

            let signature = validator_signer.sign_account_announce(
                validator_signer.validator_id(),
                &self.node_id,
                &next_epoch_id,
            );
            self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::AnnounceAccount(AnnounceAccount {
                    account_id: validator_signer.validator_id().clone(),
                    peer_id: self.node_id.clone(),
                    epoch_id: next_epoch_id,
                    signature,
                }),
            ));
        }
    }

    /// Check whether need to (continue) sync.
    /// Also return higher height with known peers at that height.
    fn syncing_info(&self) -> Result<(bool, u64), near_chain::Error> {
        let head = self.client.chain.head()?;

        let full_peer_info = if let Some(full_peer_info) =
            self.network_info.highest_height_peers.choose(&mut thread_rng())
        {
            full_peer_info
        } else {
            if !self.client.config.skip_sync_wait {
                warn!(target: "client", "Sync: no peers available, disabling sync");
            }
            return Ok((false, 0));
        };

        let mut is_syncing = self.client.sync_status.is_syncing();
        if is_syncing {
            if full_peer_info.chain_info.height <= head.height {
                info!(target: "client", "Sync: synced at {} [{}], {}, highest height peer: {}",
                      head.height, format_hash(head.last_block_hash),
                      full_peer_info.peer_info.id, full_peer_info.chain_info.height
                );
                is_syncing = false;
            }
        } else {
            if full_peer_info.chain_info.height > head.height + self.client.config.sync_height_threshold {
                info!(
                    target: "client",
                    "Sync: height: {}, peer id/height: {}/{}, enabling sync",
                    head.height,
                    full_peer_info.peer_info.id,
                    full_peer_info.chain_info.height,
                );
                is_syncing = true;
            }
        }
        Ok((is_syncing, full_peer_info.chain_info.height))
    }
    
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
        let currently_syncing = self.client.sync_status.is_syncing();
        let (needs_syncing, highest_height) = self.syncing_info()?;

        if !needs_syncing {
            if currently_syncing {
                debug!(
                    target: "client",
                    "{:?} transitions to no sync",
                    self.client.validator_signer.as_ref().map(|vs| vs.validator_id()),
                );
                self.client.sync_status = SyncStatus::NoSync;

                // Initial transition out of "syncing" state.
                // Announce this client's account id if their epoch is coming up.
                let head = self.client.chain.head()?;
                self.check_send_announce_account(head.prev_block_hash);
            }
            return Ok(self.client.config.sync_check_period);
        }
        // Run each step of syncing separately.
        // header_sync.run() fails only if header_head() fails.
        // returns Ok whether or not headers are synced or requested for next headers
        self.client.header_sync.run(
            &mut self.client.sync_status,
            &mut self.client.chain,
            highest_height,
            &self.network_info.highest_height_peers
        )?;
        // Returns true if state_sync is needed (we gave up on block syncing).
        self.client.block_sync.run(
            &mut self.client.sync_status,
            &mut self.client.chain,
            highest_height,
            &self.network_info.highest_height_peers
        )?;
        return Ok(self.client.config.sync_step_period);
    }
}

impl Handler<ApplyStatePartsResponse> for ClientActor {
    type Result = ();

    fn handle(&mut self, msg: ApplyStatePartsResponse, _: &mut Self::Context) -> Self::Result {
        if let Some((sync, _, _)) = self.client.catchup_state_syncs.get_mut(&msg.sync_hash) {
            // We are doing catchup
            sync.set_apply_result(msg.shard_id, msg.apply_result);
        } else {
            self.client.state_sync.set_apply_result(msg.shard_id, msg.apply_result);
        }
    }
}

impl Handler<BlockCatchUpResponse> for ClientActor {
    type Result = ();

    fn handle(&mut self, msg: BlockCatchUpResponse, _: &mut Self::Context) -> Self::Result {
        if let Some((_, _, blocks_catch_up_state)) =
            self.client.catchup_state_syncs.get_mut(&msg.sync_hash)
        {
            let saved_store_update = blocks_catch_up_state
                .scheduled_blocks
                .remove(&msg.block_hash)
                .expect("block caught up, but is not in processing");
            blocks_catch_up_state
                .processed_blocks
                .insert(msg.block_hash, (saved_store_update, msg.results));
        } else {
            panic!("block catch up processing result from unknown sync hash");
        }
    }
}

impl Handler<StateSplitResponse> for ClientActor {
    type Result = ();

    fn handle(&mut self, msg: StateSplitResponse, _: &mut Self::Context) -> Self::Result {
        if let Some((sync, _, _)) = self.client.catchup_state_syncs.get_mut(&msg.sync_hash) {
            // We are doing catchup
            sync.set_split_result(msg.shard_id, msg.new_state_roots);
        } else {
            self.client.state_sync.set_split_result(msg.shard_id, msg.new_state_roots);
        }
    }
}
