// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/releases`: the release log and deploy stamps.
//!
//! What a reader may see is decided in one place, [`visible_projects`]:
//! every read of releases, here, on the timeline and on the HTML pages,
//! passes its answer to the store, which filters in SQL.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use tocsin_core::config::Config;
use tocsin_core::incident::{Actor, FieldError, Timestamp};
use tocsin_core::release::{
    DeployStamp, NewRelease, Rejected, Release, ReleaseCursor, ReleaseList,
};
use tocsin_store::{Projects, ReleaseQuery, Written};
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::{Authorized, Viewer, scope};
use super::error::{ApiError, ApiJson, ErrorBody, OptionalJson};
use super::events::Event;
use super::{AppState, now};

/// The OpenAPI tag for these routes.
pub const TAG: &str = "releases";

/// Page size when `limit` is not given.
pub const DEFAULT_LIMIT: u32 = 50;
/// Largest page.
pub const MAX_LIMIT: u32 = 200;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_releases, publish_release))
        .routes(routes!(stamp_deploy))
}

/// The projects `viewer` may see releases of: every one for a trusted
/// viewer, otherwise the configured projects marked `public`. A release
/// whose project is no longer configured is therefore never public.
///
/// Every read of releases goes through here; nothing else decides.
pub fn visible_projects(config: &Config, viewer: Viewer) -> Projects {
    match viewer {
        Viewer::Trusted => Projects::All,
        Viewer::Public => Projects::Only(
            config
                .projects
                .iter()
                .filter(|project| project.public)
                .map(|project| project.id.as_str().to_owned())
                .collect(),
        ),
    }
}

/// `?project=&since=&before=&limit=` on the list.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    /// Only this project's releases. A project the reader may not see
    /// matches nothing, like one that does not exist.
    pub project: Option<String>,
    /// Only releases published at or after this instant (RFC 3339).
    pub since: Option<Timestamp>,
    /// Only releases older than the previous page's last: its
    /// `next_before`.
    pub before: Option<String>,
    /// Page size, 1 to 200; default 50.
    pub limit: Option<u32>,
}

/// Published releases, newest first. Without a `status:read` token only
/// public projects' releases are listed.
#[utoipa::path(
    get,
    path = "/api/v1/releases",
    tag = TAG,
    params(ListQuery),
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "Newest `released_at` first", body = ReleaseList),
        (status = 401, description = "An `Authorization` header with an unknown token", body = ErrorBody),
        (status = 422, description = "A bad `since`, `before` or `limit`", body = ErrorBody),
    ),
)]
pub async fn list_releases(
    State(state): State<AppState>,
    viewer: Viewer,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<ReleaseList>, ApiError> {
    let Query(query) = query
        .map_err(|rejection| ApiError::invalid(vec![FieldError::new("", rejection.body_text())]))?;
    list(&state, viewer, query).await.map(Json)
}

/// One page of the releases `viewer` may see, newest first: the read
/// behind `GET /api/v1/releases`.
///
/// # Errors
///
/// 422 for a bad `before` or `limit`.
pub async fn list(
    state: &AppState,
    viewer: Viewer,
    query: ListQuery,
) -> Result<ReleaseList, ApiError> {
    let mut errors = Vec::new();
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        errors.push(FieldError::new(
            "limit",
            format!("must be 1 to {MAX_LIMIT}"),
        ));
    }
    let before = match query.before.as_deref().map(str::parse::<ReleaseCursor>) {
        Some(Err(bad)) => {
            errors.push(FieldError::new("before", bad.to_string()));
            None
        }
        Some(Ok(cursor)) => Some(cursor),
        None => None,
    };
    if !errors.is_empty() {
        return Err(ApiError::invalid(errors));
    }
    let (releases, truncated) = state
        .store
        .releases(ReleaseQuery {
            visible: visible_projects(&state.config, viewer),
            project: query.project,
            version: None,
            since: query.since,
            before,
            limit,
        })
        .await?;
    // The last page carries no cursor, even when it is exactly full.
    let next_before = releases
        .last()
        .filter(|_| truncated)
        .and_then(ReleaseCursor::after)
        .map(|cursor| cursor.to_string());
    Ok(ReleaseList {
        releases,
        truncated,
        next_before,
    })
}

/// Publish a release. Posting the same project, version and environment
/// again updates it in place: title, notes and url are replaced,
/// `released_at` moves only later, and a deploy already stamped is kept.
#[utoipa::path(
    post,
    path = "/api/v1/releases",
    tag = TAG,
    security(("bearer" = [])),
    request_body = NewRelease,
    responses(
        (status = 201, description = "Created", body = Release),
        (status = 200, description = "Already recorded (published, or stamped as deployed); updated", body = Release),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `release` scope", body = ErrorBody),
        (status = 413, description = "`notes` is over 64 KiB", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path; an unknown project is named", body = ErrorBody),
    ),
)]
pub async fn publish_release(
    State(state): State<AppState>,
    auth: Authorized<scope::Release>,
    ApiJson(new): ApiJson<NewRelease>,
) -> Result<impl IntoResponse, ApiError> {
    let (release, written) = publish(&state, new, auth.actor).await?;
    Ok((status(written), Json(release)))
}

/// `/api/v1/releases/{project}/{version}/deployed` path parameters.
#[derive(Debug, Deserialize)]
pub struct ReleasePath {
    project: String,
    version: String,
}

/// Record that a version was deployed, at `deployed_at` or now. The body
/// may be empty. A version not yet published is recorded as a stub that
/// its publish fills in; until then it is on the timeline but not in the
/// release log.
#[utoipa::path(
    post,
    path = "/api/v1/releases/{project}/{version}/deployed",
    tag = TAG,
    security(("bearer" = [])),
    params(
        ("project" = String, Path, description = "A `[[project]]` id"),
        ("version" = String, Path, description = "The version, as published"),
    ),
    request_body(content = Option<DeployStamp>, description = "Optional; empty means production, now"),
    responses(
        (status = 201, description = "Not yet published: recorded as a stub", body = Release),
        (status = 200, description = "Stamped on the recorded release", body = Release),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `release` scope", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path; an unknown project is named", body = ErrorBody),
    ),
)]
pub async fn stamp_deploy(
    State(state): State<AppState>,
    auth: Authorized<scope::Release>,
    path: Result<Path<ReleasePath>, PathRejection>,
    OptionalJson(stamp): OptionalJson<DeployStamp>,
) -> Result<impl IntoResponse, ApiError> {
    let Path(ReleasePath { project, version }) = path
        .map_err(|rejection| ApiError::invalid(vec![FieldError::new("", rejection.body_text())]))?;
    let (release, written) = deployed(&state, project, version, stamp, auth.actor).await?;
    Ok((status(written), Json(release)))
}

/// Publishes `new` as `actor`: checks, stores, logs and emits
/// [`Event::ReleasePublished`].
///
/// # Errors
///
/// 413 for notes over [`tocsin_core::release::MAX_NOTES_BYTES`]; 422 for other invalid fields,
/// a `released_at` before the recorded one, or a time too far in the
/// future; 500 if the store fails.
pub async fn publish(
    state: &AppState,
    new: NewRelease,
    actor: Actor,
) -> Result<(Arc<Release>, Written), ApiError> {
    new.validate(&|id| state.knows_project(id))
        .map_err(|rejected| match rejected {
            Rejected::NotesTooLarge { .. } => ApiError::too_large(rejected.to_string()),
            Rejected::Invalid(errors) => ApiError::invalid(errors),
        })?;
    let (release, written) = state
        .store
        .publish_release(new, actor.clone(), now())
        .await?;
    tracing::info!(
        release = release.id,
        project = %release.project,
        version = %release.version,
        environment = %release.environment,
        ?written,
        %actor,
        "release published"
    );
    let release = Arc::new(release);
    state.events.emit(Event::ReleasePublished {
        public: state.project_is_public(&release.project),
        release: Arc::clone(&release),
    });
    Ok((release, written))
}

/// Stamps a deploy of `project` `version` as `actor`: checks, stores,
/// logs and emits [`Event::ReleaseDeployed`].
///
/// # Errors
///
/// 422 for invalid fields or a time too far in the future; 500 if the
/// store fails.
pub async fn deployed(
    state: &AppState,
    project: String,
    version: String,
    stamp: DeployStamp,
    actor: Actor,
) -> Result<(Arc<Release>, Written), ApiError> {
    stamp
        .validate(&project, &version, &|id| state.knows_project(id))
        .map_err(ApiError::invalid)?;
    let (release, written) = state
        .store
        .stamp_deploy(project, version, stamp, actor.clone(), now())
        .await?;
    tracing::info!(
        release = release.id,
        project = %release.project,
        version = %release.version,
        environment = %release.environment,
        ?written,
        %actor,
        "deploy stamped"
    );
    let release = Arc::new(release);
    state.events.emit(Event::ReleaseDeployed {
        public: state.project_is_public(&release.project),
        release: Arc::clone(&release),
    });
    Ok((release, written))
}

fn status(written: Written) -> StatusCode {
    match written {
        Written::Created => StatusCode::CREATED,
        Written::Updated => StatusCode::OK,
    }
}
