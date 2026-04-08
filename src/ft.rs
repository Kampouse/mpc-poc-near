use anyhow::{Context, Result};
use near_api::{AccountId, Contract};
use crate::config::Config;

pub struct FtMetadata {
    pub symbol: String,
    pub decimals: u8,
    pub name: String,
}

/// Fetch NEP-141 metadata from any token contract.
pub async fn get_metadata(
    network: &near_api::NetworkConfig,
    contract_id: &AccountId,
) -> Result<FtMetadata> {
    let result: near_api::Data<serde_json::Value> = Contract(contract_id.clone())
        .call_function("ft_metadata", serde_json::json!({}))
        .read_only()
        .fetch_from(network)
        .await
        .with_context(|| format!("ft_metadata call failed for {}", contract_id))?;

    let data = &result.data;
    Ok(FtMetadata {
        symbol: data.get("symbol").and_then(|v| v.as_str()).unwrap_or("???").to_string(),
        decimals: data.get("decimals").and_then(|v| v.as_u64()).unwrap_or(24) as u8,
        name: data.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string(),
    })
}

/// Query FT balance from any NEP-141 contract. Returns raw amount.
pub async fn get_balance(
    network: &near_api::NetworkConfig,
    account_id: &AccountId,
    contract_id: &AccountId,
) -> Result<u128> {
    let result: near_api::Data<serde_json::Value> = Contract(contract_id.clone())
        .call_function("ft_balance_of", serde_json::json!({
            "account_id": account_id.as_str(),
        }))
        .read_only()
        .fetch_from(network)
        .await
        .with_context(|| format!("ft_balance_of failed for {}", contract_id))?;

    result.data.as_str()
        .and_then(|s| s.parse::<u128>().ok())
        .with_context(|| format!("Invalid balance response from {}", contract_id))
}

/// Display balance for a specific token.
pub async fn show_balance(cfg: &Config, contract_id: &str) -> Result<()> {
    let contract: AccountId = contract_id.parse()
        .with_context(|| format!("Invalid contract ID: {}", contract_id))?;

    let meta = get_metadata(&cfg.network, &contract).await?;
    let balance = get_balance(&cfg.network, &cfg.near_account, &contract).await?;
    let human = balance as f64 / 10u128.pow(meta.decimals as u32) as f64;

    println!("{} ({})", meta.symbol, meta.name);
    println!("Contract: {}", contract_id);
    println!("Decimals: {}", meta.decimals);
    println!("Raw:      {}", balance);
    println!("Balance:  {:.6} {}", human, meta.symbol);
    Ok(())
}

/// Common tokens to check in `balances` command (just hints, not hardcoded limits).
const COMMON_TOKENS: &[&str] = &[
    "usdt.fakes.testnet",
    "usdc.fakes.testnet",
    "wrap.testnet",
    "token.v2.ref-finance.testnet",
];

/// Show balances for common tokens (non-zero only).
pub async fn show_common_balances(cfg: &Config) -> Result<()> {
    for contract_id in COMMON_TOKENS {
        let contract: AccountId = contract_id.parse()
            .with_context(|| format!("Invalid FT contract ID: {}", contract_id))?;
        // Fetch metadata first, then balance — avoids double call
        let meta = match get_metadata(&cfg.network, &contract).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if let Ok(balance) = get_balance(&cfg.network, &cfg.near_account, &contract).await {
            if balance > 0 {
                let human = balance as f64 / 10u128.pow(meta.decimals as u32) as f64;
                println!("║   {}: {:.6}", format!("{:8}", meta.symbol), human);
            }
        }
    }
    Ok(())
}
