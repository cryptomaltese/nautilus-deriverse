//! Symbol grammar (spec §1.8): `Symbol` = `{ASSET}-{CRNCY}` for spot
//! (mainnet instr_id 0 ⇒ `WSOL-USDC`), `{ASSET}-{CRNCY}-PERP` for perps;
//! `InstrumentId` = `{symbol}.DERIVERSE`.
//!
//! The on-chain token registry (`TokenState`) carries mint + decimals but NO
//! ticker; tickers resolve through this curated mint→ticker table (seeded
//! with the two mainnet mints the venue lists), falling back to a truncated
//! mint string so an unknown listing never panics.

use crate::common::consts::{USDC_MINT, WSOL_MINT};

/// Curated mint → ticker table.
pub fn ticker_for_mint(mint: &str) -> String {
    match mint {
        WSOL_MINT => "WSOL".to_string(),
        USDC_MINT => "USDC".to_string(),
        other => format!("MINT{}", &other[..other.len().min(4)]),
    }
}

/// `{ASSET}-{CRNCY}` spot symbol.
#[must_use]
pub fn spot_symbol(asset_ticker: &str, crncy_ticker: &str) -> String {
    format!("{asset_ticker}-{crncy_ticker}")
}

/// `{ASSET}-{CRNCY}-PERP` perp symbol (staged for M1b/M2; grammar owned here).
#[must_use]
pub fn perp_symbol(asset_ticker: &str, crncy_ticker: &str) -> String {
    format!("{asset_ticker}-{crncy_ticker}-PERP")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_mints_resolve() {
        assert_eq!(ticker_for_mint(WSOL_MINT), "WSOL");
        assert_eq!(ticker_for_mint(USDC_MINT), "USDC");
    }

    #[test]
    fn unknown_mint_falls_back_deterministically() {
        assert_eq!(ticker_for_mint("AbCdEfGh1234"), "MINTAbCd");
    }

    #[test]
    fn grammar_matches_spec() {
        assert_eq!(spot_symbol("WSOL", "USDC"), "WSOL-USDC");
        assert_eq!(perp_symbol("WSOL", "USDC"), "WSOL-USDC-PERP");
    }
}
