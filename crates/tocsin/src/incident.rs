// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin incident` and `tocsin maintenance`: the incident API from the
//! command line.

use std::fmt::Write as _;
use std::path::Path;

use reqwest::Method;
use serde_json::{Value, json};
use tocsin_core::incident::{
    Incident, IncidentEdit, IncidentKind, IncidentList, NewIncident, NewUpdate,
};

use crate::cli::{ExportFormat, IncidentCommand, MaintenanceCommand, ServerArgs};
use crate::client::Client;
use crate::exit::Exit;
use crate::export;
use crate::output::{Failure, Report};

const INCIDENTS: &str = "api/v1/incidents";

pub fn run(command: IncidentCommand) -> Result<Report, Failure> {
    match command {
        IncidentCommand::New {
            server,
            title,
            impact,
            components,
            message,
            status,
            quiet,
            draft,
            internal_note,
            external_key,
        } => open_incident(
            &server,
            &NewIncident {
                kind: IncidentKind::Incident,
                title,
                impact: impact.unwrap_or(tocsin_core::incident::Impact::None),
                components,
                status,
                message,
                scheduled_start: None,
                scheduled_end: None,
                notify: !quiet,
                draft: draft.then_some(true),
                internal_note,
                external_key,
            },
        ),
        IncidentCommand::Publish { server, id, quiet } => publish(&server, id, quiet),
        IncidentCommand::Discard { server, id } => discard(&server, id),
        IncidentCommand::Update {
            server,
            id,
            status,
            message,
            quiet,
        } => {
            let update = NewUpdate {
                status,
                message,
                notify: !quiet,
            };
            let incident: Incident = Client::new(&server)?.write(
                &Method::POST,
                &format!("{INCIDENTS}/{id}/updates"),
                &update,
            )?;
            Ok(written(&incident, "updated"))
        }
        IncidentCommand::Edit {
            server,
            id,
            title,
            impact,
            components,
            no_components,
            start,
            end,
            internal_note,
        } => {
            let edit = IncidentEdit {
                title,
                impact,
                components: (no_components || !components.is_empty()).then_some(components),
                scheduled_start: start,
                scheduled_end: end,
                internal_note,
            };
            edit_incident(&server, id, &edit)
        }
        IncidentCommand::Show { server, id } => show(&server, id),
        IncidentCommand::List {
            server,
            limit,
            before,
            external_key,
        } => list(&server, limit, before, external_key.as_deref()),
        IncidentCommand::Export {
            server,
            id,
            all,
            format: ExportFormat::Md,
            out,
        } => {
            let client = Client::new(&server)?;
            match (id, out) {
                (_, Some(out)) if all => export_all(&client, &out),
                (Some(id), out) => export_one(&client, id, out.as_deref()),
                _ => unreachable!("clap requires an id or --all with --out"),
            }
        }
    }
}

/// `incident list`: a page of incidents, or the one with `external_key`.
fn list(
    server: &ServerArgs,
    limit: u32,
    before: Option<i64>,
    external_key: Option<&str>,
) -> Result<Report, Failure> {
    let mut path = format!("{INCIDENTS}?limit={limit}");
    if let Some(before) = before {
        let _ = write!(path, "&before={before}");
    }
    if let Some(key) = external_key {
        let key: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
        let _ = write!(path, "&external_key={key}");
    }
    let list: IncidentList = Client::new(server)?.read_as(&path)?;
    Ok(Report {
        exit: Exit::Ok,
        json: to_json(&list),
        text: table(&list),
    })
}

/// `incident publish`: shows draft `id` publicly, telling subscribers
/// unless `quiet`.
fn publish(server: &ServerArgs, id: i64, quiet: bool) -> Result<Report, Failure> {
    let incident: Incident = Client::new(server)?.write(
        &Method::POST,
        &format!("{INCIDENTS}/{id}/publish"),
        &json!({ "notify": !quiet }),
    )?;
    Ok(written(&incident, "published"))
}

/// `incident discard`: deletes draft `id`.
fn discard(server: &ServerArgs, id: i64) -> Result<Report, Failure> {
    let incident: Incident =
        Client::new(server)?.write(&Method::DELETE, &format!("{INCIDENTS}/{id}"), &json!({}))?;
    Ok(written(&incident, "discarded"))
}

/// `incident show`: one incident with every update.
fn show(server: &ServerArgs, id: i64) -> Result<Report, Failure> {
    let incident: Incident = Client::new(server)?.read_as(&format!("{INCIDENTS}/{id}"))?;
    Ok(Report {
        exit: Exit::Ok,
        json: to_json(&incident),
        text: describe(&incident),
    })
}

fn open_incident(server: &ServerArgs, new: &NewIncident) -> Result<Report, Failure> {
    let incident: Incident = Client::new(server)?.write(&Method::POST, INCIDENTS, new)?;
    Ok(written(&incident, "opened"))
}

fn edit_incident(server: &ServerArgs, id: i64, edit: &IncidentEdit) -> Result<Report, Failure> {
    if edit.is_empty() {
        return Err(Failure::new(
            Exit::InvalidInput,
            "nothing to change: pass --title, --impact, --component, --no-components, --start, --end \
             or --internal-note",
        ));
    }
    let incident: Incident =
        Client::new(server)?.write(&Method::PATCH, &format!("{INCIDENTS}/{id}"), edit)?;
    Ok(written(&incident, "edited"))
}

/// One published incident, printed or written to `out`. Read without the
/// token, so a draft is a 404: the export is for publishing.
fn export_one(client: &Client, id: i64, out: Option<&Path>) -> Result<Report, Failure> {
    let incident: Incident = client.get(&format!("{INCIDENTS}/{id}"))?;
    let markdown = export::markdown(&incident);
    match out {
        Some(out) => {
            let path = write_file(out, &incident, &markdown)?;
            Ok(Report {
                exit: Exit::Ok,
                json: json!({ "format": "md", "files": [{ "id": id, "path": path }] }),
                text: format!("wrote {path}"),
            })
        }
        None => Ok(Report {
            exit: Exit::Ok,
            json: json!({ "id": id, "format": "md", "markdown": markdown }),
            text: markdown.trim_end().to_owned(),
        }),
    }
}

pub fn run_maintenance(command: MaintenanceCommand) -> Result<Report, Failure> {
    match command {
        MaintenanceCommand::Schedule {
            server,
            title,
            start,
            end,
            components,
            message,
            impact,
        } => {
            let new = NewIncident {
                kind: IncidentKind::Maintenance,
                title,
                impact: impact.unwrap_or(tocsin_core::incident::Impact::None),
                components,
                status: None,
                message,
                scheduled_start: Some(start),
                scheduled_end: Some(end),
                notify: true,
                draft: None,
                internal_note: None,
                external_key: None,
            };
            let incident: Incident = Client::new(&server)?.write(&Method::POST, INCIDENTS, &new)?;
            Ok(written(&incident, "scheduled"))
        }
    }
}

/// Every published incident, paging back from the newest, one file each.
/// Read without the token, so drafts are never listed.
fn export_all(client: &Client, out: &Path) -> Result<Report, Failure> {
    let mut files = Vec::new();
    let mut before = None;
    loop {
        let mut path = format!("{INCIDENTS}?limit=100");
        if let Some(before) = before {
            let _ = write!(path, "&before={before}");
        }
        let page: IncidentList = client.get(&path)?;
        for incident in &page.incidents {
            let path = write_file(out, incident, &export::markdown(incident))?;
            files.push(json!({ "id": incident.id, "path": path }));
        }
        match page.next_before {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    Ok(Report {
        exit: Exit::Ok,
        text: format!(
            "wrote {} incident file(s) to {}",
            files.len(),
            out.display()
        ),
        json: json!({ "format": "md", "files": files }),
    })
}

fn write_file(out: &Path, incident: &Incident, markdown: &str) -> Result<String, Failure> {
    std::fs::create_dir_all(out).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot create {}: {error}", out.display()),
        )
    })?;
    let path = out.join(export::file_name(incident));
    std::fs::write(&path, markdown).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot write {}: {error}", path.display()),
        )
    })?;
    Ok(path.display().to_string())
}

fn written(incident: &Incident, verb: &str) -> Report {
    let draft = if incident.is_draft() { " draft" } else { "" };
    Report {
        exit: Exit::Ok,
        json: to_json(incident),
        text: format!(
            "{}{draft} {} {verb}: {} ({})",
            incident.kind, incident.id, incident.title, incident.status
        ),
    }
}

fn to_json(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("API types serialize")
}

/// One incident for a person: header, then the timeline.
fn describe(incident: &Incident) -> String {
    let mut text = format!(
        "{} {}: {}\nstatus {}, impact {}",
        incident.kind, incident.id, incident.title, incident.status, incident.impact
    );
    if incident.is_draft() {
        let _ = write!(text, "\ndraft by {}: not public", incident.created_by);
    }
    if !incident.components.is_empty() {
        let _ = write!(text, ", components {}", incident.components.join(", "));
    }
    if let (Some(start), Some(end)) = (incident.scheduled_start, incident.scheduled_end) {
        let _ = write!(text, "\nscheduled {start} to {end}");
    }
    if let Some(key) = &incident.external_key {
        let _ = write!(text, "\nexternal key {key}");
    }
    if let Some(note) = &incident.internal_note {
        let _ = write!(text, "\n\ninternal note (not public)\n{}", note.trim_end());
    }
    for update in &incident.updates {
        let _ = write!(
            text,
            "\n\n{} {} by {}\n{}",
            update.created_at,
            update.status,
            update.created_by,
            update.message.trim_end()
        );
    }
    text
}

/// One line per incident.
fn table(list: &IncidentList) -> String {
    if list.incidents.is_empty() {
        return "no incidents".to_owned();
    }
    let mut text = String::new();
    for incident in &list.incidents {
        let _ = writeln!(
            text,
            "{:>5}  {:<11}  {:<13}  {:<8}  {}{}",
            incident.id,
            incident.kind,
            incident.status,
            incident.impact,
            incident.title,
            if incident.is_draft() { " (draft)" } else { "" }
        );
    }
    if let Some(next) = list.next_before {
        let _ = writeln!(text, "more: --before {next}");
    }
    text.trim_end().to_owned()
}
