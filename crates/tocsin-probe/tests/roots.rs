// SPDX-License-Identifier: AGPL-3.0-only

//! A context without root certificates: the checks that verify them fail
//! with a setup error without being attempted, and the rest still run.

mod support;

use tocsin_core::Health;
use tocsin_core::probe::{ProbeError, ProbeErrorKind};
use tocsin_probe::{ProbeContext, run};
use tokio::net::TcpListener;

use support::check;

fn rootless() -> ProbeContext {
    ProbeContext::without_roots(ProbeError::new(
        ProbeErrorKind::Setup,
        "no root certificates could be loaded from the system",
    ))
}

#[tokio::test]
async fn http_and_tls_checks_fail_with_the_setup_error() {
    let context = rootless();
    for text in [
        "[[check]]\nid = \"web\"\ntype = \"http\"\ntarget = \"https://127.0.0.1:1/\"\n",
        "[[check]]\nid = \"cert\"\ntype = \"tls\"\ntarget = \"127.0.0.1:1\"\n",
    ] {
        let compiled = check(text);
        let reason = context.cannot_run(compiled.check()).cloned();
        let outcome = run(&compiled, &context).await;
        assert_eq!(outcome.health, Health::Down);
        assert_eq!(outcome.facts, None, "never attempted");
        let error = outcome.error.expect("an error");
        assert_eq!(error.kind, ProbeErrorKind::Setup);
        assert_eq!(
            error.message,
            "no root certificates could be loaded from the system"
        );
        assert_eq!(reason, Some(error));
    }
}

#[tokio::test]
async fn other_checks_still_run() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let compiled = check(&format!(
        "[[check]]\nid = \"db\"\ntype = \"tcp\"\ntarget = \"{address}\"\n"
    ));
    let context = rootless();
    assert_eq!(context.cannot_run(compiled.check()), None);
    let outcome = run(&compiled, &context).await;
    assert_eq!(outcome.health, Health::Up, "{:?}", outcome.error);
}

#[tokio::test]
async fn a_context_with_roots_runs_everything() {
    let compiled =
        check("[[check]]\nid = \"web\"\ntype = \"http\"\ntarget = \"https://127.0.0.1:1/\"\n");
    let context = ProbeContext::with_roots(rustls::RootCertStore::empty());
    assert_eq!(context.cannot_run(compiled.check()), None);
    let outcome = run(&compiled, &context).await;
    assert_ne!(
        outcome.error.map(|error| error.kind),
        Some(ProbeErrorKind::Setup),
        "attempted, and refused by the closed port"
    );
}
