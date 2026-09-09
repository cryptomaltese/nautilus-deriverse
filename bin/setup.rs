//! One-time venue setup (spec §5 M1a scope): client primary/community
//! account init + spot USDC Deposit — both ride the SAME first-deposit
//! instruction (kit buildDepositInstruction exists=false creates the client
//! accounts + client LUT; exists=true just deposits).
//!
//! Subcommands:
//!   keygen  --keypair <path>              — generate hot wallet (prints PUBKEY ONLY)
//!   check   --keypair <path>              — SOL + USDC ATA + client account status
//!   init-deposit --keypair <path> --usdc <amount> — build/sign/send/confirm
//!
//! SECRETS: key bytes never leave process memory; only the pubkey and the
//! keypair PATH are ever printed (Invariant 10).

use std::io::Write as _;
use std::str::FromStr;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use solana_pubkey::Pubkey;

use nautilus_deriverse::common::consts::{TOKEN_PROGRAM, USDC_MINT};
use nautilus_deriverse::config::DeriverseExecClientConfig;
use nautilus_deriverse::rpc::{DeriverseRpcClient, SolanaRpc};
use nautilus_deriverse::tx::accounts::{associated_token_address, resolve_directory, VenueContext};
use nautilus_deriverse::tx::build::{DepositArgs, TxBuilder};
use nautilus_deriverse::tx::send::{BlockhashCache, SubmitOutcome, TxSender};

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn required(name: &str) -> anyhow::Result<String> {
    arg(name).ok_or_else(|| anyhow::anyhow!("missing required {name} <value>"))
}

/// Load a Solana-format keypair file (JSON array of 64 bytes) — never
/// printing any of it.
fn load_keypair(path: &str) -> anyhow::Result<SigningKey> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read keypair file {path}: {e}"))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw)
        .map_err(|_| anyhow::anyhow!("keypair file {path} is not a JSON byte array"))?;
    anyhow::ensure!(bytes.len() == 64, "keypair file must hold 64 bytes");
    let seed: [u8; 32] = bytes[..32].try_into().expect("seed slice");
    let key = SigningKey::from_bytes(&seed);
    anyhow::ensure!(
        key.verifying_key().to_bytes()[..] == bytes[32..],
        "keypair file pubkey half does not match its seed"
    );
    Ok(key)
}

fn pubkey_of(key: &SigningKey) -> Pubkey {
    Pubkey::new_from_array(key.verifying_key().to_bytes())
}

async fn context(
    cfg: &DeriverseExecClientConfig,
    rpc: &DeriverseRpcClient,
    signer: Pubkey,
) -> anyhow::Result<VenueContext> {
    let program_id = Pubkey::from_str(&cfg.effective_program_id())?;
    let dir = resolve_directory(rpc, &program_id, cfg.effective_program_version()).await?;
    Ok(VenueContext::new(dir, signer))
}

/// SPL token account amount (u64 LE at offset 64 of the 165-byte layout).
async fn ata_balance(rpc: &DeriverseRpcClient, ata: &Pubkey) -> anyhow::Result<Option<u64>> {
    match rpc
        .get_account_info(&ata.to_string(), Some((64, 8)))
        .await?
    {
        None => Ok(None),
        Some((_slot, bytes)) => {
            anyhow::ensure!(bytes.len() == 8, "token amount slice");
            Ok(Some(u64::from_le_bytes(bytes.try_into().expect("8 bytes"))))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let keypair_path = required("--keypair")?;
    let cfg = DeriverseExecClientConfig {
        keypair_path: Some(keypair_path.clone().into()),
        ..Default::default()
    };
    match cmd.as_str() {
        "keygen" => {
            anyhow::ensure!(
                !std::path::Path::new(&keypair_path).exists(),
                "{keypair_path} already exists — refusing to overwrite"
            );
            let mut seed = [0u8; 32];
            getrandom::getrandom(&mut seed).map_err(|e| anyhow::anyhow!("entropy: {e}"))?;
            let key = SigningKey::from_bytes(&seed);
            let mut file_bytes = Vec::with_capacity(64);
            file_bytes.extend_from_slice(&seed);
            file_bytes.extend_from_slice(&key.verifying_key().to_bytes());
            let json = serde_json::to_string(&file_bytes)?;
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&keypair_path)?;
                f.write_all(json.as_bytes())?;
            }
            println!("pubkey: {}", pubkey_of(&key));
        }
        "check" => {
            let key = load_keypair(&keypair_path)?;
            let signer = pubkey_of(&key);
            let rpc = DeriverseRpcClient::new(
                cfg.effective_rpc_url(),
                cfg.effective_program_id(),
                cfg.commitment,
                cfg.http_timeout_secs,
            )?;
            let ctx = context(&cfg, &rpc, signer).await?;
            let sol = rpc.get_balance(&signer.to_string()).await? as f64 / 1e9;
            let usdc_mint = Pubkey::from_str(USDC_MINT)?;
            let ata =
                associated_token_address(&signer, &Pubkey::from_str(TOKEN_PROGRAM)?, &usdc_mint);
            let usdc = ata_balance(&rpc, &ata).await?;
            let client_primary_exists = rpc
                .get_account_info(&ctx.client_primary.to_string(), Some((0, 8)))
                .await?
                .is_some();
            println!("signer: {signer}");
            println!("sol: {sol}");
            match usdc {
                Some(raw) => println!("usdc (ata): {}", raw as f64 / 1e6),
                None => println!("usdc (ata): NO ATA (fund the wallet with USDC first)"),
            }
            println!(
                "client_primary: {}",
                if client_primary_exists {
                    "EXISTS"
                } else {
                    "MISSING (setup will create it)"
                }
            );
        }
        "init-deposit" => {
            let usdc_amount: f64 = required("--usdc")?.parse()?;
            anyhow::ensure!(
                usdc_amount > 0.0 && usdc_amount <= 11.0,
                "M1a budget cap: 0 < usdc <= 11"
            );
            let key = load_keypair(&keypair_path)?;
            let signer = pubkey_of(&key);
            let rpc = Arc::new(DeriverseRpcClient::new(
                cfg.effective_rpc_url(),
                cfg.effective_program_id(),
                cfg.commitment,
                cfg.http_timeout_secs,
            )?);
            let ctx = context(&cfg, rpc.as_ref(), signer).await?;
            // USDC token id = instr 0's crncy token (no hardcoded registry ids).
            let instr0 = ctx
                .dir
                .instruments
                .get(&0)
                .ok_or_else(|| anyhow::anyhow!("no instr 0"))?;
            let token_id = instr0.crncy_token_id;
            let token = ctx
                .dir
                .tokens
                .get(&token_id)
                .ok_or_else(|| anyhow::anyhow!("no crncy token"))?;
            anyhow::ensure!(
                token.mint == Pubkey::from_str(USDC_MINT)?,
                "instr 0 crncy mint is not USDC — refusing (got {})",
                token.mint
            );
            let exists = rpc
                .get_account_info(&ctx.client_primary.to_string(), Some((0, 8)))
                .await?
                .is_some();
            let lut_slot = if exists {
                0
            } else {
                (rpc.get_slot().await? - 1) as u32
            };
            let builder = TxBuilder {
                ctx: &ctx,
                compute_unit_limit: cfg.compute_unit_limit,
                compute_unit_price_microlamports: cfg.priority_fee_microlamports,
            };
            let ix = builder.deposit_ix(&DepositArgs {
                token_id,
                amount: usdc_amount,
                deposit_all: false,
                exists,
                lut_slot,
                custom_id: 0,
            })?;
            let blockhash_cache = BlockhashCache::new(Arc::clone(&rpc), cfg.blockhash_refresh_secs);
            let (blockhash, lvbh) = blockhash_cache.get().await?;
            // First deposit touches no instrument accounts ⇒ no LUTs needed.
            let tx = builder.build_signed_tx(&[ix], &key, &blockhash, &[])?;
            println!(
                "sending deposit (exists={exists}) sig={}",
                tx.signature_base58()
            );
            let sender = TxSender {
                rpc: Arc::clone(&rpc),
                poll_interval_ms: cfg.signature_poll_interval_ms,
            };
            match sender.submit_and_confirm(&tx, lvbh).await? {
                SubmitOutcome::Confirmed { slot, logs, err } => {
                    for line in &logs {
                        println!("  {line}");
                    }
                    if let Some(err) = err {
                        anyhow::bail!(
                            "deposit landed at slot {slot} but the program FAILED: {err} — \
                             no state change (atomic tx); safe to retry once"
                        );
                    }
                    println!("CONFIRMED at slot {slot}; {} log lines", logs.len());
                }
                SubmitOutcome::Expired => {
                    anyhow::bail!("deposit tx EXPIRED — safe to retry once, then stop")
                }
                SubmitOutcome::Rejected { reason } => anyhow::bail!("deposit REJECTED: {reason}"),
            }
        }
        other => anyhow::bail!("unknown subcommand: {other} (keygen|check|init-deposit)"),
    }
    Ok(())
}
