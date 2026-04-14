use anyhow::Result;
use ethers::types::H256;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// A fill event extracted from the CLOB websocket.
#[derive(Debug, Clone)]
pub struct ClobFill {
    pub tx_hash: H256,
    pub market: String,
    pub side: String,
    pub size: f64,
    pub price: f64,
}

/// Raw JSON shape of a CLOB fill message.
#[derive(Debug, Deserialize)]
struct ClobMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    data: Option<ClobFillData>,
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
}

impl ClobFillData {
    fn tx_hash_str(&self) -> Option<&str> {
        self.transaction_hash
            .as_deref()
            .or(self.transaction_hash_alt.as_deref())
    }
}

/// Connect to the Polymarket CLOB WebSocket and stream fill events.
///
/// Sends each extracted fill into `tx`. Reconnects on disconnect.
pub async fn listen_clob_fills(ws_url: &str, tx: mpsc::Sender<ClobFill>) -> Result<()> {
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
                if let Err(e) = write.send(Message::Text(subscribe_msg.to_string())).await {
                    error!(error = %e, "failed to send subscribe message");
                    continue;
                }

                while let Some(msg_result) = read.next().await {
                    match msg_result {
                        Ok(Message::Text(text)) => {
                            if let Some(fill) = parse_fill_message(&text) {
                                debug!(?fill.tx_hash, "received fill from CLOB");
                                if tx.send(fill).await.is_err() {
                                    info!("fill channel closed, shutting down ws listener");
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

/// Parse a raw CLOB websocket message into a ClobFill if it contains a fill.
fn parse_fill_message(text: &str) -> Option<ClobFill> {
    let msg: ClobMessage = serde_json::from_str(text).ok()?;

    // Only process fill/trade messages
    let msg_type = msg.msg_type.as_deref().unwrap_or("");
    if !matches!(msg_type, "fill" | "trade" | "order_fill") {
        return None;
    }

    let data = msg.data?;
    let tx_hash_str = data.tx_hash_str()?;

    let tx_hash: H256 = tx_hash_str
        .parse()
        .inspect_err(|e| {
            warn!(tx_hash = tx_hash_str, error = %e, "invalid tx hash in fill");
        })
        .ok()?;

    Some(ClobFill {
        tx_hash,
        market: data.market.unwrap_or_default(),
        side: data.side.unwrap_or_default(),
        size: data.size.and_then(|s| s.parse().ok()).unwrap_or(0.0),
        price: data.price.and_then(|s| s.parse().ok()).unwrap_or(0.0),
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

        let fill = parse_fill_message(json).expect("should parse fill");
        assert_eq!(
            format!("{:?}", fill.tx_hash),
            "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0"
        );
        assert_eq!(fill.market, "0xabc123");
        assert_eq!(fill.side, "BUY");
        assert!((fill.size - 100.0).abs() < f64::EPSILON);
        assert!((fill.price - 0.65).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_non_fill_message() {
        let json = r#"{"type": "heartbeat"}"#;
        assert!(parse_fill_message(json).is_none());
    }
}
