// SPDX-License-Identifier: AGPL-3.0-only

//! A client for a running tocsin's HTTP API: blocking for the CLI, async
//! for `tocsin mcp`.
//!
//! Server answers map onto the exit-code contract: a 4xx is the caller's
//! input (exit 2, with the server's field errors), a 5xx or no answer at
//! all is a runtime failure (exit 3).

use std::time::Duration;

use reqwest::{Method, StatusCode, Url, header};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tocsin_core::config::Diagnostic;

use crate::cli::ServerArgs;
use crate::exit::Exit;
use crate::output::Failure;

/// How long one request may take.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A tocsin's HTTP API, async: what [`Client`] blocks on for the CLI and
/// what `tocsin mcp` calls from its own runtime.
#[derive(Clone)]
pub struct Api {
    base: Url,
    token: Option<String>,
    http: reqwest::Client,
}

impl Api {
    pub fn new(server: &ServerArgs) -> Result<Self, Failure> {
        let mut base = Url::parse(&server.url).map_err(|error| {
            Failure::new(
                Exit::InvalidInput,
                format!("--url {:?} is not a URL: {error}", server.url),
            )
        })?;
        if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() {
            return Err(Failure::new(
                Exit::InvalidInput,
                format!("--url {:?} must be an http or https URL", server.url),
            ));
        }
        // A base with a path (behind a proxy) keeps it when joined.
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let http = crate::net::http_client(TIMEOUT).build().map_err(|error| {
            Failure::new(Exit::Runtime, format!("cannot start HTTP client: {error}"))
        })?;
        Ok(Api {
            base,
            token: server.token.clone().filter(|token| !token.is_empty()),
            http,
        })
    }

    /// Whether a token is set.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// The URL made of `segments` under the base. Each segment is pushed
    /// percent-encoded, so a `/` or `?` in one stays inside it.
    pub fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("new() accepts only a URL that can be a base")
            .pop_if_empty()
            .extend(segments);
        url
    }

    /// `path` joined onto the base.
    pub fn join(&self, path: &str) -> Result<Url, Failure> {
        self.base.join(path).map_err(|error| {
            Failure::new(
                Exit::InvalidInput,
                format!("bad request path {path}: {error}"),
            )
        })
    }

    /// `method url` with `query`, a JSON `body` when given and the token
    /// when `authenticated`; the answer decoded as `T`.
    pub async fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &Method,
        mut url: Url,
        query: &[(&str, &str)],
        body: Option<&B>,
        authenticated: bool,
    ) -> Result<T, Failure> {
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let mut request = self.http.request(method.clone(), url.clone());
        if authenticated {
            let Some(token) = &self.token else {
                return Err(Failure::new(
                    Exit::InvalidInput,
                    "this needs a token: set TOCSIN_TOKEN or pass --token",
                ));
            };
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            let bytes = serde_json::to_vec(body).map_err(|error| {
                Failure::new(Exit::Runtime, format!("cannot encode request: {error}"))
            })?;
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .body(bytes);
        }
        let (status, bytes) = async {
            let response = request.send().await?;
            let status = response.status();
            Ok::<_, reqwest::Error>((status, response.bytes().await?))
        }
        .await
        .map_err(|error| {
            Failure::new(
                Exit::Runtime,
                format!("{method} {url} failed: {}", chain(&error.without_url())),
            )
        })?;
        if !status.is_success() {
            return Err(failure(method, &url, status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            Failure::new(
                Exit::Runtime,
                format!("{method} {url}: the server's answer is not what tocsin expects: {error}"),
            )
        })
    }
}

/// A blocking client for the CLI: [`Api`] on a runtime of its own.
pub struct Client {
    api: Api,
    runtime: tokio::runtime::Runtime,
}

impl Client {
    pub fn new(server: &ServerArgs) -> Result<Self, Failure> {
        let api = Api::new(server)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                Failure::new(Exit::Runtime, format!("cannot start runtime: {error}"))
            })?;
        Ok(Client { api, runtime })
    }

    /// `GET path`, unauthenticated.
    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failure> {
        self.request(&Method::GET, path, None::<&()>, false)
    }

    /// `GET path` with the token, for a read the server never answers
    /// without one.
    pub fn get_with_token<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failure> {
        self.request(&Method::GET, path, None::<&()>, true)
    }

    /// `GET path`, with the token when one is set: the server answers
    /// without one as it does the public. The answer comes back as the
    /// server sent it, any JSON at all; a body that is not JSON is a
    /// runtime failure.
    pub fn read(&self, path: &str) -> Result<Value, Failure> {
        self.request(&Method::GET, path, None::<&()>, self.api.has_token())
    }

    /// A write: `method path` with a JSON body and the token.
    pub fn write<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &Method,
        path: &str,
        body: &B,
    ) -> Result<T, Failure> {
        self.request(method, path, Some(body), true)
    }

    /// A write with no body: `POST` with the token to the path made of
    /// `segments` under the base, plus `query`. Each segment is pushed
    /// percent-encoded, so a `/` or `?` in one stays inside it.
    pub fn post_query<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, &str)],
    ) -> Result<T, Failure> {
        let url = self.api.url(segments);
        self.runtime
            .block_on(self.api.send(&Method::POST, url, query, None::<&()>, true))
    }

    fn request<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &Method,
        path: &str,
        body: Option<&B>,
        authenticated: bool,
    ) -> Result<T, Failure> {
        let url = self.api.join(path)?;
        self.runtime
            .block_on(self.api.send(method, url, &[], body, authenticated))
    }
}

/// An error with its causes, which say what actually went wrong
/// ("connection refused") where the error itself does not.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// The failure for an error response: its message and field errors.
fn failure(method: &Method, url: &Url, status: StatusCode, bytes: &[u8]) -> Failure {
    let exit = if status.is_client_error() {
        Exit::InvalidInput
    } else {
        Exit::Runtime
    };
    let body: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let message = body["message"]
        .as_str()
        .map_or_else(|| status.to_string(), ToOwned::to_owned);
    let mut failure = Failure::new(exit, format!("{method} {url}: {status}: {message}"));
    failure.status = Some(status.as_u16());
    if let Some(fields) = body["fields"].as_array() {
        failure.errors = fields
            .iter()
            .map(|field| Diagnostic {
                file: None,
                line: None,
                column: None,
                path: field["path"]
                    .as_str()
                    .filter(|path| !path.is_empty())
                    .map(ToOwned::to_owned),
                message: field["message"].as_str().unwrap_or_default().to_owned(),
                hint: None,
            })
            .collect();
    }
    failure
}
