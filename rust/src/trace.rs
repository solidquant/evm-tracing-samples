use anyhow::{anyhow, Result};
use cfmms::{
    checkpoint::sync_pools_from_checkpoint,
    dex::{Dex, DexVariant},
    pool::Pool,
    sync::sync_pairs,
};
use dashmap::DashMap;
use ethers::{
    abi,
    providers::{Provider, Ws},
    types::{Address, BlockNumber, Diff, TraceType, Transaction, H160, H256, U256, U64},
    utils::keccak256,
};
use ethers_providers::Middleware;
use log::info;
use std::{path::Path, str::FromStr, sync::Arc};
use tokio::sync::broadcast::{self, Sender};
use tokio::task::JoinSet;
use tokio_stream::StreamExt;

use crate::utils::calculate_next_block_base_fee;

#[derive(Default, Debug, Clone)]
pub struct NewBlock {
    pub number: U64,
    pub gas_used: U256,
    pub gas_limit: U256,
    pub base_fee_per_gas: U256,
    pub timestamp: U256,
}

#[derive(Debug, Clone)]
pub enum Event {
    NewBlock(NewBlock),
    Transaction(Transaction),
}

async fn trace_state_diff(
    provider: Arc<Provider<Ws>>,
    tx: &Transaction,
    block_number: U64,
    pools: &DashMap<H160, Pool>,
    target_address: String,
) -> Result<()> {
    info!(
        "Tx #{} received. Checking if it touches: {}",
        tx.hash, target_address
    );

    let target_address: Address = target_address.parse().unwrap();

    let state_diff = provider
        .trace_call(
            tx,
            vec![TraceType::StateDiff],
            Some(BlockNumber::from(block_number)),
        )
        .await?
        .state_diff
        .ok_or(anyhow!("state diff does not exist"))?
        .0;

    let touched_pools: Vec<Pool> = state_diff
        .keys()
        .filter_map(|addr| pools.get(addr).map(|p| (*p.value()).clone()))
        .filter(|p| match p {
            Pool::UniswapV2(pool) => vec![pool.token_a, pool.token_b].contains(&target_address),
            Pool::UniswapV3(pool) => vec![pool.token_a, pool.token_b].contains(&target_address),
        })
        .collect();

    if touched_pools.is_empty() {
        return Ok(());
    }

    let target_storage = &state_diff
        .get(&target_address)
        .ok_or(anyhow!("no target storage"))?
        .storage;

    for pool in &touched_pools {
        let slot = H256::from(keccak256(abi::encode(&[
            abi::Token::Address(pool.address()),
            abi::Token::Uint(U256::from(3)),
        ])));

        if let Some(Diff::Changed(c)) = target_storage.get(&slot) {
            let from = U256::from(c.from.to_fixed_bytes());
            let to = U256::from(c.to.to_fixed_bytes());

            if to > from {
                // if to > from, the balance of pool's <target_token> has increased
                // thus, the transaction was a call to swap: <target_token> -> token
                info!(
                    "(Tx #{}) Balance change: {} -> {} @ Pool {}",
                    tx.hash,
                    from,
                    to,
                    pool.address()
                );
            }
        }
    }

    Ok(())
}

pub async fn mempool_watching(target_address: String) -> Result<()> {
    let wss_url: String = std::env::var("WSS_URL").unwrap();
    let provider = Provider::<Ws>::connect(wss_url).await?;
    let provider = Arc::new(provider);

    // Step #1: Using cfmms-rs to sync all pools created on Uniswap V3
    let checkpoint_path = ".cfmms-checkpoint.json";
    let checkpoint_exists = Path::new(checkpoint_path).exists();

    let pools = DashMap::new();

    let dexes_data = [(
        // Uniswap v3
        "0x1F98431c8aD98523631AE4a59f267346ea31F984",
        DexVariant::UniswapV3,
        12369621u64,
    )];
    let dexes: Vec<_> = dexes_data
        .into_iter()
        .map(|(address, variant, number)| {
            Dex::new(H160::from_str(address).unwrap(), variant, number, Some(300))
        })
        .collect();

    let pools_vec = if checkpoint_exists {
        let (_, pools_vec) =
            sync_pools_from_checkpoint(checkpoint_path, 100000, provider.clone()).await?;
        pools_vec
    } else {
        sync_pairs(dexes.clone(), provider.clone(), Some(checkpoint_path)).await?
    };

    for pool in pools_vec {
        pools.insert(pool.address(), pool);
    }

    info!("Uniswap V3 pools synced: {}", pools.len());

    // Step #2: Stream data asynchronously
    let (event_sender, _): (Sender<Event>, _) = broadcast::channel(512);

    let mut set = JoinSet::new();

    // Stream new headers
    {
        let provider = provider.clone();
        let event_sender = event_sender.clone();

        set.spawn(async move {
            let stream = provider.subscribe_blocks().await.unwrap();
            let mut stream = stream.filter_map(|block| match block.number {
                Some(number) => Some(NewBlock {
                    number,
                    gas_used: block.gas_used,
                    gas_limit: block.gas_limit,
                    base_fee_per_gas: block.base_fee_per_gas.unwrap_or_default(),
                    timestamp: block.timestamp,
                }),
                None => None,
            });

            while let Some(block) = stream.next().await {
                match event_sender.send(Event::NewBlock(block)) {
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        });
    }

    // Stream pending transactions
    {
        let provider = provider.clone();
        let event_sender = event_sender.clone();

        set.spawn(async move {
            let stream = provider.subscribe_pending_txs().await.unwrap();
            let mut stream = stream.transactions_unordered(256).fuse();

            while let Some(result) = stream.next().await {
                match result {
                    Ok(tx) => match event_sender.send(Event::Transaction(tx)) {
                        Ok(_) => {}
                        Err(_) => {}
                    },
                    Err(_) => {}
                };
            }
        });
    }

    // Event handler
    {
        let mut event_receiver = event_sender.subscribe();

        set.spawn(async move {
            let mut new_block = NewBlock::default();

            loop {
                match event_receiver.recv().await {
                    Ok(event) => match event {
                        Event::NewBlock(block) => {
                            new_block = block;
                            info!("{:?}", new_block);
                        }
                        Event::Transaction(tx) => {
                            if new_block.number != U64::zero() {
                                let next_base_fee = calculate_next_block_base_fee(
                                    new_block.gas_used,
                                    new_block.gas_limit,
                                    new_block.base_fee_per_gas,
                                );

                                // max_fee_per_gas has to be greater than next block's base fee
                                if tx.max_fee_per_gas.unwrap_or_default()
                                    > U256::from(next_base_fee)
                                {
                                    match trace_state_diff(
                                        provider.clone(),
                                        &tx,
                                        new_block.number,
                                        &pools,
                                        target_address.clone(),
                                    )
                                    .await
                                    {
                                        Ok(_) => {}
                                        Err(_) => {}
                                    }
                                }
                            }
                        }
                    },
                    Err(_) => {}
                }
            }
        });
    }

    while let Some(res) = set.join_next().await {
        info!("{:?}", res);
    }

    Ok(())
}
