// SPDX-License-Identifier: AGPL-3.0-only

//! What the probe tests share: loading a check from TOML the way the
//! configuration loader does, and certificates for local TLS servers.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, date_time_ymd};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tocsin_core::config::{Environment, Source, load};
use tocsin_core::probe::CompiledCheck;
use tocsin_core::probe::rfc3339;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// The secrets tests may reference.
fn variable(name: &str) -> Option<String> {
    match name {
        "API_TOKEN" => Some("s3cret-token".to_owned()),
        "API_PASSWORD" => Some("hunter2".to_owned()),
        _ => None,
    }
}

/// The only check in `text`, loaded, validated and compiled like a real
/// file.
pub fn check(text: &str) -> CompiledCheck {
    let environment = Environment {
        hostname: "probe-test".to_owned(),
        node_id: None,
        variable: &variable,
    };
    let source = Source {
        name: "checks.toml".to_owned(),
        text: Ok(text.to_owned()),
    };
    let mut config = load(&[source], &environment).unwrap_or_else(|diagnostics| {
        panic!("the test configuration is invalid: {diagnostics:#?}");
    });
    assert_eq!(config.checks.len(), 1, "one check per test configuration");
    CompiledCheck::new(config.checks.remove(0)).expect("a loaded check compiles")
}

/// A certificate authority for tests.
pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    certificate: CertificateDer<'static>,
}

impl Authority {
    /// A fresh root named `name`.
    pub fn new(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA parameters");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        let key = KeyPair::generate().expect("CA key");
        let certificate = params.self_signed(&key).expect("CA certificate");
        Authority {
            issuer: Issuer::new(params, key),
            certificate: certificate.der().clone(),
        }
    }

    /// A root store trusting only this authority.
    pub fn roots(&self) -> RootCertStore {
        let mut roots = RootCertStore::empty();
        roots
            .add(self.certificate.clone())
            .expect("CA is a valid root");
        roots
    }

    /// A server certificate for `names` expiring an hour after
    /// `days` whole days from now (negative for the past).
    pub fn leaf(&self, names: &[&str], days: i64) -> Leaf {
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let mut params = CertificateParams::new(names).expect("leaf parameters");
        params
            .distinguished_name
            .push(DnType::CommonName, "probe test server");
        params.not_before = date_time_ymd(2020, 1, 1);
        let (year, month, day, seconds_today) = today();
        // An hour past this time of day, so the whole days remaining are
        // exactly `days` whenever the test runs.
        let offset = days * 86_400 + seconds_today + 3_600;
        let midnight = date_time_ymd(year, month, day);
        let magnitude = Duration::from_secs(offset.unsigned_abs());
        params.not_after = if offset >= 0 {
            midnight + magnitude
        } else {
            midnight - magnitude
        };
        let key = KeyPair::generate().expect("leaf key");
        let certificate = params
            .signed_by(&key, &self.issuer)
            .expect("leaf certificate");
        Leaf {
            chain: vec![certificate.der().clone(), self.certificate.clone()],
            key: PrivatePkcs8KeyDer::from(key.serialize_der()),
        }
    }
}

/// A server certificate, its chain and key.
pub struct Leaf {
    chain: Vec<CertificateDer<'static>>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Leaf {
    /// A TLS acceptor serving this certificate.
    pub fn acceptor(&self) -> TlsAcceptor {
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            self.chain.clone(),
            PrivateKeyDer::Pkcs8(self.key.clone_key()),
        )
        .expect("server configuration");
        TlsAcceptor::from(Arc::new(config))
    }
}

/// Serves TLS on a local port, answering every request with a small HTTP
/// response, and returns the port.
pub async fn https_server(leaf: &Leaf) -> u16 {
    https_server_redirecting(leaf, None).await
}

/// [`https_server`], but a request for `/away` is redirected to `location`.
pub async fn https_server_redirecting(leaf: &Leaf, location: Option<String>) -> u16 {
    let acceptor = leaf.acceptor();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let location = location.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match tls.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let host = String::from_utf8_lossy(&request)
                    .lines()
                    .find_map(|line| line.strip_prefix("host: ").or(line.strip_prefix("Host: ")))
                    .unwrap_or_default()
                    .to_owned();
                let away = request.starts_with(b"GET /away ");
                let response = match location {
                    Some(location) if away => format!(
                        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    ),
                    _ => {
                        let body = format!("hello {host}");
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    }
                };
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

/// Today's date, and the seconds since its midnight, in UTC.
fn today() -> (i32, u8, u8, i64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs();
    let now = i64::try_from(now).expect("seconds fit");
    let text = rfc3339(now);
    let year = text[0..4].parse().expect("year");
    let month = text[5..7].parse().expect("month");
    let day = text[8..10].parse().expect("day");
    (year, month, day, now.rem_euclid(86_400))
}
