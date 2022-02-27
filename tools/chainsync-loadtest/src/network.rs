use crate::concurrency::{Ctx,Dispatcher};

use near_network_primitives::types::{
    AccountIdOrPeerTrackingShard,
    PartialEncodedChunkRequestMsg,
    PartialEncodedChunkResponseMsg,
};

use near_primitives::block::{Block, BlockHeader};
use near_network::types::PeerManagerMessageRequest;
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
use governor::Quota;

type RateLimiter = governor::RateLimiter<
    governor::state::direct::NotKeyed,
    governor::state::InMemoryState,
    governor::clock::DefaultClock,
    governor::middleware::NoOpMiddleware
>; 



struct NetworkData {
    info_futures: Vec<(oneshot::Sender<Arc<NetworkInfo>>,usize)>,
    info_ : Arc<NetworkInfo>,
}

pub struct Network {
    network_adapter: Arc<dyn PeerManagerAdapter>,
    block_headers_disp: Dispatcher<CryptoHash,Vec<BlockHeader>>,
    block_disp: Dispatcher<CryptoHash,Block>,
    chunk_disp: Dispatcher<ChunkHash,PartialEncodedChunkResponseMsg>,
    data : Mutex<NetworkData>,

    // client_config.min_num_peers
    min_num_peers : usize,
    request_timeout : tokio::time::Duration,
    rate_limiter : RateLimiter,
}

impl Network {
    pub fn new(config:&NearConfig,network_adapter:Arc<dyn PeerManagerAdapter>) -> Arc<Network> {
        Arc::new(Network{
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

            min_num_peers: config.client_config.min_num_peers,
            rate_limiter: RateLimiter::direct(Quota::per_second(std::num::NonZeroU32::new(10).unwrap())),
            request_timeout: time::Duration::from_secs(2),
        })
    }
    
    async fn keep_sending(&self, ctx:Ctx, interval: time::Duration,new_req:Fn(PeerId) -> PeerManagerMessageRequest) -> anyhow::Result<()> {
        for peer in &self.network.info(ctx.clone()).await?.connected_peers {
            loop {
                self.network_adapter.do_send(new_req(peer));
                ctx.wait(interval).await?;
            }
        }
    }

    pub async fn info(&self, ctx:Ctx, min_peers:usize) -> anyhow::Result<Arc<NetworkInfo>> {
        let (send,recv) = oneshot::channel();
        {
            let mut n = self.data.lock().unwrap();
            if n.info_.num_connected_peers>=min_peers {
                let _ = send.send(n.info_.clone());
            } else {
                n.info_futures.push((send,min_peers));
            }
        }
        anyhow::Ok(ctx.wrap(recv).await??)
    }

    pub async fn fetch_block_headers(&self, ctx:Ctx, peer_id:PeerId, hash:CryptoHash) -> anyhow::Result<Vec<BlockHeader>> {
        self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
            NetworkRequests::BlockHeadersRequest{hashes:vec![hash.clone()],peer_id:peer_id.clone()},
        ));
        Ok(ctx.wrap(self.block_headers_disp.subscribe(&hash)).await?)
    }

    pub async fn fetch_block(&self, ctx:Ctx, peer_id:PeerId, hash:CryptoHash) -> anyhow::Result<Block> {
        self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
            NetworkRequests::BlockRequest{hash:hash.clone(),peer_id:peer_id.clone()},
        ));
        Ok(ctx.wrap(self.block_disp.subscribe(&hash)).await?)
    }

    pub async fn fetch_chunk(&self, ctx:Ctx, ch:&ShardChunkHeader, parts:Vec<u64>) -> anyhow::Result<PartialEncodedChunkResponseMsg> {
        self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
            NetworkRequests::PartialEncodedChunkRequest{
                target: AccountIdOrPeerTrackingShard {
                    account_id: None,
                    prefer_peer: true, 
                    shard_id: ch.shard_id(),
                    only_archival: false,
                    min_height: ch.height_included(),
                },
                request: PartialEncodedChunkRequestMsg {
                    chunk_hash: ch.chunk_hash(),
                    part_ords: parts, 
                    tracking_shards: Default::default(),
                },
            },
        ));
        Ok(ctx.wrap(self.chunk_disp.subscribe(&ch.chunk_hash())).await?)
    }

    fn notify(&self, msg : NetworkClientMessages) {
        let mut n = self.data.lock().unwrap();
        match msg {
            NetworkClientMessages::NetworkInfo(info) => {
                n.info_ = Arc::new(info);
                let it = n.info_futures.split_off(0);
                for (s,min) in it {
                    if n.info_.num_connected_peers>=min {
                        let _ = s.send(n.info_.clone());
                    } else {
                        info!("connected = {}/{}",n.info_.num_connected_peers,min);
                        n.info_futures.push((s,min));
                    }
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

impl Handler<NetworkClientMessages> for ClientActor {
    type Result = NetworkClientResponses;
    fn handle(&mut self, msg: NetworkClientMessages, _ctx: &mut Context<Self>) -> Self::Result {
        self.network.notify(msg);
        return NetworkClientResponses::NoResponse;
    }
}

