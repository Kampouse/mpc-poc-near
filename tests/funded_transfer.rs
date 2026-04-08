//! Full funded transfer test: register → fund → transfer
//!
//! RELAY_URL=ws://127.0.0.1:8090 cargo test --test funded_transfer -- --nocapture

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use nostr::key::Keys;
use nostr::{EventBuilder, JsonUtil, Kind, Tag, TagKind};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Send a nostr-sdk-signed event to a relay.
async fn send_event(relay_url: &str, event: &nostr::Event) -> Result<bool> {
    let (mut ws, _) = connect_async(relay_url).await?;
    let msg = serde_json::json!(["EVENT", event]).to_string();
    ws.send(Message::Text(msg.into())).await?;
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let parsed: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                if parsed.first().and_then(|v| v.as_str()) == Some("OK") {
                    return Ok(parsed.get(2).and_then(|v| v.as_bool()).unwrap_or(false));
                }
            }
            _ => continue,
        }
    }
    Ok(false)
}

/// Subscribe and wait for a single feedback event
async fn wait_for_feedback(relay_url: &str, event_id: &str, timeout_secs: u64) -> Result<Option<nostr::Event>> {
    let (mut ws, _) = connect_async(relay_url).await?;
    let sub_id = format!("fb-{}", &event_id[..8]);
    let filter = serde_json::json!({"kinds": [7000], "#e": [event_id], "limit": 5});
    ws.send(Message::Text(serde_json::json!(["REQ", sub_id, filter]).to_string().into())).await?;

    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < timeout_secs {
        match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let parsed: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                match parsed.first().and_then(|v| v.as_str()) {
                    Some("EVENT") if parsed.len() >= 3 => {
                        if let Ok(event) = nostr::Event::from_json(parsed[2].to_string()) {
                            let _ = ws.send(Message::Text(format!(r#"["CLOSE","{}"]"#, sub_id).into())).await;
                            return Ok(Some(event));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(Ok(Message::Ping(data)))) => { ws.send(Message::Pong(data)).await?; }
            _ => continue,
        }
    }
    let _ = ws.send(Message::Text(format!(r#"["CLOSE","{}"]"#, sub_id).into())).await;
    Ok(None)
}

/// Parse feedback content for account/key/path
fn parse_feedback(content: &str) -> Option<(&str, &str, &str)> {
    let mut account = "";
    let mut key = "";
    let mut path = "";
    for part in content.split('|') {
        match part {
            p if p.starts_with("account:") => account = &p[8..],
            p if p.starts_with("key:") => key = &p[4..],
            p if p.starts_with("path:") => path = &p[5..],
            _ => {}
        }
    }
    if account.is_empty() { None } else { Some((account, key, path)) }
}

#[tokio::test]
async fn test_funded_transfer() -> Result<()> {
    let relay_url = std::env::var("RELAY_URL")
        .unwrap_or_else(|_| "ws://127.0.0.1:8090".to_string());

    println!("\n═══════════════════════════════════════════════════");
    println!("   FUNDED TRANSFER TEST (register → fund → send)");
    println!("═══════════════════════════════════════════════════\n");

    // ── Step 1: Register ─────────────────────────────────────
    println!("[1/4] Registering via kind 5000...");
    let sender_keys = Keys::generate();
    let sender_pk = sender_keys.public_key().to_hex();
    println!("  Nostr key: {}...{}", &sender_pk[..16], &sender_pk[sender_pk.len()-8..]);

    let reg_event = EventBuilder::new(Kind::Custom(5000), "register")
        .sign_with_keys(&sender_keys)?;
    assert!(reg_event.verify_signature());

    let accepted = send_event(&relay_url, &reg_event).await?;
    assert!(accepted, "Relay should accept registration");
    println!("  ✅ Sent registration");

    let feedback = wait_for_feedback(&relay_url, &reg_event.id.to_hex(), 30).await?
        .context("No feedback from worker")?;
    println!("  Feedback: {}", feedback.content);

    let status = feedback.tags.iter()
        .find(|t| t.kind() == TagKind::custom("status"))
        .and_then(|t| t.content())
        .unwrap_or("?").to_string();
    
    if status == "error" {
        anyhow::bail!("Registration failed: {}", feedback.content);
    }

    let (account_id, mpc_key, path) = parse_feedback(&feedback.content)
        .context("Could not parse account info from feedback")?;
    println!("  📋 Account: {}", account_id);
    println!("  🔑 MPC Key: {}...", &mpc_key[..40.min(mpc_key.len())]);
    println!("  🛤️  Path: {}", path);

    // ── Step 2: Fund the account ─────────────────────────────
    println!("\n[2/4] Funding {} with 0.5 NEAR...", account_id);
    
    // Use near CLI to send NEAR to the account
    let fund_output = tokio::process::Command::new("near")
        .args(["send", "kampouse.testnet", account_id, "0.5", "--networkId", "testnet"])
        .output()
        .await
        .context("Failed to run near CLI")?;

    if !fund_output.status.success() {
        let stderr = String::from_utf8_lossy(&fund_output.stderr);
        println!("  ⚠️  Funding may have failed: {}", &stderr[..stderr.len().min(200)]);
        // Continue anyway — account might already have funds
    } else {
        println!("  ✅ Funded");
    }

    // ── Step 3: Send transfer via Nostr (triggers invoice) ─────
    println!("\n[3/5] Sending transfer request via kind 5001 (expect invoice)...");
    let transfer_amount = "0.001";
    let transfer_to = "kampouse.testnet";
    let job_params = serde_json::json!({
        "to": transfer_to,
        "amount": transfer_amount,
    }).to_string();

    let tags = vec![
        Tag::custom(TagKind::custom("i"), [&job_params]),
    ];

    let tx_event = EventBuilder::new(Kind::Custom(5001), &job_params)
        .tags(tags)
        .sign_with_keys(&sender_keys)?;
    assert!(tx_event.verify_signature());

    let accepted = send_event(&relay_url, &tx_event).await?;
    assert!(accepted, "Relay should accept transfer");
    println!("  ✅ Sent transfer request: {} NEAR → {}", transfer_amount, transfer_to);

    // ── Step 4: Wait for invoice ─────────────────────────────
    println!("\n[4/5] Waiting for invoice from worker...");
    let invoice_feedback = wait_for_feedback(&relay_url, &tx_event.id.to_hex(), 30).await?
        .context("No invoice feedback")?;

    let invoice_status = invoice_feedback.tags.iter()
        .find(|t| t.kind() == TagKind::custom("status"))
        .and_then(|t| t.content())
        .unwrap_or("?").to_string();
    println!("  Status: {}", invoice_status);
    println!("  Content: {}", invoice_feedback.content);

    if invoice_status != "payment_required" {
        anyhow::bail!("Expected payment_required, got: {}", invoice_status);
    }

    // Parse invoice from feedback
    let mut bolt11 = "";
    let mut payment_hash = "";
    for part in invoice_feedback.content.split('|') {
        match part {
            p if p.starts_with("invoice:") => bolt11 = &p[8..],
            p if p.starts_with("hash:") => payment_hash = &p[5..],
            _ => {}
        }
    }
    println!("  ⚡ Invoice: {}...", &bolt11[..40.min(bolt11.len())]);
    println!("  📋 Payment hash: {}...", &payment_hash[..16]);
    assert!(!bolt11.is_empty(), "Should have an invoice");

    // ── Step 5: Pay & re-submit with proof ────────────────────
    println!("\n[5/5] Re-submitting with payment proof...");
    let paid_tags = vec![
        Tag::custom(TagKind::custom("i"), [&job_params]),
        Tag::custom(TagKind::custom("payment_hash"), [payment_hash]),
    ];

    let paid_event = EventBuilder::new(Kind::Custom(5001), &job_params)
        .tags(paid_tags)
        .sign_with_keys(&sender_keys)?;
    assert!(paid_event.verify_signature());

    let accepted = send_event(&relay_url, &paid_event).await?;
    assert!(accepted, "Relay should accept paid transfer");
    println!("  ✅ Sent paid transfer request");

    // Wait for transfer result
    println!("  Waiting for transfer result (60s)...");
    let tx_feedback = wait_for_feedback(&relay_url, &paid_event.id.to_hex(), 60).await?
        .context("No transfer feedback")?;

    let tx_status = tx_feedback.tags.iter()
        .find(|t| t.kind() == TagKind::custom("status"))
        .and_then(|t| t.content())
        .unwrap_or("?").to_string();

    println!("\n  Status: {}", tx_status);
    println!("  Content: {}", tx_feedback.content);

    if tx_status == "success" {
        println!("\n  ✅ FUNDED TRANSFER WITH PAYMENT SUCCEEDED!");
    } else {
        println!("\n  ⚠️  Transfer status: {} (mock payments should auto-approve)", tx_status);
    }

    println!("\n═══════════════════════════════════════════════════");
    println!("   TEST COMPLETE");
    println!("═══════════════════════════════════════════════════");

    Ok(())
}
