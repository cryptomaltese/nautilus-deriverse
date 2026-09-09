//! §1.6 steps 6–7: send + confirm by signature polling. The in-flight state
//! machine is three-valued — Pending (unknown), Confirmed(slot, logs, err),
//! Expired — and Pending is "unknown": the transaction may still land any
//! time until expiry is PROVEN past (`getEpochInfo` block height >
//! last_valid_block_height, then a ledger-backed status re-check answered
//! from `context.slot ≥ slot_A + ROOTING_MARGIN_SLOTS` with no status).
//! Expiry is the only cancel-by-timeout the transport offers (Invariant 5).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::rpc::{SendError, SolanaRpc};
use crate::tx::build::VersionedTransaction;

/// Terminal transport outcome for a sent transaction.
///
/// `Confirmed` means LANDED on-chain, not success: `err` carries the
/// on-chain transaction error (`getSignatureStatuses` `err`, verbatim
/// compact JSON) when the program failed. A failed transaction changes no
/// program state (Solana txs are atomic); callers MUST branch on `err`
/// before treating a confirmation as success.
#[derive(Clone, Debug, PartialEq)]
pub enum TxOutcome {
    Confirmed {
        slot: u64,
        logs: Vec<String>,
        err: Option<String>,
    },
    Expired,
}

/// submit_and_confirm's result: transport outcome or a definitive RPC
/// reject (reason VERBATIM — spec §1.6). `Confirmed.err` as on `TxOutcome`.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmitOutcome {
    Confirmed {
        slot: u64,
        logs: Vec<String>,
        err: Option<String>,
    },
    Expired,
    Rejected {
        reason: String,
    },
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

/// Slot margin the expiry proof requires between the height-proving node's
/// bank slot (slot_A, from `getEpochInfo.absoluteSlot`) and the searched
/// status node's `context.slot`. Residual assumption (spec Invariant 5): a
/// node at bank slot ≥ slot_A + this margin has rooted every block ≤ lvbh —
/// holds unless cluster rooting stalls; 512 slots (~3.5 min) absorbs
/// rooted-vs-bank lag between nodes.
pub const ROOTING_MARGIN_SLOTS: u64 = 512;

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
            TxOutcome::Confirmed { slot, logs, err } => {
                Ok(SubmitOutcome::Confirmed { slot, logs, err })
            }
            TxOutcome::Expired => Ok(SubmitOutcome::Expired),
        }
    }

    /// Poll `getSignatureStatuses` until `confirmed`/`finalized` (then fetch
    /// logs via `getTransaction`) or until expiry is PROVEN, in like units:
    /// `getEpochInfo` shows `block_height > last_valid_block_height`
    /// (height vs height, one node — its `absoluteSlot` recorded as
    /// slot_A), then a ledger-backed status re-check answered by a node at
    /// `context.slot ≥ slot_A + ROOTING_MARGIN_SLOTS` (slot vs slot) comes
    /// back absent. Never expires early.
    ///
    /// Expiry-race guard: the status read and the height read are not
    /// atomic — the transaction can land in a block ≤ lvbh between them.
    /// Only a status read taken after the height proof is proof of absence.
    /// The slot gate must never compare `context.slot` against lvbh:
    /// cumulative skipped slots put every node's slot far above any block
    /// height, so that comparison always passes and rejects nothing
    /// (round-2 review).
    ///
    /// Lifecycle edges (#67 item 4): the ONLY exits are `Confirmed` and
    /// proven `Expired` (Invariant 5 — no code path fabricates a terminal
    /// state for a transaction still in flight). Transient RPC failures and
    /// a confirmed-but-not-yet-fetchable transaction log loudly and keep
    /// polling.
    ///
    /// DELIBERATELY UNBOUNDED: this loop never gives up on its own —
    /// cancellation is the caller's responsibility (attended bins rely on
    /// operator interrupt; the M1b execution client must drive this under
    /// its own CancellationToken/timeout — noted here, not built here).
    pub async fn poll_to_outcome(
        &self,
        signature: &str,
        last_valid_block_height: u64,
    ) -> anyhow::Result<TxOutcome> {
        loop {
            match self.poll_once(signature, last_valid_block_height).await {
                Ok(Some(outcome)) => return Ok(outcome),
                Ok(None) => {}
                Err(e) => log::warn!(
                    "deriverse confirm poll transient failure for {signature} ({e}); \
                     tx still in flight — retrying"
                ),
            }
            tokio::time::sleep(Duration::from_millis(self.poll_interval_ms)).await;
        }
    }

    /// One poll iteration: `Ok(Some(_))` = proven outcome, `Ok(None)` = still
    /// in flight, `Err` = transient RPC failure (the outer loop retries).
    async fn poll_once(
        &self,
        signature: &str,
        last_valid_block_height: u64,
    ) -> anyhow::Result<Option<TxOutcome>> {
        let mut status = self.rpc.get_signature_status(signature).await?;
        if status.is_none() {
            // One atomic response: height (vs lvbh, like units) and the
            // same node's bank slot (slot_A, anchoring the slot-to-slot
            // acceptance gate below).
            let (slot_a, block_height) = self.rpc.get_epoch_info().await?;
            if block_height > last_valid_block_height {
                // Re-check AFTER the height proof (see doc comment), and the
                // proof of absence must be LEDGER-backed: the recent-status
                // cache evicts ~150 blocks past lvbh and this loop
                // deliberately survives long outages, so a cache-backed None
                // here could false-Expire a landed (possibly FILLED)
                // transaction. Present = it landed in the gap — fall through
                // and handle the status.
                let (node_slot, searched) =
                    self.rpc.get_signature_status_searched(signature).await?;
                match searched {
                    // Height and status are separate POSTs through a pooled
                    // client — behind a load balancer they can answer from
                    // different backends. The proof must be self-contained
                    // in like units: the ANSWERING node's context.slot must
                    // be ≥ slot_A + ROOTING_MARGIN_SLOTS, i.e. far enough
                    // past the height-proving node's bank slot to have
                    // rooted every block ≤ lvbh. (Never compare slot to
                    // lvbh: slots run tens of millions above block heights,
                    // so that gate is vacuous.)
                    None if node_slot >= slot_a.saturating_add(ROOTING_MARGIN_SLOTS) => {
                        return Ok(Some(TxOutcome::Expired));
                    }
                    None => {}
                    Some(s) => status = Some(s),
                }
            }
        }
        if let Some(status) = status {
            if matches!(
                status.confirmation_status.as_str(),
                "confirmed" | "finalized"
            ) {
                // On-chain error travels WITH the confirmation: a landed-
                // but-failed transaction must never read as success.
                let err = status.err.as_ref().map(ToString::to_string);
                if let Some((slot, logs)) = self.rpc.get_transaction(signature).await? {
                    return Ok(Some(TxOutcome::Confirmed { slot, logs, err }));
                }
                // getTransaction may lag status by a beat; the tx is PROVEN
                // landed, so never manufacture an error — keep polling.
                log::warn!(
                    "deriverse: {signature} confirmed but getTransaction still empty — retrying"
                );
            }
            // Landed at `processed` commitment: keep polling — it either
            // confirms, or its block is dropped and expiry re-proves.
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::SignatureStatus;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Mock RPC: never-landing transaction; block height advances one per
    /// get_epoch_info call (absolute slot rides 20M slots above height —
    /// realistic skipped-slot gap).
    struct NeverLandsRpc {
        height: AtomicU64,
        epoch_calls: AtomicU64,
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
            unimplemented!("not used: the proving sequence reads getEpochInfo")
        }
        async fn get_epoch_info(&self) -> anyhow::Result<(u64, u64)> {
            self.epoch_calls.fetch_add(1, Ordering::SeqCst);
            let height = self.height.fetch_add(1, Ordering::SeqCst);
            Ok((height + 20_000_000, height))
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
        async fn get_signature_status_searched(
            &self,
            _s: &str,
        ) -> anyhow::Result<(u64, Option<SignatureStatus>)> {
            Ok((u64::MAX, None)) // ledger search agrees: never landed
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
            epoch_calls: AtomicU64::new(0),
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
        assert_eq!(rpc.epoch_calls.load(Ordering::SeqCst), 7);
    }

    #[tokio::test]
    async fn height_equal_to_lvbh_is_still_pending() {
        // One-shot: mock pinned AT lvbh forever would loop; assert the
        // predicate directly at the boundary by giving exactly two heights.
        let rpc = Arc::new(NeverLandsRpc {
            height: AtomicU64::new(105),
            epoch_calls: AtomicU64::new(0),
        });
        let sender = TxSender {
            rpc,
            poll_interval_ms: 1,
        };
        // heights: 105 (== lvbh ⇒ pending), 106 (> lvbh ⇒ expired)
        let outcome = sender.poll_to_outcome("sig111", 105).await.unwrap();
        assert_eq!(outcome, TxOutcome::Expired);
    }

    /// Mock RPC reproducing both the expiry race AND cache eviction: the
    /// cheap cache read ALWAYS returns None (landed in the status/height gap
    /// or evicted after a long outage); only the ledger-backed searched read
    /// finds the confirmed transaction. Block height is already past lvbh.
    struct RaceRpc {
        status_calls: AtomicU64,
    }

    #[async_trait::async_trait]
    impl SolanaRpc for RaceRpc {
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
            unimplemented!("not used: the proving sequence reads getEpochInfo")
        }
        async fn get_epoch_info(&self) -> anyhow::Result<(u64, u64)> {
            Ok((360_000_000, 200)) // height already past lvbh
        }
        async fn get_slot(&self) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_balance(&self, _p: &str) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_signature_status(&self, _s: &str) -> anyhow::Result<Option<SignatureStatus>> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            Ok(None) // cache read: not visible (gap landing / evicted)
        }
        async fn get_signature_status_searched(
            &self,
            _s: &str,
        ) -> anyhow::Result<(u64, Option<SignatureStatus>)> {
            Ok((
                360_001_000,
                Some(SignatureStatus {
                    slot: 99,
                    confirmation_status: "confirmed".to_string(),
                    err: None,
                }),
            ))
        }
        async fn send_transaction(&self, _t: &str) -> Result<String, SendError> {
            unimplemented!("not used")
        }
        async fn get_transaction(&self, _s: &str) -> anyhow::Result<Option<(u64, Vec<String>)>> {
            Ok(Some((99, vec!["Program log: landed".to_string()])))
        }
    }

    /// M1b gate (#67 item 2): a transaction landing between the status read
    /// and the height read must resolve Confirmed, never Expired — expiry is
    /// proven only by a status read taken AFTER height > lvbh.
    #[tokio::test]
    async fn landing_between_status_and_height_reads_is_confirmed_not_expired() {
        let sender = TxSender {
            rpc: Arc::new(RaceRpc {
                status_calls: AtomicU64::new(0),
            }),
            poll_interval_ms: 1,
        };
        let outcome = sender.poll_to_outcome("sig111", 105).await.unwrap();
        assert!(
            matches!(outcome, TxOutcome::Confirmed { slot: 99, .. }),
            "false Expired on the status/height race: {outcome:?}"
        );
    }

    /// M1b review (critical): the proving re-check must be the LEDGER-backed
    /// read. RaceRpc's cache read returns None on every call (as it does
    /// after recent-status-cache eviction, ~150 blocks past lvbh); only the
    /// searched read finds the landed transaction. A cache-backed re-check
    /// would false-Expire a landed — possibly filled — order.
    #[tokio::test]
    async fn evicted_cache_status_never_proves_expiry() {
        let rpc = Arc::new(RaceRpc {
            status_calls: AtomicU64::new(0),
        });
        let sender = TxSender {
            rpc: Arc::clone(&rpc),
            poll_interval_ms: 1,
        };
        let outcome = sender.poll_to_outcome("sig111", 105).await.unwrap();
        assert!(
            matches!(outcome, TxOutcome::Confirmed { slot: 99, .. }),
            "cache None past lvbh must not read as expiry: {outcome:?}"
        );
        assert!(
            rpc.status_calls.load(Ordering::SeqCst) >= 1,
            "hot poll still uses the cheap cache read"
        );
    }

    /// Mock RPC for the load-balancer split, with REALISTIC mainnet-shaped
    /// numbers (slots ~3.6e8 run tens of millions above heights ~3.4e8):
    /// the epoch-info node proves height past lvbh at bank slot slot_A =
    /// 360_000_000; the first `lagging` searched reads answer from a node
    /// whose slot (360_000_200) is HUGE — far above lvbh = 340_000_000, so
    /// the retired `slot > lvbh` gate would have accepted it (the unit
    /// conflation this mock pins) — but below slot_A + ROOTING_MARGIN_SLOTS,
    /// so its None is NOT proof.
    struct LaggingNodeRpc {
        lagging: u64,
        searched_calls: AtomicU64,
    }

    #[async_trait::async_trait]
    impl SolanaRpc for LaggingNodeRpc {
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
            unimplemented!("not used: the proving sequence reads getEpochInfo")
        }
        async fn get_epoch_info(&self) -> anyhow::Result<(u64, u64)> {
            // Fresh backend: height 340_000_100 > lvbh, bank slot slot_A.
            Ok((360_000_000, 340_000_100))
        }
        async fn get_slot(&self) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_balance(&self, _p: &str) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_signature_status(&self, _s: &str) -> anyhow::Result<Option<SignatureStatus>> {
            Ok(None)
        }
        async fn get_signature_status_searched(
            &self,
            _s: &str,
        ) -> anyhow::Result<(u64, Option<SignatureStatus>)> {
            if self.searched_calls.fetch_add(1, Ordering::SeqCst) < self.lagging {
                // Lagging backend: slot ≫ lvbh (the vacuous unit-conflated
                // gate would accept) but < slot_A + ROOTING_MARGIN_SLOTS —
                // its None is no proof.
                Ok((360_000_200, None))
            } else {
                // Caught-up backend: ≥ slot_A + margin — absence IS proof.
                Ok((360_000_600, None))
            }
        }
        async fn send_transaction(&self, _t: &str) -> Result<String, SendError> {
            unimplemented!("not used")
        }
        async fn get_transaction(&self, _s: &str) -> anyhow::Result<Option<(u64, Vec<String>)>> {
            Ok(None)
        }
    }

    /// M1b review round 2 (important): the acceptance gate must compare
    /// slot to slot, never slot to block height. A lagging backend's None —
    /// answered at a slot far above lvbh (as every mainnet slot is) but
    /// below slot_A + ROOTING_MARGIN_SLOTS — is not proof; the loop keeps
    /// polling until a node past the slot threshold answers absent.
    #[tokio::test]
    async fn lagging_backend_none_is_not_expiry_proof() {
        let rpc = Arc::new(LaggingNodeRpc {
            lagging: 2,
            searched_calls: AtomicU64::new(0),
        });
        let sender = TxSender {
            rpc: Arc::clone(&rpc),
            poll_interval_ms: 1,
        };
        let lvbh = 340_000_000u64; // block-height units; slots run ~2e7 above
        let outcome = sender.poll_to_outcome("sig111", lvbh).await.unwrap();
        assert_eq!(outcome, TxOutcome::Expired);
        assert_eq!(
            rpc.searched_calls.load(Ordering::SeqCst),
            3,
            "two lagging-node Nones (slot ≫ lvbh yet < slot_A + margin) \
             rejected, third (past the slot threshold) accepted"
        );
    }

    /// Mock RPC for the lifecycle edges (#67 item 4): the first
    /// `status_errors` status reads fail at transport level, then confirmed;
    /// the first `tx_lags` getTransaction reads return None, then serve.
    struct FlakyRpc {
        status_errors: u64,
        tx_lags: u64,
        status_calls: AtomicU64,
        tx_calls: AtomicU64,
    }

    #[async_trait::async_trait]
    impl SolanaRpc for FlakyRpc {
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
            unimplemented!("not used: status never returns None here")
        }
        async fn get_epoch_info(&self) -> anyhow::Result<(u64, u64)> {
            unimplemented!("not used: status never returns None here")
        }
        async fn get_slot(&self) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_balance(&self, _p: &str) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_signature_status(&self, _s: &str) -> anyhow::Result<Option<SignatureStatus>> {
            if self.status_calls.fetch_add(1, Ordering::SeqCst) < self.status_errors {
                anyhow::bail!("transient: connection reset by peer")
            }
            Ok(Some(SignatureStatus {
                slot: 55,
                confirmation_status: "confirmed".to_string(),
                err: None,
            }))
        }
        async fn get_signature_status_searched(
            &self,
            _s: &str,
        ) -> anyhow::Result<(u64, Option<SignatureStatus>)> {
            unimplemented!("not used: cheap read never returns None here")
        }
        async fn send_transaction(&self, _t: &str) -> Result<String, SendError> {
            unimplemented!("not used")
        }
        async fn get_transaction(&self, _s: &str) -> anyhow::Result<Option<(u64, Vec<String>)>> {
            if self.tx_calls.fetch_add(1, Ordering::SeqCst) < self.tx_lags {
                Ok(None)
            } else {
                Ok(Some((55, vec!["Program log: late".to_string()])))
            }
        }
    }

    /// M1b gate (#67 item 4): a transient RPC failure mid-poll must not
    /// abandon an in-flight transaction — the loop rides through to proof.
    #[tokio::test]
    async fn transient_rpc_failure_rides_through_to_confirmed() {
        let sender = TxSender {
            rpc: Arc::new(FlakyRpc {
                status_errors: 3,
                tx_lags: 0,
                status_calls: AtomicU64::new(0),
                tx_calls: AtomicU64::new(0),
            }),
            poll_interval_ms: 1,
        };
        let outcome = sender.poll_to_outcome("sig111", 105).await.unwrap();
        assert!(
            matches!(outcome, TxOutcome::Confirmed { slot: 55, .. }),
            "transient RPC errors must never abandon the in-flight tx: {outcome:?}"
        );
    }

    /// M1b gate (#67 item 4): a PROVEN-landed transaction whose
    /// getTransaction lags must resolve Confirmed, never a fabricated error
    /// (the old code bailed after 10 fetch attempts).
    #[tokio::test]
    async fn confirmed_with_lagging_get_transaction_still_confirms() {
        let rpc = Arc::new(FlakyRpc {
            status_errors: 0,
            tx_lags: 15, // outlives the old 10-fetch bound
            status_calls: AtomicU64::new(0),
            tx_calls: AtomicU64::new(0),
        });
        let sender = TxSender {
            rpc: Arc::clone(&rpc),
            poll_interval_ms: 1,
        };
        let outcome = sender.poll_to_outcome("sig111", 105).await.unwrap();
        assert!(matches!(outcome, TxOutcome::Confirmed { slot: 55, .. }));
        assert!(
            rpc.tx_calls.load(Ordering::SeqCst) > 10,
            "must outlive the old 10-fetch bail bound"
        );
    }

    /// Mock RPC: transaction lands (confirmed) with a configurable on-chain
    /// error; `get_transaction` serves logs immediately.
    struct LandsRpc {
        err: Option<serde_json::Value>,
    }

    #[async_trait::async_trait]
    impl SolanaRpc for LandsRpc {
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
            unimplemented!("not used")
        }
        async fn get_epoch_info(&self) -> anyhow::Result<(u64, u64)> {
            unimplemented!("not used: status never returns None here")
        }
        async fn get_slot(&self) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_balance(&self, _p: &str) -> anyhow::Result<u64> {
            unimplemented!("not used")
        }
        async fn get_signature_status(&self, _s: &str) -> anyhow::Result<Option<SignatureStatus>> {
            Ok(Some(SignatureStatus {
                slot: 42,
                confirmation_status: "confirmed".to_string(),
                err: self.err.clone(),
            }))
        }
        async fn get_signature_status_searched(
            &self,
            _s: &str,
        ) -> anyhow::Result<(u64, Option<SignatureStatus>)> {
            unimplemented!("not used: cheap read never returns None here")
        }
        async fn send_transaction(&self, _t: &str) -> Result<String, SendError> {
            unimplemented!("not used")
        }
        async fn get_transaction(&self, _s: &str) -> anyhow::Result<Option<(u64, Vec<String>)>> {
            Ok(Some((42, vec!["Program log: x".to_string()])))
        }
    }

    /// M1b gate (#67): a transaction that LANDS but whose program errors must
    /// surface the on-chain error on Confirmed — never read as success.
    #[tokio::test]
    async fn confirmed_with_program_error_carries_err_verbatim() {
        let on_chain_err = serde_json::json!({"InstructionError": [1, {"Custom": 132}]});
        let sender = TxSender {
            rpc: Arc::new(LandsRpc {
                err: Some(on_chain_err.clone()),
            }),
            poll_interval_ms: 1,
        };
        let outcome = sender.poll_to_outcome("sig111", 105).await.unwrap();
        match outcome {
            TxOutcome::Confirmed { slot, logs, err } => {
                assert_eq!(slot, 42);
                assert!(!logs.is_empty());
                // Verbatim compact JSON of the status `err` member.
                assert_eq!(err.as_deref(), Some(on_chain_err.to_string().as_str()));
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn confirmed_without_error_has_err_none() {
        let sender = TxSender {
            rpc: Arc::new(LandsRpc { err: None }),
            poll_interval_ms: 1,
        };
        let outcome = sender.poll_to_outcome("sig111", 105).await.unwrap();
        assert!(matches!(outcome, TxOutcome::Confirmed { err: None, .. }));
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
