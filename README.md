# nautilus-deriverse

Deriverse venue adapter for [Nautilus Trader](https://nautilustrader.io) (Rust).

[Deriverse](https://deriverse.io) is a fully on-chain CLOB on Solana — there is
no exchange gateway. Market data is Solana RPC `accountSubscribe` on the
instrument account, every order/cancel is a signed v0 transaction, and fills
are decoded from the client's own transaction logs, confirmed by signature
polling.

> **Generated mirror.** This repository is a 100%-generated export of the
> adapter's source of truth; every commit here is produced by an automated
> sync. Issues and pull requests are welcome — they are imported upstream
> rather than merged here. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Status

| Surface | Milestone | Status |
|---|---|---|
| Transport substrate (`rpc/`, `decode/`, `tx/`) | M1a | **Live-proven on mainnet** — golden-gated signing, live soak, live order round-trip |
| Spot data client (`data.rs`, `factories.rs`) | M1a | **Live-proven on mainnet** |
| Perp execution surface | M1b | Fixture-proven; pending venue perp launch |
| Python layer | M1b | Not started |

Byte layouts (accounts, logs, instructions) come from the venue's
[`drv-models`](https://github.com/deriverse/drv-smart-contract-common) crate,
pinned by release tag — never re-declared here. Transaction encoding and
signing are gated by golden fixtures generated once from the venue's
TypeScript kit and checked in under `tests/fixtures/`.

## Quickstart

```rust
use nautilus_deriverse::config::{DeriverseDataClientConfig, DeriverseExecClientConfig};

let data_cfg = DeriverseDataClientConfig::default(); // mainnet defaults
let exec_cfg = DeriverseExecClientConfig {
    keypair_path: Some("path/to/solana-keypair.json".into()),
    ..Default::default()
};
```

Key config fields (see `src/config.rs` for the full set):

| Field | Default | Notes |
|---|---|---|
| `rpc_url` | `https://rpc-mainnet.deriverse.io` | Solana RPC HTTP endpoint |
| `ws_url` | `wss://rpc-mainnet.deriverse.io` | Solana RPC WS endpoint (`accountSubscribe`) |
| `program_id` / `program_version` | mainnet program | override **both** to select devnet |
| `commitment` | `Confirmed` | |
| `keypair_path` (exec) | — | path to a Solana keypair JSON; the key material never appears in logs or errors |
| `market_slippage_bps` (exec) | 50 | MARKET orders are priced as bounded crossing limits |
| `leverage` (exec) | `None` | 1–100; `None` = venue maximum |

Operator binaries (all read-only except `drv-setup`/`drv-spot-order`):

- `drv-probe` — RPC sanity, instrument snapshot decode, data-client soak
- `drv-capture` — fetch an instrument account snapshot into `tests/fixtures/`
- `drv-setup` — one-time client-account init + spot USDC deposit
- `drv-spot-order` — attended spot order place/cancel rungs

## Venue notes

- **Price band.** Limit orders must price within roughly ±9.375% of the book;
  orders outside the band reject with `InvalidPrice` (custom error 131).
- **Oversell.** Selling more than the available balance rejects with
  `InsufficientFunds` (custom error 132) — the venue never clamps order size.

## Build & test

```sh
cargo build
cargo test
```

MSRV 1.96.0. The test suite is hermetic: the two live network probes are
`#[ignore]`d and run only on request
(`cargo test --test live_probe -- --ignored --nocapture`).

## License

[MIT](LICENSE)
