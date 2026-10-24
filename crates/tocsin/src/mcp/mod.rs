// SPDX-License-Identifier: AGPL-3.0-only

//! The MCP server: the HTTP API's reads and writes as tools an agent can
//! call without learning the API.
//!
//! One set of tools runs two ways. In `tocsin serve` it is mounted at
//! `/mcp` and each tool calls the function the matching REST route calls,
//! with the scope that route needs. In `tocsin mcp` it speaks over stdin
//! and stdout and each tool calls the matching route on a remote node,
//! which checks the scope itself; only there can `validate_config` read a
//! local configuration folder.

pub mod http;
pub mod stdio;

use std::path::PathBuf;

use reqwest::Method;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Extensions, Implementation, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::{MaybeSendFuture, RequestContext};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tocsin_core::incident::{IncidentStatus, NewIncident, NewUpdate, Timestamp};
use tocsin_core::release::{DeployStamp, NewRelease};

use crate::client::Api;
use crate::http::auth::{Caller, scope};
use crate::http::error::ApiError;
use crate::http::history::{OutageParams, ResultsParams};
use crate::http::timeline::{DAY_MS, Window};
use crate::http::{AppState, history, incidents, now, releases, status, timeline};
use crate::output::Failure;
use tocsin_store::Order;

/// The configuration JSON Schema, as `tocsin config schema` prints it.
pub const CONFIG_SCHEMA_URI: &str = "tocsin://schema/config";
/// The OpenAPI document, as `/api/v1/openapi.json` serves it.
pub const OPENAPI_URI: &str = "tocsin://schema/openapi";

/// The tool that reads a local folder, offered only by `tocsin mcp`.
const VALIDATE_CONFIG: &str = "validate_config";

/// How far back `explain_check` looks for a check's latest result: a day
/// inside the furthest any read may reach.
const EXPLAIN_WINDOW_MS: i64 = 365 * DAY_MS;

/// The tools, over one node's state or a remote node's API.
#[derive(Clone)]
pub struct Server {
    backend: Backend,
    tool_router: ToolRouter<Server>,
}

#[derive(Clone)]
enum Backend {
    /// In `tocsin serve`: the node's own state. Who is calling comes with
    /// each request.
    Local(AppState),
    /// In `tocsin mcp`: a node's HTTP API, called with the configured
    /// token.
    Remote(Api),
}

impl Server {
    /// The tools `/mcp` serves from `state`.
    pub fn local(state: AppState) -> Self {
        let mut tool_router = Self::tool_router();
        tool_router.remove_route(VALIDATE_CONFIG);
        Server {
            backend: Backend::Local(state),
            tool_router,
        }
    }

    /// The tools `tocsin mcp` serves, calling `api`.
    pub fn remote(api: Api) -> Self {
        Server {
            backend: Backend::Remote(api),
            tool_router: Self::tool_router(),
        }
    }
}

/// `limit` or `default`, held to 1 to `max` as the REST route's page is.
/// An agent asking for too many gets the most a page holds, not an error.
fn clamp(limit: Option<u32>, default: u32, max: u32) -> u32 {
    limit.unwrap_or(default).clamp(1, max)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OutagesInput {
    /// The start of the window: outages still open at or after it. At
    /// most 366 days ago; default 30 days ago. Keep it the same while
    /// paging.
    since: Option<Timestamp>,
    /// The previous page's `next_before`.
    before: Option<i64>,
    /// Outages per page; default 100, at most 500 (more is cut to 500).
    #[schemars(range(min = 1, max = 500))]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HistoryInput {
    /// A `[[check]]` or `[[heartbeat]]` id.
    check: String,
    /// The start of the window: at most 366 days ago; default one day ago.
    /// Keep it the same while paging.
    since: Option<Timestamp>,
    /// The previous page's `next_before`.
    before: Option<i64>,
    /// Results per page; default 100, at most 500 (more is cut to 500).
    #[schemars(range(min = 1, max = 500))]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExplainInput {
    /// A `[[check]]` or `[[heartbeat]]` id.
    check: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TimelineInput {
    /// Only events touching this `[[component]]`.
    component: Option<String>,
    /// The start of the window: at most 366 days ago; default 30 days ago.
    /// Keep it the same while paging.
    since: Option<Timestamp>,
    /// The previous page's `next_after`.
    after: Option<String>,
    /// Events per page; default 100, at most 500 (more is cut to 500).
    #[schemars(range(min = 1, max = 500))]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IncidentsInput {
    /// The previous page's `next_before`.
    before: Option<i64>,
    /// Incidents per page; default 20, at most 100 (more is cut to 100).
    #[schemars(range(min = 1, max = 100))]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReleasesInput {
    /// Only this `[[project]]`'s releases.
    project: Option<String>,
    /// Only releases published at or after this instant.
    since: Option<Timestamp>,
    /// The previous page's `next_before`.
    before: Option<String>,
    /// Releases per page; default 50, at most 200 (more is cut to 200).
    #[schemars(range(min = 1, max = 200))]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateInput {
    /// The incident's id.
    id: i64,
    /// The status the incident moves to; it must suit the incident's kind.
    status: IncidentStatus,
    /// Markdown. Not empty.
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeployInput {
    /// A configured `[[project]]` id.
    project: String,
    /// The published version.
    version: String,
    /// Defaults to `production`.
    #[schemars(pattern("^[A-Za-z0-9][A-Za-z0-9._+-]{0,31}$"))]
    environment: Option<String>,
    /// Defaults to now; at most an hour past the server's clock.
    deployed_at: Option<Timestamp>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ValidateInput {
    /// A folder of `*.toml` configuration files on this machine.
    folder: String,
}

#[tool_router(router = tool_router)]
impl Server {
    /// The status now: overall, every component, and the open incidents.
    /// Start here to find what is down.
    #[tool(name = "get_status")]
    async fn get_status(&self, extensions: Extensions) -> CallToolResult {
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    caller(&extensions)?;
                    status::document(state).await
                }
                .await,
            ),
            Backend::Remote(api) => {
                remote(get(api, &["api", "v1", "status.json"], &[], api.has_token()).await)
            }
        }
    }

    /// Outages open at any time in a window, newest first: what went down
    /// and since when. Outages of checks on no component need the
    /// `status:read` scope.
    #[tool(name = "list_outages")]
    async fn list_outages(
        &self,
        extensions: Extensions,
        Parameters(input): Parameters<OutagesInput>,
    ) -> CallToolResult {
        let limit = clamp(input.limit, timeline::DEFAULT_LIMIT, timeline::MAX_LIMIT);
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    let viewer = caller(&extensions)?.viewer();
                    history::outages(
                        state,
                        viewer,
                        OutageParams {
                            since: input.since,
                            before: input.before,
                            limit: Some(limit),
                        },
                    )
                    .await
                }
                .await,
            ),
            Backend::Remote(api) => {
                let query = page_query(input.since, input.before, limit);
                remote(get(api, &["api", "v1", "outages"], &query, api.has_token()).await)
            }
        }
    }

    /// One check's or heartbeat's raw results, newest first, with errors
    /// and assertion detail. Needs the `status:read` scope.
    #[tool(name = "get_check_history")]
    async fn get_check_history(
        &self,
        extensions: Extensions,
        Parameters(input): Parameters<HistoryInput>,
    ) -> CallToolResult {
        let limit = clamp(input.limit, timeline::DEFAULT_LIMIT, timeline::MAX_LIMIT);
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    caller(&extensions)?.grant::<scope::StatusRead>()?;
                    history::results(
                        state,
                        input.check,
                        ResultsParams {
                            since: input.since,
                            before: input.before,
                            limit: Some(limit),
                        },
                    )
                    .await
                }
                .await,
            ),
            Backend::Remote(api) => {
                let query = page_query(input.since, input.before, limit);
                let segments = ["api", "v1", "checks", input.check.as_str(), "results"];
                remote(get(api, &segments, &query, true).await)
            }
        }
    }

    /// Why a check is in the state it is: its latest result, with the
    /// error, status code and how each assertion fared. Needs the
    /// `status:read` scope.
    #[tool(name = "explain_check")]
    async fn explain_check(
        &self,
        extensions: Extensions,
        Parameters(input): Parameters<ExplainInput>,
    ) -> CallToolResult {
        let since = Timestamp::from_millis(now().as_millis() - EXPLAIN_WINDOW_MS);
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    caller(&extensions)?.grant::<scope::StatusRead>()?;
                    let query = ResultsParams {
                        since: Some(since),
                        before: None,
                        limit: Some(1),
                    };
                    let results = history::results(state, input.check, query).await?;
                    Ok(json!({
                        "check_id": results.check_id,
                        "latest": results.results.first(),
                    }))
                }
                .await,
            ),
            Backend::Remote(api) => {
                let query = page_query(Some(since), None, 1);
                let segments = ["api", "v1", "checks", input.check.as_str(), "results"];
                remote(get(api, &segments, &query, true).await.map(|results| {
                    json!({
                        "check_id": results["check_id"],
                        "latest": results["results"].get(0),
                    })
                }))
            }
        }
    }

    /// Outages, incidents and deploys, oldest first: what changed just
    /// before something broke. Non-public projects' deploys, and outages of
    /// checks on no component, need the `status:read` scope.
    #[tool(name = "get_timeline")]
    async fn get_timeline(
        &self,
        extensions: Extensions,
        Parameters(input): Parameters<TimelineInput>,
    ) -> CallToolResult {
        let limit = clamp(input.limit, timeline::DEFAULT_LIMIT, timeline::MAX_LIMIT);
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    let viewer = caller(&extensions)?.viewer();
                    let window = Window {
                        component: input.component.as_deref(),
                        since: input.since.unwrap_or_else(timeline::default_since),
                        after: input.after.as_deref(),
                        order: Order::OldestFirst,
                        limit,
                    };
                    timeline::read(state, viewer, window).await
                }
                .await,
            ),
            Backend::Remote(api) => {
                let mut query = vec![("limit", limit.to_string())];
                query.extend(input.component.map(|component| ("component", component)));
                query.extend(input.since.map(|since| ("since", since.to_string())));
                query.extend(input.after.map(|after| ("after", after)));
                remote(get(api, &["api", "v1", "timeline"], &query, api.has_token()).await)
            }
        }
    }

    /// Incidents and maintenance, newest first, with every update.
    #[tool(name = "list_incidents")]
    async fn list_incidents(
        &self,
        extensions: Extensions,
        Parameters(input): Parameters<IncidentsInput>,
    ) -> CallToolResult {
        let limit = clamp(input.limit, incidents::DEFAULT_LIMIT, incidents::MAX_LIMIT);
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    caller(&extensions)?;
                    incidents::list(
                        state,
                        incidents::ListQuery {
                            before: input.before,
                            limit: Some(limit),
                        },
                    )
                    .await
                }
                .await,
            ),
            Backend::Remote(api) => {
                let mut query = vec![("limit", limit.to_string())];
                query.extend(input.before.map(|before| ("before", before.to_string())));
                remote(get(api, &["api", "v1", "incidents"], &query, api.has_token()).await)
            }
        }
    }

    /// Published releases, newest first. Non-public projects' releases
    /// need the `status:read` scope.
    #[tool(name = "list_releases")]
    async fn list_releases(
        &self,
        extensions: Extensions,
        Parameters(input): Parameters<ReleasesInput>,
    ) -> CallToolResult {
        let limit = clamp(input.limit, releases::DEFAULT_LIMIT, releases::MAX_LIMIT);
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    let viewer = caller(&extensions)?.viewer();
                    let query = releases::ListQuery {
                        project: input.project,
                        since: input.since,
                        before: input.before,
                        limit: Some(limit),
                    };
                    releases::list(state, viewer, query).await
                }
                .await,
            ),
            Backend::Remote(api) => {
                let mut query = vec![("limit", limit.to_string())];
                query.extend(input.project.map(|project| ("project", project)));
                query.extend(input.since.map(|since| ("since", since.to_string())));
                query.extend(input.before.map(|before| ("before", before)));
                remote(get(api, &["api", "v1", "releases"], &query, api.has_token()).await)
            }
        }
    }

    /// Open an incident, or announce maintenance with `kind` set to
    /// `maintenance`. Needs the `incident` scope.
    #[tool(name = "create_incident")]
    async fn create_incident(
        &self,
        extensions: Extensions,
        Parameters(new): Parameters<NewIncident>,
    ) -> CallToolResult {
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    let actor = caller(&extensions)?.grant::<scope::Incident>()?;
                    incidents::open(state, new, actor).await
                }
                .await,
            ),
            Backend::Remote(api) => remote(post(api, &["api", "v1", "incidents"], &new).await),
        }
    }

    /// Post an update to an incident, moving it to a new status. Needs the
    /// `incident` scope.
    #[tool(name = "update_incident")]
    async fn update_incident(
        &self,
        extensions: Extensions,
        Parameters(input): Parameters<UpdateInput>,
    ) -> CallToolResult {
        let update = NewUpdate {
            status: input.status,
            message: input.message,
        };
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    let actor = caller(&extensions)?.grant::<scope::Incident>()?;
                    incidents::append(state, input.id, update, actor).await
                }
                .await,
            ),
            Backend::Remote(api) => {
                let id = input.id.to_string();
                let segments = ["api", "v1", "incidents", id.as_str(), "updates"];
                remote(post(api, &segments, &update).await)
            }
        }
    }

    /// Publish a release, or update the one with the same project, version
    /// and environment. Needs the `release` scope.
    #[tool(name = "publish_release")]
    async fn publish_release(
        &self,
        extensions: Extensions,
        Parameters(new): Parameters<NewRelease>,
    ) -> CallToolResult {
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    let actor = caller(&extensions)?.grant::<scope::Release>()?;
                    releases::publish(state, new, actor)
                        .await
                        .map(|(release, _)| release)
                }
                .await,
            ),
            Backend::Remote(api) => remote(post(api, &["api", "v1", "releases"], &new).await),
        }
    }

    /// Record that a published version was deployed. Needs the `release`
    /// scope.
    #[tool(name = "mark_deployed")]
    async fn mark_deployed(
        &self,
        extensions: Extensions,
        Parameters(input): Parameters<DeployInput>,
    ) -> CallToolResult {
        let stamp = DeployStamp {
            environment: input.environment,
            deployed_at: input.deployed_at,
        };
        match &self.backend {
            Backend::Local(state) => local(
                async {
                    let actor = caller(&extensions)?.grant::<scope::Release>()?;
                    releases::deployed(state, input.project, input.version, stamp, actor)
                        .await
                        .map(|(release, _)| release)
                }
                .await,
            ),
            Backend::Remote(api) => {
                let segments = [
                    "api",
                    "v1",
                    "releases",
                    input.project.as_str(),
                    input.version.as_str(),
                    "deployed",
                ];
                remote(post(api, &segments, &stamp).await)
            }
        }
    }

    /// Validate a configuration folder on this machine, as `tocsin config
    /// check` does: `ok` and what it holds, or every problem found.
    #[tool(name = "validate_config")]
    async fn validate_config(
        &self,
        Parameters(input): Parameters<ValidateInput>,
    ) -> CallToolResult {
        let folder = PathBuf::from(input.folder);
        let checked = tokio::task::spawn_blocking(move || crate::config::check(&folder)).await;
        match checked {
            Ok(Ok(report)) => CallToolResult::structured(report.json),
            Ok(Err(failure)) => CallToolResult::structured(json!({
                "ok": false,
                "message": failure.message,
                "errors": failure.errors,
            })),
            Err(error) => CallToolResult::structured_error(json!({ "message": error.to_string() })),
        }
    }
}

#[allow(clippy::unused_async_trait_impl)] // the methods the macro writes
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("tocsin", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "tocsin's status, history, incidents and releases. To find what is down and \
             why: get_status, then list_outages and explain_check, then get_timeline for \
             what changed just before. Writes need a token with the matching scope.",
        )
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(CONFIG_SCHEMA_URI, "config-schema")
                .with_description("JSON Schema for the TOML configuration files")
                .with_mime_type("application/schema+json"),
            Resource::new(OPENAPI_URI, "openapi")
                .with_description("OpenAPI document for the HTTP API")
                .with_mime_type("application/json"),
        ])))
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, McpError>> + MaybeSendFuture + '_ {
        std::future::ready(read_resource(&request))
    }
}

/// One of the two resources, the documents `tocsin config schema` and
/// `tocsin openapi` print.
fn read_resource(request: &ReadResourceRequestParams) -> Result<ReadResourceResponse, McpError> {
    let (text, mime) = match request.uri.as_str() {
        CONFIG_SCHEMA_URI => (crate::config::schema().text, "application/schema+json"),
        OPENAPI_URI => (crate::http::openapi::pretty(), "application/json"),
        other => {
            return Err(McpError::resource_not_found(
                format!("no resource {other}"),
                None,
            ));
        }
    };
    Ok(ReadResourceResult::new(vec![
        ResourceContents::text(text, request.uri.clone()).with_mime_type(mime),
    ])
    .into())
}

/// Who sent this `/mcp` request, as the authentication in front of it
/// recorded.
fn caller(extensions: &Extensions) -> Result<Caller, ApiError> {
    extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<Caller>())
        .cloned()
        .ok_or_else(|| ApiError::unauthorized("no token"))
}

/// A local answer as the tool's result: the JSON the REST route would
/// send, or its error as `{status, message, fields}`.
fn local<T: Serialize>(result: Result<T, ApiError>) -> CallToolResult {
    match result
        .and_then(|value| serde_json::to_value(value).map_err(|error| ApiError::internal(&error)))
    {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => CallToolResult::structured_error(json!({
            "status": error.status().as_u16(),
            "message": error.message(),
            "fields": error.fields(),
        })),
    }
}

/// A remote answer as the tool's result, errors shaped as [`local`]'s.
fn remote(result: Result<Value, Failure>) -> CallToolResult {
    match result {
        Ok(value) => CallToolResult::structured(value),
        Err(failure) => CallToolResult::structured_error(json!({
            "status": failure.status,
            "message": failure.message,
            "fields": failure
                .errors
                .iter()
                .map(|error| json!({
                    "path": error.path.clone().unwrap_or_default(),
                    "message": error.message,
                }))
                .collect::<Vec<_>>(),
        })),
    }
}

/// `limit`, `since` and `before` as a newest-first page's query.
fn page_query(
    since: Option<Timestamp>,
    before: Option<i64>,
    limit: u32,
) -> Vec<(&'static str, String)> {
    let mut query = vec![("limit", limit.to_string())];
    query.extend(since.map(|since| ("since", since.to_string())));
    query.extend(before.map(|before| ("before", before.to_string())));
    query
}

async fn get(
    api: &Api,
    segments: &[&str],
    query: &[(&'static str, String)],
    authenticated: bool,
) -> Result<Value, Failure> {
    let query: Vec<(&str, &str)> = query
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    api.send(
        &Method::GET,
        api.url(segments),
        &query,
        None::<&()>,
        authenticated,
    )
    .await
}

async fn post<B: Serialize>(api: &Api, segments: &[&str], body: &B) -> Result<Value, Failure> {
    api.send(&Method::POST, api.url(segments), &[], Some(body), true)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_is_held_to_the_page_bounds() {
        assert_eq!(clamp(None, 100, 500), 100);
        assert_eq!(clamp(Some(0), 100, 500), 1);
        assert_eq!(clamp(Some(7), 100, 500), 7);
        assert_eq!(clamp(Some(10_000), 100, 500), 500);
    }

    #[test]
    fn every_list_tool_advertises_its_page_bound() {
        for (name, max) in [
            ("list_outages", timeline::MAX_LIMIT),
            ("get_check_history", timeline::MAX_LIMIT),
            ("get_timeline", timeline::MAX_LIMIT),
            ("list_incidents", incidents::MAX_LIMIT),
            ("list_releases", releases::MAX_LIMIT),
        ] {
            let tool = Server::tool_router()
                .get(name)
                .unwrap_or_else(|| panic!("{name} is a tool"))
                .clone();
            let limit = &tool.input_schema["properties"]["limit"];
            assert_eq!(limit["maximum"], max, "{name}: {limit}");
            assert_eq!(limit["minimum"], 1, "{name}: {limit}");
        }
    }
}
