//! MPC Worker Daemon.
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
use ed25519_dalek::{SigningKey, Signer as DalekSigner, Verifier, Signature as DalekSignature};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use mpc_poc_near::{config, ft, mpc, near};

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

// ── Nostr types ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
struct NostrEvent {
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
    worker_sk: SigningKey,
    worker_npub: String,
    sponsor_key: String,
    sponsor_account: near_api::AccountId,
    network: near_api::NetworkConfig,
}

impl Worker {
    fn from_env() -> Result<Self> {
        let relay_url = std::env::var("RELAY_URL").unwrap_or_else(|_| "wss://relay.damus.io".to_string());
        let nsec_hex = std::env::var("WORKER_NSEC").context("Set WORKER_NSEC")?;
        let sk_bytes: [u8; 32] = hex::decode(&nsec_hex)?.try_into()
            .map_err(|_| anyhow::anyhow!("WORKER_NSEC must be 32 bytes"))?;
        let worker_sk = SigningKey::from_bytes(&sk_bytes);
        let worker_npub = hex::encode(worker_sk.verifying_key().as_bytes());
        let sponsor_key = std::env::var("SPONSOR_KEY").context("Set SPONSOR_KEY")?;
        let sponsor_account: near_api::AccountId = std::env::var("SPONSOR_ACCOUNT")
            .context("Set SPONSOR_ACCOUNT")?.parse().context("Invalid SPONSOR_ACCOUNT")?;
        let network = near_api::NetworkConfig::from_rpc_url("testnet", "https://rpc.testnet.near.org".parse()?);
        Ok(Self { relay_url, worker_sk, worker_npub, sponsor_key, sponsor_account, network })
    }

    async fn run(&self, pidfile: &str) -> Result<()> {
        write_pid(pidfile)?;

        println!("╔══════════════════════════════════════════════════╗");
        println!("║   MPC Worker Daemon                              ║");
        println!("╠══════════════════════════════════════════════════╣");
        println!("║   Relay:   {}", self.relay_url);
        println!("║   Worker:  {}...{}", &self.worker_npub[..16], &self.worker_npub[56..]);
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
        let url = Url::parse(&self.relay_url)?;
        let (mut ws, _) = connect_async(url).await
            .with_context(|| format!("Failed to connect to {}", self.relay_url))?;
        println!("✅ Connected to relay");

        let sub_id = format!("mpc-{}", &self.worker_npub[..8]);
        ws.send(Message::Text(serde_json::json!([
            "REQ", sub_id, {"kinds": [5001], "limit": 100}
        ]).to_string())).await?;
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
        let event: NostrEvent = serde_json::from_value(event_json.clone())?;
        if event.kind != 5001 { return Ok(()); }

        println!("📨 Job from {}...{}", &event.pubkey[..16], &event.pubkey[56..]);
        println!("   Content: {}", &event.content[..100.min(event.content.len())]);

        // 1. Verify signature
        if !verify_nostr_sig(&event)? {
            tracing::warn!("Invalid signature from {}", &event.pubkey[..16]);
            self.publish(&event, "error", "Invalid signature").await?;
            return Ok(());
        }
        println!("   ✅ Signature valid");

        // 2. Parse job
        let params = parse_job(&event)?;
        let token = params.token_contract.as_deref().unwrap_or("NEAR");
        println!("   📋 Send {} {} to {}", params.amount, token, params.to);

        // 3. Derive MPC key
        let path = format!("nostr:{}", event.pubkey);
        let mpc_key = self.derive_key(&path).await?;
        println!("   🔑 MPC key: {}...", &mpc_key[..40]);

        // 4. Process (TODO: full MPC signing — currently acknowledges)
        self.publish(&event, "success", &format!(
            "Received: send {} {} to {} | path: {}", params.amount, token, params.to, path
        )).await?;

        println!("   ✅ Processed\n");
        Ok(())
    }

    async fn derive_key(&self, path: &str) -> Result<String> {
        let result: near_api::Data<serde_json::Value> = near_api::Contract("v1.signer-prod.testnet".parse()?)
            .call_function("derived_public_key", serde_json::json!({
                "path": path, "predecessor": self.sponsor_account.as_str(), "domain_id": 1,
            }))
            .read_only().fetch_from(&self.network).await?;
        let raw = result.data.as_str().context("MPC non-string")?;
        Ok(if raw.starts_with("ed25519:") { raw.to_string() }
           else { format!("ed25519:{}", bs58::encode(hex::decode(raw)?).into_string()) })
    }

    async fn publish(&self, original: &NostrEvent, status: &str, msg: &str) -> Result<()> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let tags = vec![
            vec!["e".into(), original.id.clone()],
            vec!["p".into(), original.pubkey.clone()],
            vec!["status".into(), status.into()],
        ];
        let ser = serde_json::json!([0, self.worker_npub, now, 7000, tags, msg]);
        let ser_str = serde_json::to_string(&ser)?;
        let hash = Sha256::digest(ser_str.as_bytes());
        let sig = self.worker_sk.sign(&hash);

        let event = serde_json::json!({
            "id": hex::encode(hash),
            "pubkey": self.worker_npub,
            "created_at": now,
            "kind": 7000,
            "tags": tags,
            "content": msg,
            "sig": hex::encode(sig.to_bytes()),
        });

        let url = Url::parse(&self.relay_url)?;
        let (mut ws, _) = connect_async(url).await?;
        ws.send(Message::Text(format!("[\"EVENT\",{}]", event))).await?;
        println!("   📤 Feedback: [{}]", status);
        Ok(())
    }
}

// ── Nostr signature verification ──────────────────────────────────────────────

fn verify_nostr_sig(event: &NostrEvent) -> Result<bool> {
    let ser = serde_json::to_string(&serde_json::json!([
        0, event.pubkey, event.created_at, event.kind, event.tags, event.content
    ]))?;
    let hash = Sha256::digest(ser.as_bytes());
    let pk_bytes = hex::decode(&event.pubkey)?;
    let sig_bytes = hex::decode(&event.sig)?;
    if pk_bytes.len() != 32 || sig_bytes.len() != 64 { return Ok(false); }
    let pk = ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes.try_into().unwrap())
        .map_err(|_| anyhow::anyhow!("bad pk"))?;
    let sig = DalekSignature::from_bytes(&sig_bytes.try_into().unwrap());
    Ok(pk.verify(&hash, &sig).is_ok())
}

fn parse_job(event: &NostrEvent) -> Result<JobParams> {
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
        // Double-fork to detach
        match unsafe { libc::fork() } {
            -1 => anyhow::bail!("Fork failed"),
            0 => { /* child */ }
            pid => {
                // Parent exits
                println!("Daemon started (PID {})", pid);
                // Wait for child to write PID
                std::thread::sleep(std::time::Duration::from_secs(1));
                if let Some(pid) = read_pid(&pidfile) {
                    println!("PID file: {}", pidfile);
                }
                return Ok(());
            }
        }

        // Detach from terminal
        unsafe { libc::setsid(); }

        // Redirect stdout/stderr to log
        if let Ok(log) = std::fs::File::options().create(true).append(true).open(&logfile) {
            use std::os::unix::io::IntoRawFd;
            let fd = log.into_raw_fd();
            unsafe {
                libc::close(0); // stdin
                libc::close(1); // stdout
                libc::close(2); // stderr
                libc::open("/dev/null", libc::O_RDONLY); // fd 0
                libc::dup(fd); // fd 1
                libc::dup(fd); // fd 2
                libc::close(fd);
            }
        }
    }

    tracing_subscriber::init();

    let worker = Worker::from_env()?;
    worker.run(&pidfile).await
}
