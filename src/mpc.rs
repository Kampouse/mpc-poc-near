use anyhow::{Context, Result};
use near_api::{AccountId, Contract, Signer};
use crate::config::Config;

const MPC_CONTRACT: &str = "v1.signer-prod.testnet";

fn mpc_account() -> Result<AccountId> {
    MPC_CONTRACT.parse().context("MPC contract ID")
}

/// Derive the ed25519 public key from MPC for a given path.
pub async fn derive_public_key(cfg: &Config) -> Result<String> {
    let derived: near_api::Data<serde_json::Value> = Contract(mpc_account()?)
        .call_function("derived_public_key", serde_json::json!({
            "path": &cfg.mpc_path,
            "predecessor": cfg.near_account.as_str(),
            "domain_id": 1,
        }))
        .read_only()
        .fetch_from(&cfg.network)
        .await
        .context("MPC derived_public_key call failed")?;

    let raw = derived.data.as_str()
        .context("MPC returned non-string")?;

    if raw.starts_with("ed25519:") {
        Ok(raw.to_string())
    } else {
        let bytes = hex::decode(raw).context("MPC key not valid hex")?;
        Ok(format!("ed25519:{}", bs58::encode(&bytes).into_string()))
    }
}

/// Request MPC to sign a payload via a sponsor account.
pub async fn sign_payload(cfg: &Config, payload: &[u8; 32]) -> Result<()> {
    let (sponsor_id, sponsor_key) = cfg.require_sponsor()?;
    let signer = Signer::from_secret_key(sponsor_key.parse()?)?;

    println!("④ Calling MPC.sign() as {}...", sponsor_id);

    let payload_vec: Vec<u8> = payload.to_vec();
    let result = Contract(mpc_account()?)
        .call_function("sign", serde_json::json!({
            "request": {
                "payload_v2": { "Eddsa": payload_vec },
                "path": &cfg.mpc_path,
                "domain_id": 1,
            }
        }))
        .transaction()
        .gas(near_api::NearGas::from_tgas(100))
        .deposit(near_api::NearToken::from_yoctonear(1))
        .with_signer(sponsor_id.clone(), signer)
        .send_to(&cfg.network)
        .await;

    match result {
        Ok(_) => {
            println!("   ✅ MPC signing submitted");
            println!("\n   Track: https://explorer.testnet.near.org/accounts/{}", cfg.near_account);
        }
        Err(e) => {
            let err = format!("{:?}", e);
            anyhow::bail!("MPC.sign() failed: {}", &err[..err.len().min(300)]);
        }
    }
    Ok(())
}
