use log::{error, info, warn};

use crate::network;

struct ChainSync {
    network: Arc<network::Network>,
    
    // Currently it is equivalent to genesis_config.num_block_producer_seats,
    // (see https://cs.github.com/near/nearcore/blob/dae9553670de13c279d3ebd55f17da13d94fa691/nearcore/src/runtime/mod.rs#L1114).
    // AFAICT eventually it will change dynamically (I guess it will be provided in the Block).
    parts_per_chunk : u64,
}

impl ChainSync {
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
