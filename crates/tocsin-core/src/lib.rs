// SPDX-License-Identifier: AGPL-3.0-only

//! Domain types, the configuration model and assertions for tocsin.
//!
//! This crate has no I/O. The binary loads configuration files and the
//! store crate persists history; both depend on the types defined here.

pub mod alert;
pub mod config;
pub mod federation;
pub mod health;
pub mod incident;
pub mod link;
pub mod probe;
pub mod release;
pub mod silence;
pub mod state;
pub mod status;

pub use health::Health;
