use std::sync::{Arc};

use log::{error, info, warn};

use anyhow::{Context};
use crate::network;
use crate::concurrency::{Ctx,Scope};

use near_primitives::hash::CryptoHash;

pub async fn run(ctx:Ctx, network: Arc<network::Network>, start_block_hash: CryptoHash) -> anyhow::Result<()> {
    info!("SYNC start");
    let peers = network.info(&ctx).await?;
    let target_height = peers.highest_height_peers[0].chain_info.height;
    info!("SYNC target_height = {}",target_height);
    Scope::run(&ctx,|ctx,s|async move {
        let mut last_hash = start_block_hash;
        let mut last_height = 0;
        while last_height<target_height {
            let mut headers = network.fetch_block_headers(&ctx,&last_hash).await?;
            headers.sort_by_key(|h|h.height());
            let last_header = headers.last().context("no headers")?;
            last_hash = last_header.hash().clone();
            last_height = last_header.height();
            info!("SYNC last_height = {}, {} headers left",last_height,target_height-last_height);
            
            for h in headers.drain(0..) {
                let network = network.clone();
                s.spawn(|ctx,s|async move {
                    info!("SYNC requesting block #{}",h.height());
                    let block = network.fetch_block(&ctx,h.hash()).await?;
                    info!("SYNC got block #{}, it has {} chunks",block.header().height(),block.chunks().len());
                    for ch in block.chunks().iter() {
                        let ch = ch.clone();
                        let network = network.clone();
                        s.spawn(|ctx,s|async move {
                            //info!("SYNC requesting chunk {} of block #{} ({})",chunk_header.shard_id(),block.header().height(),chunk_header.chunk_hash().0);
                            network.fetch_chunk(&ctx,&ch).await?;
                            anyhow::Ok(())
                            //info!("SYNC got chunk {}, it has {} parts",chunk.chunk_hash.0,chunk.parts.len());
                        });
                    }
                    anyhow::Ok(())
                });
            }
        }
        anyhow::Ok(())
    }).await
}
