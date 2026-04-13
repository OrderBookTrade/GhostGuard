# Ghost Fills on Polymarket: How They Work, How They Evolved, and How to Defend Against Them

I've been building trading infrastructure on Polymarket for the past several months. In February 2026 I started losing money on fills that never settled. This article is everything I've learned about ghost fills -- what causes them, how attackers profit from them, why every existing defense fails, and what I built to fix it.

If you run a market-making bot on Polymarket, this is the article I wish existed three months ago.

---

## Part 1: Where Ghost Fills Come From

### The architecture gap

Polymarket runs a hybrid system: off-chain order matching via a Central Limit Order Book (CLOB), with on-chain settlement on Polygon. When you submit an order, the CLOB matches it instantly and sends you a fill notification. Settlement happens later -- a relayer bundles matched orders and submits `matchOrders()` to the CTF Exchange contract on Polygon.

There's a time gap between "CLOB says you're filled" and "chain confirms settlement." That gap is the attack surface.

```
Your bot places order
        |
        v
CLOB matches order (instant)
        |
        v
CLOB sends fill notification via websocket  <-- your bot sees this
        |
        v
  [TIME GAP: seconds to minutes]            <-- attack window
        |
        v
Relayer submits matchOrders() to Polygon
        |
        v
Transaction mines, receipt available
        |
        +---> status=1: fill is real, tokens transferred
        |
        +---> status=0: REVERTED. fill is ghost.
                         your bot thought it was filled.
                         it wasn't.
```

Most bots update their internal state on the CLOB fill notification. They adjust positions, move hedges, update risk limits. If that fill was a ghost, every downstream decision is wrong.

### The timeline

**Late 2025**: Polymarket launches 5-minute BTC/ETH/SOL binary markets. These are rapid-fire prediction markets -- will BTC be above $X in 5 minutes? High frequency, full automation, predictable prices in the final seconds. Perfect conditions for what comes next.

**February 19, 2026**: @itslirrato publishes the first public disclosure. The attack method: call `incrementNonce()` on the CTF Exchange contract (`0x4bfb41d5b3570defd03c39a9a4d8de6bd8b8982e`) after getting matched but before settlement. This invalidates the order's signature, so `matchOrders()` reverts. The attacker bets both sides, cancels the losing side via nonce increment, keeps the winning side. Risk-free profit.

**February 22, 2026**: TheOneWhoBurns releases [nonce-guard](https://github.com/TheOneWhoBurns/polymarket-nonce-guard) -- open-source tooling that monitors `incrementNonce()` calls (method signature `0x627cdcb9`). It gets 38 stars on GitHub. First community defense.

**February 25-26, 2026**: Coverage goes wide. PANews, Binance Square, DeFi Rate all report on it. The numbers: one attacker made $16,427 profit across 7 markets in a single day. Cost per attack: less than $0.10 in Polygon gas.

**March 2026**: The attack evolves. Attackers stop using `incrementNonce()` -- it's detectable and gets you blacklisted. New method: **wallet drain**. Transfer USDC out of the wallet before the relayer submits settlement. `matchOrders()` calls `transferFrom()` on USDC, fails because balance is zero, reverts with `TRANSFER_FROM_FAILED`. Nonce-guard can't see this. No nonce was incremented.

**April 6, 2026**: Polymarket announces CTF Exchange V2. Removes the nonce field entirely. This fixes the `incrementNonce` variant. But V2 does **not** fix wallet drain. Deposit wallets with timelocks exist as an option, but they're not mandatory. Migration is not instant.

**April 12, 2026**: Marm reports 6% of volume reverted. JAMAL says 100% of his hedges are ghost filled. Multiple makers are bleeding. No public tool covers the wallet drain variant.

---

## Part 2: How Attackers Actually Make Money

This is the part that hasn't been written about publicly. The ghost fill itself doesn't generate direct profit -- the attacking wallet is disposable. It takes the "loss" that never settles. The profit comes from what happens after.

### Strategy 1: Free option

Place orders on both YES and NO sides of a market using two wallets -- one clean, one disposable. If the disposable wallet's side wins, settle normally and collect. If it loses, drain the wallet before settlement. The loss reverts. You've turned a 50/50 bet into a free option.

```
Disposable wallet:  BUY YES @ 0.50  (matched by makers)
Clean wallet:       BUY NO  @ 0.50  (matched by makers)

Outcome: YES wins
  - Disposable wallet: YES pays out. Settle normally. Profit.
  - Clean wallet: NO is worthless. Normal loss.

Outcome: NO wins
  - Disposable wallet: YES is worthless. Drain USDC before settlement.
    matchOrders() reverts. No loss.
  - Clean wallet: NO pays out. Settle normally. Profit.

Net: you win either way.
```

### Strategy 2: Orderbook clearing + queue priority

This is subtler. When a `matchOrders()` transaction reverts, Polymarket's system removes all orders involved in the failed match from the book. The attacker uses this to clear out maker orders:

1. Ghost fill clears all maker orders from one side of the book
2. Attacker uses a **second clean wallet** to immediately place orders in the now-empty book
3. With no competition, they set wider spreads
4. Monopoly pricing until other makers notice and re-enter

The window is small, but in 5-minute markets, a few seconds of monopoly pricing is enough.

### Strategy 3: Hunting naked exposure

This is the most damaging variant for sophisticated makers:

1. Identify a maker who hedges across correlated markets
2. Ghost fill the maker's hedge position
3. Maker's bot thinks it's hedged, adjusts other positions accordingly
4. But the hedge doesn't exist -- it was a ghost
5. Trade against the maker's now-exposed position on the other side

The maker is running naked risk without knowing it.

### On-chain evidence

Here's a confirmed ghost fill from the wallet drain variant:

```
Attack tx:     0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0
Status:        0 (reverted)
Revert reason: TRANSFER_FROM_FAILED
Taker:         0xcf23977659940be739745d0c1486cc11d0eaf73d
```

The pattern on Polygonscan:

```
1. Fund disposable wallet with USDC
2. Place orders on CLOB, get matched against makers
3. Before relayer settles: transfer USDC out to funder address
4. matchOrders() tries transferFrom() on empty wallet
5. Reverts with TRANSFER_FROM_FAILED
6. Makers' books are cleared, positions are wrong
```

Funder address `0x46176ef6...E4a20B412` -- trace this to find the profit-taking wallet. The disposable wallet is burned after each use.

As Cutnpaste ($488K trader) confirmed in the Polymarket Discord #devs channel: the ghost fill is a free option, and you get to clear out makers on that side and gain queue priority.

---

## Part 3: Why Existing Tools Don't Work

### nonce-guard

[TheOneWhoBurns/polymarket-nonce-guard](https://github.com/TheOneWhoBurns/polymarket-nonce-guard) monitors `incrementNonce()` calls on the CTF Exchange. It watches for method signature `0x627cdcb9` in pending transactions and alerts if a counterparty increments their nonce after a fill.

The problem: the wallet drain variant has **zero** `incrementNonce()` calls. NotDragN tested it -- tracked every block, zero detections, ghost fills still happening.

nonce-guard solves a specific attack method. The attacker changed methods.

### Polymarket internal blacklist

Polymarket maintains a blacklist of addresses that called `incrementNonce()`. If your address is on the list, your orders get rejected.

Bypasses:
- Wallet drain doesn't call `incrementNonce()` -- no trigger for blacklisting
- Fresh disposable addresses each time -- each attack uses a new wallet
- Addresses are cheap on Polygon

### CTF Exchange V2

Polymarket's V2 contract removes the nonce field entirely. This permanently fixes the `incrementNonce` variant. Good.

But V2 does **not** lock funds at match time. There's no escrow between match and settlement. The wallet drain variant works exactly the same way on V2: CLOB matches your order, you drain USDC before the V2 `matchOrders()` executes, settlement reverts.

Deposit wallets with timelocks exist as an option on V2, but they're not mandatory. As 830Lincoln noted in Discord, there are doubts it's fully fixed on day one.

### The fundamental gap

Every existing defense detects **attack methods**:
- nonce-guard detects `incrementNonce()` calls
- The blacklist blocks known `incrementNonce()` callers
- V2 removes the nonce mechanism

Attackers evolve methods. They went from `incrementNonce()` to wallet drain in weeks. They'll find the next thing after V2 locks it down further. Method-based detection is whack-a-mole.

The only invariant across all variants: **the on-chain settlement transaction reverts**. `status=0` is the universal fingerprint.

---

## Part 4: GhostGuard

I built [GhostGuard](https://github.com/OrderBookTrade/GhostGuard) to detect ghost fills by verifying every CLOB fill on-chain. It doesn't care how the attack happened. It checks whether the fill actually settled.

### Core logic

The entire product is in `verify_fill()`:

```rust
pub async fn verify_fill(
    rpc_url: &str,
    tx_hash: H256,
    config: &Config,
) -> Result<FillVerdict> {
    let provider = Provider::<Http>::try_from(rpc_url)?;
    let deadline = Instant::now() + config.verify_timeout;

    loop {
        if Instant::now() >= deadline {
            return Ok(FillVerdict::Timeout { tx_hash });
        }

        match provider.get_transaction_receipt(tx_hash).await {
            Ok(Some(receipt)) => {
                let status = receipt.status.map(|s| s.as_u64()).unwrap_or(0);
                let block = receipt.block_number.map(|b| b.as_u64()).unwrap_or(0);

                if status == 1 {
                    return Ok(FillVerdict::Real { tx_hash, block });
                }

                // status == 0: reverted. ghost fill.
                let reason = extract_revert_reason(&provider, tx_hash, &receipt).await;
                let counterparty = extract_counterparty(&receipt);

                return Ok(FillVerdict::Ghost {
                    tx_hash,
                    reason,
                    counterparty,
                });
            }
            Ok(None) => { /* no receipt yet, keep polling */ }
            Err(e) => { /* RPC error, keep polling */ }
        }

        sleep(config.poll_interval).await;
    }
}
```

Three possible outcomes, modeled as an enum:

```rust
pub enum FillVerdict {
    Real  { tx_hash: H256, block: u64 },
    Ghost { tx_hash: H256, reason: String, counterparty: Option<Address> },
    Timeout { tx_hash: H256 },
}
```

`Real` means status=1, your fill settled, safe to update state. `Ghost` means status=0, the settlement reverted -- this is the detection. `Timeout` means no receipt after 10 seconds. On Polygon, blocks are every 2 seconds. If there's no receipt after 10s, something is wrong. Treat as ghost until proven otherwise.

The `is_ghost()` helper covers both:

```rust
pub fn is_ghost(&self) -> bool {
    matches!(self, FillVerdict::Ghost { .. } | FillVerdict::Timeout { .. })
}
```

### Why this catches everything

The detection state machine:

```
                    +-----------+
                    |  CLOB     |
                    |  fill     |
                    |  event    |
                    +-----+-----+
                          |
                    extract tx_hash
                          |
                    +-----v-----+
                    |  poll     |
                    |  receipt  |<--------+
                    +-----+-----+         |
                          |               |
                   +------+------+   no receipt
                   |             |   yet
              got receipt   +----+----+
                   |        | timeout?|
            +------+------+ +----+----+
            | status == 1 |      |yes
            |   REAL      | +----v----+
            +-------------+ | TIMEOUT |
            | status == 0 | | (ghost) |
            |   GHOST     | +---------+
            +-------------+
```

It doesn't matter if the revert was caused by `incrementNonce()`, wallet drain, gas front-running, or some new method nobody's discovered yet. If the transaction reverted, GhostGuard catches it. Method-agnostic detection.

### Integration: Rust SDK

```rust
use ghostguard::{GhostGuard, Config};

let config = Config {
    rpc_url: "https://polygon-rpc.com".into(),
    clob_ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
    verify_timeout: Duration::from_secs(10),
    poll_interval: Duration::from_millis(500),
    webhook_url: None,
};

let mut guard = GhostGuard::new(config);

guard.on_real_fill(|verdict| {
    // status=1 confirmed. safe to update positions.
    if let FillVerdict::Real { tx_hash, block } = verdict {
        println!("[REAL] tx={tx_hash:?} block={block}");
    }
});

guard.on_ghost_fill(|event| {
    // ghost detected. roll back state, cancel orders, alert.
    eprintln!("[GHOST] tx={:?} reason={}", event.tx_hash, event.reason);
    // TODO: cancel open orders on this market
    // TODO: recalculate position without the ghost fill
    // TODO: alert ops team
});

guard.start().await?;
```

The `start()` method spawns a websocket listener that connects to the Polymarket CLOB, extracts `tx_hash` from each fill notification, and feeds it to `verify_fill()`. Each verification runs in its own tokio task, so multiple fills are verified concurrently.

### Integration: CLI sidecar

If you don't want to modify your existing bot's code, run GhostGuard as a sidecar process:

```bash
ghostguard \
  --rpc https://polygon-rpc.com \
  --clob-ws wss://ws-subscriptions-clob.polymarket.com/ws/market \
  --webhook http://localhost:8080/fill-alert
```

It logs every verdict to stdout:

```
[REAL]  tx=0xabc123... block=84701564
[GHOST] tx=0x9e3230... reason=TRANSFER_FROM_FAILED
```

And POSTs JSON to your webhook on every fill:

```json
{
  "verdict": "Ghost",
  "tx_hash": "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0",
  "reason": "TRANSFER_FROM_FAILED",
  "counterparty": "0x4bfb41d5b3570defd03c39a9a4d8de6bd8b8982e"
}
```

Your existing bot just needs an HTTP endpoint that receives the webhook and reacts accordingly. Zero changes to your matching logic.

### One-off verification

Test with the known ghost fill tx:

```bash
ghostguard \
  --verify-tx 0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0
```

Output:

```
[GHOST] tx=0x9e32...2df0 reason=TRANSFER_FROM_FAILED
        counterparty=0x4bfb...982e
```

---

## Part 5: How I Thought About This Problem

The insight that changed everything: **stop trying to detect the attack. Detect the result.**

Every previous tool -- nonce-guard, Polymarket's blacklist, even V2's design -- plays whack-a-mole with attack methods. Each defense forces attackers to evolve. `incrementNonce()` gets monitored? Switch to wallet drain. Wallet drain gets fixed? They'll find something else. This is an unwinnable arms race when the attacker's cost is $0.10 on Polygon.

But all attack variants share one invariant: **the on-chain settlement transaction reverts.** `status=0` is the universal fingerprint. By moving detection from "what did the attacker do" to "did my fill actually settle," you make the defense method-agnostic. New attack methods don't require new detection logic.

This is the same principle as antivirus vs. sandboxing:

```
L1 detection (nonce-guard, blacklist):
  - Maintain a database of known attack signatures
  - Each new attack requires a new signature
  - Always one step behind

L2 detection (GhostGuard):
  - Check the outcome: did the transaction succeed?
  - Doesn't matter what the attacker did
  - Works against unknown future attacks
```

Antivirus tries to recognize every virus signature. Sandboxing checks whether the program did something harmful. GhostGuard is sandboxing for Polymarket fills.

### The tradeoff

GhostGuard adds latency. You can't update your state on the CLOB fill notification anymore -- you have to wait for on-chain confirmation. That's 2-10 seconds on Polygon.

In sub-second HFT, this would be a dealbreaker. But Polymarket's 5-minute binary markets created the exact conditions where this tradeoff makes sense: the markets are high-frequency enough to attract ghost fill attacks, but slow enough that 2-10 seconds of verification latency is acceptable. Your bot waits a few seconds before confirming a position, rather than instantly trusting the CLOB.

The 5-minute markets created the problem. They also make the solution viable.

### What GhostGuard doesn't do (yet)

This is v1. It detects ghost fills. It doesn't:

- **Auto re-enter**: After detecting a ghost, automatically re-place the orders that were cleared. Coming in v2.
- **Blacklist counterparties**: Track repeat offenders and avoid filling against them. Coming in v2.
- **Pre-settlement detection**: Monitor the mempool for wallet drains in progress, before settlement fails. This would catch ghosts faster but adds significant complexity.

v1 solves the critical problem: knowing that a ghost happened, in time to react.

---

## Links

- **GhostGuard**: [github.com/OrderBookTrade/GhostGuard](https://github.com/OrderBookTrade/GhostGuard)
- **Known ghost fill tx**: [0x9e3230ab...on Polygonscan](https://polygonscan.com/tx/0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0)
- **CTF Exchange contract**: [0x4bfb41d5...on Polygonscan](https://polygonscan.com/address/0x4bfb41d5b3570defd03c39a9a4d8de6bd8b8982e)

Find me in Polymarket Discord #devs.
