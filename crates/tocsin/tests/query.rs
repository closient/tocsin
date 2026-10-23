// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin status`, `history`, `outages` and `timeline` against a live
//! `tocsin serve` whose database was seeded before it started: one JSON
//! document each, what a token adds, and the exit codes failures map to.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tocsin_core::Health::{self, Degraded, Down, Up};
use tocsin_core::incident::Timestamp;
use tocsin_store::{NewCheckResult, NewOutage, Store, SubjectKind};

const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:0"
node_id = "node-a"

[page]
title = "Example status"

[[component]]
id = "api"
name = "API"

[[component]]
id = "jobs"
name = "Jobs"

[[check]]
id = "api-https"
component = "api"
type = "http"
target = "http://127.0.0.1:9/health"
interval = "1d"

[[heartbeat]]
id = "backup"
component = "jobs"
interval = "1d"

[[project]]
id = "site"
name = "Site"
components = ["api"]

[[project]]
id = "secret"
name = "Secret"
public = false
components = ["api"]

[[token]]
id = "oncall"
secret = "${ENV:ONCALL_TOKEN}"
scopes = ["incident"]

[[token]]
id = "deploys"
secret = "${ENV:DEPLOY_TOKEN}"
scopes = ["release"]

[[token]]
id = "reader"
secret = "${ENV:READER_TOKEN}"
scopes = ["status:read"]
"#;

const ONCALL: &str = "oncall-secret-0123456789";
const DEPLOYS: &str = "deploy-secret-0123456789";
const READER: &str = "reader-secret-0123456789";

const READY: &str = "waiting for SIGINT or SIGTERM";
const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// A running `tocsin serve`, killed on drop.
struct Server {
    child: Child,
    address: SocketAddr,
    _config: tempfile::TempDir,
    _data: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// Seeds the database with [`seed`], then starts serving it.
    fn start() -> Self {
        // The check must not run while the test reads: its first run is
        // its jitter after the start.
        let jitter = tocsin_core::state::jitter("api-https", Duration::from_hours(24));
        assert!(jitter > Duration::from_mins(10), "{jitter:?}");

        let config = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(config.path().join("tocsin.toml"), CONFIG).unwrap();
        seed(&data.path().join(tocsin_store::DB_FILE));
        let mut child = Command::new(env!("CARGO_BIN_EXE_tocsin"))
            .args(["--json", "serve", "--config"])
            .arg(config.path())
            .arg("--data")
            .arg(data.path())
            .env("ONCALL_TOKEN", ONCALL)
            .env("DEPLOY_TOKEN", DEPLOYS)
            .env("READER_TOKEN", READER)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("tocsin serve starts");
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let ready = lines
            .by_ref()
            .map_while(Result::ok)
            .find(|line| line.contains(READY))
            .expect("serve exited before becoming ready");
        let address = ready
            .split_whitespace()
            .find_map(|field| field.strip_prefix("listen="))
            .expect("the ready line names the address")
            .parse()
            .unwrap();
        // Keep draining stderr so the server never writes into a full pipe.
        std::thread::spawn(move || lines.map_while(Result::ok).for_each(drop));
        Server {
            child,
            address,
            _config: config,
            _data: data,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The CLI with `--json`, `TOCSIN_URL` and only the given token.
    fn cli(&self, token: Option<&str>, args: &[&str]) -> Output {
        cli(&self.url(), token, args)
    }

    /// One `--json` run: its exit code and its document, with times,
    /// cursors and this server's address replaced so it snapshots.
    fn run(&self, token: Option<&str>, args: &[&str]) -> (Option<i32>, Value) {
        let output = self.cli(token, args);
        let mut document: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "stdout is one JSON document\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        redact(&mut document, &self.url());
        (output.status.code(), document)
    }
}

fn cli(url: &str, token: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tocsin"));
    command
        .arg("--json")
        .args(args)
        .env("TOCSIN_URL", url)
        .env_remove("TOCSIN_TOKEN");
    if let Some(token) = token {
        command.env("TOCSIN_TOKEN", token);
    }
    command.output().expect("tocsin runs")
}

fn redact(value: &mut Value, url: &str) {
    match value {
        Value::String(text) => {
            if text.parse::<Timestamp>().is_ok() {
                "[time]".clone_into(text);
            } else {
                *text = text.replace(url, "[url]");
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redact(item, url)),
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if key == "next_after" && field.is_string() {
                    *field = Value::from("[cursor]");
                } else {
                    redact(field, url);
                }
            }
        }
        _ => {}
    }
}

fn now_ms() -> i64 {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(since_epoch.as_millis()).unwrap()
}

/// Results for `api-https` 3h, 90, 30, 20 and 10 minutes ago, and four
/// outages: one 10 days ago, one from 8 to 6 days ago, one 2 days ago, and
/// one open since 20 minutes ago, which the engine resumes so the API
/// starts down.
fn seed(path: &Path) {
    fn result(started_at_ms: i64, health: Health) -> NewCheckResult {
        NewCheckResult {
            node_id: "node-a".into(),
            check_id: "api-https".into(),
            started_at_ms,
            duration_ms: 40,
            health,
            detail: None,
            error: (health != Up).then(|| "status 503".to_owned()),
            status_code: Some(if health == Up { 200 } else { 503 }),
        }
    }
    fn outage(started_at_ms: i64, health: Health) -> NewOutage {
        NewOutage {
            node_id: "node-a".into(),
            subject_kind: SubjectKind::Check,
            check_id: "api-https".into(),
            component_id: Some("api".into()),
            health,
            started_at_ms,
            alerted: true,
        }
    }
    let now = now_ms();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::open(path).unwrap();
            for (ago, health) in [
                (3 * HOUR, Up),
                (90 * MINUTE, Up),
                (30 * MINUTE, Degraded),
                (20 * MINUTE, Down),
                (10 * MINUTE, Down),
            ] {
                store.append(result(now - ago, health)).await.unwrap();
            }
            let old = store
                .open_outage(outage(now - 10 * DAY, Down))
                .await
                .unwrap();
            store.end_outage(old, now - 10 * DAY + HOUR).await.unwrap();
            let straddles = store
                .open_outage(outage(now - 8 * DAY, Down))
                .await
                .unwrap();
            store.end_outage(straddles, now - 6 * DAY).await.unwrap();
            let recent = store
                .open_outage(outage(now - 2 * DAY, Degraded))
                .await
                .unwrap();
            store
                .end_outage(recent, now - 2 * DAY + 15 * MINUTE)
                .await
                .unwrap();
            store
                .open_outage(outage(now - 20 * MINUTE, Down))
                .await
                .unwrap();
            store.close().await.unwrap();
        });
}

/// Opens an incident on the API, then stamps a deploy of the public
/// project and of the private one, in that order.
fn record_events(server: &Server) {
    let (code, opened) = server.run(
        Some(ONCALL),
        &[
            "incident",
            "new",
            "--title",
            "API errors",
            "--impact",
            "major",
            "--component",
            "api",
            "--message",
            "Looking into it.",
        ],
    );
    assert_eq!(code, Some(0), "{opened}");
    for project in ["site", "secret"] {
        let (code, deployed) = server.run(
            Some(DEPLOYS),
            &[
                "release",
                "deployed",
                "--project",
                project,
                "--version",
                "v1.2.0",
            ],
        );
        assert_eq!(code, Some(0), "{deployed}");
    }
}

#[test]
fn status_reports_the_resumed_outage_and_exits_one() {
    let server = Server::start();
    record_events(&server);
    let (code, document) = server.run(None, &["status"]);
    assert_eq!(code, Some(1), "the API is down: {document}");
    insta::assert_json_snapshot!(document);

    // Text names the same thing for a person.
    let output = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .arg("status")
        .env("TOCSIN_URL", server.url())
        .env_remove("TOCSIN_TOKEN")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("Example status: down as of "), "{text}");
    assert!(text.contains("API errors"), "{text}");
}

#[test]
fn history_pages_a_checks_results_for_a_reader() {
    let server = Server::start();
    let (code, first) = server.run(
        Some(READER),
        &["history", "api-https", "--since", "2h", "--limit", "2"],
    );
    assert_eq!(code, Some(0), "{first}");
    insta::assert_json_snapshot!("history_first_page", first);

    let before = first["next_before"]
        .as_i64()
        .expect("a second page")
        .to_string();
    let (code, second) = server.run(
        Some(READER),
        &[
            "history",
            "api-https",
            "--since",
            "2h",
            "--limit",
            "2",
            "--before",
            &before,
        ],
    );
    assert_eq!(code, Some(0), "{second}");
    insta::assert_json_snapshot!("history_last_page", second);
}

#[test]
fn history_needs_a_reader_and_a_known_check() {
    let server = Server::start();
    let (code, error) = server.run(None, &["history", "api-https"]);
    assert_eq!(code, Some(2), "{error}");
    insta::assert_json_snapshot!("history_without_a_token", error);

    let (code, error) = server.run(Some(ONCALL), &["history", "api-https"]);
    assert_eq!(code, Some(2), "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("403 Forbidden"),
        "{error}"
    );

    let (code, error) = server.run(Some(READER), &["history", "nope"]);
    assert_eq!(code, Some(2), "{error}");
    insta::assert_json_snapshot!("history_of_an_unknown_check", error);

    let (code, error) = server.run(Some(READER), &["history", "../outages"]);
    assert_eq!(code, Some(2), "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("check \"../outages\""),
        "never sent: {error}"
    );
}

#[test]
fn outages_overlap_the_window_without_a_token() {
    let server = Server::start();
    let (code, list) = server.run(None, &["outages", "--since", "7d"]);
    assert_eq!(code, Some(0), "{list}");
    insta::assert_json_snapshot!(list);
    assert_eq!(
        list["outages"].as_array().unwrap().len(),
        3,
        "the one 8 days ago that ended 6 days ago is in, the one 10 days ago out: {list}"
    );
}

#[test]
fn timeline_shows_private_deploys_only_to_a_reader() {
    let server = Server::start();
    record_events(&server);
    let args = ["timeline", "--component", "api", "--since", "1d"];

    let (code, public) = server.run(None, &args);
    assert_eq!(code, Some(0), "{public}");
    assert!(!public.to_string().contains("secret"), "{public}");
    insta::assert_json_snapshot!("timeline_public", public);

    let (code, trusted) = server.run(Some(READER), &args);
    assert_eq!(code, Some(0), "{trusted}");
    insta::assert_json_snapshot!("timeline_reader", trusted);

    // A write-only TOCSIN_TOKEN reads the public view rather than failing.
    let (code, written) = server.run(Some(DEPLOYS), &args);
    assert_eq!(code, Some(0), "{written}");
    assert_eq!(written, public);
}

#[test]
fn bad_windows_are_invalid_input() {
    let server = Server::start();
    let (code, error) = server.run(Some(READER), &["history", "api-https", "--limit", "0"]);
    assert_eq!(code, Some(2), "{error}");
    insta::assert_json_snapshot!("history_limit_zero", error);

    let (code, error) = server.run(None, &["outages", "--since", "400d"]);
    assert_eq!(code, Some(2), "{error}");
    assert_eq!(error["error"]["errors"][0]["path"], "since", "{error}");

    let (code, error) = server.run(None, &["timeline", "--limit", "501"]);
    assert_eq!(code, Some(2), "{error}");
    assert_eq!(error["error"]["errors"][0]["path"], "limit", "{error}");

    let (code, error) = server.run(None, &["outages", "--since", "yesterday"]);
    assert_eq!(code, Some(2), "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("such as 2h or 7d"),
        "{error}"
    );
}

#[test]
fn an_unreachable_server_is_a_runtime_failure() {
    // Bind and drop: nothing listens on the port afterwards.
    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    for command in ["status", "outages", "timeline"] {
        let output = cli(&format!("http://{address}"), None, &[command]);
        assert_eq!(output.status.code(), Some(3), "{command}");
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(document["error"]["kind"], "runtime_error", "{command}");
    }
}

/// A server that answers every request with `status` and `body`, for
/// answers a real tocsin never gives.
fn canned(status: &'static str, body: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            // The CLI sends a GET with no body: read to the blank line.
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                request.push(byte[0]);
            }
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{address}")
}

#[test]
fn json_is_the_answer_as_it_came_and_text_prints_what_it_does_not_know() {
    let status_document = r#"{"title": "Example status", "status": "partial",
        "generated_at": "2026-10-23T14:05:00.000Z", "region": "eu-west",
        "components": [{"id": "api", "name": "API", "description": null,
            "group": null, "status": "frozen", "health": null, "uptime": 0.99}],
        "incidents": []}"#;
    let url = canned("200 OK", status_document);
    let output = cli(&url, None, &["status"]);
    assert_eq!(output.status.code(), Some(0), "not degraded or down");
    let echoed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        echoed,
        serde_json::from_str::<Value>(status_document).unwrap()
    );

    let output = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .arg("status")
        .env("TOCSIN_URL", &url)
        .env_remove("TOCSIN_TOKEN")
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.starts_with("Example status: partial as of "), "{text}");
    assert!(text.contains("frozen"), "{text}");

    let timeline = r#"{"since": "2026-10-23T14:05:00.000Z", "next_after": null,
        "events": [{"kind": "audit", "at": "2026-10-23T14:06:00.000Z", "who": "ops"},
            {"kind": "outage", "at": "2026-10-23T14:05:00.000Z", "id": 1,
             "node_id": "node-a", "check_id": "api-https", "subject_kind": "probe",
             "component_id": null, "health": "flapping", "ended_at": null}]}"#;
    let url = canned("200 OK", timeline);
    let output = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .arg("timeline")
        .env("TOCSIN_URL", &url)
        .env_remove("TOCSIN_TOKEN")
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(
        text.contains("2026-10-23T14:06:00.000Z  audit\n"),
        "an unknown kind by name: {text}"
    );
    assert!(
        text.contains("outage    api-https flapping, still open"),
        "{text}"
    );
}

#[test]
fn a_server_error_or_an_unexpected_answer_is_a_runtime_failure() {
    for (status, body) in [
        (
            "503 Service Unavailable",
            r#"{"error": "unavailable", "message": "busy"}"#,
        ),
        ("500 Internal Server Error", "<html>oops</html>"),
        ("200 OK", "<html>a captive portal</html>"),
        ("200 OK", r#"{"title": 7}"#),
    ] {
        let url = canned(status, body);
        for command in ["status", "outages", "timeline"] {
            let output = cli(&url, None, &[command]);
            let document: Value = serde_json::from_slice(&output.stdout)
                .unwrap_or_else(|error| panic!("{status} {body} {command}: {error}"));
            assert_eq!(
                output.status.code(),
                Some(3),
                "{status} {body} {command}: {document}"
            );
            assert_eq!(
                document["error"]["kind"], "runtime_error",
                "{status} {body} {command}"
            );
        }
    }
}
