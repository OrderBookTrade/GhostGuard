use anyhow::Result;
use clap::Parser;
use ethers::types::H256;
use ghostguard::{Config, FillVerdict, GhostGuard};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "ghostguard")]
#[command(about = "Detect ghost fills on Polymarket CLOB")]
#[command(version)]
struct Args {
    /// Polygon JSON-RPC URL
    #[arg(long, default_value = "https://polygon-rpc.com")]
    rpc: String,

    /// Polymarket CLOB WebSocket URL
    #[arg(
        long,
        default_value = "wss://ws-subscriptions-clob.polymarket.com/ws/market"
    )]
    clob_ws: String,

    /// Webhook URL to POST fill verdicts to
    #[arg(long)]
    webhook: Option<String>,

    /// Verification timeout in seconds
    #[arg(long, default_value = "10")]
    timeout: u64,

    /// Poll interval in milliseconds
    #[arg(long, default_value = "500")]
    poll_ms: u64,

    /// Verify a single tx hash and exit (for testing)
    #[arg(long)]
    verify_tx: Option<String>,

    /// Comma-separated list of market IDs to monitor. If empty, subscribes to user channel.
    #[arg(long, value_delimiter = ',')]
    markets: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let config = Config {
        rpc_url: args.rpc.clone(),
        clob_ws_url: args.clob_ws.clone(),
        verify_timeout: Duration::from_secs(args.timeout),
        poll_interval: Duration::from_millis(args.poll_ms),
        webhook_url: args.webhook.clone(),
        markets: args.markets.clone(),
    };

    // Single tx verification mode
    if let Some(tx_str) = args.verify_tx {
        let tx_hash: H256 = tx_str
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid tx hash: {e}"))?;

        println!("Verifying tx: {tx_hash:?}");
        let verdict = ghostguard::verify_fill(&config.rpc_url, tx_hash, &config).await?;

        match &verdict {
            FillVerdict::Real { tx_hash, block } => {
                println!("[REAL] tx={tx_hash:?} block={block}");
            }
            FillVerdict::Ghost {
                tx_hash,
                reason,
                counterparty,
            } => {
                println!("[GHOST] tx={tx_hash:?} reason={reason}");
                if let Some(cp) = counterparty {
                    println!("        counterparty={cp:?}");
                }
            }
            FillVerdict::Timeout { tx_hash } => {
                println!("[TIMEOUT] tx={tx_hash:?} — treating as ghost");
            }
        }

        return Ok(());
    }

    // Sidecar mode: listen to CLOB websocket and verify fills
    println!("Starting GhostGuard sidecar...");
    println!("  RPC:     {}", args.rpc);
    println!("  CLOB WS: {}", args.clob_ws);
    if let Some(ref wh) = args.webhook {
        println!("  Webhook: {wh}");
    }
    println!();

    let mut guard = GhostGuard::new(config);

    guard.on_real_fill(|verdict| {
        if let FillVerdict::Real { tx_hash, block } = verdict {
            println!("[REAL] tx={tx_hash:?} block={block}");
        }
    });

    guard.on_ghost_fill(|event| {
        println!(
            "[GHOST] tx={:?} reason={} market={} side={} size={} price={}",
            event.tx_hash, event.reason, event.market, event.side, event.size, event.price,
        );
        if let Some(cp) = event.counterparty {
            println!("        counterparty={cp:?}");
        }
    });

    guard.start().await?;

    Ok(())
}
