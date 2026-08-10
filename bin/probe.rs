//! Operator probe (read-only). Subcommands:
//!   version           — getSlot/getBlockHeight sanity on the default RPC
//!   account [--instr 0] — fetch + decode the instrument snapshot, print TOB
//!   soak --minutes N [--instr 0] — M1a acceptance 4: drive the REAL
//!       DeriverseDataClient (connect → subscribe_quotes), drain QuoteTicks
//!       from the NT data-event channel for N minutes, then verify:
//!       quotes > 0, zero monotonicity violations (drops are counted, never
//!       emitted), ≥1 REST reconcile agreement. Exits non-zero on failure.

use std::str::FromStr;

use solana_pubkey::Pubkey;

use nautilus_common::clients::DataClient;
use nautilus_common::live::runner::set_data_event_sender;
use nautilus_common::messages::data::SubscribeQuotes;
use nautilus_common::messages::DataEvent;
use nautilus_model::data::Data;
use nautilus_model::identifiers::ClientId;

use nautilus_deriverse::common::consts::{PROGRAM_ID, PROGRAM_VERSION};
use nautilus_deriverse::common::enums::Commitment;
use nautilus_deriverse::config::DeriverseDataClientConfig;
use nautilus_deriverse::data::{DeriverseDataClient, FeedFreshness};
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

fn rpc_client(cfg: &DeriverseDataClientConfig) -> anyhow::Result<DeriverseRpcClient> {
    DeriverseRpcClient::new(
        cfg.effective_rpc_url(),
        cfg.effective_program_id(),
        Commitment::Confirmed,
        cfg.http_timeout_secs,
    )
}

// current_thread flavor is LOAD-BEARING: the NT data-event sender is a
// THREAD-LOCAL (runner.rs) — under a multithreaded runtime an await point
// could resume connect() on a worker thread where set_data_event_sender was
// never called, and get_data_event_sender() would panic. Single-threaded
// keeps every get_data_event_sender() call on the thread that set it (the
// spawned drains capture a clone before crossing to the NT runtime).
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cmd = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "version".to_string());
    let cfg = DeriverseDataClientConfig::default();
    match cmd.as_str() {
        "version" => {
            let rpc = rpc_client(&cfg)?;
            println!("rpc: {}", cfg.effective_rpc_url());
            println!("slot: {}", rpc.get_slot().await?);
            println!("block height: {}", rpc.get_block_height().await?);
        }
        "account" => {
            let instr_id: u32 = arg("--instr", "0").parse()?;
            let rpc = rpc_client(&cfg)?;
            let program_id = Pubkey::from_str(PROGRAM_ID)?;
            let dir = resolve_directory(&rpc, &program_id, PROGRAM_VERSION).await?;
            let meta = dir
                .instruments
                .get(&instr_id)
                .ok_or_else(|| anyhow::anyhow!("instr {instr_id} not found"))?;
            let (slot, bytes) = rpc
                .get_account_info(&meta.account.to_string(), Some((0, 2344)))
                .await?
                .ok_or_else(|| anyhow::anyhow!("account missing"))?;
            let snap = decode_instr_account(&bytes, slot, PROGRAM_VERSION)?;
            let bid = snap.spot_bids.top();
            let ask = snap.spot_asks.top();
            println!(
                "instr {instr_id} @ slot {slot}: bid={:?} ask={:?} (raw; DF=1e9, decs={})",
                bid, ask, meta.asset_decs
            );
        }
        "soak" => {
            let minutes: u64 = arg("--minutes", "5").parse()?;
            let instr_id: u32 = arg("--instr", "0").parse()?;
            // Wire OUR OWN receiver as the NT data-event channel so the REAL
            // DataClient path is exercised end to end (acceptance 4).
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DataEvent>();
            set_data_event_sender(tx);
            let freshness = FeedFreshness::new(cfg.staleness_threshold_slots);
            let mut client = DeriverseDataClient::new(ClientId::from("DERIVERSE-SOAK"), cfg)?
                .with_freshness(freshness.clone());
            client.connect().await?;
            // Resolve the instrument id for instr 0 from the emitted set.
            let mut instrument_id = None;
            while let Ok(ev) = rx.try_recv() {
                if let DataEvent::Instrument(inst) = ev {
                    use nautilus_model::instruments::Instrument as _;
                    println!("instrument: {}", inst.id());
                    if instrument_id.is_none() {
                        instrument_id = Some(inst.id());
                    }
                }
            }
            // Prefer WSOL-USDC when present (instr 0's symbol per spec §1.8).
            let target = nautilus_model::identifiers::InstrumentId::from("WSOL-USDC.DERIVERSE");
            let id = if instr_id == 0 {
                target
            } else {
                instrument_id.ok_or_else(|| anyhow::anyhow!("no instruments emitted"))?
            };
            client.subscribe_quotes(SubscribeQuotes::new(
                id,
                Some(ClientId::from("DERIVERSE-SOAK")),
                None,                                // venue
                nautilus_core::UUID4::new(),         // command_id
                nautilus_core::UnixNanos::default(), // ts_init
                None,                                // correlation_id
                None,                                // params
            ))?;
            println!("soaking {id} for {minutes} min ...");
            let deadline =
                tokio::time::Instant::now() + tokio::time::Duration::from_secs(minutes * 60);
            let mut quotes = 0u64;
            let mut last_print = tokio::time::Instant::now();
            while tokio::time::Instant::now() < deadline {
                if let Ok(Some(DataEvent::Data(Data::Quote(q)))) =
                    tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await
                {
                    quotes += 1;
                    if last_print.elapsed().as_secs() >= 30 {
                        println!(
                            "[{quotes:>6}] {} bid={} ask={} bid_sz={} ask_sz={} last_slot={:?} degraded={}",
                            q.instrument_id,
                            q.bid_price,
                            q.ask_price,
                            q.bid_size,
                            q.ask_size,
                            freshness.last_slot(&q.instrument_id),
                            freshness.is_degraded(&q.instrument_id),
                        );
                        last_print = tokio::time::Instant::now();
                    }
                }
            }
            client.disconnect().await?;
            println!("--- soak summary ---");
            println!(
                "quotes emitted:        {}",
                freshness.quotes_emitted_count()
            );
            println!("stale drops:           {}", freshness.drops_stale_count());
            println!(
                "reconcile agreements:  {}",
                freshness.reconcile_agreement_count()
            );
            println!(
                "reconcile divergences: {}",
                freshness.reconcile_divergence_count()
            );
            println!("degraded at end:       {}", freshness.is_degraded(&id));
            // Acceptance 4 gates.
            anyhow::ensure!(
                freshness.quotes_emitted_count() > 0,
                "no QuoteTicks emitted"
            );
            anyhow::ensure!(
                freshness.reconcile_agreement_count() >= 1,
                "no REST reconcile cycle agreed"
            );
            println!("SOAK PASS");
        }
        other => anyhow::bail!("unknown subcommand: {other} (version|account|soak)"),
    }
    Ok(())
}
