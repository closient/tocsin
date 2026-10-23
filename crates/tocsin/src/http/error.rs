// SPDX-License-Identifier: AGPL-3.0-only

//! API errors, always rendered as JSON `{error, message, fields?}`, and the
//! JSON body extractor that reports bad fields by path.

use axum::Json;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tocsin_core::incident::FieldError;
use utoipa::ToSchema;

/// The body of every error response.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable code: `unauthorized`, `forbidden`,
    /// `not_found`, `method_not_allowed`, `bad_request`, `invalid` or `internal`.
    pub error: &'static str,
    /// What went wrong, for a person.
    pub message: String,
    /// For `invalid`: every problem, by path in the request.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldError>,
}

/// An error response.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    body: ErrorBody,
}

impl ApiError {
    fn new(status: StatusCode, error: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            body: ErrorBody {
                error,
                message: message.into(),
                fields: Vec::new(),
            },
        }
    }

    /// 401: no credentials, or credentials tocsin does not know.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    /// 403: a known writer without the scope the route needs.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    /// 404.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// 405: the route exists, the method does not.
    pub fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "this route does not take that method",
        )
    }

    /// 400: a body or query that is not even well-formed.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    /// 422: well-formed, but some fields are wrong.
    pub fn invalid(fields: Vec<FieldError>) -> Self {
        let message = fields
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        let mut error = Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", message);
        error.body.fields = fields;
        error
    }

    /// 500. The cause is logged, not sent.
    pub fn internal(cause: &dyn std::fmt::Display) -> Self {
        tracing::error!(error = %cause, "request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error; see the server log",
        )
    }

    /// The HTTP status.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// What went wrong, for a person.
    pub fn message(&self) -> &str {
        &self.body.message
    }

    /// For a 422: every problem, by path.
    pub fn fields(&self) -> &[FieldError] {
        &self.body.fields
    }
}

impl From<tocsin_store::Error> for ApiError {
    fn from(error: tocsin_store::Error) -> Self {
        match error {
            tocsin_store::Error::Invalid(fields) => ApiError::invalid(fields),
            other => ApiError::internal(&other),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(self.body)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// A JSON request body. Unlike `axum::Json`, a body that parses as JSON
/// but does not fit the type is a 422 naming the field path, and every
/// rejection is an [`ApiError`].
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let json = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|mime| {
                let mime = mime.trim();
                mime.eq_ignore_ascii_case("application/json") || mime.ends_with("+json")
            });
        if !json {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "bad_request",
                "send the body as JSON with `Content-Type: application/json`",
            ));
        }
        let bytes = axum::body::Bytes::from_request(request, state)
            .await
            .map_err(|rejection| match rejection.status() {
                StatusCode::PAYLOAD_TOO_LARGE => ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    format!("the body is over the {} byte limit", super::BODY_LIMIT),
                ),
                status => ApiError::new(status, "bad_request", rejection.body_text()),
            })?;
        parse(&bytes).map(ApiJson)
    }
}

/// Parses `bytes` as `T`, reporting a type mismatch at its field path.
pub fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        let path = error.path().to_string();
        let inner = error.into_inner();
        if !inner.is_data() {
            return ApiError::bad_request(format!(
                "the body is not valid JSON: {}",
                strip_position(&inner.to_string())
            ));
        }
        let message = strip_position(&inner.to_string()).to_owned();
        let path = if path == "." { String::new() } else { path };
        ApiError::invalid(vec![field_error(path, message)])
    })?;
    deserializer
        .end()
        .map_err(|_| ApiError::bad_request("the body has more after the JSON value"))?;
    Ok(value)
}

/// A missing field is reported by serde at its parent; move it to the
/// field itself.
fn field_error(path: String, message: String) -> FieldError {
    if let Some(name) = message
        .strip_prefix("missing field `")
        .and_then(|rest| rest.strip_suffix('`'))
    {
        let path = if path.is_empty() {
            name.to_owned()
        } else {
            format!("{path}.{name}")
        };
        return FieldError::new(path, "required");
    }
    FieldError::new(path, message)
}

/// `serde_json` appends ` at line L column C`; the path says where already.
fn strip_position(message: &str) -> &str {
    match message.rfind(" at line ") {
        Some(index) if message[index..].contains(" column ") => &message[..index],
        _ => message,
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::parse;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Body {
        name: String,
        tags: Vec<u8>,
    }

    fn fields(body: &str) -> Vec<(String, String)> {
        let error = parse::<Body>(body.as_bytes()).unwrap_err();
        assert_eq!(error.status().as_u16(), 422, "{error:?}");
        error
            .body
            .fields
            .into_iter()
            .map(|field| (field.path, field.message))
            .collect()
    }

    #[test]
    fn type_errors_name_the_field_path() {
        let found = fields(r#"{"name":"a","tags":[1,"x"]}"#);
        assert_eq!(found[0].0, "tags[1]");
        assert!(!found[0].1.contains("line"), "{found:?}");
    }

    #[test]
    fn a_missing_field_is_reported_at_the_field() {
        assert_eq!(
            fields(r#"{"tags":[]}"#),
            [("name".to_owned(), "required".to_owned())]
        );
    }

    #[test]
    fn an_unknown_field_is_reported_at_itself() {
        let found = fields(r#"{"name":"a","tags":[],"colour":1}"#);
        assert_eq!(found[0].0, "colour");
        assert!(found[0].1.contains("colour"), "{found:?}");
    }

    #[test]
    fn malformed_json_is_400() {
        for body in ["{", "nope", r#"{"name":"a","tags":[]} x"#] {
            let error = parse::<Body>(body.as_bytes()).unwrap_err();
            assert_eq!(error.status().as_u16(), 400, "{body}");
        }
    }
}
