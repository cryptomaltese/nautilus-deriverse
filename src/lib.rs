//! Bespoke Nautilus Trader venue adapter for **Deriverse** — a fully
//! on-chain CLOB on Solana. There is no gateway: market data is Solana RPC
//! `accountSubscribe` on the instrument account, every order/cancel is a
//! signed v0 transaction, and fills are decoded from own-transaction logs
//! confirmed by signature polling (spec §1.1).
//!
//! M1a scope: transport substrate (`rpc/`, `decode/`, `tx/`) + data client,
//! live-validated on mainnet spot. Execution client + Python layer are M1b.

pub mod common;
pub mod config;
pub mod data;
pub mod decode;
pub mod factories;
pub mod rpc;
pub mod tx;
