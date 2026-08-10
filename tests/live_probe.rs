//! Live PUBLIC-read probes — `#[ignore]`d so the suite stays hermetic.
//! Run manually: `cargo +1.96.0 test -p nautilus-deriverse --test live_probe -- --ignored --nocapture`
//!
//! TLS gate (spec §2.1): prove the WORKSPACE's own rustls stack
//! (tokio-tungstenite 0.28, rustls-tls-webpki-roots) handshakes against
//! both endpoints BEFORE the crate's dep set is considered locked. Zero
//! credentials, zero writes.

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use nautilus_deriverse::common::consts::{
    DEFAULT_RPC_HTTP_URL, DEFAULT_RPC_WS_URL, FALLBACK_RPC_HTTP_URL, PROGRAM_ID,
};
use nautilus_deriverse::common::enums::Commitment;
use nautilus_deriverse::rpc::{DeriverseRpcClient, SolanaRpc};

#[ignore = "network probe (public reads only) — run manually"]
#[tokio::test]
async fn http_get_version_and_slot_on_both_endpoints() {
    for url in [DEFAULT_RPC_HTTP_URL, FALLBACK_RPC_HTTP_URL] {
        let rpc = DeriverseRpcClient::new(
            url.to_string(),
            PROGRAM_ID.to_string(),
            Commitment::Confirmed,
            15,
        )
        .expect("client");
        let slot = rpc.get_slot().await.expect("getSlot");
        assert!(slot > 300_000_000, "{url}: implausible slot {slot}");
        println!("{url} OK — slot {slot}");
    }
}

#[ignore = "network probe (public reads only) — run manually"]
#[tokio::test]
async fn ws_handshake_and_slot_subscribe_via_workspace_tls() {
    let (mut socket, _resp) = tokio_tungstenite::connect_async(DEFAULT_RPC_WS_URL)
        .await
        .expect("WS handshake via the workspace rustls stack");
    socket
        .send(Message::Text(
            r#"{"jsonrpc":"2.0","id":1,"method":"slotSubscribe","params":[]}"#
                .to_string()
                .into(),
        ))
        .await
        .expect("subscribe send");
    let mut slots = 0u32;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
    while slots < 3 && tokio::time::Instant::now() < deadline {
        let Ok(Some(Ok(Message::Text(text)))) =
            tokio::time::timeout_at(deadline, socket.next()).await
        else {
            continue;
        };
        if text.contains("slotNotification") {
            slots += 1;
        }
    }
    assert!(
        slots >= 3,
        "expected >=3 slot notifications within 15s, got {slots}"
    );
    println!("WS OK — {slots} slot notifications");
}
