//! Account + log decoding over the pinned `drv-models` structs (spec §2.2).
//! Pure functions, fixture-tested. Decode is offset-exact against the pinned
//! tag; discriminator (tag + version) is verified before any field read
//! (Invariant 7).

pub mod instr;
pub mod logs;
pub mod token;
