// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of the CLI output contract, run against the built binary.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

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
    assert_eq!(info["schema_version"], 4);
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
    assert_eq!(info["supported_schema_version"], 4);
    assert_eq!(info["rows"]["check_result"], 4);
    assert_eq!(user_version(&data), 99);

    let text = tocsin(&["db", "info", "--data", data_arg(&data)]);
    assert!(
        String::from_utf8_lossy(&text.stdout).contains("newer than this build's 4"),
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
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
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
