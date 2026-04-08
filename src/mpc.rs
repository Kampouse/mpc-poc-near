use anyhow::{Context, Result};
use near_api::types::transaction::{SignedTransaction, Transaction};
use near_api::types::Signature;
use near_api::{AccountId, Contract, Signer};
use serde::{Deserialize, Serialize};

use crate::config::Config;

const MPC_CONTRACT: &str = "v1.signer-prod.testnet";

pub fn mpc_account() -> Result<AccountId> {
    MPC_CONTRACT.parse().context("MPC contract ID")
}

// ── Type-safe MPC request/response types ─────────────────────────────────────

/// Domain identifier for key derivation
#[derive(Clone, Copy, Debug, Serialize)]
#[repr(u8)]
pub enum DomainId {
    /// Ed25519 (NEAR) key derivation
    Ed25519 = 1,
    /// Secp256k1 (EVM/BTC) key derivation
    Secp256k1 = 2,
}

/// The payload to sign
#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum SignPayload {
    /// EdDSA (Ed25519) — hex-encoded 32-byte hash
    #[serde(rename = "Eddsa")]
    Eddsa(String),
    /// ECDSA (secp256k1) — EIP-191 or ERC-712
    #[serde(rename = "Ecsa")]
    Ecsa { message: String, signer_scheme: SignerScheme },
}

/// ECDSA signing scheme variants
#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum SignerScheme {
    #[serde(rename = "Eip191")]
    Eip191,
    #[serde(rename = "Erc712")]
    Erc712 { types: serde_json::Value, primary_type: String },
}

/// Top-level sign request
#[derive(Debug, Serialize)]
struct SignRequest {
    request: SignRequestInner,
}

#[derive(Debug, Serialize)]
struct SignRequestInner {
    payload_v2: SignPayload,
    path: String,
    domain_id: DomainId,
}

/// Derive key request
#[derive(Debug, Serialize)]
struct DeriveKeyRequest {
    path: String,
    predecessor: String,
    domain_id: DomainId,
}

/// MPC signature response
#[derive(Debug, Deserialize)]
pub struct SignResult {
    #[allow(dead_code)]
    pub scheme: String,
    pub signature: Vec<u8>,
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Derive the ed25519 public key from MPC for a given path.
pub async fn derive_public_key(
    path: &str,
    predecessor: &str,
    network: &near_api::NetworkConfig,
) -> Result<String> {
    let req = DeriveKeyRequest {
        path: path.to_string(),
        predecessor: predecessor.to_string(),
        domain_id: DomainId::Ed25519,
    };

    let derived: near_api::Data<serde_json::Value> = Contract(mpc_account()?)
        .call_function("derived_public_key", &req)
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

/// Convenience: derive key using Config (CLI use)
#[allow(dead_code)]
pub async fn derive_key_for_config(cfg: &Config) -> Result<String> {
    derive_public_key(&cfg.mpc_path, cfg.near_account.as_str(), &cfg.network).await
}

/// Request MPC to sign a 32-byte payload.
pub async fn sign_payload(
    payload: &[u8; 32],
    path: &str,
    _predecessor: &str,
    sponsor_id: &AccountId,
    sponsor_key: &str,
    network: &near_api::NetworkConfig,
) -> Result<SignResult> {
    let req = SignRequest {
        request: SignRequestInner {
            payload_v2: SignPayload::Eddsa(hex::encode(payload)),
            path: path.to_string(),
            domain_id: DomainId::Ed25519,
        },
    };

    let signer = Signer::from_secret_key(sponsor_key.parse()?)?;
    tracing::info!("MPC.sign(path={}, sponsor={})", path, sponsor_id);

    let result = Contract(mpc_account()?)
        .call_function("sign", &req)
        .transaction()
        .gas(near_api::NearGas::from_tgas(100))
        .deposit(near_api::NearToken::from_yoctonear(1))
        .with_signer(sponsor_id.clone(), signer)
        .send_to(network)
        .await
        .map_err(|e| anyhow::anyhow!("MPC.sign() failed: {:?}", e))?;

    let exec_result = result.into_result()
        .map_err(|e| anyhow::anyhow!("MPC execution failed: {:?}", e))?;

    match exec_result.json::<SignResult>() {
        Ok(sig) => {
            tracing::info!("MPC signature received ({} bytes)", sig.signature.len());
            Ok(sig)
        }
        Err(e) => {
            let bytes = exec_result.raw_bytes()
                .context("No return data from MPC sign")?;
            let sig: SignResult = serde_json::from_slice(&bytes)
                .context(format!("Cannot parse MPC sign result (json err: {:?})", e))?;
            tracing::info!("MPC signature received via fallback ({} bytes)", sig.signature.len());
            Ok(sig)
        }
    }
}

/// Sign via Config (CLI use)
#[allow(dead_code)]
pub async fn sign_payload_with_config(cfg: &Config, payload: &[u8; 32]) -> Result<SignResult> {
    let (sponsor_id, sponsor_key) = cfg.require_sponsor()?;
    sign_payload(payload, &cfg.mpc_path, cfg.near_account.as_str(), sponsor_id, sponsor_key, &cfg.network).await
}

// ── Shared internals ─────────────────────────────────────────────────────────

async fn sign_tx(
    unsigned_tx: &Transaction,
    payload: &[u8; 32],
    path: &str,
    predecessor: &str,
    sponsor_id: &AccountId,
    sponsor_key: &str,
    network: &near_api::NetworkConfig,
) -> Result<(SignedTransaction, String)> {
    let sign_result = sign_payload(payload, path, predecessor, sponsor_id, sponsor_key, network).await?;
    if sign_result.signature.len() != 64 {
        anyhow::bail!("Expected 64-byte ed25519 signature, got {} bytes", sign_result.signature.len());
    }

    let sig = Signature::from_parts(
        near_api::types::crypto::KeyType::ED25519,
        &sign_result.signature,
    ).context("Invalid ed25519 signature bytes")?;

    let signed_tx = SignedTransaction::new(sig, unsigned_tx.clone());
    let tx_hash_hex = signed_tx.get_hash().to_string();
    tracing::info!("Signed TX hash: {}", tx_hash_hex);
    Ok((signed_tx, tx_hash_hex))
}

fn encode_tx(signed_tx: &SignedTransaction) -> Result<String> {
    let tx_borsh = borsh::to_vec(signed_tx)?;
    Ok(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &tx_borsh))
}

fn rpc_url(network: &near_api::NetworkConfig) -> String {
    network.rpc_endpoints.first()
        .map(|e| e.url.to_string())
        .unwrap_or_else(|| "https://rpc.testnet.near.org".to_string())
}

async fn rpc_post(client: &reqwest::Client, url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    client.post(url).json(body).send().await
        .context("RPC request failed")?
        .json().await
        .context("RPC response parse failed")
}

// ── Broadcast pipelines ──────────────────────────────────────────────────────

/// MPC sign → broadcast (waits for finality). Used by CLI.
pub async fn sign_and_broadcast(
    unsigned_tx: &Transaction,
    payload: &[u8; 32],
    path: &str,
    predecessor: &str,
    sponsor_id: &AccountId,
    sponsor_key: &str,
    network: &near_api::NetworkConfig,
) -> Result<String> {
    let (signed_tx, tx_hash_hex) = sign_tx(unsigned_tx, payload, path, predecessor, sponsor_id, sponsor_key, network).await?;
    let tx_b64 = encode_tx(&signed_tx)?;

    let rpc = rpc_url(network);
    let client = reqwest::Client::new();
    let resp = rpc_post(&client, &rpc, &serde_json::json!({
        "jsonrpc": "2.0", "id": "0",
        "method": "broadcast_tx_commit",
        "params": [tx_b64]
    })).await?;

    if let Some(error) = resp.get("error") { anyhow::bail!("RPC error: {:?}", error); }

    let final_hash = resp.get("result")
        .and_then(|r| r.get("transaction_outcome"))
        .and_then(|r| r.get("id"))
        .and_then(|id| id.as_str())
        .unwrap_or(&tx_hash_hex)
        .to_string();

    tracing::info!("TX finalized: {} — https://explorer.testnet.near.org/transactions/{}", final_hash, final_hash);
    Ok(final_hash)
}

/// MPC sign → async submit → poll for finality. Used by worker.
pub async fn sign_and_broadcast_async(
    unsigned_tx: &Transaction,
    payload: &[u8; 32],
    path: &str,
    predecessor: &str,
    sponsor_id: &AccountId,
    sponsor_key: &str,
    network: &near_api::NetworkConfig,
) -> Result<String> {
    let (signed_tx, tx_hash_hex) = sign_tx(unsigned_tx, payload, path, predecessor, sponsor_id, sponsor_key, network).await?;
    let tx_b64 = encode_tx(&signed_tx)?;

    let rpc = rpc_url(network);
    let client = reqwest::Client::new();

    tracing::info!("Submitting tx async to {}", rpc);
    let resp = rpc_post(&client, &rpc, &serde_json::json!({
        "jsonrpc": "2.0", "id": "0",
        "method": "broadcast_tx_async",
        "params": [tx_b64]
    })).await?;
    if let Some(error) = resp.get("error") { anyhow::bail!("RPC error: {:?}", error); }

    tracing::info!("Polling for finality: {}", tx_hash_hex);
    for attempt in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let check_resp = rpc_post(&client, &rpc, &serde_json::json!({
            "jsonrpc": "2.0", "id": "0",
            "method": "tx",
            "params": [tx_hash_hex, "unused"]
        })).await.unwrap_or(serde_json::json!({}));

        if let Some(result) = check_resp.get("result") {
            let status = result.get("status").or_else(|| result.get("final_execution_status"));
            if let Some(s) = status {
                let s_str = serde_json::to_string(s).unwrap_or_default();
                if s_str.contains("Final") || s_str.contains("SuccessValue") {
                    tracing::info!("TX finalized: {} ({}s) — https://explorer.testnet.near.org/transactions/{}",
                        tx_hash_hex, (attempt + 1) * 2, tx_hash_hex);
                    return Ok(tx_hash_hex);
                }
                if s_str.contains("Failure") || s_str.contains("Error") {
                    anyhow::bail!("TX failed: {:?}", s);
                }
            }
        }
    }

    tracing::warn!("TX finality timeout (60s), hash: {}", tx_hash_hex);
    Ok(format!("{} (pending)", tx_hash_hex))
}
