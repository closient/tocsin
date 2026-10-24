// SPDX-License-Identifier: AGPL-3.0-only

//! Connecting Slack channels by signing in to Slack, with `[slack]` on.
//! [`crate::slack`] holds the sign-in itself; this is its pages.
//!
//! - `/slack/callback` is where Slack sends people back. It answers once
//!   per sign-in, only to the browser that started it, and always clears
//!   the sign-in's cookie.
//! - `/admin/slack` (with the admin pages on) connects channels by name,
//!   for `[notifier.slack] connection`, shows which are dead, and removes
//!   any connection.
//! - `/subscribe/slack` (with `[subscribers]` on) is "Add to Slack" on the
//!   sign-up form, and the pages the links in every subscriber message
//!   open: choosing what the channel hears, and removing it. Like the
//!   email pages, a `GET` never changes anything, so Slack fetching a link
//!   to preview it is harmless.
//!
//! No page here shows a webhook URL: Slack hands it over in the callback,
//! it goes straight to the store, and only the postman and notifiers read
//! it back.

use std::fmt::Write as _;
use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tocsin_core::config::Id;
use tocsin_core::incident::Timestamp;
use tocsin_store::{EVERY, NewSlackConnection, Preferences, SlackConnection, SlackPurpose};

use super::form::Fields;
use super::identity::{Admin, AdminForm};
use super::page::{HtmlError, Layout, render, when};
use super::subscribe::{
    Action, ManagePage, SignupForm, Stop, choices, enabled, explain, frame, notice, path_token,
    private, refusal, same_site, signup_response, what,
};
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::slack::{Granted, Purpose, Refused, SlackApp, Started, clear_cookie};
use crate::subscribers::{paths, token};

/// Where Slack sends people back.
pub fn router() -> Router<AppState> {
    Router::new().route(crate::slack::CALLBACK, get(callback))
}

/// "Add to Slack" and the subscriber channel's pages. Merged only when
/// `[subscribers]` is on too.
pub fn subscribe_router() -> Router<AppState> {
    Router::new()
        .route(paths::SLACK, post(subscribe))
        .route(
            "/subscribe/slack/manage/{token}",
            get(manage_page).post(save),
        )
        .route(
            "/subscribe/slack/unsubscribe/{token}",
            get(remove_page).post(remove),
        )
}

/// The operator's page. Merged only when the admin pages are on too.
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/slack", get(admin_page))
        .route(
            "/admin/slack/connect",
            get(|| async { Redirect::to("/admin/slack") }).post(admin_connect),
        )
        .route(
            "/admin/slack/{id}/remove",
            get(|| async { Redirect::to("/admin/slack") }).post(admin_remove),
        )
}

/// The running app. The routes are merged only when it is on.
fn app(state: &AppState) -> Result<Arc<SlackApp>, HtmlError> {
    state.slack.clone().ok_or_else(HtmlError::not_found)
}

/// Sends the browser to Slack's consent screen, with the sign-in's cookie.
fn to_slack(started: &Started) -> Result<Response, HtmlError> {
    let cookie =
        HeaderValue::from_str(&started.cookie).map_err(|error| HtmlError::internal(&error))?;
    let mut response = Redirect::to(&started.location).into_response();
    response.headers_mut().append(header::SET_COOKIE, cookie);
    Ok(response)
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<Callback>, QueryRejection>,
) -> Response {
    let mut response = match finish(&state, &headers, query).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    if let Ok(cookie) = HeaderValue::from_str(&clear_cookie()) {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
}

async fn finish(
    state: &AppState,
    headers: &HeaderMap,
    query: Result<Query<Callback>, QueryRejection>,
) -> Result<Response, HtmlError> {
    let app = app(state)?;
    let Ok(Query(query)) = query else {
        return Err(HtmlError::new(
            StatusCode::BAD_REQUEST,
            "This is not a Slack sign-in this site started.",
        ));
    };
    let purpose = match app.finish(query.state.as_deref().unwrap_or_default(), headers) {
        Ok(purpose) => purpose,
        Err(Refused::Unknown) => {
            tracing::warn!("Slack callback refused: its state is unknown, used or expired");
            return Err(HtmlError::new(
                StatusCode::BAD_REQUEST,
                "This Slack sign-in has expired or was already used. Start again from the page you came from.",
            ));
        }
        Err(Refused::WrongBrowser) => {
            tracing::warn!(
                "Slack callback refused: it came back to a browser that did not start it"
            );
            return Err(HtmlError::new(
                StatusCode::BAD_REQUEST,
                "This Slack sign-in was started in another browser. Start again here.",
            ));
        }
    };
    let back = match &purpose {
        Purpose::Operator { .. } => "/admin/slack",
        Purpose::Subscriber { .. } => paths::SUBSCRIBE,
    };
    if let Some(error) = query.error {
        let error: String = error
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        tracing::info!(%error, "Slack sign-in ended without a channel");
        return Ok(notice(
            state,
            StatusCode::OK,
            "Slack was not connected",
            vec![if error == "access_denied" {
                "You cancelled on Slack's page, so nothing was added.".to_owned()
            } else {
                format!("Slack ended the sign-in ({error}), so nothing was added.")
            }],
            None,
            Some((back.to_owned(), "Go back")),
        ));
    }
    let code = query.code.unwrap_or_default();
    let granted = match app.exchange(&code).await {
        Ok(granted) => granted,
        Err(error) => {
            tracing::error!(%error, "Slack sign-in failed");
            return Ok(notice(
                state,
                StatusCode::BAD_GATEWAY,
                "Slack was not connected",
                vec![format!("{error}."), "Try again in a moment.".to_owned()],
                None,
                Some((back.to_owned(), "Go back")),
            ));
        }
    };
    match purpose {
        Purpose::Operator { name, admin } => connect_operator(state, granted, name, admin).await,
        Purpose::Subscriber { preferences } => {
            connect_subscriber(state, granted, preferences).await
        }
    }
}

async fn connect_operator(
    state: &AppState,
    granted: Granted,
    name: String,
    admin: String,
) -> Result<Response, HtmlError> {
    let connection = state
        .store
        .connect_slack(NewSlackConnection {
            purpose: SlackPurpose::Operator,
            name: Some(name.clone()),
            webhook_url: granted.webhook_url,
            channel: granted.channel,
            channel_id: granted.channel_id,
            team: granted.team,
            team_id: granted.team_id,
            configuration_url: granted.configuration_url,
            created_by: Some(admin.clone()),
            manage_secret: None,
            preferences: Preferences::default(),
            now_ms: tocsin_store::now_ms(),
        })
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    tracing::info!(
        connection = %name,
        channel = %connection.channel,
        team = %connection.team,
        actor = %admin,
        "Slack channel connected for alerts"
    );
    Ok(Redirect::to("/admin/slack?done=connected").into_response())
}

async fn connect_subscriber(
    state: &AppState,
    granted: Granted,
    preferences: Preferences,
) -> Result<Response, HtmlError> {
    let subscriptions = enabled(state)?;
    let secret = token::random().map_err(|error| HtmlError::internal(&error))?;
    let webhook_url = granted.webhook_url.expose().to_owned();
    let connection = state
        .store
        .connect_slack(NewSlackConnection {
            purpose: SlackPurpose::Subscriber,
            name: None,
            webhook_url: granted.webhook_url,
            channel: granted.channel,
            channel_id: granted.channel_id,
            team: granted.team,
            team_id: granted.team_id,
            configuration_url: granted.configuration_url,
            created_by: None,
            manage_secret: Some(secret.clone()),
            preferences: preferences.clone(),
            now_ms: tocsin_store::now_ms(),
        })
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    tracing::info!(
        connection = connection.id,
        channel = %connection.channel,
        team = %connection.team,
        "subscriber added a Slack channel"
    );
    let manage = token::manage(connection.id, &secret);
    subscriptions.welcome_slack(
        state,
        connection.id,
        webhook_url,
        manage.clone(),
        &preferences,
    );
    Ok(Redirect::to(&format!("{}?done=connected", paths::slack_manage(&manage))).into_response())
}

/// "Add to Slack" on the sign-up form: the same choices as email, then
/// Slack's consent screen.
async fn subscribe(
    State(state): State<AppState>,
    parts: Parts,
    body: Bytes,
) -> Result<Response, HtmlError> {
    let subscriptions = enabled(&state)?;
    let app = app(&state)?;
    same_site(&parts.headers, &subscriptions)?;
    let fields = Fields::parse(&body);
    let components = fields.all("components");
    let projects = fields.all("projects");
    let preferences = match crate::subscribers::preferences(&state, &components, &projects) {
        Ok(preferences) => preferences,
        Err(error) => {
            let mut form = SignupForm::full(&state, &subscriptions, &components, &projects);
            form.email = fields.text("email");
            form.errors = explain(error.fields());
            return Ok(signup_response(
                &state,
                form,
                StatusCode::UNPROCESSABLE_ENTITY,
            ));
        }
    };
    let ip = subscriptions.client_ip(&parts)?;
    if let Err(wait) = app.admit(ip) {
        tracing::warn!(%ip, "Slack sign-in refused: too many from this address");
        return Ok(refusal(ApiError::too_many_requests(
            "Too many sign-ups from this address. Try again later.",
            wait,
        )));
    }
    let started = app
        .begin(Purpose::Subscriber { preferences })
        .map_err(|error| HtmlError::new(StatusCode::SERVICE_UNAVAILABLE, error))?;
    to_slack(&started)
}

/// The subscriber channel a manage token names, if its secret matches.
async fn holder(state: &AppState, token: &str) -> Result<Option<SlackConnection>, HtmlError> {
    let Some((id, secret)) = token::parse_manage(token) else {
        return Ok(None);
    };
    let connection = state
        .store
        .slack_connection(id)
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    Ok(connection.filter(|connection| {
        connection.purpose == SlackPurpose::Subscriber
            && connection
                .manage_secret
                .as_deref()
                .is_some_and(|known| token::same_secret(secret, known))
    }))
}

fn gone(state: &AppState) -> Response {
    notice(
        state,
        StatusCode::NOT_FOUND,
        "No channel here",
        vec!["This channel was removed, or the link is not one we sent.".to_owned()],
        None,
        Some((paths::SUBSCRIBE.to_owned(), "Subscribe")),
    )
}

/// `#channel in Team`.
fn place(connection: &SlackConnection) -> String {
    format!("{} in {}", connection.channel, connection.team)
}

#[derive(Deserialize)]
struct DoneQuery {
    done: Option<String>,
}

fn done_query(query: Result<Query<DoneQuery>, QueryRejection>) -> Option<String> {
    query.ok().and_then(|Query(query)| query.done)
}

async fn manage_page(
    State(state): State<AppState>,
    token: Result<Path<String>, PathRejection>,
    query: Result<Query<DoneQuery>, QueryRejection>,
) -> Result<Response, HtmlError> {
    enabled(&state)?;
    let token = path_token(token)?;
    let Some(connection) = holder(&state, &token).await? else {
        return Ok(gone(&state));
    };
    let done = match done_query(query).as_deref() {
        Some("connected") => Some(
            "Added. The channel will hear about what is checked below; a first message is on its way.",
        ),
        Some("saved") => Some("Saved."),
        _ => None,
    };
    let preferences = connection.preferences.clone();
    Ok(manage_response(
        &state,
        &token,
        &connection,
        &preferences,
        done,
        Vec::new(),
    ))
}

fn manage_response(
    state: &AppState,
    token: &str,
    connection: &SlackConnection,
    preferences: &Preferences,
    done: Option<&'static str>,
    errors: Vec<String>,
) -> Response {
    let site = &state.config.page.title;
    let (layout, chrome) = frame(state, "Your Slack channel", paths::SUBSCRIBE);
    let (components, projects) = choices(state, preferences);
    let mut lede = format!("Slack messages from {site} to {}.", place(connection));
    if let Some(reason) = &connection.dead_reason {
        let _ = write!(
            lede,
            " Slack no longer accepts them ({reason}), so nothing is posted; add the channel again from the subscribe page."
        );
    }
    let status = if errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    private(
        status,
        &ManagePage {
            layout,
            chrome,
            lede,
            done,
            path: paths::slack_manage(token),
            stop: Stop {
                heading: "Remove this channel",
                line: format!(
                    "Stop all messages from {site} to {}. Anyone in the channel can add it again.",
                    connection.channel
                ),
                path: paths::slack_unsubscribe(token),
                label: "Remove",
            },
            components,
            projects,
            errors,
        },
    )
}

async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    token: Result<Path<String>, PathRejection>,
    body: Bytes,
) -> Result<Response, HtmlError> {
    let subscriptions = enabled(&state)?;
    same_site(&headers, &subscriptions)?;
    let token = path_token(token)?;
    let Some(connection) = holder(&state, &token).await? else {
        return Ok(gone(&state));
    };
    let fields = Fields::parse(&body);
    let components = fields.all("components");
    let projects = fields.all("projects");
    match crate::subscribers::preferences(&state, &components, &projects) {
        Ok(preferences) => {
            let saved = state
                .store
                .set_slack_preferences(connection.id, preferences)
                .await
                .map_err(|error| HtmlError::internal(&error))?;
            if !saved {
                return Ok(gone(&state));
            }
            Ok(
                Redirect::to(&format!("{}?done=saved", paths::slack_manage(&token)))
                    .into_response(),
            )
        }
        Err(error) => {
            let asked = Preferences {
                components,
                projects,
            };
            Ok(manage_response(
                &state,
                &token,
                &connection,
                &asked,
                None,
                explain(error.fields()),
            ))
        }
    }
}

async fn remove_page(
    State(state): State<AppState>,
    token: Result<Path<String>, PathRejection>,
) -> Result<Response, HtmlError> {
    enabled(&state)?;
    let token = path_token(token)?;
    let Some(connection) = holder(&state, &token).await? else {
        return Ok(gone(&state));
    };
    Ok(notice(
        &state,
        StatusCode::OK,
        "Remove this channel",
        vec![format!(
            "Stop all messages from {} to {}?",
            state.config.page.title,
            place(&connection)
        )],
        Some(Action {
            path: paths::slack_unsubscribe(&token),
            label: "Remove",
        }),
        Some((
            paths::slack_manage(&token),
            "Choose what it hears about instead",
        )),
    ))
}

async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    token: Result<Path<String>, PathRejection>,
) -> Result<Response, HtmlError> {
    let subscriptions = enabled(&state)?;
    same_site(&headers, &subscriptions)?;
    let token = path_token(token)?;
    let Some(connection) = holder(&state, &token).await? else {
        return Ok(gone(&state));
    };
    if !state
        .store
        .remove_slack(connection.id)
        .await
        .map_err(|error| HtmlError::internal(&error))?
    {
        return Ok(gone(&state));
    }
    tracing::info!(
        connection = connection.id,
        "subscriber removed a Slack channel"
    );
    let mut lines = vec![format!(
        "{} will post nothing more to {}.",
        state.config.page.title,
        place(&connection)
    )];
    let onward = match connection.configuration_url {
        Some(url) => {
            lines.push(
                "The app is still installed in your workspace; remove it there if you like."
                    .to_owned(),
            );
            (url, "Manage the app in Slack")
        }
        None => ("/".to_owned(), "Back to the status page"),
    };
    Ok(notice(
        &state,
        StatusCode::OK,
        "Channel removed",
        lines,
        None,
        Some(onward),
    ))
}

/// One connection on the admin page.
struct Row {
    id: i64,
    /// The operator connection's name; empty for a subscriber's.
    name: String,
    place: String,
    live: bool,
    /// Why Slack stopped accepting posts, when it did, and since when.
    dead: Option<String>,
    since: String,
    /// Who connected it, for an operator connection.
    by: Option<String>,
    /// The notifiers naming it, or what a subscriber's hears about.
    uses: String,
}

/// A notifier naming a connection nobody has connected.
struct Unconnected {
    notifier: String,
    connection: String,
}

#[derive(Template)]
#[template(path = "admin/slack.html")]
struct AdminPage {
    layout: Layout,
    csrf: String,
    done: Option<&'static str>,
    errors: Vec<String>,
    name: String,
    operator: Vec<Row>,
    unconnected: Vec<Unconnected>,
    subscriber: Vec<Row>,
    /// Whether `[subscribers]` is on, so visitors can add channels.
    subscribers: bool,
}

/// `(notifier id, connection name)` for each notifier using a connection.
fn named(state: &AppState) -> Vec<(String, String)> {
    state
        .config
        .notifiers
        .iter()
        .filter_map(|notifier| {
            let connection = notifier.slack.as_ref()?.connection.as_ref()?;
            Some((
                notifier.id.as_str().to_owned(),
                connection.as_str().to_owned(),
            ))
        })
        .collect()
}

fn row(connection: SlackConnection, uses: String) -> Row {
    Row {
        id: connection.id,
        live: connection.is_live(),
        place: place(&connection),
        dead: connection
            .dead_reason
            .clone()
            .map(|reason| match connection.dead_at_ms {
                Some(at) => format!("{reason}, since {}", when(Timestamp::from_millis(at))),
                None => reason,
            }),
        since: when(Timestamp::from_millis(connection.created_at_ms)),
        by: connection.created_by,
        name: connection.name.unwrap_or_default(),
        uses,
    }
}

async fn admin_response(
    state: &AppState,
    admin: &Admin,
    done: Option<&'static str>,
    errors: Vec<String>,
    name: String,
) -> Result<Response, HtmlError> {
    let named = named(state);
    let operator = state
        .store
        .slack_connections(SlackPurpose::Operator)
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    let unconnected = named
        .iter()
        .filter(|(_, connection)| {
            !operator
                .iter()
                .any(|known| known.name.as_deref() == Some(connection))
        })
        .map(|(notifier, connection)| Unconnected {
            notifier: notifier.clone(),
            connection: connection.clone(),
        })
        .collect();
    let operator = operator
        .into_iter()
        .map(|connection| {
            let notifiers = named
                .iter()
                .filter(|(_, name)| connection.name.as_deref() == Some(name))
                .map(|(notifier, _)| notifier.as_str())
                .collect::<Vec<_>>();
            let uses = if notifiers.is_empty() {
                "no notifier yet".to_owned()
            } else {
                format!("used by {}", notifiers.join(", "))
            };
            row(connection, uses)
        })
        .collect();
    let subscriber = if state.subscribers.is_some() {
        state
            .store
            .slack_connections(SlackPurpose::Subscriber)
            .await
            .map_err(|error| HtmlError::internal(&error))?
            .into_iter()
            .map(|connection| {
                let uses = format!("hears about {}", hears(&connection.preferences));
                row(connection, uses)
            })
            .collect()
    } else {
        Vec::new()
    };
    let status = if errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    };
    Ok(render(
        status,
        &AdminPage {
            layout: Layout::admin("Slack", admin),
            csrf: admin.csrf_token(),
            done,
            errors,
            name,
            operator,
            unconnected,
            subscriber,
            subscribers: state.subscribers.is_some(),
        },
    ))
}

/// What a subscriber channel hears about, for the admin page.
fn hears(preferences: &Preferences) -> String {
    let listed = |ids: &[String]| match ids {
        [only] if only == EVERY => "all".to_owned(),
        ids => ids.join(", "),
    };
    match (
        preferences.components.is_empty(),
        preferences.projects.is_empty(),
    ) {
        (true, true) => "nothing".to_owned(),
        (false, false) => format!(
            "incidents ({}) and releases ({})",
            listed(&preferences.components),
            listed(&preferences.projects)
        ),
        _ if preferences.projects.is_empty() => {
            format!(
                "{} ({})",
                what(preferences),
                listed(&preferences.components)
            )
        }
        _ => format!("{} ({})", what(preferences), listed(&preferences.projects)),
    }
}

async fn admin_page(
    State(state): State<AppState>,
    admin: Admin,
    query: Result<Query<DoneQuery>, QueryRejection>,
) -> Result<Response, HtmlError> {
    app(&state)?;
    let done = match done_query(query).as_deref() {
        Some("connected") => Some("Connected."),
        Some("removed") => Some("Removed."),
        _ => None,
    };
    admin_response(&state, &admin, done, Vec::new(), String::new()).await
}

async fn admin_connect(
    State(state): State<AppState>,
    AdminForm { admin, fields }: AdminForm,
) -> Result<Response, HtmlError> {
    let app = app(&state)?;
    let name = fields.text("name").trim().to_owned();
    if let Err(error) = Id::parse(&name) {
        return admin_response(&state, &admin, None, vec![format!("Name: {error}.")], name).await;
    }
    let started = app
        .begin(Purpose::Operator {
            name: name.clone(),
            admin: admin.email.clone(),
        })
        .map_err(|error| HtmlError::new(StatusCode::SERVICE_UNAVAILABLE, error))?;
    tracing::info!(connection = %name, actor = %admin.actor(), "Slack sign-in started for alerts");
    to_slack(&started)
}

async fn admin_remove(
    State(state): State<AppState>,
    id: Result<Path<i64>, PathRejection>,
    AdminForm { admin, .. }: AdminForm,
) -> Result<Response, HtmlError> {
    app(&state)?;
    let id = id.map_err(|_| HtmlError::not_found())?.0;
    if !state
        .store
        .remove_slack(id)
        .await
        .map_err(|error| HtmlError::internal(&error))?
    {
        return Err(HtmlError::not_found());
    }
    tracing::info!(connection = id, actor = %admin.actor(), "Slack connection removed");
    Ok(Redirect::to("/admin/slack?done=removed").into_response())
}
