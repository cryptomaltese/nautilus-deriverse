//! Deriverse live `DataClient` (spec §2.4): `accountSubscribe` on the
//! instrument account → `QuoteTick` from the spot ladders' level 0.
//!
//! Invariants (spec §3):
//! 1. Per-instrument `last_slot` strictly monotone — `slot ≤ last_slot`
//!    drops the notification, no emit. `ts_event` = wall-clock receive time
//!    (NT QuoteTick carries no slot field); slot never enters the tick.
//! 2. Staleness alarm: instrument lag vs the slotSubscribe heartbeat beyond
//!    `staleness_threshold_slots` marks the feed degraded LOUDLY; the M1b
//!    exec client's MARKET gate reads the same handle (BboCache pattern).
//! 3. WS is a latency hint, never truth: a periodic REST reconcile
//!    (`get_account_info`, dataSlice (0, 2344)) runs every
//!    `reconcile_interval_secs`; on divergence the REST read WINS (its
//!    quote is emitted) and the divergence is counted.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use solana_pubkey::Pubkey;
use tokio_util::sync::CancellationToken;

use nautilus_common::clients::DataClient;
use nautilus_common::live::runner::get_data_event_sender;
use nautilus_common::live::runtime::get_runtime;
use nautilus_common::messages::data::{
    DataResponse, InstrumentResponse, InstrumentsResponse, RequestInstrument, RequestInstruments,
    SubscribeQuotes,
};
use nautilus_common::messages::DataEvent;
use nautilus_core::datetime::datetime_to_unix_nanos;
use nautilus_core::time::{get_atomic_clock_realtime, AtomicTime};
use nautilus_core::UnixNanos;
use nautilus_model::data::{Data, QuoteTick};
use nautilus_model::enums::CurrencyType;
use nautilus_model::identifiers::{ClientId, InstrumentId, Symbol, Venue};
use nautilus_model::instruments::{CurrencyPair, Instrument, InstrumentAny};
use nautilus_model::types::{Currency, Price, Quantity};

use crate::common::consts::{DERIVERSE_VENUE, INSTR_SNAPSHOT_LEN, PRICE_PRECISION};
use crate::common::symbol::{spot_symbol, ticker_for_mint};
use crate::config::DeriverseDataClientConfig;
use crate::decode::instr::{decode_instr_account, InstrSnapshot};
use crate::rpc::ws::DeriverseRpcSubscriptionClient;
use crate::rpc::{DeriverseRpcClient, SolanaRpc};
use crate::tx::accounts::{resolve_directory, InstrMeta, VenueDirectory};

// ── FeedFreshness ──────────────────────────────────────────────────────────

#[derive(Default)]
struct FreshnessInner {
    heartbeat_slot: AtomicU64,
    per_instrument: RwLock<HashMap<InstrumentId, u64>>,
    degraded: RwLock<HashMap<InstrumentId, bool>>,
    last_tob: RwLock<HashMap<InstrumentId, (i64, i64)>>,
    quotes_emitted: AtomicU64,
    drops_stale: AtomicU64,
    reconcile_agreements: AtomicU64,
    reconcile_divergences: AtomicU64,
    staleness_threshold_slots: AtomicU64,
}

/// Clone-cheap shared freshness handle (spec §2.4 — the pacifica BboCache
/// pattern: factory constructs ONE, data client writes, M1b exec reads).
#[derive(Clone, Default)]
pub struct FeedFreshness {
    inner: Arc<FreshnessInner>,
}

impl std::fmt::Debug for FeedFreshness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedFreshness")
            .field("heartbeat_slot", &self.heartbeat_slot())
            .field("quotes_emitted", &self.quotes_emitted_count())
            .finish_non_exhaustive()
    }
}

impl FeedFreshness {
    #[must_use]
    pub fn new(staleness_threshold_slots: u64) -> Self {
        let s = Self::default();
        s.inner
            .staleness_threshold_slots
            .store(staleness_threshold_slots, Ordering::Relaxed);
        s
    }

    /// Strictly-monotone advance; `false` ⇒ stale notification ⇒ caller
    /// DROPS it (Invariant 1).
    pub fn try_advance(&self, id: InstrumentId, slot: u64) -> bool {
        let mut map = self.inner.per_instrument.write().expect("freshness lock");
        match map.get(&id) {
            Some(last) if slot <= *last => {
                self.inner.drops_stale.fetch_add(1, Ordering::Relaxed);
                false
            }
            _ => {
                map.insert(id, slot);
                true
            }
        }
    }

    #[must_use]
    pub fn last_slot(&self, id: &InstrumentId) -> Option<u64> {
        self.inner.per_instrument.read().expect("freshness lock").get(id).copied()
    }

    pub fn note_heartbeat(&self, slot: u64) {
        self.inner.heartbeat_slot.fetch_max(slot, Ordering::Relaxed);
    }

    #[must_use]
    pub fn heartbeat_slot(&self) -> u64 {
        self.inner.heartbeat_slot.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn staleness_threshold_slots(&self) -> u64 {
        self.inner.staleness_threshold_slots.load(Ordering::Relaxed)
    }

    /// Store the configured staleness threshold into this (shared) handle.
    /// The factory calls this in `create()` — a `Default`-constructed shared
    /// handle otherwise carries threshold 0 and the watchdog hair-triggers
    /// DEGRADED (review F1).
    pub fn set_staleness_threshold_slots(&self, staleness_threshold_slots: u64) {
        self.inner
            .staleness_threshold_slots
            .store(staleness_threshold_slots, Ordering::Relaxed);
    }

    pub fn set_degraded(&self, id: InstrumentId, degraded: bool) {
        self.inner.degraded.write().expect("freshness lock").insert(id, degraded);
    }

    #[must_use]
    pub fn is_degraded(&self, id: &InstrumentId) -> bool {
        self.inner.degraded.read().expect("freshness lock").get(id).copied().unwrap_or(false)
    }

    pub fn set_last_tob(&self, id: InstrumentId, bid_px: i64, ask_px: i64) {
        self.inner.last_tob.write().expect("freshness lock").insert(id, (bid_px, ask_px));
    }

    #[must_use]
    pub fn last_tob(&self, id: &InstrumentId) -> Option<(i64, i64)> {
        self.inner.last_tob.read().expect("freshness lock").get(id).copied()
    }

    pub fn count_quote(&self) {
        self.inner.quotes_emitted.fetch_add(1, Ordering::Relaxed);
    }
    pub fn count_reconcile_agreement(&self) {
        self.inner.reconcile_agreements.fetch_add(1, Ordering::Relaxed);
    }
    pub fn count_reconcile_divergence(&self) {
        self.inner.reconcile_divergences.fetch_add(1, Ordering::Relaxed);
    }
    #[must_use]
    pub fn quotes_emitted_count(&self) -> u64 {
        self.inner.quotes_emitted.load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn drops_stale_count(&self) -> u64 {
        self.inner.drops_stale.load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn reconcile_agreement_count(&self) -> u64 {
        self.inner.reconcile_agreements.load(Ordering::Relaxed)
    }
    #[must_use]
    pub fn reconcile_divergence_count(&self) -> u64 {
        self.inner.reconcile_divergences.load(Ordering::Relaxed)
    }
}

// ── QuoteTick mapping ──────────────────────────────────────────────────────

/// Spot ladders' level 0 → QuoteTick (spec §1.3). `None` when either side
/// is empty (a one-sided book emits nothing — QuoteTick needs both sides).
/// `ts` is BOTH ts_event and ts_init (wall-clock receive time; Invariant 1).
#[must_use]
pub fn snapshot_to_quote(
    snap: &InstrSnapshot,
    instrument_id: InstrumentId,
    price_precision: u8,
    size_precision: u8,
    size_decs: u32,
    ts: UnixNanos,
) -> Option<QuoteTick> {
    let bid = snap.spot_bids.top()?;
    let ask = snap.spot_asks.top()?;
    let px = |raw: i64| Price::new(raw as f64 / 1e9, price_precision);
    let qty = |raw: i64| Quantity::new(raw as f64 / 10f64.powi(size_decs as i32), size_precision);
    Some(QuoteTick::new(
        instrument_id,
        px(bid.px),
        px(ask.px),
        qty(bid.qty),
        qty(ask.qty),
        ts,
        ts,
    ))
}

// ── Instrument construction ────────────────────────────────────────────────

fn get_currency(code: &str) -> Currency {
    Currency::try_from_str(code).unwrap_or_else(|| {
        let currency = Currency::new(code, 8, 0, code, CurrencyType::Crypto);
        if let Err(e) = Currency::register(currency, false) {
            log::error!("failed to register currency '{code}': {e}");
        }
        currency
    })
}

/// One spot CurrencyPair per instrument (spec §1.8). Price precision from
/// DF (9 dp); size precision from asset_token_decs_count (clamped to NT's
/// fixed-point limit).
fn instrument_from_meta(
    meta: &InstrMeta,
    dir: &VenueDirectory,
    ts_init: UnixNanos,
) -> Option<(InstrumentId, InstrumentAny)> {
    let asset = dir.tokens.get(&meta.asset_token_id)?;
    let crncy = dir.tokens.get(&meta.crncy_token_id)?;
    let asset_ticker = ticker_for_mint(&asset.mint.to_string());
    let crncy_ticker = ticker_for_mint(&crncy.mint.to_string());
    let symbol = spot_symbol(&asset_ticker, &crncy_ticker);
    let instrument_id = InstrumentId::from(format!("{symbol}.{}", *DERIVERSE_VENUE).as_str());
    let size_precision = meta.asset_decs.min(9) as u8;
    let pair = CurrencyPair::new_checked(
        instrument_id,
        Symbol::from(symbol.as_str()),
        get_currency(&asset_ticker),
        get_currency(&crncy_ticker),
        PRICE_PRECISION,
        size_precision,
        Price::new(1e-9, PRICE_PRECISION),
        Quantity::new(10f64.powi(-i32::from(size_precision)), size_precision),
        None, None, None, None, None, None, None, None, None, None, None, None, None,
        ts_init,
        ts_init,
    )
    .map_err(|e| log::error!("instrument build failed for {symbol}: {e}"))
    .ok()?;
    Some((instrument_id, InstrumentAny::CurrencyPair(pair)))
}

// ── Data client ────────────────────────────────────────────────────────────

pub struct DeriverseDataClient {
    client_id: ClientId,
    config: DeriverseDataClientConfig,
    rpc: Arc<DeriverseRpcClient>,
    ws: Arc<DeriverseRpcSubscriptionClient>,
    freshness: FeedFreshness,
    directory: Arc<RwLock<Option<VenueDirectory>>>,
    instruments: Arc<RwLock<HashMap<InstrumentId, InstrumentAny>>>,
    /// InstrumentId → (account pubkey, instr meta) for subscribed markets.
    subscriptions: Arc<RwLock<HashMap<InstrumentId, InstrMeta>>>,
    is_connected: AtomicBool,
    cancel: CancellationToken,
    clock: &'static AtomicTime,
}

impl std::fmt::Debug for DeriverseDataClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeriverseDataClient")
            .field("client_id", &self.client_id)
            .field("is_connected", &self.is_connected.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl DeriverseDataClient {
    pub fn new(client_id: ClientId, config: DeriverseDataClientConfig) -> anyhow::Result<Self> {
        let rpc = Arc::new(DeriverseRpcClient::new(
            config.effective_rpc_url(),
            config.effective_program_id(),
            config.commitment,
            config.http_timeout_secs,
        )?);
        let ws = Arc::new(DeriverseRpcSubscriptionClient::new(
            config.effective_ws_url(),
            config.commitment,
        ));
        let freshness = FeedFreshness::new(config.staleness_threshold_slots);
        Ok(Self {
            client_id,
            config,
            rpc,
            ws,
            freshness,
            directory: Arc::new(RwLock::new(None)),
            instruments: Arc::new(RwLock::new(HashMap::new())),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            is_connected: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            clock: get_atomic_clock_realtime(),
        })
    }

    /// Factory wiring: share an external FeedFreshness (call before
    /// connect()). The same handle goes to the M1b exec factory.
    #[must_use]
    pub fn with_freshness(mut self, freshness: FeedFreshness) -> Self {
        self.freshness = freshness;
        self
    }

    #[must_use]
    pub fn freshness(&self) -> FeedFreshness {
        self.freshness.clone()
    }

    fn expected_version(&self) -> u32 {
        self.config.effective_program_version()
    }

    fn bootstrap_instruments(&self, dir: &VenueDirectory) {
        let sender = get_data_event_sender();
        let ts_init = self.clock.get_time_ns();
        let mut map = self.instruments.write().expect("instruments lock");
        let mut built = 0usize;
        for meta in dir.instruments.values() {
            let Some((id, instrument)) = instrument_from_meta(meta, dir, ts_init) else {
                continue;
            };
            map.insert(id, instrument.clone());
            if let Err(e) = sender.send(DataEvent::Instrument(instrument)) {
                log::warn!("deriverse: failed to emit instrument: {e}");
            }
            built += 1;
        }
        log::info!("bootstrapped {built} Deriverse instruments");
    }

    /// Heartbeat + staleness watchdog (§3 Invariant 2).
    fn spawn_heartbeat_and_watchdog(&self) {
        let mut slot_rx = self.ws.slot_subscribe();
        let freshness = self.freshness.clone();
        let cancel = self.cancel.clone();
        get_runtime().spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    maybe = slot_rx.recv() => {
                        let Some(slot) = maybe else { break };
                        freshness.note_heartbeat(slot);
                    }
                }
            }
        });
        let freshness = self.freshness.clone();
        let subscriptions = Arc::clone(&self.subscriptions);
        let cancel = self.cancel.clone();
        get_runtime().spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        let hb = freshness.heartbeat_slot();
                        if hb == 0 { continue; }
                        let ids: Vec<InstrumentId> =
                            subscriptions.read().expect("subs lock").keys().copied().collect();
                        for id in ids {
                            let lag = freshness
                                .last_slot(&id)
                                .map_or(u64::MAX, |s| hb.saturating_sub(s));
                            let degraded = lag > freshness.staleness_threshold_slots();
                            if degraded && !freshness.is_degraded(&id) {
                                log::error!(
                                    "deriverse feed DEGRADED for {id}: last_slot lags heartbeat by {lag} slots"
                                );
                            } else if !degraded && freshness.is_degraded(&id) {
                                log::warn!("deriverse feed recovered for {id}");
                            }
                            freshness.set_degraded(id, degraded);
                        }
                    }
                }
            }
        });
    }

    /// REST reconcile (§3 Invariant 3): every reconcile_interval_secs fetch
    /// the snapshot region; REST wins on divergence and the event is counted.
    fn spawn_reconcile(&self) {
        let rpc = Arc::clone(&self.rpc);
        let freshness = self.freshness.clone();
        let subscriptions = Arc::clone(&self.subscriptions);
        let instruments = Arc::clone(&self.instruments);
        let cancel = self.cancel.clone();
        let interval_secs = self.config.reconcile_interval_secs.max(1);
        let expected_version = self.expected_version();
        let sender = get_data_event_sender();
        let clock = self.clock;
        get_runtime().spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        let subs: Vec<(InstrumentId, InstrMeta)> = subscriptions
                            .read().expect("subs lock")
                            .iter().map(|(k, v)| (*k, v.clone())).collect();
                        for (id, meta) in subs {
                            let account = meta.account.to_string();
                            let fetched = rpc
                                .get_account_info(&account, Some((0, INSTR_SNAPSHOT_LEN as u64)))
                                .await;
                            let Ok(Some((slot, bytes))) = fetched else {
                                log::warn!("deriverse reconcile fetch failed for {id}");
                                continue;
                            };
                            let Ok(snap) = decode_instr_account(&bytes, slot, expected_version) else {
                                log::error!("deriverse reconcile decode failed for {id}");
                                continue;
                            };
                            let rest_tob = match (snap.spot_bids.top(), snap.spot_asks.top()) {
                                (Some(b), Some(a)) => (b.px, a.px),
                                _ => continue,
                            };
                            match freshness.last_tob(&id) {
                                Some(ws_tob) if ws_tob == rest_tob => {
                                    freshness.count_reconcile_agreement();
                                }
                                _ => {
                                    // REST wins: count + emit the REST-derived quote.
                                    freshness.count_reconcile_divergence();
                                    log::warn!(
                                        "deriverse reconcile divergence for {id}: REST wins (slot {slot})"
                                    );
                                    let (pp, sp, decs) = precision_of(&instruments, &id, &meta);
                                    if freshness.try_advance(id, slot) {
                                        let ts = clock.get_time_ns();
                                        if let Some(tick) =
                                            snapshot_to_quote(&snap, id, pp, sp, decs, ts)
                                        {
                                            freshness.set_last_tob(id, rest_tob.0, rest_tob.1);
                                            freshness.count_quote();
                                            let _ = sender.send(DataEvent::Data(Data::Quote(tick)));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    fn venue(&self) -> Venue {
        *DERIVERSE_VENUE
    }
}

fn precision_of(
    instruments: &Arc<RwLock<HashMap<InstrumentId, InstrumentAny>>>,
    id: &InstrumentId,
    meta: &InstrMeta,
) -> (u8, u8, u32) {
    let sp = instruments
        .read()
        .expect("instruments lock")
        .get(id)
        .map_or(meta.asset_decs.min(9) as u8, |i| i.size_precision());
    (PRICE_PRECISION, sp, meta.asset_decs)
}

#[async_trait(?Send)]
impl DataClient for DeriverseDataClient {
    fn client_id(&self) -> ClientId {
        self.client_id
    }

    fn venue(&self) -> Option<Venue> {
        Some(self.venue())
    }

    fn start(&mut self) -> anyhow::Result<()> {
        log::info!("Started: client_id={}", self.client_id);
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        log::info!("Stopping {}", self.client_id);
        self.cancel.cancel();
        self.ws.disconnect();
        self.is_connected.store(false, Ordering::Release);
        Ok(())
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        self.cancel.cancel();
        self.cancel = CancellationToken::new();
        self.subscriptions.write().expect("subs lock").clear();
        self.is_connected.store(false, Ordering::Release);
        Ok(())
    }

    fn dispose(&mut self) -> anyhow::Result<()> {
        self.stop()
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        if self.is_connected() {
            return Ok(());
        }
        // Discovery first: instruments must be in the engine cache before
        // any subscribe (RiskEngine precedent from the pacifica adapter).
        let program_id = Pubkey::from_str(&self.config.effective_program_id())?;
        let dir = resolve_directory(self.rpc.as_ref(), &program_id, self.expected_version()).await?;
        self.bootstrap_instruments(&dir);
        *self.directory.write().expect("directory lock") = Some(dir);
        self.ws.connect();
        self.spawn_heartbeat_and_watchdog();
        self.spawn_reconcile();
        self.is_connected.store(true, Ordering::Release);
        log::info!("Connected: client_id={}", self.client_id);
        Ok(())
    }

    async fn disconnect(&mut self) -> anyhow::Result<()> {
        if self.is_disconnected() {
            return Ok(());
        }
        self.cancel.cancel();
        self.ws.disconnect();
        self.cancel = CancellationToken::new();
        self.is_connected.store(false, Ordering::Release);
        log::info!("Disconnected: client_id={}", self.client_id);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.is_connected.load(Ordering::Relaxed)
    }

    fn is_disconnected(&self) -> bool {
        !self.is_connected()
    }

    fn subscribe_quotes(&mut self, cmd: SubscribeQuotes) -> anyhow::Result<()> {
        let id = cmd.instrument_id;
        let meta = {
            let dir = self.directory.read().expect("directory lock");
            let dir = dir.as_ref().ok_or_else(|| {
                anyhow::anyhow!("subscribe_quotes before connect(): no venue directory")
            })?;
            // Find the instrument whose constructed id matches.
            let ts = UnixNanos::default();
            dir.instruments
                .values()
                .find(|m| {
                    instrument_from_meta(m, dir, ts).is_some_and(|(built_id, _)| built_id == id)
                })
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown instrument: {id}"))?
        };
        self.subscriptions.write().expect("subs lock").insert(id, meta.clone());

        let mut rx = self.ws.account_subscribe(meta.account.to_string());
        let sender = get_data_event_sender();
        let freshness = self.freshness.clone();
        let instruments = Arc::clone(&self.instruments);
        let cancel = self.cancel.clone();
        let clock = self.clock;
        let expected_version = self.expected_version();
        get_runtime().spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    maybe = rx.recv() => {
                        let Some(n) = maybe else { break };
                        // Invariant 1: strictly monotone slot, drop stale.
                        if !freshness.try_advance(id, n.slot) {
                            continue;
                        }
                        let snap = match decode_instr_account(&n.bytes, n.slot, expected_version) {
                            Ok(s) => s,
                            Err(e) => {
                                log::error!("deriverse account decode failed for {id}: {e}");
                                continue;
                            }
                        };
                        let (pp, sp, decs) = precision_of(&instruments, &id, &meta);
                        let ts = clock.get_time_ns(); // wall-clock receive time
                        if let Some(tick) = snapshot_to_quote(&snap, id, pp, sp, decs, ts) {
                            if let (Some(b), Some(a)) = (snap.spot_bids.top(), snap.spot_asks.top()) {
                                freshness.set_last_tob(id, b.px, a.px);
                            }
                            freshness.count_quote();
                            if let Err(e) = sender.send(DataEvent::Data(Data::Quote(tick))) {
                                log::error!("deriverse: failed to emit quote: {e}");
                                break;
                            }
                        }
                    }
                }
            }
            log::debug!("deriverse quote drain stopped for {id}");
        });
        log::debug!("Subscribed quotes: {id}");
        Ok(())
    }

    fn request_instruments(&self, request: RequestInstruments) -> anyhow::Result<()> {
        let instruments: Vec<InstrumentAny> = {
            let map = self.instruments.read().expect("instruments lock");
            map.values().cloned().collect()
        };
        let response = DataResponse::Instruments(InstrumentsResponse::new(
            request.request_id,
            request.client_id.unwrap_or(self.client_id),
            self.venue(),
            instruments,
            datetime_to_unix_nanos(request.start),
            datetime_to_unix_nanos(request.end),
            self.clock.get_time_ns(),
            request.params,
        ));
        if let Err(e) = get_data_event_sender().send(DataEvent::Response(response)) {
            log::error!("failed to send instruments response: {e}");
        }
        Ok(())
    }

    fn request_instrument(&self, request: RequestInstrument) -> anyhow::Result<()> {
        let instrument = {
            let map = self.instruments.read().expect("instruments lock");
            map.get(&request.instrument_id).cloned()
        }
        .ok_or_else(|| {
            anyhow::anyhow!("instrument not found: {} (connect() bootstraps)", request.instrument_id)
        })?;
        let response = DataResponse::Instrument(Box::new(InstrumentResponse::new(
            request.request_id,
            request.client_id.unwrap_or(self.client_id),
            instrument.id(),
            instrument,
            datetime_to_unix_nanos(request.start),
            datetime_to_unix_nanos(request.end),
            self.clock.get_time_ns(),
            request.params,
        )));
        if let Err(e) = get_data_event_sender().send(DataEvent::Response(response)) {
            log::error!("failed to send instrument response: {e}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::instr::{Ladder, Level};
    use drv_models::state::instrument::InstrAccountHeader;

    fn snap_with_tob(bid: Option<(i64, i64)>, ask: Option<(i64, i64)>) -> InstrSnapshot {
        let mk = |side: Option<(i64, i64)>| {
            let mut l = Ladder::default();
            if let Some((px, qty)) = side {
                l.levels[0] = Level { px, qty };
                l.len = 1;
            }
            l
        };
        InstrSnapshot {
            slot: 1,
            header: InstrAccountHeader::default(),
            spot_bids: mk(bid),
            spot_asks: mk(ask),
            perp_bids: Ladder::default(),
            perp_asks: Ladder::default(),
        }
    }

    #[test]
    fn snapshot_to_quote_maps_ladder_level_zero() {
        let snap = snap_with_tob(Some((152_250_000_000, 33_000_000)), Some((152_350_000_000, 12_000_000)));
        let id = InstrumentId::from("WSOL-USDC.DERIVERSE");
        let tick = snapshot_to_quote(&snap, id, 9, 9, 9, UnixNanos::from(7u64)).expect("tick");
        assert_eq!(tick.bid_price.as_f64(), 152.25);
        assert_eq!(tick.ask_price.as_f64(), 152.35);
        assert_eq!(tick.bid_size.as_f64(), 0.033);
        assert_eq!(tick.ask_size.as_f64(), 0.012);
        assert_eq!(tick.ts_event, UnixNanos::from(7u64));
    }

    #[test]
    fn one_sided_book_emits_nothing() {
        let snap = snap_with_tob(Some((1_000_000_000, 1)), None);
        let id = InstrumentId::from("WSOL-USDC.DERIVERSE");
        assert!(snapshot_to_quote(&snap, id, 9, 9, 9, UnixNanos::default()).is_none());
    }

    #[test]
    fn freshness_is_strictly_monotone() {
        let f = FeedFreshness::new(50);
        let id = InstrumentId::from("WSOL-USDC.DERIVERSE");
        assert!(f.try_advance(id, 100));
        assert!(!f.try_advance(id, 100), "equal slot must drop");
        assert!(!f.try_advance(id, 99), "older slot must drop");
        assert!(f.try_advance(id, 101));
        assert_eq!(f.drops_stale_count(), 2);
        assert_eq!(f.last_slot(&id), Some(101));
    }

    #[test]
    fn degraded_flag_round_trips_and_heartbeat_maxes() {
        let f = FeedFreshness::new(50);
        let id = InstrumentId::from("WSOL-USDC.DERIVERSE");
        assert!(!f.is_degraded(&id));
        f.set_degraded(id, true);
        assert!(f.is_degraded(&id));
        f.note_heartbeat(10);
        f.note_heartbeat(9); // regression must not lower it
        assert_eq!(f.heartbeat_slot(), 10);
    }
}
