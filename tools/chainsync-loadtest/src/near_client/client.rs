#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]
//! Client is responsible for tracking the chain, chunks, and producing them when needed.
//! This client works completely synchronously and must be operated by some async actor outside.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use near_primitives::time::Clock;

use near_chain::chain::{
    BlockMissingChunks, OrphanMissingChunks, TX_ROUTING_HEIGHT_HORIZON,
};
use near_chain::test_utils::format_hash;
use near_chain::types::{AcceptedBlock, LatestKnown};
use near_chain::{
    BlockStatus, Chain, ChainGenesis, ChainStoreAccess, Doomslug, DoomslugThresholdMode, ErrorKind,
    Provenance, RuntimeAdapter,
};
use near_chain_configs::ClientConfig;
use near_chunks::{ProcessPartialEncodedChunkResult, ShardsManager};
use near_network::types::{
    NetworkClientResponses, NetworkRequests, PeerManagerAdapter,
};
use near_primitives::block::{Approval, ApprovalInner, ApprovalMessage, Block, BlockHeader, Tip};
use near_primitives::challenge::{Challenge, ChallengeBody};
use near_primitives::hash::CryptoHash;
use near_primitives::sharding::{
    PartialEncodedChunk, PartialEncodedChunkV2, ReedSolomonWrapper,
    ShardChunkHeader, 
};
use near_primitives::transaction::SignedTransaction;
use near_primitives::types::{AccountId, ApprovalStake, BlockHeight, EpochId, NumBlocks, ShardId};
use near_primitives::unwrap_or_return;
use near_primitives::utils::{to_timestamp, MaybeValidated};

use crate::near_client::sync::{BlockSync, HeaderSync};
use near_client_primitives::types::{Error};
use near_network::types::PeerManagerMessageRequest;
use near_network_primitives::types::{
    PartialEncodedChunkForwardMsg, PartialEncodedChunkResponseMsg,
};
use near_primitives::block_header::ApprovalType;
use near_primitives::epoch_manager::RngSeed;
use near_primitives::version::PROTOCOL_VERSION;

pub struct Client {
    pub config: ClientConfig,
    pub chain: Chain,
    pub doomslug: Doomslug,
    pub runtime_adapter: Arc<dyn RuntimeAdapter>,
    pub shards_mgr: ShardsManager,
    /// Keeps track of syncing headers.
    pub header_sync: HeaderSync,
    /// Keeps track of syncing block.
    pub block_sync: BlockSync,
    /// A ReedSolomon instance to reconstruct shard.
    pub rs: ReedSolomonWrapper,
}

impl Client {
    pub fn new(
        config: ClientConfig,
        chain_genesis: ChainGenesis,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        network_adapter: Arc<dyn PeerManagerAdapter>,
        enable_doomslug: bool,
        rng_seed: RngSeed,
    ) -> Result<Self, Error> {
        let doomslug_threshold_mode = if enable_doomslug {
            DoomslugThresholdMode::TwoThirds
        } else {
            DoomslugThresholdMode::NoApprovals
        };
        let chain = Chain::new(runtime_adapter.clone(), &chain_genesis, doomslug_threshold_mode)?;
        let shards_mgr = ShardsManager::new(
            None,
            runtime_adapter.clone(),
            network_adapter.clone(),
            rng_seed,
        );
        let genesis_block = chain.genesis_block();
        let header_sync = HeaderSync::new(
            network_adapter.clone(),
            config.header_sync_initial_timeout,
            config.header_sync_progress_timeout,
            config.header_sync_stall_ban_timeout,
            config.header_sync_expected_height_per_second,
        );
        let block_sync =
            BlockSync::new(network_adapter.clone(), config.archive);
        let data_parts = runtime_adapter.num_data_parts();
        let parity_parts = runtime_adapter.num_total_parts() - data_parts;

        let doomslug = Doomslug::new(
            chain.store().largest_target_height()?,
            config.min_block_production_delay,
            config.max_block_production_delay,
            config.max_block_production_delay / 10,
            config.max_block_wait_delay,
            None,
            doomslug_threshold_mode,
        );
        Ok(Self {
            #[cfg(feature = "test_features")]
            adv_produce_blocks: false,
            #[cfg(feature = "test_features")]
            adv_produce_blocks_only_valid: false,
            config,
            chain,
            doomslug,
            runtime_adapter,
            shards_mgr,
            header_sync,
            block_sync,
            rs: ReedSolomonWrapper::new(data_parts, parity_parts),
        })
    }

    pub fn request_missing_chunks(
        &mut self,
        blocks_missing_chunks: Vec<BlockMissingChunks>,
        orphans_missing_chunks: Vec<OrphanMissingChunks>,
    ) {
        for BlockMissingChunks { prev_hash, missing_chunks } in blocks_missing_chunks {
            self.shards_mgr.request_chunks(
                missing_chunks,
                prev_hash,
                &self
                    .chain
                    .header_head()
                    .expect("header_head must be available when processing a block"),
            );
        }

        for OrphanMissingChunks { missing_chunks, epoch_id, ancestor_hash } in
            orphans_missing_chunks
        {
            self.shards_mgr.request_chunks_for_orphan(
                missing_chunks,
                &epoch_id,
                ancestor_hash,
                &self
                    .chain
                    .header_head()
                    .expect("header_head must be available when processing a block"),
            );
        }
    }
}
