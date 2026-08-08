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
        Ok(Self { rpc_url, program_id, commitment, client, next_id: AtomicU64::new(1) })
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
        anyhow::ensure!(status < 400, "rpc {method} HTTP {status}: {}", String::from_utf8_lossy(&resp.body));
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
    ds.map_or(Value::Null, |(offset, length)| json!({"offset": offset, "length": length}))
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
            .call("getLatestBlockhash", json!([{"commitment": self.commitment.as_str()}]))
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
        self.call("getBlockHeight", json!([{"commitment": self.commitment.as_str()}]))
            .await?
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("getBlockHeight: non-numeric"))
    }

    async fn get_slot(&self) -> anyhow::Result<u64> {
        self.call("getSlot", json!([{"commitment": self.commitment.as_str()}]))
            .await?
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("getSlot: non-numeric"))
    }

    async fn get_balance(&self, pubkey: &str) -> anyhow::Result<u64> {
        let result = self
            .call("getBalance", json!([pubkey, {"commitment": self.commitment.as_str()}]))
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
                json!([[signature], {"searchTransactionHistory": false}]),
            )
            .await?;
        match &result["value"][0] {
            Value::Null => Ok(None),
            s => Ok(Some(SignatureStatus {
                slot: s["slot"].as_u64().unwrap_or_default(),
                confirmation_status: s["confirmationStatus"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                err: (!s["err"].is_null()).then(|| s["err"].clone()),
            })),
        }
    }

    async fn send_transaction(&self, tx_base64: &str) -> Result<String, SendError> {
        let params = json!([tx_base64, {
            "encoding": "base64",
            "skipPreflight": false,
            "preflightCommitment": self.commitment.as_str(),
        }]);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": "sendTransaction", "params": params});
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
            // (preflight/program error under skip_preflight = false).
            return Err(SendError::Definitive(rpc_error_text(err)));
        }
        v.get("result")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| SendError::Transport("sendTransaction: no result signature".into()))
    }

    async fn get_transaction(
        &self,
        signature: &str,
    ) -> anyhow::Result<Option<(u64, Vec<String>)>> {
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
    use base64::Engine as _;

    #[test]
    fn decodes_plain_base64_account_data() {
        let payload = b"hello-solana".to_vec();
        let v = json!([base64::engine::general_purpose::STANDARD.encode(&payload), "base64"]);
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
}
