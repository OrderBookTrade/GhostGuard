use anyhow::Result;
use ethers::types::H256;
use ghostguard::{verify_fill, Config, FillVerdict};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config {
        rpc_url: "https://polygon-rpc.com".into(),
        verify_timeout: Duration::from_secs(30),
        poll_interval: Duration::from_millis(500),
        ..Default::default()
    };

    // Known ghost fill tx — wallet drain attack, TRANSFER_FROM_FAILED
    let ghost_tx: H256 =
        "0x9e3230abde0f569da87511a6f8823076f7b211bb00d10689db3b7c50d6652df0".parse()?;

    println!("Verifying known ghost fill: {ghost_tx:?}\n");

    let verdict = verify_fill(&config.rpc_url, ghost_tx, &config).await?;

    match &verdict {
        FillVerdict::Real { tx_hash, block } => {
            println!("REAL fill confirmed at block {block}");
            println!("  tx: {tx_hash:?}");
        }
        FillVerdict::Ghost {
            tx_hash,
            reason,
            counterparty,
        } => {
            println!("GHOST fill detected!");
            println!("  tx:     {tx_hash:?}");
            println!("  reason: {reason}");
            if let Some(cp) = counterparty {
                println!("  counterparty: {cp:?}");
            }
        }
        FillVerdict::Timeout { tx_hash } => {
            println!("TIMEOUT — no receipt for {tx_hash:?}");
        }
    }

    println!("\nis_ghost = {}", verdict.is_ghost());

    Ok(())
}
