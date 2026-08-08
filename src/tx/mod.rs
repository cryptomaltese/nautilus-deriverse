//! Transaction pipeline (spec §1.6): instruction build → v0 tx → sign →
//! send → confirm. Porting SoT: kit `engine/{spot-instructions.ts,
//! instructions.ts, context-builders.ts, account-helpers.ts}` +
//! kit-example `lib/transactions.ts`; conformance gate: the checked-in kit
//! goldens (tests/golden_*.rs).

pub mod accounts;
pub mod build;
pub mod send;
