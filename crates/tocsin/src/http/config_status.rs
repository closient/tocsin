// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/config/status`: whether the configuration last reloaded.
//!
//! It needs a `status:read` token: a failed reload's errors name the
//! configuration's files and the environment variables its secrets come
//! from, which say more about the host than a status page should.

use axum::Json;
use axum::extract::State;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::{Authorized, scope};
use super::error::ErrorBody;
use crate::reload::ConfigStatus;

/// The OpenAPI tag for this route.
pub const TAG: &str = "config";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_config_status))
}

/// When the running configuration was loaded, and whether the last reload
/// put its configuration in place. After a failed one, `ok` is false,
/// `errors` says why, and the configuration from `loaded_at` is still
/// running.
#[utoipa::path(
    get,
    path = "/api/v1/config/status",
    tag = TAG,
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The last reload's outcome", body = ConfigStatus),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "A token without the `status:read` scope", body = ErrorBody),
    ),
)]
pub async fn get_config_status(
    State(state): State<AppState>,
    _reader: Authorized<scope::StatusRead>,
) -> Json<ConfigStatus> {
    Json(state.config_status.borrow().clone())
}
