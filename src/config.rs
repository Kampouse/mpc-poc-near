use anyhow::{Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use near_api::AccountId;

pub struct Config {
    pub nostr_sk: SigningKey,
    pub nostr_pk: VerifyingKey,
    pub npub: String,
    pub near_account: AccountId,
    pub mpc_path: String,
    pub network: near_api::NetworkConfig,
    pub sponsor_key: Option<String>,
    pub sponsor_account: Option<AccountId>,
    pub funder_key: Option<String>,
    pub funder_account: Option<AccountId>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let nostr_sk_hex = env("NOSTR_SK")?;
        let sk_bytes: [u8; 32] = hex::decode(&nostr_sk_hex)
            .context("NOSTR_SK must be 64 hex chars")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("NOSTR_SK must be exactly 32 bytes"))?;

        let nostr_sk = SigningKey::from_bytes(&sk_bytes);
        let nostr_pk = nostr_sk.verifying_key();
        let npub = hex::encode(nostr_pk.as_bytes());
        let near_account: AccountId = env("NEAR_ACCOUNT")?.parse()
            .context("NEAR_ACCOUNT is not a valid account ID")?;
        let mpc_path = format!("nostr:{}", npub);

        let network = near_api::NetworkConfig::from_rpc_url(
            "testnet",
            "https://rpc.testnet.near.org".parse()?,
        );

        Ok(Self {
            nostr_sk,
            nostr_pk,
            npub,
            near_account,
            mpc_path,
            network,
            sponsor_key: std::env::var("SPONSOR_KEY").ok(),
            sponsor_account: std::env::var("SPONSOR_ACCOUNT").ok()
                .map(|s| s.parse()).transpose()
                .context("SPONSOR_ACCOUNT is not a valid account ID")?,
            funder_key: std::env::var("PRIVATE_KEY").ok(),
            funder_account: std::env::var("ACCOUNT_ID").ok()
                .map(|s| s.parse()).transpose()
                .context("ACCOUNT_ID is not a valid account ID")?,
        })
    }

    pub fn require_sponsor(&self) -> Result<(&AccountId, &str)> {
        let account = self.sponsor_account.as_ref()
            .context("Set SPONSOR_ACCOUNT (NEAR account that pays for MPC gas)")?;
        let key = self.sponsor_key.as_deref()
            .context("Set SPONSOR_KEY (ed25519:xxx key for sponsor account)")?;
        Ok((account, key))
    }

    pub fn require_funder(&self) -> Result<(&AccountId, &str)> {
        let account = self.funder_account.as_ref()
            .context("Set ACCOUNT_ID (funder NEAR account)")?;
        let key = self.funder_key.as_deref()
            .context("Set PRIVATE_KEY (ed25519:xxx key for funder account)")?;
        Ok((account, key))
    }
}

fn env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("Set {}", name))
}
