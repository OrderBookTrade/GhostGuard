use anyhow::{anyhow, Context, Result};
use ethers::providers::{Http, Middleware, Provider, RawCall};
use ethers::types::{Address, H256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{sleep, Instant};
use tracing::{debug, error, info, warn};

use crate::callback;
use crate::logging::JsonlWriter;
use crate::tui::TuiEvent;
use crate::types::{Config, FillVerdict, GhostFillEvent, TRANSFER_FROM_FAILED_SELECTOR};
use crate::ws::ClobFill;

/// Max consecutive RPC connection errors before verify_fill bails out.
/// This avoids burning the whole verify_timeout when the RPC is unreachable.
const MAX_CONSECUTIVE_RPC_ERRORS: u32 = 3;

/// Minimum seconds between TUI [SYS] "RPC unreachable" messages — otherwise
/// a broken RPC would flood the feed with one entry per fill.
const RPC_ERROR_NOTICE_INTERVAL_SECS: u64 = 30;

/// Callbacks dispatched by `handle_fill` after on-chain verification.
pub type VerdictCallback = Arc<dyn Fn(FillVerdict) + Send + Sync>;
pub type GhostCallback = Arc<dyn Fn(GhostFillEvent) + Send + Sync>;

/// Dependencies for the detection pipeline.
#[derive(Clone)]
pub struct DetectionContext {
    pub config: Arc<Config>,
    pub verdict_log: Option<Arc<JsonlWriter>>,
    pub on_real: Arc<Vec<VerdictCallback>>,
    pub on_ghost: Arc<Vec<GhostCallback>>,
    /// When TUI mode is enabled, verdicts are forwarded here for rendering.
    pub tui_tx: Option<mpsc::UnboundedSender<TuiEvent>>,
    /// Unix timestamp of the last RPC-failure notice sent to the TUI.
    /// Used to rate-limit the [SYS] "RPC unreachable" messages.
    pub last_rpc_notice: Arc<AtomicU64>,
}

/// Verify a single fill transaction on-chain.
///
/// Polls `eth_getTransactionReceipt` until:
/// - `receipt.status == 1` → `FillVerdict::Real`
/// - `receipt.status == 0` → `FillVerdict::Ghost` (parses revert reason)
/// - timeout expires with no receipt → `FillVerdict::Timeout`
///
/// If the RPC is unreachable (`MAX_CONSECUTIVE_RPC_ERRORS` in a row), returns
/// `Err` early so the caller can distinguish "infrastructure broken" from
/// "genuine timeout". Callers should NOT count Err results as ghosts.
pub async fn verify_fill(rpc_url: &str, tx_hash: H256, config: &Config) -> Result<FillVerdict> {
    let provider = Provider::<Http>::try_from(rpc_url).context("failed to create RPC provider")?;

    let deadline = Instant::now() + config.verify_timeout;
    let mut consecutive_errors: u32 = 0;

    loop {
        if Instant::now() >= deadline {
            warn!(
                ?tx_hash,
                "verification timed out — no receipt before deadline"
            );
            return Ok(FillVerdict::Timeout { tx_hash });
        }

        match provider.get_transaction_receipt(tx_hash).await {
            Ok(Some(receipt)) => {
                let status = receipt.status.map(|s| s.as_u64()).unwrap_or(0);
                let block = receipt.block_number.map(|b| b.as_u64()).unwrap_or(0);

                if status == 1 {
                    info!(?tx_hash, block, "fill verified — REAL");
                    return Ok(FillVerdict::Real { tx_hash, block });
                }

                // status == 0: transaction reverted — ghost fill. Fetch tx
                // once and derive both the revert reason and the actual
                // counterparty (decoded from calldata, NOT the exchange
                // contract address in receipt.to).
                let tx = provider.get_transaction(tx_hash).await.ok().flatten();
                let reason = extract_revert_reason(&provider, &receipt, tx.as_ref()).await;
                let counterparty = extract_counterparty_from_calldata(tx.as_ref());

                if let Some(cp) = counterparty {
                    warn!(?tx_hash, %reason, ?cp, "fill reverted — GHOST");
                } else {
                    warn!(?tx_hash, %reason, "fill reverted — GHOST (no counterparty decoded)");
                }
                return Ok(FillVerdict::Ghost {
                    tx_hash,
                    reason,
                    counterparty,
                });
            }
            Ok(None) => {
                // Real response — server reachable, tx just not mined yet.
                consecutive_errors = 0;
                debug!(?tx_hash, "no receipt yet, polling...");
            }
            Err(e) => {
                consecutive_errors += 1;
                let err_str = e.to_string();
                warn!(
                    ?tx_hash,
                    error = %err_str,
                    consecutive_errors,
                    "RPC error during receipt poll"
                );

                if consecutive_errors >= MAX_CONSECUTIVE_RPC_ERRORS {
                    return Err(anyhow!(
                        "RPC unreachable after {consecutive_errors} consecutive errors: {err_str}"
                    ));
                }
            }
        }

        sleep(config.poll_interval).await;
    }
}

/// Try to extract a human-readable revert reason from a reverted tx.
///
/// Strategies:
/// 1. Call `eth_call` to replay the tx and get the revert data.
/// 2. Look for known signatures in the revert data.
/// 3. Fall back to "unknown revert".
async fn extract_revert_reason(
    provider: &Provider<Http>,
    receipt: &ethers::types::TransactionReceipt,
    tx: Option<&ethers::types::Transaction>,
) -> String {
    let tx = match tx {
        Some(tx) => tx,
        None => return "unknown revert (tx not found)".into(),
    };

    // Replay the transaction via eth_call at the block it was included in
    let block = receipt.block_number;
    let call_request = ethers::types::transaction::eip2718::TypedTransaction::Legacy(
        ethers::types::TransactionRequest {
            from: tx.from.into(),
            to: tx.to.map(ethers::types::NameOrAddress::Address),
            gas: tx.gas.into(),
            gas_price: tx.gas_price,
            value: tx.value.into(),
            data: tx.input.clone().into(),
            nonce: None,
            chain_id: None,
        },
    );

    match provider
        .call_raw(&call_request)
        .block(block.unwrap().into())
        .await
    {
        Err(e) => {
            let err_str = e.to_string();

            // Check for TRANSFER_FROM_FAILED in the error message
            if err_str.contains(TRANSFER_FROM_FAILED_SELECTOR) {
                return TRANSFER_FROM_FAILED_SELECTOR.to_string();
            }

            // Try to decode standard Solidity Error(string) from revert data
            if let Some(reason) = parse_solidity_revert(&err_str) {
                return reason;
            }

            // Check for common patterns in hex revert data
            if let Some(hex_data) = extract_hex_revert_data(&err_str) {
                if let Some(reason) = decode_revert_bytes(&hex_data) {
                    return reason;
                }
            }

            format!("reverted: {}", truncate(&err_str, 200))
        }
        Ok(_) => {
            // eth_call succeeded — this can happen if state changed between
            // the original tx and our replay. Still a ghost fill though.
            "reverted on-chain (eth_call replay succeeded — state-dependent revert)".into()
        }
    }
}

/// Addresses we should EXCLUDE when scanning calldata for counterparties —
/// these are Polymarket infrastructure, not actual traders.
fn is_known_contract(addr: Address) -> bool {
    // Parse once at compile time would need const-fn, so do it lazily.
    // Comparison is cheap (20-byte memcmp).
    const KNOWN: &[&str] = &[
        // Polymarket CTF Exchange
        "0x4bfb41d5b3570defd03c39a9a4d8de6bd8b8982e",
        // Polymarket NegRisk CTF Exchange
        "0xC5d563A36AE78145C45a50134d48A1215220f80a",
        // Polymarket Fee Module (operator)
        "0xC5d563A36AE78145C45a50134d48A1215220f80a",
        // USDC.e on Polygon
        "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
        // Conditional Tokens Framework (ERC1155)
        "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045",
        // Known Polymarket relayers/operators
        "0x0E92bA4A56f24C4371E4d097adfB6D2d3F0a0c6e",
    ];
    KNOWN
        .iter()
        .any(|k| k.parse::<Address>().map(|a| a == addr).unwrap_or(false))
}

/// Extract the actual counterparty (maker/taker) from the transaction calldata.
///
/// For Polymarket `matchOrders` / `fillOrder` / `fillOrders`, the real user
/// addresses are encoded inside `Order` tuples in the calldata, NOT in
/// `receipt.to` (which is always the exchange contract).
///
/// Strategy: scan the calldata body for every 32-byte slot that looks like
/// an address-encoded value (12 zero bytes + 20 non-zero bytes). Filter out
/// known Polymarket contract addresses. Return the first remaining address —
/// this is typically the maker of the order that failed to settle, which is
/// the wallet that drained its balance (the attacker) in the common case.
fn extract_counterparty_from_calldata(tx: Option<&ethers::types::Transaction>) -> Option<Address> {
    let tx = tx?;
    let input = tx.input.as_ref();
    // Skip the 4-byte function selector.
    if input.len() <= 4 {
        return None;
    }
    let body = &input[4..];

    for chunk in body.chunks_exact(32) {
        // ABI-encoded `address` = 12 zero bytes + 20 address bytes.
        if chunk[..12].iter().any(|&b| b != 0) {
            continue;
        }
        let addr = Address::from_slice(&chunk[12..32]);
        if addr.is_zero() {
            continue;
        }
        if is_known_contract(addr) {
            continue;
        }
        // Also skip the relayer that submitted the tx.
        if addr == tx.from {
            continue;
        }
        return Some(addr);
    }
    None
}

/// Try to parse Solidity's `Error(string)` from an error message.
fn parse_solidity_revert(err: &str) -> Option<String> {
    // The standard revert selector is 0x08c379a0
    if let Some(idx) = err.find("08c379a0") {
        let hex_start = idx + 8; // skip selector
        let remaining = &err[hex_start..];
        // Extract hex chars
        let hex: String = remaining
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if hex.len() >= 128 {
            // offset (32 bytes) + length (32 bytes) + string data
            let len_hex = &hex[64..128];
            if let Ok(len) = usize::from_str_radix(len_hex, 16) {
                let str_hex = &hex[128..128 + (len * 2).min(hex.len() - 128)];
                if let Ok(bytes) = hex::decode(str_hex) {
                    if let Ok(s) = String::from_utf8(bytes) {
                        return Some(s);
                    }
                }
            }
        }
    }
    None
}

/// Extract hex-encoded revert data from a provider error string.
fn extract_hex_revert_data(err: &str) -> Option<String> {
    // Many providers return revert data as 0x-prefixed hex
    if let Some(idx) = err.find("0x") {
        let hex: String = err[idx + 2..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if hex.len() >= 8 {
            return Some(hex);
        }
    }
    None
}

/// Decode raw revert bytes looking for known signatures.
fn decode_revert_bytes(hex_data: &str) -> Option<String> {
    // Check for known custom error selectors
    if hex_data.len() >= 8 {
        let _selector = &hex_data[..8];
        // Common Polymarket CTF exchange errors can be matched here
        // Add known selectors as discovered
    }

    // Check if the entire revert data decodes to ASCII
    if let Ok(bytes) = hex::decode(hex_data) {
        let ascii: String = bytes
            .iter()
            .filter(|b| b.is_ascii_graphic() || b.is_ascii_whitespace())
            .map(|b| *b as char)
            .collect();
        if ascii.len() > 4 {
            return Some(ascii.trim().to_string());
        }
    }

    None
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Send a rate-limited [SYS] message to the TUI when the RPC becomes
/// unreachable. Only fires once every `RPC_ERROR_NOTICE_INTERVAL_SECS`.
fn maybe_notify_rpc_failure(ctx: &DetectionContext, err: &str) {
    let tx = match &ctx.tui_tx {
        Some(tx) => tx,
        None => return,
    };

    let now = now_secs();
    let last = ctx.last_rpc_notice.load(Ordering::Relaxed);
    if now.saturating_sub(last) < RPC_ERROR_NOTICE_INTERVAL_SECS {
        return; // still within rate-limit window
    }

    // Try to claim the window; if someone else already did, skip.
    if ctx
        .last_rpc_notice
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    let short = err.chars().take(160).collect::<String>();
    let _ = tx.send(TuiEvent::Status(format!(
        "RPC unreachable: {short} — try --rpc https://polygon.drpc.org"
    )));
}

/// Run the full verify → log → dispatch pipeline for a single fill.
///
/// This is the orchestrator that `lib.rs` spawns per `ClobEvent::Fill`.
/// Runs `verify_fill`, appends to the verdict JSONL log (if configured),
/// POSTs to the webhook (if configured), and fires registered callbacks.
pub async fn handle_fill(ctx: DetectionContext, fill: ClobFill) {
    let config = &ctx.config;

    let started = Instant::now();
    let verdict = match verify_fill(&config.rpc_url, fill.tx_hash, config).await {
        Ok(v) => v,
        Err(e) => {
            // RPC unreachable — this is an infrastructure problem, NOT a ghost
            // fill. Don't emit a verdict. Instead, send a rate-limited status
            // message to the TUI so the user sees what's happening.
            error!(tx_hash = ?fill.tx_hash, error = %e, "verification failed");
            maybe_notify_rpc_failure(&ctx, &e.to_string());
            return;
        }
    };
    let latency_ms = started.elapsed().as_millis() as u64;

    // TUI event
    if let Some(ref tx) = ctx.tui_tx {
        let _ = tx.send(TuiEvent::Verdict {
            verdict: verdict.clone(),
            fill: fill.clone(),
            latency_ms,
        });
    }

    // Webhook dispatch (verdict level)
    if let Some(ref url) = config.webhook_url {
        if let Err(e) = callback::send_webhook(url, &verdict).await {
            warn!(error = %e, "webhook dispatch failed");
        }
    }

    // JSONL audit log
    if let Some(ref log) = ctx.verdict_log {
        if let Err(e) = log.append(&build_verdict_log_line(&verdict, &fill)).await {
            warn!(error = %e, "failed to append verdict log");
        }
    }

    // Dispatch to registered callbacks
    match &verdict {
        FillVerdict::Real { .. } => {
            for cb in ctx.on_real.iter() {
                cb(verdict.clone());
            }
        }
        FillVerdict::Ghost {
            tx_hash,
            reason,
            counterparty,
        } => {
            let event = GhostFillEvent {
                tx_hash: *tx_hash,
                market: fill.market.clone(),
                side: fill.side.clone(),
                size: fill.size,
                price: fill.price,
                counterparty: *counterparty,
                reason: reason.clone(),
                timestamp: now_secs(),
            };

            if let Some(ref url) = config.webhook_url {
                if let Err(e) = callback::send_ghost_event_webhook(url, &event).await {
                    warn!(error = %e, "ghost event webhook failed");
                }
            }

            for cb in ctx.on_ghost.iter() {
                cb(event.clone());
            }
        }
        FillVerdict::Timeout { tx_hash } => {
            let event = GhostFillEvent {
                tx_hash: *tx_hash,
                market: fill.market.clone(),
                side: fill.side.clone(),
                size: fill.size,
                price: fill.price,
                counterparty: None,
                reason: "timeout — no receipt".into(),
                timestamp: now_secs(),
            };

            for cb in ctx.on_ghost.iter() {
                cb(event.clone());
            }
        }
    }
}

/// Build the JSONL audit log line for a verdict.
fn build_verdict_log_line(verdict: &FillVerdict, fill: &ClobFill) -> serde_json::Value {
    let ts = now_secs();
    match verdict {
        FillVerdict::Real { tx_hash, block } => serde_json::json!({
            "ts": ts,
            "kind": "verdict",
            "verdict": "Real",
            "tx": format!("{:?}", tx_hash),
            "market": fill.market,
            "block": block,
            "size": fill.size,
            "price": fill.price,
        }),
        FillVerdict::Ghost {
            tx_hash,
            reason,
            counterparty,
        } => serde_json::json!({
            "ts": ts,
            "kind": "verdict",
            "verdict": "Ghost",
            "tx": format!("{:?}", tx_hash),
            "market": fill.market,
            "reason": reason,
            "counterparty": counterparty.map(|a| format!("{a:?}")),
            "size": fill.size,
            "price": fill.price,
        }),
        FillVerdict::Timeout { tx_hash } => serde_json::json!({
            "ts": ts,
            "kind": "verdict",
            "verdict": "Timeout",
            "tx": format!("{:?}", tx_hash),
            "market": fill.market,
            "size": fill.size,
            "price": fill.price,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Config;
    use std::time::Duration;

    /// Test with a known ghost fill tx on Polygon mainnet.
    /// This tx has status=0 and reverted with TRANSFER_FROM_FAILED.
    ///
    /// Run with: cargo test -- --ignored test_known_ghost_fill
    #[tokio::test]
    #[ignore = "requires Polygon RPC access"]
    async fn test_known_ghost_fill() {
        let config = Config {
            rpc_url: "https://polygon-rpc.com".into(),
            verify_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(500),
            ..Default::default()
        };

        let tx_hash: H256 = "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0"
            .parse()
            .unwrap();

        let verdict = verify_fill(&config.rpc_url, tx_hash, &config)
            .await
            .unwrap();
        println!("Verdict: {verdict:?}");

        assert!(verdict.is_ghost(), "expected ghost fill, got: {verdict:?}");
    }
}
