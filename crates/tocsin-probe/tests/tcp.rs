// SPDX-License-Identifier: AGPL-3.0-only

//! The `tcp` probe against local listeners.

mod support;

use std::time::Duration;

use tocsin_core::Health;
use tocsin_core::probe::{Facts, ProbeErrorKind, ProbeOutcome, TcpFacts};
use tocsin_probe::{ProbeContext, run};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use support::check;

fn context() -> ProbeContext {
    ProbeContext::with_roots(rustls::RootCertStore::empty())
}

fn tcp(outcome: &ProbeOutcome) -> &TcpFacts {
    match &outcome.facts {
        Some(Facts::Tcp(facts)) => facts,
        other => panic!("expected TCP facts, got {other:?} ({:?})", outcome.error),
    }
}

/// A server that greets every connection with `greeting`.
async fn greeter(greeting: &'static [u8]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = stream.write_all(greeting).await;
                // Hold the connection open, as a real server would.
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
        }
    });
    port
}

#[tokio::test]
async fn a_connection_without_a_banner_assertion_reads_nothing() {
    let port = greeter(b"SSH-2.0-OpenSSH_9.9\r\n").await;
    let check = check(&format!(
        r#"
[[check]]
id = "ssh"
type = "tcp"
target = "127.0.0.1:{port}"
"#
    ));
    let outcome = run(&check, &context()).await;
    let facts = tcp(&outcome);
    assert_eq!(facts.address, format!("127.0.0.1:{port}"));
    assert_eq!(facts.banner, None);
    assert_eq!(outcome.assertions.len(), 0);
    assert_eq!(outcome.health, Health::Up);
}

#[tokio::test]
async fn the_banner_is_read_to_the_first_newline() {
    let port = greeter(b"SSH-2.0-OpenSSH_9.9\r\nmore after the line").await;
    let check = check(&format!(
        r#"
[[check]]
id = "ssh"
type = "tcp"
target = "127.0.0.1:{port}"

[[check.assert]]
banner = {{ regex = "^SSH-2\\.0-" }}

[[check.assert]]
banner = {{ contains = "Dropbear" }}
on_fail = "degraded"
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(tcp(&outcome).banner.as_deref(), Some("SSH-2.0-OpenSSH_9.9"));
    let passed: Vec<bool> = outcome
        .assertions
        .iter()
        .map(|verdict| verdict.passed)
        .collect();
    assert_eq!(passed, [true, false]);
    assert_eq!(outcome.health, Health::Degraded);
}

#[tokio::test]
async fn send_then_expect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = [0_u8; 64];
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            let reply: &[u8] = if &buffer[..read] == b"PING\r\n" {
                b"+PONG\r\n"
            } else {
                b"-ERR\r\n"
            };
            let _ = stream.write_all(reply).await;
        }
    });
    let check = check(&format!(
        r#"
[[check]]
id = "redis"
type = "tcp"
target = "127.0.0.1:{port}"

[check.tcp]
send = "PING\r\n"

[[check.assert]]
banner = {{ contains = "+PONG" }}
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(tcp(&outcome).banner.as_deref(), Some("+PONG"));
    assert_eq!(outcome.health, Health::Up);
}

#[tokio::test]
async fn a_banner_without_a_newline_ends_at_a_pause() {
    let port = greeter(b"220 ready").await;
    let check = check(&format!(
        r#"
[[check]]
id = "smtp"
type = "tcp"
target = "127.0.0.1:{port}"
timeout = "3s"

[[check.assert]]
banner = {{ contains = "220" }}
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(tcp(&outcome).banner.as_deref(), Some("220 ready"));
    assert_eq!(outcome.health, Health::Up);
    assert!(outcome.duration_ms < 2000, "took {}ms", outcome.duration_ms);
}

#[tokio::test]
async fn a_refused_connection_is_down() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("address").port()
    };
    let check = check(&format!(
        r#"
[[check]]
id = "closed"
type = "tcp"
target = "127.0.0.1:{port}"
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(outcome.health, Health::Down);
    assert_eq!(
        outcome.error.expect("an error").kind,
        ProbeErrorKind::Connect
    );
}

#[tokio::test]
async fn a_silent_server_times_out_waiting_for_the_banner() {
    let port = greeter(b"").await;
    let check = check(&format!(
        r#"
[[check]]
id = "silent"
type = "tcp"
target = "127.0.0.1:{port}"
timeout = "300ms"

[[check.assert]]
banner = {{ contains = "hello" }}
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(outcome.health, Health::Down);
    assert_eq!(
        outcome.error.expect("an error").kind,
        ProbeErrorKind::Timeout
    );
}

#[tokio::test]
async fn send_without_a_banner_assertion_does_not_wait_for_a_reply() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    let (report, reports) = std::sync::mpsc::channel();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let report = report.clone();
            tokio::spawn(async move {
                // Read what is sent, answer nothing, hold the connection.
                let mut buffer = [0_u8; 64];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                let _ = report.send(buffer[..read].to_vec());
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
        }
    });
    let check = check(&format!(
        r#"
[[check]]
id = "silent-sink"
type = "tcp"
target = "127.0.0.1:{port}"
timeout = "1s"

[check.tcp]
send = "PING\r\n"
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(outcome.error, None);
    assert_eq!(tcp(&outcome).banner, None);
    assert_eq!(outcome.health, Health::Up);
    // The server task runs on this test's thread, so wait without blocking it.
    let sent = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(sent) = reports.try_recv() {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server received the send");
    assert_eq!(sent, b"PING\r\n");
}
