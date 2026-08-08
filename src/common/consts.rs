//! Deriverse venue constants (spec §4). Wire facts SoT: the kit clone's
//! `constants.ts` + feasibility doc §7 (RPC endpoints, verified live
//! 2026-08-04).

use std::sync::LazyLock;

use nautilus_model::identifiers::Venue;

/// Canonical venue identifier used across the adapter.
pub static DERIVERSE_VENUE: LazyLock<Venue> = LazyLock::new(|| Venue::from("DERIVERSE"));

/// Mainnet program id (kit `constants.ts::PROGRAM_ID`).
pub const PROGRAM_ID: &str = "DRVSpZ2YUYYKgZP8XtLhAGtT1zYSCKzeHfb4DgRnrgqD";
/// Mainnet program VERSION (kit `constants.ts::VERSION`).
pub const PROGRAM_VERSION: u32 = 1;
/// Devnet program id + version (feasibility §1). Selecting devnet = override
/// BOTH `program_id` and `program_version` in config (spec §2.5).
pub const DEVNET_PROGRAM_ID: &str = "CDESjex4EDBKLwx9ZPzVbjiHEHatasb5fhSJZMzNfvw2";
pub const DEVNET_PROGRAM_VERSION: u32 = 7;

/// Venue-hosted keyless RPC (feasibility §7 — undocumented but live-probed;
/// serves their browser UI). Fallback = the public Solana endpoint.
pub const DEFAULT_RPC_HTTP_URL: &str = "https://rpc-mainnet.deriverse.io";
pub const DEFAULT_RPC_WS_URL: &str = "wss://rpc-mainnet.deriverse.io";
pub const FALLBACK_RPC_HTTP_URL: &str = "https://api.mainnet-beta.solana.com";

/// Ladder depth per side (kit `constants.ts::MARKET_DEPTH` / `types.rs`).
pub const MARKET_DEPTH: usize = 20;
/// Price decimal factor: raw prices divide by DF (kit `constants.ts::DF`).
pub const DF: i64 = 1_000_000_000;
/// NT price precision implied by DF = 1e9.
pub const PRICE_PRECISION: u8 = 9;
/// Instrument account header length (kit `structure_models.ts`
/// `InstrAccountHeaderModel.LENGTH`); compile-asserted against
/// `size_of::<InstrAccountHeader>()` in decode/instr.rs.
pub const INSTR_HEADER_LEN: usize = 1064;
/// Header + 4 ladders × 20 × 16 B — the decoded region; the rest of the
/// ~766–792 KB account (RB-tree book internals) is not decoded.
pub const INSTR_SNAPSHOT_LEN: usize = 2344;

/// Seed for the program's `drvsAuthority` PDA (kit `engine/index.ts:521`).
pub const DRVS_AUTHORITY_SEED: &[u8] = b"ndxnt";

// Solana-wide program ids (kit `constants.ts`).
pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const ADDRESS_LOOKUP_TABLE_PROGRAM: &str = "AddressLookupTab1e1111111111111111111111111";
pub const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";

// Well-known mainnet mints (the on-chain token registry carries mints +
// decimals but NO names — common/symbol.rs maps mint → ticker).
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
