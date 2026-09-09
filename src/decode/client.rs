//! Client-primary-account decode (spec §1.8): `ClientPrimaryAccountHeader`
//! (drv-models, `#[repr(C)]` Pod) followed by `assets_count` × 16-byte
//! `AssetRecord { asset_id, temp_id, value }` rows. The record's asset type
//! rides the id's top nibble (`asset_id >> 28`, kit
//! `engine/client-queries.js::getClientData`): 1 = Token (spot balance,
//! `value` in raw `10^-token_decs` units), 2 = SpotLp, 3 = SpotOrders,
//! 4 = Perp. This is the venue's own holdings state — the qty-targeting
//! source for close/flatten flows (the venue oversell-rejects with
//! `InsufficientFunds` 132 and never clamps, spec §1.4).

use anyhow::{bail, ensure};
use drv_models::state::client_primary_account_header::ClientPrimaryAccountHeader;
// Plan-drift note (recorded): at pinned tag v0.2.74 the enum lives under the
// `account_type` submodule, not directly in `types` (types.rs:77/:118).
use drv_models::state::types::account_type::AccountType;
use drv_models::state::types::{AssetRecord, AssetType};

/// kit `structure_models.ts` `ClientPrimaryAccountHeaderModel.LENGTH`.
const CLIENT_PRIMARY_HEADER_LEN: usize = 376;
const ASSET_RECORD_LEN: usize = 16; // kit AssetRecordModel.LENGTH

// Offset-exactness gate vs the pinned drv-models tag AND the kit model
// (OFFSET_ASSETS_COUNT = 300): a reshape fails the BUILD, not a live decode.
const _: () =
    assert!(std::mem::size_of::<ClientPrimaryAccountHeader>() == CLIENT_PRIMARY_HEADER_LEN);
const _: () = assert!(std::mem::offset_of!(ClientPrimaryAccountHeader, assets_count) == 300);
const _: () = assert!(std::mem::size_of::<AssetRecord>() == ASSET_RECORD_LEN);

/// Asset-type nibble of an `AssetRecord.asset_id` (kit: `assetInfo >> 28`).
#[must_use]
pub fn asset_tag(asset_id: u32) -> u32 {
    asset_id >> 28
}

/// Id payload of an `AssetRecord.asset_id` (kit: `assetInfo & 0xfffffff`).
#[must_use]
pub fn asset_id_value(asset_id: u32) -> u32 {
    asset_id & 0x0fff_ffff
}

/// Decode header + asset records. Discriminator (tag + version) verified
/// before any field read; a truncated record region is a hard error, never
/// a partial parse (Invariant 7).
pub fn decode_client_primary(
    bytes: &[u8],
    expected_version: u32,
) -> anyhow::Result<(ClientPrimaryAccountHeader, Vec<AssetRecord>)> {
    ensure!(
        bytes.len() >= CLIENT_PRIMARY_HEADER_LEN,
        "client primary account too short: {} < {CLIENT_PRIMARY_HEADER_LEN}",
        bytes.len()
    );
    let tag = u32::from_le_bytes(bytes[0..4].try_into()?);
    let version = u32::from_le_bytes(bytes[4..8].try_into()?);
    if tag != AccountType::ClientPrimary as u32 {
        bail!(
            "client primary tag mismatch: got {tag}, want {}",
            AccountType::ClientPrimary as u32
        );
    }
    if version != expected_version {
        bail!("client primary version mismatch: got {version}, want {expected_version}");
    }
    let header = bytemuck::try_pod_read_unaligned::<ClientPrimaryAccountHeader>(
        &bytes[..CLIENT_PRIMARY_HEADER_LEN],
    )
    .map_err(|e| anyhow::anyhow!("client primary header read: {e}"))?;
    let count = header.assets_count as usize;
    let need = CLIENT_PRIMARY_HEADER_LEN + count * ASSET_RECORD_LEN;
    ensure!(
        bytes.len() >= need,
        "client primary assets truncated: assets_count {count} needs {need} bytes, got {}",
        bytes.len()
    );
    let mut assets = Vec::with_capacity(count);
    for i in 0..count {
        let off = CLIENT_PRIMARY_HEADER_LEN + i * ASSET_RECORD_LEN;
        let rec =
            bytemuck::try_pod_read_unaligned::<AssetRecord>(&bytes[off..off + ASSET_RECORD_LEN])
                .map_err(|e| anyhow::anyhow!("asset record {i} read: {e}"))?;
        assets.push(rec);
    }
    Ok((header, assets))
}

/// Venue-held spot balance for `token_id`, raw `10^-token_decs` units.
/// `Ok(None)` = no Token record for that id (zero holdings).
pub fn client_spot_token_balance(
    bytes: &[u8],
    expected_version: u32,
    token_id: u32,
) -> anyhow::Result<Option<i64>> {
    let (_header, assets) = decode_client_primary(bytes, expected_version)?;
    let token_tag = asset_tag(AssetType::Token as u32);
    Ok(assets
        .iter()
        .find(|r| asset_tag(r.asset_id) == token_tag && asset_id_value(r.asset_id) == token_id)
        .map(|r| r.value))
}

/// Largest integer f64 represents exactly (2^53): above it, `raw as f64`
/// itself can round UP — a lossy conversion this function refuses.
const MAX_F64_EXACT: i64 = 1 << 53;

/// Venue balance → order qty as `(display_qty, target_raw)`, FLOORED to the
/// instrument size step — never rounded up: the venue oversell-rejects
/// (`InsufficientFunds` 132) and never clamps, so a close/flatten qty must
/// be ≤ holdings exactly. The floor happens in INTEGER space
/// (`raw / step_raw * step_raw`) — a float floor with an epsilon rounds up
/// whenever the balance sits within the epsilon below a step multiple. The
/// WIRE qty encodes from `target_raw` (`TxBuilder::new_spot_order_ix_raw`),
/// never through `display_qty`: the display round-trip (÷10^decs then
/// ×10^decs + round) rounds UP 1–2 raw units for targets ≥ ~2^51.
/// `display_qty` is for logs/operator output only. The (≤ one-step)
/// remainder stays as venue dust rather than risking a reject. Balances
/// above 2^53 raw units are a hard error, not a silent precision loss.
pub fn qty_target_from_balance(
    raw_balance: i64,
    token_decs: u32,
    size_step: f64,
) -> anyhow::Result<(f64, i64)> {
    if raw_balance <= 0 || size_step <= 0.0 {
        return Ok((0.0, 0));
    }
    ensure!(
        raw_balance <= MAX_F64_EXACT,
        "venue balance {raw_balance} (raw) exceeds f64-exact range (2^53) — \
         refusing lossy qty derivation"
    );
    let step_raw = crate::tx::build::qty_to_raw(size_step, token_decs);
    ensure!(
        step_raw > 0,
        "size step {size_step} rounds to zero raw units at {token_decs} decimals"
    );
    let target_raw = (raw_balance / step_raw) * step_raw; // integer floor
    Ok((
        target_raw as f64 / 10f64.powi(token_decs as i32),
        target_raw,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::build::qty_to_raw;
    use bytemuck::Zeroable;

    /// Synthetic client primary account: zeroed header with discriminator +
    /// assets_count written wire-level (offsets compile-asserted above),
    /// records appended as drv-models structs.
    fn synthetic_account(tag: u32, version: u32, records: &[AssetRecord]) -> Vec<u8> {
        let header = ClientPrimaryAccountHeader::zeroed();
        let mut b = bytemuck::bytes_of(&header).to_vec();
        b[0..4].copy_from_slice(&tag.to_le_bytes());
        b[4..8].copy_from_slice(&version.to_le_bytes());
        b[300..304].copy_from_slice(&(records.len() as u32).to_le_bytes());
        for r in records {
            b.extend_from_slice(bytemuck::bytes_of(r));
        }
        b
    }

    fn rec(type_bits: u32, id: u32, value: i64) -> AssetRecord {
        AssetRecord {
            asset_id: type_bits | id,
            temp_id: 0,
            value,
        }
    }

    #[test]
    fn spot_token_balance_selects_token_records_only() {
        let bytes = synthetic_account(
            AccountType::ClientPrimary as u32,
            1,
            &[
                rec(AssetType::Token as u32, 2, 5_000_000_000), // held token 2
                rec(AssetType::SpotOrders as u32, 0, 12345),    // open-orders row, NOT a balance
                rec(AssetType::SpotLp as u32, 7, 999),          // LP row for id 7, NOT a balance
                rec(AssetType::Token as u32, 5, 7),
            ],
        );
        assert_eq!(
            client_spot_token_balance(&bytes, 1, 2).unwrap(),
            Some(5_000_000_000)
        );
        assert_eq!(client_spot_token_balance(&bytes, 1, 5).unwrap(), Some(7));
        // Id 7 exists only as an LP record: no TOKEN holdings.
        assert_eq!(client_spot_token_balance(&bytes, 1, 7).unwrap(), None);
        assert_eq!(client_spot_token_balance(&bytes, 1, 9).unwrap(), None);
    }

    #[test]
    fn wrong_tag_version_or_truncation_hard_errors() {
        let good = synthetic_account(
            AccountType::ClientPrimary as u32,
            1,
            &[rec(AssetType::Token as u32, 2, 1)],
        );
        assert!(decode_client_primary(&good, 1).is_ok());
        // Wrong tag / wrong version.
        let bad_tag = synthetic_account(7, 1, &[]);
        assert!(decode_client_primary(&bad_tag, 1).is_err());
        let bad_ver = synthetic_account(AccountType::ClientPrimary as u32, 9, &[]);
        assert!(decode_client_primary(&bad_ver, 1).is_err());
        // assets_count promises more records than the bytes hold.
        let truncated = &good[..good.len() - 1];
        assert!(decode_client_primary(truncated, 1).is_err());
    }

    /// The qty target FLOORS in integer space — a float floor + epsilon (or
    /// a `.round()`) is an oversell (the venue rejects, never clamps). The
    /// WIRE quantity is the returned integer; it must never exceed the
    /// balance.
    #[test]
    fn qty_target_floors_and_never_exceeds_balance() {
        // Exact multiple: floor is identity.
        assert_eq!(
            qty_target_from_balance(33_000_000, 9, 1e-3).unwrap(),
            (0.033, 33_000_000)
        );
        // 0.033999999: round() would say 0.034 (> holdings) — floor says 0.033.
        assert_eq!(
            qty_target_from_balance(33_999_999, 9, 1e-3).unwrap(),
            (0.033, 33_000_000)
        );
        // Review counterexample: 0.999999999 with step 1.0 — a float floor
        // with +1e-9 epsilon says 1.0 (raw 1e9 > balance ⇒ InsufficientFunds
        // 132, flatten deterministically fails). Integer floor says 0.
        assert_eq!(
            qty_target_from_balance(999_999_999, 9, 1.0).unwrap(),
            (0.0, 0)
        );
        assert_eq!(qty_target_from_balance(0, 9, 1e-3).unwrap(), (0.0, 0));
        assert_eq!(qty_target_from_balance(-5, 9, 1e-3).unwrap(), (0.0, 0));
        for &(raw, decs, step) in &[
            (33_000_000_i64, 9_u32, 1e-3_f64),
            (33_999_999, 9, 1e-3),
            (999_999_999, 9, 1.0),
            (1, 9, 1e-9),
            (999_999_999, 6, 1e-6),
            (5_000_000_000, 9, 1e-9),
            (9_007_199_254_000_000, 9, 1e-9), // round-2 failing-range case
            (MAX_F64_EXACT, 9, 1e-3),         // largest exactly-representable balance
        ] {
            let (_display, target_raw) = qty_target_from_balance(raw, decs, step).unwrap();
            assert!(
                target_raw <= raw,
                "wire qty {target_raw} exceeds balance {raw}"
            );
        }
    }

    /// Round-2 review: above ~2^51 the display-float round-trip
    /// (÷10^decs then qty_to_raw) rounds UP 1–2 raw units — the wire MUST
    /// encode from the integer, never through the display value.
    #[test]
    fn display_float_roundtrip_overshoots_where_raw_path_is_exact() {
        let raw = 9_007_199_254_000_000_i64;
        let (display, target_raw) = qty_target_from_balance(raw, 9, 1e-9).unwrap();
        // Integer path: exact (step_raw = 1 ⇒ zero slack, full balance).
        assert_eq!(target_raw, raw);
        // Display path: proven +1 raw unit — an oversell if it reached the
        // wire (InsufficientFunds 132; flatten has zero slack).
        assert_eq!(qty_to_raw(display, 9), raw + 1);
    }

    /// Above 2^53 raw units `raw as f64` itself rounds up — hard error, not
    /// silent precision loss.
    #[test]
    fn balance_above_f64_exact_range_hard_errors() {
        assert!(qty_target_from_balance(MAX_F64_EXACT + 1, 9, 1e-3).is_err());
        assert!(qty_target_from_balance(i64::MAX, 9, 1e-3).is_err());
        assert!(qty_target_from_balance(MAX_F64_EXACT, 9, 1e-3).is_ok());
    }
}
