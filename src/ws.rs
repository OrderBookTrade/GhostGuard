use anyhow::Result;
use ethers::types::H256;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// Stream-level events from the CLOB websocket.
#[derive(Debug, Clone)]
pub enum ClobEvent {
    /// A trade / fill with on-chain settlement tx.
    Fill(ClobFill),
    /// A mid-price or orderbook tick for a market.
    PriceUpdate(PriceUpdate),
    /// WebSocket successfully (re)connected.
    Connected,
    /// WebSocket disconnected (before reconnect attempt).
    Disconnected,
    /// Human-readable status message (errors, reconnect attempts).
    /// Forwarded to the TUI feed so the user sees what's happening.
    Status(String),
}

/// A fill event extracted from the CLOB websocket.
#[derive(Debug, Clone)]
pub struct ClobFill {
    pub tx_hash: H256,
    pub market: String,
    pub side: String,
    pub size: f64,
    pub price: f64,
}

/// Best bid / best ask update for a market.
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub market: String,
    pub best_bid: f64,
    pub best_ask: f64,
}


/// Connect to the Polymarket CLOB WebSocket and stream events.
///
/// Reconnects with exponential backoff (1s → 2s → 4s → ... capped at 60s)
/// and resets on a successful connection.
pub async fn listen_clob_events(
    ws_url: &str,
    markets: &[String],
    tx: mpsc::Sender<ClobEvent>,
) -> Result<()> {
    let mut backoff_secs: u64 = 1;

    loop {
        info!(url = ws_url, "connecting to CLOB websocket...");
        let _ = tx
            .send(ClobEvent::Status(format!("connecting to {ws_url}")))
            .await;

        match connect_async(ws_url).await {
            Ok((ws_stream, _)) => {
                info!("CLOB websocket connected");
                backoff_secs = 1;
                let _ = tx.send(ClobEvent::Connected).await;
                let _ = tx
                    .send(ClobEvent::Status("CLOB websocket connected".into()))
                    .await;

                let (mut write, mut read) = ws_stream.split();

                // Polymarket CLOB expects `assets_ids` (plural of asset) with
                // `type = "market"` directly; no "subscribe" envelope.
                let subscribe_msg = if markets.is_empty() {
                    serde_json::json!({
                        "type": "market",
                        "assets_ids": [],
                    })
                } else {
                    serde_json::json!({
                        "type": "market",
                        "assets_ids": markets,
                    })
                };
                let sub_json = subscribe_msg.to_string();
                debug!(payload = %sub_json, "sending subscribe");
                if let Err(e) = write.send(Message::Text(sub_json.clone())).await {
                    error!(error = %e, "failed to send subscribe message");
                    let _ = tx
                        .send(ClobEvent::Status(format!("subscribe send failed: {e}")))
                        .await;
                } else {
                    let _ = tx
                        .send(ClobEvent::Status(format!(
                            "subscribed to {} asset(s)",
                            markets.len()
                        )))
                        .await;
                }

                while let Some(msg_result) = read.next().await {
                    match msg_result {
                        Ok(Message::Text(text)) => {
                            // Log every raw message at debug. Useful for
                            // `RUST_LOG=ghostguard::ws=debug` diagnosis.
                            debug!(len = text.len(), raw = %truncate(&text, 400), "ws msg");

                            for event in parse_clob_message(&text) {
                                match &event {
                                    ClobEvent::Fill(f) => {
                                        debug!(market = %f.market, size = f.size, price = f.price, "parsed fill");
                                    }
                                    ClobEvent::PriceUpdate(p) => {
                                        debug!(market = %p.market, bid = p.best_bid, ask = p.best_ask, "parsed price update");
                                    }
                                    _ => {}
                                }
                                if tx.send(event).await.is_err() {
                                    info!("event channel closed, shutting down ws listener");
                                    return Ok(());
                                }
                            }
                        }
                        Ok(Message::Ping(data)) => {
                            let _ = write.send(Message::Pong(data)).await;
                        }
                        Ok(Message::Close(_)) => {
                            warn!("CLOB websocket closed by server");
                            break;
                        }
                        Err(e) => {
                            error!(error = %e, "websocket error");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "failed to connect to CLOB websocket");
                let _ = tx
                    .send(ClobEvent::Status(format!("connect failed: {e}")))
                    .await;
            }
        }

        let _ = tx.send(ClobEvent::Disconnected).await;
        warn!(backoff_secs, "reconnecting to CLOB websocket...");
        let _ = tx
            .send(ClobEvent::Status(format!(
                "reconnecting in {backoff_secs}s..."
            )))
            .await;
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

/// Parse a raw CLOB websocket message.
///
/// Polymarket's market channel emits events with fields at the TOP level
/// (no `data` wrapper) and may batch multiple events into a JSON array.
/// Known `event_type` values:
///   - `book`              — full orderbook snapshot (bids + asks)
///   - `price_change`      — per-level update (array of `changes`)
///   - `last_trade_price`  — most recent trade (no tx_hash!)
///   - `tick_size_change`  — rare, ignored
fn parse_clob_message(text: &str) -> Vec<ClobEvent> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, raw = %truncate(text, 200), "failed to parse ws msg");
            return vec![];
        }
    };

    if let Some(arr) = value.as_array() {
        arr.iter().filter_map(parse_single_event).collect()
    } else {
        parse_single_event(&value).into_iter().collect()
    }
}

fn parse_single_event(v: &serde_json::Value) -> Option<ClobEvent> {
    let event_type = v
        .get("event_type")
        .or_else(|| v.get("type"))
        .and_then(|x| x.as_str())
        .unwrap_or("");

    match event_type {
        "book" => parse_book(v).map(ClobEvent::PriceUpdate),
        "price_change" => parse_price_change(v).map(ClobEvent::PriceUpdate),
        "last_trade_price" | "trade" | "fill" | "order_fill" => {
            parse_last_trade(v).map(ClobEvent::Fill)
        }
        "tick_size_change" | "pong" | "heartbeat" | "" => None,
        other => {
            debug!(event_type = other, "unknown ws event type");
            None
        }
    }
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

fn f64_field(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|x| match x {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    })
}

fn market_id(v: &serde_json::Value) -> String {
    str_field(v, "asset_id")
        .or_else(|| str_field(v, "market"))
        .unwrap_or_default()
}

/// Parse an orderbook snapshot into a mid-price update.
/// `bids` and `asks` are arrays of `{price, size}` objects. Sizes may be "0"
/// for removed levels; those are filtered out.
fn parse_book(v: &serde_json::Value) -> Option<PriceUpdate> {
    let market = market_id(v);
    if market.is_empty() {
        return None;
    }

    let bids = v.get("bids").and_then(|b| b.as_array())?;
    let asks = v.get("asks").and_then(|a| a.as_array())?;

    let best_bid = bids
        .iter()
        .filter(|lvl| f64_field(lvl, "size").unwrap_or(0.0) > 0.0)
        .filter_map(|lvl| f64_field(lvl, "price"))
        .fold(0.0_f64, f64::max);

    let best_ask = bids_min_ask(asks);

    if best_bid <= 0.0 || !best_ask.is_finite() || best_ask <= 0.0 {
        return None;
    }

    Some(PriceUpdate {
        market,
        best_bid,
        best_ask,
    })
}

fn bids_min_ask(asks: &[serde_json::Value]) -> f64 {
    asks.iter()
        .filter(|lvl| f64_field(lvl, "size").unwrap_or(0.0) > 0.0)
        .filter_map(|lvl| f64_field(lvl, "price"))
        .filter(|&p| p > 0.0)
        .fold(f64::INFINITY, f64::min)
}

/// Parse an incremental price change. Multiple changes may share one message.
/// We re-emit the FIRST non-zero bid/ask we find; this is an approximation
/// (the real orderbook top might need delta reconstruction) but is good
/// enough to seed a rolling mid for predictive scoring.
fn parse_price_change(v: &serde_json::Value) -> Option<PriceUpdate> {
    let market = market_id(v);
    if market.is_empty() {
        return None;
    }

    let changes = v.get("changes").and_then(|c| c.as_array())?;
    let mut best_bid: f64 = 0.0;
    let mut best_ask: f64 = f64::INFINITY;
    for ch in changes {
        let price = f64_field(ch, "price").unwrap_or(0.0);
        let size = f64_field(ch, "size").unwrap_or(0.0);
        let side = str_field(ch, "side").unwrap_or_default();
        if price <= 0.0 || size <= 0.0 {
            continue;
        }
        match side.as_str() {
            "BUY" | "bid" => {
                if price > best_bid {
                    best_bid = price;
                }
            }
            "SELL" | "ask" => {
                if price < best_ask {
                    best_ask = price;
                }
            }
            _ => {}
        }
    }

    if best_bid > 0.0 && best_ask.is_finite() {
        Some(PriceUpdate {
            market,
            best_bid,
            best_ask,
        })
    } else {
        None
    }
}

/// Parse a `last_trade_price` event. Polymarket's market channel does NOT
/// include a settlement transaction hash — that's resolved later via the
/// trade-history API. We emit a `ClobFill` with a zero `tx_hash`; downstream
/// code (detection.rs) skips on-chain verification when the hash is zero.
fn parse_last_trade(v: &serde_json::Value) -> Option<ClobFill> {
    let market = market_id(v);
    if market.is_empty() {
        return None;
    }

    let size = f64_field(v, "size").unwrap_or(0.0);
    let price = f64_field(v, "price").unwrap_or(0.0);
    if size <= 0.0 || price <= 0.0 {
        return None;
    }

    // Prefer explicit transaction_hash if the event happens to include one
    // (e.g. user-channel fills). Otherwise leave as zero.
    let tx_hash = str_field(v, "transaction_hash")
        .or_else(|| str_field(v, "transactionHash"))
        .and_then(|s| s.parse::<H256>().ok())
        .unwrap_or_else(H256::zero);

    Some(ClobFill {
        tx_hash,
        market,
        side: str_field(v, "side").unwrap_or_default(),
        size,
        price,
    })
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

    fn first(events: Vec<ClobEvent>) -> ClobEvent {
        events.into_iter().next().expect("expected one event")
    }

    #[test]
    fn test_parse_last_trade_polymarket_format() {
        // Real Polymarket market channel format: fields at top level,
        // event_type instead of type, no data wrapper, no tx_hash.
        let json = r#"{
            "event_type": "last_trade_price",
            "asset_id": "12345",
            "market": "0xabc",
            "price": "0.42",
            "side": "SELL",
            "size": "25",
            "timestamp": "1700000000"
        }"#;

        match first(parse_clob_message(json)) {
            ClobEvent::Fill(f) => {
                assert_eq!(f.market, "12345");
                assert_eq!(f.side, "SELL");
                assert!((f.size - 25.0).abs() < f64::EPSILON);
                assert!((f.price - 0.42).abs() < f64::EPSILON);
                assert_eq!(f.tx_hash, H256::zero(), "market channel has no tx hash");
            }
            _ => panic!("expected Fill"),
        }
    }

    #[test]
    fn test_parse_book_snapshot() {
        let json = r#"{
            "event_type": "book",
            "asset_id": "12345",
            "market": "0xabc",
            "bids": [
                {"price": "0.48", "size": "100"},
                {"price": "0.47", "size": "200"}
            ],
            "asks": [
                {"price": "0.52", "size": "150"},
                {"price": "0.53", "size": "50"}
            ]
        }"#;

        match first(parse_clob_message(json)) {
            ClobEvent::PriceUpdate(p) => {
                assert_eq!(p.market, "12345");
                assert!((p.best_bid - 0.48).abs() < f64::EPSILON);
                assert!((p.best_ask - 0.52).abs() < f64::EPSILON);
            }
            _ => panic!("expected PriceUpdate"),
        }
    }

    #[test]
    fn test_parse_book_skips_zero_sizes() {
        // Size 0 means the level was removed.
        let json = r#"{
            "event_type": "book",
            "asset_id": "m",
            "bids": [
                {"price": "0.99", "size": "0"},
                {"price": "0.48", "size": "100"}
            ],
            "asks": [
                {"price": "0.01", "size": "0"},
                {"price": "0.52", "size": "50"}
            ]
        }"#;
        match first(parse_clob_message(json)) {
            ClobEvent::PriceUpdate(p) => {
                assert!((p.best_bid - 0.48).abs() < f64::EPSILON);
                assert!((p.best_ask - 0.52).abs() < f64::EPSILON);
            }
            _ => panic!("expected PriceUpdate"),
        }
    }

    #[test]
    fn test_parse_price_change() {
        let json = r#"{
            "event_type": "price_change",
            "asset_id": "m",
            "changes": [
                {"price": "0.49", "side": "BUY", "size": "100"},
                {"price": "0.51", "side": "SELL", "size": "200"}
            ]
        }"#;
        match first(parse_clob_message(json)) {
            ClobEvent::PriceUpdate(p) => {
                assert!((p.best_bid - 0.49).abs() < f64::EPSILON);
                assert!((p.best_ask - 0.51).abs() < f64::EPSILON);
            }
            _ => panic!("expected PriceUpdate"),
        }
    }

    #[test]
    fn test_parse_message_array() {
        // Polymarket batches events into an array on initial snapshot.
        let json = r#"[
            {"event_type": "book", "asset_id": "m",
             "bids": [{"price":"0.48","size":"100"}],
             "asks": [{"price":"0.52","size":"100"}]},
            {"event_type": "last_trade_price", "asset_id": "m",
             "price": "0.50", "side": "BUY", "size": "10"}
        ]"#;
        let events = parse_clob_message(json);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ClobEvent::PriceUpdate(_)));
        assert!(matches!(events[1], ClobEvent::Fill(_)));
    }

    #[test]
    fn test_parse_unknown_event() {
        let json = r#"{"event_type": "tick_size_change", "asset_id": "m"}"#;
        assert!(parse_clob_message(json).is_empty());
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(parse_clob_message("not json").is_empty());
    }

    #[test]
    fn test_last_trade_with_tx_hash_preserved() {
        // User-channel fills may include the settlement tx hash.
        let json = r#"{
            "event_type": "trade",
            "asset_id": "m",
            "side": "BUY",
            "size": "50",
            "price": "0.6",
            "transaction_hash": "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0"
        }"#;
        match first(parse_clob_message(json)) {
            ClobEvent::Fill(f) => {
                assert_ne!(f.tx_hash, H256::zero());
            }
            _ => panic!("expected Fill"),
        }
    }
}
