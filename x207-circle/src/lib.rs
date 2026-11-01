//! `x207-circle`: mutual-credit circle contract. Interface: `x207 - Spec Antarmuka Kontrak`.
//!
//! The contract root lives in `state.rs` and every process has its own module with its own
//! `#[near] impl X207Circle` block, so lanes working in parallel do not need to edit this file.

pub mod bundle;
pub mod canonical;
pub mod clearing;
pub mod config;
pub mod events;
pub mod member;
pub mod params;
pub mod pool;
pub mod risk;
pub mod settlement;
pub mod state;
pub mod types;
pub mod verifier;
pub mod voucher;

pub use state::X207Circle;

/// Interface version; same value as the `version` field of every event (spec section 1).
pub const INTERFACE_VERSION: &str = "0.0.1";
