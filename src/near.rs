use anyhow::{Context, Result};
use near_api::types::transaction::actions::{FunctionCallAction, TransferAction};
use near_api::types::transaction::{Transaction, TransactionV0};
use near_api::types::{Action, CryptoHash, PublicKey};
use near_api::{Account, AccountId, NearGas, NearToken};
use sha2::{Digest, Sha256};
use std::str::FromStr;

use crate::config::Config;
use crate::ft;
use crate::mpc;

// ── Create Account ────────────────────────────────────────────────────────────

pub async fn create_account(cfg: &Config) -> Result<()> {
    let (funder_id, funder_key) = cfg.require_funder()?;
    let near_public_key = derive_key(cfg).await?;
    let signer = near_api::Signer::from_secret_key(funder_key.parse()?)?;

    println!("Creating {} with MPC-derived key\n", cfg.near_account);

    let result = Account::create_account(cfg.near_account.clone())
        .fund_myself(funder_id.clone(), NearToken::from_near(0))
        .with_public_key(PublicKey::from_str(&near_public_key)?)
        .with_signer(signer)
        .send_to(&cfg.network)
        .await;

    match result {
        Ok(tx) => {
            tx.assert_success();
            println!("✅ Created: {}", cfg.near_account);
        }
        Err(e) => {
            let err = format!("{:?}", e);
            if err.contains("AlreadyExists") {
                println!("Already exists");
            } else {
                anyhow::bail!("{}", &err[..err.len().min(300)]);
            }
        }
    }
    Ok(())
}

// ── Info ──────────────────────────────────────────────────────────────────────

pub async fn show_info(cfg: &Config) -> Result<()> {
    let near_public_key = derive_key(cfg).await?;
    let chain = match Account(cfg.near_account.clone()).view().fetch_from(&cfg.network).await {
        Ok(s) => format!("✅ ({} NEAR)", s.data.amount.as_yoctonear() as f64 / 1e24),
        Err(_) => "❌ not found".into(),
    };

    println!("╔══════════════════════════════════════════════════╗");
    println!("║   NEAR account: {}", cfg.near_account);
    println!("║   Nostr npub:   {}...{}", &cfg.npub[..16], &cfg.npub[56..]);
    println!("║   MPC path:     {}", cfg.mpc_path);
    println!("║   MPC key:      {}...", &near_public_key[..40]);
    println!("║   On-chain:     {}", chain);
    println!("║                                                  ║");
    println!("║   Recovery: this tool + Nostr key. No worker. ✅ ║");
    println!("╚══════════════════════════════════════════════════╝");
    Ok(())
}

// ── Balances ─────────────────────────────────────────────────────────────────

pub async fn show_balances(cfg: &Config) -> Result<()> {
    println!("╔══════════════════════════════════════════════════╗");
    println!("║   Balances for {}", cfg.near_account);
    println!("╠══════════════════════════════════════════════════╣");

    match Account(cfg.near_account.clone()).view().fetch_from(&cfg.network).await {
        Ok(s) => {
            let bal = s.data.amount.as_yoctonear() as f64 / 1e24;
            println!("║   NEAR:   {:.6}", bal);
        }
        Err(_) => println!("║   NEAR:   ⚠️  account not found"),
    }

    ft::show_common_balances(cfg).await?;

    println!("║");
    println!("║   Check any token: balance <contract_id>");
    println!("╚══════════════════════════════════════════════════╝");
    Ok(())
}

// ── Transfer ─────────────────────────────────────────────────────────────────

/// Derive key for this config
pub async fn derive_key(cfg: &Config) -> Result<String> {
    mpc::derive_public_key(&cfg.mpc_path, cfg.near_account.as_str(), &cfg.network).await
}

pub async fn transfer(cfg: &Config, to: &str, amount_str: &str, token: Option<&str>) -> Result<()> {
    let to_id: AccountId = to.parse().with_context(|| format!("Invalid recipient: {}", to))?;
    let amount: f64 = amount_str.parse().with_context(|| format!("Invalid amount: {}", amount_str))?;
    let near_public_key = derive_key(cfg).await?;

    match token {
        None => build_and_sign_near(cfg, &near_public_key, &to_id, amount).await,
        Some(contract_id) => build_and_sign_ft(cfg, &near_public_key, &to_id, amount, contract_id).await,
    }
}

async fn build_and_sign_near(
    cfg: &Config,
    near_public_key: &str,
    to_id: &AccountId,
    amount_near: f64,
) -> Result<()> {
    let amount_yocto = (amount_near * 1e24) as u128;
    println!("Transfer {} NEAR → {} via MPC\n", amount_near, to_id);

    let pk = parse_pk(near_public_key)?;
    let (nonce, block_hash) = get_nonce_blockhash(cfg, &pk).await?;
    println!("① Nonce: {}", nonce);

    let unsigned_tx = Transaction::V0(TransactionV0 {
        signer_id: cfg.near_account.clone(),
        public_key: pk,
        nonce: nonce + 1,
        receiver_id: to_id.clone(),
        block_hash,
        actions: vec![Action::Transfer(TransferAction {
            deposit: NearToken::from_yoctonear(amount_yocto),
        })],
    });

    sign_and_send(cfg, &unsigned_tx).await
}

async fn build_and_sign_ft(
    cfg: &Config,
    near_public_key: &str,
    to_id: &AccountId,
    amount: f64,
    contract_id: &str,
) -> Result<()> {
    let contract: AccountId = contract_id.parse()
        .with_context(|| format!("Invalid contract ID: {}", contract_id))?;

    let meta = ft::get_metadata(&cfg.network, &contract).await
        .with_context(|| format!("Could not fetch metadata for {}", contract_id))?;

    let raw_amount = (amount * 10f64.powi(meta.decimals as i32)) as u128;
    println!("Transfer {} {} → {} via MPC", amount, meta.symbol, to_id);
    println!("Contract: {}, Decimals: {}, Raw: {}\n", contract_id, meta.decimals, raw_amount);

    let pk = parse_pk(near_public_key)?;
    let (nonce, block_hash) = get_nonce_blockhash(cfg, &pk).await?;
    println!("① Nonce: {}", nonce);

    let ft_args = serde_json::json!({
        "receiver_id": to_id.as_str(),
        "amount": raw_amount.to_string(),
    });

    let unsigned_tx = Transaction::V0(TransactionV0 {
        signer_id: cfg.near_account.clone(),
        public_key: pk,
        nonce: nonce + 1,
        receiver_id: contract,
        block_hash,
        actions: vec![
            Action::Transfer(TransferAction { deposit: NearToken::from_yoctonear(1) }),
            Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "ft_transfer".to_string(),
                args: serde_json::to_vec(&ft_args)?,
                gas: NearGas::from_tgas(50),
                deposit: NearToken::from_yoctonear(1),
            })),
        ],
    });

    sign_and_send(cfg, &unsigned_tx).await
}

// ── Shared internals ─────────────────────────────────────────────────────────

fn parse_pk(key: &str) -> Result<PublicKey> {
    PublicKey::from_str(key).context("Invalid MPC-derived public key")
}

async fn get_nonce_blockhash(cfg: &Config, pk: &PublicKey) -> Result<(u64, CryptoHash)> {
    let access_key = Account(cfg.near_account.clone())
        .access_key(*pk)
        .fetch_from(&cfg.network)
        .await
        .context("Failed to fetch access key — account may not exist or key not registered")?;
    Ok((access_key.data.nonce.0, access_key.block_hash))
}

async fn sign_and_send(cfg: &Config, unsigned_tx: &Transaction) -> Result<()> {
    let tx_bytes = borsh::to_vec(unsigned_tx)?;
    let tx_hash: [u8; 32] = Sha256::digest(&tx_bytes).into();
    println!("② TX hash: {}", hex::encode(tx_hash));

    // Nostr authorization proof
    let auth_msg = format!("authorize from {} | hash:{}", cfg.near_account, hex::encode(tx_hash));
    let auth_sig = ed25519_dalek::Signer::sign(&cfg.nostr_sk, auth_msg.as_bytes());
    println!("③ Nostr auth: {}...✅", &hex::encode(auth_sig.to_bytes())[..16]);

    // Call MPC to sign + broadcast
    let (sponsor_id, sponsor_key) = cfg.require_sponsor()?;
    let tx_hash_str = mpc::sign_and_broadcast(
        unsigned_tx,
        &tx_hash,
        &cfg.mpc_path,
        cfg.near_account.as_str(),
        sponsor_id,
        sponsor_key,
        &cfg.network,
    ).await?;

    println!("④ TX finalized: {}", tx_hash_str);
    Ok(())
}
