// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin mcp`: the tools over stdin and stdout, calling a node's HTTP
//! API. Stdout carries the protocol and nothing else; logs go to stderr.

use rmcp::ServiceExt;
use rmcp::transport::stdio;

use super::Server;
use crate::cli::ServerArgs;
use crate::client::Api;
use crate::exit::Exit;
use crate::output::Failure;

/// Serves until the client closes stdin.
pub fn run(server: &ServerArgs) -> Result<(), Failure> {
    let api = Api::new(server)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::new(Exit::Runtime, format!("cannot start runtime: {error}")))?;
    runtime.block_on(async {
        let running = Server::remote(api).serve(stdio()).await.map_err(|error| {
            Failure::new(Exit::Runtime, format!("MCP handshake failed: {error}"))
        })?;
        running
            .waiting()
            .await
            .map_err(|error| Failure::new(Exit::Runtime, format!("MCP server failed: {error}")))?;
        Ok(())
    })
}
