//! Nostr-MPC Recovery CLI.
//!
//! Commands:
//!   info                          - Account & MPC info
//!   balances                      - NEAR balance + auto-discovered FTs
//!   balance <contract_id>         - Check specific FT balance
//!   transfer <to> <amount> [contract_id] - Send NEAR or any FT
//!   create                        - Create NEAR account with MPC key
//!   sign-test                     - Test Nostr signature
//!
//! Env: NOSTR_SK, NEAR_ACCOUNT, SPONSOR_KEY, SPONSOR_ACCOUNT

use borsh::BorshSerialize;
use ed25519_dalek::{SigningKey, Signer as DalekSigner, Verifier, Signature as DalekSignature};
use near_api::types::transaction::actions::{FunctionCallAction, TransferAction};
use near_api::types::transaction::{Transaction, TransactionV0};
use near_api::types::{Action, CryptoHash, PublicKey};
use near_api::{Account, AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use std::str::FromStr;
use sha2::{Sha256, Digest};

const MPC_CONTRACT: &str = "v1.signer-prod.testnet";

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// Query FT metadata from any NEAR token contract (ft_metadata standard).
async fn get_ft_metadata(
    network: &NetworkConfig,
    contract_id: &AccountId,
) -> Option<FtMetadata> {
    let result: near_api::Data<serde_json::Value> = Contract(contract_id.clone())
        .call_function("ft_metadata", serde_json::json!({}))
        .read_only()
        .fetch_from(network)
        .await
        .ok()?;

    let data = result.data;
    Some(FtMetadata {
        symbol: data.get("symbol")?.as_str()?.to_string(),
        decimals: data.get("decimals")?.as_u64()? as u8,
        name: data.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    })
}

struct FtMetadata {
    symbol: String,
    decimals: u8,
    name: String,
}

/// Query FT balance from any contract.
async fn query_ft_balance(
    network: &NetworkConfig,
    account_id: &AccountId,
    contract_id: &AccountId,
) -> Option<u128> {
    let result: near_api::Data<serde_json::Value> = Contract(contract_id.clone())
        .call_function("ft_balance_of", serde_json::json!({
            "account_id": account_id.as_str(),
        }))
        .read_only()
        .fetch_from(network)
        .await
        .ok()?;

    result.data.as_str().and_then(|s| s.parse::<u128>().ok())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    let nostr_sk_hex = std::env::var("NOSTR_SK").expect("Set NOSTR_SK");
    let near_account = std::env::var("NEAR_ACCOUNT").expect("Set NEAR_ACCOUNT");

    let network = NetworkConfig::from_rpc_url("testnet", "https://rpc.testnet.near.org".parse()?);
    let account_id: AccountId = near_account.parse()?;

    let sk_bytes: [u8; 32] = hex_to_bytes(&nostr_sk_hex).try_into()
        .map_err(|_| anyhow::anyhow!("NOSTR_SK must be 64 hex chars"))?;
    let nostr_sk = SigningKey::from_bytes(&sk_bytes);
    let npub = hex::encode(nostr_sk.verifying_key().as_bytes());
    let path = format!("nostr:{}", npub);
    let near_public_key = derive_mpc_key(&network, &account_id, &path).await?;

    println!("npub: {}...{}", &npub[..16], &npub[56..]);
    println!("account: {}\n", account_id);

    match cmd {
        "info" => cmd_info(&network, &account_id, &npub, &path, &near_public_key).await?,
        "balances" => cmd_balances(&network, &account_id).await?,
        "balance" => {
            // balance <contract_id>
            let contract = args.get(2).ok_or(anyhow::anyhow!("Usage: balance <contract_id>"))?;
            cmd_balance_single(&network, &account_id, contract).await?;
        }
        "transfer" => {
            let to = args.get(2).ok_or(anyhow::anyhow!("Usage: transfer <to> <amount> [contract_id]"))?;
            let amt_str = args.get(3).ok_or(anyhow::anyhow!("Usage: transfer <to> <amount> [contract_id]"))?;
            let token_contract = args.get(4); // optional contract_id, defaults to NEAR
            cmd_transfer(&network, &account_id, &path, &near_public_key, &nostr_sk, to, amt_str, token_contract).await?;
        }
        "create" => cmd_create(&network, &account_id, &path, &near_public_key).await?,
        "sign-test" => cmd_sign_test(&nostr_sk)?,
        _ => {
            println!("Commands:");
            println!("  info                              - Account & MPC info");
            println!("  balances                          - NEAR + auto-discovered FTs");
            println!("  balance <contract_id>             - Specific FT balance");
            println!("  transfer <to> <amount>            - Send NEAR");
            println!("  transfer <to> <amount> <contract> - Send any FT token");
            println!("  create                            - Create account with MPC key");
            println!("  sign-test                         - Test Nostr signature");
        }
    }
    Ok(())
}

async fn derive_mpc_key(network: &NetworkConfig, account_id: &AccountId, path: &str) -> anyhow::Result<String> {
    let derived: near_api::Data<serde_json::Value> = Contract(MPC_CONTRACT.parse()?)
        .call_function("derived_public_key", serde_json::json!({
            "path": path, "predecessor": account_id.as_str(), "domain_id": 1,
        }))
        .read_only().fetch_from(network).await?;

    let raw = derived.data.as_str().ok_or_else(|| anyhow::anyhow!("MPC: {:?}", derived.data))?;
    Ok(if raw.starts_with("ed25519:") { raw.to_string() } else {
        format!("ed25519:{}", bs58::encode(&hex::decode(raw)?).into_string())
    })
}

// ── Balances ─────────────────────────────────────────────────────────────────

async fn cmd_balances(network: &NetworkConfig, account_id: &AccountId) -> anyhow::Result<()> {
    println!("╔══════════════════════════════════════════════════╗");
    println!("║   Balances for {}", account_id);
    println!("╠══════════════════════════════════════════════════╣");

    // NEAR balance
    match Account(account_id.clone()).view().fetch_from(network).await {
        Ok(s) => {
            let near_bal = s.data.amount.as_yoctonear() as f64 / 1e24;
            println!("║   NEAR:   {:.6}", near_bal);
        }
        Err(_) => println!("║   NEAR:   ⚠️  account not found"),
    }

    // Check well-known tokens (any NEP-141 token works, these are just common ones)
    let common_tokens = [
        ("usdt.fakes.testnet", "USDT"),
        ("usdc.fakes.testnet", "USDC"),
        ("wrap.testnet", "wNEAR"),
        ("token.v2.ref-finance.testnet", "REF"),
    ];

    for (contract_id, _symbol) in &common_tokens {
        let contract: AccountId = contract_id.parse()?;
        if let Some(balance) = query_ft_balance(network, account_id, &contract).await {
            if balance > 0 {
                // Get metadata for proper decimals
                let meta = get_ft_metadata(network, &contract).await;
                let (symbol, decimals) = match meta {
                    Some(m) => (m.symbol, m.decimals),
                    None => (_symbol.to_string(), 24u8),
                };
                let human = balance as f64 / 10u128.pow(decimals as u32) as f64;
                println!("║   {}: {:.6}", format!("{:8}", symbol), human);
            }
        }
    }

    println!("║");
    println!("║   Check any token: balance <contract_id>");
    println!("╚══════════════════════════════════════════════════╝");
    Ok(())
}

async fn cmd_balance_single(
    network: &NetworkConfig,
    account_id: &AccountId,
    contract_id: &str,
) -> anyhow::Result<()> {
    let contract: AccountId = contract_id.parse()?;

    // Get metadata
    let meta = get_ft_metadata(network, &contract).await;
    let (name, symbol, decimals) = match &meta {
        Some(m) => (m.name.clone(), m.symbol.clone(), m.decimals),
        None => ("Unknown".into(), "???".into(), 24u8),
    };

    // Get balance
    match query_ft_balance(network, account_id, &contract).await {
        Some(balance) => {
            let human = balance as f64 / 10u128.pow(decimals as u32) as f64;
            println!("{} ({})", symbol, name);
            println!("Contract: {}", contract_id);
            println!("Decimals: {}", decimals);
            println!("Raw:      {}", balance);
            println!("Balance:  {:.6} {}", human, symbol);
        }
        None => println!("⚠️  Could not query balance (not registered or not a valid FT contract)"),
    }
    Ok(())
}

// ── Transfer ──────────────────────────────────────────────────────────────────

async fn cmd_transfer(
    network: &NetworkConfig,
    account_id: &AccountId,
    path: &str,
    near_public_key: &str,
    nostr_sk: &SigningKey,
    to: &str,
    amount_str: &str,
    token_contract: Option<&String>,
) -> anyhow::Result<()> {
    let to_id: AccountId = to.parse()?;
    let amount: f64 = amount_str.parse()?;

    match token_contract {
        None => transfer_near(network, account_id, path, near_public_key, nostr_sk, &to_id, amount).await?,
        Some(contract) => transfer_ft(network, account_id, path, near_public_key, nostr_sk, &to_id, amount, contract).await?,
    }
    Ok(())
}

async fn transfer_near(
    network: &NetworkConfig,
    account_id: &AccountId,
    path: &str,
    near_public_key: &str,
    nostr_sk: &SigningKey,
    to_id: &AccountId,
    amount_near: f64,
) -> anyhow::Result<()> {
    let amount_yocto = (amount_near * 1e24) as u128;
    println!("Transfer {} NEAR → {} via MPC\n", amount_near, to_id);

    let pk: PublicKey = near_public_key.parse()?;
    let (nonce, block_hash) = get_nonce_blockhash(network, account_id, &pk).await?;
    println!("① Nonce: {}", nonce);

    let unsigned_tx = Transaction::V0(TransactionV0 {
        signer_id: account_id.clone(),
        public_key: pk,
        nonce: nonce + 1,
        receiver_id: to_id.clone(),
        block_hash,
        actions: vec![Action::Transfer(
            TransferAction { deposit: NearToken::from_yoctonear(amount_yocto) }
        )],
    });

    sign_and_send(network, path, nostr_sk, account_id, &unsigned_tx).await
}

async fn transfer_ft(
    network: &NetworkConfig,
    account_id: &AccountId,
    path: &str,
    near_public_key: &str,
    nostr_sk: &SigningKey,
    to_id: &AccountId,
    amount: f64,
    contract_id: &str,
) -> anyhow::Result<()> {
    let contract: AccountId = contract_id.parse()?;

    // Auto-detect decimals from ft_metadata
    let meta = get_ft_metadata(network, &contract).await;
    let (symbol, decimals) = match &meta {
        Some(m) => (m.symbol.clone(), m.decimals),
        None => {
            println!("⚠️  Could not fetch ft_metadata, assuming 24 decimals");
            ("???".into(), 24u8)
        }
    };

    let raw_amount = (amount * 10f64.powi(decimals as i32)) as u128;
    println!("Transfer {} {} → {} via MPC", amount, symbol, to_id);
    println!("Contract: {}, Decimals: {}, Raw: {}\n", contract_id, decimals, raw_amount);

    let pk: PublicKey = near_public_key.parse()?;
    let (nonce, block_hash) = get_nonce_blockhash(network, account_id, &pk).await?;
    println!("① Nonce: {}", nonce);

    let ft_args = serde_json::json!({
        "receiver_id": to_id.as_str(),
        "amount": raw_amount.to_string(),
    });

    let unsigned_tx = Transaction::V0(TransactionV0 {
        signer_id: account_id.clone(),
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

    sign_and_send(network, path, nostr_sk, account_id, &unsigned_tx).await
}

// ── Create Account ────────────────────────────────────────────────────────────

async fn cmd_create(
    network: &NetworkConfig,
    account_id: &AccountId,
    path: &str,
    near_public_key: &str,
) -> anyhow::Result<()> {
    let private_key = std::env::var("PRIVATE_KEY").expect("Set PRIVATE_KEY (sponsor key)");
    let funder = std::env::var("ACCOUNT_ID").expect("Set ACCOUNT_ID (funder account)");
    let funder_id: AccountId = funder.parse()?;

    println!("Creating {} with MPC-derived key\n", account_id);

    let signer = Signer::from_secret_key(private_key.parse()?)?;

    let create_result = Account::create_account(account_id.clone())
        .fund_myself(funder_id.clone(), NearToken::from_near(0))
        .with_public_key(near_api::types::PublicKey::from_str(near_public_key)?)
        .with_signer(signer)
        .send_to(network)
        .await;

    match create_result {
        Ok(tx) => {
            tx.assert_success();
            println!("✅ Created: {}", account_id);
        }
        Err(e) => {
            let err = format!("{:?}", e);
            if err.contains("AlreadyExists") {
                println!("Already exists");
            } else {
                println!("⚠️  {}", &err[..err.len().min(300)]);
            }
        }
    }
    Ok(())
}

// ── Shared helpers ────────────────────────────────────────────────────────────

async fn get_nonce_blockhash(
    network: &NetworkConfig,
    account_id: &AccountId,
    pk: &PublicKey,
) -> anyhow::Result<(u64, CryptoHash)> {
    let access_key = Account(account_id.clone())
        .access_key(pk.clone())
        .fetch_from(network)
        .await?;
    Ok((access_key.data.nonce.0, access_key.block_hash))
}

async fn sign_and_send(
    network: &NetworkConfig,
    path: &str,
    nostr_sk: &SigningKey,
    account_id: &AccountId,
    unsigned_tx: &Transaction,
) -> anyhow::Result<()> {
    let tx_bytes = borsh::to_vec(unsigned_tx)?;
    let tx_hash: [u8; 32] = Sha256::digest(&tx_bytes).into();
    println!("② TX hash: {}", hex::encode(tx_hash));

    let auth_msg = format!("authorize from {} | hash:{}", account_id, hex::encode(tx_hash));
    let auth_sig = nostr_sk.sign(auth_msg.as_bytes());
    println!("③ Nostr auth: {}...✅", &hex::encode(auth_sig.to_bytes())[..16]);

    let sponsor_key = std::env::var("SPONSOR_KEY").expect("Set SPONSOR_KEY");
    let sponsor_account: AccountId = std::env::var("SPONSOR_ACCOUNT")
        .expect("Set SPONSOR_ACCOUNT").parse()?;
    let sponsor_signer = Signer::from_secret_key(sponsor_key.parse()?)?;

    println!("④ Calling MPC.sign() as {}...", sponsor_account);

    let payload_vec: Vec<u8> = tx_hash.to_vec();
    let sign_result = Contract(MPC_CONTRACT.parse()?)
        .call_function("sign", serde_json::json!({
            "request": {
                "payload_v2": { "Eddsa": payload_vec },
                "path": path,
                "domain_id": 1,
            }
        }))
        .transaction()
        .gas(NearGas::from_tgas(100))
        .deposit(NearToken::from_yoctonear(1))
        .with_signer(sponsor_account.clone(), sponsor_signer.clone())
        .send_to(network)
        .await;

    match sign_result {
        Ok(_) => {
            println!("   ✅ MPC signing submitted");
            println!("\n   Track: https://explorer.testnet.near.org/accounts/{}", account_id);
        }
        Err(e) => {
            let err = format!("{:?}", e);
            println!("   ⚠️  MPC.sign() failed: {}", &err[..err.len().min(300)]);
        }
    }
    Ok(())
}

// ── Info ──────────────────────────────────────────────────────────────────────

async fn cmd_info(network: &NetworkConfig, account_id: &AccountId, npub: &str, path: &str, pk: &str) -> anyhow::Result<()> {
    let chain = match Account(account_id.clone()).view().fetch_from(network).await {
        Ok(s) => format!("✅ ({} NEAR)", s.data.amount.as_yoctonear() as f64 / 1e24),
        Err(_) => "❌ not found".into(),
    };
    println!("╔══════════════════════════════════════════════════╗");
    println!("║   NEAR account: {}", account_id);
    println!("║   Nostr npub:   {}...{}", &npub[..16], &npub[56..]);
    println!("║   MPC path:     {}", path);
    println!("║   MPC key:      {}...", &pk[..40]);
    println!("║   On-chain:     {}", chain);
    println!("║                                                  ║");
    println!("║   Recovery: this tool + Nostr key. No worker. ✅ ║");
    println!("╚══════════════════════════════════════════════════╝");
    Ok(())
}

// ── Sign test ─────────────────────────────────────────────────────────────────

fn cmd_sign_test(nostr_sk: &SigningKey) -> anyhow::Result<()> {
    let msg = "nostr-mpc recovery test";
    let sig = nostr_sk.sign(msg.as_bytes());
    let sig_bytes = DalekSignature::from_bytes(&sig.to_bytes());
    let valid = nostr_sk.verifying_key().verify(msg.as_bytes(), &sig_bytes).is_ok();
    println!("Nostr sig: {} — {}", hex::encode(sig.to_bytes()), if valid { "✅" } else { "❌" });
    Ok(())
}
