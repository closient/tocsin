// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of the CLI output contract, run against the built binary.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
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

/// Starts `tocsin serve` and returns once it is ready for signals.
///
/// Stderr is drained on a thread for the life of the process, so the
/// server never writes into a closed pipe; the thread returns all of it.
fn spawn_serve(config: &str, data: &str) -> (Child, JoinHandle<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .args(["--json", "serve", "--config", config, "--data", data])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tocsin serve starts");
    let stderr = child.stderr.take().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let drain = std::thread::spawn(move || {
        let mut log = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("waiting for SIGINT or SIGTERM") {
                let _ = ready_tx.send(());
            }
            log.push_str(&line);
            log.push('\n');
        }
        log
    });
    if ready_rx.recv_timeout(Duration::from_secs(10)).is_err() {
        let _ = child.kill();
        panic!(
            "serve never became ready; stderr:\n{}",
            drain.join().unwrap()
        );
    }
    (child, drain)
}

fn wait_with_deadline(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "serve did not stop within 10s");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stops_cleanly_on(signal: &str, name: &str) {
    let config = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let data = parent.path().join("data");
    let (mut child, drain) = spawn_serve(config.path().to_str().unwrap(), data.to_str().unwrap());

    let sent = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());

    let status = wait_with_deadline(&mut child);
    let output = child.wait_with_output().unwrap();
    let log = drain.join().unwrap();
    assert_eq!(status.code(), Some(0), "stderr:\n{log}");
    assert_eq!(
        json_stdout(&output),
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
    let mut child = Command::new(env!("CARGO_BIN_EXE_tocsin"))
        .args(["--json", "serve", "--config"])
        .arg(config.path())
        .arg("--data")
        .arg(data.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tocsin serve starts");
    let ready = BufReader::new(child.stderr.take().unwrap())
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains("waiting for SIGINT or SIGTERM"));
    // The reader is dropped here, so the next log line hits a broken pipe.
    assert!(ready, "serve exited before becoming ready");

    let sent = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    assert_eq!(wait_with_deadline(&mut child).code(), Some(0));
}
