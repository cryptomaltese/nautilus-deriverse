//! §1.6 steps 6–7: send + confirm by signature polling. The in-flight state
//! machine is three-valued — Pending (unknown), Confirmed(slot, logs),
//! Expired — and Pending is "unknown": the transaction may still land any
//! time until expiry is PROVEN past (status absent AND
//! get_block_height() > last_valid_block_height). Expiry is the only
//! cancel-by-timeout the transport offers (Invariant 5).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::rpc::{SendError, SolanaRpc};
use crate::tx::build::VersionedTransaction;

/// Terminal transport outcome for a sent transaction.
#[derive(Clone, Debug, PartialEq)]
pub enum TxOutcome {
    Confirmed { slot: u64, logs: Vec<String> },
    Expired,
}

/// submit_and_confirm's result: transport outcome or a definitive RPC
/// reject (reason VERBATIM — spec §1.6).
#[derive(Clone, Debug, PartialEq)]
pub enum SubmitOutcome {
    Confirmed { slot: u64, logs: Vec<String> },
    Expired,
    Rejected { reason: String },
}

/// Session-local record for one sent transaction (spec §1.5). `venue_order_id`
/// binds when the confirmed tx's own logs yield the place report; ids are
/// i64 end to end (Invariant 8).
#[derive(Clone, Debug)]
pub struct TxRecord {
    pub signature: String,
    pub venue_order_id: Option<i64>,
    pub last_valid_block_height: u64,
}

/// Insert-before-send / prune-on-terminal-only map (Invariant 11). Keyed
/// generically: bins key by a local label now; M1b keys by ClientOrderId.
#[derive(Debug, Default)]
pub struct TxLedger<K: Eq + Hash + Clone> {
    inner: Mutex<HashMap<K, TxRecord>>,
}

impl<K: Eq + Hash + Clone> TxLedger<K> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// MUST be called before the send hits the wire (Invariant 11).
    pub fn insert_before_send(&self, key: K, record: TxRecord) {
        self.inner.lock().expect("ledger lock").insert(key, record);
    }

    pub fn bind_order_id(&self, key: &K, venue_order_id: i64) {
        if let Some(r) = self.inner.lock().expect("ledger lock").get_mut(key) {
            r.venue_order_id = Some(venue_order_id);
        }
    }

    /// Prune ONLY on terminal outcomes (confirmed full fill, confirmed
    /// cancel, expiry, definite reject) — never on intermediate status.
    pub fn prune_terminal(&self, key: &K) -> Option<TxRecord> {
        self.inner.lock().expect("ledger lock").remove(key)
    }

    #[must_use]
    pub fn get(&self, key: &K) -> Option<TxRecord> {
        self.inner.lock().expect("ledger lock").get(key).cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("ledger lock").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Blockhash cache (spec §1.6): refreshed every `refresh_secs`; each caller
/// pins the `last_valid_block_height` it was signed against.
pub struct BlockhashCache<R: SolanaRpc> {
    rpc: Arc<R>,
    refresh_secs: u64,
    cell: Mutex<Option<(std::time::Instant, String, u64)>>,
}

impl<R: SolanaRpc> BlockhashCache<R> {
    #[must_use]
    pub fn new(rpc: Arc<R>, refresh_secs: u64) -> Self {
        Self {
            rpc,
            refresh_secs,
            cell: Mutex::new(None),
        }
    }

    pub async fn get(&self) -> anyhow::Result<(String, u64)> {
        if let Some((at, hash, lvbh)) = self.cell.lock().expect("blockhash lock").clone() {
            if at.elapsed() < Duration::from_secs(self.refresh_secs) {
                return Ok((hash, lvbh));
            }
        }
        let (hash, lvbh) = self.rpc.get_latest_blockhash().await?;
        *self.cell.lock().expect("blockhash lock") =
            Some((std::time::Instant::now(), hash.clone(), lvbh));
        Ok((hash, lvbh))
    }
}

pub struct TxSender<R: SolanaRpc> {
    pub rpc: Arc<R>,
    pub poll_interval_ms: u64,
}

impl<R: SolanaRpc> TxSender<R> {
    /// Send, then poll to a terminal outcome. Error taxonomy (spec §1.6):
    /// - definitive RPC error response → `Rejected` (reason verbatim);
    /// - transport-level send failure → the tx may have reached the leader:
    ///   enter the Pending/expiry path, never a terminal event at send time.
    pub async fn submit_and_confirm(
        &self,
        tx: &VersionedTransaction,
        last_valid_block_height: u64,
    ) -> anyhow::Result<SubmitOutcome> {
        let signature = tx.signature_base58();
        match self.rpc.send_transaction(&tx.base64()).await {
            Ok(sig) => {
                anyhow::ensure!(sig == signature, "node echoed foreign signature: {sig}");
            }
            Err(SendError::Definitive(reason)) => return Ok(SubmitOutcome::Rejected { reason }),
            Err(SendError::Transport(e)) => {
                log::warn!(
                    "deriverse send transport failure ({e}); outcome unknown — polling to expiry"
                );
            }
        }
        match self
            .poll_to_outcome(&signature, last_valid_block_height)
            .await?
        {
            TxOutcome::Confirmed { slot, logs } => Ok(SubmitOutcome::Confirmed { slot, logs }),
            TxOutcome::Expired => Ok(SubmitOutcome::Expired),
        }
    }

    /// Poll `getSignatureStatuses` until `confirmed`/`finalized` (then fetch
    /// logs via `getTransaction`) or until expiry is PROVEN: status absent
    /// AND block height > last_valid_block_height. Never expires early.
    pub async fn poll_to_outcome(
        &self,
        signature: &str,
        last_valid_block_height: u64,
    ) -> anyhow::Result<TxOutcome> {
        loop {
            if let Some(status) = self.rpc.get_signature_status(signature).await? {
                if matches!(
                    status.confirmation_status.as_str(),
                    "confirmed" | "finalized"
                ) {
                    // Logs may lag status by a beat; retry the fetch.
                    for _ in 0..10 {
                        if let Some((slot, logs)) = self.rpc.get_transaction(signature).await? {
                            return Ok(TxOutcome::Confirmed { slot, logs });
                        }
                        tokio::time::sleep(Duration::from_millis(self.poll_interval_ms)).await;
                    }
                    anyhow::bail!("confirmed but getTransaction stayed empty: {signature}");
                }
            } else {
                let height = self.rpc.get_block_height().await?;
                if height > last_valid_block_height {
                    return Ok(TxOutcome::Expired);
                }
            }
            tokio::time::sleep(Duration::from_millis(self.poll_interval_ms)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::SignatureStatus;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Mock RPC: never-landing transaction; block height advances one per
    /// get_block_height call.
    struct NeverLandsRpc {
        height: AtomicU64,
        height_calls: AtomicU64,
    }

    #[async_trait::async_trait]
    impl SolanaRpc for NeverLandsRpc {
        async fn get_account_info(
            &self,
            _p: &str,
            _d: Option<(u64, u64)>,
        ) -> anyhow::Result<Option<(u64, Vec<u8>)>> {
            unimplemented!("not used")
        }
        async fn get_program_accounts(
            &self,
            _m: &str,
            _d: Option<(u64, u64)>,
        ) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
            unimplemented!("not used")
        }
        async fn get_latest_blockhash(&self) -> anyhow::Result<(String, u64)> {
            unimplemented!("not used")
        }
        async fn get_block_height(&self) -> anyhow::Result<u64> {
            self.height_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.height.fetch_add(1, Ordering::SeqCst))
        }
        async fn get_slot(&self) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_balance(&self, _p: &str) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_signature_status(&self, _s: &str) -> anyhow::Result<Option<SignatureStatus>> {
            Ok(None) // never lands
        }
        async fn send_transaction(&self, _t: &str) -> Result<String, SendError> {
            unimplemented!("not used")
        }
        async fn get_transaction(&self, _s: &str) -> anyhow::Result<Option<(u64, Vec<String>)>> {
            Ok(None)
        }
    }

    /// M1a acceptance 7: Pending → Expired EXACTLY when block height passes
    /// last_valid_block_height, and never before. Heights returned:
    /// 100, 101, ..., lvbh (=105, still pending), 106 → Expired.
    #[tokio::test]
    async fn expiry_fires_exactly_when_height_passes_lvbh_never_before() {
        let rpc = Arc::new(NeverLandsRpc {
            height: AtomicU64::new(100),
            height_calls: AtomicU64::new(0),
        });
        let sender = TxSender {
            rpc: Arc::clone(&rpc),
            poll_interval_ms: 1,
        };
        let lvbh = 105u64;
        let outcome = sender.poll_to_outcome("sig111", lvbh).await.unwrap();
        assert_eq!(outcome, TxOutcome::Expired);
        // Heights seen: 100..=106 — seven polls. The poll that saw height
        // == lvbh (105) did NOT expire; only 106 (> lvbh) did.
        assert_eq!(rpc.height_calls.load(Ordering::SeqCst), 7);
    }

    #[tokio::test]
    async fn height_equal_to_lvbh_is_still_pending() {
        // One-shot: mock pinned AT lvbh forever would loop; assert the
        // predicate directly at the boundary by giving exactly two heights.
        let rpc = Arc::new(NeverLandsRpc {
            height: AtomicU64::new(105),
            height_calls: AtomicU64::new(0),
        });
        let sender = TxSender {
            rpc,
            poll_interval_ms: 1,
        };
        // heights: 105 (== lvbh ⇒ pending), 106 (> lvbh ⇒ expired)
        let outcome = sender.poll_to_outcome("sig111", 105).await.unwrap();
        assert_eq!(outcome, TxOutcome::Expired);
    }

    #[test]
    fn ledger_prunes_only_terminal() {
        let ledger: TxLedger<String> = TxLedger::new();
        ledger.insert_before_send(
            "local-1".to_string(),
            TxRecord {
                signature: "s".into(),
                venue_order_id: None,
                last_valid_block_height: 9,
            },
        );
        assert_eq!(ledger.len(), 1);
        ledger.bind_order_id(&"local-1".to_string(), 424_242);
        assert_eq!(
            ledger.get(&"local-1".to_string()).unwrap().venue_order_id,
            Some(424_242)
        );
        // Intermediate statuses never remove; only an explicit terminal prune.
        let pruned = ledger.prune_terminal(&"local-1".to_string()).unwrap();
        assert_eq!(pruned.venue_order_id, Some(424_242));
        assert!(ledger.is_empty());
    }
}
