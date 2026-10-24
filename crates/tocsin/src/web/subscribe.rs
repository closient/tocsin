// SPDX-License-Identifier: AGPL-3.0-only

//! `/subscribe`: the sign-up form, and the pages the links in subscriber
//! mail open; and `/admin/subscribers`, the operator's list. Merged only
//! when `[subscribers]` is on, so with it off each is the same 404 as any
//! unknown path. `docs/subscribers.md` describes the flow.
//!
//! - **Sign-up** posts to [`crate::subscribers::Subscriptions::sign_up`],
//!   the same checks, limits and answer as `POST /api/v1/subscribers`.
//! - **Confirm** and **unsubscribe** answer `GET` with a page holding one
//!   button and act only on `POST`, so a mail scanner that follows links
//!   changes nothing. Unsubscribe's `POST` is also RFC 8058's one-click
//!   request, which comes from the mail client with no `Origin`, so it is
//!   the one form here without the origin check; its token is the guard.
//! - **Manage** links carry `<id>.<secret>`; the secret is compared in
//!   constant time. These pages are never cached or indexed, and the
//!   `Referrer-Policy` keeps their URLs on this site.

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use serde::Deserialize;
use tocsin_core::incident::FieldError;
use tocsin_store::{EVERY, Preferences, Subscriber};

use super::csrf;
use super::feeds;
use super::form::Fields;
use super::identity::{Admin, AdminForm};
use super::page::{CONTENT_SECURITY_POLICY, HtmlError, Layout, render, when};
use super::status::{Chrome, web_page};
use crate::http::AppState;
use crate::http::error::{ApiError, retry_after_value};
use crate::http::subscribers::{DEFAULT_LIMIT, page};
use crate::subscribers::{SignupRequest, Subscriptions, paths, token, turnstile};

/// Longest confirmation or manage token taken from a path. Real ones are
/// 43 characters, or a number, a dot and 43.
const MAX_TOKEN: usize = 96;

/// The public pages.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(paths::SUBSCRIBE, get(signup_page).post(sign_up))
        .route(
            "/subscribe/confirm/{token}",
            get(confirm_page).post(confirm),
        )
        .route("/subscribe/manage/{token}", get(manage_page).post(save))
        .route(
            "/subscribe/unsubscribe/{token}",
            get(unsubscribe_page).post(unsubscribe),
        )
}

/// The operator's list. Merged only when the admin pages are on too.
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/admin/subscribers", get(admin_list))
        .route(
            "/admin/subscribers/{id}/remove",
            get(|| async { Redirect::to("/admin/subscribers") }).post(admin_remove),
        )
}

/// One checkbox.
pub struct Choice {
    pub(super) value: String,
    pub(super) label: String,
    pub(super) checked: bool,
}

/// The sign-up form: compact on `/`, every choice on `/subscribe`.
pub struct SignupForm {
    /// Every component and every project as a choice, rather than the two
    /// "everything" boxes and a link here.
    pub(super) full: bool,
    pub(super) email: String,
    pub(super) components: Vec<Choice>,
    /// Empty when no project is public.
    pub(super) projects: Vec<Choice>,
    /// Each problem with the last submission, for a person.
    pub(super) errors: Vec<String>,
    /// The Turnstile site key. On `/` the form is then a link to
    /// `/subscribe`, which loads the widget.
    pub(super) turnstile: Option<String>,
    /// The no-email alternative.
    pub(super) incident_feed: &'static str,
    pub(super) release_feed: Option<&'static str>,
}

impl SignupForm {
    /// The form on `/`: an address, all incidents, and releases if any
    /// project is public.
    pub fn compact(state: &AppState, subscriptions: &Subscriptions) -> Self {
        let mut form = Self::full(state, subscriptions, &every(), &[]);
        form.full = false;
        form.components.truncate(1);
        form.projects.truncate(1);
        form
    }

    /// The form on `/subscribe`, with `components` and `projects` checked.
    fn full(
        state: &AppState,
        subscriptions: &Subscriptions,
        components: &[String],
        projects: &[String],
    ) -> Self {
        let preferences = Preferences {
            components: components.to_vec(),
            projects: projects.to_vec(),
        };
        let (components, projects) = choices(state, &preferences);
        SignupForm {
            full: true,
            email: String::new(),
            components,
            projects,
            errors: Vec::new(),
            turnstile: subscriptions.turnstile_site_key().map(str::to_owned),
            incident_feed: feeds::INCIDENTS,
            release_feed: state
                .config
                .projects
                .iter()
                .any(|project| project.public)
                .then_some(feeds::RELEASES),
        }
    }
}

fn every() -> Vec<String> {
    vec![EVERY.to_owned()]
}

/// The checkboxes for `preferences`: "every component" then each one,
/// and "every public project" then each one, or none without a public
/// project.
fn choices(state: &AppState, preferences: &Preferences) -> (Vec<Choice>, Vec<Choice>) {
    let checked = |chosen: &[String], id: &str| chosen.iter().any(|known| known == id);
    let all_components = checked(&preferences.components, EVERY);
    let components = std::iter::once(Choice {
        value: EVERY.to_owned(),
        label: "Every component".to_owned(),
        checked: all_components,
    })
    .chain(state.config.components.iter().map(|component| Choice {
        value: component.id.as_str().to_owned(),
        label: component.name.clone(),
        checked: !all_components && checked(&preferences.components, component.id.as_str()),
    }))
    .collect();
    let public = state
        .config
        .projects
        .iter()
        .filter(|project| project.public)
        .collect::<Vec<_>>();
    let all_projects = checked(&preferences.projects, EVERY);
    let projects = if public.is_empty() {
        Vec::new()
    } else {
        std::iter::once(Choice {
            value: EVERY.to_owned(),
            label: "Every project".to_owned(),
            checked: all_projects,
        })
        .chain(public.into_iter().map(|project| Choice {
            value: project.id.as_str().to_owned(),
            label: project.name.clone(),
            checked: !all_projects && checked(&preferences.projects, project.id.as_str()),
        }))
        .collect()
    };
    (components, projects)
}

#[derive(Template)]
#[template(path = "status/subscribe-page.html")]
struct SignupPage {
    layout: Layout,
    chrome: Chrome,
    form: SignupForm,
}

/// `errors` as sentences, each naming its field as the form does.
fn explain(errors: &[FieldError]) -> Vec<String> {
    errors
        .iter()
        .map(|error| {
            let field = match error.path.split('[').next().unwrap_or_default() {
                "email" => "Email address",
                "components" => "Incidents",
                "projects" => "Releases",
                "turnstile" => "Spam check",
                _ => "Form",
            };
            format!("{field}: {}.", error.message)
        })
        .collect()
}

/// A link after a [`NoticePage`]'s text.
struct Onward {
    href: String,
    text: &'static str,
}

/// A button that posts, on a [`NoticePage`].
struct Action {
    path: String,
    label: &'static str,
}

/// A short page: a heading, a few sentences, perhaps one button.
#[derive(Template)]
#[template(path = "status/subscribe-notice.html")]
struct NoticePage {
    layout: Layout,
    chrome: Chrome,
    heading: String,
    lines: Vec<String>,
    action: Option<Action>,
    onward: Option<Onward>,
}

#[derive(Template)]
#[template(path = "status/subscribe-manage.html")]
struct ManagePage {
    layout: Layout,
    chrome: Chrome,
    email: String,
    /// What the page says it just did, if anything.
    done: Option<&'static str>,
    path: String,
    unsubscribe: String,
    components: Vec<Choice>,
    projects: Vec<Choice>,
    errors: Vec<String>,
}

/// The layout and chrome every page here shares.
fn frame(state: &AppState, title: &str, path: &str) -> (Layout, Chrome) {
    let config = &state.config;
    let title = format!("{title} · {}", config.page.title);
    let json_ld = serde_json::json!({
        "@context": "https://schema.org",
        "@graph": [web_page(config, &title, path)],
    });
    (
        Layout::public(title)
            .with_feeds(feeds::site(config))
            .themed(&config.page),
        Chrome::new(config, &json_ld),
    )
}

/// `page` with no indexing: these pages are personal or transient.
fn private(status: StatusCode, page: &impl Template) -> Response {
    let mut response = render(status, page);
    response.headers_mut().insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex"),
    );
    response
}

fn notice(
    state: &AppState,
    status: StatusCode,
    heading: &str,
    lines: Vec<String>,
    action: Option<Action>,
    onward: Option<(String, &'static str)>,
) -> Response {
    let (layout, chrome) = frame(state, heading, paths::SUBSCRIBE);
    private(
        status,
        &NoticePage {
            layout,
            chrome,
            heading: heading.to_owned(),
            lines,
            action,
            onward: onward.map(|(href, text)| Onward { href, text }),
        },
    )
}

/// The running feature. The routes are merged only when it is on, so this
/// fails only if it was turned off by a reload, which keeps it as it was.
fn enabled(state: &AppState) -> Result<Arc<Subscriptions>, HtmlError> {
    state.subscribers.clone().ok_or_else(HtmlError::not_found)
}

/// Refuses a post that did not come from a page on this site.
fn same_site(headers: &HeaderMap, subscriptions: &Subscriptions) -> Result<(), HtmlError> {
    csrf::same_origin(headers, subscriptions.origin()).map_err(|reason| {
        tracing::warn!(reason, "subscriber form refused as cross-site");
        HtmlError::new(
            StatusCode::FORBIDDEN,
            "This form has to be sent from this site's own page. Go back, reload it and try again.",
        )
    })
}

/// The sign-up page, with Turnstile's script and CSP when it is on.
fn signup_response(state: &AppState, form: SignupForm, status: StatusCode) -> Response {
    let (layout, mut chrome) = frame(state, "Subscribe", paths::SUBSCRIBE);
    let turnstile = form.turnstile.is_some();
    if turnstile {
        chrome = chrome.with_script(turnstile::SCRIPT_URL);
    }
    let mut response = render(
        status,
        &SignupPage {
            layout,
            chrome,
            form,
        },
    );
    if turnstile {
        let policy = turnstile_policy();
        if let Ok(value) = HeaderValue::from_str(&policy) {
            response
                .headers_mut()
                .insert(header::CONTENT_SECURITY_POLICY, value);
        }
    }
    response
}

/// [`CONTENT_SECURITY_POLICY`] letting Turnstile's script and frame in.
fn turnstile_policy() -> String {
    CONTENT_SECURITY_POLICY.replace(
        "script-src 'self';",
        &format!(
            "script-src 'self' {origin}; frame-src {origin};",
            origin = turnstile::ORIGIN
        ),
    )
}

async fn signup_page(State(state): State<AppState>) -> Result<Response, HtmlError> {
    let subscriptions = enabled(&state)?;
    let form = SignupForm::full(&state, &subscriptions, &every(), &[]);
    Ok(signup_response(&state, form, StatusCode::OK))
}

async fn sign_up(
    State(state): State<AppState>,
    parts: Parts,
    body: Bytes,
) -> Result<Response, HtmlError> {
    let subscriptions = enabled(&state)?;
    same_site(&parts.headers, &subscriptions)?;
    let fields = Fields::parse(&body);
    let request = SignupRequest {
        email: fields.text("email"),
        components: fields.all("components"),
        projects: fields.all("projects"),
        turnstile: fields.get(turnstile::FIELD).map(str::to_owned),
    };
    let ip = subscriptions.client_ip(&parts)?;
    match subscriptions.sign_up(&state, request.clone(), ip).await {
        Ok(()) => Ok(notice(
            &state,
            StatusCode::OK,
            "Check your inbox",
            vec![
                "If the address can receive mail, a message is on its way. Follow the link in it \
                 to confirm; nothing else is sent until you do."
                    .to_owned(),
                "Nothing there after a few minutes? Look in spam, or reply to any of our emails."
                    .to_owned(),
            ],
            None,
            Some(("/".to_owned(), "Back to the status page")),
        )),
        Err(error) if error.status() == StatusCode::UNPROCESSABLE_ENTITY => {
            let mut form = SignupForm::full(
                &state,
                &subscriptions,
                &request.components,
                &request.projects,
            );
            form.email = request.email;
            form.errors = explain(error.fields());
            Ok(signup_response(
                &state,
                form,
                StatusCode::UNPROCESSABLE_ENTITY,
            ))
        }
        Err(error) => Ok(refusal(error)),
    }
}

/// An API error as a page, keeping `Retry-After`.
fn refusal(error: ApiError) -> Response {
    let wait = error.retry_after();
    let mut response = HtmlError::from(error).into_response();
    if let Some(wait) = wait {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, retry_after_value(wait));
    }
    response
}

/// A token from a path, if it is shaped like one.
fn path_token(token: Result<Path<String>, PathRejection>) -> Result<String, HtmlError> {
    let Ok(Path(token)) = token else {
        return Err(HtmlError::not_found());
    };
    let shaped = !token.is_empty()
        && token.len() <= MAX_TOKEN
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte));
    if shaped {
        Ok(token)
    } else {
        Err(HtmlError::not_found())
    }
}

fn expired_link(state: &AppState) -> Response {
    notice(
        state,
        StatusCode::NOT_FOUND,
        "This link has expired",
        vec!["It was already used, or it is too old. Sign up again to get a fresh one.".to_owned()],
        None,
        Some((paths::SUBSCRIBE.to_owned(), "Sign up again")),
    )
}

async fn confirm_page(
    State(state): State<AppState>,
    token: Result<Path<String>, PathRejection>,
) -> Result<Response, HtmlError> {
    enabled(&state)?;
    let token = path_token(token)?;
    let pending = state
        .store
        .subscriber_to_confirm(token::digest(&token), tocsin_store::now_ms())
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    let Some(subscriber) = pending else {
        return Ok(expired_link(&state));
    };
    Ok(notice(
        &state,
        StatusCode::OK,
        "Confirm your subscription",
        vec![format!(
            "Send email about {} to {}?",
            what(&subscriber.preferences),
            subscriber.email
        )],
        Some(Action {
            path: paths::confirm(&token),
            label: "Confirm",
        }),
        None,
    ))
}

async fn confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    token: Result<Path<String>, PathRejection>,
) -> Result<Response, HtmlError> {
    let subscriptions = enabled(&state)?;
    same_site(&headers, &subscriptions)?;
    let token = path_token(token)?;
    let confirmed = state
        .store
        .confirm_subscriber(token::digest(&token), tocsin_store::now_ms())
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    let Some(subscriber) = confirmed else {
        return Ok(expired_link(&state));
    };
    tracing::info!(
        subscriber = subscriber.id,
        email = %token::mask(&subscriber.email),
        "subscriber confirmed"
    );
    let manage = token::manage(subscriber.id, &subscriber.manage_secret);
    Ok(Redirect::to(&format!("{}?done=confirmed", paths::manage(&manage))).into_response())
}

/// What `preferences` covers, in words.
fn what(preferences: &Preferences) -> &'static str {
    match (
        preferences.components.is_empty(),
        preferences.projects.is_empty(),
    ) {
        (false, false) => "incidents and releases",
        (false, true) => "incidents",
        (true, _) => "releases",
    }
}

/// The subscriber a manage token names, if its secret matches.
async fn holder(state: &AppState, token: &str) -> Result<Option<Subscriber>, HtmlError> {
    let Some((id, secret)) = token::parse_manage(token) else {
        return Ok(None);
    };
    let subscriber = state
        .store
        .subscriber(id)
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    Ok(subscriber.filter(|subscriber| token::same_secret(secret, &subscriber.manage_secret)))
}

fn gone(state: &AppState) -> Response {
    notice(
        state,
        StatusCode::NOT_FOUND,
        "No subscription here",
        vec!["This address is not subscribed any more, or the link is not one we sent.".to_owned()],
        None,
        Some((paths::SUBSCRIBE.to_owned(), "Subscribe")),
    )
}

#[derive(Deserialize)]
struct ManageQuery {
    done: Option<String>,
}

async fn manage_page(
    State(state): State<AppState>,
    token: Result<Path<String>, PathRejection>,
    query: Result<Query<ManageQuery>, QueryRejection>,
) -> Result<Response, HtmlError> {
    enabled(&state)?;
    let token = path_token(token)?;
    let Some(subscriber) = holder(&state, &token).await? else {
        return Ok(gone(&state));
    };
    let done = match query.ok().and_then(|Query(query)| query.done).as_deref() {
        Some("confirmed") => Some("Confirmed. You will get email about what is checked below."),
        Some("saved") => Some("Saved."),
        _ => None,
    };
    let preferences = subscriber.preferences.clone();
    Ok(manage_response(
        &state,
        &token,
        subscriber,
        &preferences,
        done,
        Vec::new(),
    ))
}

fn manage_response(
    state: &AppState,
    token: &str,
    subscriber: Subscriber,
    preferences: &Preferences,
    done: Option<&'static str>,
    errors: Vec<String>,
) -> Response {
    let (layout, chrome) = frame(state, "Your subscription", paths::SUBSCRIBE);
    let (components, projects) = choices(state, preferences);
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
            email: subscriber.email,
            done,
            path: paths::manage(token),
            unsubscribe: paths::unsubscribe(token),
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
    let Some(subscriber) = holder(&state, &token).await? else {
        return Ok(gone(&state));
    };
    let fields = Fields::parse(&body);
    let components = fields.all("components");
    let projects = fields.all("projects");
    match crate::subscribers::preferences(&state, &components, &projects) {
        Ok(preferences) => {
            let saved = state
                .store
                .set_subscriber_preferences(subscriber.id, preferences)
                .await
                .map_err(|error| HtmlError::internal(&error))?;
            if !saved {
                return Ok(gone(&state));
            }
            Ok(Redirect::to(&format!("{}?done=saved", paths::manage(&token))).into_response())
        }
        Err(error) => {
            let asked = Preferences {
                components,
                projects,
            };
            Ok(manage_response(
                &state,
                &token,
                subscriber,
                &asked,
                None,
                explain(error.fields()),
            ))
        }
    }
}

async fn unsubscribe_page(
    State(state): State<AppState>,
    token: Result<Path<String>, PathRejection>,
) -> Result<Response, HtmlError> {
    enabled(&state)?;
    let token = path_token(token)?;
    let Some(subscriber) = holder(&state, &token).await? else {
        return Ok(gone(&state));
    };
    Ok(notice(
        &state,
        StatusCode::OK,
        "Unsubscribe",
        vec![format!(
            "Stop all email from {} to {}?",
            state.config.page.title, subscriber.email
        )],
        Some(Action {
            path: paths::unsubscribe(&token),
            label: "Unsubscribe",
        }),
        Some((paths::manage(&token), "Choose what you hear about instead")),
    ))
}

/// Removes the subscriber at once, with any mail still queued for them.
/// Also RFC 8058's one-click `POST`, sent by the mail client: no origin
/// check, no login, and the answer is a page either way.
async fn unsubscribe(
    State(state): State<AppState>,
    token: Result<Path<String>, PathRejection>,
) -> Result<Response, HtmlError> {
    enabled(&state)?;
    let token = path_token(token)?;
    let Some(subscriber) = holder(&state, &token).await? else {
        return Ok(gone(&state));
    };
    let removed = state
        .store
        .remove_subscriber(subscriber.id)
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    if !removed {
        return Ok(gone(&state));
    }
    tracing::info!(
        subscriber = subscriber.id,
        email = %token::mask(&subscriber.email),
        "subscriber unsubscribed"
    );
    Ok(notice(
        &state,
        StatusCode::OK,
        "You are unsubscribed",
        vec![format!(
            "{} will send no more email to {}.",
            state.config.page.title, subscriber.email
        )],
        None,
        Some(("/".to_owned(), "Back to the status page")),
    ))
}

/// One subscriber in the operator's list.
struct AdminRow {
    id: i64,
    email: String,
    confirmed: bool,
    created_at: String,
    components: String,
    projects: String,
}

#[derive(Template)]
#[template(path = "admin/subscribers.html")]
struct AdminPage {
    layout: Layout,
    csrf: String,
    confirmed: i64,
    unconfirmed: i64,
    rows: Vec<AdminRow>,
    next_after: Option<i64>,
}

#[derive(Deserialize)]
struct AdminQuery {
    after: Option<i64>,
}

async fn admin_list(
    State(state): State<AppState>,
    admin: Admin,
    query: Result<Query<AdminQuery>, QueryRejection>,
) -> Result<Response, HtmlError> {
    enabled(&state)?;
    let Query(query) =
        query.map_err(|_| HtmlError::new(StatusCode::BAD_REQUEST, "`after` must be a number."))?;
    let list = page(&state, query.after, DEFAULT_LIMIT).await?;
    let listed = |ids: &[String]| match ids {
        [] => "none".to_owned(),
        [only] if only == EVERY => "all".to_owned(),
        ids => ids.join(", "),
    };
    let rows = list
        .subscribers
        .into_iter()
        .map(|entry| AdminRow {
            id: entry.id,
            email: entry.email,
            confirmed: entry.confirmed_at.is_some(),
            created_at: when(entry.created_at),
            components: listed(&entry.components),
            projects: listed(&entry.projects),
        })
        .collect();
    Ok(render(
        StatusCode::OK,
        &AdminPage {
            layout: Layout::admin("Subscribers", &admin),
            csrf: admin.csrf_token(),
            confirmed: list.totals.confirmed,
            unconfirmed: list.totals.unconfirmed,
            rows,
            next_after: list.next_after,
        },
    ))
}

async fn admin_remove(
    State(state): State<AppState>,
    id: Result<Path<i64>, PathRejection>,
    AdminForm { admin, .. }: AdminForm,
) -> Result<Response, HtmlError> {
    enabled(&state)?;
    let id = id.map_err(|_| HtmlError::not_found())?.0;
    if !state
        .store
        .remove_subscriber(id)
        .await
        .map_err(|error| HtmlError::internal(&error))?
    {
        return Err(HtmlError::not_found());
    }
    tracing::info!(subscriber = id, actor = %admin.actor(), "subscriber removed");
    Ok(Redirect::to("/admin/subscribers").into_response())
}
