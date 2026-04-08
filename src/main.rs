use anyhow::Result;
use clap::Parser;
use ed25519_dalek::Signer;

mod config;
mod ft;
mod mpc;
mod near;

#[derive(Parser)]
#[command(name = "mpc-poc-near", about = "Nostr → MPC → NEAR account control")]
enum Cmd {
    /// Create a NEAR account with MPC-derived key
    Create,
    /// Show account info and MPC derivation
    Info,
    /// Show NEAR balance + common FT balances
    Balances,
    /// Check a specific FT token balance
    Balance {
        /// Token contract account ID
        contract_id: String,
    },
    /// Send NEAR or any FT token via MPC signing
    Transfer {
        /// Recipient account ID
        to: String,
        /// Amount to send (human-readable, e.g. 1.5)
        amount: String,
        /// Token contract ID (omit for NEAR)
        #[arg(last = true)]
        token: Option<String>,
    },
    /// Test Nostr key signature
    SignTest,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cmd = Cmd::parse();
    let cfg = config::Config::from_env()?;

    match cmd {
        Cmd::Create => near::create_account(&cfg).await,
        Cmd::Info => near::show_info(&cfg).await,
        Cmd::Balances => near::show_balances(&cfg).await,
        Cmd::Balance { contract_id } => ft::show_balance(&cfg, &contract_id).await,
        Cmd::Transfer { to, amount, token } => near::transfer(&cfg, &to, &amount, token.as_deref()).await,
        Cmd::SignTest => {
            let sig = cfg.nostr_sk.sign(b"nostr-mpc recovery test");
            use ed25519_dalek::Verifier;
            let verifiable = ed25519_dalek::Signature::from_bytes(&sig.to_bytes());
            let valid = cfg.nostr_pk.verify(b"nostr-mpc recovery test", &verifiable).is_ok();
            println!("{} — {}", hex::encode(sig.to_bytes()), if valid { "✅" } else { "❌" });
            Ok(())
        }
    }
}
