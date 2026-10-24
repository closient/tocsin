// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin serve` and the OTLP export: with no `[telemetry.otlp]`, or
//! with `OTEL_SDK_DISABLED=true`, it never connects to a collector, even
//! with the `OTEL_*` variables set; with the table, the same variables
//! send it there, and it sends once more as it stops.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CONFIG: &str = r#"
[server]
listen = "127.0.0.1:0"

[[heartbeat]]
id = "backup"
interval = "1h"
"#;

const READY: &str = "waiting for SIGINT or SIGTERM";

/// A running `tocsin serve`, killed on drop.
struct Server {
    child: Child,
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
    /// Serves `config` with the export's variables pointing at
    /// `collector`, sending every second, and `environment` on top.
    fn start(config_text: &str, collector: &TcpListener, environment: &[(&str, &str)]) -> Self {
        let config = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(config.path().join("tocsin.toml"), config_text).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_tocsin"))
            .args(["--json", "serve", "--config"])
            .arg(config.path())
            .arg("--data")
            .arg(data.path())
            .env(
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                format!("http://{}", collector.local_addr().unwrap()),
            )
            .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")
            .env("OTEL_METRIC_EXPORT_INTERVAL", "1000")
            .envs(environment.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("tocsin serve starts");
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        // What it logs while starting is kept with the rest.
        let mut starting = Vec::new();
        for line in lines.by_ref().map_while(Result::ok) {
            let ready = line.contains(READY);
            starting.push(line);
            if ready {
                break;
            }
        }
        assert!(
            starting.last().is_some_and(|line| line.contains(READY)),
            "serve exited before becoming ready: {starting:?}"
        );
        let log = std::thread::spawn(move || {
            starting
                .into_iter()
                .chain(lines.map_while(Result::ok))
                .collect()
        });
        Server {
            child,
            log: Some(log),
            _config: config,
            _data: data,
        }
    }

    /// Stops the server with SIGTERM: its exit code and what it logged.
    fn stop(mut self) -> (Option<i32>, String) {
        let sent = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
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
        let log = self.log.take().unwrap().join().unwrap().join("\n");
        (status.code(), log)
    }
}

/// Whether anything connects to `collector` within `wait`.
fn connected_within(collector: &TcpListener, wait: Duration) -> bool {
    collector.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        match collector.accept() {
            Ok(_) => return true,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("cannot accept: {error}"),
        }
    }
    false
}

/// The variables alone start nothing: over three export intervals no
/// connection reaches the collector they name.
#[test]
fn without_the_table_serve_never_connects_to_a_collector() {
    let collector = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = Server::start(CONFIG, &collector, &[]);
    assert!(
        !connected_within(&collector, Duration::from_secs(3)),
        "serve connected to the collector with no [telemetry.otlp]"
    );
    let (code, log) = server.stop();
    assert_eq!(code, Some(0), "{log}");
    assert!(!log.contains("OTLP"), "{log}");
}

/// The control: the same variables with an empty table do send, so the
/// test above could have seen a connection; the collector dropping it is
/// logged; and serve still stops cleanly with the exporter running.
#[test]
fn with_the_table_serve_sends_where_the_variables_say() {
    let collector = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = Server::start(&format!("{CONFIG}\n[telemetry.otlp]\n"), &collector, &[]);
    assert!(
        connected_within(&collector, Duration::from_secs(5)),
        "serve never connected to the collector"
    );
    let (code, log) = server.stop();
    assert_eq!(code, Some(0), "{log}");
    // The collector dropped the connection it accepted: a failure, logged.
    assert!(
        log.contains("OTLP metric export failed, counted in tocsin_otlp_export_failures_total"),
        "{log}"
    );
}

/// `OTEL_SDK_DISABLED=true` turns the table off: over three export
/// intervals nothing connects, and serve says why.
#[test]
fn otel_sdk_disabled_stops_serve_from_exporting() {
    let collector = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = Server::start(
        &format!("{CONFIG}\n[telemetry.otlp]\n"),
        &collector,
        &[("OTEL_SDK_DISABLED", "true")],
    );
    assert!(
        !connected_within(&collector, Duration::from_secs(3)),
        "serve connected to the collector with OTEL_SDK_DISABLED=true"
    );
    let (code, log) = server.stop();
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("OTEL_SDK_DISABLED=true turns the OpenTelemetry export off"),
        "{log}"
    );
    assert!(!log.contains("sending metrics over OTLP"), "{log}");
}

/// One HTTP request on `stream`: its request line and body, after which
/// it is answered 200 with an empty OTLP response.
fn answer(mut stream: std::net::TcpStream) -> (String, Vec<u8>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut read = Vec::new();
    let mut buffer = [0; 4096];
    let head = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "the request ended early");
        read.extend_from_slice(&buffer[..count]);
        if let Some(end) = read.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&read[..head]).to_ascii_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .expect("no content-length")
        .trim()
        .parse()
        .unwrap();
    while read.len() < head + length {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "the body ended early");
        read.extend_from_slice(&buffer[..count]);
    }
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\n\
              content-length: 0\r\nconnection: close\r\n\r\n",
        )
        .unwrap();
    let line = headers.lines().next().unwrap_or_default().to_owned();
    (line, read[head..head + length].to_vec())
}

/// With an hour between exports, the only one is the last, sent as serve
/// stops on SIGTERM; serve waits for the collector to take it.
#[test]
fn serve_sends_the_last_export_as_it_stops() {
    let collector = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = Server::start(
        &format!("{CONFIG}\n[telemetry.otlp]\n"),
        &collector,
        &[("OTEL_METRIC_EXPORT_INTERVAL", "3600000")],
    );
    assert!(
        !connected_within(&collector, Duration::from_secs(1)),
        "serve exported before it was asked to stop"
    );
    let receiving = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match collector.accept() {
                Ok((stream, _)) => return answer(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "nothing was sent at shutdown");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("cannot accept: {error}"),
            }
        }
    });
    let (code, log) = server.stop();
    let (line, body) = receiving.join().unwrap();
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(line, "post /v1/metrics http/1.1");
    assert_ne!(body.len(), 0, "an empty export");
    assert!(!log.contains("OTLP metric export failed"), "{log}");
}
