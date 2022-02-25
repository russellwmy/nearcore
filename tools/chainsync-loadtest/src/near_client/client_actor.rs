//! Client actor orchestrates Client and facilitates network connection.
#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

use near_primitives::sharding::{ChunkHash, ShardChunkHeader};
use near_network_primitives::types::{
    AccountIdOrPeerTrackingShard,
    PartialEncodedChunkRequestMsg,
    PartialEncodedChunkResponseMsg,
};

use near_primitives::block::{Block, BlockHeader};
use near_network::types::PeerManagerMessageRequest;
use near_primitives::hash::CryptoHash;
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
    PeerManagerAdapter, NetworkRequests,
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
use std::sync::{Arc,Weak,Mutex,RwLock};
use tokio::sync::oneshot;
use tokio::sync;
use tokio::time;
use crate::async_ctx::Ctx;

#[derive(Clone)]
pub struct Network {
    data : Arc<Mutex<NetworkData>>
}

struct NetworkData {
    network_adapter: Arc<dyn PeerManagerAdapter>,
    info_futures: Vec<(oneshot::Sender<Arc<NetworkInfo>>,usize)>,
    
    block_headers_futures: std::collections::HashMap<(PeerId,CryptoHash),Vec<oneshot::Sender<Vec<BlockHeader>>>>,
    block_futures: std::collections::HashMap<(PeerId,CryptoHash),Vec<oneshot::Sender<Block>>>,
    chunk_futures: std::collections::HashMap<ChunkHash,Vec<oneshot::Sender<PartialEncodedChunkResponseMsg>>>,
    info_ : Arc<NetworkInfo>,
}

impl Network {
    pub fn new(network_adapter:Arc<dyn PeerManagerAdapter>) -> Network {
        Network{data:Arc::new(Mutex::new(NetworkData{
            network_adapter,
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
            block_futures: Default::default(),
            block_headers_futures: Default::default(),
            chunk_futures: Default::default(),
        }))}
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
        let (send,recv) = oneshot::channel();
        {
            let mut n = self.data.lock().unwrap();
            n.block_headers_futures.entry((peer_id.clone(),hash.clone())).or_default().push(send);
            n.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::BlockHeadersRequest{hashes:vec![hash],peer_id},
            ));
        }
        anyhow::Ok(ctx.wrap(recv).await??)
    }

    pub async fn fetch_block(&self, ctx:Ctx, peer_id:PeerId, hash:CryptoHash) -> anyhow::Result<Block> {
        let (send,recv) = oneshot::channel();
        {
            let mut n = self.data.lock().unwrap();
            n.block_futures.entry((peer_id.clone(),hash.clone())).or_default().push(send);
            n.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::BlockRequest{hash,peer_id},
            ));
        }
        anyhow::Ok(ctx.wrap(recv).await??)
    }

    pub async fn fetch_chunk(&self, ctx:Ctx, ch:&ShardChunkHeader, parts:Vec<u64>) -> anyhow::Result<PartialEncodedChunkResponseMsg> {
        let (send,recv) = oneshot::channel();
        {
            let mut n = self.data.lock().unwrap();
            n.chunk_futures.entry(ch.chunk_hash().clone()).or_default().push(send);
            n.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::PartialEncodedChunkRequest{
                    target: AccountIdOrPeerTrackingShard {
                        account_id: None,
                        prefer_peer: true, 
                        shard_id: ch.shard_id(),
                        only_archival: false,
                        min_height: ch.height_included(),
                    },
                    request: PartialEncodedChunkRequestMsg {
                        chunk_hash: ch.chunk_hash().clone(),
                        part_ords: parts, 
                        tracking_shards: Default::default(),
                    },
                },
            ));
        }
        anyhow::Ok(ctx.wrap(recv).await??)
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
                n.block_futures.entry((peer_id,block.hash().clone())).or_default().pop().map(|s|s.send(block));
            }
            NetworkClientMessages::BlockHeaders(headers,peer_id) => {
                if let Some(h) = headers.iter().min_by_key(|h|h.height()) {
                    let hash = h.prev_hash().clone();
                    n.block_headers_futures.entry((peer_id,hash)).or_default().pop().map(|s|s.send(headers));
                }
            }
            NetworkClientMessages::PartialEncodedChunkResponse(resp) => {
                n.chunk_futures.entry(resp.chunk_hash.clone()).or_default().pop().map(|s|s.send(resp));
            }
            _ => {},
        }
    }
}

pub struct ClientActor {
    client: Client,
    /// Identity that represents this Client at the network level.
    /// It is used as part of the messages that identify this client.
    node_id: PeerId,
    network : Network,
}

impl ClientActor {
    pub fn new(
        config: ClientConfig,
        chain_genesis: ChainGenesis,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        node_id: PeerId,
        network_adapter: Arc<dyn PeerManagerAdapter>,
        network: Network,
    ) -> Result<Self, Error> {
        let client = Client::new(
            config,
            chain_genesis,
            runtime_adapter,
            network_adapter.clone(),
        )?;

        Ok(ClientActor {
            client,
            node_id,
            network,
        })
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
        return Err("dupa123".into()); 
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
