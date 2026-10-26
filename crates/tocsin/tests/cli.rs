// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of the CLI output contract, run against the built binary.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

#[allow(dead_code)]
mod support;

fn tocsin(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .args(args)
        .output()
        .expect("tocsin runs")
}

/// The smallest valid file, on a port the system picks so tests running
/// at once never collide.
const LISTEN_ANY_PORT: &str = "[server]\nlisten = \"127.0.0.1:0\"\n";

/// A configuration folder holding the smallest valid file.
fn config_folder() -> tempfile::TempDir {
    let config = tempfile::tempdir().unwrap();
    std::fs::write(config.path().join("server.toml"), LISTEN_ANY_PORT).unwrap();
    config
}

/// Stdout must be exactly one JSON document.
fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout is a single JSON document")
}

#[test]
fn version_json_prints_one_document_and_exits_zero() {
    let output = tocsin(&["--version", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        json_stdout(&output),
        serde_json::json!({ "version": env!("CARGO_PKG_VERSION") })
    );
}

#[test]
fn version_text() {
    let output = tocsin(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("tocsin {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_argument_is_invalid_input_as_json() {
    let output = tocsin(&["--json", "no-such-command"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json_stdout(&output)["error"]["kind"], "invalid_input");
}

#[test]
fn no_command_is_invalid_input() {
    let output = tocsin(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
}

#[test]
fn serve_rejects_missing_config_folder() {
    let data = tempfile::tempdir().unwrap();
    let output = tocsin(&[
        "--json",
        "serve",
        "--config",
        "/nonexistent/tocsin-config",
        "--data",
        data.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json_stdout(&output)["error"]["kind"], "invalid_input");
}

/// A running `tocsin serve` that is killed if a test panics before it stops,
/// so a failed test never leaves a server behind.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl Server {
    fn start(config: &Path, data: &Path) -> Self {
        Server::start_with(config, data, &[])
    }

    /// Starts serve with extra environment variables.
    fn start_with(config: &Path, data: &Path, environment: &[(&str, &Path)]) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_tocsin"))
            .envs(environment.iter().copied())
            .args(["--json", "serve", "--config"])
            .arg(config)
            .arg("--data")
            .arg(data)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("tocsin serve starts");
        Server(child)
    }

    fn signal(&self, signal: &str) {
        let sent = Command::new("kill")
            .args([signal, &self.0.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
    }

    fn wait_with_deadline(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "serve did not stop within 10s");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

const READY: &str = "waiting for SIGINT or SIGTERM";

/// Reads stderr until the ready line, then returns the reader still open.
fn wait_ready(server: &mut Server) -> std::io::Lines<BufReader<ChildStderr>> {
    let mut lines = BufReader::new(server.0.stderr.take().unwrap()).lines();
    let ready = lines
        .by_ref()
        .map_while(Result::ok)
        .any(|line| line.contains(READY));
    assert!(ready, "serve exited before becoming ready");
    lines
}

fn stops_cleanly_on(signal: &str, name: &str) {
    let config = config_folder();
    let parent = tempfile::tempdir().unwrap();
    let data = parent.path().join("data");
    let mut server = Server::start(config.path(), &data);
    // Keep draining stderr so the server never writes into a closed pipe.
    let lines = wait_ready(&mut server);
    let drain = std::thread::spawn(move || lines.map_while(Result::ok).collect::<Vec<_>>());

    server.signal(signal);
    let status = server.wait_with_deadline();
    let mut stdout = Vec::new();
    server
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let log = drain.join().unwrap().join("\n");
    assert_eq!(status.code(), Some(0), "stderr:\n{log}");
    assert_eq!(
        serde_json::from_slice::<Value>(&stdout).expect("stdout is a single JSON document"),
        serde_json::json!({ "status": "stopped", "signal": name })
    );
    assert!(data.is_dir(), "serve creates the data folder");
    assert!(
        data.join("tocsin.db").is_file(),
        "serve creates and migrates the database"
    );
}

#[test]
fn serve_stops_cleanly_on_sigterm() {
    stops_cleanly_on("-TERM", "SIGTERM");
}

#[test]
fn serve_stops_cleanly_on_sigint() {
    stops_cleanly_on("-INT", "SIGINT");
}

#[test]
fn serve_reloads_on_sighup_and_keeps_running() {
    let config = config_folder();
    let parent = tempfile::tempdir().unwrap();
    let data = parent.path().join("data");
    let mut server = Server::start(config.path(), &data);
    let lines = wait_ready(&mut server);
    // Read on a thread, so a server that never logs the reload fails the
    // test instead of hanging it.
    let (sender, received) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        for line in lines.map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });

    server.signal("-HUP");
    let reloaded = std::iter::from_fn(|| received.recv_timeout(Duration::from_secs(10)).ok())
        .find(|line| line.contains("configuration reloaded"))
        .expect("serve reloads on SIGHUP instead of exiting");
    assert!(reloaded.contains("SIGHUP"), "{reloaded}");

    server.signal("-TERM");
    let status = server.wait_with_deadline();
    drain.join().unwrap();
    let log = received.try_iter().collect::<Vec<_>>().join("\n");
    assert_eq!(status.code(), Some(0), "stderr:\n{log}");
}

/// A monitor must not die because whatever reads its logs went away.
#[test]
fn serve_survives_a_closed_log_pipe() {
    let config = config_folder();
    let data = tempfile::tempdir().unwrap();
    let mut server = Server::start(config.path(), data.path());
    drop(wait_ready(&mut server));
    server.signal("-TERM");
    assert_eq!(server.wait_with_deadline().code(), Some(0));
}

/// Nor because whatever reads its result went away: the exit code still holds.
#[test]
fn serve_survives_a_closed_result_pipe() {
    let config = config_folder();
    let data = tempfile::tempdir().unwrap();
    let mut server = Server::start(config.path(), data.path());
    let lines = wait_ready(&mut server);
    let drain = std::thread::spawn(move || lines.map_while(Result::ok).count());
    drop(server.0.stdout.take());
    server.signal("-TERM");
    assert_eq!(server.wait_with_deadline().code(), Some(0));
    drain.join().unwrap();
}

/// Answers `count` HTTP requests with 200, one per connection, and returns
/// their bodies.
fn http_receiver(count: usize) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let bodies = std::thread::spawn(move || {
        (0..count)
            .map(|_| {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                (&stream)
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .unwrap();
                serde_json::from_slice(&body).unwrap()
            })
            .collect()
    });
    (url, bodies)
}

#[test]
fn notify_test_sends_a_fire_and_its_resolve() {
    let (url, bodies) = http_receiver(2);
    let config = tempfile::tempdir().unwrap();
    std::fs::write(
        config.path().join("server.toml"),
        format!("{LISTEN_ANY_PORT}node_id = \"node-a\"\n"),
    )
    .unwrap();
    std::fs::write(
        config.path().join("notifiers.toml"),
        format!(
            "[[notifier]]\nid = \"hook\"\ntype = \"webhook\"\n\n\
             [notifier.webhook]\nurl = \"{url}\"\n\
             signing_secret = \"${{ENV:TOCSIN_TEST_SIGNING_SECRET}}\"\n"
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .env("TOCSIN_TEST_SIGNING_SECRET", "whsec-test")
        .args(["--json", "notify", "test", "hook", "--config"])
        .arg(config.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        json_stdout(&output),
        serde_json::json!({
            "notifier": "hook",
            "type": "webhook",
            "fire": { "sent": true },
            "resolve": { "sent": true },
        })
    );
    let bodies = bodies.join().unwrap();
    assert_eq!(bodies[0]["event"], "fire");
    assert_eq!(bodies[1]["event"], "resolve");
    assert_eq!(bodies[0]["identity"], "node-a/check/tocsin-test");
    assert_eq!(bodies[1]["identity"], bodies[0]["identity"]);
    assert_eq!(
        (&bodies[0]["test"], &bodies[1]["test"]),
        (&Value::Bool(true), &Value::Bool(true))
    );
}

#[test]
fn notify_test_names_the_notifier_it_meant() {
    let config = config_folder();
    std::fs::write(
        config.path().join("notifiers.toml"),
        "[[notifier]]\nid = \"ops-slack\"\ntype = \"slack\"\n\n\
         [notifier.slack]\nwebhook_url = \"${ENV:TOCSIN_TEST_SLACK_URL}\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .env("TOCSIN_TEST_SLACK_URL", "https://hooks.example.com/x")
        .args(["--json", "notify", "test", "ops-slak", "--config"])
        .arg(config.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        json_stdout(&output)["error"]["message"],
        "no notifier `ops-slak`; did you mean `ops-slack`?"
    );
}

/// Stopping while an alert is being sent abandons the send within a bound,
/// closes the store cleanly, and leaves the alert queued for the next start.
#[test]
fn serve_stops_mid_delivery_and_keeps_the_alert_queued() {
    // A check on a port nobody listens on, so it goes down at once.
    let closed = support::RefusedPort::new();
    let down_target = closed.address();
    // A webhook receiver that accepts the connection and never answers.
    let receiver = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let webhook = format!("http://{}/hook", receiver.local_addr().unwrap());
    let (accepted, connection) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = receiver.accept() {
            let _ = accepted.send(stream);
        }
    });

    let config = config_folder();
    std::fs::write(
        config.path().join("alerting.toml"),
        format!(
            "[[check]]\nid = \"api\"\ntype = \"tcp\"\ntarget = \"{down_target}\"\n\
             interval = \"1s\"\ntimeout = \"500ms\"\nconfirm = 1\n\n\
             [[notifier]]\nid = \"hook\"\ntype = \"webhook\"\n\n\
             [notifier.webhook]\nurl = \"{webhook}\"\n\
             signing_secret = \"${{ENV:TOCSIN_TEST_SIGNING_SECRET}}\"\n\n\
             [[route]]\nid = \"all\"\nnotifiers = [\"hook\"]\n"
        ),
    )
    .unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut server = Server::start_with(
        config.path(),
        data.path(),
        &[("TOCSIN_TEST_SIGNING_SECRET", Path::new("whsec-test"))],
    );
    let lines = wait_ready(&mut server);
    let drain = std::thread::spawn(move || lines.map_while(Result::ok).collect::<Vec<_>>());

    // The fire is in flight once the receiver has its connection.
    let _held = connection
        .recv_timeout(Duration::from_secs(10))
        .expect("serve posts the alert");
    server.signal("-TERM");
    let status = server.wait_with_deadline();
    let log = drain.join().unwrap().join("\n");
    assert_eq!(status.code(), Some(0), "stderr:\n{log}");

    let database = rusqlite::Connection::open(data.path().join("tocsin.db")).unwrap();
    let deliveries: Vec<(String, String, i64)> = database
        .prepare("SELECT kind, status, attempts FROM alert_delivery ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(deliveries, [("fire".to_owned(), "pending".to_owned(), 0)]);
}

#[test]
fn serve_rejects_a_dangling_symlink_as_data_folder() {
    let config = config_folder();
    let parent = tempfile::tempdir().unwrap();
    let link = parent.path().join("data");
    std::os::unix::fs::symlink(parent.path().join("missing"), &link).unwrap();
    let output = tocsin(&[
        "--json",
        "serve",
        "--config",
        config.path().to_str().unwrap(),
        "--data",
        link.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    let error = &json_stdout(&output)["error"];
    assert_eq!(error["kind"], "invalid_input");
    let expected = format!("data folder {} is not a directory", link.display());
    assert_eq!(error["message"], expected.as_str());
}

#[test]
fn serve_reads_etc_tocsin_by_default() {
    let output = tocsin(&["serve", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("[default: /etc/tocsin]"), "{help}");
}

#[test]
fn an_empty_config_folder_is_invalid_input() {
    let config = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let folder = config.path().to_str().unwrap();
    let commands: [&[&str]; 2] = [
        &["--json", "config", "check", "--config", folder],
        &[
            "--json",
            "serve",
            "--config",
            folder,
            "--data",
            data.path().to_str().unwrap(),
        ],
    ];
    for command in commands {
        let output = tocsin(command);
        assert_eq!(output.status.code(), Some(2), "{command:?}");
        let error = &json_stdout(&output)["error"];
        assert_eq!(error["kind"], "invalid_input");
        assert_eq!(
            error["errors"][0]["message"],
            "the folder holds no `*.toml` files"
        );
        assert!(
            error["errors"][0]["hint"]
                .as_str()
                .unwrap()
                .contains("server.toml")
        );
    }
}

#[test]
fn help_under_json_is_one_document() {
    let output = tocsin(&["--json", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = json_stdout(&output);
    assert!(help["help"].as_str().unwrap().contains("Usage: tocsin"));
}

#[test]
fn version_with_a_command_is_invalid_input() {
    let output = tocsin(&[
        "--json",
        "--version",
        "serve",
        "--config",
        ".",
        "--data",
        ".",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json_stdout(&output)["error"]["kind"], "invalid_input");
}

/// A database with three results on 2026-10-20 and one on 2026-10-21 (UTC).
fn seeded_database() -> tempfile::TempDir {
    use tocsin_core::Health;
    use tocsin_store::{NewCheckResult, Store};

    let data = tempfile::tempdir().unwrap();
    let day = 1_792_454_400_000; // 2026-10-20T00:00:00Z
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = Store::open(&data.path().join("tocsin.db")).unwrap();
        for (offset_ms, health) in [
            (1_000, Health::Up),
            (2_000, Health::Down),
            (3_000, Health::Degraded),
            (86_400_000 + 1_000, Health::Up),
        ] {
            store
                .append(NewCheckResult {
                    node_id: "node-a".into(),
                    check_id: "web".into(),
                    started_at_ms: day + offset_ms,
                    duration_ms: 12,
                    health,
                    detail: None,
                    error: None,
                    status_code: Some(200),
                })
                .await
                .unwrap();
        }
        store.close().await.unwrap();
    });
    data
}

fn data_arg(data: &tempfile::TempDir) -> &str {
    data.path().to_str().unwrap()
}

#[test]
fn db_info_reports_rows_and_the_oldest_result() {
    let data = seeded_database();
    let output = tocsin(&["--json", "db", "info", "--data", data_arg(&data)]);
    assert_eq!(output.status.code(), Some(0));
    let info = json_stdout(&output);
    assert_eq!(info["rows"]["check_result"], 4);
    assert_eq!(info["rows"]["daily_rollup"], 0);
    assert_eq!(info["schema_version"], 13);
    assert!(info["size_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        info["oldest_result"],
        serde_json::json!({ "ms": 1_792_454_401_000_i64, "utc": "2026-10-20T00:00:01.000Z" })
    );
    assert_eq!(info["purged_before"], Value::Null);
}

#[test]
fn db_rollup_recomputes_from_a_day_and_is_idempotent() {
    let data = seeded_database();
    let args = [
        "--json",
        "db",
        "rollup",
        "--data",
        data_arg(&data),
        "--since",
        "2026-10-01",
    ];
    for _ in 0..2 {
        let output = tocsin(&args);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            json_stdout(&output),
            serde_json::json!({ "requested_since": "2026-10-01", "since": "2026-10-01", "rows": 2 })
        );
    }
    let info = json_stdout(&tocsin(&[
        "--json",
        "db",
        "info",
        "--data",
        data_arg(&data),
    ]));
    assert_eq!(info["rows"]["daily_rollup"], 2);
}

#[test]
fn db_rollup_rejects_a_malformed_day() {
    let data = seeded_database();
    for since in ["2026-13-01", "2026-02-30", "yesterday", "2026-1-01"] {
        let output = tocsin(&[
            "--json",
            "db",
            "rollup",
            "--data",
            data_arg(&data),
            "--since",
            since,
        ]);
        assert_eq!(output.status.code(), Some(2), "--since {since}");
        assert_eq!(json_stdout(&output)["error"]["kind"], "invalid_input");
    }
}

#[test]
fn db_vacuum_reports_sizes() {
    let data = seeded_database();
    let output = tocsin(&["--json", "db", "vacuum", "--data", data_arg(&data)]);
    assert_eq!(output.status.code(), Some(0));
    let report = json_stdout(&output);
    assert!(report["size_before_bytes"].as_u64().unwrap() > 0);
    assert!(report["size_after_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn db_commands_never_create_a_database() {
    let data = tempfile::tempdir().unwrap();
    for command in ["info", "vacuum"] {
        let output = tocsin(&["--json", "db", command, "--data", data_arg(&data)]);
        assert_eq!(output.status.code(), Some(2), "db {command}");
        assert_eq!(json_stdout(&output)["error"]["kind"], "invalid_input");
    }
    assert!(!data.path().join("tocsin.db").exists());
}

fn user_version(data: &tempfile::TempDir) -> i64 {
    rusqlite::Connection::open(data.path().join("tocsin.db"))
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

/// `db info` opens read-only and never migrates: a database a newer build
/// wrote is described, with its version, and left exactly as it was.
#[test]
fn db_info_reports_a_newer_schema_without_touching_it() {
    let data = seeded_database();
    {
        let conn = rusqlite::Connection::open(data.path().join("tocsin.db")).unwrap();
        conn.execute_batch("PRAGMA user_version = 99; CREATE TABLE from_the_future (id INTEGER);")
            .unwrap();
    }
    let output = tocsin(&["--json", "db", "info", "--data", data_arg(&data)]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let info = json_stdout(&output);
    assert_eq!(info["schema_version"], 99);
    assert_eq!(info["supported_schema_version"], 13);
    assert_eq!(info["rows"]["check_result"], 4);
    assert_eq!(user_version(&data), 99);

    let text = tocsin(&["db", "info", "--data", data_arg(&data)]);
    assert!(
        String::from_utf8_lossy(&text.stdout).contains("newer than this build's 13"),
        "{}",
        String::from_utf8_lossy(&text.stdout)
    );
}

/// An older database is reported at its own version, not migrated first.
#[test]
fn db_info_does_not_migrate() {
    let data = tempfile::tempdir().unwrap();
    rusqlite::Connection::open(data.path().join("tocsin.db"))
        .unwrap()
        .execute_batch("CREATE TABLE check_result (started_at INTEGER); INSERT INTO check_result VALUES (1000);")
        .unwrap();
    let output = tocsin(&["--json", "db", "info", "--data", data_arg(&data)]);
    assert_eq!(output.status.code(), Some(0));
    let info = json_stdout(&output);
    assert_eq!(info["schema_version"], 0);
    assert_eq!(info["rows"], serde_json::json!({ "check_result": 1 }));
    assert_eq!(info["purged_before"], Value::Null);
    assert_eq!(user_version(&data), 0);
}

/// While `serve` holds the data folder, `db vacuum` and a second `serve` are
/// refused; `db info` and `db rollup` still run. Once it stops, vacuum works.
#[test]
fn a_running_server_locks_out_vacuum_and_a_second_server() {
    let config = config_folder();
    let data = seeded_database();
    let mut server = Server::start(config.path(), data.path());
    let lines = wait_ready(&mut server);
    let drain = std::thread::spawn(move || lines.map_while(Result::ok).count());

    let vacuum = tocsin(&["--json", "db", "vacuum", "--data", data_arg(&data)]);
    assert_eq!(vacuum.status.code(), Some(2));
    let error = &json_stdout(&vacuum)["error"];
    assert_eq!(error["kind"], "invalid_input");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("tocsin serve is running"),
        "{error}"
    );

    let second = tocsin(&[
        "--json",
        "serve",
        "--config",
        data_arg(&config),
        "--data",
        data_arg(&data),
    ]);
    assert_eq!(second.status.code(), Some(2));
    let error = &json_stdout(&second)["error"];
    assert_eq!(error["kind"], "invalid_input");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("another tocsin serve"),
        "{error}"
    );

    let info = tocsin(&["--json", "db", "info", "--data", data_arg(&data)]);
    assert_eq!(info.status.code(), Some(0));
    let rollup = tocsin(&[
        "--json",
        "db",
        "rollup",
        "--data",
        data_arg(&data),
        "--since",
        "2026-10-01",
    ]);
    assert_eq!(rollup.status.code(), Some(0));

    server.signal("-TERM");
    assert_eq!(server.wait_with_deadline().code(), Some(0));
    drain.join().unwrap();
    let vacuum = tocsin(&["--json", "db", "vacuum", "--data", data_arg(&data)]);
    assert_eq!(vacuum.status.code(), Some(0));
}

#[test]
fn serve_refuses_an_invalid_configuration() {
    let config = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(
        config.path().join("server.toml"),
        "[server]\nlisten = \"127.0.0.1:0\"\nretention_days = 0\n",
    )
    .unwrap();
    let output = tocsin(&[
        "--json",
        "serve",
        "--config",
        config.path().to_str().unwrap(),
        "--data",
        data.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    let error = &json_stdout(&output)["error"];
    assert_eq!(error["kind"], "invalid_input");
    assert_eq!(error["errors"][0]["line"], 3);
    assert_eq!(error["errors"][0]["path"], "server.retention_days");
}

#[test]
fn serve_needs_a_data_folder_from_the_flag_or_the_configuration() {
    let config = config_folder();
    let output = tocsin(&[
        "--json",
        "serve",
        "--config",
        config.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    let message = json_stdout(&output)["error"]["message"].clone();
    assert!(message.as_str().unwrap().contains("data_dir"), "{message}");

    // A relative `data_dir` is read relative to the config folder: here it
    // names a file, so serve refuses it rather than starting.
    std::fs::write(config.path().join("taken"), "").unwrap();
    std::fs::write(
        config.path().join("server.toml"),
        "[server]\nlisten = \"127.0.0.1:0\"\ndata_dir = \"taken\"\n",
    )
    .unwrap();
    let output = tocsin(&[
        "--json",
        "serve",
        "--config",
        config.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    let message = json_stdout(&output)["error"]["message"].clone();
    let expected = format!(
        "data folder {} is not a directory",
        config.path().join("taken").display()
    );
    assert_eq!(message, expected.as_str());
}

/// The first maintenance pass runs at startup: it rolls up the expired days
/// and then deletes their raw results.
#[test]
fn serve_applies_retention_at_startup() {
    let config = tempfile::tempdir().unwrap();
    std::fs::write(
        config.path().join("server.toml"),
        "[server]\nlisten = \"127.0.0.1:0\"\nretention_days = 1\n",
    )
    .unwrap();
    let data = seeded_database();
    let mut server = Server::start(config.path(), data.path());
    // Lines arrive through a channel so a missing line fails at a deadline
    // instead of blocking on a server that never exits.
    let stderr = server.0.stderr.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut ready, mut purged) = (false, false);
    while !(ready && purged) {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(line) = receiver.recv_timeout(left) else {
            break;
        };
        ready |= line.contains(READY);
        purged |= line.contains("retention purged old check results");
    }
    assert!(ready && purged, "ready: {ready}, purged: {purged}");
    server.signal("-TERM");
    assert_eq!(server.wait_with_deadline().code(), Some(0));
    drain.join().unwrap();

    let info = json_stdout(&tocsin(&[
        "--json",
        "db",
        "info",
        "--data",
        data_arg(&data),
    ]));
    assert_eq!(info["rows"]["check_result"], 0);
    assert_eq!(info["rows"]["daily_rollup"], 2);
    assert_ne!(info["purged_before"], Value::Null);
}

/// Serve runs the configured checks: results are stored, and a check that
/// cannot connect opens an outage while one that can does not.
#[test]
fn serve_runs_the_checks_and_records_an_outage() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let open = listener.local_addr().unwrap();
    let refused = support::RefusedPort::new();
    let closed = refused.address();
    let config = config_folder();
    let checks = format!(
        "[[check]]\nid = \"open\"\ntype = \"tcp\"\ntarget = \"{open}\"\n\
         interval = \"1s\"\ntimeout = \"500ms\"\nconfirm = 1\n\n\
         [[check]]\nid = \"closed\"\ntype = \"tcp\"\ntarget = \"{closed}\"\n\
         interval = \"1s\"\ntimeout = \"500ms\"\nconfirm = 1\n"
    );
    std::fs::write(config.path().join("checks.toml"), checks).unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut server = Server::start(config.path(), data.path());
    let lines = wait_ready(&mut server);
    let drain = std::thread::spawn(move || lines.map_while(Result::ok).collect::<Vec<_>>());

    std::thread::sleep(Duration::from_millis(2_500));
    server.signal("-TERM");
    let status = server.wait_with_deadline();
    let log = drain.join().unwrap().join("\n");
    assert_eq!(status.code(), Some(0), "stderr:\n{log}");
    drop(listener);

    let output = tocsin(&["--json", "db", "info", "--data", data_arg(&data)]);
    assert_eq!(output.status.code(), Some(0));
    let info = json_stdout(&output);
    let results = info["rows"]["check_result"].as_i64().unwrap();
    assert!(results >= 4, "{results} results; stderr:\n{log}");
    assert_eq!(info["rows"]["outage"], 1, "stderr:\n{log}");
}

/// Without system root certificates serve stays up: HTTP checks are
/// recorded down with the reason, said once at startup, and the other
/// checks run as usual.
#[test]
fn serve_without_root_certificates_records_http_checks_down() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let open = listener.local_addr().unwrap();
    let config = config_folder();
    let checks = format!(
        "[[check]]\nid = \"web\"\ntype = \"http\"\ntarget = \"https://127.0.0.1:1/\"\n\
         interval = \"1s\"\ntimeout = \"500ms\"\nconfirm = 1\n\n\
         [[check]]\nid = \"open\"\ntype = \"tcp\"\ntarget = \"{open}\"\n\
         interval = \"1s\"\ntimeout = \"500ms\"\nconfirm = 1\n"
    );
    std::fs::write(config.path().join("checks.toml"), checks).unwrap();
    // When either is set, these are the only places roots are loaded from.
    let roots = tempfile::tempdir().unwrap();
    let no_roots = roots.path().join("empty.pem");
    std::fs::write(&no_roots, "").unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut server = Server::start_with(
        config.path(),
        data.path(),
        &[("SSL_CERT_FILE", &no_roots), ("SSL_CERT_DIR", roots.path())],
    );
    let mut lines = BufReader::new(server.0.stderr.take().unwrap()).lines();
    let before_ready: Vec<String> = lines
        .by_ref()
        .map_while(Result::ok)
        .take_while(|line| !line.contains(READY))
        .collect();
    let drain = std::thread::spawn(move || lines.map_while(Result::ok).collect::<Vec<_>>());

    std::thread::sleep(Duration::from_millis(2_500));
    server.signal("-TERM");
    let status = server.wait_with_deadline();
    let log = format!(
        "{}\n{}",
        before_ready.join("\n"),
        drain.join().unwrap().join("\n")
    );
    assert_eq!(status.code(), Some(0), "serve stays up; stderr:\n{log}");
    drop(listener);
    let warnings: Vec<&String> = before_ready
        .iter()
        .filter(|line| line.contains("no root certificates"))
        .collect();
    assert_eq!(warnings.len(), 1, "one startup error; stderr:\n{log}");
    assert!(
        warnings[0].contains("ERROR")
            && warnings[0].contains("web")
            && !warnings[0].contains("open"),
        "it names the affected checks: {}",
        warnings[0]
    );

    let conn = rusqlite::Connection::open(data.path().join("tocsin.db")).unwrap();
    let results = |check: &str| -> Vec<(String, Option<String>)> {
        conn.prepare("SELECT health, error FROM check_result WHERE check_id = ?1")
            .unwrap()
            .query_map([check], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let web = results("web");
    assert!(web.len() >= 2, "{web:?}; stderr:\n{log}");
    for (health, error) in &web {
        assert_eq!(health, "down");
        let error = error.as_deref().unwrap_or_default();
        assert!(error.contains("no root certificates"), "{error}");
    }
    let open = results("open");
    assert!(
        !open.is_empty() && open.iter().all(|(health, _)| health == "up"),
        "{open:?}"
    );
}

/// A server Slack can send people back to, with the admin page on.
const SLACK_SERVER: &str = "[server]\nlisten = \"127.0.0.1:0\"\n\
                            public_url = \"https://status.acme.example/\"\n\n\
                            [server.auth]\nmode = \"trusted_header\"\n\
                            trusted_proxies = [\"10.0.0.0/8\"]\n\n";
const SLACK_APP: &str = "[slack]\nclient_id = \"${ENV:TOCSIN_TEST_SLACK_ID}\"\n\
                         client_secret = \"${ENV:TOCSIN_TEST_SLACK_SECRET}\"\n\n";
const SLACK_NOTIFIER: &str = "[[notifier]]\nid = \"ops-slack\"\ntype = \"slack\"\n\n\
                              [notifier.slack]\nconnection = \"ops-alerts\"\n";

fn folder_with(toml: &str) -> tempfile::TempDir {
    let config = tempfile::tempdir().unwrap();
    std::fs::write(config.path().join("server.toml"), toml).unwrap();
    config
}

#[test]
fn slack_setup_prints_a_create_link_holding_the_manifest() {
    // The client ID and secret need not be set yet: the manifest uses
    // neither.
    let config = folder_with(&format!("{SLACK_SERVER}{SLACK_APP}"));
    let output = tocsin(&[
        "--json",
        "slack",
        "setup",
        "--config",
        config.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let report = json_stdout(&output);
    let manifest = &report["manifest"];
    assert_eq!(
        manifest["oauth_config"]["redirect_urls"],
        serde_json::json!(["https://status.acme.example/slack/callback"])
    );
    assert_eq!(
        manifest["oauth_config"]["scopes"],
        serde_json::json!({"bot": ["incoming-webhook"]})
    );
    assert_eq!(
        manifest["settings"]["incoming_webhooks"]["incoming_webhooks_enabled"],
        true
    );
    let link = report["create_link"].as_str().unwrap();
    let encoded = link
        .strip_prefix("https://api.slack.com/apps?new_app=1&manifest_json=")
        .unwrap_or_else(|| panic!("{link}"));
    let decoded: String = url::form_urlencoded::parse(format!("m={encoded}").as_bytes())
        .next()
        .unwrap()
        .1
        .into_owned();
    assert_eq!(&serde_json::from_str::<Value>(&decoded).unwrap(), manifest);
    let text = tocsin(&[
        "slack",
        "setup",
        "--config",
        config.path().to_str().unwrap(),
    ]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains(link), "{text}");
    assert!(text.contains("Never in a file"), "{text}");
}

#[test]
fn slack_setup_needs_an_https_public_url() {
    let config = folder_with(
        "[server]\nlisten = \"127.0.0.1:0\"\npublic_url = \"http://status.acme.example\"\n",
    );
    let output = tocsin(&[
        "--json",
        "slack",
        "setup",
        "--config",
        config.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let error = json_stdout(&output)["error"].to_string();
    assert!(error.contains("only to https addresses"), "{error}");
}

#[test]
fn config_check_warns_of_a_slack_connection_not_connected() {
    let data = tempfile::tempdir().unwrap();
    for (toml, wanted) in [
        (
            format!("{SLACK_SERVER}{SLACK_APP}{SLACK_NOTIFIER}"),
            vec![
                "notifier `ops-slack` names Slack connection `ops-alerts`, which is not connected; its alerts fail until it is connected on /admin/slack",
            ],
        ),
        (
            format!("{SLACK_SERVER}{SLACK_NOTIFIER}"),
            vec![
                "notifiers name Slack connections, but without `[slack]` none can be connected",
                "notifier `ops-slack` names Slack connection `ops-alerts`, which is not connected; its alerts fail until it is connected on /admin/slack",
            ],
        ),
        (format!("{SLACK_SERVER}{SLACK_APP}"), vec![]),
    ] {
        let config = folder_with(&toml);
        let output = Command::new(env!("CARGO_BIN_EXE_tocsin"))
            .args([
                "--json",
                "config",
                "check",
                "--config",
                config.path().to_str().unwrap(),
                "--data",
                data.path().to_str().unwrap(),
            ])
            .env("TOCSIN_TEST_SLACK_ID", "1111.2222")
            .env("TOCSIN_TEST_SLACK_SECRET", "acme-client-secret")
            .output()
            .unwrap();
        // A warning, not an error: the channel is connected after the
        // configuration is valid.
        assert_eq!(output.status.code(), Some(0), "{toml}\n{output:?}");
        let report = json_stdout(&output);
        assert_eq!(report["warnings"], serde_json::json!(wanted), "{toml}");
    }
    // Nothing written to the data folder.
    assert_eq!(std::fs::read_dir(data.path()).unwrap().count(), 0);
}

/// What `tocsin incident new` sends to a server that answers 500 after
/// reading one request: the JSON body of that request.
fn body_sent_by_incident_new(extra: &[&str]) -> Value {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        let body = loop {
            let read = stream.read(&mut chunk).unwrap();
            request.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&request).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .to_ascii_lowercase()
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:").map(str::to_owned))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                if body.len() >= length {
                    break body.to_owned();
                }
            }
            assert!(read > 0, "the client closed before sending its body");
        };
        stream
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .unwrap();
        body
    });
    let mut args = vec![
        "incident",
        "new",
        "--url",
        &url,
        "--token",
        "t",
        "--title",
        "API errors",
        "--message",
        "Looking into it.",
    ];
    args.extend_from_slice(extra);
    tocsin(&args);
    serde_json::from_str(&server.join().unwrap()).unwrap()
}

#[test]
fn incident_new_quiet_sends_notify_false_and_the_default_leaves_it_out() {
    let quiet = body_sent_by_incident_new(&["--quiet"]);
    assert_eq!(quiet["notify"], false, "{quiet}");
    // The default is the field's absence: the server's default is to tell.
    let loud = body_sent_by_incident_new(&[]);
    assert!(loud.get("notify").is_none(), "{loud}");
}

#[test]
fn incident_new_draft_sends_draft_true_and_the_default_leaves_it_out() {
    let draft = body_sent_by_incident_new(&["--draft"]);
    assert_eq!(draft["draft"], true, "{draft}");
    // Absent, the server decides from the token's scope.
    let plain = body_sent_by_incident_new(&[]);
    assert!(plain.get("draft").is_none(), "{plain}");
}
