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
        let child = Command::new(env!("CARGO_BIN_EXE_tocsin"))
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
    let config = tempfile::tempdir().unwrap();
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
    let config = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut server = Server::start(config.path(), data.path());
    drop(wait_ready(&mut server));
    server.signal("-TERM");
    assert_eq!(server.wait_with_deadline().code(), Some(0));
}

/// Nor because whatever reads its result went away: the exit code still holds.
#[test]
fn serve_survives_a_closed_result_pipe() {
    let config = tempfile::tempdir().unwrap();
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
    let config = tempfile::tempdir().unwrap();
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
    assert_eq!(json_stdout(&output)["error"]["kind"], "invalid_input");
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
