# GhostGuard

Rust crate + CLI tool that detects **ghost fills** on Polymarket's CLOB.

A ghost fill occurs when the CLOB reports an order as filled, but the on-chain settlement transaction reverts. This costs market makers real money. GhostGuard monitors fills and verifies each one on-chain, catching ghosts in real time.

## How it works

```
CLOB fill event ──> extract tx_hash ──> poll eth_getTransactionReceipt
                                              │
                                    ┌─────────┼──────────┐
                                    │         │          │
                                status=1   status=0   no receipt
                                  REAL      GHOST      TIMEOUT
                                            │          (treat as ghost)
                                            │
                                    parse revert reason
                                    (TRANSFER_FROM_FAILED, etc.)
```

Three known attack variants all produce the same on-chain result (status=0):

1. **incrementNonce()** -- invalidates order signature before settlement
2. **Wallet drain** -- transfers USDC out before settlement, tx reverts with `TRANSFER_FROM_FAILED`
3. **Gas front-running** -- outbids the settlement tx

GhostGuard detects the **result**, not the method.

## Installation

### One-liner (recommended)

```bash
curl -L https://raw.githubusercontent.com/orderbooktrade/GhostGuard/main/ghostguardup | bash
```

This downloads the prebuilt binary for your platform to `~/.ghostguard/bin` and adds it to your PATH. Works on macOS (Intel/Apple Silicon) and Linux (x86_64/ARM64).

### From source (cargo)

```bash
cargo install --git https://github.com/OrderBookTrade/GhostGuard
```

### From source (clone + make)

```bash
git clone https://github.com/OrderBookTrade/GhostGuard
cd GhostGuard
make install
```

### As a Rust dependency

```toml
[dependencies]
ghostguard = { git = "https://github.com/OrderBookTrade/GhostGuard" }
```

### Verify installation

```bash
ghostguard --version
```

## Quick start

### Verify a single transaction

```bash
ghostguard \
  --verify-tx 0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0
```

This is a known ghost fill (wallet drain, `TRANSFER_FROM_FAILED`). Output:

```
[GHOST] tx=0x9e32...2df0 reason=TRANSFER_FROM_FAILED
        counterparty=0x4bfb...982e
```

### Run as sidecar

```bash
ghostguard \
  --rpc https://polygon-rpc.com \
  --clob-ws wss://ws-subscriptions-clob.polymarket.com/ws/market \
  --webhook http://localhost:8080/fill-alert
```

Listens to the CLOB websocket for fills, verifies each on-chain, and:
- Logs to stdout: `[REAL]` or `[GHOST]` with tx details
- POSTs JSON to your webhook URL on every verdict

### Use as a library

```rust
use ghostguard::{GhostGuard, Config};

let config = Config {
    rpc_url: "https://polygon-rpc.com".into(),
    clob_ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
    ..Default::default()
};

let mut guard = GhostGuard::new(config);

guard.on_real_fill(|verdict| {
    // safe to update position state
});

guard.on_ghost_fill(|event| {
    eprintln!("GHOST: {} -- {}", event.tx_hash, event.reason);
    // trigger recovery: cancel open orders, alert ops, etc.
});

guard.start().await?;
```

You can also verify a single tx directly:

```rust
use ghostguard::{verify_fill, Config, FillVerdict};

let verdict = verify_fill(&config.rpc_url, tx_hash, &config).await?;
if verdict.is_ghost() {
    // handle ghost fill
}
```

## CLI options

```
ghostguard [OPTIONS]

Options:
    --rpc <URL>              Polygon JSON-RPC endpoint [default: https://polygon-rpc.com]
    --clob-ws <URL>          CLOB WebSocket URL [default: wss://ws-subscriptions-clob...]
    --webhook <URL>          Webhook URL to POST verdicts to
    --timeout <SECS>         Verification timeout in seconds [default: 10]
    --poll-ms <MS>           Receipt poll interval in milliseconds [default: 500]
    --verify-tx <HASH>       Verify a single tx hash and exit
    -h, --help               Print help
    -V, --version            Print version
```

## Project structure

```
src/
  lib.rs          Public API -- GhostGuard struct, re-exports
  types.rs        Config, FillVerdict, GhostFillEvent, contract addresses
  verifier.rs     Core logic -- polls receipt, parses revert, returns verdict
  ws.rs           CLOB websocket listener, parses fill messages
  callback.rs     Webhook POST dispatcher
  bin/cli.rs      CLI sidecar binary
examples/
  basic.rs        Minimal usage with known ghost tx
```

## Core types

```rust
enum FillVerdict {
    Real { tx_hash: H256, block: u64 },
    Ghost { tx_hash: H256, reason: String, counterparty: Option<Address> },
    Timeout { tx_hash: H256 },
}

struct GhostFillEvent {
    tx_hash: H256,
    market: String,
    side: String,
    size: f64,
    price: f64,
    counterparty: Option<Address>,
    reason: String,
    timestamp: u64,
}
```

## Webhook payload

On each fill, GhostGuard POSTs JSON to your webhook:

```json
{
  "verdict": "Ghost",
  "tx_hash": "0x9e3230ab...",
  "reason": "TRANSFER_FROM_FAILED",
  "counterparty": "0x4bfb41d5..."
}
```

## Contract addresses

| Contract             | Address                                      |
|----------------------|----------------------------------------------|
| CTF Exchange         | `0x4bfb41d5b3570defd03c39a9a4d8de6bd8b8982e` |
| NegRisk CTF Exchange | `0xC5d563A36AE78145C45a50134d48A1215220f80a` |
| USDC.e (Polygon)     | `0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174` |

## Testing

```bash
# Unit tests
cargo test

# Integration test against live Polygon RPC
cargo test -- --ignored test_known_ghost_fill

# Run the example
cargo run --example basic
```

## License

MIT
