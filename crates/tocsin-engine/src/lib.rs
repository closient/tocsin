// SPDX-License-Identifier: AGPL-3.0-only

//! The scheduler and state engine.
//!
//! The engine runs every configured check on its own task, at its interval
//! plus a fixed per-check jitter, never more than
//! `server.max_concurrent_checks` at once. Every heartbeat has a task too,
//! which records each ping it is handed and a down result whenever a
//! deadline passes without one. It stores every result, turns
//! results into confirmed state changes ([`tocsin_core::state::Confirmation`]),
//! opens and ends outage records, derives each component's status from its
//! checks, open incidents and maintenance windows, and emits one
//! [`EngineEvent`] per change.
//!
//! Everything the engine touches outside itself goes through a port in
//! [`ports`], so tests drive it with fakes on a paused clock and the binary
//! wires it to the real probes, store and event bus. The rules themselves
//! (jitter, confirmation, component health and status) are pure functions in
//! [`tocsin_core::state`].
//!
//! See `docs/engine.md` for the behaviour in prose.

mod engine;
mod event;
mod notice;
mod plan;
pub mod ports;

pub use crate::engine::{
    CheckSnapshot, ComponentSnapshot, Engine, HEARTBEAT_CONFIRM, HeartbeatHandle, LastResult,
    NoticesHandle, PING_MESSAGE_LIMIT, PROBE_BACKSTOP, Parts, Ping, PingError, Snapshot,
    StartError,
};
pub use crate::event::{CheckChange, ComponentChange, EngineEvent, MemberKind};
pub use crate::notice::{Notice, impact_health};
pub use crate::plan::{Plan, PlanError};
