//! Golden instruction encodings vs @deriverse/kit (M1a acceptance 2).
//! Fixture: tests/fixtures/goldens.json (kit 1.0.74, offline generation).

use std::str::FromStr;

use solana_pubkey::Pubkey;

use nautilus_deriverse::tx::accounts::{InstrMeta, VenueContext, VenueDirectory, VenueToken};
use nautilus_deriverse::tx::build::{
    price_to_raw, qty_to_raw, DepositArgs, NewSpotOrderArgs, SpotOrderCancelArgs, TxBuilder,
};

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

fn builder(ctx: &VenueContext) -> TxBuilder<'_> {
    TxBuilder { ctx, compute_unit_limit: 400_000, compute_unit_price_microlamports: 1_000 }
}

fn assert_ix_matches(ix: &nautilus_deriverse::tx::build::Instruction, golden: &serde_json::Value) {
    assert_eq!(hex::encode(&ix.data), golden["data_hex"].as_str().unwrap(), "data bytes");
    let want = golden["accounts"].as_array().unwrap();
    assert_eq!(ix.accounts.len(), want.len(), "account count");
    for (i, (got, want)) in ix.accounts.iter().zip(want).enumerate() {
        assert_eq!(got.pubkey, pk(&want["address"]), "account[{i}] address");
        assert_eq!(got.is_signer, want["signer"].as_bool().unwrap(), "account[{i}] signer");
        assert_eq!(got.is_writable, want["writable"].as_bool().unwrap(), "account[{i}] writable");
    }
}

#[test]
fn new_spot_order_matches_kit() {
    let g = goldens();
    let ctx = fixture_ctx(&g);
    let a = &g["instructions"]["new_spot_order"]["args"];
    let ix = builder(&ctx)
        .new_spot_order_ix(&NewSpotOrderArgs {
            instr_id: 0,
            side: a["side"].as_u64().unwrap() as u8,
            ioc: a["ioc"].as_u64().unwrap() as u8,
            order_type: a["orderType"].as_u64().unwrap() as u8,
            price: a["price"].as_f64().unwrap(),
            qty: a["qty"].as_f64().unwrap(),
            edge_price: a["edgePrice"].as_f64().unwrap(),
        })
        .unwrap();
    assert_ix_matches(&ix, &g["instructions"]["new_spot_order"]);
    // IOC variant (side=ask, ioc=1) — the acceptance-6 shape.
    let a = &g["instructions"]["new_spot_order_ioc"]["args"];
    let ix = builder(&ctx)
        .new_spot_order_ix(&NewSpotOrderArgs {
            instr_id: 0,
            side: a["side"].as_u64().unwrap() as u8,
            ioc: a["ioc"].as_u64().unwrap() as u8,
            order_type: 0,
            price: a["price"].as_f64().unwrap(),
            qty: a["qty"].as_f64().unwrap(),
            edge_price: 0.0,
        })
        .unwrap();
    assert_ix_matches(&ix, &g["instructions"]["new_spot_order_ioc"]);
}

#[test]
fn new_spot_order_mixed_bid_ioc_matches_kit() {
    // Mixed control (review F6): side=0 WITH ioc=1 decorrelates the side and
    // ioc bytes, which the base (0,0) and ioc (1,1) goldens leave coupled.
    let g = goldens();
    let ctx = fixture_ctx(&g);
    let a = &g["instructions"]["new_spot_order_mixed"]["args"];
    let ix = builder(&ctx)
        .new_spot_order_ix(&NewSpotOrderArgs {
            instr_id: 0,
            side: a["side"].as_u64().unwrap() as u8,
            ioc: a["ioc"].as_u64().unwrap() as u8,
            order_type: a["orderType"].as_u64().unwrap() as u8,
            price: a["price"].as_f64().unwrap(),
            qty: a["qty"].as_f64().unwrap(),
            edge_price: a["edgePrice"].as_f64().unwrap(),
        })
        .unwrap();
    assert_ix_matches(&ix, &g["instructions"]["new_spot_order_mixed"]);
}

#[test]
fn spot_order_cancel_matches_kit() {
    let g = goldens();
    let ctx = fixture_ctx(&g);
    let ix = builder(&ctx)
        .spot_order_cancel_ix(&SpotOrderCancelArgs {
            instr_id: 0,
            side: 0,
            order_id: 987_654_321_012_345,
        })
        .unwrap();
    assert_ix_matches(&ix, &g["instructions"]["spot_order_cancel"]);
}

#[test]
fn deposit_both_branches_match_kit() {
    let g = goldens();
    let ctx = fixture_ctx(&g);
    let lut_slot = g["pda"]["lut_slot"].as_u64().unwrap() as u32;
    let existing = builder(&ctx)
        .deposit_ix(&DepositArgs {
            token_id: 2, amount: 10.0, deposit_all: false, exists: true, lut_slot: 0, custom_id: 0,
        })
        .unwrap();
    assert_ix_matches(&existing, &g["instructions"]["deposit_exists"]);
    let first = builder(&ctx)
        .deposit_ix(&DepositArgs {
            token_id: 2, amount: 10.0, deposit_all: false, exists: false, lut_slot, custom_id: 0,
        })
        .unwrap();
    assert_ix_matches(&first, &g["instructions"]["deposit_first"]);
}

#[test]
fn perp_data_bytes_match_kit() {
    let g = goldens();
    let ctx = fixture_ctx(&g);
    let b = builder(&ctx);
    assert_eq!(
        hex::encode(b.new_perp_order_data(0, 0, 0, 5, 0, 152.25, 0.033, 0.0).unwrap()),
        g["instructions"]["new_perp_order"]["data_hex"].as_str().unwrap()
    );
    assert_eq!(
        hex::encode(b.perp_order_cancel_data(0, 0, 987_654_321_012_345)),
        g["instructions"]["perp_order_cancel"]["data_hex"].as_str().unwrap()
    );
    assert_eq!(
        hex::encode(b.perp_mass_cancel_data(0)),
        g["instructions"]["perp_mass_cancel"]["data_hex"].as_str().unwrap()
    );
}

#[test]
fn negative_control_perturbed_field_changes_bytes() {
    let g = goldens();
    let ctx = fixture_ctx(&g);
    let base = NewSpotOrderArgs {
        instr_id: 0, side: 0, ioc: 0, order_type: 0, price: 152.25, qty: 0.033, edge_price: 0.0,
    };
    let golden_hex = g["instructions"]["new_spot_order"]["data_hex"].as_str().unwrap();
    for perturbed in [
        NewSpotOrderArgs { price: 152.26, ..base.clone() },
        NewSpotOrderArgs { qty: 0.034, ..base.clone() },
        NewSpotOrderArgs { side: 1, ..base.clone() },
        NewSpotOrderArgs { ioc: 1, ..base.clone() },
    ] {
        let ix = builder(&ctx).new_spot_order_ix(&perturbed).unwrap();
        assert_ne!(hex::encode(&ix.data), golden_hex, "perturbation must change bytes");
    }
}

#[test]
fn scaling_layer_is_kit_identical() {
    // §1.4 scaling split, covered explicitly: prices ×DF with round;
    // order qty ×10^asset_decs; deposit amount ×10^token_decs.
    assert_eq!(price_to_raw(152.25), 152_250_000_000);
    assert_eq!(qty_to_raw(0.033, 9), 33_000_000);
    assert_eq!(qty_to_raw(10.0, 6), 10_000_000);
    // Round (not floor): kit Math.round.
    assert_eq!(price_to_raw(0.000_000_001_5), 2);
    // NT Quantity threads through the same layer (acceptance 2's
    // conversion-layer clause).
    let q = nautilus_model::types::Quantity::new(0.033, 9);
    assert_eq!(qty_to_raw(q.as_f64(), 9), 33_000_000);
}
