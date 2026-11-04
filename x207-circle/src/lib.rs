//! `x207-circle`: mutual-credit circle contract. Interface: see `README.md` and `abi.json`.
//!
//! The contract root lives in `state.rs` and every process has its own module with its own
//! `#[near] impl X207Circle` block, so lanes working in parallel do not need to edit this file.

pub mod asset_contracts;
pub mod bundle;
pub mod canonical;
pub mod clearing;
pub mod commit;
pub mod config;
pub mod deal;
pub mod escrow;
pub mod events;
pub mod freeze;
pub mod grant;
pub mod intents_withdraw;
pub mod member;
pub mod params;
pub mod pool;
pub mod prune;
pub mod risk;
pub mod settlement;
pub mod stake;
pub mod state;
#[cfg(test)]
mod stateful_tests;
pub mod stubs;
pub mod types;
pub mod upgrade;
pub mod verifier;
pub mod voucher;

pub use state::X207Circle;

/// Interface version; same value as the `version` field of every event (spec section 1).
pub const INTERFACE_VERSION: &str = "0.0.13";
