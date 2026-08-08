//! Adapter-local enums (spec §2.1).

/// Solana commitment level. Adapter default: `Confirmed`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Commitment {
    Processed,
    #[default]
    Confirmed,
    Finalized,
}

impl Commitment {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Processed => "processed",
            Self::Confirmed => "confirmed",
            Self::Finalized => "finalized",
        }
    }
}
