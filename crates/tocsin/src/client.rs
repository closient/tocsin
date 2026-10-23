// SPDX-License-Identifier: AGPL-3.0-only

//! A blocking client for a running tocsin's HTTP API, for the CLI.
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

pub struct Client {
    base: Url,
    token: Option<String>,
    http: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl Client {
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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                Failure::new(Exit::Runtime, format!("cannot start runtime: {error}"))
            })?;
        Ok(Client {
            base,
            token: server.token.clone().filter(|token| !token.is_empty()),
            http,
            runtime,
        })
    }

    /// `GET path`, unauthenticated.
    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failure> {
        self.request(&Method::GET, path, None::<&()>, false)
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

    fn request<B: Serialize, T: DeserializeOwned>(
        &self,
        method: &Method,
        path: &str,
        body: Option<&B>,
        authenticated: bool,
    ) -> Result<T, Failure> {
        let url = self.base.join(path).map_err(|error| {
            Failure::new(
                Exit::InvalidInput,
                format!("bad request path {path}: {error}"),
            )
        })?;
        let mut request = self.http.request(method.clone(), url.clone());
        if authenticated {
            let Some(token) = &self.token else {
                return Err(Failure::new(
                    Exit::InvalidInput,
                    "writing needs a token: set TOCSIN_TOKEN or pass --token",
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
        let (status, bytes) = self
            .runtime
            .block_on(async {
                let response = request.send().await?;
                let status = response.status();
                Ok::<_, reqwest::Error>((status, response.bytes().await?))
            })
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
