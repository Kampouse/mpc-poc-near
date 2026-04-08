use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Payment layer trait — can be backed by LND, CLN, NIP-47 (NWC), etc.
#[async_trait::async_trait]
pub trait PaymentProvider: Send + Sync {
    /// Create a Lightning invoice for the given amount (sats) and description.
    async fn create_invoice(&self, amount_sats: u64, description: &str) -> Result<Invoice>;

    /// Check if an invoice has been paid.
    async fn check_payment(&self, payment_hash: &str) -> Result<PaymentStatus>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invoice {
    pub bolt11: String,
    pub payment_hash: String,
    pub amount_sats: u64,
    pub description: String,
    pub expires_at: u64, // unix timestamp
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaymentStatus {
    Pending,
    Paid,
    Expired,
}

/// Pricing for NEAR operations (in sats)
pub struct Pricing {
    pub registration_sats: u64,
    pub transfer_base_sats: u64,
    pub transfer_per_near_sats: u64, // per 1 NEAR transferred
    pub ft_transfer_sats: u64,
}

impl Default for Pricing {
    fn default() -> Self {
        Self {
            registration_sats: 1000,     // ~$0.01 at 1M sats/BTC
            transfer_base_sats: 500,     // base fee
            transfer_per_near_sats: 100, // per NEAR
            ft_transfer_sats: 500,       // flat FT fee
        }
    }
}

impl Pricing {
    pub fn price_registration(&self) -> u64 {
        self.registration_sats
    }

    pub fn price_transfer(&self, amount_near: f64) -> u64 {
        self.transfer_base_sats + (amount_near * self.transfer_per_near_sats as f64) as u64
    }

    pub fn price_ft_transfer(&self) -> u64 {
        self.ft_transfer_sats
    }
}

// ── Mock provider (for testing without a Lightning node) ────────────────────

pub struct MockPaymentProvider {
    paid_invoices: std::sync::Mutex<std::collections::HashSet<String>>,
    auto_approve: bool,
}

impl Default for MockPaymentProvider {
    fn default() -> Self { Self::new() }
}

impl MockPaymentProvider {
    pub fn new() -> Self {
        Self {
            paid_invoices: std::sync::Mutex::new(std::collections::HashSet::new()),
            auto_approve: false,
        }
    }

    /// Auto-approves all payments (for testing / free-tier mode)
    pub fn auto_approving() -> Self {
        Self {
            paid_invoices: std::sync::Mutex::new(std::collections::HashSet::new()),
            auto_approve: true,
        }
    }

    /// Mark an invoice as paid (simulates user paying)
    pub fn mark_paid(&self, payment_hash: &str) {
        self.paid_invoices.lock().unwrap().insert(payment_hash.to_string());
    }
}

#[async_trait::async_trait]
impl PaymentProvider for MockPaymentProvider {
    async fn create_invoice(&self, amount_sats: u64, description: &str) -> Result<Invoice> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let payment_hash = format!("{:064x}", {
            use sha2::Digest;
            sha2::Sha256::digest(format!("mock-{}-{}", amount_sats, now))
        });

        Ok(Invoice {
            bolt11: format!("lnbc{}n1mock{}", amount_sats, &payment_hash[..20]),
            payment_hash,
            amount_sats,
            description: description.to_string(),
            expires_at: now + 600, // 10 minutes
        })
    }

    async fn check_payment(&self, payment_hash: &str) -> Result<PaymentStatus> {
        if self.auto_approve {
            return Ok(PaymentStatus::Paid);
        }
        let paid = self.paid_invoices.lock().unwrap().contains(payment_hash);
        Ok(if paid { PaymentStatus::Paid } else { PaymentStatus::Pending })
    }
}

/// Free payment provider — no payment required, everything auto-approved.
/// Used for development or when the sponsor covers all costs.
pub struct FreePaymentProvider;

impl FreePaymentProvider {
    pub fn new() -> Self { Self }
}

impl Default for FreePaymentProvider {
    fn default() -> Self { Self::new() }
}

#[async_trait::async_trait]
impl PaymentProvider for FreePaymentProvider {
    async fn create_invoice(&self, _amount_sats: u64, _description: &str) -> Result<Invoice> {
        // Returns a dummy invoice — will be auto-approved
        Ok(Invoice {
            bolt11: "free".to_string(),
            payment_hash: "free".to_string(),
            amount_sats: 0,
            description: "free".to_string(),
            expires_at: u64::MAX,
        })
    }

    async fn check_payment(&self, _payment_hash: &str) -> Result<PaymentStatus> {
        Ok(PaymentStatus::Paid)
    }
}

// ── NIP-47 (Nostr Wallet Connect) provider ─────────────────────────────────
// Uses the nostr crate's built-in NIP-47 support to connect to wallets like Alby.
// Configure via NWC_URL env var: "nostr+walletconnect://..."

pub struct NwcPaymentProvider {
    nwc_secret_key: nostr::SecretKey,
    wallet_pubkey: nostr::key::PublicKey,
    relay_url: String,
}

impl NwcPaymentProvider {
    pub fn from_env() -> Result<Self> {
        let nwc_url = std::env::var("NWC_URL").context("Set NWC_URL (nostr+walletconnect://... URI)")?;
        Self::from_url(&nwc_url)
    }

    pub fn from_url(nwc_url: &str) -> Result<Self> {
        // Parse: nostr+walletconnect://<pubkey>?relay=<url>&secret=<hex>
        let url: url::Url = nwc_url.parse().context("Invalid NWC URL")?;
        let pubkey_hex = url.host_str().context("Missing pubkey in NWC URL")?;
        let wallet_pubkey = nostr::key::PublicKey::from_hex(pubkey_hex)?;
        let relay_url = url.query_pairs()
            .find(|(k, _)| k == "relay")
            .map(|(_, v)| v.to_string())
            .context("Missing relay in NWC URL")?;
        let secret_hex = url.query_pairs()
            .find(|(k, _)| k == "secret")
            .map(|(_, v)| v.to_string())
            .context("Missing secret in NWC URL")?;
        let nwc_secret_key = nostr::SecretKey::from_slice(&hex::decode(&secret_hex)?)?;

        Ok(Self { nwc_secret_key, wallet_pubkey, relay_url })
    }
}

#[async_trait::async_trait]
impl PaymentProvider for NwcPaymentProvider {
    async fn create_invoice(&self, amount_sats: u64, description: &str) -> Result<Invoice> {
        use futures_util::{SinkExt, StreamExt};
        use nostr::{EventBuilder, JsonUtil, Kind, Tag, TagKind};
        use tokio_tungstenite::connect_async;

        let keys = nostr::key::Keys::new(self.nwc_secret_key.clone());

        // NIP-47: kind 23194 = NWC request
        // Method: make_invoice
        let request = serde_json::json!({
            "method": "make_invoice",
            "params": {
                "amount": amount_sats * 1000, // msats
                "description": description,
            }
        });

        let tags = vec![
            Tag::custom(TagKind::p(), [self.wallet_pubkey.to_hex()]),
        ];

        let event = EventBuilder::new(Kind::Custom(23194), request.to_string())
            .tags(tags)
            .sign_with_keys(&keys)?;

        let (mut ws, _) = connect_async(&self.relay_url).await?;
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!(["EVENT", event]).to_string().into()
        )).await?;

        // Subscribe for response (kind 23195)
        let sub_filter = serde_json::json!({"kinds": [23195], "#p": [keys.public_key().to_hex()], "limit": 1});
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!(["REQ", "nwc-1", sub_filter]).to_string().into()
        )).await?;

        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < 30 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(msg))) => {
                    if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                        let parsed: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                        if parsed.first().and_then(|v| v.as_str()) == Some("EVENT") && parsed.len() >= 3 {
                            let resp_event: nostr::Event = nostr::Event::from_json(parsed[2].to_string())?;
                            let resp: serde_json::Value = serde_json::from_str(&resp_event.content)?;
                            if let Some(result) = resp.get("result") {
                                let invoice = result.get("invoice").and_then(|v| v.as_str()).context("No invoice in NWC response")?;
                                let payment_hash = result.get("payment_hash").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                return Ok(Invoice {
                                    bolt11: invoice.to_string(),
                                    payment_hash,
                                    amount_sats,
                                    description: description.to_string(),
                                    expires_at: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)?.as_secs() + 600,
                                });
                            }
                        }
                    }
                }
                _ => continue,
            }
        }
        anyhow::bail!("NWC: timeout waiting for invoice response")
    }

    async fn check_payment(&self, payment_hash: &str) -> Result<PaymentStatus> {
        use futures_util::{SinkExt, StreamExt};
        use nostr::{EventBuilder, JsonUtil, Kind, Tag, TagKind};
        use tokio_tungstenite::connect_async;

        let keys = nostr::key::Keys::new(self.nwc_secret_key.clone());

        let request = serde_json::json!({
            "method": "lookup_invoice",
            "params": {
                "payment_hash": payment_hash,
            }
        });

        let tags = vec![
            Tag::custom(TagKind::p(), [self.wallet_pubkey.to_hex()]),
        ];

        let event = EventBuilder::new(Kind::Custom(23194), request.to_string())
            .tags(tags)
            .sign_with_keys(&keys)?;

        let (mut ws, _) = connect_async(&self.relay_url).await?;
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!(["EVENT", event]).to_string().into()
        )).await?;

        let sub_filter = serde_json::json!({"kinds": [23195], "#p": [keys.public_key().to_hex()], "limit": 1});
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!(["REQ", "nwc-2", sub_filter]).to_string().into()
        )).await?;

        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < 10 {
            match tokio::time::timeout(tokio::time::Duration::from_secs(2), ws.next()).await {
                Ok(Some(Ok(msg))) => {
                    if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                        let parsed: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                        if parsed.first().and_then(|v| v.as_str()) == Some("EVENT") && parsed.len() >= 3 {
                            let resp_event: nostr::Event = nostr::Event::from_json(parsed[2].to_string())?;
                            let resp: serde_json::Value = serde_json::from_str(&resp_event.content)?;
                            if let Some(result) = resp.get("result") {
                                let settled = result.get("settled_at")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let expires = result.get("expires_at")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)?.as_secs();

                                return Ok(if settled > 0 {
                                    PaymentStatus::Paid
                                } else if expires > 0 && now > expires {
                                    PaymentStatus::Expired
                                } else {
                                    PaymentStatus::Pending
                                });
                            }
                        }
                    }
                }
                _ => continue,
            }
        }
        Ok(PaymentStatus::Pending)
    }
}
