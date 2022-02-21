#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]
//! Client is responsible for tracking the chain, chunks, and producing them when needed.
//! This client works completely synchronously and must be operated by some async actor outside.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use rand::{Rng};

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
    pub runtime_adapter: Arc<dyn RuntimeAdapter>,
    pub shards_mgr: ShardsManager,
    /// A ReedSolomon instance to reconstruct shard.
    pub rs: ReedSolomonWrapper,
}

/// Returns random seed sampled from the current thread
pub fn random_seed_from_thread() -> RngSeed {
    let mut rng_seed: RngSeed = [0; 32];
    rand::thread_rng().fill(&mut rng_seed);
    rng_seed
}

impl Client {
    pub fn new(
        config: ClientConfig,
        chain_genesis: ChainGenesis,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        network_adapter: Arc<dyn PeerManagerAdapter>,
    ) -> Result<Self, Error> {
        let chain = Chain::new(runtime_adapter.clone(), &chain_genesis, DoomslugThresholdMode::TwoThirds)?;
        let shards_mgr = ShardsManager::new(
            None,
            runtime_adapter.clone(),
            network_adapter.clone(),
            random_seed_from_thread(),
        );
        let genesis_block = chain.genesis_block();
        let data_parts = runtime_adapter.num_data_parts();
        let parity_parts = runtime_adapter.num_total_parts() - data_parts;

        Ok(Self {
            config,
            chain,
            runtime_adapter,
            shards_mgr,
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
