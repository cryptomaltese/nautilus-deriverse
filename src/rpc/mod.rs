//! Solana JSON-RPC transport (spec §2.1): HTTP client + WS subscription
//! client. `SolanaRpc` is the mockable seam — `tx/send.rs`'s expiry test
//! (M1a acceptance 7) and M1b's lifecycle tests run against a mock impl.

pub mod http;
pub mod ws;

pub use http::{decode_account_data, DeriverseRpcClient};

/// One entry of `getSignatureStatuses`.
#[derive(Clone, Debug)]
pub struct SignatureStatus {
    pub slot: u64,
    pub confirmation_status: String,
    pub err: Option<serde_json::Value>,
}

/// Send-error split (spec §1.6): a definitive RPC error response (preflight
/// or program error; skip_preflight = false) is terminal — reason verbatim;
/// a transport-level failure is UNKNOWN (the tx may have reached the leader)
/// and resolves only via the Pending/expiry path (Invariant 5).
#[derive(Debug)]
pub enum SendError {
    Definitive(String),
    Transport(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Definitive(m) => write!(f, "definitive RPC reject: {m}"),
            Self::Transport(m) => write!(f, "transport failure (outcome unknown): {m}"),
        }
    }
}
impl std::error::Error for SendError {}

#[async_trait::async_trait]
pub trait SolanaRpc: Send + Sync {
    /// `getAccountInfo` with `encoding: base64+zstd` (+ optional dataSlice).
    /// `Ok(None)` = account does not exist. Returns `(slot, bytes)`.
    async fn get_account_info(
        &self,
        pubkey: &str,
        data_slice: Option<(u64, u64)>,
    ) -> anyhow::Result<Option<(u64, Vec<u8>)>>;
    /// `getProgramAccounts` filtered by a memcmp at offset 0 (base58 of the
    /// 8-byte discriminator: tag u32 LE ++ version u32 LE — NOT the PDA seed
    /// order). Returns `(pubkey_base58, bytes)` per account.
    async fn get_program_accounts(
        &self,
        memcmp_offset0_base58: &str,
        data_slice: Option<(u64, u64)>,
    ) -> anyhow::Result<Vec<(String, Vec<u8>)>>;
    /// `(blockhash_base58, last_valid_block_height)`.
    async fn get_latest_blockhash(&self) -> anyhow::Result<(String, u64)>;
    async fn get_block_height(&self) -> anyhow::Result<u64>;
    async fn get_slot(&self) -> anyhow::Result<u64>;
    /// Lamports.
    async fn get_balance(&self, pubkey: &str) -> anyhow::Result<u64>;
    async fn get_signature_status(
        &self,
        signature: &str,
    ) -> anyhow::Result<Option<SignatureStatus>>;
    /// `sendTransaction` (base64, skipPreflight = false).
    async fn send_transaction(&self, tx_base64: &str) -> Result<String, SendError>;
    /// `(slot, log_messages)` of a confirmed transaction; `Ok(None)` if the
    /// node does not (yet) know the signature.
    async fn get_transaction(&self, signature: &str) -> anyhow::Result<Option<(u64, Vec<String>)>>;
}
