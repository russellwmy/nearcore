#![allow(unused_imports)]
#![allow(unused_variables)]


use near_chain::{near_chain_primitives, ChainStoreAccess, Error};
use std::cmp::min;
use std::collections::{HashMap};
use std::ops::Add;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration as TimeDuration;

use ansi_term::Color::{Purple, Yellow};
use chrono::{DateTime, Duration};
use futures::{future, FutureExt};
use log::{debug, error, info, warn};
use rand::seq::{IteratorRandom, SliceRandom};
use rand::{thread_rng, Rng};

use near_chain::{Chain, RuntimeAdapter};
use near_network::types::{FullPeerInfo, NetworkRequests, NetworkResponses, PeerManagerAdapter};
use near_primitives::block::Tip;
use near_primitives::hash::CryptoHash;
use near_primitives::syncing::get_num_state_parts;
use near_primitives::time::{Clock, Utc};
use near_primitives::types::{
    AccountId, BlockHeight, BlockHeightDelta, ShardId, StateRoot,
};
use near_primitives::utils::to_timestamp;

use near_chain::chain::{ApplyStatePartsRequest, StateSplitRequest};
use near_client_primitives::types::{
    DownloadStatus, ShardSyncDownload, ShardSyncStatus, SyncStatus,
};
use near_network::types::PeerManagerMessageRequest;
use near_network_primitives::types::AccountOrPeerIdOrHash;
use near_primitives::shard_layout::ShardUId;

/// Maximum number of block headers send over the network.
pub const MAX_BLOCK_HEADERS: u64 = 512;

/// Maximum number of block header hashes to send as part of a locator.
pub const MAX_BLOCK_HEADER_HASHES: usize = 20;

/// Maximum number of state parts to request per peer on each round when node is trying to download the state.
pub const MAX_STATE_PART_REQUEST: u64 = 16;
/// Number of state parts already requested stored as pending.
/// This number should not exceed MAX_STATE_PART_REQUEST times (number of peers in the network).
pub const MAX_PENDING_PART: u64 = MAX_STATE_PART_REQUEST * 10000;

pub const NS_PER_SECOND: u128 = 1_000_000_000;

/// Helper to keep track of sync headers.
/// Handles major re-orgs by finding closest header that matches and re-downloading headers from that point.
pub struct HeaderSync {
    network_adapter: Arc<dyn PeerManagerAdapter>,
    syncing_peer: Option<FullPeerInfo>,
}

impl HeaderSync {
    pub fn new(
        network_adapter: Arc<dyn PeerManagerAdapter>,
        initial_timeout: TimeDuration,
        progress_timeout: TimeDuration,
        stall_ban_timeout: TimeDuration,
        expected_height_per_second: u64,
    ) -> Self {
        HeaderSync {
            network_adapter,
            syncing_peer: None,
        }
    }

    pub fn run(
        &mut self,
        chain: &mut Chain,
        highest_height: BlockHeight,
        highest_height_peers: &Vec<FullPeerInfo>,
    ) -> Result<(), near_chain::Error> {
        let header_head = chain.header_head()?;
        self.syncing_peer = None;
        if let Some(peer) = highest_height_peers.choose(&mut thread_rng()).cloned() {
            if peer.chain_info.height > header_head.height {
                self.syncing_peer = self.request_headers(chain, peer);
            }
        }
        Ok(())
    }

    /// Request headers from a given peer to advance the chain.
    fn request_headers(&mut self, chain: &mut Chain, peer: FullPeerInfo) -> Option<FullPeerInfo> {
        let tip = chain.header_head().ok()?;
        let locator = vec![tip.last_block_hash];
        debug!(target: "sync", "Sync: request headers: asking {} for headers, {:?}", peer.peer_info.id, locator);
        self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
            NetworkRequests::BlockHeadersRequest {
                hashes: locator,
                peer_id: peer.peer_info.id.clone(),
            },
        ));
        return Some(peer);
    }
}

pub struct BlockSyncRequest {
    height: BlockHeight,
    hash: CryptoHash,
}

/// Helper to track block syncing.
pub struct BlockSync {
    network_adapter: Arc<dyn PeerManagerAdapter>,
    last_request: Option<BlockSyncRequest>,
    /// Whether to enforce block sync
    archive: bool,
}

impl BlockSync {
    pub fn new(
        network_adapter: Arc<dyn PeerManagerAdapter>,
        archive: bool,
    ) -> Self {
        BlockSync { network_adapter, last_request: None, archive }
    }

    /// Runs check if block sync is needed, if it's needed and it's too far - sync state is started instead (returning true).
    /// Otherwise requests recent blocks from peers.
    pub fn run(
        &mut self,
        chain: &mut Chain,
        highest_height: BlockHeight,
        highest_height_peers: &[FullPeerInfo],
    ) -> Result<bool, near_chain::Error> {
        if self.block_sync(chain, highest_height_peers)? {
            debug!(target: "sync", "Sync: transition to State Sync.");
            return Ok(true);
        }
        Ok(false)
    }

    /// Returns true if state download is required (last known block is too far).
    /// Otherwise request recent blocks from peers round robin.
    pub fn block_sync(
        &mut self,
        chain: &mut Chain,
        highest_height_peers: &[FullPeerInfo],
    ) -> Result<bool, near_chain::Error> {
        let reference_hash = match &self.last_request {
            Some(request) if chain.is_chunk_orphan(&request.hash) => request.hash,
            _ => chain.head()?.last_block_hash,
        };

        let reference_hash = {
            // Find the most recent block we know on the canonical chain.
            // In practice the forks from the last final block are very short, so it is
            // acceptable to perform this on each request
            let header = chain.get_block_header(&reference_hash)?;
            let mut candidate = (header.height(), *header.hash(), *header.prev_hash());

            // First go back until we find the common block
            while match chain.get_header_by_height(candidate.0) {
                Ok(header) => header.hash() != &candidate.1,
                Err(e) => match e.kind() {
                    near_chain::ErrorKind::DBNotFoundErr(_) => true,
                    _ => return Err(e),
                },
            } {
                let prev_header = chain.get_block_header(&candidate.2)?;
                candidate = (prev_header.height(), *prev_header.hash(), *prev_header.prev_hash());
            }

            // Then go forward for as long as we known the next block
            let mut ret_hash = candidate.1;
            loop {
                match chain.mut_store().get_next_block_hash(&ret_hash) {
                    Ok(hash) => {
                        let hash = *hash;
                        if chain.block_exists(&hash)? {
                            ret_hash = hash;
                        } else {
                            break;
                        }
                    }
                    Err(e) => match e.kind() {
                        near_chain::ErrorKind::DBNotFoundErr(_) => break,
                        _ => return Err(e),
                    },
                }
            }

            ret_hash
        };

        let next_hash = match chain.mut_store().get_next_block_hash(&reference_hash) {
            Ok(hash) => *hash,
            Err(e) => match e.kind() {
                near_chain::ErrorKind::DBNotFoundErr(_) => {
                    return Ok(false);
                }
                _ => return Err(e),
            },
        };
        let next_height = chain.get_block_header(&next_hash)?.height();
        let request = BlockSyncRequest { height: next_height, hash: next_hash };

        let head = chain.head()?;
        let header_head = chain.header_head()?;

        let gc_stop_height = chain.runtime_adapter.get_gc_stop_height(&header_head.last_block_hash);

        let request_from_archival = self.archive && request.height < gc_stop_height;
        let peer = if request_from_archival {
            let archival_peer_iter = highest_height_peers.iter().filter(|p| p.chain_info.archival);
            archival_peer_iter.choose(&mut rand::thread_rng())
        } else {
            let peer_iter = highest_height_peers.iter();
            peer_iter.choose(&mut rand::thread_rng())
        };

        if let Some(peer) = peer {
            debug!(target: "sync", "Block sync: {}/{} requesting block {} from {} (out of {} peers)",
		   head.height, header_head.height, next_hash, peer.peer_info.id, highest_height_peers.len());
            self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::BlockRequest {
                    hash: request.hash,
                    peer_id: peer.peer_info.id.clone(),
                },
            ));
        } else {
            warn!(target: "sync", "Block sync: {}/{} No available {}peers to request block {} from",
		  head.height, header_head.height, if request_from_archival { "archival " } else { "" }, next_hash);
        }

        self.last_request = Some(request);

        Ok(false)
    }
}

