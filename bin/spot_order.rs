//! Attended spot-order rungs (M1a acceptance 5–6) + qty-targeted flatten.
//!
//!   place  --keypair <path> --instr 0 --side bid|ask
//!          sizing (exactly one):
//!            --notional-usd 5     qty recomputed from the live book (opens)
//!            --qty 0.033          exact qty, floored to the size step;
//!                                 capped at qty x price <= $6 unless
//!                                 --allow-large
//!            --flatten            sell the ENTIRE venue-held asset balance
//!                                 (--side ask; implies --marketable --ioc;
//!                                 cap-exempt — an exit is never blocked)
//!          [--offset-pct -30]   off-market limit (default)
//!          [--marketable --ioc] crossing IOC (acceptance 6)
//!   cancel --keypair <path> --instr 0 --side bid|ask --order-id <i64>
//!
//! Close/flatten flows QUANTITY-TARGET the venue's own state (client
//! primary account balance, decode/client.rs): the venue oversell-rejects
//! (InsufficientFunds 132) and never clamps, so recomputing a close qty
//! from notional ÷ a moved price is a reject waiting to happen (spec §1.4).
//!
//! Own-transaction truth (Invariant 4): the venue order_id and every
//! fill/cancel proof are decoded from the CONFIRMED transaction's own logs
//! fetched via signature polling — logsSubscribe is not consulted here.

use std::str::FromStr;
use std::sync::Arc;

use solana_pubkey::Pubkey;

use nautilus_deriverse::common::consts::{DF, USDC_MINT};
use nautilus_deriverse::config::DeriverseExecClientConfig;
use nautilus_deriverse::decode::client::{client_spot_token_balance, qty_target_from_balance};
use nautilus_deriverse::decode::instr::decode_instr_account;
use nautilus_deriverse::decode::logs::{decode_program_data_line, DeriverseLogEvent};
use nautilus_deriverse::rpc::{DeriverseRpcClient, SolanaRpc};
use nautilus_deriverse::tx::accounts::{resolve_directory, VenueContext};
use nautilus_deriverse::tx::build::{
    snap_qty_to_step_raw, NewSpotOrderArgs, ResolvedLut, SpotOrderCancelArgs, TxBuilder,
};
use nautilus_deriverse::tx::send::{BlockhashCache, SubmitOutcome, TxLedger, TxRecord, TxSender};

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

fn required(name: &str) -> anyhow::Result<String> {
    arg(name).ok_or_else(|| anyhow::anyhow!("missing required {name} <value>"))
}

// load_keypair / pubkey_of: identical to bin/setup.rs (bins are separate
// targets; duplicating these ~20 lines beats a pub-in-lib secrets helper).
fn load_keypair(path: &str) -> anyhow::Result<ed25519_dalek::SigningKey> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read keypair file {path}: {e}"))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw)
        .map_err(|_| anyhow::anyhow!("keypair file {path} is not a JSON byte array"))?;
    anyhow::ensure!(bytes.len() == 64, "keypair file must hold 64 bytes");
    let seed: [u8; 32] = bytes[..32].try_into().expect("seed slice");
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    anyhow::ensure!(
        key.verifying_key().to_bytes()[..] == bytes[32..],
        "keypair file pubkey half does not match its seed"
    );
    Ok(key)
}

fn pubkey_of(key: &ed25519_dalek::SigningKey) -> Pubkey {
    Pubkey::new_from_array(key.verifying_key().to_bytes())
}

fn print_own_reports(logs: &[String]) -> (Option<i64>, bool, bool) {
    let mut order_id = None;
    let mut saw_fill = false;
    let mut saw_cancel = false;
    for line in logs {
        match decode_program_data_line(line) {
            Some(DeriverseLogEvent::SpotPlaceOrder(r)) => {
                order_id = Some(r.order_id);
                println!(
                    "SpotPlaceOrderReport: order_id={} side={} qty={} price={} seq_no={}",
                    r.order_id, r.side, r.qty, r.price, r.seq_no
                );
            }
            Some(DeriverseLogEvent::SpotFillOrder(r)) => {
                saw_fill = true;
                println!(
                    "SpotFillOrderReport: order_id={} qty={} crncy={} price={} rebates={} seq_no={}",
                    r.order_id, r.qty, r.crncy, r.price, r.rebates, r.seq_no
                );
            }
            Some(DeriverseLogEvent::SpotOrderCancel(r)) => {
                saw_cancel = true;
                println!(
                    "SpotOrderCancelReport: order_id={} qty={} crncy={} seq_no={}",
                    r.order_id, r.qty, r.crncy, r.seq_no
                );
            }
            _ => {}
        }
    }
    (order_id, saw_fill, saw_cancel)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let keypair_path = required("--keypair")?;
    let instr_id: u32 = arg("--instr").unwrap_or_else(|| "0".into()).parse()?;
    let side_str = required("--side")?;
    let side: u8 = match side_str.as_str() {
        "bid" => 0,
        "ask" => 1,
        other => anyhow::bail!("--side must be bid|ask, got {other}"),
    };
    let cfg = DeriverseExecClientConfig {
        keypair_path: Some(keypair_path.clone().into()),
        ..Default::default()
    };
    let key = load_keypair(&keypair_path)?;
    let signer = pubkey_of(&key);
    let rpc = Arc::new(DeriverseRpcClient::new(
        cfg.effective_rpc_url(),
        cfg.effective_program_id(),
        cfg.commitment,
        cfg.http_timeout_secs,
    )?);
    let program_id = Pubkey::from_str(&cfg.effective_program_id())?;
    let dir = resolve_directory(rpc.as_ref(), &program_id, cfg.effective_program_version()).await?;
    let ctx = VenueContext::new(dir, signer);
    let meta = ctx
        .dir
        .instruments
        .get(&instr_id)
        .ok_or_else(|| anyhow::anyhow!("instr {instr_id} not found"))?
        .clone();
    // Sanity: M1a orders are USDC-quoted spot only.
    let crncy = ctx
        .dir
        .tokens
        .get(&meta.crncy_token_id)
        .ok_or_else(|| anyhow::anyhow!("no crncy"))?;
    anyhow::ensure!(
        crncy.mint == Pubkey::from_str(USDC_MINT)?,
        "instr {instr_id} is not USDC-quoted"
    );

    let builder = TxBuilder {
        ctx: &ctx,
        compute_unit_limit: cfg.compute_unit_limit,
        compute_unit_price_microlamports: cfg.priority_fee_microlamports,
    };
    let luts = vec![ResolvedLut {
        address: meta.lut_address,
        // Resolve the instrument LUT's address list live (ALT layout:
        // addresses start at byte 56, 32 B each).
        addresses: {
            match rpc
                .get_account_info(&meta.lut_address.to_string(), None)
                .await?
            {
                Some((_s, bytes)) if bytes.len() > 56 => bytes[56..]
                    .chunks_exact(32)
                    .map(|c| Pubkey::new_from_array(c.try_into().expect("32")))
                    .collect(),
                _ => Vec::new(), // no LUT ⇒ uncompressed v0 (still valid)
            }
        },
    }];

    let ledger: TxLedger<String> = TxLedger::new();
    let blockhash_cache = BlockhashCache::new(Arc::clone(&rpc), cfg.blockhash_refresh_secs);
    let sender = TxSender {
        rpc: Arc::clone(&rpc),
        poll_interval_ms: cfg.signature_poll_interval_ms,
    };

    match cmd.as_str() {
        "place" => {
            let notional_arg: Option<f64> = arg("--notional-usd").map(|v| v.parse()).transpose()?;
            let qty_arg: Option<f64> = arg("--qty").map(|v| v.parse()).transpose()?;
            let flatten = flag("--flatten");
            anyhow::ensure!(
                usize::from(notional_arg.is_some())
                    + usize::from(qty_arg.is_some())
                    + usize::from(flatten)
                    == 1,
                "exactly one sizing mode: --notional-usd | --qty | --flatten"
            );
            let marketable = flag("--marketable") || flatten;
            let ioc = flag("--ioc") || marketable;
            let offset_pct: f64 = arg("--offset-pct")
                .unwrap_or_else(|| "-30".into())
                .parse()?;
            // Live TOB for pricing.
            let (slot, bytes) = rpc
                .get_account_info(&meta.account.to_string(), Some((0, 2344)))
                .await?
                .ok_or_else(|| anyhow::anyhow!("instrument account missing"))?;
            let snap = decode_instr_account(&bytes, slot, cfg.effective_program_version())?;
            let bid = snap
                .spot_bids
                .top()
                .ok_or_else(|| anyhow::anyhow!("empty bid ladder"))?;
            let ask = snap
                .spot_asks
                .top()
                .ok_or_else(|| anyhow::anyhow!("empty ask ladder"))?;
            let (bid_px, ask_px) = (bid.px as f64 / DF as f64, ask.px as f64 / DF as f64);
            let price = if marketable {
                // Cross by 1%: a bid lifts the ask, an ask hits the bid.
                if side == 0 {
                    ask_px * 1.01
                } else {
                    bid_px * 0.99
                }
            } else {
                // Off-market by offset_pct from own-side TOB (negative =
                // below for bids; mirrored above for asks).
                if side == 0 {
                    bid_px * (1.0 + offset_pct / 100.0)
                } else {
                    ask_px * (1.0 - offset_pct / 100.0)
                }
            };
            let step = 10f64.powi(-(meta.asset_decs.min(9) as i32));
            // (display qty, wire-exact raw override). Flatten carries the
            // integer to the wire — the display-float round-trip can round
            // UP 1-2 raw units at large magnitudes (oversell ⇒ 132).
            let (qty, qty_raw_override) = if flatten {
                // Qty-target the VENUE's own holdings (spec §1.4): the venue
                // oversell-rejects (InsufficientFunds 132), never clamps.
                anyhow::ensure!(side == 1, "--flatten sells holdings: --side ask required");
                let (_s, acct) = rpc
                    .get_account_info(&ctx.client_primary.to_string(), None)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("client primary account missing — run setup first")
                    })?;
                let raw = client_spot_token_balance(
                    &acct,
                    cfg.effective_program_version(),
                    meta.asset_token_id,
                )?
                .unwrap_or(0);
                let (q, target_raw) = qty_target_from_balance(raw, meta.asset_decs, step)?;
                anyhow::ensure!(
                    target_raw > 0,
                    "nothing to flatten: venue balance {raw} (raw) for token {} rounds to zero",
                    meta.asset_token_id
                );
                println!(
                    "flatten: venue-held balance {raw} (raw) -> qty {q} (wire raw {target_raw})"
                );
                (q, Some(target_raw))
            } else if let Some(q) = qty_arg {
                // Explicit qty: snap to the size step in RAW-INTEGER space
                // (snap_qty_to_step_raw) — exact on clean k·step multiples
                // (relative epsilon rescues their representation ulp),
                // off-grid input floors DOWN, and the result never exceeds
                // the epsilon-floored intent encoding (a close qty above
                // holdings rejects, spec §1.4). The wire encodes the
                // snapped raw verbatim; a snap to zero falls into the
                // qty > 0 ensure below.
                let snapped_raw = snap_qty_to_step_raw(q, step, meta.asset_decs)?;
                let q = snapped_raw.max(0) as f64 / 10f64.powi(meta.asset_decs as i32);
                // Budget cap rides the same live price every mode already
                // uses (marketable price = the crossing bound). Flatten is
                // exempt — an exit is never blocked.
                let est = q * price;
                anyhow::ensure!(
                    est <= 6.0 || flag("--allow-large"),
                    "M1a budget cap: --qty x price ~= ${est:.2} > $6 (pass --allow-large to override)"
                );
                (q, Some(snapped_raw))
            } else {
                let notional = notional_arg.expect("sizing mode checked above");
                anyhow::ensure!(
                    notional > 0.0 && notional <= 6.0,
                    "M1a budget cap: notional <= $6"
                );
                (((notional / price) / step).round() * step, None)
            };
            anyhow::ensure!(qty > 0.0, "qty rounded to zero");
            let est_notional = qty * price;
            println!(
                "book: bid={bid_px} ask={ask_px} | placing side={side_str} ioc={ioc} price={price} qty={qty} (~${est_notional:.2})"
            );
            let order_args = NewSpotOrderArgs {
                instr_id,
                side,
                ioc: u8::from(ioc),
                order_type: 0,
                price,
                qty,
                edge_price: 0.0,
            };
            let ix = match qty_raw_override {
                // Wire-exact: the balance-derived integer, never the
                // display float (round-trip overshoot ⇒ oversell ⇒ 132).
                Some(raw) => builder.new_spot_order_ix_raw(&order_args, raw)?,
                None => builder.new_spot_order_ix(&order_args)?,
            };
            let (blockhash, lvbh) = blockhash_cache.get().await?;
            let tx = builder.build_signed_tx(&[ix], &key, &blockhash, &luts)?;
            let local_key = format!("place-{}", tx.signature_base58());
            ledger.insert_before_send(
                local_key.clone(),
                TxRecord {
                    signature: tx.signature_base58(),
                    venue_order_id: None,
                    last_valid_block_height: lvbh,
                },
            );
            println!("sig: {}", tx.signature_base58());
            // Ledger prune discipline (Invariant 11): TERMINAL outcomes only
            // — landed-but-failed, IOC (fills or dies, never rests), expiry,
            // definite reject. A confirmed RESTING order is NOT terminal:
            // its record (with the bound venue_order_id) stays until the
            // cancel/fill leg. Bins are one-shot so the retained record dies
            // with the process; M1b keys the long-lived ledger by
            // ClientOrderId and prunes on the same terminal set.
            match sender.submit_and_confirm(&tx, lvbh).await? {
                SubmitOutcome::Confirmed { slot, logs, err } => {
                    if let Some(err) = err {
                        for line in &logs {
                            println!("  {line}");
                        }
                        ledger.prune_terminal(&local_key);
                        anyhow::bail!(
                            "tx landed at slot {slot} but the program FAILED: {err} — \
                             no order placed (atomic tx, no state change); safe to re-run once"
                        );
                    }
                    println!("TxOutcome::Confirmed at slot {slot}");
                    let (order_id, saw_fill, _c) = print_own_reports(&logs);
                    if let Some(id) = order_id {
                        ledger.bind_order_id(&local_key, id);
                        println!("venue order_id bound: {id}");
                    }
                    if ioc {
                        if !saw_fill {
                            println!(
                                "IOC did not fill (book moved) — nothing rests, safe to re-run"
                            );
                        }
                        ledger.prune_terminal(&local_key); // IOC never rests: terminal
                    } else if order_id.is_none() {
                        // Confirmed clean but no place report: state unknown —
                        // NOT proven terminal, record retained (Invariant 11).
                        anyhow::bail!(
                            "no SpotPlaceOrderReport in own logs — investigate before retry"
                        );
                    }
                    // Resting order: record retained until its cancel/fill.
                }
                SubmitOutcome::Expired => {
                    ledger.prune_terminal(&local_key);
                    anyhow::bail!("tx EXPIRED — nothing landed; safe to re-run once")
                }
                SubmitOutcome::Rejected { reason } => {
                    ledger.prune_terminal(&local_key);
                    anyhow::bail!("REJECTED: {reason}")
                }
            }
        }
        "cancel" => {
            let order_id: i64 = required("--order-id")?.parse()?;
            let ix = builder.spot_order_cancel_ix(&SpotOrderCancelArgs {
                instr_id,
                side,
                order_id,
            })?;
            let (blockhash, lvbh) = blockhash_cache.get().await?;
            let tx = builder.build_signed_tx(&[ix], &key, &blockhash, &luts)?;
            println!("sig: {}", tx.signature_base58());
            match sender.submit_and_confirm(&tx, lvbh).await? {
                SubmitOutcome::Confirmed { slot, logs, err } => {
                    if let Some(err) = err {
                        for line in &logs {
                            println!("  {line}");
                        }
                        anyhow::bail!(
                            "cancel landed at slot {slot} but the program FAILED: {err} — \
                             order NOT cancelled; investigate before retry"
                        );
                    }
                    println!("TxOutcome::Confirmed at slot {slot}");
                    let (_o, _f, saw_cancel) = print_own_reports(&logs);
                    anyhow::ensure!(saw_cancel, "no SpotOrderCancelReport in own logs");
                }
                SubmitOutcome::Expired => anyhow::bail!("cancel EXPIRED — re-run once"),
                SubmitOutcome::Rejected { reason } => anyhow::bail!("REJECTED: {reason}"),
            }
        }
        other => anyhow::bail!("unknown subcommand: {other} (place|cancel)"),
    }
    Ok(())
}
