//! Token-registry decode (spec §1.8): `TokenState`
//! (drv-models `state/token.rs` / kit `TokenStateModel`, 88 bytes) — one
//! account per listed token, discovered by tag `AccountType::Token = 4`.
//! Decimals ride the mask's low byte (kit `tokenDec`: `10^(mask & 0xff)`);
//! bit 31 selects Token-2022 (kit `instructions.ts` deposit builder).

use anyhow::{bail, ensure};
use drv_models::state::token::TokenState;
// Plan-drift note (recorded): at pinned tag v0.2.74 the enum lives under the
// `account_type` submodule, not directly in `types` (types.rs:77/:118).
use drv_models::state::types::account_type::AccountType;

const TOKEN_STATE_LEN: usize = 88; // kit TokenStateModel.LENGTH
const _: () = assert!(std::mem::size_of::<TokenState>() == TOKEN_STATE_LEN);

/// Decode one token-registry account. Discriminator verified before any
/// field read (Invariant 7).
pub fn decode_token_state(bytes: &[u8], expected_version: u32) -> anyhow::Result<TokenState> {
    ensure!(
        bytes.len() >= TOKEN_STATE_LEN,
        "token account too short: {} < {TOKEN_STATE_LEN}",
        bytes.len()
    );
    let tag = u32::from_le_bytes(bytes[0..4].try_into()?);
    let version = u32::from_le_bytes(bytes[4..8].try_into()?);
    if tag != AccountType::Token as u32 {
        bail!("token account tag mismatch: got {tag}, want {}", AccountType::Token as u32);
    }
    if version != expected_version {
        bail!("token account version mismatch: got {version}, want {expected_version}");
    }
    Ok(bytemuck::try_pod_read_unaligned::<TokenState>(&bytes[..TOKEN_STATE_LEN])
        .map_err(|e| anyhow::anyhow!("token state read: {e}"))?)
}

/// SPL decimals for this token (mask low byte).
#[must_use]
pub fn token_decimals(t: &TokenState) -> u32 {
    mask_bits(t) & 0xff
}

/// Token-2022 program flag (mask bit 31).
#[must_use]
pub fn token_is_2022(t: &TokenState) -> bool {
    mask_bits(t) & 0x8000_0000 != 0
}

/// TokenMask is a drv-models newtype; read it wire-level (offset 76) so this
/// module does not depend on its field visibility.
fn mask_bits(t: &TokenState) -> u32 {
    let bytes = bytemuck::bytes_of(t);
    u32::from_le_bytes(bytes[76..80].try_into().expect("mask slice"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_token(tag: u32, version: u32, id: u32, mask: u32) -> Vec<u8> {
        let mut b = vec![0u8; TOKEN_STATE_LEN];
        b[0..4].copy_from_slice(&tag.to_le_bytes());
        b[4..8].copy_from_slice(&version.to_le_bytes());
        b[72..76].copy_from_slice(&id.to_le_bytes()); // id (offset 72, kit OFFSET_ID)
        b[76..80].copy_from_slice(&mask.to_le_bytes()); // mask (offset 76)
        b
    }

    #[test]
    fn decodes_decimals_and_2022_flag() {
        let bytes = synthetic_token(AccountType::Token as u32, 1, 2, 6);
        let t = decode_token_state(&bytes, 1).expect("decode");
        assert_eq!(token_decimals(&t), 6);
        assert!(!token_is_2022(&t));
        let bytes = synthetic_token(AccountType::Token as u32, 1, 3, 0x8000_0009);
        let t = decode_token_state(&bytes, 1).expect("decode");
        assert_eq!(token_decimals(&t), 9);
        assert!(token_is_2022(&t));
    }

    #[test]
    fn wrong_tag_or_version_hard_errors() {
        assert!(decode_token_state(&synthetic_token(7, 1, 2, 6), 1).is_err());
        assert!(decode_token_state(&synthetic_token(4, 7, 2, 6), 1).is_err());
    }
}
