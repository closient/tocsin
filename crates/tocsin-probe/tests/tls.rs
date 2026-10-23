// SPDX-License-Identifier: AGPL-3.0-only

//! The `tls` probe against a local TLS server with rcgen certificates.

mod support;

use tocsin_core::Health;
use tocsin_core::probe::{CertificateFacts, Facts, ProbeErrorKind, ProbeOutcome};
use tocsin_probe::{ProbeContext, run};

use support::{Authority, check, https_server};

fn certificate(outcome: &ProbeOutcome) -> &CertificateFacts {
    match &outcome.facts {
        Some(Facts::Tls(facts)) => &facts.certificate,
        other => panic!("expected TLS facts, got {other:?} ({:?})", outcome.error),
    }
}

fn tls_check(port: u16, server_name: &str, assertions: &str) -> tocsin_core::probe::CompiledCheck {
    check(&format!(
        r#"
[[check]]
id = "certificate"
type = "tls"
target = "127.0.0.1:{port}"

[check.tls]
server_name = "{server_name}"
{assertions}
"#
    ))
}

#[tokio::test]
async fn a_valid_certificate_is_up() {
    let authority = Authority::new("Probe Test CA");
    let port = https_server(&authority.leaf(&["localhost", "www.localhost"], 30)).await;
    let context = ProbeContext::with_roots(authority.roots());
    let outcome = run(&tls_check(port, "localhost", ""), &context).await;
    let certificate = certificate(&outcome);
    assert!(certificate.chain_valid);
    assert!(certificate.hostname_valid);
    assert_eq!(certificate.chain_error, None);
    assert_eq!(certificate.days_remaining, Some(30));
    assert_eq!(certificate.chain[0].subject, "CN=probe test server");
    assert_eq!(certificate.chain[0].issuer, "CN=Probe Test CA");
    assert_eq!(
        certificate.chain[0].subject_alternative_names,
        ["localhost", "www.localhost"]
    );
    // Implicit verdicts: chain_valid and hostname_valid.
    let conditions: Vec<&str> = outcome
        .assertions
        .iter()
        .map(|verdict| verdict.condition.as_str())
        .collect();
    assert_eq!(conditions, ["chain_valid", "hostname_valid"]);
    assert_eq!(outcome.health, Health::Up);
}

#[tokio::test]
async fn a_name_mismatch_is_reported_with_a_valid_chain() {
    let authority = Authority::new("Probe Test CA");
    let port = https_server(&authority.leaf(&["localhost"], 30)).await;
    let context = ProbeContext::with_roots(authority.roots());
    let outcome = run(&tls_check(port, "other.example", ""), &context).await;
    let certificate = certificate(&outcome);
    assert!(certificate.chain_valid);
    assert!(!certificate.hostname_valid);
    assert_eq!(outcome.health, Health::Down);
}

#[tokio::test]
async fn an_untrusted_chain_is_reported() {
    let authority = Authority::new("Probe Test CA");
    let port = https_server(&authority.leaf(&["localhost"], 30)).await;
    let context = ProbeContext::with_roots(Authority::new("Other CA").roots());
    let outcome = run(&tls_check(port, "localhost", ""), &context).await;
    let certificate = certificate(&outcome);
    assert!(!certificate.chain_valid);
    assert!(
        certificate
            .chain_error
            .as_deref()
            .is_some_and(|error| error.contains("UnknownIssuer")),
        "{:?}",
        certificate.chain_error
    );
    assert!(certificate.hostname_valid);
    assert_eq!(outcome.health, Health::Down);
}

#[tokio::test]
async fn expiry_assertions_map_to_their_on_fail_state() {
    let authority = Authority::new("Probe Test CA");
    let port = https_server(&authority.leaf(&["localhost"], 10)).await;
    let context = ProbeContext::with_roots(authority.roots());
    let outcome = run(
        &tls_check(
            port,
            "localhost",
            r#"
[[check.assert]]
certificate_days_remaining = { gte = 14 }
on_fail = "degraded"

[[check.assert]]
certificate_days_remaining = { gte = 3 }

[[check.assert]]
hostname_valid = true
"#,
        ),
        &context,
    )
    .await;
    assert_eq!(certificate(&outcome).days_remaining, Some(10));
    let passed: Vec<bool> = outcome
        .assertions
        .iter()
        .map(|verdict| verdict.passed)
        .collect();
    assert_eq!(passed, [false, true, true]);
    assert_eq!(outcome.health, Health::Degraded);
}

#[tokio::test]
async fn an_expired_certificate_fails_the_chain() {
    let authority = Authority::new("Probe Test CA");
    let port = https_server(&authority.leaf(&["localhost"], -2)).await;
    let context = ProbeContext::with_roots(authority.roots());
    let outcome = run(
        &tls_check(
            port,
            "localhost",
            r"
[[check.assert]]
certificate_days_remaining = { gte = 0 }
",
        ),
        &context,
    )
    .await;
    let certificate = certificate(&outcome);
    assert!(!certificate.chain_valid);
    assert!(certificate.days_remaining.is_some_and(|days| days < 0));
    assert_eq!(outcome.health, Health::Down);
}

#[tokio::test]
async fn a_server_that_does_not_speak_tls_is_a_tls_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(b"220 not tls\r\n").await;
        }
    });
    let context = ProbeContext::with_roots(Authority::new("Probe Test CA").roots());
    let outcome = run(&tls_check(port, "localhost", ""), &context).await;
    assert_eq!(outcome.health, Health::Down);
    assert_eq!(outcome.error.expect("an error").kind, ProbeErrorKind::Tls);
}
