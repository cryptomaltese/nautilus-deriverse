//! Solana WS subscription client (spec §2.1): lean tokio-tungstenite per
//! socket (in-repo Extended/Pacifica pattern), resubscribe-on-connect,
//! `last_text_at` staleness guard. In-band JSON-RPC error replies are an
//! owned frame class: a subscription bounce logs LOUD and triggers a
//! resubscribe — never dropped on the floor (spec §2.1).
//!
//! Truth model (Invariant 3/4): everything on this socket is a latency
//! hint. Account notifications feed the QuoteTick path but are reconciled
//! by REST; `logsSubscribe` never creates an order event by itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::common::enums::Commitment;
use crate::rpc::http::decode_account_data;

/// One `accountNotification` (full current-state snapshot — no delta
/// variant exists on this API).
#[derive(Clone, Debug)]
pub struct AccountNotification {
    pub slot: u64,
    pub bytes: Vec<u8>,
}

/// One `logsNotification`.
#[derive(Clone, Debug)]
pub struct LogsNotification {
    pub slot: u64,
    pub signature: String,
    pub logs: Vec<String>,
}

#[derive(Clone)]
enum SubSink {
    Account(mpsc::UnboundedSender<AccountNotification>),
    Logs(mpsc::UnboundedSender<LogsNotification>),
    Slot(mpsc::UnboundedSender<u64>),
}

#[derive(Clone)]
struct SubSpec {
    kind: SubKind,
    sink: SubSink,
}

#[derive(Clone, Debug, PartialEq)]
enum SubKind {
    Account { pubkey: String },
    Logs { program_id: String },
    Slot,
}

impl SubKind {
    fn request(&self, id: u64, commitment: Commitment) -> Value {
        match self {
            Self::Account { pubkey } => json!({
                "jsonrpc": "2.0", "id": id, "method": "accountSubscribe",
                "params": [pubkey, {"encoding": "base64+zstd", "commitment": commitment.as_str()}],
            }),
            Self::Logs { program_id } => json!({
                "jsonrpc": "2.0", "id": id, "method": "logsSubscribe",
                "params": [{"mentions": [program_id]}, {"commitment": commitment.as_str()}],
            }),
            Self::Slot => json!({
                "jsonrpc": "2.0", "id": id, "method": "slotSubscribe", "params": [],
            }),
        }
    }
}

/// Shared registry: specs survive reconnects; (pending request id |
/// live server subscription id) → registry index.
#[derive(Default)]
struct Registry {
    specs: Vec<SubSpec>,
    pending: std::collections::HashMap<u64, usize>, // request id → spec idx
    live: std::collections::HashMap<u64, usize>,    // server sub id → spec idx
}

pub struct DeriverseRpcSubscriptionClient {
    ws_url: String,
    commitment: Commitment,
    registry: Arc<Mutex<Registry>>,
    /// Wakes the actor to (re)send subscribe frames for new specs.
    resubscribe: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
    started: AtomicBool,
}

impl DeriverseRpcSubscriptionClient {
    #[must_use]
    pub fn new(ws_url: String, commitment: Commitment) -> Self {
        Self {
            ws_url,
            commitment,
            registry: Arc::new(Mutex::new(Registry::default())),
            resubscribe: Arc::new(tokio::sync::Notify::new()),
            cancel: CancellationToken::new(),
            started: AtomicBool::new(false),
        }
    }

    fn register(&self, kind: SubKind, sink: SubSink) {
        self.registry
            .lock()
            .expect("ws registry lock")
            .specs
            .push(SubSpec { kind, sink });
        self.resubscribe.notify_one();
    }

    #[must_use]
    pub fn account_subscribe(&self, pubkey: String) -> mpsc::UnboundedReceiver<AccountNotification> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.register(SubKind::Account { pubkey }, SubSink::Account(tx));
        rx
    }

    #[must_use]
    pub fn logs_subscribe(&self, program_id: String) -> mpsc::UnboundedReceiver<LogsNotification> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.register(SubKind::Logs { program_id }, SubSink::Logs(tx));
        rx
    }

    #[must_use]
    pub fn slot_subscribe(&self) -> mpsc::UnboundedReceiver<u64> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.register(SubKind::Slot, SubSink::Slot(tx));
        rx
    }

    /// Spawn the connect/reconnect actor onto the ambient tokio runtime.
    pub fn connect(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let ws_url = self.ws_url.clone();
        let commitment = self.commitment;
        let registry = Arc::clone(&self.registry);
        let resubscribe = Arc::clone(&self.resubscribe);
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let mut backoff_ms = 250u64;
            'reconnect: loop {
                if cancel.is_cancelled() {
                    break;
                }
                let (mut socket, _resp) = match tokio_tungstenite::connect_async(&ws_url).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        log::error!("deriverse ws connect failed: {e}; retry in {backoff_ms}ms");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(10_000);
                        continue;
                    }
                };
                backoff_ms = 250;
                let mut next_id: u64 = 1;
                {
                    // Fresh socket ⇒ every live mapping is void; resend all.
                    let mut reg = registry.lock().expect("ws registry lock");
                    reg.pending.clear();
                    reg.live.clear();
                }
                if resend_all(&mut socket, &registry, commitment, &mut next_id).await.is_err() {
                    continue 'reconnect;
                }
                let mut last_text_at = tokio::time::Instant::now();
                let mut staleness = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => { let _ = socket.close(None).await; break 'reconnect; }
                        () = resubscribe.notified() => {
                            if resend_unsubscribed(&mut socket, &registry, commitment, &mut next_id).await.is_err() {
                                continue 'reconnect;
                            }
                        }
                        _ = staleness.tick() => {
                            // slotSubscribe ticks ~2.5/s; >30s of silence on a
                            // socket carrying any subscription = dead feed.
                            let has_subs = !registry.lock().expect("ws registry lock").specs.is_empty();
                            if has_subs && last_text_at.elapsed() > Duration::from_secs(30) {
                                log::error!("deriverse ws stale (>30s no frames) — reconnecting");
                                let _ = socket.close(None).await;
                                continue 'reconnect;
                            }
                        }
                        frame = socket.next() => {
                            match frame {
                                Some(Ok(Message::Text(text))) => {
                                    last_text_at = tokio::time::Instant::now();
                                    handle_text(&text, &registry);
                                }
                                Some(Ok(Message::Ping(p))) => { let _ = socket.send(Message::Pong(p)).await; }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => { log::error!("deriverse ws error: {e}"); continue 'reconnect; }
                                None => { log::warn!("deriverse ws closed by peer"); continue 'reconnect; }
                            }
                        }
                    }
                }
            }
            log::debug!("deriverse ws actor stopped");
        });
    }

    pub fn disconnect(&self) {
        self.cancel.cancel();
    }
}

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn send_subscribe(
    socket: &mut Socket,
    registry: &Arc<Mutex<Registry>>,
    commitment: Commitment,
    next_id: &mut u64,
    idx: usize,
) -> anyhow::Result<()> {
    let req = {
        let mut reg = registry.lock().expect("ws registry lock");
        let id = *next_id;
        *next_id += 1;
        reg.pending.insert(id, idx);
        reg.specs[idx].kind.request(id, commitment)
    };
    socket
        .send(Message::Text(req.to_string().into()))
        .await
        .map_err(|e| anyhow::anyhow!("ws send: {e}"))
}

async fn resend_all(
    socket: &mut Socket,
    registry: &Arc<Mutex<Registry>>,
    commitment: Commitment,
    next_id: &mut u64,
) -> anyhow::Result<()> {
    let n = registry.lock().expect("ws registry lock").specs.len();
    for idx in 0..n {
        send_subscribe(socket, registry, commitment, next_id, idx).await?;
    }
    Ok(())
}

async fn resend_unsubscribed(
    socket: &mut Socket,
    registry: &Arc<Mutex<Registry>>,
    commitment: Commitment,
    next_id: &mut u64,
) -> anyhow::Result<()> {
    let todo: Vec<usize> = {
        let reg = registry.lock().expect("ws registry lock");
        (0..reg.specs.len())
            .filter(|idx| {
                !reg.live.values().any(|i| i == idx) && !reg.pending.values().any(|i| i == idx)
            })
            .collect()
    };
    for idx in todo {
        send_subscribe(socket, registry, commitment, next_id, idx).await?;
    }
    Ok(())
}

/// Pure frame router (unit-tested): subscribe confirmations, in-band error
/// replies (LOUD + rearm), and the three notification kinds.
fn handle_text(text: &str, registry: &Arc<Mutex<Registry>>) {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        log::warn!("deriverse ws: non-JSON frame");
        return;
    };
    // Reply to a subscribe request?
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let mut reg = registry.lock().expect("ws registry lock");
        let Some(idx) = reg.pending.remove(&id) else { return };
        if let Some(err) = v.get("error") {
            // Owned frame class: subscription bounce ⇒ loud error; the spec
            // survives in the registry, so the next resubscribe nudge or
            // reconnect retries it.
            log::error!(
                "deriverse ws subscription bounced (spec {:?}): {err}",
                reg.specs[idx].kind
            );
            return;
        }
        if let Some(sub_id) = v.get("result").and_then(Value::as_u64) {
            reg.live.insert(sub_id, idx);
        }
        return;
    }
    // Notification?
    let method = v.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = &v["params"];
    let Some(sub_id) = params.get("subscription").and_then(Value::as_u64) else { return };
    let sink = {
        let reg = registry.lock().expect("ws registry lock");
        reg.live.get(&sub_id).map(|idx| reg.specs[*idx].sink.clone())
    };
    let Some(sink) = sink else {
        log::warn!("deriverse ws: notification for unknown subscription {sub_id}");
        return;
    };
    match (method, sink) {
        ("accountNotification", SubSink::Account(tx)) => {
            let result = &params["result"];
            let slot = result["context"]["slot"].as_u64().unwrap_or_default();
            match decode_account_data(&result["value"]["data"]) {
                Ok(bytes) => {
                    let _ = tx.send(AccountNotification { slot, bytes });
                }
                Err(e) => log::error!("deriverse ws account data decode: {e}"),
            }
        }
        ("logsNotification", SubSink::Logs(tx)) => {
            let result = &params["result"];
            let slot = result["context"]["slot"].as_u64().unwrap_or_default();
            let value = &result["value"];
            let _ = tx.send(LogsNotification {
                slot,
                signature: value["signature"].as_str().unwrap_or_default().to_string(),
                logs: value["logs"]
                    .as_array()
                    .map(|a| {
                        a.iter().filter_map(Value::as_str).map(ToString::to_string).collect()
                    })
                    .unwrap_or_default(),
            });
        }
        ("slotNotification", SubSink::Slot(tx)) => {
            if let Some(slot) = params["result"]["slot"].as_u64() {
                let _ = tx.send(slot);
            }
        }
        (m, _) => log::warn!("deriverse ws: sink/method mismatch for {m}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn client_with_account_sub() -> (
        DeriverseRpcSubscriptionClient,
        mpsc::UnboundedReceiver<AccountNotification>,
    ) {
        let client = DeriverseRpcSubscriptionClient::new(
            "wss://unused.invalid".to_string(),
            Commitment::Confirmed,
        );
        let rx = client.account_subscribe("SomePubkey".to_string());
        (client, rx)
    }

    #[test]
    fn subscribe_confirmation_then_notification_routes() {
        let (client, mut rx) = client_with_account_sub();
        // Simulate the actor's request-id bookkeeping.
        client.registry.lock().unwrap().pending.insert(1, 0);
        handle_text(r#"{"jsonrpc":"2.0","id":1,"result":5551}"#, &client.registry);
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"payload");
        let frame = format!(
            r#"{{"jsonrpc":"2.0","method":"accountNotification","params":{{"subscription":5551,"result":{{"context":{{"slot":361000123}},"value":{{"data":["{b64}","base64"]}}}}}}}}"#
        );
        handle_text(&frame, &client.registry);
        let n = rx.try_recv().expect("notification routed");
        assert_eq!(n.slot, 361_000_123);
        assert_eq!(n.bytes, b"payload");
    }

    #[test]
    fn error_reply_is_loud_but_keeps_spec_registered() {
        let (client, mut rx) = client_with_account_sub();
        client.registry.lock().unwrap().pending.insert(1, 0);
        handle_text(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid params"}}"#,
            &client.registry,
        );
        let reg = client.registry.lock().unwrap();
        assert_eq!(reg.specs.len(), 1, "spec survives for resubscribe");
        assert!(reg.live.is_empty());
        drop(reg);
        assert!(rx.try_recv().is_err(), "no notification fabricated");
    }

    #[test]
    fn slot_notifications_route() {
        let client = DeriverseRpcSubscriptionClient::new(
            "wss://unused.invalid".to_string(),
            Commitment::Confirmed,
        );
        let mut rx = client.slot_subscribe();
        client.registry.lock().unwrap().pending.insert(1, 0);
        handle_text(r#"{"id":1,"result":9}"#, &client.registry);
        handle_text(
            r#"{"method":"slotNotification","params":{"subscription":9,"result":{"parent":1,"root":1,"slot":361000200}}}"#,
            &client.registry,
        );
        assert_eq!(rx.try_recv().unwrap(), 361_000_200);
    }

    #[test]
    fn unknown_subscription_is_dropped_quietly() {
        let (client, mut rx) = client_with_account_sub();
        handle_text(
            r#"{"method":"accountNotification","params":{"subscription":404,"result":{"context":{"slot":1},"value":{"data":["","base64"]}}}}"#,
            &client.registry,
        );
        assert!(rx.try_recv().is_err());
    }
}
