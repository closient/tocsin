// SPDX-License-Identifier: AGPL-3.0-only

//! Which checks and heartbeats a reader may see: the one rule every
//! surface applies.
//!
//! A check or heartbeat is public when it belongs to a component, since
//! every `[[component]]` is on the status page. One on no component is
//! internal: the board at `/internal/` and a `status:read` token see it,
//! and nothing anonymous names it, its state or its outages. That covers
//! `/api/v1/checks`, the event stream, `/api/v1/outages`, the timeline, the
//! status page and the MCP tools a token without `status:read` calls.
//! Component state needs no filter: the engine derives it from the
//! component's own checks, so an internal check never moves it.

use tocsin_core::config::{Config, Id};
use tocsin_store::Checks;

use super::auth::Viewer;

/// Whether a check or heartbeat on `component` is public.
pub fn is_public(component: Option<&str>) -> bool {
    component.is_some()
}

/// Whether `viewer` may see a check or heartbeat on `component`.
pub fn may_see(viewer: Viewer, component: Option<&str>) -> bool {
    viewer == Viewer::Trusted || is_public(component)
}

/// The checks and heartbeats whose outages `viewer` may read. Outages
/// outlive the configuration: a configured check's follow where it is
/// now, so moving it off its component hides them too, and a removed
/// check's follow the component each recorded (see [`Checks::Public`]).
pub fn shown(config: &Config, viewer: Viewer) -> Checks {
    match viewer {
        Viewer::Trusted => Checks::All,
        Viewer::Public => {
            let (public, internal) = config
                .checks
                .iter()
                .map(|check| (&check.id, &check.component))
                .chain(
                    config
                        .heartbeats
                        .iter()
                        .map(|heartbeat| (&heartbeat.id, &heartbeat.component)),
                )
                .partition::<Vec<_>, _>(|(_, component)| {
                    is_public(component.as_ref().map(Id::as_str))
                });
            let ids = |members: Vec<(&Id, &Option<Id>)>| {
                members
                    .into_iter()
                    .map(|(id, _)| id.as_str().to_owned())
                    .collect()
            };
            Checks::Public {
                public: ids(public),
                internal: ids(internal),
            }
        }
    }
}
