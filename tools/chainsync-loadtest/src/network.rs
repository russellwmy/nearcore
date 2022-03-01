use std::sync::atomic::{AtomicU64,Ordering};

use crate::concurrency::{Ctx,Dispatcher,Scope,RateLimiter};

use near_network_primitives::types::{
    AccountIdOrPeerTrackingShard,
    PartialEncodedChunkRequestMsg,
    PartialEncodedChunkResponseMsg,
    NetworkViewClientMessages, NetworkViewClientResponses,
};

use std::future::Future;
use nearcore::config::NearConfig;
use near_primitives::sharding::{ChunkHash,ShardChunkHeader};
use near_primitives::block::{Block, BlockHeader, GenesisId};
use near_primitives::hash::CryptoHash;
use near_client_primitives::types::StatusResponse;
use actix::{Actor, Context, Handler};
use log::{error, info, warn};
use near_chain::{ChainGenesis,RuntimeAdapter};
use near_chain_configs::ClientConfig;
use near_client_primitives::types::{
    Error, GetNetworkInfo, NetworkInfoResponse, Status,
    StatusError, StatusSyncInfo, 
};
use near_network::types::{
    NetworkClientMessages, NetworkClientResponses, NetworkInfo,
    PeerManagerAdapter, NetworkRequests,
    PeerManagerMessageRequest,FullPeerInfo,
};
use near_performance_metrics;
use near_performance_metrics_macros::{perf, perf_with_debug};
use near_primitives::epoch_manager::RngSeed;
use near_primitives::network::{PeerId};
use near_primitives::utils::{from_timestamp};
use near_primitives::validator_signer::ValidatorSigner;
use near_primitives::version::PROTOCOL_VERSION;
use near_primitives::views::ValidatorInfo;
use rand::seq::SliceRandom;
use rand::{thread_rng};
use std::sync::{Arc,Weak,Mutex,RwLock};
use tokio::sync::oneshot;
use tokio::sync;
use tokio::time;

fn genesis_hash(chain_id:&str) -> CryptoHash {
	return match chain_id {
		"mainnet" => "EPnLgE7iEq9s7yTkos96M3cWymH5avBAPm3qx3NXqR8H",
		"testnet" => "FWJ9kR6KFWoyMoNjpLXXGHeuiy7tEY6GmoFeCA5yuc6b",
		"betanet" => "6hy7VoEJhPEUaJr1d5ePBhKdgeDWKCjLoUAn7XS9YPj",
		_ => { return Default::default(); },
	}.parse().unwrap()
}

#[derive(Default,Debug)]
pub struct Stats {
    msgs_sent: AtomicU64,
    
    header_req : AtomicU64,
    header_resp : AtomicU64,
    block_req : AtomicU64,
    block_resp : AtomicU64,
    chunk_req : AtomicU64,
    chunk_resp : AtomicU64,
}

struct NetworkData {
    info_futures: Vec<oneshot::Sender<Arc<NetworkInfo>>>,
    info_ : Arc<NetworkInfo>,
}

pub struct Network {
    pub stats: Stats,
    network_adapter: Arc<dyn PeerManagerAdapter>,
    block_headers_disp: Dispatcher<CryptoHash,Vec<BlockHeader>>,
    block_disp: Dispatcher<CryptoHash,Block>,
    chunk_disp: Dispatcher<ChunkHash,PartialEncodedChunkResponseMsg>,
    data : Mutex<NetworkData>,

    chain_id : String,
    // client_config.min_num_peers
    min_peers : usize,
    // Currently it is equivalent to genesis_config.num_block_producer_seats,
    // (see https://cs.github.com/near/nearcore/blob/dae9553670de13c279d3ebd55f17da13d94fa691/nearcore/src/runtime/mod.rs#L1114).
    // AFAICT eventually it will change dynamically (I guess it will be provided in the Block).
    parts_per_chunk : u64,
    
    request_timeout : tokio::time::Duration,
    rate_limiter : RateLimiter,
}

impl Network {
    pub fn new(config:&NearConfig,network_adapter:Arc<dyn PeerManagerAdapter>) -> Arc<Network> {
        Arc::new(Network{
            stats: Default::default(),
            network_adapter,
            data: Mutex::new(NetworkData{
                info_ : Arc::new(NetworkInfo{
                    connected_peers: vec![],
                    num_connected_peers: 0,
                    peer_max_count: 0,
                    highest_height_peers: vec![],
                    sent_bytes_per_sec: 0,
                    received_bytes_per_sec: 0,
                    known_producers: vec![],
                    peer_counter: 0,        
                }),
                info_futures: Default::default(),
            }),
            block_disp: Default::default(),
            block_headers_disp: Default::default(),
            chunk_disp: Default::default(),

            chain_id: config.client_config.chain_id.clone(),
            min_peers: config.client_config.min_num_peers,
            parts_per_chunk: config.genesis.config.num_block_producer_seats,
            rate_limiter : RateLimiter::new(time::Duration::from_secs(1)/10,10),
            request_timeout: time::Duration::from_secs(2),
        })
    }
    
    fn keep_sending(self:&Arc<Self>, ctx:&Ctx, new_req:impl Fn(FullPeerInfo) -> NetworkRequests) -> impl Future<Output=anyhow::Result<()>> {
        let self_ = self.clone();
        let ctx = ctx.with_label("keep_sending");
        async move {
            loop {
                // TODO: shuffle the peer order.
                for peer in &self_.info(&ctx).await?.connected_peers {
                    // TODO: rate limit per peer.
                    self_.rate_limiter.allow(&ctx).await?;
                    self_.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                        new_req(peer.clone())
                    ));
                    self_.stats.msgs_sent.fetch_add(1,Ordering::Relaxed);
                    ctx.wait(self_.request_timeout).await?;
                }
            }
        }
    }

    pub async fn info(self:&Arc<Self>, ctx:&Ctx) -> anyhow::Result<Arc<NetworkInfo>> {
        let (send,recv) = oneshot::channel();
        {
            let mut n = self.data.lock().unwrap();
            if n.info_.num_connected_peers>=self.min_peers {
                let _ = send.send(n.info_.clone());
            } else {
                n.info_futures.push(send);
            }
        }
        anyhow::Ok(ctx.wrap(recv).await??)
    }

    pub async fn fetch_block_headers(self:&Arc<Self>, ctx:&Ctx, hash:&CryptoHash) -> anyhow::Result<Vec<BlockHeader>> {
        let hash = hash.clone();
        let recv = self.block_headers_disp.subscribe(&hash);
        let (ctx,cancel) = ctx.with_label("fetch_block_headers").with_cancel();
        self.stats.header_req.fetch_add(1,Ordering::Relaxed);
        let handle = tokio::spawn(self.keep_sending(&ctx,move|peer|{
            NetworkRequests::BlockHeadersRequest{
                hashes: vec![hash.clone()],
                peer_id: peer.peer_info.id.clone(),
            }
        }));
        let res = ctx.wrap(recv).await?;
        self.stats.header_resp.fetch_add(1,Ordering::Relaxed);
        cancel();
        let _ = handle.await?;
        anyhow::Ok(res)
    }

    pub async fn fetch_block(self:&Arc<Self>, ctx:&Ctx, hash:&CryptoHash) -> anyhow::Result<Block> {
        let hash = hash.clone();
        let recv = self.block_disp.subscribe(&hash);
        let (ctx,cancel) = ctx.with_label("fetch_block").with_cancel();
        self.stats.block_req.fetch_add(1,Ordering::Relaxed);
        let handle = tokio::spawn(self.keep_sending(&ctx,move|peer|{
            NetworkRequests::BlockRequest{
                hash:hash.clone(),
                peer_id: peer.peer_info.id.clone(),
            }
        }));
        let res = ctx.wrap(recv).await?;
        cancel();
        let _ = handle.await?;
        self.stats.block_resp.fetch_add(1,Ordering::Relaxed);
        anyhow::Ok(res)
    }

    pub async fn fetch_chunk(self:&Arc<Self>, ctx:&Ctx, ch:&ShardChunkHeader) -> anyhow::Result<PartialEncodedChunkResponseMsg> {
        let ch = ch.clone();
        let recv = self.chunk_disp.subscribe(&ch.chunk_hash());
        self.stats.chunk_req.fetch_add(1,Ordering::Relaxed);
        let (ctx,cancel) = ctx.with_label("fetch_chunk").with_cancel();
        let handle = tokio::spawn(self.keep_sending(&ctx,{
            let ppc = self.parts_per_chunk;
            move|peer|{
                NetworkRequests::PartialEncodedChunkRequest{
                    target: AccountIdOrPeerTrackingShard {
                        account_id: peer.peer_info.account_id,
                        prefer_peer: true, 
                        shard_id: ch.shard_id(),
                        only_archival: false,
                        min_height: ch.height_included(),
                    },
                    request: PartialEncodedChunkRequestMsg {
                        chunk_hash: ch.chunk_hash(),
                        part_ords: (0..ppc).collect(), 
                        tracking_shards: Default::default(),
                    },
                }
            }
        }));
        let res = ctx.wrap(recv).await?;
        cancel();
        let _ = handle.await?;
        self.stats.chunk_resp.fetch_add(1,Ordering::Relaxed);
        anyhow::Ok(res)
    }

    fn notify(&self, msg : NetworkClientMessages) {
        let mut n = self.data.lock().unwrap();
        match msg {
            NetworkClientMessages::NetworkInfo(info) => {
                n.info_ = Arc::new(info);
                if n.info_.num_connected_peers<self.min_peers {
                    info!("connected = {}/{}",n.info_.num_connected_peers,self.min_peers);
                    return;
                }
                for s in n.info_futures.split_off(0) {
                    s.send(n.info_.clone()).unwrap();
                }
            }
            NetworkClientMessages::Block(block,peer_id,_) => {
                self.block_disp.send(&block.hash().clone(),block);
            }
            NetworkClientMessages::BlockHeaders(headers,peer_id) => {
                if let Some(h) = headers.iter().min_by_key(|h|h.height()) {
                    let hash = h.prev_hash().clone();
                    self.block_headers_disp.send(&hash,headers);
                }
            }
            NetworkClientMessages::PartialEncodedChunkResponse(resp) => {
                self.chunk_disp.send(&resp.chunk_hash.clone(),resp);
            }
            _ => {},
        }
    }
}

pub struct ClientActor {
    network : Arc<Network>,
}

impl ClientActor {
    pub fn new(network: Arc<Network>) -> Self {
        ClientActor{network}
    }
}

impl Actor for ClientActor {
    type Context = Context<Self>;
}


impl Handler<NetworkViewClientMessages> for ClientActor {
    type Result = NetworkViewClientResponses;
    fn handle(&mut self, msg: NetworkViewClientMessages, _ctx: &mut Self::Context) -> Self::Result {
        let name = match msg {
			NetworkViewClientMessages::TxStatus{..} => {"TxStatus"} 
			NetworkViewClientMessages::TxStatusResponse(_) => {"TxStatusResponse"}
			NetworkViewClientMessages::ReceiptOutcomeRequest(_) => {"ReceiptOutcomeRequest"}
			NetworkViewClientMessages::ReceiptOutcomeResponse(_) => {"ReceiptOutputResponse"}
			NetworkViewClientMessages::BlockRequest(_) =>  {"BlockRequest"}
			NetworkViewClientMessages::BlockHeadersRequest(_) => {"BlockHeadersRequest"}
			NetworkViewClientMessages::StateRequestHeader{..} => {"StateRequestHeader"}
			NetworkViewClientMessages::StateRequestPart{..}=> {"StateRequestPart"}
			NetworkViewClientMessages::EpochSyncRequest{..}=> {"EpochSyncRequest"}
			NetworkViewClientMessages::EpochSyncFinalizationRequest{..}=>{"EpochSyncFinalizationRequest"}
			NetworkViewClientMessages::GetChainInfo => {
				return NetworkViewClientResponses::ChainInfo {
					genesis_id: GenesisId {
						chain_id: self.network.chain_id.clone(), 
						hash: genesis_hash(&self.network.chain_id), 
					},
					height: 0, 
					tracked_shards: Default::default(), 
					archival: false, 
				}
			}
			NetworkViewClientMessages::AnnounceAccount(_) => {"AnnounceAccount"}
		};
		info!("view_request: {}",name);
        return NetworkViewClientResponses::NoResponse;
    }
}

impl Handler<NetworkClientMessages> for ClientActor {
    type Result = NetworkClientResponses;
    fn handle(&mut self, msg: NetworkClientMessages, _ctx: &mut Context<Self>) -> Self::Result {
        self.network.notify(msg);
        return NetworkClientResponses::NoResponse;
    }
}

