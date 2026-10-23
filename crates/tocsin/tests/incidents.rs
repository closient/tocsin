// SPDX-License-Identifier: AGPL-3.0-only

//! The incident, release and heartbeat CLIs against a live `tocsin serve`:
//! the round trips, the exit codes HTTP failures map to, and serve answering
//! HTTP until a signal stops it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:0"

[[component]]
id = "api"
name = "API"

[[component]]
id = "web"
name = "Web"

[[project]]
id = "api"
name = "API server"
components = ["api"]

[[component]]
id = "jobs"
name = "Jobs"

[[heartbeat]]
id = "backup"
component = "jobs"
interval = "1h"

[[token]]
id = "oncall"
secret = "${ENV:ONCALL_TOKEN}"
scopes = ["incident"]

[[token]]
id = "deploys"
secret = "${ENV:DEPLOY_TOKEN}"
scopes = ["release", "heartbeat"]
"#;

const ONCALL: &str = "oncall-secret-0123456789";
const DEPLOYS: &str = "deploy-secret-0123456789";

const READY: &str = "waiting for SIGINT or SIGTERM";

/// A running `tocsin serve` on a port the system picked; killed on drop
/// so a failing test never leaves it behind.
struct Server {
    child: Child,
    address: SocketAddr,
    log: Option<JoinHandle<Vec<String>>>,
    _config: tempfile::TempDir,
    _data: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Server {
    fn start() -> Self {
        let config = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(config.path().join("tocsin.toml"), CONFIG).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_tocsin"))
            .args(["--json", "serve", "--config"])
            .arg(config.path())
            .arg("--data")
            .arg(data.path())
            .env("ONCALL_TOKEN", ONCALL)
            .env("DEPLOY_TOKEN", DEPLOYS)
            .stdout(Stdio::piped())
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
        let log = std::thread::spawn(move || lines.map_while(Result::ok).collect());
        Server {
            child,
            address,
            log: Some(log),
            _config: config,
            _data: data,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Runs the CLI against this server with `token`, `--json` first.
    fn cli(&self, token: Option<&str>, args: &[&str]) -> Output {
        cli(&self.url(), token, args)
    }

    fn stop(mut self, signal: &str) -> (std::process::ExitStatus, Value, String) {
        let sent = Command::new("kill")
            .args([signal, &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "serve did not stop within 15s");
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = read_all(self.child.stdout.take().unwrap());
        let log = self.log.take().unwrap().join().unwrap().join("\n");
        let stdout = serde_json::from_slice(&stdout)
            .unwrap_or_else(|_| panic!("stdout is one JSON document; stderr:\n{log}"));
        (status, stdout, log)
    }
}

fn read_all(mut stdout: ChildStdout) -> Vec<u8> {
    let mut bytes = Vec::new();
    stdout.read_to_end(&mut bytes).unwrap();
    bytes
}

/// The CLI with `TOCSIN_URL` and, when given, `TOCSIN_TOKEN` set only for
/// this child, so the test runner's own environment never leaks in.
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

/// Exit 0 and exactly one JSON document on stdout.
fn ok(output: &Output) -> Value {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout is a single JSON document")
}

/// The exit code and the `error` object of a failed `--json` run.
fn failed(output: &Output) -> (Option<i32>, Value) {
    let document: Value =
        serde_json::from_slice(&output.stdout).expect("stdout is a single JSON document");
    (output.status.code(), document["error"].clone())
}

/// A bare HTTP/1.1 GET; returns the status line and the body.
fn http_get(address: SocketAddr, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").expect("a full response");
    let status = head.lines().next().unwrap().to_owned();
    (status, body.to_owned())
}

#[test]
fn the_release_cli_publishes_and_stamps_deploys() {
    let server = Server::start();
    let notes = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(notes.path(), "**Faster** search.\n").unwrap();
    let notes = notes.path().to_str().unwrap();

    // Deployed before it is published: a stub, then filled in.
    let stub = ok(&server.cli(
        Some(DEPLOYS),
        &[
            "release",
            "deployed",
            "--project",
            "api",
            "--version",
            "v1.4.0",
        ],
    ));
    assert_eq!(stub["released_at"], Value::Null);
    let published = ok(&server.cli(
        Some(DEPLOYS),
        &[
            "release",
            "publish",
            "--project",
            "api",
            "--version",
            "v1.4.0",
            "--title",
            "Faster search",
            "--notes-file",
            notes,
            "--release-url",
            "https://github.com/closient/tocsin/releases/tag/v1.4.0",
        ],
    ));
    assert_eq!(published["id"], stub["id"]);
    assert_eq!(published["deployed_at"], stub["deployed_at"]);
    assert_eq!(published["notes"], "**Faster** search.\n");
    assert_eq!(published["created_by"]["id"], "deploys");

    // Publish with --deployed-at, into another environment.
    let staging = ok(&server.cli(
        Some(DEPLOYS),
        &[
            "release",
            "publish",
            "--project",
            "api",
            "--version",
            "v1.5.0",
            "--notes-file",
            notes,
            "--environment",
            "staging",
            "--deployed-at",
            "2026-10-23T12:00:00Z",
        ],
    ));
    assert_eq!(staging["environment"], "staging");
    assert_eq!(staging["deployed_at"], "2026-10-23T12:00:00.000Z");
    assert_eq!(staging["title"], "v1.5.0");

    let (status, body) = http_get(server.address, "/releases/api/v1.4.0");
    assert!(status.contains("200"), "{status}");
    assert!(body.contains("<strong>Faster</strong> search."));
}

#[test]
fn the_release_cli_maps_failures_to_exit_codes() {
    let server = Server::start();
    // A 422 naming the project, a 401, a 403, a missing file, an unsafe
    // version caught before any request, and no server at all.
    let (code, error) = failed(&server.cli(
        Some(DEPLOYS),
        &["release", "publish", "--project", "nope", "--version", "v1"],
    ));
    assert_eq!(code, Some(2));
    assert!(
        error["message"].as_str().unwrap().contains("nope"),
        "{error}"
    );
    let (code, _) = failed(&server.cli(
        Some("wrong-token-0123456789"),
        &["release", "deployed", "--project", "api", "--version", "v1"],
    ));
    assert_eq!(code, Some(2));
    let (code, _) = failed(&server.cli(
        Some(ONCALL),
        &["release", "deployed", "--project", "api", "--version", "v1"],
    ));
    assert_eq!(code, Some(2));
    let (code, error) = failed(&server.cli(
        Some(DEPLOYS),
        &[
            "release",
            "publish",
            "--project",
            "api",
            "--version",
            "v1",
            "--notes-file",
            "/nonexistent/notes.md",
        ],
    ));
    assert_eq!(
        (code, error["kind"].as_str()),
        (Some(2), Some("invalid_input"))
    );
    let (code, error) = failed(&server.cli(
        Some(DEPLOYS),
        &[
            "release",
            "deployed",
            "--project",
            "api",
            "--version",
            "../incidents",
        ],
    ));
    assert_eq!(code, Some(2));
    assert!(
        error["message"].as_str().unwrap().contains("--version"),
        "{error}"
    );
    drop(server);
    let (code, error) = failed(&cli(
        "http://127.0.0.1:9",
        Some(DEPLOYS),
        &["release", "deployed", "--project", "api", "--version", "v1"],
    ));
    assert_eq!(
        (code, error["kind"].as_str()),
        (Some(3), Some("runtime_error"))
    );
}

#[test]
fn serve_answers_http_and_exits_cleanly_on_sigterm() {
    let server = Server::start();
    let (status, body) = http_get(server.address, "/api/v1/openapi.json");
    assert_eq!(status, "HTTP/1.1 200 OK");
    let spec: Value = serde_json::from_str(&body).unwrap();
    assert!(spec["paths"]["/api/v1/incidents"].is_object(), "{spec}");

    let (status, body) = http_get(server.address, "/api/v1/incidents");
    assert_eq!(status, "HTTP/1.1 200 OK");
    let list: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(list["incidents"], serde_json::json!([]));

    let (exit, stdout, log) = server.stop("-TERM");
    assert_eq!(exit.code(), Some(0), "stderr:\n{log}");
    assert_eq!(
        stdout,
        serde_json::json!({ "status": "stopped", "signal": "SIGTERM" })
    );
    assert!(log.contains("shutting down"), "{log}");
    assert!(!log.contains("still in use"), "{log}");
}

#[test]
fn the_cli_round_trips_against_a_live_server() {
    let server = Server::start();
    let oncall = Some(ONCALL);

    let opened = ok(&server.cli(
        oncall,
        &[
            "incident",
            "new",
            "--title",
            "API errors",
            "--impact",
            "major",
            "--component",
            "web",
            "--component",
            "api",
            "--message",
            "Looking into **elevated** errors.",
        ],
    ));
    let id = opened["id"].as_i64().unwrap();
    assert_eq!(opened["kind"], "incident");
    assert_eq!(opened["status"], "investigating");
    assert_eq!(opened["impact"], "major");
    assert_eq!(opened["components"], serde_json::json!(["api", "web"]));
    let author = serde_json::json!({ "kind": "token", "id": "oncall" });
    assert_eq!(opened["created_by"], author);
    assert_eq!(opened["updates"][0]["created_by"], author);

    let id_arg = id.to_string();
    let updated = ok(&server.cli(
        oncall,
        &[
            "incident",
            "update",
            &id_arg,
            "--status",
            "resolved",
            "--message",
            "Fixed.",
        ],
    ));
    assert_eq!(updated["status"], "resolved");
    assert_eq!(updated["updates"].as_array().unwrap().len(), 2);
    assert!(updated["resolved_at"].is_string());

    let edited = ok(&server.cli(
        oncall,
        &[
            "incident",
            "edit",
            &id_arg,
            "--title",
            "API errors in eu-west",
            "--component",
            "api",
        ],
    ));
    assert_eq!(edited["title"], "API errors in eu-west");
    assert_eq!(edited["components"], serde_json::json!(["api"]));
    assert_eq!(edited["updated_by"], author);
    assert_eq!(
        edited["updates"], updated["updates"],
        "an edit adds no update"
    );

    let scheduled = schedule_maintenance(&server);

    // Reads need no token.
    let shown = ok(&server.cli(None, &["incident", "show", &id_arg]));
    assert_eq!(shown, edited);
    let list = ok(&server.cli(None, &["incident", "list"]));
    let ids: Vec<i64> = list["incidents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|incident| incident["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [scheduled["id"].as_i64().unwrap(), id]);
    let page = ok(&server.cli(None, &["incident", "list", "--limit", "1"]));
    assert_eq!(page["next_before"], scheduled["id"]);

    exports_round_trip(&server, id, scheduled["id"].as_i64().unwrap());

    // Without --json, list is a table; with it, one document (checked by
    // every `ok` above).
    let text = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .args(["incident", "list", "--url", &server.url()])
        .env_remove("TOCSIN_URL")
        .output()
        .unwrap();
    assert_eq!(text.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&text.stdout).contains("API errors in eu-west"));

    let (exit, _, log) = server.stop("-TERM");
    assert_eq!(exit.code(), Some(0), "stderr:\n{log}");
}

/// `maintenance schedule`, announced ahead with the default title.
fn schedule_maintenance(server: &Server) -> Value {
    let scheduled = ok(&server.cli(
        Some(ONCALL),
        &[
            "maintenance",
            "schedule",
            "--start",
            "2030-01-01T02:00:00Z",
            "--end",
            "2030-01-01T03:00:00Z",
            "--component",
            "web",
            "--message",
            "Upgrading the database.",
        ],
    ));
    assert_eq!(scheduled["kind"], "maintenance");
    assert_eq!(scheduled["title"], "Scheduled maintenance");
    assert_eq!(scheduled["status"], "scheduled");
    assert_eq!(scheduled["scheduled_start"], "2030-01-01T02:00:00.000Z");
    scheduled
}

/// One incident printed, then every incident written to a folder.
fn exports_round_trip(server: &Server, id: i64, scheduled: i64) {
    let id_arg = id.to_string();
    let exported = ok(&server.cli(None, &["incident", "export", &id_arg, "--format", "md"]));
    let markdown = exported["markdown"].as_str().unwrap();
    assert!(markdown.starts_with(&format!("---\nid: {id}\nkind: \"incident\"\n")));
    assert!(
        markdown.contains("\ncreated_by: \"token:oncall\"\n"),
        "{markdown}"
    );
    assert!(
        markdown.contains("\n# API errors in eu-west\n"),
        "{markdown}"
    );
    assert!(markdown.contains("Looking into **elevated** errors."));
    assert!(markdown.contains(" — resolved\n\n_by token:oncall_\n\nFixed.\n"));

    let out = tempfile::tempdir().unwrap();
    let out_dir = out.path().join("incidents");
    let all = ok(&server.cli(
        None,
        &[
            "incident",
            "export",
            "--all",
            "--out",
            out_dir.to_str().unwrap(),
        ],
    ));
    assert_eq!(all["files"].as_array().unwrap().len(), 2);
    let file = std::fs::read_to_string(out_dir.join(format!("incident-{id}.md"))).unwrap();
    assert_eq!(file, markdown, "--out writes what stdout prints");
    assert!(file_exists(&out_dir, &format!("incident-{scheduled}.md")));
}

fn file_exists(folder: &Path, name: &str) -> bool {
    folder.join(name).is_file()
}

#[test]
fn http_failures_map_to_exit_codes() {
    let server = Server::start();
    let new = [
        "incident",
        "new",
        "--title",
        "x",
        "--component",
        "api",
        "--message",
        "x",
    ];

    // No token at all: refused before any request.
    let (code, error) = failed(&server.cli(None, &new));
    assert_eq!(code, Some(2));
    assert!(
        error["message"].as_str().unwrap().contains("TOCSIN_TOKEN"),
        "{error}"
    );

    // An unknown token is 401, exit 2; the secret is never echoed.
    let output = server.cli(Some("not-a-configured-token"), &new);
    let (code, error) = failed(&output);
    assert_eq!(code, Some(2));
    assert!(
        error["message"].as_str().unwrap().contains("401"),
        "{error}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("not-a-configured-token"));

    // A known token without the incident scope is 403, exit 2.
    let (code, error) = failed(&server.cli(Some(DEPLOYS), &new));
    assert_eq!(code, Some(2));
    assert!(
        error["message"].as_str().unwrap().contains("403"),
        "{error}"
    );
    assert!(
        error["message"].as_str().unwrap().contains("deploys"),
        "{error}"
    );

    // `--token` overrides `TOCSIN_TOKEN`.
    let mut with_flag = new.to_vec();
    with_flag.extend(["--token", ONCALL]);
    ok(&server.cli(Some(DEPLOYS), &with_flag));

    // An unknown component is 422 with the field path, exit 2.
    let (code, error) = failed(&server.cli(
        Some(ONCALL),
        &[
            "incident",
            "new",
            "--title",
            "x",
            "--component",
            "api",
            "--component",
            "billing",
            "--message",
            "x",
        ],
    ));
    assert_eq!(code, Some(2));
    assert!(
        error["message"].as_str().unwrap().contains("422"),
        "{error}"
    );
    assert!(
        error.to_string().contains("components[1]"),
        "the field path reaches the user: {error}"
    );

    // An unknown id is 404, exit 2.
    let (code, error) = failed(&server.cli(None, &["incident", "show", "999"]));
    assert_eq!(code, Some(2));
    assert!(
        error["message"].as_str().unwrap().contains("404"),
        "{error}"
    );

    // Nobody listening is a runtime error, exit 3.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let (code, error) = failed(&cli(
        &format!("http://{closed}"),
        None,
        &["incident", "list"],
    ));
    assert_eq!(code, Some(3), "{error}");
    assert_eq!(error["kind"], "runtime_error");

    let (exit, _, log) = server.stop("-TERM");
    assert_eq!(exit.code(), Some(0), "stderr:\n{log}");
    assert!(!log.contains(ONCALL), "the server never logs a secret");
    assert!(!log.contains("not-a-configured-token"));
}

#[test]
fn the_committed_openapi_document_is_current() {
    let output = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .arg("openapi")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let committed = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/openapi.json"),
    )
    .expect("schema/openapi.json exists");
    assert!(
        String::from_utf8_lossy(&output.stdout) == committed,
        "schema/openapi.json is stale; regenerate it with\n  cargo run -- openapi > schema/openapi.json"
    );
}

#[test]
fn the_heartbeat_cli_pings_a_live_server() {
    let server = Server::start();

    let receipt = ok(&server.cli(Some(DEPLOYS), &["heartbeat", "ping", "backup"]));
    assert_eq!(receipt["id"], "backup");
    assert_eq!(receipt["status"], "up");
    assert!(receipt["received_at"].is_string(), "{receipt}");

    // A message with characters that mean something in a query arrives
    // whole; reporting down is still a successful ping.
    let receipt = ok(&server.cli(
        Some(DEPLOYS),
        &[
            "heartbeat",
            "ping",
            "backup",
            "--status",
            "down",
            "--message",
            "disk full & token=x?",
        ],
    ));
    assert_eq!(receipt["status"], "down");

    for (token, args, status) in [
        (Some("not-a-configured-token"), vec!["backup"], "401"),
        (Some(ONCALL), vec!["backup"], "403"),
        (Some(DEPLOYS), vec!["nonesuch"], "404"),
    ] {
        let args: Vec<&str> = [vec!["heartbeat", "ping"], args].concat();
        let (code, error) = failed(&server.cli(token, &args));
        assert_eq!(code, Some(2), "{args:?}: {error}");
        assert!(
            error["message"].as_str().unwrap().contains(status),
            "{args:?}: {error}"
        );
    }
    // An id that is not one is refused before any request: the URL here
    // has nothing listening, so a request would fail with exit 3.
    for id in ["..", "a/b", "a?b", "Backup", ""] {
        let (code, error) = failed(&cli(
            "http://127.0.0.1:9",
            Some(DEPLOYS),
            &["heartbeat", "ping", id],
        ));
        assert_eq!(code, Some(2), "{id:?}: {error}");
        assert!(
            error["message"].as_str().unwrap().contains("invalid id"),
            "{id:?}: {error}"
        );
    }
    let long = "x".repeat(1025);
    let (code, error) = failed(&server.cli(
        Some(DEPLOYS),
        &["heartbeat", "ping", "backup", "--message", &long],
    ));
    assert_eq!(code, Some(2));
    assert!(
        error["message"].as_str().unwrap().contains("422"),
        "{error}"
    );

    let (code, error) = failed(&server.cli(None, &["heartbeat", "ping", "backup"]));
    assert_eq!(code, Some(2));
    assert!(
        error["message"].as_str().unwrap().contains("TOCSIN_TOKEN"),
        "{error}"
    );
}
