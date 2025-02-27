use solana_client::{
    pubsub_client::PubsubClient,
    rpc_client::RpcClient,
    rpc_config::{RpcTransactionConfig, RpcTransactionLogsFilter, RpcTransactionLogsConfig},
    rpc_response::Response as RpcResponse,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::Signature,
};
use solana_transaction_status::UiTransactionEncoding;
use spl_token::state::Mint;
use solana_program::program_pack::Pack;
use anyhow::{Result, anyhow};
use std::str::FromStr;
use tokio::sync::mpsc;
use log::{info, error, warn};
use borsh::{BorshDeserialize, BorshSerialize};
use std::time::Duration;

const RAYDIUM_V4_PROGRAM_ID: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const RPC_URL: &str = "https://mainnet.helius-rpc.com/?api-key=177e861e-680b-4c8f-9e7c-a41c87c43968";
const WS_URL: &str = "wss://mainnet.helius-rpc.com/?api-key=177e861e-680b-4c8f-9e7c-a41c87c43968";
const TOKEN_METADATA_PROGRAM_ID: &str = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(BorshDeserialize, BorshSerialize, Debug)]
struct Initialize2Data {
    discriminator: u8,
    nonce: u8,
    open_time: u64,
    init_pc_amount: u64,
    init_coin_amount: u64,
}

struct TokenInfo {
    name: String,
    decimals: u8,
}

async fn fetch_token_info(rpc_client: &RpcClient, token_pubkey: &Pubkey) -> Result<TokenInfo> {
    // 获取代币信息
    let mint_account = rpc_client.get_account(token_pubkey)?;
    let mint = Mint::unpack_from_slice(&mint_account.data)?;
    
    // 获取元数据 PDA
    let metadata_program_id = Pubkey::from_str(TOKEN_METADATA_PROGRAM_ID)?;
    let seeds = &[
        b"metadata",
        metadata_program_id.as_ref(),
        token_pubkey.as_ref(),
    ];
    let (metadata_address, _) = Pubkey::find_program_address(seeds, &metadata_program_id);

    // 获取元数据
    match rpc_client.get_account(&metadata_address) {
        Ok(metadata_account) => {
            info!("Metadata account data length: {}", metadata_account.data.len());
            
            // 跳过前缀数据，直接解析名称
            if metadata_account.data.len() < 65 {
                warn!("Metadata account data too short");
                return Ok(TokenInfo {
                    name: format!("Unknown Token {}", token_pubkey),
                    decimals: mint.decimals,
                });
            }

            let name_start = 65; // 跳过前缀数据
            let name_length = metadata_account.data[name_start] as usize;
            
            if metadata_account.data.len() < name_start + 1 + name_length {
                warn!("Metadata account data too short for name");
                return Ok(TokenInfo {
                    name: format!("Unknown Token {}", token_pubkey),
                    decimals: mint.decimals,
                });
            }

            let name_data = &metadata_account.data[name_start + 1..name_start + 1 + name_length];
            
            match String::from_utf8(name_data.to_vec()) {
                Ok(name) => {
                    info!("Successfully parsed token name: {}", name);
                    Ok(TokenInfo {
                        name: name.trim_matches(char::from(0)).to_string(),
                        decimals: mint.decimals,
                    })
                }
                Err(e) => {
                    warn!("Failed to parse name data: {}", e);
                    warn!("Name data bytes: {:?}", name_data);
                    Ok(TokenInfo {
                        name: format!("Unknown Token {}", token_pubkey),
                        decimals: mint.decimals,
                    })
                }
            }
        }
        Err(e) => {
            warn!("Failed to get metadata account: {}", e);
            Ok(TokenInfo {
                name: format!("Unknown Token {}", token_pubkey),
                decimals: mint.decimals,
            })
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // 设置日志级别为 INFO
    std::env::set_var("RUST_LOG", "info");
    env_logger::init();
    
    info!("Starting Raydium V4 liquidity pool monitor...");
    info!("Connecting to RPC endpoint: {}", RPC_URL);
    info!("Connecting to WebSocket endpoint: {}", WS_URL);

    let rpc_client = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());
    let _raydium_pubkey = Pubkey::from_str(RAYDIUM_V4_PROGRAM_ID)?;

    // 创建一个 mpsc 通道来接收日志
    let (tx, mut rx) = mpsc::channel::<RpcResponse<solana_client::rpc_response::RpcLogsResponse>>(100);

    // 启动 WebSocket 订阅的任务
    tokio::spawn(async move {
        info!("Starting WebSocket subscription...");
        match PubsubClient::logs_subscribe(
            WS_URL,
            RpcTransactionLogsFilter::Mentions(vec![RAYDIUM_V4_PROGRAM_ID.to_string()]),
            RpcTransactionLogsConfig {
                commitment: Some(CommitmentConfig::confirmed()),
            },
        ) {
            Ok((_, receiver)) => {
                info!("Successfully subscribed to program logs");
                // 从订阅中接收日志并发送到通道
                while let Ok(log) = receiver.recv() {
                    if tx.send(log).await.is_err() {
                        error!("Failed to send log through channel, exiting...");
                        break;
                    }
                }
            }
            Err(e) => {
                error!("Failed to subscribe to program logs: {}", e);
            }
        }
        warn!("WebSocket subscription task ended");
    });

    info!("Monitoring logs for program: {}", RAYDIUM_V4_PROGRAM_ID);
    info!("Waiting for transactions...");

    // 主循环从通道接收日志
    while let Some(log) = rx.recv().await {
        if log.value.logs.iter().any(|l| l.contains("initialize2")) {
            info!("Found initialize2 instruction in transaction: {}", log.value.signature);
            match Signature::from_str(&log.value.signature) {
                Ok(signature) => {
                    // 等待交易完成，减少等待时间
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if let Err(e) = process_transaction(&rpc_client, signature).await {
                        error!("Failed to process transaction {}: {}", signature, e);
                    }
                }
                Err(e) => {
                    error!("Failed to parse signature {}: {}", log.value.signature, e);
                }
            }
        }
    }

    warn!("Main loop ended unexpectedly");
    Ok(())
}

async fn process_transaction(rpc_client: &RpcClient, signature: Signature) -> Result<()> {
    let tx_config = RpcTransactionConfig {
        max_supported_transaction_version: Some(0),
        encoding: Some(UiTransactionEncoding::Base64),
        commitment: Some(CommitmentConfig::confirmed()),  // 使用 confirmed 而不是 finalized
    };

    // 使用重试机制获取交易
    let mut retries = 0;
    let tx = loop {
        match rpc_client.get_transaction_with_config(&signature, tx_config.clone()) {
            Ok(tx) => break tx,
            Err(e) => {
                if retries >= MAX_RETRIES {
                    return Err(anyhow!("Failed to get transaction after {} retries: {}", MAX_RETRIES, e));
                }
                warn!("Failed to get transaction, retrying ({}/{}): {}", retries + 1, MAX_RETRIES, e);
                tokio::time::sleep(RETRY_DELAY).await;
                retries += 1;
                continue;
            }
        }
    };

    // 解析交易数据
    let transaction = tx.transaction.transaction.decode().ok_or_else(|| anyhow!("Failed to decode transaction"))?;
    let message = transaction.message;

    // 获取账户和指令
    let static_keys = message.static_account_keys();
    let instructions = message.instructions();

    // 查找 Raydium 指令
    let raydium_ix = instructions.iter()
        .find(|ix| {
            static_keys[ix.program_id_index as usize] == Pubkey::from_str(RAYDIUM_V4_PROGRAM_ID).unwrap()
        });

    if let Some(ix) = raydium_ix {
        // 直接使用指令数据的原始字节
        let data = Initialize2Data::try_from_slice(&ix.data)?;
        
        // 获取相关账户
        let lp_account = &static_keys[4];
        let token_a_account = &static_keys[8];
        let token_b_account = &static_keys[9];

        // 获取代币信息
        let token_a_info = match fetch_token_info(rpc_client, token_a_account).await {
            Ok(info) => info,
            Err(e) => {
                warn!("Failed to fetch token A info: {}", e);
                TokenInfo {
                    name: format!("Unknown Token {}", token_a_account),
                    decimals: 9, // 默认使用 9 位小数
                }
            }
        };

        let token_b_info = match fetch_token_info(rpc_client, token_b_account).await {
            Ok(info) => info,
            Err(e) => {
                warn!("Failed to fetch token B info: {}", e);
                TokenInfo {
                    name: format!("Unknown Token {}", token_b_account),
                    decimals: 9, // 默认使用 9 位小数
                }
            }
        };

        // 输出信息
        info!("Found new liquidity pool!");
        info!("----------------------------");
        info!("Transaction: https://solscan.io/tx/{}", signature);
        info!("New LP Account: {}", lp_account);
        info!("Token A: {} ({})", token_a_info.name, token_a_account);
        info!("Token A Amount: {}", data.init_coin_amount as f64 / 10f64.powi(token_a_info.decimals as i32));
        info!("Token B: {} ({})", token_b_info.name, token_b_account);
        info!("Token B Amount: {}", data.init_pc_amount as f64 / 10f64.powi(token_b_info.decimals as i32));
        info!("Open Time: {}", data.open_time);

        // 计算延迟
        if let Some(block_time) = tx.block_time {
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let delay = current_time.saturating_sub(block_time as u64);
            info!("Transaction delay: {} seconds", delay);
        }
        info!("----------------------------");
    }

    Ok(())
}