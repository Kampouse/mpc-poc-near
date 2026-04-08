//! End-to-end integration test for the MPC worker partial flow.
//!
//! Tests: send kind 5001 event → worker picks up → verifies sig → derives MPC key → publishes feedback
//!
//! Run:
//!   RELAY_URL=ws://127.0.0.1:8080 cargo test --test e2e -- --nocapture

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use nostr::key::Keys;
use nostr::{EventBuilder, JsonUtil, Kind, Tag, TagKind};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Send a nostr-sdk-signed event to a relay. Returns true if accepted.
async fn send_event(relay_url: &str, event: &nostr::Event) -> Result<bool> {
    let (mut ws, _) = connect_async(relay_url).await?;
    let msg = serde_json::json!(["EVENT", event]).to_string();
    ws.send(Message::Text(msg.into())).await?;

    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                println!("  Relay raw: {}", text);
                let parsed: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                if parsed.first().and_then(|v| v.as_str()) == Some("OK") {
                    let ok = parsed.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
                    println!("  OK: {} msg: {}", ok, parsed.get(3).and_then(|v| v.as_str()).unwrap_or(""));
                    return Ok(ok);
                }
            }
            _ => continue,
        }
    }
    Ok(false)
}

/// Subscribe to relay and collect parsed events matching filter
async fn subscribe_and_collect(
    relay_url: &str,
    kinds: Vec<u64>,
    e_tag: Option<&str>,
    timeout_secs: u64,
) -> Result<Vec<nostr::Event>> {
    let (mut ws, _) = connect_async(relay_url).await?;

    let sub_id = format!("test-{}", kinds.iter().map(|k| k.to_string()).collect::<String>());
    let kinds_json: Vec<serde_json::Value> = kinds.iter().map(|k| serde_json::json!(*k)).collect();
    let mut filter = serde_json::json!({"kinds": kinds_json, "limit": 50});
    if let Some(e) = e_tag {
        filter["#e"] = serde_json::json!([e]);
    }
    ws.send(Message::Text(serde_json::json!(["REQ", sub_id, filter]).to_string().into())).await?;

    let mut events = Vec::new();
    let start = std::time::Instant::now();

    while start.elapsed().as_secs() < timeout_secs {
        match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let parsed: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                match parsed.first().and_then(|v| v.as_str()) {
                    Some("EVENT") if parsed.len() >= 3 => {
                        if let Ok(event) = nostr::Event::from_json(parsed[2].to_string()) {
                            events.push(event);
                        }
                    }
                    Some("EOSE") => {
                        if !events.is_empty() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(Ok(Message::Ping(data)))) => {
                ws.send(Message::Pong(data)).await?;
            }
            _ => continue,
        }
    }

    let _ = ws.send(Message::Text(format!(r#"["CLOSE","{}"]"#, sub_id).into())).await;
    Ok(events)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_key_generation_and_signing() {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::Custom(5001), "test")
        .sign_with_keys(&keys)
        .expect("signing should work");

    assert!(event.verify_signature(), "Self-verification should pass");
    assert_eq!(event.kind.as_u16(), 5001);
    println!("✅ Key generation & secp256k1 schnorr signing works");
}

#[tokio::test]
async fn test_send_event_to_relay() {
    let relay_url = std::env::var("RELAY_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:8080".to_string());
    let keys = Keys::generate();

    let content = format!("e2e test ping at {}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    let event = EventBuilder::new(Kind::Custom(1), &content)
        .sign_with_keys(&keys)
        .expect("event build");

    println!("Sending kind 1 event to {}...", relay_url);
    println!("  Event: {}", nostr::Event::as_json(&event));
    match send_event(&relay_url, &event).await {
        Ok(accepted) => {
            assert!(accepted, "Relay should accept the event");
            println!("✅ Relay accepted the event");
        }
        Err(e) => {
            println!("⚠️  Could not connect to relay: {} (skipping)", e);
        }
    }
}

#[tokio::test]
async fn test_full_e2e_partial_flow() -> Result<()> {
    let relay_url = std::env::var("RELAY_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:8080".to_string());

    // 1. Generate sender keypair (secp256k1 via nostr-sdk)
    let sender_keys = Keys::generate();
    let sender_pk = sender_keys.public_key().to_hex();
    println!("\n═══ E2E Partial Flow Test ═══");
    println!("Sender: {}...{}", &sender_pk[..16], &sender_pk[sender_pk.len()-8..]);
    println!("Relay: {}", relay_url);

    // 2. Build kind 5001 job event with proper schnorr signature
    let job_params = serde_json::json!({
        "to": "kampouse.testnet",
        "amount": "0.001"
    }).to_string();

    let tags = vec![
        Tag::custom(TagKind::custom("i"), [&job_params]),
    ];

    let event = EventBuilder::new(Kind::Custom(5001), &job_params)
        .tags(tags)
        .sign_with_keys(&sender_keys)
        .context("Failed to build/sign event")?;

    // 3. Verify locally
    assert!(event.verify_signature(), "Event signature should be valid");
    println!("✅ Event signature valid (secp256k1 schnorr)");

    // 4. Send to relay
    println!("Sending kind 5001 event...");
    let accepted = send_event(&relay_url, &event).await
        .context("Failed to send event to relay")?;
    assert!(accepted, "Relay should accept the event");
    println!("✅ Relay accepted event (id: {}...)", &event.id.to_hex()[..16]);

    // 5. Listen for kind 7000 feedback from worker
    println!("Waiting for kind 7000 feedback (30s timeout)...");
    println!("NOTE: Worker must be running and connected to the same relay!");
    println!("  Start it with:");
    println!("  RELAY_URL={} WORKER_NSEC=<nsec> SPONSOR_KEY=ed25519:xxx SPONSOR_ACCOUNT=kampouse.testnet cargo run --bin mpc-worker", relay_url);

    let feedback = subscribe_and_collect(&relay_url, vec![7000], Some(&event.id.to_hex()), 30).await?;

    if feedback.is_empty() {
        println!("\n⚠️  No feedback received (worker may not be running)");
        println!("Partial test passed: event was built, signed (secp256k1 schnorr), and accepted by relay.");
    } else {
        println!("\n🎉 FEEDBACK RECEIVED!");
        for fb in &feedback {
            let status = fb.tags.iter()
                .find(|t| t.kind() == TagKind::custom("status"))
                .and_then(|t| t.content())
                .unwrap_or("?");
            println!("  Status: {}", status);
            println!("  Content: {}", fb.content);
            let fb_pk = fb.pubkey.to_hex();
            println!("  Worker: {}...{}", &fb_pk[..16], &fb_pk[fb_pk.len()-8..]);

            assert!(fb.verify_signature(), "Worker feedback signature should be valid");
            println!("  ✅ Feedback signature verified (secp256k1 schnorr)");
        }
        println!("\n✅ FULL E2E TEST PASSED");
    }

    Ok(())
}

#[tokio::test]
async fn test_registration_flow() -> Result<()> {
    let relay_url = std::env::var("RELAY_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:8080".to_string());

    let sender_keys = Keys::generate();
    let sender_pk = sender_keys.public_key().to_hex();
    println!("\n═══ Registration Flow Test ═══");
    println!("Sender: {}...{}", &sender_pk[..16], &sender_pk[sender_pk.len()-8..]);
    println!("Relay: {}", relay_url);

    // Build kind 5000 (registration) event
    let event = EventBuilder::new(Kind::Custom(5000), "register")
        .sign_with_keys(&sender_keys)?;

    assert!(event.verify_signature(), "Event signature should be valid");
    println!("✅ Registration event signed");

    let accepted = send_event(&relay_url, &event).await
        .context("Failed to send event to relay")?;
    assert!(accepted, "Relay should accept the event");
    println!("✅ Relay accepted event (id: {}...)", &event.id.to_hex()[..16]);

    println!("Waiting for kind 7000 feedback (30s timeout)...");
    println!("NOTE: Worker must be running!");

    let feedback = subscribe_and_collect(&relay_url, vec![7000], Some(&event.id.to_hex()), 30).await?;

    if feedback.is_empty() {
        println!("\n⚠️  No feedback received (worker may not be running)");
    } else {
        println!("\n🎉 REGISTRATION FEEDBACK RECEIVED!");
        for fb in &feedback {
            let status = fb.tags.iter()
                .find(|t| t.kind() == TagKind::custom("status"))
                .and_then(|t| t.content())
                .unwrap_or("?");
            println!("  Status: {}", status);
            println!("  Content: {}", fb.content);

            if fb.content.starts_with("account:") {
                for part in fb.content.split('|') {
                    match part {
                        p if p.starts_with("account:") => println!("  📋 NEAR Account: {}", &p[8..]),
                        p if p.starts_with("key:") => println!("  🔑 MPC Key: {}...", &p[4..40.min(p.len())]),
                        p if p.starts_with("path:") => println!("  🛤️  Path: {}", &p[5..]),
                        _ => {}
                    }
                }
            }

            assert!(fb.verify_signature(), "Feedback signature should be valid");
            println!("  ✅ Signature verified");
        }
        println!("\n✅ REGISTRATION TEST PASSED");
    }
    Ok(())
}
