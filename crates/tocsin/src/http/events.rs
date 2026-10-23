// SPDX-License-Identifier: AGPL-3.0-only

//! The in-process event bus. Writers emit an [`Event`] after a change is
//! committed (the incident and release APIs, and the engine for check and
//! component changes); subscribers (the SSE stream, notifiers) each get every event
//! sent after they subscribe. Events share the incident through an `Arc`,
//! so each subscriber's copy of an event costs a reference count, not a
//! copy of every update.
//!
//! `GET /api/v1/events` forwards the bus as server-sent events: the status
//! page's live updates, and anything else that wants to follow along.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::sse::{self, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tocsin_core::incident::Incident;
use tocsin_core::release::Release;
use tocsin_engine::ports::Sink;
use tocsin_engine::{CheckChange, ComponentChange, EngineEvent, NoticesHandle};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::Viewer;
use super::error::ErrorBody;

/// The OpenAPI tag for the event stream.
pub const TAG: &str = "events";

/// The stream's event name for "you may have missed something": sent
/// first on every connection and after the stream fell behind the bus.
/// A client re-reads whatever it shows.
pub const RESYNC: &str = "resync";

/// How long a quiet stream waits before a comment line, so proxies that
/// close idle connections leave it open.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

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
    /// A release was published, or published again with new fields.
    ReleasePublished {
        /// As stored.
        release: Arc<Release>,
        /// Whether its project is public: a subscriber that shows events
        /// to anyone forwards only these.
        public: bool,
    },
    /// A deploy of a release was stamped; before its publish, the release
    /// is a stub with an empty title and no `released_at`.
    ReleaseDeployed {
        /// As stored, with the new `deployed_at`.
        release: Arc<Release>,
        /// Whether its project is public.
        public: bool,
    },
    /// A check's confirmed state changed. Alerting keys off
    /// [`CheckChange::alerts`], which is false inside maintenance.
    CheckChanged(CheckChange),
    /// A component's displayed status changed.
    ComponentChanged(ComponentChange),
}

impl Event {
    /// The event's `type`, which the stream also uses as the SSE event
    /// name.
    pub fn name(&self) -> &'static str {
        match self {
            Event::IncidentCreated { .. } => "incident_created",
            Event::IncidentUpdated { .. } => "incident_updated",
            Event::IncidentEdited { .. } => "incident_edited",
            Event::ReleasePublished { .. } => "release_published",
            Event::ReleaseDeployed { .. } => "release_deployed",
            Event::CheckChanged(_) => "check_changed",
            Event::ComponentChanged(_) => "component_changed",
        }
    }

    /// Whether `viewer` may see this event: everything but a release of a
    /// project that is not public, which only a trusted viewer sees.
    fn visible_to(&self, viewer: Viewer) -> bool {
        match self {
            Event::ReleasePublished { public, .. } | Event::ReleaseDeployed { public, .. } => {
                *public || viewer == Viewer::Trusted
            }
            Event::IncidentCreated { .. }
            | Event::IncidentUpdated { .. }
            | Event::IncidentEdited { .. }
            | Event::CheckChanged(_)
            | Event::ComponentChanged(_) => true,
        }
    }
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
    /// Becomes `true` once, when the server stops: every open stream then
    /// ends, so draining requests does not wait out connections that never
    /// finish on their own.
    closing: Arc<watch::Sender<bool>>,
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            sender: broadcast::channel(CAPACITY).0,
            closing: Arc::new(watch::channel(false).0),
        }
    }

    /// Ends every event stream, open or opened later. Emitting still works,
    /// for the writes that finish while the server drains.
    pub fn close(&self) {
        self.closing.send_replace(true);
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

/// The routes in the OpenAPI document.
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(stream_events))
}

/// Every event from now on, as server-sent events: the event name is the
/// event's `type` and the data its JSON. The first event is `resync`, and
/// so is the one after the stream fell behind and missed some; on either,
/// re-read what you show. A comment line every 15 seconds keeps a quiet
/// connection open. Anyone sees incident, check and component events, and
/// releases of public projects; a `status:read` token sees every release.
#[utoipa::path(
    get,
    path = "/api/v1/events",
    tag = TAG,
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "An endless `text/event-stream`", content_type = "text/event-stream", body = String),
        (status = 401, description = "An `Authorization` header with an unknown token", body = ErrorBody),
    ),
)]
pub async fn stream_events(State(state): State<AppState>, viewer: Viewer) -> Response {
    // Only the receivers: an open stream must not keep the store alive
    // past shutdown.
    let subscription = Subscription {
        events: state.events.subscribe(),
        closing: state.events.closing.subscribe(),
        viewer,
        greeted: false,
    };
    let stream = futures_util::stream::unfold(subscription, |mut subscription| async move {
        let event = subscription.next().await?;
        Some((Ok::<_, Infallible>(event), subscription))
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response();
    // nginx buffers a response unless told not to, which would hold events
    // back until the buffer fills.
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response
}

/// One client's place on the bus.
struct Subscription {
    events: broadcast::Receiver<Event>,
    closing: watch::Receiver<bool>,
    viewer: Viewer,
    /// Whether the opening `resync` was sent.
    greeted: bool,
}

impl Subscription {
    /// The next event to send; `None` ends the stream, when the server
    /// stops.
    async fn next(&mut self) -> Option<sse::Event> {
        if !self.greeted {
            self.greeted = true;
            return Some(resync());
        }
        loop {
            tokio::select! {
                biased;
                // Also ends the stream if the bus is gone, which only
                // happens once the server is.
                _ = self.closing.wait_for(|closing| *closing) => return None,
                received = self.events.recv() => match received {
                    Ok(event) if event.visible_to(self.viewer) => return Some(message(&event)),
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => return Some(resync()),
                    Err(RecvError::Closed) => return None,
                },
            }
        }
    }
}

/// `event` as the stream sends it.
fn message(event: &Event) -> sse::Event {
    let data = serde_json::to_string(event).expect("an event serializes");
    sse::Event::default().event(event.name()).data(data)
}

/// The `resync` event: no data beyond its name.
fn resync() -> sse::Event {
    sse::Event::default().event(RESYNC).data("{}")
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
            Ok(
                Event::ReleasePublished { .. }
                | Event::ReleaseDeployed { .. }
                | Event::CheckChanged(_)
                | Event::ComponentChanged(_),
            ) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use tocsin_core::health::Health;
    use tocsin_core::incident::{Actor, Impact, IncidentKind, IncidentStatus, Timestamp};
    use tocsin_core::state::ComponentStatus;
    use tocsin_engine::MemberKind;

    use super::*;

    pub(crate) fn every_event(public: bool) -> Vec<Event> {
        let actor = Actor::token("ops");
        let incident = Arc::new(Incident {
            id: 1,
            kind: IncidentKind::Incident,
            title: "API errors".to_owned(),
            impact: Impact::Major,
            status: IncidentStatus::Investigating,
            components: vec!["api".to_owned()],
            scheduled_start: None,
            scheduled_end: None,
            resolved_at: None,
            created_at: Timestamp::from_millis(0),
            created_by: actor.clone(),
            updated_at: Timestamp::from_millis(0),
            updated_by: actor.clone(),
            updates: Vec::new(),
        });
        let release = Arc::new(Release {
            id: 1,
            project: "api".to_owned(),
            version: "1.0.0".to_owned(),
            environment: "production".to_owned(),
            title: "1.0.0".to_owned(),
            notes: String::new(),
            url: None,
            released_at: None,
            deployed_at: None,
            created_at: Timestamp::from_millis(0),
            created_by: actor.clone(),
            updated_at: Timestamp::from_millis(0),
            updated_by: actor,
        });
        vec![
            Event::IncidentCreated {
                incident: incident.clone(),
            },
            Event::IncidentUpdated {
                incident: incident.clone(),
            },
            Event::IncidentEdited { incident },
            Event::ReleasePublished {
                release: release.clone(),
                public,
            },
            Event::ReleaseDeployed { release, public },
            Event::CheckChanged(CheckChange {
                check_id: "api-health".to_owned(),
                kind: MemberKind::Check,
                component_id: Some("api".to_owned()),
                from: Some(Health::Up),
                to: Health::Down,
                since_ms: 0,
                confirmed_at_ms: 0,
                ended_outage_id: None,
                opened_outage_id: Some(1),
                maintenance: false,
                alert: true,
            }),
            Event::ComponentChanged(ComponentChange {
                component_id: "api".to_owned(),
                from: ComponentStatus::Operational,
                to: ComponentStatus::Down,
                health: Some(Health::Down),
                at_ms: 0,
            }),
        ]
    }

    /// The status page names the SSE events it swaps on; the name has to be
    /// the `type` a client of the JSON already knows.
    #[test]
    fn each_event_is_named_after_its_type() {
        for event in every_event(true) {
            let json = serde_json::to_value(&event).unwrap();
            assert_eq!(json["type"], event.name(), "{json}");
        }
    }

    #[test]
    fn a_release_of_a_private_project_reaches_only_a_trusted_viewer() {
        for event in every_event(false) {
            let release = matches!(
                event,
                Event::ReleasePublished { .. } | Event::ReleaseDeployed { .. }
            );
            assert_eq!(
                event.visible_to(Viewer::Public),
                !release,
                "{}",
                event.name()
            );
            assert!(event.visible_to(Viewer::Trusted), "{}", event.name());
        }
        assert!(
            every_event(true)
                .iter()
                .all(|event| event.visible_to(Viewer::Public))
        );
    }
}
