// SPDX-License-Identifier: AGPL-3.0-only

//! The in-process event bus. Writers emit an [`Event`] after a change is
//! committed; subscribers (the SSE stream, notifiers) each get every event
//! sent after they subscribe. Events share the incident through an `Arc`,
//! so each subscriber's copy of an event costs a reference count, not a
//! copy of every update.

use std::sync::Arc;

use serde::Serialize;
use tocsin_core::incident::Incident;
use tokio::sync::broadcast;

/// How many events a slow subscriber may fall behind before it misses
/// some (it then sees `RecvError::Lagged` and can resynchronize).
const CAPACITY: usize = 256;

/// Something that changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(
    clippy::enum_variant_names,
    reason = "heartbeat and release events join these"
)]
pub enum Event {
    /// An incident or maintenance was opened.
    IncidentCreated {
        /// As stored, with its first update.
        incident: Arc<Incident>,
    },
    /// An update was appended.
    IncidentUpdated {
        /// As stored, with the new update last.
        incident: Arc<Incident>,
    },
    /// Title, impact, components or schedule were edited.
    IncidentEdited {
        /// As stored after the edit.
        incident: Arc<Incident>,
    },
}

/// The sending side, cheap to clone into every handler.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            sender: broadcast::channel(CAPACITY).0,
        }
    }

    /// Sends `event` to every current subscriber. Having none is normal.
    pub fn emit(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    /// A receiver for every event emitted from now on.
    #[allow(dead_code, reason = "the SSE stream subscribes; tests do today")]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
