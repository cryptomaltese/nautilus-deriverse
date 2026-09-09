# Deriverse Adapter for Nautilus

This is a venue adapter that lets [NautilusTrader](https://nautilustrader.io)
trade on [Deriverse](https://deriverse.io).

If you already run a Nautilus trading system and you want it to see Deriverse
market data, this crate is the piece that connects the two. You add it to your
node, point it at a Solana RPC endpoint, and your strategies start receiving
Deriverse quotes like they would from any other venue.

## Nautilus

**[NautilusTrader](https://nautilustrader.io)** is an open-source algorithmic
trading platform — you write a strategy once, backtest it on historical data,
and run the identical code live. It is written in Rust with a Python API.

New to it? Start with the
[documentation home](https://nautilustrader.io/docs/latest/) and the
[getting-started guide](https://nautilustrader.io/docs/latest/getting_started/).
The [concepts section](https://nautilustrader.io/docs/latest/concepts/) explains
the moving parts, and
[architecture](https://nautilustrader.io/docs/latest/concepts/architecture)
explains where an adapter like this one fits.

### Setting up Nautilus

This adapter is an addition to a working Nautilus install, not a standalone
program. Get Nautilus running before you come back here:

- **[Installation guide](https://nautilustrader.io/docs/latest/getting_started/installation)**
  — covers pre-built wheels, the Nautech package index, and building from
  source.
- **[Live trading concepts](https://nautilustrader.io/docs/latest/concepts/live)**
  — this adapter only runs in a live node.
- **[Rust API reference](https://nautilustrader.io/docs/rust-api-latest/)** —
  this adapter is Rust-only, so this is the reference you will actually use.

## Deriverse

**[Deriverse](https://deriverse.io)** is a non-custodial, fully on-chain
central limit order book on Solana, for spot and derivatives. "Fully on-chain"
is the important part and it shapes everything below: there is no exchange
server, no REST API, and no API key. The order book *is* a Solana account, and
placing an order *is* a Solana transaction signed by your own wallet. Nobody
takes custody of your funds.

Start with the **[Deriverse User Guide](https://deriverse.gitbook.io/deriverse-v1)**
— it explains the platform from a trader's point of view:
[onboarding and account activation](https://deriverse.gitbook.io/deriverse-v1/onboarding-and-account-activation),
[deposits and withdrawals](https://deriverse.gitbook.io/deriverse-v1/deposits-and-withdrawals),
and [working with orders](https://deriverse.gitbook.io/deriverse-v1/working-with-orders).
Reading those three will tell you what your strategy is actually doing when it
talks to this venue.

For protocol-level detail the user guide does not cover — program IDs, account
layouts, instruction encoding — the reference is the venue's own TypeScript
SDK, [`kit`](https://github.com/deriverse/kit), with
[`kit-example`](https://github.com/deriverse/kit-example) as a short worked
project. There is no hosted API reference. Also worth a look: the
[Deriverse site](https://deriverse.io) and the
[whitepaper](https://github.com/deriverse/Whitepaper).

### Worth knowing

- **Price band.** Limit orders must be priced within roughly ±9.375% of a
  mid-anchored reference price (the venue's `MAX_DEVIATION`, measured
  empirically). Outside that, the venue rejects with `InvalidPrice` (custom
  error 131).
- **No size clamping.** Selling more than your available balance is rejected
  outright with `InsufficientFunds` (custom error 132) — the venue will not
  silently reduce your order to fit.
- **Self-trade prevention.** The venue rejects any order that would match
  against your own resting orders. Worth knowing if you run more than one
  strategy on a single wallet — they share an account, and they will reject
  each other.
- **Spot only, for now.** The venue's own documentation confirms perpetuals
  are not yet live.
- **Finality is a Solana concern.** An order is not live until its transaction
  confirms. Fees, priority fees and confirmation latency behave like any Solana
  transaction, which is why this adapter exposes compute and priority-fee
  settings.

## What the adapter does today

Because Deriverse has no gateway, this adapter speaks Solana directly:

| Concern | How it works |
|---|---|
| Market data | Solana RPC `accountSubscribe` on the instrument account |
| Orders | Signed v0 Solana transactions, built and signed locally |
| Fills | Decoded from your own transaction logs, confirmed by signature polling |
| Byte layouts | Taken from the venue's own [`drv-models`](https://github.com/deriverse/drv-smart-contract-common) crate — never re-declared here |

**Status — please read before planning around this.**

| Surface | Status |
|---|---|
| Transport substrate (`rpc/`, `decode/`, `tx/`) | **Live-proven on Solana mainnet** — golden-gated signing, live soak, live order round-trip |
| Market-data client (`data.rs`, `factories.rs`) | **Live-proven on Solana mainnet**, wired into Nautilus |
| Order execution **through Nautilus** | **Not yet available.** No `ExecutionClient` is implemented |
| Perpetual futures | Fixture-proven, waiting on the venue's perp launch |
| Python bindings | Not started |

The gap worth being explicit about: you can feed Deriverse data into a Nautilus
strategy today, but calling `submit_order(...)` in that strategy will not send
an order to Deriverse, because nothing is registered to route it there. Orders
are currently placed through this crate's own transaction layer and the
`drv-spot-order` operator binary, outside the Nautilus execution engine. A
Nautilus `ExecutionClient` is the next milestone.

Verified current as of 2026-09-08 against `drv-models` `v0.2.74` and
`@deriverse/kit` `1.0.76`.

## Adding Deriverse to your Nautilus node

### Data-only

Reading market data needs no wallet, no key and no venue account. Three steps.

#### 1. Depend on the crate

```toml
[dependencies]
nautilus-deriverse = { git = "https://github.com/cryptomaltese/nautilus-deriverse" }
```

The Nautilus crates are pinned to a specific `develop` git revision rather than
a crates.io version, because the published `0.59.0` was cut later and is
API-incompatible with this code. Your project must resolve `nautilus-common`,
`nautilus-core`, `nautilus-live`, `nautilus-model` and `nautilus-network` to the
**same revision** this crate pins — see `Cargo.toml`. If you already vendor
Nautilus, a `[patch]` section pointing at your checkout is the usual way.

Rust 1.96.0 or newer.

#### 2. Register the data client

Nautilus builds a live node from a builder, and each venue contributes a
*factory* plus a *config*. Deriverse contributes a data client:

```rust
use nautilus_deriverse::config::DeriverseDataClientConfig;
use nautilus_deriverse::factories::DeriverseDataClientFactory;
use nautilus_common::enums::Environment;
use nautilus_live::node::LiveNode;
use nautilus_model::identifiers::TraderId;

let trader_id = TraderId::from("MY-TRADER-001");

let mut builder = LiveNode::builder(trader_id, Environment::Live)?
    .with_name("my-deriverse-node".to_string());

let data_config = DeriverseDataClientConfig::default(); // mainnet defaults

builder = builder.add_data_client(
    None,                                              // use the default client id
    Box::new(DeriverseDataClientFactory::default()),
    Box::new(data_config),
)?;

let mut node = builder.build()?;
node.run().await;
```

That is the whole integration. From here Deriverse is just another venue to
your strategies.

#### 3. Point it at an RPC endpoint

Every field is optional and defaults to Deriverse mainnet:

| Field | Default | What it is |
|---|---|---|
| `rpc_url` | `https://rpc-mainnet.deriverse.io` | Solana RPC, HTTP — snapshots and reconciliation |
| `ws_url` | `wss://rpc-mainnet.deriverse.io` | Solana RPC, WebSocket — the `accountSubscribe` live feed |
| `program_id` | mainnet program | The Deriverse on-chain program |
| `program_version` | mainnet version | Override **together with** `program_id` to target devnet |
| `commitment` | `Confirmed` | Solana commitment level |
| `http_timeout_secs` | — | HTTP request timeout |
| `reconcile_interval_secs` | — | How often to re-fetch a snapshot and check it against the live feed |
| `staleness_threshold_slots` | — | How many slots without an update before the feed is flagged stale |

Any Solana RPC provider that supports `accountSubscribe` will work. The default
endpoint is Deriverse's own. A private or paid endpoint is worth considering for
production — public RPCs rate-limit aggressively, and this adapter holds a
long-lived subscription.

```rust
let data_config = DeriverseDataClientConfig {
    rpc_url: Some("https://my-private-rpc.example.com".into()),
    ws_url:  Some("wss://my-private-rpc.example.com".into()),
    ..Default::default()
};
```

### Execution

Placing orders needs a funded Solana wallet and an activated Deriverse account.

Note where these orders go: **not** through Nautilus. As the status table above
says, there is no `ExecutionClient` yet, so a wallet configured here is used by
this crate's own transaction layer and by the `drv-spot-order` binary — not by
`submit_order(...)` in a strategy.

#### 4. Set up your wallet

Deriverse is non-custodial, so "your account" is a Solana keypair you control
plus some on-chain accounts derived from it. The `drv-setup` binary walks
through it:

```sh
# 1. Create a hot wallet. Prints the public key only — the secret never leaves
#    the file it is written to.
cargo run --bin drv-setup -- keygen --keypair ~/.config/deriverse/hot.json

# 2. Fund that address with SOL and USDC, then check what the venue sees.
#    You need at least 0.02 SOL: Solana charges account rent, which is held
#    rather than spent — it comes back if the account is closed. Transaction
#    fees come out of the same balance.
cargo run --bin drv-setup -- check --keypair ~/.config/deriverse/hot.json

# 3. One-time: create your Deriverse client accounts and make a first deposit.
#    Both happen in a single instruction.
cargo run --bin drv-setup -- init-deposit \
    --keypair ~/.config/deriverse/hot.json --usdc 100
```

Step 3 is not optional bookkeeping — on Deriverse your account *becomes* active
when that first deposit confirms on-chain, which is what the
[onboarding guide](https://deriverse.gitbook.io/deriverse-v1/onboarding-and-account-activation)
describes doing through the web UI. `drv-setup init-deposit` is the same thing
without the browser.

The keypair file is the standard Solana format — a JSON array of 64 bytes, the
same thing `solana-keygen` produces, so an existing wallet works. Point the exec
config at it:

```rust
use nautilus_deriverse::config::DeriverseExecClientConfig;

let exec_config = DeriverseExecClientConfig {
    keypair_path: Some("~/.config/deriverse/hot.json".into()),
    market_slippage_bps: 50,
    ..Default::default()
};
```

Key material is never written to logs or error messages — only the public key
and the file path ever appear. Treat the file as a secret: a hot wallet holds
real funds, so fund it with what you are willing to have exposed.

Other exec-config fields: `priority_fee_microlamports` and
`compute_unit_limit` (Solana fee/compute budget), `blockhash_refresh_secs` and
`signature_poll_interval_ms` (transaction lifecycle timing),
`market_slippage_bps` (MARKET orders are sent as bounded crossing limits — this
sets the bound), and `leverage` (1–100, or `None` for the venue maximum).

## Using Deriverse from a Nautilus strategy

If you are new to Nautilus: you do not call the adapter. You write a strategy
that implements the `DataActor` trait, the node delivers data to it through
callbacks, and you ask for the data you want by subscribing when the strategy
starts.

**What this adapter supports:** quote ticks, for one instrument per client.

```rust
use nautilus_common::actor::DataActor;
use nautilus_model::data::QuoteTick;

impl DataActor for MyStrategy {
    fn on_start(&mut self) -> anyhow::Result<()> {
        Strategy::on_start(self)?;
        // (instrument, client id, params) — name the client so the
        // subscription is routed to Deriverse rather than another venue.
        self.subscribe_quotes(self.instrument_id, Some(self.client_id), None);
        Ok(())
    }

    fn on_quote(&mut self, quote: &QuoteTick) -> anyhow::Result<()> {
        let (bid, ask) = (quote.bid_price, quote.ask_price);
        // your logic here
        Ok(())
    }
}
```

A `QuoteTick` here is the top of the Deriverse spot ladder — best bid and best
ask with their sizes (`bid_price`, `ask_price`, `bid_size`, `ask_size`). A
one-sided book emits nothing, because a Nautilus `QuoteTick` requires both
sides.

**What is not implemented**, so you know not to reach for it: trade ticks, order
book deltas or snapshots, bars, and instrument discovery. Subscribing to those
will not produce data. As noted above, order submission through Nautilus is not
available either.

The node handles `connect` / `disconnect` / `start` / `stop` / `reset` for you
as part of its lifecycle; you do not call them yourself.

## Operator tools

Four binaries ship with the crate. The first two are read-only and safe to run
against mainnet at any time:

| Binary | What it does |
|---|---|
| `drv-probe version` | RPC sanity check — prints the current slot and block height |
| `drv-probe account --instr 0` | Fetches and decodes an instrument, prints top of book |
| `drv-probe soak --minutes N` | Runs the real data client for N minutes and verifies quote monotonicity and reconciliation |
| `drv-capture` | Saves an instrument account snapshot into `tests/fixtures/` |
| `drv-setup` | Wallet onboarding — `keygen`, `check`, `init-deposit` (**sends transactions**) |
| `drv-spot-order` | Attended spot order place/cancel (**sends transactions**) |

```sh
cargo run --bin drv-probe -- version
cargo run --bin drv-probe -- account --instr 0
```

## Build and test

```sh
cargo build
cargo test
```

Rust 1.96.0 minimum. The test suite is hermetic and needs no network or
credentials — the two live probes are `#[ignore]`d and run only on request:

```sh
cargo test --test live_probe -- --ignored --nocapture
```

Transaction encoding is locked down by golden fixtures under `tests/fixtures/`,
generated from the venue's own TypeScript kit. Regenerating them against a newer
kit and getting a byte-identical result is how we confirm the adapter still
matches the venue.

## Contributing

> **This repository is a generated mirror.** Every commit here is produced
> automatically from the adapter's upstream source of truth; no commit
> originates in this repository. Issues and pull requests are very welcome —
> they are imported upstream rather than merged here. See
> [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
