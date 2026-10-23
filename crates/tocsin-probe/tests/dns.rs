// SPDX-License-Identifier: AGPL-3.0-only

//! The `dns` probe against an in-process UDP DNS server.

mod support;

use std::net::Ipv4Addr;

use hickory_proto::op::{Message, MessageType, OpCode, ResponseCode};
use hickory_proto::rr::rdata::{A, TXT};
use hickory_proto::rr::{RData, Record, RecordType};
use tocsin_core::Health;
use tocsin_core::probe::{DnsFacts, Facts, ProbeErrorKind, ProbeOutcome};
use tocsin_probe::{ProbeContext, run};
use tokio::net::UdpSocket;

use support::check;

/// Answers `app.test` with two documentation addresses and a TXT record,
/// `refused.test` with REFUSED, ignores `silent.test`, and answers
/// everything else with NXDOMAIN.
async fn server() -> u16 {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let port = socket.local_addr().expect("address").port();
    tokio::spawn(async move {
        let mut buffer = [0_u8; 512];
        while let Ok((length, peer)) = socket.recv_from(&mut buffer).await {
            let Ok(request) = Message::from_vec(&buffer[..length]) else {
                continue;
            };
            let Some(query) = request.queries.first().cloned() else {
                continue;
            };
            let name = query.name().to_ascii().to_ascii_lowercase();
            if name == "silent.test." {
                continue;
            }
            let mut response =
                Message::new(request.metadata.id, MessageType::Response, OpCode::Query);
            response.metadata.recursion_desired = request.metadata.recursion_desired;
            response.metadata.recursion_available = true;
            response.add_query(query.clone());
            match (name.as_str(), query.query_type()) {
                ("app.test.", RecordType::A) => {
                    for last in [11, 10] {
                        response.add_answer(Record::from_rdata(
                            query.name().clone(),
                            60,
                            RData::A(A(Ipv4Addr::new(192, 0, 2, last))),
                        ));
                    }
                }
                ("app.test.", RecordType::TXT) => {
                    response.add_answer(Record::from_rdata(
                        query.name().clone(),
                        60,
                        RData::TXT(TXT::new(vec!["v=spf1 -all".to_owned()])),
                    ));
                }
                ("app.test.", _) => {}
                ("refused.test.", _) => response.metadata.response_code = ResponseCode::Refused,
                _ => response.metadata.response_code = ResponseCode::NXDomain,
            }
            let bytes = response.to_vec().expect("encodable response");
            let _ = socket.send_to(&bytes, peer).await;
        }
    });
    port
}

fn context() -> ProbeContext {
    ProbeContext::with_roots(rustls::RootCertStore::empty())
}

fn dns(outcome: &ProbeOutcome) -> &DnsFacts {
    match &outcome.facts {
        Some(Facts::Dns(facts)) => facts,
        other => panic!("expected DNS facts, got {other:?} ({:?})", outcome.error),
    }
}

fn dns_check(
    port: u16,
    name: &str,
    record_type: &str,
    assertions: &str,
) -> tocsin_core::probe::CompiledCheck {
    check(&format!(
        r#"
[[check]]
id = "lookup"
type = "dns"
target = "{name}"
timeout = "2s"

[check.dns]
resolver = "127.0.0.1:{port}"
record_type = "{record_type}"
{assertions}
"#
    ))
}

#[tokio::test]
async fn answers_and_record_assertions() {
    let port = server().await;
    let outcome = run(
        &dns_check(
            port,
            "app.test",
            "A",
            r#"
[[check.assert]]
records = { contains = "192.0.2.10" }

[[check.assert]]
records = { eq = ["192.0.2.10", "192.0.2.11"] }

[[check.assert]]
records = { eq = ["192.0.2.10"] }
on_fail = "degraded"
"#,
        ),
        &context(),
    )
    .await;
    let facts = dns(&outcome);
    assert_eq!(facts.rcode, "NOERROR");
    assert_eq!(facts.resolver, format!("127.0.0.1:{port}"));
    let mut answers = facts.answers.clone();
    answers.sort();
    assert_eq!(answers, ["192.0.2.10", "192.0.2.11"]);
    let passed: Vec<bool> = outcome
        .assertions
        .iter()
        .map(|verdict| verdict.passed)
        .collect();
    assert_eq!(passed, [true, true, false]);
    assert_eq!(outcome.health, Health::Degraded);
}

#[tokio::test]
async fn txt_records_are_text() {
    let port = server().await;
    let outcome = run(
        &dns_check(
            port,
            "app.test",
            "TXT",
            r#"
[[check.assert]]
records = { contains = "v=spf1 -all" }
"#,
        ),
        &context(),
    )
    .await;
    assert_eq!(dns(&outcome).answers, ["v=spf1 -all"]);
    assert_eq!(outcome.health, Health::Up);
}

#[tokio::test]
async fn nxdomain_fails_the_implicit_verdict() {
    let port = server().await;
    let outcome = run(&dns_check(port, "missing.test", "A", ""), &context()).await;
    let facts = dns(&outcome);
    assert_eq!(facts.rcode, "NXDOMAIN");
    assert_eq!(facts.answers.len(), 0);
    assert_eq!(outcome.assertions[0].condition, "rcode");
    assert!(outcome.assertions[0].implicit);
    assert_eq!(outcome.health, Health::Down);
}

#[tokio::test]
async fn an_empty_answer_is_noerror() {
    let port = server().await;
    let outcome = run(&dns_check(port, "app.test", "MX", ""), &context()).await;
    let facts = dns(&outcome);
    assert_eq!(facts.rcode, "NOERROR");
    assert_eq!(facts.answers.len(), 0);
    assert_eq!(outcome.health, Health::Up);
}

#[tokio::test]
async fn refused_is_reported_as_the_rcode() {
    let port = server().await;
    let outcome = run(&dns_check(port, "refused.test", "A", ""), &context()).await;
    assert_eq!(dns(&outcome).rcode, "REFUSED");
    assert_eq!(outcome.health, Health::Down);
}

#[tokio::test]
async fn a_silent_resolver_times_out() {
    let port = server().await;
    let outcome = run(&dns_check(port, "silent.test", "A", ""), &context()).await;
    assert_eq!(outcome.health, Health::Down);
    assert_eq!(
        outcome.error.expect("an error").kind,
        ProbeErrorKind::Timeout
    );
}
