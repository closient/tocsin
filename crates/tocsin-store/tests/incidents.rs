// SPDX-License-Identifier: AGPL-3.0-only

//! Incidents through the store's public API: creation, updates, edits,
//! listing, and the rules the schema enforces on its own.

use std::path::PathBuf;
use std::time::Duration;

use rusqlite::Connection;
use tocsin_core::incident::{
    Actor, ActorKind, Impact, IncidentEdit, IncidentKind, IncidentStatus, NewIncident, NewUpdate,
    Timestamp,
};
use tocsin_store::{Drafts, Error, Store};

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Store,
}

fn open() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(tocsin_store::DB_FILE);
    let store = Store::open(&path).unwrap();
    Fixture {
        _dir: dir,
        path,
        store,
    }
}

fn raw(fixture: &Fixture) -> Connection {
    let conn = Connection::open(&fixture.path).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn
}

fn at(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms)
}

fn outage() -> NewIncident {
    NewIncident {
        kind: IncidentKind::Incident,
        title: "API errors".into(),
        impact: Impact::Major,
        components: vec!["web".into(), "api".into()],
        status: None,
        message: "Looking into elevated errors.".into(),
        scheduled_start: None,
        scheduled_end: None,
        notify: true,
        draft: None,
    }
}

fn maintenance() -> NewIncident {
    NewIncident {
        kind: IncidentKind::Maintenance,
        title: "Database upgrade".into(),
        impact: Impact::None,
        components: vec!["api".into()],
        status: None,
        message: "Upgrading Postgres.".into(),
        scheduled_start: Some(at(10_000)),
        scheduled_end: Some(at(20_000)),
        notify: true,
        draft: None,
    }
}

#[tokio::test]
async fn an_incident_is_created_with_its_first_update_and_author() {
    let fixture = open();
    let ci = Actor::token("ci");
    let incident = fixture
        .store
        .create_incident(outage(), ci.clone(), at(1000))
        .await
        .unwrap();
    assert_eq!(incident.status, IncidentStatus::Investigating);
    assert_eq!(incident.components, ["api", "web"]);
    assert_eq!(incident.created_by, ci);
    assert_eq!(incident.updated_by, ci);
    assert_eq!(incident.created_at, at(1000));
    assert_eq!(incident.resolved_at, None);
    assert_eq!(incident.updates.len(), 1);
    assert_eq!(incident.updates[0].message, "Looking into elevated errors.");
    assert_eq!(incident.updates[0].created_by, ci);
    assert_eq!(
        fixture.store.incident(incident.id).await.unwrap(),
        Some(incident)
    );
}

#[tokio::test]
async fn updates_append_move_the_status_and_record_each_author() {
    let fixture = open();
    let opened = fixture
        .store
        .create_incident(outage(), Actor::token("ci"), at(1000))
        .await
        .unwrap();
    let oncall = Actor::token("oncall");
    let resolve = NewUpdate {
        status: IncidentStatus::Resolved,
        message: "Fixed.".into(),
        notify: true,
    };
    let resolved = fixture
        .store
        .add_incident_update(opened.id, resolve.clone(), false, oncall.clone(), at(5000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.status, IncidentStatus::Resolved);
    assert_eq!(resolved.resolved_at, Some(at(5000)));
    assert_eq!(resolved.updated_by, oncall);
    assert_eq!(resolved.created_by, Actor::token("ci"));
    assert_eq!(resolved.updates.len(), 2);
    assert_eq!(resolved.updates[1].created_by, oncall);

    // A second resolving update keeps the first resolution time.
    let again = fixture
        .store
        .add_incident_update(opened.id, resolve, false, oncall.clone(), at(6000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.resolved_at, Some(at(5000)));

    // Reopening clears it.
    let reopened = fixture
        .store
        .add_incident_update(
            opened.id,
            NewUpdate {
                status: IncidentStatus::Monitoring,
                message: "Errors are back.".into(),
                notify: true,
            },
            false,
            oncall,
            at(7000),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reopened.resolved_at, None);
    assert_eq!(reopened.updates.len(), 4);
}

#[tokio::test]
async fn an_update_status_must_suit_the_kind() {
    let fixture = open();
    let opened = fixture
        .store
        .create_incident(maintenance(), Actor::token("ci"), at(1000))
        .await
        .unwrap();
    assert_eq!(opened.status, IncidentStatus::Scheduled);
    let error = fixture
        .store
        .add_incident_update(
            opened.id,
            NewUpdate {
                status: IncidentStatus::Resolved,
                message: "Done.".into(),
                notify: true,
            },
            false,
            Actor::token("ci"),
            at(2000),
        )
        .await
        .unwrap_err();
    let Error::Invalid(errors) = error else {
        panic!("expected Invalid, got {error}");
    };
    assert_eq!(errors[0].path, "status");
}

#[tokio::test]
async fn edits_change_the_row_and_are_logged_with_their_author() {
    let fixture = open();
    let opened = fixture
        .store
        .create_incident(maintenance(), Actor::token("ci"), at(1000))
        .await
        .unwrap();
    let edit = IncidentEdit {
        title: Some("Database upgrade, part 1".into()),
        components: Some(vec!["web".into()]),
        scheduled_end: Some(at(30_000)),
        ..IncidentEdit::default()
    };
    let edited = fixture
        .store
        .edit_incident(opened.id, edit, false, Actor::token("planner"), at(2000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(edited.title, "Database upgrade, part 1");
    assert_eq!(edited.components, ["web"]);
    assert_eq!(edited.scheduled_start, Some(at(10_000)));
    assert_eq!(edited.scheduled_end, Some(at(30_000)));
    assert_eq!(edited.updated_by, Actor::token("planner"));
    assert_eq!(edited.updated_at, at(2000));
    assert_eq!(edited.updates, opened.updates, "an edit adds no update");

    let (changes, author): (String, String) = raw(&fixture)
        .query_row(
            "SELECT changes, created_by_kind || ':' || created_by FROM incident_edit",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(author, "token:planner");
    let changes: serde_json::Value = serde_json::from_str(&changes).unwrap();
    assert_eq!(changes["title"], "Database upgrade, part 1");

    let backwards = IncidentEdit {
        scheduled_start: Some(at(40_000)),
        ..IncidentEdit::default()
    };
    let error = fixture
        .store
        .edit_incident(
            opened.id,
            backwards,
            false,
            Actor::token("planner"),
            at(3000),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Invalid(_)), "{error}");
}

#[tokio::test]
async fn unknown_ids_are_none() {
    let fixture = open();
    let actor = Actor::token("ci");
    assert_eq!(fixture.store.incident(42).await.unwrap(), None);
    let update = NewUpdate {
        status: IncidentStatus::Resolved,
        message: "x".into(),
        notify: true,
    };
    assert_eq!(
        fixture
            .store
            .add_incident_update(42, update, false, actor.clone(), at(1))
            .await
            .unwrap(),
        None
    );
    let edit = IncidentEdit {
        title: Some("x".into()),
        ..IncidentEdit::default()
    };
    assert_eq!(
        fixture
            .store
            .edit_incident(42, edit, false, actor, at(1))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn open_incidents_leave_out_resolved_and_completed_ones() {
    let fixture = open();
    let store = &fixture.store;
    let ci = Actor::token("ci");
    let mut ids = Vec::new();
    for new in [outage(), maintenance(), outage(), maintenance()] {
        ids.push(
            store
                .create_incident(new, ci.clone(), at(1000))
                .await
                .unwrap()
                .id,
        );
    }
    for (id, status) in [
        (ids[2], IncidentStatus::Resolved),
        (ids[3], IncidentStatus::Completed),
    ] {
        let update = NewUpdate {
            status,
            message: "Over.".into(),
            notify: true,
        };
        store
            .add_incident_update(id, update, false, ci.clone(), at(2000))
            .await
            .unwrap()
            .unwrap();
    }

    let open: Vec<_> = store
        .open_incidents()
        .await
        .unwrap()
        .into_iter()
        .map(|incident| (incident.id, incident.components))
        .collect();
    assert_eq!(
        open,
        [
            (ids[0], vec!["api".to_owned(), "web".to_owned()]),
            (ids[1], vec!["api".to_owned()]),
        ]
    );
}

#[tokio::test]
async fn listing_is_newest_first_and_pages_by_id() {
    let fixture = open();
    for minute in 0..5 {
        fixture
            .store
            .create_incident(outage(), Actor::token("ci"), at(minute * 60_000))
            .await
            .unwrap();
    }
    let first = fixture
        .store
        .incidents(Drafts::Include, None, 2)
        .await
        .unwrap();
    let ids: Vec<i64> = first.iter().map(|incident| incident.id).collect();
    assert_eq!(ids, [5, 4]);
    let next = fixture
        .store
        .incidents(Drafts::Include, Some(4), 10)
        .await
        .unwrap();
    let ids: Vec<i64> = next.iter().map(|incident| incident.id).collect();
    assert_eq!(ids, [3, 2, 1]);
    assert_eq!(next[0].updates.len(), 1);
}

/// The schema holds the rules on its own, against a writer that bypasses
/// the store.
#[tokio::test]
async fn the_schema_refuses_rows_that_break_the_rules() {
    let fixture = open();
    let opened = fixture
        .store
        .create_incident(outage(), Actor::token("ci"), at(1000))
        .await
        .unwrap();
    let conn = raw(&fixture);
    let refused = |sql: &str| {
        let error = conn.execute_batch(sql).unwrap_err();
        error.to_string()
    };
    let id = opened.id;
    refused(&format!(
        "UPDATE incident SET status = 'completed' WHERE id = {id}"
    ));
    refused(&format!(
        "UPDATE incident SET scheduled_start = 1, scheduled_end = 2 WHERE id = {id}"
    ));
    refused(&format!(
        "UPDATE incident SET status = 'resolved' WHERE id = {id}"
    ));
    refused(&format!(
        "UPDATE incident SET impact = 'degraded' WHERE id = {id}"
    ));
    assert!(
        refused(&format!(
            "UPDATE incident SET kind = 'maintenance' WHERE id = {id}"
        ))
        .contains("fixed")
    );
    assert!(refused("UPDATE incident_update SET message = 'rewritten'").contains("append-only"));
    refused(&format!(
        "INSERT INTO incident_update (incident_id, status, message, created_at, \
         created_by_kind, created_by) VALUES ({id}, 'resolved', '  ', 1, 'token', 'ci')"
    ));
    refused(&format!(
        "INSERT INTO incident_update (incident_id, status, message, created_at, \
         created_by_kind, created_by) VALUES ({id}, 'resolved', 'x', 1, 'robot', 'ci')"
    ));
}

#[tokio::test]
async fn the_actor_kind_round_trips() {
    let fixture = open();
    let person = Actor {
        kind: ActorKind::Person,
        id: "ada@example.com".into(),
    };
    let incident = fixture
        .store
        .create_incident(outage(), person.clone(), at(1000))
        .await
        .unwrap();
    assert_eq!(incident.created_by, person);
}

#[tokio::test]
async fn info_counts_the_incident_tables() {
    let fixture = open();
    fixture
        .store
        .create_incident(outage(), Actor::token("ci"), at(1000))
        .await
        .unwrap();
    let info = fixture.store.info().await.unwrap();
    let count = |table: &str| {
        info.rows
            .iter()
            .find(|(name, _)| *name == table)
            .map(|(_, count)| *count)
    };
    assert_eq!(count("incident"), Some(1));
    assert_eq!(count("incident_component"), Some(2));
    assert_eq!(count("incident_update"), Some(1));
    assert_eq!(count("incident_edit"), Some(0));
}

fn draft() -> NewIncident {
    NewIncident {
        draft: Some(true),
        ..outage()
    }
}

#[tokio::test]
async fn an_incident_is_published_when_created_unless_a_draft() {
    let fixture = open();
    let ci = Actor::token("ci");
    let published = fixture
        .store
        .create_incident(outage(), ci.clone(), at(1000))
        .await
        .unwrap();
    assert_eq!(published.published_at, Some(at(1000)));
    assert_eq!(published.published_by, Some(ci.clone()));
    let drafted = fixture
        .store
        .create_incident(draft(), Actor::token("seat"), at(2000))
        .await
        .unwrap();
    assert!(drafted.is_draft());
    assert_eq!(drafted.published_by, None);
}

#[tokio::test]
async fn a_draft_is_left_out_of_every_read_that_excludes_drafts() {
    let fixture = open();
    let store = &fixture.store;
    let drafted = store
        .create_incident(draft(), Actor::token("seat"), at(1000))
        .await
        .unwrap();
    let ids = |incidents: Vec<tocsin_core::incident::Incident>| {
        incidents
            .iter()
            .map(|incident| incident.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(store.incidents(Drafts::Exclude, None, 10).await.unwrap()),
        [] as [i64; 0]
    );
    assert_eq!(
        ids(store
            .incidents_naming(Drafts::Exclude, "api", 10)
            .await
            .unwrap()),
        [] as [i64; 0]
    );
    assert_eq!(
        ids(store.unresolved_incidents(Drafts::Exclude).await.unwrap()),
        [] as [i64; 0]
    );
    assert_eq!(store.open_incidents().await.unwrap(), []);
    assert_eq!(
        ids(store.incidents(Drafts::Include, None, 10).await.unwrap()),
        [drafted.id]
    );
    assert_eq!(
        ids(store
            .incidents_naming(Drafts::Include, "api", 10)
            .await
            .unwrap()),
        [drafted.id]
    );
    assert_eq!(
        ids(store.unresolved_incidents(Drafts::Include).await.unwrap()),
        [drafted.id]
    );
    assert_eq!(ids(store.draft_incidents().await.unwrap()), [drafted.id]);
}

#[tokio::test]
async fn publishing_stamps_who_and_when_once() {
    let fixture = open();
    let store = &fixture.store;
    let drafted = store
        .create_incident(draft(), Actor::token("seat"), at(1000))
        .await
        .unwrap();
    let person = Actor::person("ops@example.com");
    let published = store
        .publish_incident(drafted.id, person.clone(), at(5000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(published.published_at, Some(at(5000)));
    assert_eq!(published.published_by, Some(person.clone()));
    assert_eq!(published.created_by, Actor::token("seat"));
    assert_eq!(store.draft_incidents().await.unwrap(), []);
    assert_eq!(store.open_incidents().await.unwrap().len(), 1);
    let again = store.publish_incident(drafted.id, person, at(6000)).await;
    assert!(
        matches!(again, Err(Error::Published(id)) if id == drafted.id),
        "{again:?}"
    );
    assert_eq!(
        store
            .publish_incident(999, Actor::token("ci"), at(1))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn a_drafts_only_writer_changes_drafts_but_not_published_incidents() {
    let fixture = open();
    let store = &fixture.store;
    let seat = Actor::token("seat");
    let drafted = store
        .create_incident(draft(), seat.clone(), at(1000))
        .await
        .unwrap();
    let update = NewUpdate {
        status: IncidentStatus::Identified,
        message: "Found it.".into(),
        notify: true,
    };
    let edit = IncidentEdit {
        title: Some("API errors on login".into()),
        ..IncidentEdit::default()
    };
    store
        .add_incident_update(drafted.id, update.clone(), true, seat.clone(), at(2000))
        .await
        .unwrap()
        .unwrap();
    store
        .edit_incident(drafted.id, edit.clone(), true, seat.clone(), at(2000))
        .await
        .unwrap()
        .unwrap();
    let published = store
        .create_incident(outage(), Actor::token("ci"), at(1000))
        .await
        .unwrap();
    let refused = store
        .add_incident_update(published.id, update, true, seat.clone(), at(3000))
        .await;
    assert!(matches!(refused, Err(Error::Published(_))), "{refused:?}");
    let refused = store
        .edit_incident(published.id, edit, true, seat, at(3000))
        .await;
    assert!(matches!(refused, Err(Error::Published(_))), "{refused:?}");
    assert_eq!(
        store.incident(published.id).await.unwrap().unwrap(),
        published
    );
}

#[tokio::test]
async fn a_draft_is_discarded_with_its_updates_and_a_published_incident_never_is() {
    let fixture = open();
    let store = &fixture.store;
    let drafted = store
        .create_incident(draft(), Actor::token("seat"), at(1000))
        .await
        .unwrap();
    let discarded = store.discard_incident(drafted.id).await.unwrap();
    assert_eq!(discarded, Some(drafted.clone()));
    assert_eq!(store.incident(drafted.id).await.unwrap(), None);
    let updates: i64 = raw(&fixture)
        .query_row(
            "SELECT count(*) FROM incident_update WHERE incident_id = ?1",
            [drafted.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(updates, 0);
    assert_eq!(store.discard_incident(drafted.id).await.unwrap(), None);
    let published = store
        .create_incident(outage(), Actor::token("ci"), at(1000))
        .await
        .unwrap();
    let refused = store.discard_incident(published.id).await;
    assert!(matches!(refused, Err(Error::Published(_))), "{refused:?}");
    assert!(store.incident(published.id).await.unwrap().is_some());
}
