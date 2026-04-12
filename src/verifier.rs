use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider, RawCall};
use ethers::types::{Address, H256};
use tokio::time::{sleep, Instant};
use tracing::{debug, info, warn};

use crate::types::{Config, FillVerdict, TRANSFER_FROM_FAILED_SELECTOR};

/// Verify a single fill transaction on-chain.
///
/// Polls `eth_getTransactionReceipt` until:
/// - receipt.status == 1 → `FillVerdict::Real`
/// - receipt.status == 0 → `FillVerdict::Ghost` (parses revert reason)
/// - timeout expires     → `FillVerdict::Timeout`
pub async fn verify_fill(
    rpc_url: &str,
    tx_hash: H256,
    config: &Config,
) -> Result<FillVerdict> {
    let provider = Provider::<Http>::try_from(rpc_url)
        .context("failed to create RPC provider")?;

    let deadline = Instant::now() + config.verify_timeout;

    loop {
        if Instant::now() >= deadline {
            warn!(?tx_hash, "verification timed out — treating as ghost");
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

                // status == 0: transaction reverted — ghost fill
                let reason = extract_revert_reason(&provider, tx_hash, &receipt).await;
                let counterparty = extract_counterparty(&receipt);

                warn!(?tx_hash, %reason, "fill reverted — GHOST");
                return Ok(FillVerdict::Ghost {
                    tx_hash,
                    reason,
                    counterparty,
                });
            }
            Ok(None) => {
                debug!(?tx_hash, "no receipt yet, polling...");
            }
            Err(e) => {
                warn!(?tx_hash, error = %e, "RPC error during receipt poll");
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
    tx_hash: H256,
    receipt: &ethers::types::TransactionReceipt,
) -> String {
    // Try to get the original transaction to replay it
    let tx = match provider.get_transaction(tx_hash).await {
        Ok(Some(tx)) => tx,
        _ => return "unknown revert (tx not found)".into(),
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

    match provider.call_raw(&call_request).block(block.unwrap().into()).await {
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

/// Extract counterparty address from the transaction's `to` field or input data.
fn extract_counterparty(
    receipt: &ethers::types::TransactionReceipt,
) -> Option<Address> {
    // The `from` in the receipt is the sender (settlement bot).
    // For CTF exchange calls, the counterparty is encoded in calldata,
    // but as a first pass we return the `to` address (the exchange contract).
    // In v2 we can decode the actual taker/maker from calldata.
    receipt.to
}

/// Try to parse Solidity's `Error(string)` from an error message.
fn parse_solidity_revert(err: &str) -> Option<String> {
    // The standard revert selector is 0x08c379a0
    if let Some(idx) = err.find("08c379a0") {
        let hex_start = idx + 8; // skip selector
        let remaining = &err[hex_start..];
        // Extract hex chars
        let hex: String = remaining.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
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
        let selector = &hex_data[..8];
        // Common Polymarket CTF exchange errors can be matched here
        match selector {
            // Add known selectors as discovered
            _ => {}
        }
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

        let tx_hash: H256 =
            "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0"
                .parse()
                .unwrap();

        let verdict = verify_fill(&config.rpc_url, tx_hash, &config).await.unwrap();
        println!("Verdict: {verdict:?}");

        assert!(
            verdict.is_ghost(),
            "expected ghost fill, got: {verdict:?}"
        );
    }
}
