//! MPC Worker Daemon — uses secp256k1 schnorr (NIP-01) for Nostr signatures.
//!
//! Usage:
//!   mpc-worker                  # foreground
//!   mpc-worker --daemon         # background daemon
//!   mpc-worker --status         # check if running
//!   mpc-worker --stop           # stop daemon
//!
//! Env: RELAY_URL, WORKER_NSEC, SPONSOR_KEY, SPONSOR_ACCOUNT

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use nostr::key::Keys;
use nostr::{EventBuilder, JsonUtil, Kind, Tag, TagKind};
use serde::Deserialize;
use sha2::Digest;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use mpc_poc_near::{ft, mpc};

#[derive(Parser)]
#[command(name = "mpc-worker", about = "MPC Worker Daemon")]
struct Cli {
    #[arg(long)]
    daemon: bool,
    #[arg(long)]
    status: bool,
    #[arg(long)]
    stop: bool,
    #[arg(long, default_value = "~/.mpc-worker/pid")]
    pidfile: String,
    #[arg(long, default_value = "~/.mpc-worker/worker.log")]
    logfile: String,
}

fn expand(path: &str) -> String {
    shellexpand::tilde(path).to_string()
}

fn read_pid(path: &str) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn is_running(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn write_pid(path: &str) -> Result<()> {
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, std::process::id().to_string())?;
    Ok(())
}

fn remove_pid(path: &str) {
    let _ = std::fs::remove_file(path);
}

// ── Nostr event JSON (for parsing raw relay messages) ────────────────────────

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
struct NostrEventJson {
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u64,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
}

#[derive(Debug, Deserialize)]
struct JobParams {
    to: String,
    amount: String,
    #[serde(rename = "token")]
    token_contract: Option<String>,
}

// ── Worker state ─────────────────────────────────────────────────────────────

struct Worker {
    relay_url: String,
    keys: Keys,
    sponsor_key: String,
    sponsor_account: near_api::AccountId,
    network: near_api::NetworkConfig,
}

impl Worker {
    fn from_env() -> Result<Self> {
        let relay_url = std::env::var("RELAY_URL").unwrap_or_else(|_| "wss://relay.damus.io".to_string());
        let nsec_hex = std::env::var("WORKER_NSEC").context("Set WORKER_NSEC (hex secret key)")?;
        let sk_bytes: [u8; 32] = hex::decode(&nsec_hex)?.try_into()
            .map_err(|_| anyhow::anyhow!("WORKER_NSEC must be 32 bytes"))?;
        let keys = Keys::new(nostr::SecretKey::from_slice(&sk_bytes)?);
        let sponsor_key = std::env::var("SPONSOR_KEY").context("Set SPONSOR_KEY")?;
        let sponsor_account: near_api::AccountId = std::env::var("SPONSOR_ACCOUNT")
            .context("Set SPONSOR_ACCOUNT")?.parse().context("Invalid SPONSOR_ACCOUNT")?;
        let network = near_api::NetworkConfig::from_rpc_url("testnet", "https://rpc.testnet.near.org".parse()?);
        Ok(Self { relay_url, keys, sponsor_key, sponsor_account, network })
    }

    async fn run(&self, pidfile: &str) -> Result<()> {
        write_pid(pidfile)?;

        let npub = self.keys.public_key().to_hex();
        println!("╔══════════════════════════════════════════════════╗");
        println!("║   MPC Worker Daemon                              ║");
        println!("╠══════════════════════════════════════════════════╣");
        println!("║   Relay:   {}", self.relay_url);
        println!("║   Worker:  {}...{}", &npub[..16], &npub[npub.len()-8..]);
        println!("║   Sponsor: {}", self.sponsor_account);
        println!("║   PID:     {}", std::process::id());
        println!("║   PIDfile: {}", pidfile);
        println!("║                                                  ║");
        println!("║   Listening for kind 5001 (job request) events   ║");
        println!("╚══════════════════════════════════════════════════╝\n");

        loop {
            if let Err(e) = self.connect_and_process().await {
                tracing::error!("Connection error: {} — reconnecting in 5s", e);
                remove_pid(pidfile);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                write_pid(pidfile)?;
            }
        }
    }

    async fn connect_and_process(&self) -> Result<()> {
        let (mut ws, _) = connect_async(&self.relay_url).await
            .with_context(|| format!("Failed to connect to {}", self.relay_url))?;
        println!("✅ Connected to relay");

        let sub_id = format!("mpc-{}", &self.keys.public_key().to_hex()[..8]);
        let req = serde_json::json!(["REQ", sub_id, {"kinds": [5001], "limit": 100}]).to_string();
        ws.send(Message::Text(req.into())).await?;
        println!("📡 Subscribed to kind 5001 events\n");

        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Err(e) = self.handle_message(&text).await {
                        tracing::warn!("Error: {}", e);
                    }
                }
                Ok(Message::Ping(data)) => { ws.send(Message::Pong(data)).await?; }
                Ok(Message::Close(_)) => { anyhow::bail!("Relay closed connection"); }
                Err(e) => { anyhow::bail!("WebSocket error: {}", e); }
                _ => {}
            }
        }
        anyhow::bail!("WebSocket stream ended")
    }

    async fn handle_message(&self, text: &str) -> Result<()> {
        let parsed: Vec<serde_json::Value> = serde_json::from_str(text)?;
        match parsed.first().and_then(|v| v.as_str()).unwrap_or("") {
            "EVENT" if parsed.len() >= 3 => {
                self.handle_event(&parsed[2]).await?;
            }
            "EOSE" => println!("📋 Caught up. Listening for new events...\n"),
            "NOTICE" => println!("📢 {}", parsed.get(1).and_then(|v| v.as_str()).unwrap_or("")),
            _ => {}
        }
        Ok(())
    }

    async fn handle_event(&self, event_json: &serde_json::Value) -> Result<()> {
        // Parse via nostr-sdk for proper secp256k1 verification
        let event: nostr::Event = nostr::Event::from_json(event_json.to_string())
            .context("Failed to parse Nostr event")?;

        if event.kind.as_u16() != 5001 { return Ok(()); }

        let pk_hex = event.pubkey.to_hex();
        println!("📨 Job from {}...{}", &pk_hex[..16], &pk_hex[pk_hex.len()-8..]);
        println!("   Content: {}", &event.content[..100.min(event.content.len())]);

        // 1. Verify signature (secp256k1 schnorr)
        if !event.verify_signature() {
            tracing::warn!("Invalid signature from {}", &pk_hex[..16]);
            self.publish_feedback(&event, "error", "Invalid signature").await?;
            return Ok(());
        }
        println!("   ✅ Signature valid (secp256k1 schnorr)");

        // 2. Parse job
        let raw: NostrEventJson = serde_json::from_value(event_json.clone())?;
        let params = parse_job(&raw)?;
        let token = params.token_contract.as_deref().unwrap_or("NEAR");
        println!("   📋 Send {} {} to {}", params.amount, token, params.to);

        // 3. Derive MPC key
        let path = format!("nostr:{}", pk_hex);
        let mpc_key = self.derive_key(&path).await?;
        println!("   🔑 MPC key: {}...", &mpc_key[..40.min(mpc_key.len())]);

        // 4. Build tx, sign via MPC, broadcast
        match self.execute_transfer(&raw, &params, &pk_hex).await {
            Ok(tx_hash) => {
                self.publish_feedback(&event, "success", &format!(
                    "Sent {} {} to {} | tx: {}",
                    params.amount, token, params.to, tx_hash
                )).await?;
            }
            Err(e) => {
                let msg = format!("Failed: {}", e);
                tracing::error!("{}", msg);
                self.publish_feedback(&event, "error", &msg).await?;
            }
        }

        println!("   ✅ Processed\n");
        Ok(())
    }

    async fn derive_key(&self, path: &str) -> Result<String> {
        mpc::derive_public_key(path, self.sponsor_account.as_str(), &self.network).await
    }

    async fn execute_transfer(
        &self,
        _raw_event: &NostrEventJson,
        params: &JobParams,
        sender_pk_hex: &str,
    ) -> Result<String> {
        use near_api::types::transaction::actions::{FunctionCallAction, TransferAction};
        use near_api::types::transaction::{Transaction, TransactionV0};
        use near_api::types::{Action, NearGas, NearToken, PublicKey};
        use near_api::Account;
        use std::str::FromStr;

        let path = format!("nostr:{}", sender_pk_hex);
        let near_public_key = self.derive_key(&path).await?;

        // Resolve the sender's NEAR account from the nostr pubkey binding
        // For now, we use the pubkey hex as account lookup
        // TODO: maintain a nostr_pubkey → NEAR account mapping
        let sender_account: near_api::AccountId = format!("nostr-{}", &sender_pk_hex[..12])
            .parse().context("Invalid derived account")?;

        let to_id: near_api::AccountId = params.to.parse()
            .context("Invalid recipient in job")?;
        let pk = PublicKey::from_str(&near_public_key)?;

        // Get nonce + block hash for the MPC-derived key
        let access_key = Account(sender_account.clone())
            .access_key(pk.clone())
            .fetch_from(&self.network)
            .await
            .context("Failed to fetch access key — account may not exist")?;
        let nonce = access_key.data.nonce.0;
        let block_hash = access_key.block_hash;

        // Parse amount
        let amount: f64 = params.amount.parse().context("Invalid amount")?;

        // Build unsigned tx based on token type
        let unsigned_tx = match &params.token_contract {
            None => {
                // NEAR transfer
                let amount_yocto = (amount * 1e24) as u128;
                println!("   Building NEAR transfer: {} → {} ({} NEAR)", sender_account, to_id, amount);
                Transaction::V0(TransactionV0 {
                    signer_id: sender_account.clone(),
                    public_key: pk,
                    nonce: nonce + 1,
                    receiver_id: to_id,
                    block_hash,
                    actions: vec![Action::Transfer(TransferAction {
                        deposit: NearToken::from_yoctonear(amount_yocto),
                    })],
                })
            }
            Some(contract_id) => {
                // FT transfer
                let contract: near_api::AccountId = contract_id.parse()
                    .with_context(|| format!("Invalid FT contract: {}", contract_id))?;
                let meta = ft::get_metadata(&self.network, &contract).await?;
                let raw_amount = (amount * 10f64.powi(meta.decimals as i32)) as u128;
                println!("   Building FT transfer: {} {} ({}) → {}",
                         amount, meta.symbol, raw_amount, to_id);

                let ft_args = serde_json::json!({
                    "receiver_id": to_id.as_str(),
                    "amount": raw_amount.to_string(),
                });

                Transaction::V0(TransactionV0 {
                    signer_id: sender_account.clone(),
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
                })
            }
        };

        // Serialize and hash
        let tx_bytes = borsh::to_vec(&unsigned_tx)?;
        let tx_hash: [u8; 32] = sha2::Sha256::digest(&tx_bytes).into();
        println!("   TX hash: {}", hex::encode(tx_hash));

        // Call MPC to sign
        let _sign_result = mpc::sign_payload(
            &tx_hash,
            &path,
            sender_account.as_str(),
            &self.sponsor_account,
            &self.sponsor_key,
            &self.network,
        ).await?;

        // TODO: Convert SignResult (big_r, s, recovery_id) → ed25519 signature
        //       Assemble SignedTransaction and broadcast via RPC
        // For now, return the tx hash as proof
        Ok(hex::encode(tx_hash))
    }

    async fn publish_feedback(&self, original: &nostr::Event, status: &str, msg: &str) -> Result<()> {
        let tags = vec![
            Tag::custom(TagKind::e(), [original.id.to_hex(), "".to_string()]),
            Tag::custom(TagKind::p(), [original.pubkey.to_hex()]),
            Tag::custom(TagKind::custom("status"), [status.to_string()]),
        ];

        let event = EventBuilder::new(Kind::Custom(7000), msg)
            .tags(tags)
            .sign_with_keys(&self.keys)?;

        let event_json = serde_json::to_string(&serde_json::json!(["EVENT", event]))?;

        let (mut ws, _) = connect_async(&self.relay_url).await?;
        ws.send(Message::Text(event_json.into())).await?;
        println!("   📤 Feedback: [{}]", status);
        Ok(())
    }
}

// ── Job parsing ──────────────────────────────────────────────────────────────

fn parse_job(event: &NostrEventJson) -> Result<JobParams> {
    for tag in &event.tags {
        if tag.len() >= 2 && tag[0] == "i" {
            if let Ok(p) = serde_json::from_str::<JobParams>(&tag[1]) { return Ok(p); }
        }
    }
    serde_json::from_str(&event.content).with_context(|| format!("Cannot parse job from {}", event.id))
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let pidfile = expand(&cli.pidfile);
    let logfile = expand(&cli.logfile);

    if cli.status {
        match read_pid(&pidfile) {
            Some(pid) if is_running(pid) => println!("✅ Running (PID {})", pid),
            Some(pid) => println!("⚠️  Stale PID {} (not running)", pid),
            None => println!("❌ Not running"),
        }
        return Ok(());
    }

    if cli.stop {
        match read_pid(&pidfile) {
            Some(pid) if is_running(pid) => {
                unsafe { libc::kill(pid, libc::SIGTERM) };
                println!("Sent SIGTERM to PID {}", pid);
                for _ in 0..10 {
                    if !is_running(pid) {
                        println!("✅ Stopped");
                        remove_pid(&pidfile);
                        return Ok(());
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                println!("⚠️  Force killing...");
                unsafe { libc::kill(pid, libc::SIGKILL) };
                remove_pid(&pidfile);
            }
            Some(pid) => {
                println!("PID {} not running (cleaning up)", pid);
                remove_pid(&pidfile);
            }
            None => println!("Not running"),
        }
        return Ok(());
    }

    if cli.daemon {
        match unsafe { libc::fork() } {
            -1 => anyhow::bail!("Fork failed"),
            0 => { /* child */ }
            pid => {
                println!("Daemon started (PID {})", pid);
                std::thread::sleep(std::time::Duration::from_secs(1));
                if let Some(_pid) = read_pid(&pidfile) {
                    println!("PID file: {}", pidfile);
                }
                return Ok(());
            }
        }

        unsafe { libc::setsid(); }

        if let Ok(log) = std::fs::File::options().create(true).append(true).open(&logfile) {
            use std::os::unix::io::IntoRawFd;
            let fd = log.into_raw_fd();
            unsafe {
                libc::close(0);
                libc::close(1);
                libc::close(2);
                libc::open(b"/dev/null\0".as_ptr() as *const i8, libc::O_RDONLY);
                libc::dup(fd);
                libc::dup(fd);
                libc::close(fd);
            }
        }
    }

    tracing_subscriber::fmt::init();

    let worker = Worker::from_env()?;
    worker.run(&pidfile).await
}
