//! JSON-RPC over HTTP via the vendor `nautilus_network::http::HttpClient`
//! (pacifica precedent). Every call POSTs a `{"jsonrpc":"2.0",...}` envelope
//! to the configured `rpc_url`. A JSON-RPC `error` member on
//! `sendTransaction` maps to `SendError::Definitive` with the RPC error
//! string verbatim (spec §1.6); every other transport/HTTP failure maps to
//! `SendError::Transport` (send) or `anyhow` errors (reads).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use nautilus_network::http::{HttpClient, Method};
use serde_json::{json, Value};

use crate::common::enums::Commitment;
use crate::rpc::{SendError, SignatureStatus, SolanaRpc};

/// Decode an RPC account `data` field: `["<b64>", "base64"]` or
/// `["<b64>", "base64+zstd"]`.
pub fn decode_account_data(data: &Value) -> anyhow::Result<Vec<u8>> {
    let arr = data
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("account data is not an array: {data}"))?;
    let (b64, enc) = (
        arr.first().and_then(Value::as_str).unwrap_or_default(),
        arr.get(1).and_then(Value::as_str).unwrap_or("base64"),
    );
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| anyhow::anyhow!("account data base64: {e}"))?;
    match enc {
        "base64" => Ok(raw),
        "base64+zstd" => {
            zstd::stream::decode_all(&raw[..]).map_err(|e| anyhow::anyhow!("zstd: {e}"))
        }
        other => anyhow::bail!("unsupported account encoding: {other}"),
    }
}

#[derive(Debug)]
pub struct DeriverseRpcClient {
    rpc_url: String,
    program_id: String,
    commitment: Commitment,
    client: HttpClient,
    next_id: AtomicU64,
}

impl DeriverseRpcClient {
    pub fn new(
        rpc_url: String,
        program_id: String,
        commitment: Commitment,
        timeout_secs: u64,
    ) -> anyhow::Result<Self> {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        let client = HttpClient::new(headers, vec![], vec![], None, Some(timeout_secs), None)
            .map_err(|e| anyhow::anyhow!("failed to create HTTP client: {e}"))?;
        Ok(Self {
            rpc_url,
            program_id,
            commitment,
            client,
            next_id: AtomicU64::new(1),
        })
    }

    /// POST one JSON-RPC call; returns the `result` member. A JSON-RPC
    /// `error` member becomes `Err` carrying the error text VERBATIM.
    async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let resp = self
            .client
            .request(
                Method::POST,
                self.rpc_url.clone(),
                None,
                None,
                Some(serde_json::to_vec(&body)?),
                None,
                None,
            )
            .await
            .map_err(|e| anyhow::anyhow!("rpc {method}: {e}"))?;
        let status = resp.status.as_u16();
        anyhow::ensure!(
            status < 400,
            "rpc {method} HTTP {status}: {}",
            String::from_utf8_lossy(&resp.body)
        );
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| anyhow::anyhow!("rpc {method} decode: {e}"))?;
        if let Some(err) = v.get("error") {
            anyhow::bail!("{}", rpc_error_text(err));
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("rpc {method}: no result member"))
    }
}

/// The RPC error string, verbatim where possible (spec §1.6: reject reasons
/// surface the RPC error string verbatim).
fn rpc_error_text(err: &Value) -> String {
    err.get("message")
        .and_then(Value::as_str)
        .map_or_else(|| err.to_string(), ToString::to_string)
}

fn data_slice_json(ds: Option<(u64, u64)>) -> Value {
    ds.map_or(
        Value::Null,
        |(offset, length)| json!({"offset": offset, "length": length}),
    )
}

/// `getSignatureStatuses` params. `search_transaction_history = false` is
/// the recent-status-cache read (hot poll); `true` is the ledger-backed
/// read the expiry proof-of-absence requires (the cache evicts ~150 blocks
/// past the blockhash's last valid height — a cache-backed `None` is never
/// proof).
fn signature_status_params(signature: &str, search_transaction_history: bool) -> Value {
    json!([[signature], {"searchTransactionHistory": search_transaction_history}])
}

/// One row of a `getSignatureStatuses` response `value` array.
fn parse_signature_status(result: &Value) -> Option<SignatureStatus> {
    match &result["value"][0] {
        Value::Null => None,
        s => Some(SignatureStatus {
            slot: s["slot"].as_u64().unwrap_or_default(),
            confirmation_status: s["confirmationStatus"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            err: (!s["err"].is_null()).then(|| s["err"].clone()),
        }),
    }
}

#[async_trait::async_trait]
impl SolanaRpc for DeriverseRpcClient {
    async fn get_account_info(
        &self,
        pubkey: &str,
        data_slice: Option<(u64, u64)>,
    ) -> anyhow::Result<Option<(u64, Vec<u8>)>> {
        let mut cfg = json!({"encoding": "base64+zstd", "commitment": self.commitment.as_str()});
        if data_slice.is_some() {
            cfg["dataSlice"] = data_slice_json(data_slice);
        }
        let result = self.call("getAccountInfo", json!([pubkey, cfg])).await?;
        let slot = result["context"]["slot"].as_u64().unwrap_or_default();
        match &result["value"] {
            Value::Null => Ok(None),
            value => Ok(Some((slot, decode_account_data(&value["data"])?))),
        }
    }

    async fn get_program_accounts(
        &self,
        memcmp_offset0_base58: &str,
        data_slice: Option<(u64, u64)>,
    ) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        let mut cfg = json!({
            "encoding": "base64",
            "commitment": self.commitment.as_str(),
            "filters": [{"memcmp": {"offset": 0, "bytes": memcmp_offset0_base58, "encoding": "base58"}}],
        });
        if data_slice.is_some() {
            cfg["dataSlice"] = data_slice_json(data_slice);
        }
        let result = self
            .call("getProgramAccounts", json!([self.program_id, cfg]))
            .await?;
        let rows = result
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("getProgramAccounts: non-array result"))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let pubkey = row["pubkey"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("getProgramAccounts: missing pubkey"))?
                .to_string();
            out.push((pubkey, decode_account_data(&row["account"]["data"])?));
        }
        Ok(out)
    }

    async fn get_latest_blockhash(&self) -> anyhow::Result<(String, u64)> {
        let result = self
            .call(
                "getLatestBlockhash",
                json!([{"commitment": self.commitment.as_str()}]),
            )
            .await?;
        let v = &result["value"];
        Ok((
            v["blockhash"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("no blockhash"))?
                .to_string(),
            v["lastValidBlockHeight"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("no lastValidBlockHeight"))?,
        ))
    }

    async fn get_block_height(&self) -> anyhow::Result<u64> {
        self.call(
            "getBlockHeight",
            json!([{"commitment": self.commitment.as_str()}]),
        )
        .await?
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("getBlockHeight: non-numeric"))
    }

    async fn get_epoch_info(&self) -> anyhow::Result<(u64, u64)> {
        let result = self
            .call(
                "getEpochInfo",
                json!([{"commitment": self.commitment.as_str()}]),
            )
            .await?;
        let absolute_slot = result["absoluteSlot"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("getEpochInfo: no absoluteSlot"))?;
        let block_height = result["blockHeight"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("getEpochInfo: no blockHeight"))?;
        Ok((absolute_slot, block_height))
    }

    async fn get_slot(&self) -> anyhow::Result<u64> {
        self.call("getSlot", json!([{"commitment": self.commitment.as_str()}]))
            .await?
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("getSlot: non-numeric"))
    }

    async fn get_balance(&self, pubkey: &str) -> anyhow::Result<u64> {
        let result = self
            .call(
                "getBalance",
                json!([pubkey, {"commitment": self.commitment.as_str()}]),
            )
            .await?;
        result["value"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("getBalance: non-numeric"))
    }

    async fn get_signature_status(
        &self,
        signature: &str,
    ) -> anyhow::Result<Option<SignatureStatus>> {
        let result = self
            .call(
                "getSignatureStatuses",
                signature_status_params(signature, false),
            )
            .await?;
        Ok(parse_signature_status(&result))
    }

    async fn get_signature_status_searched(
        &self,
        signature: &str,
    ) -> anyhow::Result<(u64, Option<SignatureStatus>)> {
        let result = self
            .call(
                "getSignatureStatuses",
                signature_status_params(signature, true),
            )
            .await?;
        // Answering node's slot at response time; a missing context reads 0,
        // which the expiry gate treats as "node position unknown ⇒ no proof".
        let context_slot = result["context"]["slot"].as_u64().unwrap_or_default();
        Ok((context_slot, parse_signature_status(&result)))
    }

    async fn send_transaction(&self, tx_base64: &str) -> Result<String, SendError> {
        let params = json!([tx_base64, {
            "encoding": "base64",
            "skipPreflight": false,
            "preflightCommitment": self.commitment.as_str(),
        }]);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body =
            json!({"jsonrpc": "2.0", "id": id, "method": "sendTransaction", "params": params});
        let resp = self
            .client
            .request(
                Method::POST,
                self.rpc_url.clone(),
                None,
                None,
                Some(serde_json::to_vec(&body).map_err(|e| SendError::Transport(e.to_string()))?),
                None,
                None,
            )
            .await
            .map_err(|e| SendError::Transport(e.to_string()))?;
        let status = resp.status.as_u16();
        if status >= 500 {
            // Gateway/edge failure — the tx may still have reached the leader.
            return Err(SendError::Transport(format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&resp.body)
            )));
        }
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| SendError::Transport(format!("decode: {e}")))?;
        if let Some(err) = v.get("error") {
            // A structured JSON-RPC error IS the node's definitive answer
            // (preflight/program error under skip_preflight = false). The
            // simulation logs (error.data.logs) carry the program's own
            // formatted message (e.g. the InvalidPrice acceptable range) —
            // append them so the reject reason stays actionable verbatim.
            let mut reason = rpc_error_text(err);
            if let Some(logs) = err["data"]["logs"].as_array() {
                for line in logs.iter().filter_map(Value::as_str) {
                    reason.push_str("\n  ");
                    reason.push_str(line);
                }
            }
            return Err(SendError::Definitive(reason));
        }
        v.get("result")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| SendError::Transport("sendTransaction: no result signature".into()))
    }

    async fn get_transaction(&self, signature: &str) -> anyhow::Result<Option<(u64, Vec<String>)>> {
        let result = self
            .call(
                "getTransaction",
                json!([signature, {
                    "encoding": "json",
                    "commitment": self.commitment.as_str(),
                    "maxSupportedTransactionVersion": 0,
                }]),
            )
            .await?;
        match &result {
            Value::Null => Ok(None),
            tx => {
                let slot = tx["slot"].as_u64().unwrap_or_default();
                let logs = tx["meta"]["logMessages"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(ToString::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Some((slot, logs)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_plain_base64_account_data() {
        let payload = b"hello-solana".to_vec();
        let v = json!([
            base64::engine::general_purpose::STANDARD.encode(&payload),
            "base64"
        ]);
        assert_eq!(decode_account_data(&v).unwrap(), payload);
    }

    #[test]
    fn decodes_zstd_account_data() {
        let payload = vec![7u8; 4096];
        let compressed = zstd::stream::encode_all(&payload[..], 0).unwrap();
        let v = json!([
            base64::engine::general_purpose::STANDARD.encode(&compressed),
            "base64+zstd"
        ]);
        assert_eq!(decode_account_data(&v).unwrap(), payload);
    }

    #[test]
    fn rejects_unknown_encoding_and_bad_base64() {
        assert!(decode_account_data(&json!(["aGk=", "base58"])).is_err());
        assert!(decode_account_data(&json!(["%%%", "base64"])).is_err());
        assert!(decode_account_data(&json!("not-an-array")).is_err());
    }

    #[test]
    fn rpc_error_text_is_verbatim() {
        let err = json!({"code": -32002, "message": "Transaction simulation failed: Blockhash not found"});
        assert_eq!(
            rpc_error_text(&err),
            "Transaction simulation failed: Blockhash not found"
        );
    }

    /// M1b review (critical): the expiry proving re-check MUST be the
    /// ledger-backed read — searchTransactionHistory: true — while the hot
    /// poll stays on the cheap cache read.
    #[test]
    fn proving_read_sends_search_transaction_history_true() {
        let searched = signature_status_params("sigA", true);
        assert_eq!(searched[1]["searchTransactionHistory"], json!(true));
        assert_eq!(searched[0], json!(["sigA"]));
        let cached = signature_status_params("sigA", false);
        assert_eq!(cached[1]["searchTransactionHistory"], json!(false));
    }

    /// Minimal loopback HTTP/1.1 responder (hermetic — no external hosts):
    /// serves one canned JSON-RPC body per connection, capturing each
    /// request body for wire assertions.
    async fn spawn_capture_server(
        responses: Vec<String>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&bodies);
        tokio::spawn(async move {
            for resp_body in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let body = loop {
                    let Ok(n) = sock.read(&mut tmp).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                        let content_length: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let mut b = buf[pos + 4..].to_vec();
                        while b.len() < content_length {
                            let Ok(n) = sock.read(&mut tmp).await else {
                                return;
                            };
                            if n == 0 {
                                break;
                            }
                            b.extend_from_slice(&tmp[..n]);
                        }
                        break String::from_utf8_lossy(&b[..content_length]).to_string();
                    }
                };
                captured.lock().unwrap().push(body);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), bodies)
    }

    /// M1b review round 2 (nit): pin the WIRING, not just the params
    /// helper — the searched trait method must put
    /// `searchTransactionHistory: true` on the wire and the hot-poll method
    /// `false`; swapping the call-site booleans must fail this test.
    #[tokio::test]
    async fn trait_methods_send_search_transaction_history_per_contract() {
        let resp =
            r#"{"jsonrpc":"2.0","id":1,"result":{"context":{"slot":361000500},"value":[null]}}"#;
        let (url, bodies) = spawn_capture_server(vec![resp.to_string(), resp.to_string()]).await;
        let client = DeriverseRpcClient::new(
            url,
            crate::common::consts::PROGRAM_ID.to_string(),
            Commitment::Confirmed,
            5,
        )
        .unwrap();
        assert!(client.get_signature_status("sigW").await.unwrap().is_none());
        let (slot, searched) = client.get_signature_status_searched("sigW").await.unwrap();
        assert_eq!(slot, 361_000_500, "context.slot captured off the wire");
        assert!(searched.is_none());
        let b = bodies.lock().unwrap();
        assert_eq!(b.len(), 2);
        let hot: Value = serde_json::from_str(&b[0]).unwrap();
        assert_eq!(hot["method"], "getSignatureStatuses");
        assert_eq!(hot["params"][1]["searchTransactionHistory"], json!(false));
        let proving: Value = serde_json::from_str(&b[1]).unwrap();
        assert_eq!(proving["method"], "getSignatureStatuses");
        assert_eq!(
            proving["params"][1]["searchTransactionHistory"],
            json!(true)
        );
    }

    /// M1b review round 3 (nit): pin getEpochInfo's tuple ORDER off the
    /// wire — a silent absoluteSlot↔blockHeight swap would make both expiry
    /// gates vacuous at once (heights carrying slot-magnitude values and
    /// vice versa) with the whole suite green.
    #[tokio::test]
    async fn get_epoch_info_tuple_order_is_slot_then_height() {
        let resp = r#"{"jsonrpc":"2.0","id":1,"result":{"absoluteSlot":360000123,"blockHeight":340000456,"epoch":812,"slotIndex":5,"slotsInEpoch":432000,"transactionCount":0}}"#;
        let (url, bodies) = spawn_capture_server(vec![resp.to_string()]).await;
        let client = DeriverseRpcClient::new(
            url,
            crate::common::consts::PROGRAM_ID.to_string(),
            Commitment::Confirmed,
            5,
        )
        .unwrap();
        let (absolute_slot, block_height) = client.get_epoch_info().await.unwrap();
        assert_eq!(absolute_slot, 360_000_123, "first element = absoluteSlot");
        assert_eq!(block_height, 340_000_456, "second element = blockHeight");
        let b = bodies.lock().unwrap();
        let req: Value = serde_json::from_str(&b[0]).unwrap();
        assert_eq!(req["method"], "getEpochInfo");
    }

    #[test]
    fn parse_signature_status_reads_row_and_null() {
        let result = json!({
            "context": {"slot": 361000500},
            "value": [{"slot": 361000123, "confirmationStatus": "confirmed", "err": null}],
        });
        let s = parse_signature_status(&result).expect("status row");
        assert_eq!(s.slot, 361_000_123);
        assert_eq!(s.confirmation_status, "confirmed");
        assert!(s.err.is_none());
        let none = json!({"context": {"slot": 361000500}, "value": [null]});
        assert!(parse_signature_status(&none).is_none());
    }
}
