// SPDX-License-Identifier: AGPL-3.0-only

//! The in-process event bus. Writers emit an [`Event`] after a change is
//! committed (the incident API, and the engine for check and component
//! changes); subscribers (the SSE stream, notifiers) each get every event
//! sent after they subscribe. Events share the incident through an `Arc`,
//! so each subscriber's copy of an event costs a reference count, not a
//! copy of every update.

use std::sync::Arc;

use serde::Serialize;
use tocsin_core::incident::Incident;
use tocsin_engine::ports::Sink;
use tocsin_engine::{CheckChange, ComponentChange, EngineEvent, NoticesHandle};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

/// How many events a slow subscriber may fall behind before it misses
/// some (it then sees `RecvError::Lagged` and can resynchronize).
const CAPACITY: usize = 256;

/// Something that changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    /// A check's confirmed state changed. Alerting keys off
    /// [`CheckChange::alerts`], which is false inside maintenance.
    CheckChanged(CheckChange),
    /// A component's displayed status changed.
    ComponentChanged(ComponentChange),
}

impl From<EngineEvent> for Event {
    fn from(event: EngineEvent) -> Self {
        match event {
            EngineEvent::CheckChanged(change) => Event::CheckChanged(change),
            EngineEvent::ComponentChanged(change) => Event::ComponentChanged(change),
        }
    }
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
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// The engine publishes on the same bus as the incident API.
impl Sink for EventBus {
    fn emit(&self, event: EngineEvent) {
        EventBus::emit(self, event.into());
    }
}

/// Tells the engine whenever an incident or maintenance changes, so a
/// component's status follows the write at once instead of at the engine's
/// next periodic re-read. Runs until the bus closes or it is aborted.
pub async fn wake_on_incident_changes(
    mut events: broadcast::Receiver<Event>,
    engine: NoticesHandle,
) {
    loop {
        match events.recv().await {
            Ok(
                Event::IncidentCreated { .. }
                | Event::IncidentUpdated { .. }
                | Event::IncidentEdited { .. },
            )
            // Some of the events missed may have been incident changes.
            | Err(RecvError::Lagged(_)) => engine.notices_changed(),
            Ok(Event::CheckChanged(_) | Event::ComponentChanged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}
