// SPDX-License-Identifier: AGPL-3.0-only

//! `/metrics` in process: who may read it, that `promtool` accepts it, and
//! that its label sets are bounded by the configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tocsin_core::Health::{Degraded, Down, Up};
use tocsin_core::config::{Environment, Source};
use tocsin_core::state::{ComponentStatus, Confirmed};
use tocsin_engine::{
    CheckSnapshot, ComponentSnapshot, HeartbeatHandle, LastResult, MemberKind, Snapshot,
};
use tocsin_store::Store;
use tower::ServiceExt;

use super::events::EventBus;
use super::metrics::{CONTENT_TYPE, PATH};
use super::{AppState, router};
use crate::metrics::Metrics;

const CONFIG: &str = r#"
[server]
node_id = "node-a"

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
target = "https://api.internal.example.com/health"
interval = "30s"

[[check]]
id = "db"
type = "tcp"
target = "db.internal.example.com:5432"
interval = "1m"

[[heartbeat]]
id = "backup"
component = "jobs"
interval = "1d"

[[notifier]]
id = "pager"
type = "webhook"

[notifier.webhook]
url = "https://pager.example.com/hook"
signing_secret = "${ENV:PAGER_SIGNING_SECRET}"

[[token]]
id = "oncall"
secret = "${ENV:ONCALL_TOKEN}"
scopes = ["incident"]

[[token]]
id = "reader"
secret = "${ENV:READER_TOKEN}"
scopes = ["status:read"]
"#;

const ONCALL: &str = "oncall-secret-0123456789";
const READER: &str = "reader-secret-0123456789";
const NODE: &str = "node-a";

/// Every family `/metrics` serves.
const FAMILIES: [&str; 11] = [
    "tocsin_build_info",
    "tocsin_check_up",
    "tocsin_check_duration_seconds",
    "tocsin_check_last_run_timestamp",
    "tocsin_heartbeat_last_ping_timestamp",
    "tocsin_component_status",
    "tocsin_config_reload_success",
    "tocsin_config_last_error_timestamp",
    "tocsin_notification_failures_total",
    "tocsin_notifications_abandoned_total",
    "tocsin_db_size_bytes",
];

/// The API down on its HTTPS check, `db` degraded but not yet confirmed,
/// the heartbeat up, `jobs` in maintenance.
fn snapshot() -> Snapshot {
    Snapshot {
        checks: vec![
            CheckSnapshot {
                id: "api-https".into(),
                kind: MemberKind::Check,
                component_id: Some("api".into()),
                state: Some(Confirmed {
                    health: Down,
                    since_ms: 1_760_000_000_000,
                }),
                last: Some(LastResult {
                    health: Down,
                    started_at_ms: 1_760_000_060_500,
                }),
            },
            CheckSnapshot {
                id: "db".into(),
                kind: MemberKind::Check,
                component_id: None,
                state: None,
                last: Some(LastResult {
                    health: Degraded,
                    started_at_ms: 1_760_000_061_000,
                }),
            },
            CheckSnapshot {
                id: "backup".into(),
                kind: MemberKind::Heartbeat,
                component_id: Some("jobs".into()),
                state: Some(Confirmed {
                    health: Up,
                    since_ms: 1_759_900_000_000,
                }),
                last: Some(LastResult {
                    health: Up,
                    started_at_ms: 1_760_000_030_000,
                }),
            },
        ],
        components: vec![
            ComponentSnapshot {
                id: "api".into(),
                health: Some(Down),
                status: ComponentStatus::Down,
            },
            ComponentSnapshot {
                id: "jobs".into(),
                health: Some(Up),
                status: ComponentStatus::Maintenance,
            },
        ],
    }
}

struct Api {
    _dir: tempfile::TempDir,
    state: AppState,
    router: Router,
}

fn api() -> Api {
    let variable = |name: &str| match name {
        "ONCALL_TOKEN" => Some(ONCALL.to_owned()),
        "READER_TOKEN" => Some(READER.to_owned()),
        "PAGER_SIGNING_SECRET" => Some("0123456789abcdef0123456789abcdef".to_owned()),
        _ => None,
    };
    let config = tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(CONFIG.into()),
        }],
        &Environment {
            hostname: "test".into(),
            variable: &variable,
        },
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    let state = AppState {
        store: Arc::new(store),
        config: Arc::new(config),
        events: EventBus::new(),
        admin: None,
        live: Arc::new(snapshot()),
        heartbeats: HeartbeatHandle::default(),
        config_status: crate::reload::steady_status(),
        metrics: Arc::new(Metrics::new()),
        subscribers: None,
        slack: None,
    };
    Api {
        _dir: dir,
        router: router(state.clone()),
        state,
    }
}

impl Api {
    /// `GET /metrics` with `token`: the status, body and content type.
    async fn scrape(&self, token: Option<&str>) -> (StatusCode, String, Option<String>) {
        let mut request = Request::get(PATH);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().unwrap().to_owned());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            String::from_utf8(bytes.to_vec()).unwrap(),
            content_type,
        )
    }

    /// The metrics, read with a `status:read` token.
    async fn metrics(&self) -> String {
        let (status, body, content_type) = self.scrape(Some(READER)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(content_type.as_deref(), Some(CONTENT_TYPE));
        body
    }

    /// Something in every family: a probe, a ping, failed deliveries.
    fn exercise(&self) {
        let metrics = &self.state.metrics;
        metrics
            .checks
            .observe("api-https", Some("api"), NODE, Duration::from_millis(120));
        metrics
            .checks
            .observe("db", None, NODE, Duration::from_millis(4));
        metrics.checks.pinged("backup", 1_760_000_030_250);
        metrics.alerting.attempt_failed("pager");
        metrics.alerting.abandoned("pager");
    }
}

/// Each sample line's metric name and labels.
fn samples(text: &str) -> Vec<(String, BTreeMap<String, String>)> {
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let (series, _value) = line.rsplit_once(' ').unwrap();
            let Some((name, labels)) = series.split_once('{') else {
                return (series.to_owned(), BTreeMap::new());
            };
            let labels = labels
                .strip_suffix('}')
                .unwrap()
                .split(',')
                .map(|pair| {
                    let (key, value) = pair.split_once('=').unwrap();
                    (key.to_owned(), value.trim_matches('"').to_owned())
                })
                .collect();
            (name.to_owned(), labels)
        })
        .collect()
}

#[tokio::test]
async fn metrics_need_a_status_read_token() {
    let api = api();
    for (token, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("not-a-token-0123456789"), StatusCode::UNAUTHORIZED),
        (Some(ONCALL), StatusCode::FORBIDDEN),
        (Some(READER), StatusCode::OK),
    ] {
        let (status, body, _) = api.scrape(token).await;
        assert_eq!(status, expected, "{token:?}: {body}");
        if expected != StatusCode::OK {
            assert!(!body.contains("tocsin_"), "{token:?} read {body}");
        }
    }
}

#[tokio::test]
async fn the_state_now_is_exported_per_check_and_component() {
    let api = api();
    api.exercise();
    let text = api.metrics().await;
    for line in [
        "tocsin_check_up{check=\"api-https\",component=\"api\",node=\"node-a\",kind=\"check\"} 0.0\n",
        "tocsin_check_up{check=\"backup\",component=\"jobs\",node=\"node-a\",kind=\"heartbeat\"} 1.0\n",
        "tocsin_check_last_run_timestamp{check=\"api-https\",component=\"api\",node=\"node-a\"} 1760000060\n",
        "tocsin_check_last_run_timestamp{check=\"db\",component=\"\",node=\"node-a\"} 1760000061\n",
        "tocsin_heartbeat_last_ping_timestamp{check=\"backup\",component=\"jobs\",node=\"node-a\"} 1760000030\n",
        "tocsin_component_status{component=\"api\",node=\"node-a\",status=\"down\"} 1\n",
        "tocsin_component_status{component=\"api\",node=\"node-a\",status=\"operational\"} 0\n",
        "tocsin_component_status{component=\"jobs\",node=\"node-a\",status=\"maintenance\"} 1\n",
        "tocsin_check_duration_seconds_count{check=\"api-https\",component=\"api\",node=\"node-a\"} 1\n",
        "tocsin_check_duration_seconds_bucket{le=\"0.25\",check=\"api-https\",component=\"api\",node=\"node-a\"} 1\n",
        "tocsin_check_duration_seconds_bucket{le=\"0.1\",check=\"api-https\",component=\"api\",node=\"node-a\"} 0\n",
        "tocsin_notification_failures_total{notifier=\"pager\"} 1\n",
        "tocsin_notifications_abandoned_total{notifier=\"pager\"} 1\n",
        "tocsin_config_reload_success 1\n",
        concat!(
            "tocsin_build_info{version=\"",
            env!("CARGO_PKG_VERSION"),
            "\"} 1\n"
        ),
    ] {
        assert!(text.contains(line), "{line} missing from\n{text}");
    }
    // Unconfirmed: no health to report yet, rather than a guess.
    assert!(!text.contains("tocsin_check_up{check=\"db\""), "{text}");
    // The heartbeat's last result is a ping; it is not a probe run.
    assert!(
        !text.contains("tocsin_check_last_run_timestamp{check=\"backup\""),
        "{text}"
    );
    let size = api.state.store.size_on_disk().unwrap();
    assert!(size > 0);
    assert!(
        text.contains(&format!("tocsin_db_size_bytes {size}\n")),
        "{text}"
    );
}

/// The text with each family's samples in a fixed order: a family's
/// series come out of a hash map, in an order that changes per process.
/// A histogram's lines for one series keep their order (the sort is
/// stable and ignores `le`). The size of the database file and the
/// version are redacted.
fn normalized(text: &str) -> String {
    fn key(line: &str) -> String {
        let (series, _value) = line.rsplit_once(' ').unwrap();
        let (_name, labels) = series.split_once('{').unwrap_or((series, ""));
        labels
            .split(',')
            .filter(|pair| !pair.starts_with("le="))
            .collect::<Vec<_>>()
            .join(",")
    }
    let mut out = String::new();
    let mut family: Vec<&str> = Vec::new();
    let flush = |family: &mut Vec<&str>, out: &mut String| {
        family.sort_by_key(|line| key(line));
        for line in family.drain(..) {
            out.push_str(line);
            out.push('\n');
        }
    };
    for line in text.lines() {
        if line.starts_with('#') {
            flush(&mut family, &mut out);
            out.push_str(line);
            out.push('\n');
        } else if line.starts_with("tocsin_db_size_bytes ") {
            family.push("tocsin_db_size_bytes <size>");
        } else if line.starts_with("tocsin_build_info{") {
            family.push("tocsin_build_info{version=\"<version>\"} 1");
        } else {
            family.push(line);
        }
    }
    flush(&mut family, &mut out);
    out
}

/// The whole of `/metrics` after a probe, a ping and failed deliveries:
/// a change to any family, label, bucket, value or line shows up here.
#[tokio::test]
async fn the_whole_text_is_unchanged() {
    let api = api();
    let metrics = &api.state.metrics;
    metrics
        .checks
        .observe("api-https", Some("api"), NODE, Duration::from_millis(120));
    metrics
        .checks
        .observe("db", None, NODE, Duration::from_millis(4));
    metrics.checks.pinged("backup", 1_760_000_030_250);
    metrics.alerting.attempt_failed("pager");
    metrics.alerting.abandoned("pager");
    insta::assert_snapshot!(normalized(&api.metrics().await));
}

#[tokio::test]
async fn every_family_is_documented_and_described() {
    let api = api();
    api.exercise();
    let text = api.metrics().await;
    let docs = include_str!("../../../../docs/metrics.md");
    let types: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("# TYPE ")?.split(' ').next())
        .collect();
    let families = FAMILIES;
    assert_eq!(
        types.iter().copied().collect::<BTreeSet<_>>(),
        families.iter().copied().collect::<BTreeSet<_>>(),
        "/metrics serves exactly FAMILIES"
    );
    assert_eq!(types.len(), families.len(), "each family once");
    for family in families {
        assert!(
            text.contains(&format!("# HELP {family} ")),
            "{family} has no HELP"
        );
        assert!(
            docs.contains(&format!("`{family}")),
            "docs/metrics.md misses {family}"
        );
    }
}

#[tokio::test]
async fn label_sets_are_bounded_by_the_configuration() {
    let api = api();
    api.exercise();
    let before = samples(&api.metrics().await);
    // Many more results, each different: the same series, other values.
    for millis in 0..500 {
        api.state.metrics.checks.observe(
            "api-https",
            Some("api"),
            NODE,
            Duration::from_millis(millis * 97),
        );
        api.state
            .metrics
            .checks
            .pinged("backup", 1_760_000_030_000 + i64::try_from(millis).unwrap());
        api.state.metrics.alerting.attempt_failed("pager");
    }
    let after = samples(&api.metrics().await);
    let series = |samples: &[(String, BTreeMap<String, String>)]| {
        samples.iter().cloned().collect::<BTreeSet<_>>()
    };
    assert_eq!(series(&before), series(&after), "results added series");

    let config = &api.state.config;
    let checks: BTreeSet<&str> = config
        .checks
        .iter()
        .map(|check| check.id.as_str())
        .chain(
            config
                .heartbeats
                .iter()
                .map(|heartbeat| heartbeat.id.as_str()),
        )
        .collect();
    let components: BTreeSet<&str> = config
        .components
        .iter()
        .map(|component| component.id.as_str())
        .chain([""])
        .collect();
    let statuses: BTreeSet<&str> = ComponentStatus::ALL
        .iter()
        .map(|status| status.as_str())
        .collect();
    for (name, labels) in &after {
        for (key, value) in labels {
            let bounded = match key.as_str() {
                "check" => checks.contains(value.as_str()),
                "component" => components.contains(value.as_str()),
                "node" => value == NODE,
                "kind" => ["check", "heartbeat"].contains(&value.as_str()),
                "status" => statuses.contains(value.as_str()),
                "notifier" => value == "pager",
                "version" => value == env!("CARGO_PKG_VERSION"),
                // The histogram's fixed buckets.
                "le" => name == "tocsin_check_duration_seconds_bucket",
                _ => false,
            };
            assert!(
                bounded,
                "{name}: label {key}={value} is not from the configuration"
            );
        }
    }
}

#[tokio::test]
async fn series_of_what_the_configuration_dropped_are_not_served() {
    let api = api();
    api.exercise();
    // Series for a check, a heartbeat and a notifier the running
    // configuration does not have, as if a reload removed them.
    let metrics = &api.state.metrics;
    metrics
        .checks
        .observe("removed", Some("gone"), NODE, Duration::from_millis(1));
    // The same check under a component it has since left.
    metrics
        .checks
        .observe("api-https", Some("jobs"), NODE, Duration::from_millis(1));
    metrics
        .checks
        .pinged("removed-heartbeat", 1_760_000_000_000);
    metrics.alerting.attempt_failed("gone-notifier");
    let text = api.metrics().await;
    for stale in [
        "\"removed\"",
        "\"gone\"",
        "\"removed-heartbeat\"",
        "\"gone-notifier\"",
        "check=\"api-https\",component=\"jobs\"",
    ] {
        assert!(!text.contains(stale), "{stale} still served:\n{text}");
    }
    assert_eq!(metrics.alerting.attempt_failures("gone-notifier"), 0);
    assert!(
        text.contains("tocsin_check_duration_seconds_count{check=\"api-https\",component=\"api\",node=\"node-a\"} 1\n"),
        "{text}"
    );
}

/// promtool, from `TOCSIN_PROMTOOL` or the `PATH`. CI installs a pinned
/// one and sets `TOCSIN_PROMTOOL`; there it must be found, never skipped.
fn promtool() -> Option<Command> {
    if let Some(path) = std::env::var_os("TOCSIN_PROMTOOL") {
        return Some(Command::new(path));
    }
    match Command::new("promtool").arg("--version").output() {
        Ok(_) => Some(Command::new("promtool")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("cannot run promtool: {error}"),
    }
}

#[tokio::test]
async fn promtool_accepts_the_metrics() {
    let Some(mut promtool) = promtool() else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI must install promtool and set TOCSIN_PROMTOOL"
        );
        eprintln!("SKIPPED promtool_accepts_the_metrics: no promtool on the PATH");
        return;
    };
    let api = api();
    api.exercise();
    let text = api.metrics().await;
    let mut child = promtool
        .args(["check", "metrics"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("promtool runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "promtool check metrics: {}\n{}{}\n--- input ---\n{text}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
