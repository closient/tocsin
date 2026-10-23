// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin release`: publish a release and stamp deploys from a pipeline.

use std::fmt::Write as _;
use std::path::Path;

use reqwest::Method;
use tocsin_core::config::Id;
use tocsin_core::incident::Timestamp;
use tocsin_core::release::{DeployStamp, NewRelease, Release, check_version};

use crate::cli::ReleaseCommand;
use crate::client::Client;
use crate::exit::Exit;
use crate::output::{Failure, Report};

const RELEASES: &str = "api/v1/releases";

pub fn run(command: ReleaseCommand) -> Result<Report, Failure> {
    match command {
        ReleaseCommand::Publish {
            server,
            project,
            version,
            title,
            notes_file,
            environment,
            release_url,
            released_at,
            deployed_at,
        } => {
            let new = NewRelease {
                project,
                version,
                environment,
                title,
                notes: read_notes(notes_file.as_deref())?,
                url: release_url,
                released_at,
                deployed_at,
            };
            let release: Release = Client::new(&server)?.write(&Method::POST, RELEASES, &new)?;
            Ok(written(&release, "published"))
        }
        ReleaseCommand::Deployed {
            server,
            project,
            version,
            environment,
            deployed_at,
        } => {
            // Both go into the path, so check them here: a `..` or `/`
            // would otherwise address another route.
            Id::parse(&project).map_err(|error| {
                Failure::new(
                    Exit::InvalidInput,
                    format!("--project {project:?}: {error}"),
                )
            })?;
            let mut errors = Vec::new();
            check_version("version", &version, &mut errors);
            if let Some(error) = errors.first() {
                return Err(Failure::new(
                    Exit::InvalidInput,
                    format!("--version {version:?} {}", error.message),
                ));
            }
            let stamp = DeployStamp {
                environment,
                deployed_at,
            };
            let release: Release = Client::new(&server)?.write(
                &Method::POST,
                &format!("{RELEASES}/{project}/{version}/deployed"),
                &stamp,
            )?;
            Ok(written(&release, "deployed"))
        }
    }
}

/// The notes from `path`, `-` for stdin; none without a path.
fn read_notes(path: Option<&Path>) -> Result<String, Failure> {
    let Some(path) = path else {
        return Ok(String::new());
    };
    let read = if path == Path::new("-") {
        std::io::read_to_string(std::io::stdin())
    } else {
        std::fs::read_to_string(path)
    };
    read.map_err(|error| {
        Failure::new(
            Exit::InvalidInput,
            format!("cannot read --notes-file {}: {error}", path.display()),
        )
    })
}

/// `now` or an RFC 3339 instant, for `--deployed-at` and `--released-at`.
pub fn parse_when(text: &str) -> Result<Timestamp, String> {
    if text == "now" {
        return Ok(crate::http::now());
    }
    text.parse::<Timestamp>()
        .map_err(|error| format!("{error}; give `now` or RFC 3339 such as 2026-10-23T14:05:00Z"))
}

fn written(release: &Release, verb: &str) -> Report {
    let mut text = format!(
        "{} {} {verb} to {}",
        release.project, release.version, release.environment
    );
    if let Some(at) = release.deployed_at {
        let _ = write!(text, ", deployed {at}");
    }
    Report {
        exit: Exit::Ok,
        json: serde_json::to_value(release).expect("API types serialize"),
        text,
    }
}
