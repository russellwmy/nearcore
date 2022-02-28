use std::sync::{Arc};

use log::{error, info, warn};

use tokio::time;
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
        s.spawn({
            let network = network.clone();
            |ctx,s| async move{
                loop {
                    info!("stats = {:?}",network.stats);
                    ctx.wait(time::Duration::from_secs(10)).await?;
                }
            }
        });

        let mut last_hash = start_block_hash;
        let mut last_height = 0;
        while last_height<target_height {
            let mut headers = network.fetch_block_headers(&ctx,&last_hash).await?;
            headers.sort_by_key(|h|h.height());
            let last_header = headers.last().context("no headers")?;
            last_hash = last_header.hash().clone();
            last_height = last_header.height();
            info!("SYNC last_height = {}, {} headers left",last_height,target_height-last_height);
            
            for h in headers {
                s.spawn({
                    let network = network.clone();
                    |ctx,s|async move {
                        let block = network.fetch_block(&ctx,h.hash()).await?;
                        for ch in block.chunks().iter() {
                            let ch = ch.clone();
                            let network = network.clone();
                            s.spawn(|ctx,s|async move {
                                network.fetch_chunk(&ctx,&ch).await?;
                                anyhow::Ok(())
                            });
                        }
                        anyhow::Ok(())
                    }
                });
            }
        }
        anyhow::Ok(())
    }).await
}
