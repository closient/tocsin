// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin heartbeat`: pings from the command line, for cron jobs and
//! scripts that would rather not spell out the request.

use tocsin_core::config::Id;

use crate::cli::HeartbeatCommand;
use crate::client::Client;
use crate::exit::Exit;
use crate::http::heartbeats::PingReceipt;
use crate::output::{Failure, Report};

pub fn run(command: HeartbeatCommand) -> Result<Report, Failure> {
    match command {
        HeartbeatCommand::Ping {
            server,
            id,
            status,
            message,
        } => {
            // Checked before any request, so an id like `..` cannot name
            // another route.
            let id = Id::parse(&id).map_err(|error| Failure::new(Exit::InvalidInput, error))?;
            let mut query = vec![("status", status.as_str())];
            if let Some(message) = &message {
                query.push(("message", message.as_str()));
            }
            let receipt: PingReceipt = Client::new(&server)?
                .post_query(&["api", "v1", "heartbeat", id.as_str()], &query)?;
            Ok(Report {
                exit: Exit::Ok,
                text: format!(
                    "heartbeat {} pinged {} at {}",
                    receipt.id,
                    receipt.status.as_str(),
                    receipt.received_at
                ),
                json: serde_json::to_value(&receipt).expect("API types serialize"),
            })
        }
    }
}
