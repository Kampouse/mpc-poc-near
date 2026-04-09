use anyhow::{Context, Result};
use near_api::AccountId;
use nostr::key::Keys;

pub struct Config {
    pub keys: Keys,
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
        let nsec_hex = env("WORKER_NSEC").or_else(|_| env("NOSTR_SK"))?;
        let sk_bytes: [u8; 32] = hex::decode(&nsec_hex)
            .context("Key must be 64 hex chars")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Key must be exactly 32 bytes"))?;
        let keys = Keys::new(nostr::SecretKey::from_slice(&sk_bytes)?);
        let npub = keys.public_key().to_hex();
        let near_account: AccountId = env("NEAR_ACCOUNT")?.parse()
            .context("NEAR_ACCOUNT is not a valid account ID")?;
        let mpc_path = format!("nostr:{}", npub);

        let network = near_api::NetworkConfig::from_rpc_url(
            "testnet",
            "https://rpc.testnet.near.org".parse()?,
        );

        Ok(Self {
            keys,
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
