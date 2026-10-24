// SPDX-License-Identifier: AGPL-3.0-only

//! The `http` probe: one request, reading at most `max_body` of the
//! response body.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Instant;

use reqwest::redirect::Policy;
use reqwest::{Method, Url};
use tocsin_core::config::{Check, HttpMethod, HttpSettings};
use tocsin_core::probe::{HttpFacts, ProbeError, ProbeErrorKind};
use url::Host;

use crate::tls::Recorder;
use crate::{ProbeContext, error_chain, milliseconds};

/// How many redirects are followed when `follow_redirects` is on.
const REDIRECT_LIMIT: usize = 10;

pub(crate) async fn probe(check: &Check, context: &ProbeContext) -> Result<HttpFacts, ProbeError> {
    let default = HttpSettings::default();
    let settings = check.http.as_ref().unwrap_or(&default);
    let mut url = Url::parse(&check.target).map_err(|error| {
        ProbeError::new(
            ProbeErrorKind::Setup,
            format!("`{}` is not a URL: {error}", check.target),
        )
    })?;
    let recorder = Recorder::new(context, settings.verify_certificate)?;
    let client = client(settings, &mut url, &recorder, context).await?;

    let mut request = client.request(method(settings.method), url);
    for (name, value) in &settings.headers {
        request = request.header(name.as_str(), value.expose());
    }
    if let Some(token) = &settings.bearer_token {
        request = request.bearer_auth(token.expose());
    }
    if let Some(basic) = &settings.basic_auth {
        request = request.basic_auth(basic.username.expose(), Some(basic.password.expose()));
    }
    if let Some(body) = &settings.body {
        request = request.body(body.expose().to_owned());
    }

    let start = Instant::now();
    let mut response = request
        .send()
        .await
        .map_err(|error| request_error(&error, &recorder))?;
    let status = response.status();
    // Redirects can change the scheme, so the response's URL decides
    // whether there is a certificate to report.
    let https = response.url().scheme() == "https";
    let final_url = response.url().to_string();
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in response.headers() {
        headers
            .entry(name.as_str().to_owned())
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    let redirect_location = status
        .is_redirection()
        .then(|| {
            headers
                .get("location")
                .and_then(|values| values.first())
                .cloned()
        })
        .flatten();
    // `config::load` bounds `max_body` far below `usize::MAX`.
    let limit = usize::try_from(settings.max_body.bytes()).unwrap_or(usize::MAX);
    let (body, body_truncated) = read_body(&mut response, limit, &recorder).await?;
    let latency_ms = milliseconds(start.elapsed());

    Ok(HttpFacts {
        latency_ms,
        url: final_url,
        status: status.as_u16(),
        redirect_location,
        headers,
        body_bytes: body.len(),
        body: String::from_utf8_lossy(&body).into_owned(),
        body_truncated,
        certificate: if https { recorder.facts() } else { None },
    })
}

/// A client for one probe. With `server_name` set, `url`'s host becomes
/// that name (for SNI, `Host` and the certificate check) while the
/// connection still goes to the target host: its IP literal, or every
/// address its name resolves to.
async fn client(
    settings: &HttpSettings,
    url: &mut Url,
    recorder: &std::sync::Arc<Recorder>,
    context: &ProbeContext,
) -> Result<reqwest::Client, ProbeError> {
    let mut builder = reqwest::Client::builder()
        .tls_backend_preconfigured(recorder.client_config()?)
        .redirect(if settings.follow_redirects {
            Policy::limited(REDIRECT_LIMIT)
        } else {
            Policy::none()
        })
        .no_proxy()
        .user_agent(&context.user_agent);
    if let Some(name) = &settings.server_name {
        let port = url.port_or_known_default().unwrap_or(80);
        let addresses: Vec<SocketAddr> = match url.host() {
            Some(Host::Ipv4(ip)) => vec![SocketAddr::new(ip.into(), port)],
            Some(Host::Ipv6(ip)) => vec![SocketAddr::new(ip.into(), port)],
            Some(Host::Domain(domain)) => {
                let domain = domain.to_owned();
                tokio::net::lookup_host((domain.as_str(), port))
                    .await
                    .map_err(|error| {
                        ProbeError::new(
                            ProbeErrorKind::Connect,
                            format!("could not resolve {domain}: {error}"),
                        )
                    })?
                    .collect()
            }
            None => Vec::new(),
        };
        if addresses.is_empty() {
            return Err(ProbeError::new(
                ProbeErrorKind::Connect,
                format!("{} has no addresses", url.host_str().unwrap_or_default()),
            ));
        }
        url.set_host(Some(name)).map_err(|error| {
            ProbeError::new(
                ProbeErrorKind::Setup,
                format!("`{name}` cannot be a URL host: {error}"),
            )
        })?;
        builder = builder.resolve_to_addrs(name, &addresses);
    }
    builder.build().map_err(|error| {
        ProbeError::new(
            ProbeErrorKind::Setup,
            format!("could not build the HTTP client: {}", error_chain(&error)),
        )
    })
}

/// Reads at most `limit` bytes of the body; the flag says whether there
/// was more.
async fn read_body(
    response: &mut reqwest::Response,
    limit: usize,
    recorder: &Recorder,
) -> Result<(Vec<u8>, bool), ProbeError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| request_error(&error, recorder))?
    {
        let room = limit - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

fn method(method: HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Head => Method::HEAD,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
        HttpMethod::Options => Method::OPTIONS,
    }
}

fn request_error(error: &reqwest::Error, recorder: &Recorder) -> ProbeError {
    let kind = if error.is_timeout() {
        ProbeErrorKind::Timeout
    } else if recorder.rejected() || is_tls(error) {
        ProbeErrorKind::Tls
    } else if error.is_connect() {
        ProbeErrorKind::Connect
    } else if error.is_builder() {
        ProbeErrorKind::Setup
    } else {
        ProbeErrorKind::Protocol
    };
    ProbeError::new(kind, error_chain(error))
}

/// Whether a rustls error is anywhere in the chain. `io::Error` hides its
/// inner error from `source()`, so it is unwrapped by hand.
fn is_tls(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<rustls::Error>() {
            return true;
        }
        if let Some(inner) = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            && inner.is::<rustls::Error>()
        {
            return true;
        }
        current = error.source();
    }
    false
}
