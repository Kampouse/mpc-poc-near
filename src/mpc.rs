use anyhow::{Context, Result};
use near_api::types::transaction::{SignedTransaction, Transaction};
use near_api::types::Signature;
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

/// MPC signature response (from the sign call via yield-resume)
#[derive(Debug, Deserialize)]
pub struct SignResult {
    #[serde(default)]
    pub big_r: Option<String>,
    #[serde(default)]
    pub s: Option<String>,
    #[serde(default)]
    pub recovery_id: Option<u32>,
}

/// Request MPC to sign a 32-byte payload and return the signature.
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

    let exec_result = result.into_result()
        .map_err(|e| anyhow::anyhow!("MPC execution failed: {:?}", e))?;

    // Try to parse the return value
    match exec_result.json::<SignResult>() {
        Ok(sig) => {
            println!("   ✅ MPC signature received");
            Ok(sig)
        }
        Err(_) => {
            let bytes = exec_result.raw_bytes()
                .context("No return data from MPC sign")?;
            if bytes.len() == 64 {
                let sig_hex = hex::encode(&bytes);
                println!("   ✅ MPC signature received (raw 64 bytes)");
                Ok(SignResult {
                    big_r: None,
                    s: Some(sig_hex),
                    recovery_id: None,
                })
            } else {
                let json_str = String::from_utf8_lossy(&bytes);
                serde_json::from_str(&json_str)
                    .context(format!("Cannot parse MPC sign result: {}", json_str))
            }
        }
    }
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

/// Assemble a signed NEAR transaction from unsigned tx + ed25519 signature bytes and broadcast it.
pub async fn assemble_and_broadcast(
    unsigned_tx: &Transaction,
    signature_bytes: &[u8],
    network: &near_api::NetworkConfig,
) -> Result<String> {
    use borsh::BorshSerialize;

    // Construct NEAR Signature from ed25519 bytes
    let sig = Signature::from_parts(
        near_api::types::crypto::KeyType::ED25519,
        signature_bytes,
    ).context("Invalid ed25519 signature bytes")?;

    // Build the signed transaction
    let signed_tx = SignedTransaction::new(sig, unsigned_tx.clone());
    let tx_hash = signed_tx.get_hash();
    let tx_hash_hex = tx_hash.to_string();
    println!("   Signed TX hash: {}", tx_hash_hex);

    // Serialize signed tx to base64 for broadcast
    let tx_borsh = borsh::to_vec(&signed_tx)?;
    let tx_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_borsh);

    // Broadcast via JSON-RPC
    let rpc_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "0",
        "method": "broadcast_tx_commit",
        "params": [tx_b64]
    });

    // Get the RPC URL from the network config
    let rpc_url = network.rpc_endpoints.first()
        .map(|e| e.url.to_string())
        .unwrap_or_else(|| "https://rpc.testnet.near.org".to_string());

    println!("   Broadcasting to {}...", rpc_url);

    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(&rpc_url)
        .json(&rpc_body)
        .send()
        .await
        .context("RPC request failed")?
        .json()
        .await
        .context("RPC response parse failed")?;

    if let Some(error) = resp.get("error") {
        anyhow::bail!("RPC error: {:?}", error);
    }

    let result = resp.get("result")
        .context("No result in RPC response")?;

    let final_hash = result.get("transaction_outcome")
        .and_then(|r| r.get("id"))
        .and_then(|id| id.as_str())
        .unwrap_or(&tx_hash_hex)
        .to_string();

    println!("   ✅ TX finalized: {}", final_hash);
    println!("   Explorer: https://explorer.testnet.near.org/transactions/{}", final_hash);

    Ok(final_hash)
}

/// Full pipeline: sign + broadcast
pub async fn sign_and_broadcast(
    unsigned_tx: &Transaction,
    payload: &[u8; 32],
    path: &str,
    predecessor: &str,
    sponsor_id: &AccountId,
    sponsor_key: &str,
    network: &near_api::NetworkConfig,
) -> Result<String> {
    let sign_result = sign_payload(payload, path, predecessor, sponsor_id, sponsor_key, network).await?;

    let sig_hex = sign_result.s.context("MPC returned no signature")?;
    let sig_bytes = hex::decode(&sig_hex).context("Invalid signature hex")?;

    if sig_bytes.len() != 64 {
        anyhow::bail!("Expected 64-byte ed25519 signature, got {} bytes", sig_bytes.len());
    }

    assemble_and_broadcast(unsigned_tx, &sig_bytes, network).await
}
