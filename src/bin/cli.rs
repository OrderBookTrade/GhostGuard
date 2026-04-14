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
    #[arg(long, default_value = "wss://ws-subscriptions-clob.polymarket.com/ws/market")]
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

    /// Enable predictive ghost fill scoring
    #[arg(long)]
    predictive: bool,

    /// Risk threshold for predictive warnings (0.0-1.0)
    #[arg(long, default_value = "0.7")]
    risk_threshold: f64,

    /// Enable wallet history lookups for predictive scoring
    #[arg(long)]
    wallet_lookup: bool,

    /// Rolling window size for market statistics
    #[arg(long, default_value = "100")]
    stats_window: usize,
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
        predictive_enabled: args.predictive,
        risk_threshold: args.risk_threshold,
        wallet_lookup_enabled: args.wallet_lookup,
        stats_window_size: args.stats_window,
        ..Default::default()
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

    // Sidecar mode
    println!("Starting GhostGuard sidecar...");
    println!("  RPC:        {}", args.rpc);
    println!("  CLOB WS:    {}", args.clob_ws);
    if let Some(ref wh) = args.webhook {
        println!("  Webhook:    {wh}");
    }
    if args.predictive {
        println!("  Predictive: ENABLED (threshold={:.2})", args.risk_threshold);
        if args.wallet_lookup {
            println!("  Wallet:     lookup enabled");
        }
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

    guard.on_high_risk(|risk| {
        println!(
            "[HIGH RISK {:.2}] tx={:?} market={} price_dev={:.2} size_anom={:.2} wallet={:?}",
            risk.score, risk.tx_hash, risk.market,
            risk.factors.price_deviation, risk.factors.size_anomaly,
            risk.factors.wallet_risk,
        );
    });

    guard.start().await?;

    Ok(())
}
