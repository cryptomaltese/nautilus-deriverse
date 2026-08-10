//! Fixture capture (M1a acceptance 3): fetch the WSOL/USDC (instr 0)
//! account's 2344-byte snapshot region from the default mainnet RPC and
//! write it (+ sidecar) under tests/fixtures/. Read-only; no credentials.
//!
//! Usage: drv-capture [--instr 0] [--out-dir crates/adapters/deriverse/tests/fixtures]

use std::str::FromStr;

use solana_pubkey::Pubkey;

use nautilus_deriverse::common::consts::{INSTR_SNAPSHOT_LEN, PROGRAM_ID, PROGRAM_VERSION};
use nautilus_deriverse::common::enums::Commitment;
use nautilus_deriverse::config::DeriverseDataClientConfig;
use nautilus_deriverse::decode::instr::decode_instr_account;
use nautilus_deriverse::rpc::{DeriverseRpcClient, SolanaRpc};
use nautilus_deriverse::tx::accounts::resolve_directory;

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let instr_id: u32 = arg("--instr", "0").parse()?;
    let out_dir = arg("--out-dir", "crates/adapters/deriverse/tests/fixtures");
    let cfg = DeriverseDataClientConfig::default();
    let rpc = DeriverseRpcClient::new(
        cfg.effective_rpc_url(),
        cfg.effective_program_id(),
        Commitment::Confirmed,
        cfg.http_timeout_secs,
    )?;
    let program_id = Pubkey::from_str(PROGRAM_ID)?;
    let dir = resolve_directory(&rpc, &program_id, PROGRAM_VERSION).await?;
    let meta = dir
        .instruments
        .get(&instr_id)
        .ok_or_else(|| anyhow::anyhow!("instr {instr_id} not found on mainnet"))?;
    let (slot, bytes) = rpc
        .get_account_info(
            &meta.account.to_string(),
            Some((0, INSTR_SNAPSHOT_LEN as u64)),
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("instrument account missing"))?;
    // Sanity before writing: it must decode with a two-sided spot book.
    let snap = decode_instr_account(&bytes, slot, PROGRAM_VERSION)?;
    anyhow::ensure!(
        snap.spot_bids.len > 0 && snap.spot_asks.len > 0,
        "captured book is one-sided; re-run during active quoting"
    );
    std::fs::write(format!("{out_dir}/instr{instr_id}_mainnet.bin"), &bytes)?;
    std::fs::write(
        format!("{out_dir}/instr{instr_id}_mainnet.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "pubkey": meta.account.to_string(),
            "slot": slot,
            "len": bytes.len(),
            "captured_at_unix": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?.as_secs(),
            "asset_decs": meta.asset_decs,
        }))?,
    )?;
    println!(
        "captured instr {instr_id}: {} bytes at slot {slot}; spot TOB raw bid={} ask={}",
        bytes.len(),
        snap.spot_bids.top().unwrap().px,
        snap.spot_asks.top().unwrap().px,
    );
    Ok(())
}
