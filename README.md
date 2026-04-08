# mpc-poc-near

Proof of concept: **Nostr key controls a NEAR account via MPC chain-signatures.**

No wallet. No seed phrase. No worker. Just a Nostr keypair.

## How It Works

```
Nostr npub (ed25519) → MPC derivation path → deterministic NEAR public key
                                              → NEAR account (non-custodial)
```

1. User has a Nostr keypair (any Nostr client generates this for free)
2. The npub becomes an MPC derivation path: `nostr:<npub>`
3. NEAR's MPC contract (`v1.signer-prod.testnet`) derives a deterministic ed25519 public key
4. That key becomes the full access key for a NEAR account
5. **Nobody has the private key** — only the MPC network can sign
6. User authorizes transactions by signing Nostr events with their Nostr key
7. Any sponsor (worker, CLI, contract) calls `MPC.sign(path, payload)` to get the signature

## Architecture

```
┌─────────────┐     ┌──────────────┐     ┌──────────────────┐
│  Nostr Key   │────→│  MPC Contract │────→│  NEAR Account     │
│  (ed25519)   │     │  (on-chain)   │     │  (non-custodial)  │
└─────────────┘     └──────────────┘     └──────────────────┘
       │                                          │
       │ signs Nostr event (kind 5001)             │
       ↓                                          ↓
┌─────────────┐     ┌──────────────┐     ┌──────────────────┐
│  Nostr Relay │←───→│    Worker     │←───→│  NEAR Blockchain  │
│  (NIP-90)    │     │  (inlayer)    │     │                   │
└─────────────┘     └──────────────┘     └──────────────────┘
       │                    │
       │    OR (recovery)   │
       ↓                    ↓
┌─────────────────────────────────┐
│  This CLI (no worker needed)    │
│  NOSTR_SK + any sponsor key     │
└─────────────────────────────────┘
```

## Quick Start

```bash
# Build
cargo build

# Create a NEAR account from a Nostr key
NOSTR_SK=<hex> \
NEAR_ACCOUNT=my-agent.testnet \
PRIVATE_KEY=ed25519:<sponsor_key> \
ACCOUNT_ID=kampouse.testnet \
cargo run -- create

# Check balances (NEAR + all FTs)
NOSTR_SK=<hex> NEAR_ACCOUNT=my-agent.testnet cargo run -- balances

# Transfer NEAR
NOSTR_SK=<hex> NEAR_ACCOUNT=my-agent.testnet \
SPONSOR_KEY=ed25519:xxx SPONSOR_ACCOUNT=kampouse.testnet \
cargo run -- transfer bob.testnet 0.5

# Transfer USDT
NOSTR_SK=<hex> NEAR_ACCOUNT=my-agent.testnet \
SPONSOR_KEY=ed25519:xxx SPONSOR_ACCOUNT=kampouse.testnet \
cargo run -- transfer bob.testnet 100 USDT

# Account info
NOSTR_SK=<hex> NEAR_ACCOUNT=my-agent.testnet cargo run -- info
```

## Commands

| Command | Description |
|---------|-------------|
| `create` | Create a NEAR account with MPC-derived key |
| `info` | Show account info and MPC derivation |
| `balances` | Show NEAR + all FT balances |
| `transfer <to> <amount> [token]` | Send NEAR or FT via MPC signing |
| `sign-test` | Verify Nostr key works |

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `NOSTR_SK` | Yes | Hex ed25519 secret key (from Nostr client) |
| `NEAR_ACCOUNT` | Yes | NEAR account name |
| `PRIVATE_KEY` | create only | Sponsor's ed25519 key (pays for account creation) |
| `ACCOUNT_ID` | create only | Sponsor's NEAR account |
| `SPONSOR_KEY` | transfer | Sponsor's ed25519 key (pays MPC gas) |
| `SPONSOR_ACCOUNT` | transfer | Sponsor's NEAR account (default: kampouse.testnet) |

## Supported Tokens

| Symbol | Contract (Testnet) |
|--------|-------------------|
| NEAR | native |
| USDT | `usdt.fakes.testnet` |
| USDC | `usdc.fakes.testnet` |
| wNEAR | `wrap.testnet` |
| REF | `token.v2.ref-finance.testnet` |

For mainnet, update the `KNOWN_TOKENS` array in `src/main.rs`.

## Integration with Inlayer (Worker)

The worker (inlayer) does the same thing as this CLI, but automated:

1. **Subscribe to Nostr relay** — watch for kind 5001 (job request) events
2. **Verify Nostr signature** — standard ed25519 verification
3. **Lookup MPC path** — `nostr:<event.pubkey>` → bound NEAR account
4. **Build NEAR transaction** — based on event content
5. **Call MPC.sign()** — sponsor account calls the MPC contract
6. **Broadcast signed tx** — submit to NEAR
7. **Publish result** — kind 6001 event back to Nostr relay

```rust
// Pseudocode for worker integration
async fn handle_job(event: NostrEvent) {
    verify_nostr_sig(&event)?;

    let path = format!("nostr:{}", event.pubkey);
    let near_account = lookup_binding(&event.pubkey)?;

    // Check Lightning payment (NIP-90 / L402)
    verify_payment(&event).await?;

    // Build & sign tx via MPC
    let unsigned_tx = build_transfer_tx(near_account, event.parse_params());
    let tx_hash = sha256(borsh::serialize(&unsigned_tx));

    let signature = mpc_contract.sign(path, tx_hash).await?;

    broadcast_tx(unsigned_tx, signature).await?;
    publish_result(event.pubkey, "success", tx_hash).await?;
}
```

## Security Model

| Property | How |
|----------|-----|
| Non-custodial | MPC holds the private key — nobody else has it |
| Auth | Nostr ed25519 signature = proof of key ownership |
| Recovery | This CLI + Nostr key = full access, no worker needed |
| No vendor lock-in | Any sponsor can call MPC — not tied to one worker |
| On-chain | All signing goes through the MPC contract on NEAR |

## Key Files

```
src/main.rs    — Full CLI: create account, check balances, transfer NEAR/FTs
Cargo.toml     — near-api-rs, ed25519-dalek, borsh, sha2
```

## Stack

- **Nostr** — identity & communication (ed25519 keys)
- **NEAR MPC** — chain-signatures for non-custodial key derivation
- **near-api-rs** — Rust SDK for all NEAR interactions
- **ed25519-dalek** — Nostr key handling

## Related Projects

- [nostr-identity](https://github.com/Kampouse/nostr-identity) — TEE + ZKP identity binding
- [nostr-rs-relay](https://github.com/Kampouse/nostr-rs-relay) — Nostr relay in Rust
- [NEAR Chain Signatures](https://docs.near.org/chain-abstraction/chain-signatures) — MPC docs

## License

MIT
