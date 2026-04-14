use anyhow::Result;
use ethers::types::H256;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
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

/// Envelope for any CLOB message — data is held as generic JSON so we can
/// dispatch on `msg_type` and decode the payload in a second pass.
#[derive(Debug, Deserialize)]
struct ClobMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    data: Option<serde_json::Value>,
    // Some CLOB variants put event_type at the top level.
    event_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClobFillData {
    #[serde(rename = "transactionHash")]
    transaction_hash: Option<String>,
    #[serde(rename = "transaction_hash")]
    transaction_hash_alt: Option<String>,
    market: Option<String>,
    #[serde(rename = "asset_id")]
    asset_id: Option<String>,
    side: Option<String>,
    size: Option<String>,
    price: Option<String>,
}

impl ClobFillData {
    fn tx_hash_str(&self) -> Option<&str> {
        self.transaction_hash
            .as_deref()
            .or(self.transaction_hash_alt.as_deref())
    }

    fn market(&self) -> String {
        self.market
            .clone()
            .or_else(|| self.asset_id.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
struct ClobPriceData {
    market: Option<String>,
    #[serde(rename = "asset_id")]
    asset_id: Option<String>,
    #[serde(rename = "bestBid")]
    best_bid_camel: Option<String>,
    #[serde(rename = "best_bid")]
    best_bid_snake: Option<String>,
    bid: Option<String>,
    #[serde(rename = "bestAsk")]
    best_ask_camel: Option<String>,
    #[serde(rename = "best_ask")]
    best_ask_snake: Option<String>,
    ask: Option<String>,
}

impl ClobPriceData {
    fn market(&self) -> Option<String> {
        self.market.clone().or_else(|| self.asset_id.clone())
    }

    fn best_bid(&self) -> Option<f64> {
        self.best_bid_camel
            .as_deref()
            .or(self.best_bid_snake.as_deref())
            .or(self.bid.as_deref())
            .and_then(|s| s.parse().ok())
    }

    fn best_ask(&self) -> Option<f64> {
        self.best_ask_camel
            .as_deref()
            .or(self.best_ask_snake.as_deref())
            .or(self.ask.as_deref())
            .and_then(|s| s.parse().ok())
    }
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
                            if let Some(event) = parse_clob_message(&text) {
                                match &event {
                                    ClobEvent::Fill(f) => {
                                        debug!(?f.tx_hash, "received fill");
                                    }
                                    ClobEvent::PriceUpdate(p) => {
                                        debug!(market = %p.market, "received price update");
                                    }
                                    // Connected/Disconnected are synthesized by the
                                    // listener itself, never returned by the parser.
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

/// Parse any CLOB websocket message into a `ClobEvent`, or `None` if it's
/// a heartbeat / unknown type / malformed payload.
fn parse_clob_message(text: &str) -> Option<ClobEvent> {
    let msg: ClobMessage = serde_json::from_str(text).ok()?;
    let ty = msg
        .msg_type
        .as_deref()
        .or(msg.event_type.as_deref())
        .unwrap_or("");
    let data = msg.data?;

    match ty {
        "fill" | "trade" | "order_fill" | "last_trade_price" => {
            parse_fill(&data).map(ClobEvent::Fill)
        }
        "price_change" | "book_update" | "ticker" => {
            parse_price_update(&data).map(ClobEvent::PriceUpdate)
        }
        _ => None,
    }
}

fn parse_fill(data: &serde_json::Value) -> Option<ClobFill> {
    let d: ClobFillData = serde_json::from_value(data.clone()).ok()?;
    let tx_str = d.tx_hash_str()?;
    let tx_hash: H256 = tx_str
        .parse()
        .inspect_err(|e| warn!(tx = tx_str, error = %e, "invalid tx hash"))
        .ok()?;

    Some(ClobFill {
        tx_hash,
        market: d.market(),
        side: d.side.clone().unwrap_or_default(),
        size: d
            .size
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
        price: d
            .price
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
    })
}

fn parse_price_update(data: &serde_json::Value) -> Option<PriceUpdate> {
    let d: ClobPriceData = serde_json::from_value(data.clone()).ok()?;
    let market = d.market()?;
    let best_bid = d.best_bid()?;
    let best_ask = d.best_ask()?;

    if best_bid <= 0.0 || best_ask <= 0.0 {
        return None;
    }

    Some(PriceUpdate {
        market,
        best_bid,
        best_ask,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fill_message() {
        let json = r#"{
            "type": "fill",
            "data": {
                "transactionHash": "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0",
                "market": "0xabc123",
                "side": "BUY",
                "size": "100.0",
                "price": "0.65"
            }
        }"#;

        let ev = parse_clob_message(json).expect("should parse");
        match ev {
            ClobEvent::Fill(f) => {
                assert_eq!(
                    format!("{:?}", f.tx_hash),
                    "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0"
                );
                assert_eq!(f.market, "0xabc123");
                assert_eq!(f.side, "BUY");
                assert!((f.size - 100.0).abs() < f64::EPSILON);
                assert!((f.price - 0.65).abs() < f64::EPSILON);
            }
            _ => panic!("expected Fill"),
        }
    }

    #[test]
    fn test_parse_last_trade_price_as_fill() {
        let json = r#"{
            "type": "last_trade_price",
            "data": {
                "transaction_hash": "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0",
                "asset_id": "tok_1",
                "side": "SELL",
                "size": "25",
                "price": "0.42"
            }
        }"#;

        let ev = parse_clob_message(json).expect("should parse");
        match ev {
            ClobEvent::Fill(f) => {
                assert_eq!(f.market, "tok_1");
                assert_eq!(f.side, "SELL");
            }
            _ => panic!("expected Fill"),
        }
    }

    #[test]
    fn test_parse_price_change() {
        let json = r#"{
            "type": "price_change",
            "data": {
                "market": "mkt1",
                "bestBid": "0.49",
                "bestAsk": "0.51"
            }
        }"#;
        let ev = parse_clob_message(json).expect("should parse");
        match ev {
            ClobEvent::PriceUpdate(p) => {
                assert_eq!(p.market, "mkt1");
                assert!((p.best_bid - 0.49).abs() < f64::EPSILON);
                assert!((p.best_ask - 0.51).abs() < f64::EPSILON);
            }
            _ => panic!("expected PriceUpdate"),
        }
    }

    #[test]
    fn test_parse_price_change_snake_case() {
        let json = r#"{
            "type": "book_update",
            "data": {
                "asset_id": "mkt1",
                "best_bid": "0.49",
                "best_ask": "0.51"
            }
        }"#;
        let ev = parse_clob_message(json).expect("should parse");
        assert!(matches!(ev, ClobEvent::PriceUpdate(_)));
    }

    #[test]
    fn test_parse_non_fill_message() {
        let json = r#"{"type": "heartbeat"}"#;
        assert!(parse_clob_message(json).is_none());
    }

    #[test]
    fn test_parse_price_update_rejects_zero() {
        let json = r#"{
            "type": "ticker",
            "data": {"market": "x", "bestBid": "0", "bestAsk": "0.1"}
        }"#;
        assert!(parse_clob_message(json).is_none());
    }
}
