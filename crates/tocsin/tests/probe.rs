// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of `tocsin probe run`, run against the built binary
//! and a local HTTP server. The JSON output is pinned by snapshots in
//! `tests/snapshots/`; timings, the date header and the server's port are
//! normalized first because they change from run to run.

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Output};

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::Value;

/// An Atlassian Statuspage `components.json` body, cut down to the fields
/// a vendor-status check reads.
const STATUSPAGE: &str = r#"{
  "page": {"id": "y2j98763l1zq", "name": "Example", "url": "https://status.example.com"},
  "components": [
    {"id": "b13yz5g2cw10", "name": "API", "status": "operational", "group": false},
    {"id": "9pdnc9dg8yrq", "name": "CDN", "status": "degraded_performance", "group": false}
  ]
}"#;

const CHECKS: &str = r#"
[[check]]
id = "statuspage-api"
type = "http"
target = "http://127.0.0.1:PORT/api/v2/components.json"

[[check.assert]]
status = [200]

[[check.assert]]
header = { name = "content-type", contains = "json" }

[[check.assert]]
json = { path = "$.components[?@.name=='API'].status", eq = "operational" }

[[check.assert]]
latency_ms = { lt = 5000 }

[[check]]
id = "statuspage-cdn"
type = "http"
target = "http://127.0.0.1:PORT/api/v2/components.json"

[[check.assert]]
json = { path = "$.components[?@.name=='CDN'].status", eq = "operational" }
on_fail = "degraded"

[[check.assert]]
body = { contains = "\"page\"" }

[[check]]
id = "moved"
type = "http"
target = "http://127.0.0.1:PORT/moved"

[[check]]
id = "unavailable"
type = "http"
target = "http://127.0.0.1:PORT/unavailable"

[[check]]
id = "refused"
type = "tcp"
target = "127.0.0.1:CLOSED"
"#;

/// Starts the HTTP server on its own thread and returns its port.
fn server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    listener.set_nonblocking(true).expect("non-blocking");
    let port = listener.local_addr().expect("has an address").port();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let app = Router::new()
                .route(
                    "/api/v2/components.json",
                    get(|| async { ([(header::CONTENT_TYPE, "application/json")], STATUSPAGE) }),
                )
                .route(
                    "/moved",
                    get(|| async {
                        (
                            StatusCode::FOUND,
                            [(header::LOCATION, "/api/v2/components.json")],
                        )
                    }),
                )
                .route(
                    "/unavailable",
                    get(|| async {
                        (StatusCode::SERVICE_UNAVAILABLE, "down for maintenance").into_response()
                    }),
                );
            let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
            axum::serve(listener, app).await.expect("serves");
        });
    });
    port
}

/// A port nothing listens on: bound, then released.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let address: SocketAddr = listener.local_addr().expect("has an address");
    address.port()
}

struct Fixture {
    config: tempfile::TempDir,
    port: u16,
    closed: u16,
}

fn fixture() -> Fixture {
    let port = server();
    let closed = closed_port();
    let config = tempfile::tempdir().expect("temp folder");
    write(config.path(), "server.toml", "[server]\n");
    write(
        config.path(),
        "checks.toml",
        &CHECKS
            .replace("PORT", &port.to_string())
            .replace("CLOSED", &closed.to_string()),
    );
    Fixture {
        config,
        port,
        closed,
    }
}

fn write(folder: &Path, name: &str, text: &str) {
    std::fs::write(folder.join(name), text).expect("writes");
}

impl Fixture {
    fn tocsin(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tocsin"))
            .args(args)
            .arg("--config")
            .arg(self.config.path())
            .env("TOCSIN_LOG", "warn")
            .output()
            .expect("tocsin runs")
    }

    /// Stdout as text with the ports replaced by `PORT` and `CLOSED`.
    fn stdout(&self, output: &Output) -> String {
        String::from_utf8(output.stdout.clone())
            .expect("UTF-8")
            .replace(&format!(":{}", self.port), ":PORT")
            .replace(&format!(":{}", self.closed), ":CLOSED")
    }

    /// Stdout as exactly one JSON document, normalized for a snapshot.
    fn json(&self, output: &Output) -> Value {
        let mut document: Value =
            serde_json::from_str(&self.stdout(output)).expect("stdout is one JSON document");
        normalize(&mut document);
        document
    }
}

/// Replaces what changes from run to run with a fixed marker.
fn normalize(document: &mut Value) {
    for field in ["started_at_ms", "duration_ms"] {
        if document.get(field).is_some() {
            document[field] = "[ms]".into();
        }
    }
    if let Some(facts) = document.get_mut("facts").filter(|facts| facts.is_object()) {
        facts["latency_ms"] = "[ms]".into();
        if let Some(date) = facts.pointer_mut("/headers/date/0") {
            *date = "[date]".into();
        }
    }
    if let Some(Value::Array(verdicts)) = document.get_mut("assertions") {
        for verdict in verdicts {
            if verdict["condition"] == "latency_ms" {
                verdict["actual"] = "[ms]".into();
            }
        }
    }
}

#[test]
fn an_up_check_exits_zero_with_one_json_document() {
    let fixture = fixture();
    let output = fixture.tocsin(&["probe", "run", "statuspage-api", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    insta::assert_json_snapshot!(fixture.json(&output));
}

#[test]
fn a_degraded_check_exits_one() {
    let fixture = fixture();
    let output = fixture.tocsin(&["probe", "run", "statuspage-cdn", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    insta::assert_json_snapshot!(fixture.json(&output));
}

#[test]
fn a_degraded_check_as_text() {
    let fixture = fixture();
    let output = fixture.tocsin(&["probe", "run", "statuspage-cdn"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = fixture.stdout(&output);
    let normalized: Vec<String> = text
        .lines()
        .map(|line| {
            if line.starts_with("check ") {
                line.split(" after ").next().unwrap_or(line).to_owned() + " after [ms] ms"
            } else if line.starts_with("  latency_ms: ") {
                "  latency_ms: [ms]".to_owned()
            } else if line.starts_with("  headers.date: ") {
                "  headers.date: [date]".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect();
    insta::assert_snapshot!(normalized.join("\n"));
}

#[test]
fn a_redirect_is_reported_not_followed() {
    let fixture = fixture();
    let output = fixture.tocsin(&["probe", "run", "moved", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    insta::assert_json_snapshot!(fixture.json(&output));
}

#[test]
fn an_error_status_is_down() {
    let fixture = fixture();
    let output = fixture.tocsin(&["probe", "run", "unavailable", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    insta::assert_json_snapshot!(fixture.json(&output));
}

#[test]
fn a_refused_connection_is_down() {
    let fixture = fixture();
    let output = fixture.tocsin(&["probe", "run", "refused", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    insta::assert_json_snapshot!(fixture.json(&output));
}

#[test]
fn an_unknown_check_is_invalid_input_with_a_suggestion() {
    let fixture = fixture();
    let output = fixture.tocsin(&["probe", "run", "statuspage-ap", "--json"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    insta::assert_json_snapshot!(fixture.json(&output));
}

#[test]
fn invalid_configuration_is_invalid_input() {
    let fixture = fixture();
    write(
        fixture.config.path(),
        "broken.toml",
        "[[check]]\nid = \"broken\"\ntype = \"http\"\ntarget = \"http://127.0.0.1/\"\n[[check.assert]]\nbody = { regex = \"(\" }\n",
    );
    let output = fixture.tocsin(&["probe", "run", "statuspage-api", "--json"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let document = fixture.json(&output);
    assert_eq!(document["error"]["kind"], "invalid_input");
    assert_eq!(
        document["error"]["errors"].as_array().map(Vec::len),
        Some(1)
    );
}

#[test]
fn json_output_keeps_logs_off_stdout() {
    let fixture = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .args(["probe", "run", "statuspage-api", "--json", "--config"])
        .arg(fixture.config.path())
        .env("TOCSIN_LOG", "trace")
        .output()
        .expect("tocsin runs");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    // Exactly one document, however much was logged.
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    assert_eq!(text.lines().count(), 1);
    serde_json::from_str::<Value>(&text).expect("one JSON document");
}
