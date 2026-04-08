use anyhow::{Context, Result};
use near_api::{AccountId, Contract, Signer};
use serde::Deserialize;

use crate::config::Config;

const MPC_CONTRACT: &str = "v1.signer-prod.testnet";

pub fn mpc_account() -> Result<AccountId> {
    MPC_CONTRACT.parse().context("MPC contract ID")
}

/// Derive the ed25519 public key from MPC for a given path.
pub async fn derive_public_key(
    path: &str,
    predecessor: &str,
    network: &near_api::NetworkConfig,
) -> Result<String> {
    let derived: near_api::Data<serde_json::Value> = Contract(mpc_account()?)
        .call_function("derived_public_key", serde_json::json!({
            "path": path,
            "predecessor": predecessor,
            "domain_id": 1,
        }))
        .read_only()
        .fetch_from(network)
        .await
        .context("MPC derived_public_key call failed")?;

    let raw = derived.data.as_str().context("MPC returned non-string")?;

    if raw.starts_with("ed25519:") {
        Ok(raw.to_string())
    } else {
        let bytes = hex::decode(raw).context("MPC key not valid hex")?;
        Ok(format!("ed25519:{}", bs58::encode(&bytes).into_string()))
    }
}

/// Convenience: derive key using Config
pub async fn derive_key_for_config(cfg: &Config) -> Result<String> {
    derive_public_key(&cfg.mpc_path, cfg.near_account.as_str(), &cfg.network).await
}

/// MPC signature result from the sign call
#[derive(Debug, Deserialize)]
pub struct SignResult {
    pub big_r: String,       // hex encoded affine point
    pub s: String,           // hex encoded scalar
    pub recovery_id: u32,
}

/// Request MPC to sign a 32-byte payload.
/// Returns the signature components from the MPC network.
pub async fn sign_payload(
    payload: &[u8; 32],
    path: &str,
    _predecessor: &str,
    sponsor_id: &AccountId,
    sponsor_key: &str,
    network: &near_api::NetworkConfig,
) -> Result<SignResult> {
    let signer = Signer::from_secret_key(sponsor_key.parse()?)?;
    let payload_vec: Vec<u8> = payload.to_vec();

    println!("   Calling MPC.sign(path={}, sponsor={})...", path, sponsor_id);

    let result = Contract(mpc_account()?)
        .call_function("sign", serde_json::json!({
            "request": {
                "payload_v2": { "Eddsa": payload_vec },
                "path": path,
                "domain_id": 1,
            }
        }))
        .transaction()
        .gas(near_api::NearGas::from_tgas(100))
        .deposit(near_api::NearToken::from_yoctonear(1))
        .with_signer(sponsor_id.clone(), signer)
        .send_to(network)
        .await
        .map_err(|e| anyhow::anyhow!("MPC.sign() failed: {:?}", e))?;

    // Extract the signature from the execution outcome
    let exec_result = result.into_result()
        .map_err(|e| anyhow::anyhow!("MPC execution failed: {:?}", e))?;

    // The sign function returns JSON: {"big_r": "...", "s": "...", "recovery_id": N}
    let sign_result: SignResult = exec_result.json()
        .map_err(|e| anyhow::anyhow!("Failed to parse MPC sign result: {:?}. Trying raw bytes...", e))
        .or_else(|_| {
            // Try parsing from raw bytes as fallback
            let bytes = exec_result.raw_bytes()
                .context("No return data from MPC sign")?;
            serde_json::from_slice(&bytes)
                .context("Failed to parse MPC sign result as JSON")
        })?;

    println!("   ✅ MPC signature received (recovery_id={})", sign_result.recovery_id);
    Ok(sign_result)
}

/// Legacy: sign via Config (CLI use)
pub async fn sign_payload_with_config(cfg: &Config, payload: &[u8; 32]) -> Result<SignResult> {
    let (sponsor_id, sponsor_key) = cfg.require_sponsor()?;
    sign_payload(
        payload,
        &cfg.mpc_path,
        cfg.near_account.as_str(),
        sponsor_id,
        sponsor_key,
        &cfg.network,
    ).await
}
