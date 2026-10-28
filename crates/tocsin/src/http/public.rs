// SPDX-License-Identifier: AGPL-3.0-only

//! Which components, checks and heartbeats a reader may see: the one rule
//! every surface applies.
//!
//! A component is public unless its `[[component]]` says `public = false`.
//! A check or heartbeat is public when it belongs to a public component.
//! One on no component, or on a component that is not public, is internal:
//! the board at `/internal/` and a `status:read` token see it, and nothing
//! anonymous names it, its state or its outages. A component that is not
//! public is hidden the same way, with everything on it. That covers
//! `/api/v1/status.json`, `/api/v1/components`, `/api/v1/checks`, the event
//! stream, `/api/v1/outages`, the timeline, `llms.txt`, the status page,
//! subscriber choices and the MCP tools a token without `status:read`
//! calls. A public component's state needs no filter: the engine derives
//! it from the component's own checks, so an internal check never moves
//! it.

use tocsin_core::config::{Config, Id};
use tocsin_store::Checks;

use super::auth::Viewer;

/// Whether `[[component]]` `id` is configured and public.
pub fn component_is_public(config: &Config, id: &str) -> bool {
    config
        .components
        .iter()
        .any(|component| component.id.as_str() == id && component.public)
}

/// Whether `viewer` may see `[[component]]` `id`: it is configured, and
/// public unless `viewer` is trusted.
pub fn shows_component(config: &Config, viewer: Viewer, id: &str) -> bool {
    match viewer {
        Viewer::Trusted => config
            .components
            .iter()
            .any(|component| component.id.as_str() == id),
        Viewer::Public => component_is_public(config, id),
    }
}

/// Whether a check or heartbeat on `component` is public.
pub fn is_public(config: &Config, component: Option<&str>) -> bool {
    component.is_some_and(|id| component_is_public(config, id))
}

/// Whether `viewer` may see a check or heartbeat on `component`.
pub fn may_see(config: &Config, viewer: Viewer, component: Option<&str>) -> bool {
    viewer == Viewer::Trusted || is_public(config, component)
}

/// The checks and heartbeats whose outages `viewer` may read. Outages
/// outlive the configuration: a configured check's follow where it is
/// now, so moving it off its component, or onto one that is not public,
/// hides them too, and a removed check's follow the component each
/// recorded (see [`Checks::Public`]). A quorum check's are the quorum's
/// alone: the public never reads one node's view of it.
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
                    is_public(config, component.as_ref().map(Id::as_str))
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
                internal_components: config
                    .components
                    .iter()
                    .filter(|component| !component.public)
                    .map(|component| component.id.as_str().to_owned())
                    .collect(),
                quorum: config
                    .checks
                    .iter()
                    .filter(|check| config.quorum(check).is_some())
                    .map(|check| check.id.as_str().to_owned())
                    .collect(),
            }
        }
    }
}
