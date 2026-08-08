//! Instrument-account decode (spec §1.3): `InstrAccountHeader` (drv-models,
//! `#[repr(C)]` Pod) followed by four contiguous ladders of
//! `MARKET_DEPTH = 20` × 16-byte `{px: i64, qty: i64}` lines; a zero `px`
//! terminates a ladder. Ladder order: spot_bids, spot_asks, perp_bids,
//! perp_asks. The header `best_bid`/`best_ask` are cross-checks, not the
//! source — sizes only exist in the ladders (QuoteTick mapping reads
//! ladder[k][0]).

use anyhow::{bail, ensure, Context};
use drv_models::state::instrument::InstrAccountHeader;
// Plan-drift note (recorded): at pinned tag v0.2.74 the enum lives under the
// `account_type` submodule, not directly in `types` (types.rs:77/:118).
use drv_models::state::types::account_type::AccountType;

use crate::common::consts::{INSTR_HEADER_LEN, INSTR_SNAPSHOT_LEN, MARKET_DEPTH};

// Offset-exactness gate: if the pinned drv-models tag ever reshapes the
// header, this fails the BUILD, not a live decode.
const _: () = assert!(std::mem::size_of::<InstrAccountHeader>() == INSTR_HEADER_LEN);
const _: () = assert!(INSTR_SNAPSHOT_LEN == INSTR_HEADER_LEN + 4 * MARKET_DEPTH * 16);

/// One ladder line (raw scaled integers; price ÷ DF, qty ÷ 10^asset_decs
/// happen at the consumer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Level {
    pub px: i64,
    pub qty: i64,
}

/// Fixed-capacity ladder (≤ MARKET_DEPTH levels; no heap).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ladder {
    pub levels: [Level; MARKET_DEPTH],
    pub len: usize,
}

impl Ladder {
    #[must_use]
    pub fn top(&self) -> Option<Level> {
        (self.len > 0).then(|| self.levels[0])
    }
}

/// Decoded header + ladders for one instrument account notification.
pub struct InstrSnapshot {
    pub slot: u64,
    pub header: InstrAccountHeader,
    pub spot_bids: Ladder,
    pub spot_asks: Ladder,
    pub perp_bids: Ladder,
    pub perp_asks: Ladder,
}

fn read_i64(bytes: &[u8], offset: usize) -> anyhow::Result<i64> {
    Ok(i64::from_le_bytes(
        bytes[offset..offset + 8].try_into().context("i64 read")?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> anyhow::Result<u32> {
    Ok(u32::from_le_bytes(
        bytes[offset..offset + 4].try_into().context("u32 read")?,
    ))
}

fn decode_ladder(bytes: &[u8], ladder_index: usize) -> anyhow::Result<Ladder> {
    let base = INSTR_HEADER_LEN + ladder_index * MARKET_DEPTH * 16;
    let mut ladder = Ladder::default();
    for i in 0..MARKET_DEPTH {
        let off = base + i * 16;
        let px = read_i64(bytes, off)?;
        if px == 0 {
            break;
        }
        let qty = read_i64(bytes, off + 8)?;
        ladder.levels[i] = Level { px, qty };
        ladder.len = i + 1;
    }
    Ok(ladder)
}

/// Decode the first `INSTR_SNAPSHOT_LEN` bytes of an instrument account.
///
/// Hard-errors (never a partial parse — Invariant 7) when:
/// - the buffer is shorter than the snapshot region,
/// - `discriminator.tag != AccountType::Instr`,
/// - `discriminator.version != expected_version`.
pub fn decode_instr_account(
    bytes: &[u8],
    slot: u64,
    expected_version: u32,
) -> anyhow::Result<InstrSnapshot> {
    ensure!(
        bytes.len() >= INSTR_SNAPSHOT_LEN,
        "instr account data too short: {} < {INSTR_SNAPSHOT_LEN}",
        bytes.len()
    );
    // Discriminator check BEFORE any struct cast/field read. Wire layout:
    // tag u32 LE at 0, version u32 LE at 4 (types.rs `Discriminator{tag,
    // version}` — note the PDA seed order is the TRANSPOSE: version, tag).
    let tag = read_u32(bytes, 0)?;
    let version = read_u32(bytes, 4)?;
    if tag != AccountType::Instr as u32 {
        bail!("instr account tag mismatch: got {tag}, want {}", AccountType::Instr as u32);
    }
    if version != expected_version {
        bail!("instr account program version mismatch: got {version}, want {expected_version}");
    }
    let header: InstrAccountHeader =
        bytemuck::try_pod_read_unaligned(&bytes[..INSTR_HEADER_LEN])
            .map_err(|e| anyhow::anyhow!("instr header read: {e}"))?;
    Ok(InstrSnapshot {
        slot,
        header,
        spot_bids: decode_ladder(bytes, 0)?,
        spot_asks: decode_ladder(bytes, 1)?,
        perp_bids: decode_ladder(bytes, 2)?,
        perp_asks: decode_ladder(bytes, 3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic account bytes: default header with discriminator + ids set,
    /// plus hand-written ladder lines. (The LIVE conformance fixture is
    /// tests/decode_conformance.rs, Task 14.)
    fn synthetic_account(tag: u32, version: u32) -> Vec<u8> {
        let header = InstrAccountHeader::default();
        let mut bytes = bytemuck::bytes_of(&header).to_vec();
        // Discriminator lives at offsets 0/4; write it wire-level so the
        // test does not depend on the Discriminator field types. The header
        // best_bid/best_ask cross-check fields stay zero — the ladders are
        // the source.
        bytes[0..4].copy_from_slice(&tag.to_le_bytes());
        bytes[4..8].copy_from_slice(&version.to_le_bytes());
        bytes.resize(INSTR_SNAPSHOT_LEN, 0);
        // spot_bids[0] = {px: 152_250_000_000, qty: 33_000_000}
        bytes[INSTR_HEADER_LEN..INSTR_HEADER_LEN + 8]
            .copy_from_slice(&152_250_000_000i64.to_le_bytes());
        bytes[INSTR_HEADER_LEN + 8..INSTR_HEADER_LEN + 16]
            .copy_from_slice(&33_000_000i64.to_le_bytes());
        // spot_bids[1] = zero px ⇒ terminates at len 1.
        // spot_asks[0] = {px: 152_350_000_000, qty: 12_000_000}
        let asks = INSTR_HEADER_LEN + MARKET_DEPTH * 16;
        bytes[asks..asks + 8].copy_from_slice(&152_350_000_000i64.to_le_bytes());
        bytes[asks + 8..asks + 16].copy_from_slice(&12_000_000i64.to_le_bytes());
        bytes
    }

    #[test]
    fn decodes_ladders_and_terminates_on_zero_px() {
        let bytes = synthetic_account(AccountType::Instr as u32, 1);
        let snap = decode_instr_account(&bytes, 42, 1).expect("decode");
        assert_eq!(snap.slot, 42);
        assert_eq!(snap.spot_bids.len, 1);
        assert_eq!(
            snap.spot_bids.top().unwrap(),
            Level { px: 152_250_000_000, qty: 33_000_000 }
        );
        assert_eq!(snap.spot_asks.top().unwrap().px, 152_350_000_000);
        assert_eq!(snap.perp_bids.len, 0);
        assert_eq!(snap.perp_asks.len, 0);
    }

    #[test]
    fn wrong_tag_is_a_hard_error() {
        let bytes = synthetic_account(4, 1); // Token tag, not Instr
        assert!(decode_instr_account(&bytes, 1, 1).is_err());
    }

    #[test]
    fn wrong_version_is_a_hard_error() {
        let bytes = synthetic_account(AccountType::Instr as u32, 7); // devnet version
        assert!(decode_instr_account(&bytes, 1, 1).is_err());
    }

    #[test]
    fn short_buffer_is_a_hard_error() {
        let bytes = vec![0u8; INSTR_SNAPSHOT_LEN - 1];
        assert!(decode_instr_account(&bytes, 1, 1).is_err());
    }
}
