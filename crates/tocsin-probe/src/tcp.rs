// SPDX-License-Identifier: AGPL-3.0-only

//! The `tcp` probe: connect, optionally send a line, optionally read a
//! banner.

use std::time::{Duration, Instant};

use tocsin_core::config::Check;
use tocsin_core::probe::{ProbeError, ProbeErrorKind, TcpFacts};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{connect_error, error_chain, milliseconds, split_target};

/// The most of a banner that is read.
const BANNER_LIMIT: usize = 4096;

/// How long the banner may pause, once it has started, before what has
/// arrived is taken as the whole of it.
const BANNER_PAUSE: Duration = Duration::from_millis(250);

/// Connects to the target. The banner is read only when `send` is set or
/// an assertion asks about it: up to the first newline, [`BANNER_LIMIT`]
/// bytes, the end of the stream or a [`BANNER_PAUSE`] after the first
/// bytes, whichever comes first.
pub(crate) async fn probe(check: &Check) -> Result<TcpFacts, ProbeError> {
    let (host, port) = split_target(&check.target, 0)?;
    if port == 0 {
        return Err(ProbeError::new(
            ProbeErrorKind::Setup,
            format!(
                "`{}` names no port; tcp targets are host:port",
                check.target
            ),
        ));
    }
    let send = check.tcp.as_ref().and_then(|tcp| tcp.send.as_deref());
    let wants_banner = send.is_some()
        || check
            .assertions
            .iter()
            .any(|assertion| assertion.banner.is_some());

    let start = Instant::now();
    let mut stream = TcpStream::connect((host, port))
        .await
        .map_err(|error| connect_error(&error, &format!("could not connect to {host}:{port}")))?;
    let latency_ms = milliseconds(start.elapsed());
    let address = stream
        .peer_addr()
        .map_or_else(|_| format!("{host}:{port}"), |address| address.to_string());

    let banner = if wants_banner {
        if let Some(line) = send {
            stream
                .write_all(line.as_bytes())
                .await
                .map_err(|error| protocol_error("could not send", &error))?;
        }
        Some(read_banner(&mut stream).await?)
    } else {
        None
    };
    // Close politely; the server's reaction is not part of the result.
    let _ = stream.shutdown().await;
    Ok(TcpFacts {
        latency_ms,
        address,
        banner,
    })
}

async fn read_banner(stream: &mut TcpStream) -> Result<String, ProbeError> {
    let mut banner = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = if banner.is_empty() {
            // The first bytes may take as long as the check allows.
            stream.read(&mut buffer).await
        } else {
            match tokio::time::timeout(BANNER_PAUSE, stream.read(&mut buffer)).await {
                Ok(read) => read,
                Err(_) => break,
            }
        }
        .map_err(|error| protocol_error("could not read the banner", &error))?;
        if read == 0 {
            break;
        }
        banner.extend_from_slice(&buffer[..read]);
        if let Some(newline) = banner.iter().position(|&byte| byte == b'\n') {
            banner.truncate(newline);
            break;
        }
        if banner.len() >= BANNER_LIMIT {
            banner.truncate(BANNER_LIMIT);
            break;
        }
    }
    if banner.last() == Some(&b'\r') {
        banner.pop();
    }
    Ok(String::from_utf8_lossy(&banner).into_owned())
}

fn protocol_error(what: &str, error: &std::io::Error) -> ProbeError {
    ProbeError::new(
        ProbeErrorKind::Protocol,
        format!("{what}: {}", error_chain(error)),
    )
}
