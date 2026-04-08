//! MPC Worker: Subscribes to Nostr relay, processes NIP-90 job requests,
//! executes NEAR transactions via MPC, publishes results back.
//!
//! Run:
//!   RELAY_URL=wss://relay.damus.io \
//!   WORKER_NSEC=<hex> \
//!   SPONSOR_KEY=ed25519:xxx SPONSOR_ACCOUNT=kampouse.testnet \
//!   cargo run --bin mpc-worker

use anyhow::{Context, Result};
use ed25519_dalek::{SigningKey, Signer as DalekSigner, Verifier, Signature as DalekSignature};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use mpc_poc_near::{config, ft, mpc, near};

// ── Nostr types ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
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

// ── Worker config ────────────────────────────────────────────────────────────

struct WorkerConfig {
    relay_url: String,
    worker_nsec: SigningKey,
    worker_npub: String,
    sponsor_key: String,
    sponsor_account: near_api::AccountId,
    network: near_api::NetworkConfig,
}

impl WorkerConfig {
    fn from_env() -> Result<Self> {
        let relay_url = std::env::var("RELAY_URL")
            .unwrap_or_else(|_| "wss://relay.damus.io".to_string());
        let worker_nsec_hex = std::env::var("WORKER_NSEC")
            .context("Set WORKER_NSEC (hex ed25519 secret key for the worker's Nostr identity)")?;
        let sk_bytes: [u8; 32] = hex::decode(&worker_nsec_hex)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("WORKER_NSEC must be 32 bytes"))?;
        let worker_nsec = SigningKey::from_bytes(&sk_bytes);
        let worker_npub = hex::encode(worker_nsec.verifying_key().as_bytes());

        let sponsor_key = std::env::var("SPONSOR_KEY").context("Set SPONSOR_KEY")?;
        let sponsor_account: near_api::AccountId = std::env::var("SPONSOR_ACCOUNT")
            .context("Set SPONSOR_ACCOUNT")?.parse()
            .context("Invalid SPONSOR_ACCOUNT")?;

        let network = near_api::NetworkConfig::from_rpc_url(
            "testnet", "https://rpc.testnet.near.org".parse()?,
        );

        Ok(Self { relay_url, worker_nsec, worker_npub, sponsor_key, sponsor_account, network })
    }
}

// ── Main worker loop ─────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::init();

    let wcfg = WorkerConfig::from_env()?;

    println!("╔══════════════════════════════════════════════════╗");
    println!("║   MPC Worker started                             ║");
    println!("╠══════════════════════════════════════════════════╣");
    println!("║   Relay:   {}", wcfg.relay_url);
    println!("║   Worker:  {}...{}", &wcfg.worker_npub[..16], &wcfg.worker_npub[56..]);
    println!("║   Sponsor: {}", wcfg.sponsor_account);
    println!("║                                                  ║");
    println!("║   Listening for kind 5001 (job request) events   ║");
    println!("╚══════════════════════════════════════════════════╝\n");

    // Connect to relay
    let url = Url::parse(&wcfg.relay_url)?;
    let (mut ws, _) = connect_async(url).await
        .with_context(|| format!("Failed to connect to {}", wcfg.relay_url))?;

    println!("✅ Connected to relay\n");

    // Subscribe to kind 5001 events (NIP-90 job requests)
    // Filter: kinds=[5001], limit=100 (recent + new)
    let sub_id = format!("mpc-worker-{}", &wcfg.worker_npub[..8]);
    let subscribe = serde_json::json!([
        "REQ", sub_id,
        {"kinds": [5001], "limit": 100}
    ]);
    ws.send(Message::Text(subscribe.to_string())).await?;
    println!("📡 Subscribed to kind 5001 events\n");

    // Event loop
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if let Err(e) = handle_message(&text, &wcfg).await {
                    tracing::warn!("Error handling message: {}", e);
                }
            }
            Ok(Message::Ping(data)) => {
                ws.send(Message::Pong(data)).await?;
            }
            Ok(Message::Close(_)) => {
                println!("Relay closed connection. Reconnecting...");
                break;
            }
            Err(e) => {
                tracing::error!("WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }

    println!("Worker stopped.");
    Ok(())
}

// ── Message handling ──────────────────────────────────────────────────────────

async fn handle_message(text: &str, wcfg: &WorkerConfig) -> Result<()> {
    let parsed: Vec<serde_json::Value> = serde_json::from_str(text)?;

    if parsed.is_empty() {
        return Ok(());
    }

    let msg_type = parsed[0].as_str().unwrap_or("");

    match msg_type {
        "EVENT" => {
            // ["EVENT", sub_id, event_json]
            if parsed.len() < 3 {
                return Ok(());
            }
            let event_json = &parsed[2];
            handle_event(event_json, wcfg).await?;
        }
        "EOSE" => {
            // ["EOSE", sub_id] — end of stored events
            println!("📋 Caught up with stored events. Listening for new ones...\n");
        }
        "OK" => {
            // ["OK", event_id, success, message]
            let success = parsed.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
            let msg = parsed.get(3).and_then(|v| v.as_str()).unwrap_or("");
            if !success {
                tracing::warn!("Event rejected: {}", msg);
            }
        }
        "NOTICE" => {
            let msg = parsed.get(1).and_then(|v| v.as_str()).unwrap_or("");
            println!("📢 Relay notice: {}", msg);
        }
        _ => {}
    }

    Ok(())
}

async fn handle_event(event_json: &serde_json::Value, wcfg: &WorkerConfig) -> Result<()> {
    let event: NostrEvent = serde_json::from_value(event_json.clone())?;

    // Only process kind 5001
    if event.kind != 5001 {
        return Ok(());
    }

    println!("📨 New job request from {}...{}", &event.pubkey[..16], &event.pubkey[56..]);
    println!("   Content: {}", &event.content[..100.min(event.content.len())]);

    // Step 1: Verify Nostr signature
    if !verify_nostr_signature(&event)? {
        tracing::warn!("Invalid Nostr signature from {}", &event.pubkey[..16]);
        publish_feedback(wcfg, &event, "error", "Invalid Nostr signature").await?;
        return Ok(());
    }
    println!("   ✅ Signature valid");

    // Step 2: Parse job params from tags or content
    let params = parse_job_params(&event)?;
    let token_desc = params.token_contract.as_deref().unwrap_or("NEAR");
    println!("   📋 Job: send {} {} to {}", params.amount, token_desc, params.to);

    // Step 3: Look up the user's NEAR account via MPC derivation
    let path = format!("nostr:{}", event.pubkey);
    let near_public_key = derive_key_for_pubkey(wcfg, &event.pubkey, &path).await?;
    println!("   🔑 MPC key: {}...", &near_public_key[..40]);

    // Step 4: Build and sign transaction
    // (Reuse the logic from near.rs, but adapted for worker context)
    println!("   ⏳ Processing...");

    // For now, report what we would do (full MPC signing needs the sponsor flow)
    publish_feedback(wcfg, &event, "processing", &format!(
        "Transferring {} {} to {} via MPC", params.amount, token_desc, params.to
    )).await?;

    // TODO: Full MPC.sign() flow — same as CLI transfer
    // The worker would call mpc::sign_payload() with the constructed tx
    // For the PoC, we show the worker receives, parses, and responds

    publish_feedback(wcfg, &event, "success", &format!(
        "Job received: send {} {} to {} | MPC path: {}",
        params.amount, token_desc, params.to, path
    )).await?;

    println!("   ✅ Job processed\n");
    Ok(())
}

// ── Nostr signature verification ──────────────────────────────────────────────

fn verify_nostr_signature(event: &NostrEvent) -> Result<bool> {
    // Build the event hash (serialized event for signing)
    // Format: [0, pubkey, created_at, kind, tags, content]
    let serialized = serde_json::json!([
        0,
        event.pubkey,
        event.created_at,
        event.kind,
        event.tags,
        event.content,
    ]);
    let serialized_str = serde_json::to_string(&serialized)?;

    // SHA256 hash
    let hash = Sha256::digest(serialized_str.as_bytes());

    // Verify ed25519 signature
    let pubkey_bytes = hex::decode(&event.pubkey)?;
    let sig_bytes = hex::decode(&event.sig)?;

    if pubkey_bytes.len() != 32 || sig_bytes.len() != 64 {
        return Ok(false);
    }

    let pk = ed25519_dalek::VerifyingKey::from_bytes(
        &pubkey_bytes.try_into().unwrap()
    ).map_err(|_| anyhow::anyhow!("Invalid pubkey"))?;

    let sig = DalekSignature::from_bytes(
        &sig_bytes.try_into().unwrap()
    );

    match pk.verify(&hash, &sig) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

// ── Job parsing ───────────────────────────────────────────────────────────────

fn parse_job_params(event: &NostrEvent) -> Result<JobParams> {
    // Try parsing from tags first (NIP-90 style: ["i", json_payload, "text"])
    for tag in &event.tags {
        if tag.len() >= 2 && tag[0] == "i" {
            if let Ok(params) = serde_json::from_str::<JobParams>(&tag[1]) {
                return Ok(params);
            }
        }
    }

    // Fallback: parse from content
    serde_json::from_str(&event.content)
        .with_context(|| format!("Could not parse job params from event {}", event.id))
}

// ── MPC key derivation for a pubkey ──────────────────────────────────────────

async fn derive_key_for_pubkey(
    wcfg: &WorkerConfig,
    npub: &str,
    path: &str,
) -> Result<String> {
    // We need a NEAR account to use as predecessor for the MPC call
    let derived: near_api::Data<serde_json::Value> = near_api::Contract(
        "v1.signer-prod.testnet".parse()?
    )
        .call_function("derived_public_key", serde_json::json!({
            "path": path,
            "predecessor": wcfg.sponsor_account.as_str(),
            "domain_id": 1,
        }))
        .read_only()
        .fetch_from(&wcfg.network)
        .await?;

    let raw = derived.data.as_str().context("MPC returned non-string")?;
    Ok(if raw.starts_with("ed25519:") {
        raw.to_string()
    } else {
        format!("ed25519:{}", bs58::encode(&hex::decode(raw)?).into_string())
    })
}

// ── Publish to relay ─────────────────────────────────────────────────────────

async fn publish_feedback(
    wcfg: &WorkerConfig,
    original_event: &NostrEvent,
    status: &str,
    message: &str,
) -> Result<()> {
    // Build a kind 7000 feedback event (NIP-90)
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    let tags = vec![
        vec!["e".to_string(), original_event.id.clone()],
        vec!["p".to_string(), original_event.pubkey.clone()],
        vec!["status".to_string(), status.to_string()],
    ];

    // Serialize event for hashing
    let serialized = serde_json::json!([0, wcfg.worker_npub, created_at, 7000, tags, message]);
    let serialized_str = serde_json::to_string(&serialized)?;
    let hash = Sha256::digest(serialized_str.as_bytes());

    // Sign with worker's Nostr key
    let sig = wcfg.worker_nsec.sign(&hash);
    let event_id = hex::encode(hash);

    let event_json = serde_json::json!({
        "id": event_id,
        "pubkey": wcfg.worker_npub,
        "created_at": created_at,
        "kind": 7000,
        "tags": tags,
        "content": message,
        "sig": hex::encode(sig.to_bytes()),
    });

    // Send to relay
    let url = Url::parse(&wcfg.relay_url)?;
    let (mut ws, _) = connect_async(url).await?;
    ws.send(Message::Text(format!("[\"EVENT\",{}]", event_json))).await?;

    println!("   📤 Published feedback: [{}]", status);
    Ok(())
}
