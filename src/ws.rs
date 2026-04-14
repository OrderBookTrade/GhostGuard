use anyhow::Result;
use ethers::types::{Address, H256};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// Events from the CLOB websocket.
#[derive(Debug, Clone)]
pub enum ClobEvent {
    Fill(ClobFill),
    PriceUpdate(PriceUpdate),
}

/// A fill event extracted from the CLOB websocket.
#[derive(Debug, Clone)]
pub struct ClobFill {
    pub tx_hash: H256,
    pub market: String,
    pub side: String,
    pub size: f64,
    pub price: f64,
    /// Taker address, if present in the websocket message.
    pub taker: Option<Address>,
}

/// A mid-price update from the CLOB websocket.
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub market: String,
    pub best_bid: f64,
    pub best_ask: f64,
}

/// Raw JSON shape of a CLOB message.
#[derive(Debug, Deserialize)]
struct ClobMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    data: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ClobFillData {
    #[serde(rename = "transactionHash")]
    transaction_hash: Option<String>,
    #[serde(rename = "transaction_hash")]
    transaction_hash_alt: Option<String>,
    market: Option<String>,
    side: Option<String>,
    size: Option<String>,
    price: Option<String>,
    taker: Option<String>,
    #[serde(rename = "takerAddress")]
    taker_address: Option<String>,
}

impl ClobFillData {
    fn tx_hash_str(&self) -> Option<&str> {
        self.transaction_hash
            .as_deref()
            .or(self.transaction_hash_alt.as_deref())
    }

    fn taker_addr(&self) -> Option<Address> {
        self.taker
            .as_deref()
            .or(self.taker_address.as_deref())
            .and_then(|s| s.parse().ok())
    }
}

#[derive(Debug, Deserialize)]
struct ClobPriceData {
    market: Option<String>,
    #[serde(rename = "bestBid")]
    best_bid: Option<String>,
    #[serde(rename = "bestAsk")]
    best_ask: Option<String>,
    #[serde(rename = "best_bid")]
    best_bid_alt: Option<String>,
    #[serde(rename = "best_ask")]
    best_ask_alt: Option<String>,
    // Some messages use bid/ask directly
    bid: Option<String>,
    ask: Option<String>,
}

impl ClobPriceData {
    fn best_bid_val(&self) -> Option<f64> {
        self.best_bid
            .as_deref()
            .or(self.best_bid_alt.as_deref())
            .or(self.bid.as_deref())
            .and_then(|s| s.parse().ok())
    }

    fn best_ask_val(&self) -> Option<f64> {
        self.best_ask
            .as_deref()
            .or(self.best_ask_alt.as_deref())
            .or(self.ask.as_deref())
            .and_then(|s| s.parse().ok())
    }
}

/// Connect to the Polymarket CLOB WebSocket and stream events.
///
/// Sends fills and price updates into `tx`. Reconnects on disconnect.
pub async fn listen_clob_events(
    ws_url: &str,
    tx: mpsc::Sender<ClobEvent>,
) -> Result<()> {
    loop {
        info!(url = ws_url, "connecting to CLOB websocket...");

        match connect_async(ws_url).await {
            Ok((ws_stream, _)) => {
                info!("CLOB websocket connected");
                let (mut write, mut read) = ws_stream.split();

                // Subscribe to the user fill channel
                let subscribe_msg = serde_json::json!({
                    "type": "subscribe",
                    "channel": "user",
                    "markets": [],
                });
                if let Err(e) = write
                    .send(Message::Text(subscribe_msg.to_string()))
                    .await
                {
                    error!(error = %e, "failed to send subscribe message");
                    continue;
                }

                while let Some(msg_result) = read.next().await {
                    match msg_result {
                        Ok(Message::Text(text)) => {
                            if let Some(event) = parse_clob_message(&text) {
                                match &event {
                                    ClobEvent::Fill(f) => {
                                        debug!(?f.tx_hash, "received fill from CLOB");
                                    }
                                    ClobEvent::PriceUpdate(p) => {
                                        debug!(market = %p.market, "received price update");
                                    }
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
            }
        }

        warn!("reconnecting to CLOB websocket in 3s...");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// Parse a raw CLOB websocket message into a ClobEvent.
fn parse_clob_message(text: &str) -> Option<ClobEvent> {
    let msg: ClobMessage = serde_json::from_str(text).ok()?;
    let msg_type = msg.msg_type.as_deref().unwrap_or("");
    let data = msg.data?;

    match msg_type {
        "fill" | "trade" | "order_fill" => {
            parse_fill_from_value(&data).map(ClobEvent::Fill)
        }
        "price_change" | "book_update" | "ticker" => {
            parse_price_from_value(&data).map(ClobEvent::PriceUpdate)
        }
        _ => None,
    }
}

fn parse_fill_from_value(data: &serde_json::Value) -> Option<ClobFill> {
    let fill_data: ClobFillData = serde_json::from_value(data.clone()).ok()?;
    let tx_hash_str = fill_data.tx_hash_str()?;

    let tx_hash: H256 = tx_hash_str
        .parse()
        .inspect_err(|e| {
            warn!(tx_hash = tx_hash_str, error = %e, "invalid tx hash in fill");
        })
        .ok()?;

    let taker = fill_data.taker_addr();

    Some(ClobFill {
        tx_hash,
        market: fill_data.market.unwrap_or_default(),
        side: fill_data.side.unwrap_or_default(),
        size: fill_data.size.and_then(|s| s.parse().ok()).unwrap_or(0.0),
        price: fill_data.price.and_then(|s| s.parse().ok()).unwrap_or(0.0),
        taker,
    })
}

fn parse_price_from_value(data: &serde_json::Value) -> Option<PriceUpdate> {
    let price_data: ClobPriceData = serde_json::from_value(data.clone()).ok()?;
    let market = price_data.market.clone()?;
    let best_bid = price_data.best_bid_val()?;
    let best_ask = price_data.best_ask_val()?;

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

        let event = parse_clob_message(json).expect("should parse fill");
        match event {
            ClobEvent::Fill(fill) => {
                assert_eq!(
                    format!("{:?}", fill.tx_hash),
                    "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0"
                );
                assert_eq!(fill.market, "0xabc123");
                assert_eq!(fill.side, "BUY");
                assert!((fill.size - 100.0).abs() < f64::EPSILON);
                assert!((fill.price - 0.65).abs() < f64::EPSILON);
                assert!(fill.taker.is_none());
            }
            _ => panic!("expected Fill event"),
        }
    }

    #[test]
    fn test_parse_fill_with_taker() {
        let json = r#"{
            "type": "fill",
            "data": {
                "transactionHash": "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0",
                "market": "0xabc123",
                "side": "BUY",
                "size": "100.0",
                "price": "0.65",
                "taker": "0xcf23977659940be739745d0c1486cc11d0eaf73d"
            }
        }"#;

        let event = parse_clob_message(json).expect("should parse fill");
        match event {
            ClobEvent::Fill(fill) => {
                assert!(fill.taker.is_some());
            }
            _ => panic!("expected Fill event"),
        }
    }

    #[test]
    fn test_parse_price_update() {
        let json = r#"{
            "type": "price_change",
            "data": {
                "market": "0xabc123",
                "bestBid": "0.48",
                "bestAsk": "0.52"
            }
        }"#;

        let event = parse_clob_message(json).expect("should parse price update");
        match event {
            ClobEvent::PriceUpdate(p) => {
                assert_eq!(p.market, "0xabc123");
                assert!((p.best_bid - 0.48).abs() < f64::EPSILON);
                assert!((p.best_ask - 0.52).abs() < f64::EPSILON);
            }
            _ => panic!("expected PriceUpdate event"),
        }
    }

    #[test]
    fn test_parse_non_fill_message() {
        let json = r#"{"type": "heartbeat"}"#;
        assert!(parse_clob_message(json).is_none());
    }
}
