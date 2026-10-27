// SPDX-License-Identifier: AGPL-3.0-only

//! The MCP tools against a live `tocsin serve`, over `/mcp` and over
//! `tocsin mcp` on stdio: every tool answers as its REST route does, a
//! request without a known token never reaches MCP, write tools refuse a
//! token without their scope and write nothing, list tools hold a limit to
//! the REST page, and an open MCP client does not slow shutdown.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rmcp::model::{CallToolRequestParams, CallToolResult, ReadResourceRequestParams};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{Peer, ServiceExt};
use serde_json::{Value, json};
use tocsin_core::Health::{self, Down, Up};
use tocsin_core::incident::Timestamp;
use tocsin_store::{NewCheckResult, NewOutage, Store, SubjectKind};

#[allow(dead_code)]
mod support;

const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:0"
node_id = "node-a"

[page]
title = "Example status"

[[component]]
id = "api"
name = "API"

[[check]]
id = "api-https"
component = "api"
type = "http"
target = "http://127.0.0.1:9/health"
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
id = "agent"
secret = "${ENV:AGENT_TOKEN}"
scopes = ["status:read", "incident", "release", "silence"]

[[token]]
id = "oncall"
secret = "${ENV:ONCALL_TOKEN}"
scopes = ["incident"]

[[token]]
id = "reader"
secret = "${ENV:READER_TOKEN}"
scopes = ["status:read"]

[[token]]
id = "drafter"
secret = "${ENV:DRAFTER_TOKEN}"
scopes = ["incident:draft"]
"#;

const AGENT: &str = "agent-secret-0123456789";
const ONCALL: &str = "oncall-secret-0123456789";
const READER: &str = "reader-secret-0123456789";
const DRAFTER: &str = "drafter-secret-0123456789";

const READY: &str = "waiting for SIGINT or SIGTERM";
const MINUTE: i64 = 60_000;
const DAY: i64 = 24 * 60 * MINUTE;

/// Every tool `/mcp` lists; stdio adds `validate_config`.
const TOOLS: [&str; 16] = [
    "create_incident",
    "create_silence",
    "discard_incident",
    "expire_silence",
    "explain_check",
    "get_check_history",
    "get_status",
    "get_timeline",
    "list_alert_history",
    "list_incidents",
    "list_outages",
    "list_releases",
    "list_silences",
    "mark_deployed",
    "publish_release",
    "update_incident",
];

/// A running `tocsin serve`, killed on drop.
struct Node {
    child: Child,
    address: SocketAddr,
    _config: tempfile::TempDir,
    _data: tempfile::TempDir,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Node {
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
            .env("AGENT_TOKEN", AGENT)
            .env("ONCALL_TOKEN", ONCALL)
            .env("READER_TOKEN", READER)
            .env("DRAFTER_TOKEN", DRAFTER)
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
        Node {
            child,
            address,
            _config: config,
            _data: data,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// An MCP client on `/mcp`, sending `token`.
    async fn http(&self, token: &str) -> RunningService<RoleClient, ()> {
        let config = StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", self.url()))
            .auth_header(token);
        ().serve(StreamableHttpClientTransport::with_client(
            http_client(),
            config,
        ))
        .await
        .expect("an MCP session on /mcp")
    }

    /// An MCP client on `tocsin mcp`, pointed at this node with `token`.
    async fn stdio(&self, token: &str) -> RunningService<RoleClient, ()> {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tocsin"));
        command
            .arg("mcp")
            .env("TOCSIN_URL", self.url())
            .env("TOCSIN_TOKEN", token)
            .stderr(Stdio::inherit());
        let transport = TokioChildProcess::new(command).expect("tocsin mcp starts");
        ().serve(transport).await.expect("an MCP session on stdio")
    }

    /// `GET path` on the REST API with `token`.
    async fn rest(&self, token: &str, path: &str) -> Value {
        let response = http_client()
            .get(format!("{}{path}", self.url()))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "GET {path}: {response:?}");
        response.json().await.unwrap()
    }

    /// Sends SIGTERM and waits up to 15s for the exit, returning how long
    /// it took.
    fn terminate(&mut self) -> (std::process::ExitStatus, Duration) {
        let started = Instant::now();
        let sent = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return (status, started.elapsed());
            }
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "serve did not stop within 15s"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn http_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

fn now_ms() -> i64 {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(since_epoch.as_millis()).unwrap()
}

/// Three results for `api-https`, the last one down, and the outage it
/// opened 10 minutes ago, plus an older outage that ended.
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
    fn outage(started_at_ms: i64) -> NewOutage {
        NewOutage {
            node_id: "node-a".into(),
            subject_kind: SubjectKind::Check,
            check_id: "api-https".into(),
            component_id: Some("api".into()),
            health: Down,
            started_at_ms,
            alerted: true,
        }
    }
    let now = now_ms();
    // On a thread of its own: the tests already run inside a runtime.
    std::thread::scope(|scope| {
        scope.spawn(|| seed_with(path, now, result, outage));
    });
}

fn seed_with(
    path: &Path,
    now: i64,
    result: fn(i64, Health) -> NewCheckResult,
    outage: fn(i64) -> NewOutage,
) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let store = Store::open(path).unwrap();
            for (ago, health) in [(30 * MINUTE, Up), (20 * MINUTE, Up), (10 * MINUTE, Down)] {
                store.append(result(now - ago, health)).await.unwrap();
            }
            let old = store.open_outage(outage(now - 2 * DAY)).await.unwrap();
            store.end_outage(old, now - 2 * DAY + MINUTE).await.unwrap();
            store.open_outage(outage(now - 10 * MINUTE)).await.unwrap();
            store.close().await.unwrap();
        });
}

async fn call(peer: &Peer<RoleClient>, name: &'static str, arguments: Value) -> CallToolResult {
    let Value::Object(arguments) = arguments else {
        panic!("arguments are an object");
    };
    peer.call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await
        .unwrap_or_else(|error| panic!("{name}: {error}"))
}

/// The structured result of a call that worked.
async fn ok(peer: &Peer<RoleClient>, name: &'static str, arguments: Value) -> Value {
    let result = call(peer, name, arguments).await;
    assert_ne!(result.is_error, Some(true), "{name}: {result:?}");
    result.structured_content.expect("a structured result")
}

/// The error message of a call that failed as a tool error.
async fn refused(peer: &Peer<RoleClient>, name: &'static str, arguments: Value) -> String {
    let result = call(peer, name, arguments).await;
    assert_eq!(result.is_error, Some(true), "{name}: {result:?}");
    let error = result.structured_content.expect("a structured error");
    error["message"].as_str().expect("a message").to_owned()
}

async fn tool_names(peer: &Peer<RoleClient>) -> BTreeSet<String> {
    let tools = peer.list_all_tools().await.unwrap();
    for tool in &tools {
        assert_eq!(
            tool.input_schema.get("type"),
            Some(&json!("object")),
            "{} has an input schema",
            tool.name
        );
        assert!(tool.description.is_some(), "{} is described", tool.name);
    }
    tools
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect()
}

/// Reads both resources and checks they are the documents `tocsin config
/// schema` and `tocsin openapi` print.
async fn reads_resources(peer: &Peer<RoleClient>) {
    let resources = peer.list_all_resources().await.unwrap();
    let uris: BTreeSet<_> = resources
        .iter()
        .map(|resource| resource.uri.as_str())
        .collect();
    assert_eq!(
        uris,
        BTreeSet::from(["tocsin://schema/config", "tocsin://schema/openapi"])
    );
    for (uri, args) in [
        ("tocsin://schema/config", &["config", "schema"][..]),
        ("tocsin://schema/openapi", &["openapi"][..]),
    ] {
        let read = peer
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .unwrap();
        let text = match &read.contents[..] {
            [rmcp::model::ResourceContents::TextResourceContents { text, .. }] => text,
            other => panic!("{uri}: {other:?}"),
        };
        let printed = Command::new(env!("CARGO_BIN_EXE_tocsin"))
            .args(args)
            .output()
            .unwrap();
        let printed: Value = serde_json::from_slice(&printed.stdout).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            printed,
            "{uri}"
        );
    }
}

/// Calls every tool but `validate_config` with the agent token, checking
/// reads against the REST route they share and that each write landed.
#[allow(clippy::too_many_lines)] // one pass over every tool, in order
async fn drives_every_tool(peer: &Peer<RoleClient>, node: &Node) {
    let week_ago = Timestamp::from_millis(now_ms() - 7 * DAY).to_string();

    let status = ok(peer, "get_status", json!({})).await;
    assert_eq!(status["components"][0]["health"], "down", "{status}");

    let outages = ok(peer, "list_outages", json!({ "since": week_ago })).await;
    let rest = node
        .rest(AGENT, &format!("/api/v1/outages?since={week_ago}"))
        .await;
    assert_eq!(outages, rest);
    assert_eq!(outages["outages"].as_array().unwrap().len(), 2, "{outages}");
    let first = ok(
        peer,
        "list_outages",
        json!({ "since": week_ago, "limit": 1 }),
    )
    .await;
    assert_eq!(first["outages"].as_array().unwrap().len(), 1, "{first}");
    assert!(first["next_before"].is_i64(), "{first}");

    let alerts = ok(peer, "list_alert_history", json!({ "since": week_ago })).await;
    let rest = node
        .rest(AGENT, &format!("/api/v1/alerts/history?since={week_ago}"))
        .await;
    assert_eq!(alerts, rest);

    let history = ok(
        peer,
        "get_check_history",
        json!({ "check": "api-https", "since": week_ago }),
    )
    .await;
    let rest = node
        .rest(
            AGENT,
            &format!("/api/v1/checks/api-https/results?since={week_ago}"),
        )
        .await;
    assert_eq!(history, rest);
    assert_eq!(history["results"].as_array().unwrap().len(), 3, "{history}");

    let explained = ok(peer, "explain_check", json!({ "check": "api-https" })).await;
    assert_eq!(explained["check_id"], "api-https");
    assert_eq!(explained["latest"], history["results"][0], "{explained}");
    let unknown = refused(peer, "explain_check", json!({ "check": "nope" })).await;
    assert!(unknown.contains("nope"), "{unknown}");

    let opened = ok(
        peer,
        "create_incident",
        json!({
            "title": "API errors",
            "impact": "major",
            "components": ["api"],
            "message": "Looking into it.",
        }),
    )
    .await;
    let id = opened["id"].as_i64().expect("an incident id");
    assert_eq!(opened["status"], "investigating");
    assert_eq!(opened["created_by"]["id"], "agent", "{opened}");
    // An agent drafts unless told otherwise, even holding `incident`.
    assert!(opened["published_at"].is_null(), "{opened}");
    let public = http_client()
        .get(format!("{}/api/v1/incidents/{id}", node.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(public.status(), reqwest::StatusCode::NOT_FOUND);

    let updated = ok(
        peer,
        "update_incident",
        json!({ "id": id, "status": "identified", "message": "Found it." }),
    )
    .await;
    assert_eq!(updated["status"], "identified", "{updated}");
    assert_eq!(updated["updates"].as_array().unwrap().len(), 2, "{updated}");

    let incidents = ok(peer, "list_incidents", json!({})).await;
    assert_eq!(incidents, node.rest(AGENT, "/api/v1/incidents").await);
    assert_eq!(incidents["incidents"][0]["id"], id);

    // With `status:read` the agent reads back its note and key, a second
    // create with the key answers the first, and the key finds it.
    let keyed = json!({
        "title": "Scratch",
        "message": "Never mind.",
        "internal_note": "Evidence: db-01 swap.",
        "external_key": "scratch-1",
    });
    let scratch = ok(peer, "create_incident", keyed.clone()).await;
    assert_eq!(
        scratch["internal_note"], "Evidence: db-01 swap.",
        "{scratch}"
    );
    assert_eq!(scratch["external_key"], "scratch-1", "{scratch}");
    let again = ok(peer, "create_incident", keyed).await;
    assert_eq!(again, scratch);
    let found = ok(
        peer,
        "list_incidents",
        json!({ "external_key": "scratch-1" }),
    )
    .await;
    assert_eq!(
        found,
        node.rest(AGENT, "/api/v1/incidents?external_key=scratch-1&limit=20")
            .await
    );
    assert_eq!(found["incidents"].as_array().unwrap().len(), 1, "{found}");
    assert_eq!(found["incidents"][0]["id"], scratch["id"], "{found}");
    let discarded = ok(peer, "discard_incident", json!({ "id": scratch["id"] })).await;
    assert_eq!(discarded["title"], "Scratch", "{discarded}");
    let gone = refused(peer, "discard_incident", json!({ "id": scratch["id"] })).await;
    assert!(gone.contains("no incident"), "{gone}");

    let published = ok(
        peer,
        "publish_release",
        json!({ "project": "secret", "version": "v1.2.0", "notes": "Faster." }),
    )
    .await;
    assert_eq!(published["version"], "v1.2.0");
    assert!(published["deployed_at"].is_null(), "{published}");

    let deployed = ok(
        peer,
        "mark_deployed",
        json!({ "project": "secret", "version": "v1.2.0" }),
    )
    .await;
    assert_eq!(deployed["id"], published["id"]);
    assert!(deployed["deployed_at"].is_string(), "{deployed}");

    // The agent token holds `status:read`, so the private project shows.
    let releases = ok(peer, "list_releases", json!({ "project": "secret" })).await;
    assert_eq!(
        releases,
        node.rest(AGENT, "/api/v1/releases?project=secret").await
    );
    assert_eq!(releases["releases"][0]["id"], published["id"], "{releases}");

    let in_an_hour = Timestamp::from_millis(now_ms() + 60 * MINUTE).to_string();
    let silence = ok(
        peer,
        "create_silence",
        json!({
            "matchers": { "subject": "api-https" },
            "expires_at": in_an_hour,
            "reason": "known",
        }),
    )
    .await;
    assert_eq!(silence["state"], "active", "{silence}");
    assert_eq!(
        silence["created_by"],
        json!({ "kind": "token", "id": "agent" }),
        "who made it is the token"
    );
    let listed = ok(peer, "list_silences", json!({})).await;
    assert_eq!(listed, node.rest(AGENT, "/api/v1/silences").await);
    assert_eq!(listed["silences"][0]["id"], silence["id"], "{listed}");
    let ended = ok(peer, "expire_silence", json!({ "id": silence["id"] })).await;
    assert_eq!(ended["state"], "ended", "{ended}");
    assert_eq!(ended["ended_by"], json!({ "kind": "token", "id": "agent" }));

    let timeline = ok(peer, "get_timeline", json!({ "since": week_ago })).await;
    assert_eq!(
        timeline,
        node.rest(AGENT, &format!("/api/v1/timeline?since={week_ago}"))
            .await
    );
    let kinds: BTreeSet<_> = timeline["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        kinds.len() >= 3,
        "outages, incidents and deploys: {timeline}"
    );

    // A limit past the REST page is held to it, never refused.
    for (name, arguments) in [
        ("list_outages", json!({ "limit": 100_000 })),
        ("list_alert_history", json!({ "limit": 100_000 })),
        (
            "get_check_history",
            json!({ "check": "api-https", "limit": 100_000 }),
        ),
        ("get_timeline", json!({ "limit": 100_000 })),
        ("list_incidents", json!({ "limit": 100_000 })),
        ("list_releases", json!({ "limit": 100_000 })),
        ("list_silences", json!({ "limit": 100_000 })),
    ] {
        ok(peer, name, arguments).await;
    }
}

#[tokio::test]
async fn mcp_over_http_serves_every_tool_and_resource() {
    let node = Node::start();
    let client = node.http(AGENT).await;
    let names = tool_names(&client).await;
    assert_eq!(names, TOOLS.iter().map(|&name| name.to_owned()).collect());
    reads_resources(&client).await;
    drives_every_tool(&client, &node).await;
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn mcp_over_stdio_serves_every_tool_and_validates_a_folder() {
    let node = Node::start();
    let client = node.stdio(AGENT).await;
    let names = tool_names(&client).await;
    let mut expected: BTreeSet<_> = TOOLS.iter().map(|&name| name.to_owned()).collect();
    expected.insert("validate_config".to_owned());
    assert_eq!(names, expected);
    reads_resources(&client).await;
    drives_every_tool(&client, &node).await;

    // Checked in `tocsin mcp`, which has none of the node's token
    // variables, so the folder holds no tokens.
    let (tokenless, _) = CONFIG.split_once("[[token]]").unwrap();
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("tocsin.toml"), tokenless).unwrap();
    let path = folder.path().to_str().unwrap();
    let valid = ok(&client, "validate_config", json!({ "folder": path })).await;
    assert_eq!(valid["ok"], true, "{valid}");

    let broken = tokenless.replace("component = \"api\"\ntype", "component = \"gone\"\ntype");
    std::fs::write(folder.path().join("tocsin.toml"), broken).unwrap();
    let invalid = ok(&client, "validate_config", json!({ "folder": path })).await;
    assert_eq!(invalid["ok"], false, "{invalid}");
    assert!(
        invalid["errors"].to_string().contains("gone"),
        "names the unknown component: {invalid}"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn mcp_refuses_a_request_without_a_known_token() {
    let node = Node::start();
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" },
        },
    });
    for token in [None, Some("not-a-token-0123456789"), Some("")] {
        let mut request = http_client()
            .post(format!("{}/mcp", node.url()))
            .header("accept", "application/json, text/event-stream")
            .json(&initialize);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 401, "token {token:?}");
        assert!(
            response.headers().get("mcp-session-id").is_none(),
            "no session for token {token:?}"
        );
    }
    // The same request with a known token is answered.
    let response = http_client()
        .post(format!("{}/mcp", node.url()))
        .header("accept", "application/json, text/event-stream")
        .bearer_auth(READER)
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

/// Each write tool with a token that lacks its scope: a tool error naming
/// the scope, and nothing written.
async fn writes_refuse_without_scope(
    writer: &Peer<RoleClient>,
    reader: &Peer<RoleClient>,
    oncall: &Peer<RoleClient>,
) {
    let opened = ok(
        writer,
        "create_incident",
        json!({ "title": "Seed", "message": "Seeded." }),
    )
    .await;
    let id = opened["id"].as_i64().unwrap();
    ok(
        writer,
        "publish_release",
        json!({ "project": "site", "version": "v1.0.0" }),
    )
    .await;
    let before = (
        ok(writer, "list_incidents", json!({})).await,
        ok(writer, "list_releases", json!({})).await,
        ok(writer, "list_silences", json!({})).await,
    );

    for (peer, name, arguments, scope) in [
        (
            reader,
            "create_incident",
            json!({ "title": "Nope", "message": "Nope." }),
            "`incident`",
        ),
        (
            reader,
            "update_incident",
            json!({ "id": id, "status": "resolved", "message": "Nope." }),
            "`incident`",
        ),
        (
            oncall,
            "publish_release",
            json!({ "project": "site", "version": "v9.9.9" }),
            "`release`",
        ),
        (
            oncall,
            "mark_deployed",
            json!({ "project": "site", "version": "v1.0.0" }),
            "`release`",
        ),
        (
            oncall,
            "create_silence",
            json!({
                "matchers": { "subject": "api-https" },
                "expires_at": Timestamp::from_millis(now_ms() + 60 * MINUTE).to_string(),
                "reason": "nope",
            }),
            "`silence`",
        ),
        (reader, "list_silences", json!({}), "`silence`"),
        (reader, "expire_silence", json!({ "id": 1 }), "`silence`"),
        (
            oncall,
            "get_check_history",
            json!({ "check": "api-https" }),
            "`status:read`",
        ),
        (
            oncall,
            "explain_check",
            json!({ "check": "api-https" }),
            "`status:read`",
        ),
    ] {
        let message = refused(peer, name, arguments).await;
        assert!(message.contains(scope), "{name}: {message}");
    }

    let after = (
        ok(writer, "list_incidents", json!({})).await,
        ok(writer, "list_releases", json!({})).await,
        ok(writer, "list_silences", json!({})).await,
    );
    assert_eq!(before, after, "a refused write changed nothing");
    assert!(after.1["releases"][0]["deployed_at"].is_null());
}

#[tokio::test]
async fn write_tools_refuse_a_token_without_their_scope_over_http() {
    let node = Node::start();
    let (writer, reader, oncall) = (
        node.http(AGENT).await,
        node.http(READER).await,
        node.http(ONCALL).await,
    );
    writes_refuse_without_scope(&writer, &reader, &oncall).await;
}

#[tokio::test]
async fn write_tools_refuse_a_token_without_their_scope_over_stdio() {
    let node = Node::start();
    let (writer, reader, oncall) = (
        node.stdio(AGENT).await,
        node.stdio(READER).await,
        node.stdio(ONCALL).await,
    );
    writes_refuse_without_scope(&writer, &reader, &oncall).await;
}

/// An `incident:draft` token over MCP: drafts, updates and discards its
/// own drafts, and is refused anything that would publish or touch a
/// published incident.
async fn drafts_only(drafter: &Peer<RoleClient>, writer: &Peer<RoleClient>) {
    // Without `status:read` it writes a note and key but never reads them
    // back, and a second create with the key answers the first.
    let new = json!({
        "title": "Agent draft",
        "message": "Looking into it.",
        "internal_note": "Evidence: db-01 swap.",
        "external_key": "agent-draft",
    });
    let draft = ok(drafter, "create_incident", new.clone()).await;
    assert!(draft["published_at"].is_null(), "{draft}");
    assert_eq!(draft["created_by"]["id"], "drafter", "{draft}");
    assert!(draft.get("internal_note").is_none(), "{draft}");
    assert!(draft.get("external_key").is_none(), "{draft}");
    let again = ok(drafter, "create_incident", new).await;
    assert_eq!(again["id"], draft["id"], "{again}");
    let refusal = refused(
        drafter,
        "list_incidents",
        json!({ "external_key": "agent-draft" }),
    )
    .await;
    assert!(refusal.contains("`status:read`"), "{refusal}");
    let refusal = refused(
        drafter,
        "create_incident",
        json!({ "title": "Nope", "message": "Nope.", "draft": false }),
    )
    .await;
    assert!(refusal.contains("`incident`"), "{refusal}");
    let updated = ok(
        drafter,
        "update_incident",
        json!({ "id": draft["id"], "status": "identified", "message": "Found it." }),
    )
    .await;
    assert_eq!(updated["status"], "identified", "{updated}");
    ok(drafter, "discard_incident", json!({ "id": draft["id"] })).await;

    let published = ok(
        writer,
        "create_incident",
        json!({ "title": "Live", "message": "Looking into it.", "draft": false }),
    )
    .await;
    assert!(published["published_at"].is_string(), "{published}");
    let refusal = refused(
        drafter,
        "update_incident",
        json!({ "id": published["id"], "status": "resolved", "message": "Nope." }),
    )
    .await;
    assert!(refusal.contains("`incident`"), "{refusal}");
    let refusal = refused(
        drafter,
        "discard_incident",
        json!({ "id": published["id"] }),
    )
    .await;
    assert!(refusal.contains("published"), "{refusal}");
}

#[tokio::test]
async fn a_drafting_token_only_drafts_over_http() {
    let node = Node::start();
    let (drafter, writer) = (node.http(DRAFTER).await, node.http(AGENT).await);
    drafts_only(&drafter, &writer).await;
}

#[tokio::test]
async fn a_drafting_token_only_drafts_over_stdio() {
    let node = Node::start();
    let (drafter, writer) = (node.stdio(DRAFTER).await, node.stdio(AGENT).await);
    drafts_only(&drafter, &writer).await;
}

#[tokio::test]
async fn an_open_mcp_client_does_not_hold_shutdown() {
    let mut node = Node::start();
    let client = node.http(AGENT).await;
    ok(&client, "get_status", json!({})).await;

    let (status, took) = node.terminate();
    assert_eq!(status.code(), Some(0));
    assert!(
        took < Duration::from_secs(3),
        "shutdown took {took:?} with an MCP client connected"
    );
    drop(client);
}
