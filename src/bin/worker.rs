//! MPC Worker Daemon — Nostr → MPC → NEAR with Lightning payments
//!
//! Usage:
//!   mpc-worker                  # foreground
//!   mpc-worker --daemon         # background daemon
//!   mpc-worker --status         # check if running
//!   mpc-worker --stop           # stop daemon
//!
//! Env: RELAY_URL, WORKER_NSEC, SPONSOR_KEY, SPONSOR_ACCOUNT
//!      NWC_URL (optional, for real Lightning payments)
//!      NO_PAYMENT (optional, skip payment entirely)

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use nostr::key::Keys;
use nostr::{EventBuilder, JsonUtil, Kind, Tag, TagKind};
use serde::Deserialize;
use sha2::Digest;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use mpc_poc_near::{ft, mpc, payments};

// ── Clap ─────────────────────────────────────────────────────────────────────

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

// ── Helpers ──────────────────────────────────────────────────────────────────

fn expand(path: &str) -> String { shellexpand::tilde(path).to_string() }
fn read_pid(path: &str) -> Option<i32> { std::fs::read_to_string(path).ok()?.trim().parse().ok() }
fn is_running(pid: i32) -> bool { unsafe { libc::kill(pid, 0) == 0 } }
fn write_pid(path: &str) -> Result<()> {
    if let Some(dir) = std::path::Path::new(path).parent() { std::fs::create_dir_all(dir)?; }
    std::fs::write(path, std::process::id().to_string())?;
    Ok(())
}
fn remove_pid(path: &str) { let _ = std::fs::remove_file(path); }

/// Track processed events with TTL-based eviction (#1)
struct ProcessedCache {
    entries: Mutex<Vec<(String, Instant)>>,
    max_size: usize,
    ttl: Duration,
}

impl ProcessedCache {
    fn new(max_size: usize, ttl: Duration) -> Self {
        Self { entries: Mutex::new(Vec::new()), max_size, ttl }
    }

    /// Returns true if this is a new event, false if duplicate
    fn insert(&self, id: &str) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let now = Instant::now();

        // Prune expired entries
        entries.retain(|(_, ts)| now.duration_since(*ts) < self.ttl);

        // Check if already present
        if entries.iter().any(|(eid, _)| eid == id) {
            return false;
        }

        // Evict oldest if at capacity
        if entries.len() >= self.max_size {
            entries.remove(0);
        }

        entries.push((id.to_string(), now));
        true
    }
}

// ── Event types ──────────────────────────────────────────────────────────────

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
    #[serde(deserialize_with = "deserialize_amount")]
    amount: f64,
    #[serde(rename = "token")]
    token_contract: Option<String>,
    #[serde(rename = "account")]
    account_name: Option<String>,
}

fn deserialize_amount<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<f64, D::Error> {
    let v: f64 = f64::deserialize(d)?;
    if v <= 0.0 { return Err(serde::de::Error::custom("amount must be positive")); }
    if v > 1_000_000.0 { return Err(serde::de::Error::custom("amount too large")); }
    Ok(v)
}

fn validate_account_name(name: &str) -> Result<String> {
    let valid = name.len() >= 2
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.' || c == '_')
        && (name.ends_with(".testnet") || name.ends_with(".near"));
    if !valid {
        anyhow::bail!("Invalid account name '{}'. Use 2-64 lowercase alphanumeric/hyphen chars ending in .testnet", name);
    }
    Ok(name.to_string())
}

// ── Worker state ─────────────────────────────────────────────────────────────

struct Worker {
    relay_url: String,
    keys: Keys,
    sponsor_key: String,
    sponsor_account: near_api::AccountId,
    network: near_api::NetworkConfig,
    pricing: payments::Pricing,
    payment: Box<dyn payments::PaymentProvider>,
    processed: ProcessedCache,
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

        let payment: Box<dyn payments::PaymentProvider> = if std::env::var("NWC_URL").is_ok() {
            tracing::info!("Payment: NIP-47 (NWC)");
            Box::new(payments::NwcPaymentProvider::from_env()?)
        } else if std::env::var("NO_PAYMENT").is_ok() {
            tracing::info!("Payment: disabled (free mode)");
            Box::new(payments::FreePaymentProvider::new())
        } else {
            tracing::info!("Payment: mock (auto-approve)");
            Box::new(payments::MockPaymentProvider::auto_approving())
        };

        Ok(Self {
            relay_url, keys, sponsor_key, sponsor_account, network,
            pricing: payments::Pricing::default(),
            payment,
            processed: ProcessedCache::new(100_000, Duration::from_secs(3600)), // 100k entries, 1h TTL
        })
    }

    fn is_duplicate(&self, event_id: &str) -> bool {
        !self.processed.insert(event_id)
    }

    async fn run(&self, pidfile: &str) -> Result<()> {
        write_pid(pidfile)?;
        let npub = self.keys.public_key().to_hex();
        tracing::info!("Worker started: {}...{}", &npub[..16], &npub[npub.len()-8..]);
        tracing::info!("Relay: {} | Sponsor: {}", self.relay_url, self.sponsor_account);

        // #2: exponential backoff on reconnect
        let mut backoff_secs: u64 = 1;
        let max_backoff: u64 = 300; // cap at 5 min

        loop {
            if let Err(e) = self.connect_and_process().await {
                tracing::error!("Connection error: {} — retrying in {}s", e, backoff_secs);
                remove_pid(pidfile);
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff);
                write_pid(pidfile)?;
            } else {
                // Clean disconnect — reset backoff
                backoff_secs = 1;
            }
        }
    }

    async fn connect_and_process(&self) -> Result<()> {
        let (mut ws, _) = connect_async(&self.relay_url).await
            .with_context(|| format!("Failed to connect to {}", self.relay_url))?;
        tracing::info!("Connected to relay");

        let sub_id = format!("mpc-{}", &self.keys.public_key().to_hex()[..8]);
        let req = serde_json::json!(["REQ", sub_id, {"kinds": [5000, 5001], "limit": 100}]).to_string();
        ws.send(Message::Text(req.into())).await?;
        tracing::info!("Subscribed to kind 5000 (register) & 5001 (transfer)");

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
            "EVENT" if parsed.len() >= 3 => { self.handle_event(&parsed[2]).await?; }
            "EOSE" => tracing::info!("Caught up, listening..."),
            "NOTICE" => tracing::warn!("Relay notice: {}", parsed.get(1).and_then(|v| v.as_str()).unwrap_or("")),
            _ => {}
        }
        Ok(())
    }

    async fn handle_event(&self, event_json: &serde_json::Value) -> Result<()> {
        let event: nostr::Event = nostr::Event::from_json(event_json.to_string())
            .context("Failed to parse Nostr event")?;
        let kind = event.kind.as_u16();
        let event_id = event.id.to_hex();

        if self.is_duplicate(&event_id) {
            tracing::debug!("Skipping duplicate {}", &event_id[..16]);
            return Ok(());
        }

        let pk_hex = event.pubkey.to_hex();
        match kind {
            5000 => self.handle_registration(&event, &pk_hex, event_json).await,
            5001 => self.handle_transfer(&event, &pk_hex, event_json).await,
            _ => Ok(()),
        }
    }

    // ── Registration (kind 5000) ────────────────────────────────────────────

    async fn handle_registration(
        &self,
        event: &nostr::Event,
        pk_hex: &str,
        event_json: &serde_json::Value,
    ) -> Result<()> {
        if !event.verify_signature() {
            self.publish_feedback(event, "error", "Invalid signature").await?;
            return Ok(());
        }

        let path = format!("nostr:{}", pk_hex);
        let near_public_key = mpc::derive_public_key(&path, self.sponsor_account.as_str(), &self.network).await?;

        let raw: NostrEventJson = serde_json::from_value(event_json.clone())?;
        let account_name = match parse_account_name(&raw) {
            Some(name) => match validate_account_name(&name) {
                Ok(n) => n,
                Err(e) => {
                    self.publish_feedback(event, "error", &e.to_string()).await?;
                    return Ok(());
                }
            },
            None => format!("n{}-{}.testnet", &pk_hex[..4], &pk_hex[4..12]),
        };

        let account_id: near_api::AccountId = match account_name.parse() {
            Ok(id) => id,
            Err(e) => {
                self.publish_feedback(event, "error", &format!("Invalid account: {}", e)).await?;
                return Ok(());
            }
        };

        tracing::info!("Register: {} → {} (key: {}...)", &pk_hex[..16], account_id, &near_public_key[..24]);

        // #6: Create account with a small initial deposit for gas
        match self.create_account(&account_id, &near_public_key).await {
            Ok(()) => {
                let msg = format!("account:{}|key:{}|path:{}", account_id, near_public_key, path);
                self.publish_feedback(event, "success", &msg).await?;
            }
            Err(e) => {
                let err_str = format!("{}", e);
                if err_str.contains("AlreadyExists") || err_str.contains("already") {
                    let msg = format!("account:{}|key:{}|path:{}|status:exists", account_id, near_public_key, path);
                    self.publish_feedback(event, "success", &msg).await?;
                } else {
                    self.publish_feedback(event, "error", &format!("Registration failed: {}", e)).await?;
                }
            }
        }
        Ok(())
    }

    async fn create_account(
        &self,
        account_id: &near_api::AccountId,
        public_key: &str,
    ) -> Result<()> {
        use near_api::{Account, NearToken, Signer};
        use near_api::types::PublicKey;
        use std::str::FromStr;

        let signer = Signer::from_secret_key(self.sponsor_key.parse()?)?;
        let pk = PublicKey::from_str(public_key)?;

        // #6: Deposit 0.05 NEAR for initial gas
        let deposit = NearToken::from_millinear(50); // 50 mN = 0.05 NEAR
        let result = Account::create_account(account_id.clone())
            .fund_myself(self.sponsor_account.clone(), deposit)
            .with_public_key(pk)
            .with_signer(signer)
            .send_to(&self.network)
            .await
            .map_err(|e| anyhow::anyhow!("Create account failed: {:?}", e))?;

        result.assert_success();
        tracing::info!("Created {} with 0.05 NEAR initial deposit", account_id);
        Ok(())
    }

    // ── Transfer (kind 5001) ────────────────────────────────────────────────

    async fn handle_transfer(
        &self,
        event: &nostr::Event,
        pk_hex: &str,
        event_json: &serde_json::Value,
    ) -> Result<()> {
        if !event.verify_signature() {
            self.publish_feedback(event, "error", "Invalid signature").await?;
            return Ok(());
        }

        // #4: Parse from tag only (explicit), not fallback to content
        let raw: NostrEventJson = serde_json::from_value(event_json.clone())?;
        let params = match parse_job_strict(&raw) {
            Ok(p) => p,
            Err(e) => {
                self.publish_feedback(event, "error", &format!("Invalid job params: {}", e)).await?;
                return Ok(());
            }
        };

        let to_id: near_api::AccountId = match params.to.parse() {
            Ok(id) => id,
            Err(e) => {
                self.publish_feedback(event, "error", &format!("Invalid recipient '{}': {}", params.to, e)).await?;
                return Ok(());
            }
        };

        let token = params.token_contract.as_deref().unwrap_or("NEAR");

        // Payment flow
        let payment_hash_tag = event.tags.iter()
            .find(|t| t.kind() == TagKind::custom("payment_hash"))
            .and_then(|t| t.content());

        if let Some(ph) = payment_hash_tag {
            // #5: Validate payment hash format
            if ph.len() != 64 || !ph.chars().all(|c| c.is_ascii_hexdigit()) {
                self.publish_feedback(event, "error", "Invalid payment_hash format").await?;
                return Ok(());
            }

            match self.payment.check_payment(ph).await {
                Ok(payments::PaymentStatus::Paid) => {
                    tracing::info!("Payment confirmed: {}...", &ph[..16]);
                }
                Ok(payments::PaymentStatus::Expired) => {
                    self.publish_feedback(event, "error", "Payment expired").await?;
                    return Ok(());
                }
                Ok(payments::PaymentStatus::Pending) => {
                    self.publish_feedback(event, "error", &format!("Payment not confirmed: {}", ph)).await?;
                    return Ok(());
                }
                Err(e) => {
                    self.publish_feedback(event, "error", &format!("Payment check failed: {}", e)).await?;
                    return Ok(());
                }
            }
        } else {
            let price_sats = if token == "NEAR" {
                self.pricing.price_transfer(params.amount)
            } else {
                self.pricing.price_ft_transfer()
            };

            let desc = format!("NEAR: {} {} → {}", params.amount, token, to_id);
            match self.payment.create_invoice(price_sats, &desc).await {
                Ok(invoice) => {
                    let msg = format!(
                        "payment_required|invoice:{}|amount:{}|hash:{}|expires:{}",
                        invoice.bolt11, invoice.amount_sats, invoice.payment_hash, invoice.expires_at,
                    );
                    self.publish_feedback(event, "payment_required", &msg).await?;
                    return Ok(());
                }
                Err(e) => {
                    self.publish_feedback(event, "error", &format!("Invoice failed: {}", e)).await?;
                    return Ok(());
                }
            }
        }

        // Execute transfer
        let path = format!("nostr:{}", pk_hex);
        let account_name = match &params.account_name {
            Some(name) => validate_account_name(name)?,
            None => format!("n{}-{}.testnet", &pk_hex[..4], &pk_hex[4..12]),
        };

        match self.execute_transfer(&params, &account_name, &path).await {
            Ok(tx_hash) => {
                let msg = format!("Sent {} {} to {} | tx: {}", params.amount, token, to_id, tx_hash);
                self.publish_feedback(event, "success", &msg).await?;
            }
            Err(e) => {
                tracing::error!("Transfer failed: {}", e);
                self.publish_feedback(event, "error", &format!("Failed: {}", e)).await?;
            }
        }
        Ok(())
    }

    async fn execute_transfer(
        &self,
        params: &JobParams,
        account_name: &str,
        path: &str,
    ) -> Result<String> {
        use near_api::types::transaction::actions::{FunctionCallAction, TransferAction};
        use near_api::types::transaction::{Transaction, TransactionV0};
        use near_api::types::{Action, NearGas, NearToken, PublicKey};
        use near_api::Account;
        use std::str::FromStr;

        let near_public_key = mpc::derive_public_key(path, self.sponsor_account.as_str(), &self.network).await?;
        let sender_account: near_api::AccountId = account_name.parse()
            .context("Invalid derived account")?;
        let to_id: near_api::AccountId = params.to.parse().context("Invalid recipient")?;
        let pk = PublicKey::from_str(&near_public_key)?;
        let sender_id_for_mpc = sender_account.clone();

        let access_key = Account(sender_account.clone())
            .access_key(pk.clone())
            .fetch_from(&self.network)
            .await
            .context("Failed to fetch access key — account may not exist or key not registered")?;
        let nonce = access_key.data.nonce.0;
        let block_hash = access_key.block_hash;

        let unsigned_tx = match &params.token_contract {
            None => {
                let amount_yocto = (params.amount * 1e24) as u128;
                Transaction::V0(TransactionV0 {
                    signer_id: sender_account,
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
                let contract: near_api::AccountId = contract_id.parse()
                    .with_context(|| format!("Invalid FT contract: {}", contract_id))?;
                let meta = ft::get_metadata(&self.network, &contract).await?;
                let raw_amount = (params.amount * 10f64.powi(meta.decimals as i32)) as u128;
                let ft_args = serde_json::json!({"receiver_id": to_id.as_str(), "amount": raw_amount.to_string()});
                Transaction::V0(TransactionV0 {
                    signer_id: sender_account,
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

        let tx_bytes = borsh::to_vec(&unsigned_tx)?;
        let tx_hash: [u8; 32] = sha2::Sha256::digest(&tx_bytes).into();

        // #14: Use sign_and_broadcast_async for faster submission
        mpc::sign_and_broadcast_async(
            &unsigned_tx, &tx_hash, path,
            sender_id_for_mpc.as_str(), &self.sponsor_account, &self.sponsor_key, &self.network,
        ).await
    }

    // ── Feedback ────────────────────────────────────────────────────────────

    async fn publish_feedback(&self, original: &nostr::Event, status: &str, msg: &str) -> Result<()> {
        let tags = vec![
            Tag::custom(TagKind::e(), [original.id.to_hex(), "".to_string()]),
            Tag::custom(TagKind::p(), [original.pubkey.to_hex()]),
            Tag::custom(TagKind::custom("status"), [status.to_string()]),
        ];

        let event = EventBuilder::new(Kind::Custom(7000), msg)
            .tags(tags)
            .sign_with_keys(&self.keys)?;

        let (mut ws, _) = connect_async(&self.relay_url).await?;
        ws.send(Message::Text(serde_json::json!(["EVENT", event]).to_string().into())).await?;
        tracing::info!("Feedback [{}]: {}", status, &msg[..80.min(msg.len())]);
        Ok(())
    }
}

// ── Job parsing ──────────────────────────────────────────────────────────────

/// #4: Strict parse — only from 'i' tag, never falls back to content
fn parse_job_strict(event: &NostrEventJson) -> Result<JobParams> {
    for tag in &event.tags {
        if tag.len() >= 2 && tag[0] == "i" {
            return serde_json::from_str::<JobParams>(&tag[1])
                .with_context(|| format!("Invalid job in 'i' tag: {}", tag[1]));
        }
    }
    // If no 'i' tag, try content as fallback (explicit)
    serde_json::from_str(&event.content)
        .with_context(|| format!("No 'i' tag and cannot parse content as job: {}", event.id))
}

fn parse_account_name(event: &NostrEventJson) -> Option<String> {
    for tag in &event.tags {
        if tag.len() >= 2 && tag[0] == "n" { return Some(tag[1].clone()); }
    }
    if event.content.starts_with("account:") {
        return Some(event.content[8..].trim().to_string());
    }
    None
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
                // #3: Graceful shutdown — give time for in-flight ops
                unsafe { libc::kill(pid, libc::SIGTERM) };
                println!("Sent SIGTERM to PID {} (graceful shutdown, 15s)", pid);
                for _ in 0..30 {
                    if !is_running(pid) {
                        println!("✅ Stopped gracefully");
                        remove_pid(&pidfile);
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                println!("⚠️  Force killing...");
                unsafe { libc::kill(pid, libc::SIGKILL) };
                remove_pid(&pidfile);
            }
            Some(pid) => { println!("PID {} not running (cleaning up)", pid); remove_pid(&pidfile); }
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
                std::thread::sleep(Duration::from_secs(1));
                return Ok(());
            }
        }
        unsafe { libc::setsid(); }

        // #15: Always log to file (daemon or foreground with --logfile)
        if let Ok(log) = std::fs::File::options().create(true).append(true).open(&logfile) {
            use std::os::unix::io::IntoRawFd;
            let fd = log.into_raw_fd();
            unsafe {
                libc::close(0); libc::close(1); libc::close(2);
                libc::open(b"/dev/null\0".as_ptr() as *const i8, libc::O_RDONLY);
                libc::dup(fd); libc::dup(fd); libc::close(fd);
            }
        }
    }

    // #15: Log to file even in foreground mode
    let log_file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&logfile)
        .ok();
    if let Some(file) = log_file {
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt::init();
    }

    let worker = Worker::from_env()?;
    worker.run(&pidfile).await
}
