# mpc-poc-near

Nostr → NEAR via Chain Signatures. Send NEAR transactions using only a Nostr identity — no NEAR private key required.

## How it works

```
Nostr identity (secp256k1) ──→ MPC derivation path ──→ NEAR account (ed25519)
         │                                              │
    signs Nostr event                           signs NEAR tx via MPC
         │                                              │
         └───── Worker (relay) ───── Lightning payment ─┘
```

1. **Register** — Send a Nostr event (kind 5000) to register your Nostr identity as a NEAR account
2. **Pay** — Worker responds with a Lightning invoice (BTC)
3. **Transfer** — Pay the invoice, re-submit (kind 5001), worker signs via MPC and broadcasts

The MPC network (8 nodes, threshold signature) derives a unique ed25519 key from `nostr:<npub>`. No single node has the full key. No NEAR private key ever exists anywhere.

## Architecture

| Component | Role |
|-----------|------|
| **Nostr** | Identity layer — secp256k1 schnorr signatures prove who you are |
| **MPC** (v1.signer-prod.testnet) | Signing layer — threshold signature over NEAR tx |
| **Worker** | Relay — subscribes to Nostr, orchestrates MPC + payments + broadcast |
| **Lightning** | Payment layer — BTC Lightning covers NEAR gas costs |

## Flow

### Registration (kind 5000)
```
User ──[kind 5000, account:my-name.testnet]──→ Relay → Worker
Worker: verify sig → derive MPC key → create NEAR account → feedback
```

### Transfer (kind 5001)
```
User ──[kind 5001, {to, amount}]──→ Relay → Worker
Worker: verify sig → create Lightning invoice → [payment_required]
User ──[kind 5001, {to, amount}, payment_hash]──→ Relay → Worker  
Worker: verify payment → MPC sign → broadcast → [success, tx: ...]
```

## Setup

### Prerequisites
- Rust 1.75+
- NEAR testnet account with funds (sponsor)
- A Nostr relay (local or public)

### Build
```bash
git clone https://github.com/Kampouse/mpc-poc-near.git
cd mpc-poc-near
cargo build --release
```

### Run Worker

```bash
# Required
export RELAY_URL=wss://relay.damus.io          # or your own relay
export WORKER_NSEC=<hex>                        # worker's Nostr secret key
export SPONSOR_KEY=ed25519:xxx                  # NEAR sponsor private key
export SPONSOR_ACCOUNT=kampouse.testnet         # NEAR sponsor account

# Payment (pick one)
export NWC_URL=nostr+walletconnect://...        # Real Lightning via NIP-47
# export NO_PAYMENT=1                           # Skip payments (dev mode)
# (default: mock — auto-approves all payments)

# Start
cargo run --bin mpc-worker

# Or as daemon
cargo run --bin mpc-worker -- --daemon
cargo run --bin mpc-worker -- --status
cargo run --bin mpc-worker -- --stop
```

### Run Tests

```bash
# Start a local relay (uses nostr-rs-relay)
# Then:
RELAY_URL=ws://127.0.0.1:8090 cargo test --test e2e -- --nocapture
RELAY_URL=ws://127.0.0.1:8090 cargo test --test funded_transfer -- --nocapture
```

## Event Types

| Kind | Direction | Purpose |
|------|-----------|---------|
| 5000 | User → Worker | Register a NEAR account |
| 5001 | User → Worker | Transfer request |
| 7000 | Worker → User | Feedback (status, invoice, tx hash) |

### Kind 5000 — Registration
```json
{
  "kind": 5000,
  "content": "account:my-name.testnet",
  "tags": []
}
```
Or use a tag: `["n", "my-name.testnet"]`

If no name specified, derives: `n<4hex>-<8hex>.testnet`

### Kind 5001 — Transfer
```json
{
  "kind": 5001,
  "content": "{\"to\":\"bob.testnet\",\"amount\":\"0.001\"}",
  "tags": [["i", "{\"to\":\"bob.testnet\",\"amount\":\"0.001\"}"]]
}
```

With payment proof (after receiving invoice):
```json
{
  "kind": 5001,
  "content": "{\"to\":\"bob.testnet\",\"amount\":\"0.001\"}",
  "tags": [
    ["i", "{\"to\":\"bob.testnet\",\"amount\":\"0.001\"}"],
    ["payment_hash", "abc123..."]
  ]
}
```

For FT transfers, add `"token": "contract.near"`.

### Kind 7000 — Feedback
```json
{
  "kind": 7000,
  "tags": [
    ["e", "<original_event_id>"],
    ["p", "<user_pubkey>"],
    ["status", "success"]
  ],
  "content": "Sent 0.001 NEAR to bob.testnet | tx: DP5DN7vkmUgG..."
}
```

Status values: `success`, `error`, `payment_required`

## Payments

| Mode | Config | Use case |
|------|--------|----------|
| NIP-47 (NWC) | `NWC_URL=...` | Production — real Lightning |
| Mock | (default) | Testing — auto-approves |
| Free | `NO_PAYMENT=1` | Dev — sponsor covers all |

### Pricing (default, in sats)
- Registration: 1,000 sats
- NEAR transfer: 500 + 100/NEAR
- FT transfer: 500 sats

## Security

**Threat model:**
- Nostr key = full control over associated NEAR account. Protect it.
- Worker is trusted — it sees all requests and could censor.
- MPC threshold (8 nodes) — compromising ≥threshold nodes exposes all derived keys.
- Dedup prevents replay attacks.
- Input validated: amounts must be positive, recipients must be valid account IDs.

**Not yet implemented:**
- Rate limiting
- Account ownership verification (anyone could pre-create `nXXX-YYY.testnet` with their own key)
- Multi-relay failover
- Event pruning (processed set grows unbounded)

## Verified On-Chain

TX [`6fbuwBRrtKmW5KXAaCkkkKNCB6TwgjCxMZmgKxfzyU81`](https://explorer.testnet.near.org/transactions/6fbuwBRrtKmW5KXAaCkkkKNCB6TwgjCxMZmgKxfzyU81):
- `ncb41-ed482d9b.testnet` → `kampouse.testnet`
- 0.001 NEAR
- Signed by MPC-derived key from Nostr identity
- No NEAR private key existed anywhere

## License

MIT
