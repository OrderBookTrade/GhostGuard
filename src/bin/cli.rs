use anyhow::Result;
use clap::Parser;
use ethers::types::H256;
use ghostguard::config::FileConfig;
use ghostguard::{Config, FillVerdict, GhostGuard};
use std::fs::{create_dir_all, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "ghostguard")]
#[command(about = "Detect ghost fills on Polymarket CLOB")]
#[command(version)]
struct Args {
    /// Path to TOML config file. Values here are overridden by explicit CLI flags.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Polygon JSON-RPC URL. Alternatives if polygon-rpc.com fails:
    /// https://polygon.drpc.org, https://polygon.llamarpc.com,
    /// https://rpc.ankr.com/polygon, https://polygon-bor-rpc.publicnode.com
    #[arg(long)]
    rpc: Option<String>,

    /// Polymarket CLOB WebSocket URL
    #[arg(long)]
    clob_ws: Option<String>,

    /// Webhook URL to POST fill verdicts to
    #[arg(long)]
    webhook: Option<String>,

    /// Verification timeout in seconds
    #[arg(long)]
    timeout: Option<u64>,

    /// Poll interval in milliseconds
    #[arg(long)]
    poll_ms: Option<u64>,

    /// Verify a single tx hash and exit (for testing)
    #[arg(long)]
    verify_tx: Option<String>,

    /// Comma-separated list of market / asset IDs to monitor.
    #[arg(long, value_delimiter = ',')]
    markets: Vec<String>,

    /// Enable predictive ghost fill scoring
    #[arg(long)]
    predictive: bool,

    /// Risk threshold for predictive warnings (0.0-1.0)
    #[arg(long)]
    predictive_threshold: Option<f64>,

    /// Rolling window size for average trade size per market
    #[arg(long)]
    avg_window: Option<usize>,

    /// Path to JSONL verdict log. Empty string disables.
    #[arg(long)]
    verdict_log: Option<String>,

    /// Path to JSONL predictive warning log. Empty string disables.
    #[arg(long)]
    predictive_log: Option<String>,

    /// Launch the ratatui dashboard instead of stdout output
    #[arg(long)]
    tui: bool,

    /// Enable auto market rotation (follows short-cycle markets like btc-updown-5m).
    #[arg(long)]
    rotation: bool,

    /// Market slug prefix to auto-follow when rotation is enabled.
    #[arg(long)]
    rotation_pattern: Option<String>,
}

const TUI_LOG_PATH: &str = "data/ghostguard.log";

fn open_tui_log() -> Option<std::fs::File> {
    let path = Path::new(TUI_LOG_PATH);
    if let Some(parent) = path.parent() {
        let _ = create_dir_all(parent);
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Tracing setup:
    //  - Non-TUI mode: write to stderr at info level.
    //  - TUI mode: route tracing to a log file (stderr would corrupt the alt
    //    screen). Default level = info, override via RUST_LOG.
    if args.tui {
        if let Some(file) = open_tui_log() {
            tracing_subscriber::fmt()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .init();
            eprintln!("TUI logs: {TUI_LOG_PATH}  (tail -f to debug)");
        }
    } else {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .init();
    }

    // 1. Start with Config::default()
    let mut config = Config::default();

    // 2. Apply TOML if provided
    if let Some(ref path) = args.config {
        let file = FileConfig::load(path).await?;
        config = file.apply_to(config);
    }

    // 3. Apply explicit CLI flags (they override TOML)
    if let Some(v) = args.rpc.clone() {
        config.rpc_url = v;
    }
    if let Some(v) = args.clob_ws.clone() {
        config.clob_ws_url = v;
    }
    if let Some(v) = args.webhook.clone() {
        config.webhook_url = Some(v);
    }
    if let Some(v) = args.timeout {
        config.verify_timeout = Duration::from_secs(v);
    }
    if let Some(v) = args.poll_ms {
        config.poll_interval = Duration::from_millis(v);
    }
    if !args.markets.is_empty() {
        config.markets = args.markets.clone();
    }
    if args.predictive {
        config.predictive_enabled = true;
    }
    if let Some(v) = args.predictive_threshold {
        config.predictive_threshold = v;
    }
    if let Some(v) = args.avg_window {
        config.avg_window = v;
    }
    if let Some(v) = args.verdict_log.clone() {
        config.verdict_log = v;
    }
    if let Some(v) = args.predictive_log.clone() {
        config.predictive_log = v;
    }
    if args.tui {
        config.tui_mode = true;
    }
    if args.rotation {
        config.rotation_enabled = true;
    }
    if let Some(v) = args.rotation_pattern.clone() {
        config.rotation_pattern = v;
    }

    // Bootstrap: when rotation is on and we have no markets configured,
    // query Gamma for the current active rotating market and seed the
    // initial asset_ids. This way the TUI doesn't need to wait for the
    // next `new_market` event (up to 5 minutes away).
    if config.rotation_enabled && config.markets.is_empty() && args.verify_tx.is_none() {
        match bootstrap_current_market(&config.rotation_pattern).await {
            Ok(Some((slug, ids))) => {
                eprintln!("Bootstrap: current cycle = {slug} ({} token(s))", ids.len());
                config.markets = ids;
            }
            Ok(None) => {
                eprintln!(
                    "Bootstrap: no active market found for pattern '{}'",
                    config.rotation_pattern
                );
            }
            Err(e) => {
                eprintln!("Bootstrap failed: {e} — continuing without initial markets");
            }
        }
    }

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
    let tui_mode = config.tui_mode;

    if !tui_mode {
        println!("Starting GhostGuard sidecar...");
        println!("  RPC:        {}", config.rpc_url);
        println!("  CLOB WS:    {}", config.clob_ws_url);
        if !config.markets.is_empty() {
            println!("  Markets:    {}", config.markets.join(", "));
        }
        if let Some(ref wh) = config.webhook_url {
            println!("  Webhook:    {wh}");
        }
        if config.predictive_enabled {
            println!(
                "  Predictive: ENABLED (threshold={:.2}, window={})",
                config.predictive_threshold, config.avg_window
            );
        }
        if !config.verdict_log.is_empty() {
            println!("  Verdicts:   {} (JSONL)", config.verdict_log);
        }
        if !config.predictive_log.is_empty() && config.predictive_enabled {
            println!("  Warnings:   {} (JSONL)", config.predictive_log);
        }
        println!();
    }

    let mut guard = GhostGuard::new(config);

    // In TUI mode the dashboard owns the terminal — skip println callbacks
    // so they don't corrupt the alt screen. The TUI gets events via its own
    // internal channel.
    if !tui_mode {
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

        guard.on_predictive_warning(|w| {
            println!(
                "[WARN score={:.2}] tx={:?} market={} price_dev={:.3} size_anom={:.2}",
                w.score, w.tx_hash, w.market, w.price_deviation, w.size_anomaly,
            );
        });
    }

    guard.start().await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Gamma API bootstrap
// ---------------------------------------------------------------------------

/// Fetch the currently-active rotating market from Polymarket's Gamma API.
///
/// Returns `Ok(Some((slug, asset_ids)))` for the first non-closed market whose
/// slug starts with `pattern` (e.g. `"btc-updown-5m"`). This is the current
/// 5-minute cycle. Returns `Ok(None)` if nothing active matches.
///
/// The call is bounded by a 5-second timeout; failures are non-fatal — the
/// caller continues without initial markets and relies on the next
/// `new_market` WS event to populate subscriptions.
async fn bootstrap_current_market(pattern: &str) -> anyhow::Result<Option<(String, Vec<String>)>> {
    use std::time::Duration as StdDuration;

    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(5))
        .build()?;

    // Ask for a healthy batch of active markets; we filter client-side so we
    // don't depend on undocumented query params being honoured.
    let url = "https://gamma-api.polymarket.com/markets?closed=false&active=true&limit=50&order=volume24hr&ascending=false";
    let resp = client.get(url).send().await?.error_for_status()?;
    let markets: serde_json::Value = resp.json().await?;

    let arr = markets.as_array().cloned().unwrap_or_default();
    for m in arr {
        let slug = m.get("slug").and_then(|s| s.as_str()).unwrap_or("");
        if !slug.starts_with(pattern) {
            continue;
        }

        // `clobTokenIds` in Gamma is a JSON-encoded STRING holding an array of IDs.
        let raw = m
            .get("clobTokenIds")
            .and_then(|v| v.as_str())
            .unwrap_or("[]");
        let ids: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
        if ids.is_empty() {
            continue;
        }
        return Ok(Some((slug.to_string(), ids)));
    }

    Ok(None)
}
