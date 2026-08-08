//! Client configs (spec §2.5 — field names/types verbatim).
//! `program_id`/`program_version` default to the mainnet constants;
//! overriding BOTH selects devnet. `keypair_path` is a PATH (the key value
//! never appears in logs/Debug/errors — Invariant 10; the path itself is
//! not secret and may be shown).

use std::path::PathBuf;

use crate::common::consts::{
    DEFAULT_RPC_HTTP_URL, DEFAULT_RPC_WS_URL, PROGRAM_ID, PROGRAM_VERSION,
};
use crate::common::enums::Commitment;

#[derive(Clone, Debug)]
pub struct DeriverseDataClientConfig {
    pub rpc_url: Option<String>,        // default DEFAULT_RPC_HTTP_URL
    pub ws_url: Option<String>,         // default DEFAULT_RPC_WS_URL
    pub program_id: Option<String>,     // default PROGRAM_ID (mainnet)
    pub program_version: Option<u32>,   // default PROGRAM_VERSION (mainnet)
    pub commitment: Commitment,         // default Confirmed
    pub http_timeout_secs: u64,
    pub reconcile_interval_secs: u64,   // §3 REST reconcile timer
    pub staleness_threshold_slots: u64, // §3 alarm
}

impl Default for DeriverseDataClientConfig {
    fn default() -> Self {
        Self {
            rpc_url: None,
            ws_url: None,
            program_id: None,
            program_version: None,
            commitment: Commitment::Confirmed,
            http_timeout_secs: 15,
            reconcile_interval_secs: 30,
            staleness_threshold_slots: 50,
        }
    }
}

impl DeriverseDataClientConfig {
    #[must_use]
    pub fn effective_rpc_url(&self) -> String {
        self.rpc_url.clone().unwrap_or_else(|| DEFAULT_RPC_HTTP_URL.to_string())
    }
    #[must_use]
    pub fn effective_ws_url(&self) -> String {
        self.ws_url.clone().unwrap_or_else(|| DEFAULT_RPC_WS_URL.to_string())
    }
    #[must_use]
    pub fn effective_program_id(&self) -> String {
        self.program_id.clone().unwrap_or_else(|| PROGRAM_ID.to_string())
    }
    #[must_use]
    pub fn effective_program_version(&self) -> u32 {
        self.program_version.unwrap_or(PROGRAM_VERSION)
    }
}

#[derive(Clone, Debug)]
pub struct DeriverseExecClientConfig {
    pub keypair_path: Option<PathBuf>,  // secrets dir; value never logged
    pub rpc_url: Option<String>,
    pub ws_url: Option<String>,
    pub program_id: Option<String>,     // default PROGRAM_ID (mainnet)
    pub program_version: Option<u32>,   // default PROGRAM_VERSION (mainnet)
    pub commitment: Commitment,
    pub http_timeout_secs: u64,
    pub priority_fee_microlamports: u64,
    pub compute_unit_limit: u32,        // budget includes crank CU (§4)
    pub blockhash_refresh_secs: u64,
    pub signature_poll_interval_ms: u64,
    pub market_slippage_bps: u32,       // → edge_price
    pub leverage: Option<u8>,           // 1–100; None = venue max (0)
}

impl Default for DeriverseExecClientConfig {
    fn default() -> Self {
        Self {
            keypair_path: None,
            rpc_url: None,
            ws_url: None,
            program_id: None,
            program_version: None,
            commitment: Commitment::Confirmed,
            http_timeout_secs: 15,
            priority_fee_microlamports: 1_000,
            compute_unit_limit: 400_000,
            blockhash_refresh_secs: 30,
            signature_poll_interval_ms: 500,
            market_slippage_bps: 50,
            leverage: None,
        }
    }
}

impl DeriverseExecClientConfig {
    #[must_use]
    pub fn effective_rpc_url(&self) -> String {
        self.rpc_url.clone().unwrap_or_else(|| DEFAULT_RPC_HTTP_URL.to_string())
    }
    #[must_use]
    pub fn effective_ws_url(&self) -> String {
        self.ws_url.clone().unwrap_or_else(|| DEFAULT_RPC_WS_URL.to_string())
    }
    #[must_use]
    pub fn effective_program_id(&self) -> String {
        self.program_id.clone().unwrap_or_else(|| PROGRAM_ID.to_string())
    }
    #[must_use]
    pub fn effective_program_version(&self) -> u32 {
        self.program_version.unwrap_or(PROGRAM_VERSION)
    }
    /// Wire leverage byte: None ⇒ 0 = venue max (spec §1.4).
    #[must_use]
    pub fn wire_leverage(&self) -> u8 {
        self.leverage.unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_select_mainnet_and_spec_constants() {
        let d = DeriverseDataClientConfig::default();
        assert_eq!(d.effective_rpc_url(), "https://rpc-mainnet.deriverse.io");
        assert_eq!(d.effective_ws_url(), "wss://rpc-mainnet.deriverse.io");
        assert_eq!(d.effective_program_id(), PROGRAM_ID);
        assert_eq!(d.effective_program_version(), 1);
        assert_eq!(d.commitment, Commitment::Confirmed);
        let e = DeriverseExecClientConfig::default();
        assert_eq!(e.wire_leverage(), 0);
        assert!(e.keypair_path.is_none());
    }

    #[test]
    fn devnet_selection_is_both_overrides() {
        use crate::common::consts::{DEVNET_PROGRAM_ID, DEVNET_PROGRAM_VERSION};
        let d = DeriverseDataClientConfig {
            program_id: Some(DEVNET_PROGRAM_ID.to_string()),
            program_version: Some(DEVNET_PROGRAM_VERSION),
            ..Default::default()
        };
        assert_eq!(d.effective_program_id(), DEVNET_PROGRAM_ID);
        assert_eq!(d.effective_program_version(), 7);
    }

    #[test]
    fn debug_shows_path_not_key_material() {
        // keypair_path is a PATH — visible by design; no key bytes exist in
        // the config at all, so derived Debug cannot leak them.
        let e = DeriverseExecClientConfig {
            keypair_path: Some(PathBuf::from("secrets/deriverse-wallet-keypair.json")),
            ..Default::default()
        };
        let s = format!("{e:?}");
        assert!(s.contains("deriverse-wallet-keypair.json"));
    }
}
