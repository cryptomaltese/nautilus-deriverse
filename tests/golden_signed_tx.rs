//! build_signed_tx byte-exactness vs kit-signed transactions (spec §2.3):
//! identical (keypair, blockhash, order) tuples, dummy key, offline.

use std::str::FromStr;

use base64::Engine as _;
use ed25519_dalek::SigningKey;
use solana_pubkey::Pubkey;

use nautilus_deriverse::tx::accounts::{InstrMeta, VenueContext, VenueDirectory, VenueToken};
use nautilus_deriverse::tx::build::{
    DepositArgs, NewSpotOrderArgs, ResolvedLut, SpotOrderCancelArgs, TxBuilder,
};

// goldens() / pk() / fixture_ctx(): copied VERBATIM from
// tests/golden_instructions.rs (same task) — integration tests are separate
// crates, and deliberate duplication keeps each file self-contained.

fn goldens() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/goldens.json")).expect("goldens.json")
}

fn pk(v: &serde_json::Value) -> Pubkey {
    Pubkey::from_str(v.as_str().expect("str")).expect("pubkey")
}

/// VenueContext with EXACTLY the fixture's synthetic directory values, so
/// builder output is byte-comparable to the kit builders' output.
fn fixture_ctx(g: &serde_json::Value) -> VenueContext {
    let program_id = pk(&g["program_id"]);
    let version = g["version"].as_u64().unwrap() as u32;
    let mut tokens = std::collections::BTreeMap::new();
    for t in g["directory"]["tokens"].as_array().unwrap() {
        let id = t["id"].as_u64().unwrap() as u32;
        let mask = t["mask"].as_u64().unwrap() as u32;
        tokens.insert(
            id,
            VenueToken {
                id,
                mint: pk(&t["mint"]),
                vault: pk(&t["vault"]),
                decimals: mask & 0xff,
                token2022: mask & 0x8000_0000 != 0,
            },
        );
    }
    let ih = &g["directory"]["instr"];
    let mut instruments = std::collections::BTreeMap::new();
    instruments.insert(
        0,
        InstrMeta {
            instr_id: 0,
            account: pk(&g["pda"]["root_account"]), // unused by builders; any valid key
            asset_token_id: ih["assetTokenId"].as_u64().unwrap() as u32,
            crncy_token_id: ih["crncyTokenId"].as_u64().unwrap() as u32,
            asset_decs: 9, // fixture token 1 mask
            crncy_decs: 6, // fixture token 2 mask
            maps_address: pk(&ih["mapsAddress"]),
            lut_address: pk(&ih["lutAddress"]),
        },
    );
    let dir = VenueDirectory {
        program_id,
        version,
        drvs_authority: pk(&g["pda"]["drvs_authority"]),
        root_account: pk(&g["pda"]["root_account"]),
        community_account: pk(&g["pda"]["community_account"]),
        tokens,
        instruments,
    };
    VenueContext::new(dir, pk(&g["signer_pubkey"]))
}

fn dummy_key(g: &serde_json::Value) -> SigningKey {
    let seed_byte = g["signer_seed_byte"].as_u64().unwrap() as u8;
    let key = SigningKey::from_bytes(&[seed_byte; 32]);
    // The dummy pubkey must equal the kit signer's — proves seed→key parity.
    assert_eq!(
        Pubkey::new_from_array(key.verifying_key().to_bytes()).to_string(),
        g["signer_pubkey"].as_str().unwrap()
    );
    key
}

fn fixture_lut(g: &serde_json::Value) -> ResolvedLut {
    ResolvedLut {
        address: pk(&g["lut"]["address"]),
        addresses: g["lut"]["addresses"].as_array().unwrap().iter().map(pk).collect(),
    }
}

fn assert_signed_matches(
    tx: &nautilus_deriverse::tx::build::VersionedTransaction,
    golden: &serde_json::Value,
) {
    let want_msg = base64::engine::general_purpose::STANDARD
        .decode(golden["message_b64"].as_str().unwrap())
        .unwrap();
    if tx.message != want_msg {
        // Precise failure localization before the byte assert.
        eprintln!("our message ({} B): {}", tx.message.len(), hex::encode(&tx.message));
        eprintln!("kit message ({} B): {}", want_msg.len(), hex::encode(&want_msg));
    }
    assert_eq!(tx.message, want_msg, "unsigned message bytes");
    assert_eq!(tx.base64(), golden["tx_b64"].as_str().unwrap(), "signed wire tx");
    assert_eq!(
        tx.signature_base58(),
        golden["signature_b58"].as_str().unwrap(),
        "signature"
    );
}

#[test]
fn signed_new_spot_order_matches_kit() {
    let g = goldens();
    let ctx = fixture_ctx(&g);
    let b = TxBuilder {
        ctx: &ctx,
        compute_unit_limit: g["compute_unit_limit"].as_u64().unwrap() as u32,
        compute_unit_price_microlamports: g["compute_unit_price_microlamports"].as_u64().unwrap(),
    };
    let ix = b
        .new_spot_order_ix(&NewSpotOrderArgs {
            instr_id: 0, side: 0, ioc: 0, order_type: 0, price: 152.25, qty: 0.033, edge_price: 0.0,
        })
        .unwrap();
    let tx = b
        .build_signed_tx(&[ix], &dummy_key(&g), g["blockhash"].as_str().unwrap(), &[fixture_lut(&g)])
        .unwrap();
    assert_signed_matches(&tx, &g["signed_txs"]["new_spot_order"]);
}

#[test]
fn signed_spot_order_cancel_matches_kit() {
    let g = goldens();
    let ctx = fixture_ctx(&g);
    let b = TxBuilder {
        ctx: &ctx,
        compute_unit_limit: g["compute_unit_limit"].as_u64().unwrap() as u32,
        compute_unit_price_microlamports: g["compute_unit_price_microlamports"].as_u64().unwrap(),
    };
    let ix = b
        .spot_order_cancel_ix(&SpotOrderCancelArgs { instr_id: 0, side: 0, order_id: 987_654_321_012_345 })
        .unwrap();
    let tx = b
        .build_signed_tx(&[ix], &dummy_key(&g), g["blockhash"].as_str().unwrap(), &[fixture_lut(&g)])
        .unwrap();
    assert_signed_matches(&tx, &g["signed_txs"]["spot_order_cancel"]);
}

#[test]
fn signed_first_deposit_matches_kit_no_lut() {
    let g = goldens();
    let ctx = fixture_ctx(&g);
    let b = TxBuilder {
        ctx: &ctx,
        compute_unit_limit: g["compute_unit_limit"].as_u64().unwrap() as u32,
        compute_unit_price_microlamports: g["compute_unit_price_microlamports"].as_u64().unwrap(),
    };
    let lut_slot = g["pda"]["lut_slot"].as_u64().unwrap() as u32;
    let ix = b
        .deposit_ix(&DepositArgs {
            token_id: 2, amount: 10.0, deposit_all: false, exists: false, lut_slot, custom_id: 0,
        })
        .unwrap();
    let tx = b
        .build_signed_tx(&[ix], &dummy_key(&g), g["blockhash"].as_str().unwrap(), &[])
        .unwrap();
    assert_signed_matches(&tx, &g["signed_txs"]["deposit_first"]);
}
