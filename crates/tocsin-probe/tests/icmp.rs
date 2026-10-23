// SPDX-License-Identifier: AGPL-3.0-only

//! The `icmp` probe against 127.0.0.1.
//!
//! An ICMP socket needs `net.ipv4.ping_group_range` to include the test's
//! group, or `CAP_NET_RAW`. Where neither is available the probe returns a
//! setup error; that error is asserted and the reply check is skipped with
//! the reason logged, rather than passing silently.

mod support;

use tocsin_core::Health;
use tocsin_core::probe::{Facts, ProbeErrorKind};
use tocsin_probe::{ProbeContext, run};

use support::check;

#[tokio::test]
async fn loopback_answers() {
    let check = check(
        r#"
[[check]]
id = "loopback"
type = "icmp"
target = "127.0.0.1"
timeout = "2s"

[[check.assert]]
latency_ms = { lt = 1000 }
"#,
    );
    let outcome = run(
        &check,
        &ProbeContext::with_roots(rustls::RootCertStore::empty()),
    )
    .await;
    if let Some(error) = &outcome.error
        && error.kind == ProbeErrorKind::Setup
    {
        assert!(
            error.message.contains("ping_group_range"),
            "{}",
            error.message
        );
        assert!(error.message.contains("CAP_NET_RAW"), "{}", error.message);
        assert_eq!(outcome.health, Health::Down);
        eprintln!(
            "SKIPPED icmp reply check: this sandbox cannot open an ICMP socket: {}",
            error.message
        );
        return;
    }
    let Some(Facts::Icmp(facts)) = &outcome.facts else {
        panic!("expected ICMP facts, got {outcome:?}");
    };
    assert_eq!(facts.address, "127.0.0.1");
    assert!(outcome.assertions[0].passed);
    assert_eq!(outcome.health, Health::Up);
}
