// SPDX-License-Identifier: AGPL-3.0-only

//! The release log through the store's public API: the idempotent
//! publish, deploy stamps in either order, the visibility filter, the
//! timeline's order and filters, and the rules the schema enforces on its
//! own.

use std::path::PathBuf;
use std::time::Duration;

use rusqlite::Connection;
use tocsin_core::Health;
use tocsin_core::incident::{Actor, Impact, IncidentKind, NewIncident, Timestamp};
use tocsin_core::release::{DeployStamp, NewRelease, TimelineEvent};
use tocsin_store::{
    ComponentScope, Error, NewOutage, Projects, ReleaseQuery, Store, TimelineQuery, Written,
};

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

fn ci() -> Actor {
    Actor::token("ci")
}

fn release(project: &str, version: &str) -> NewRelease {
    NewRelease {
        project: project.into(),
        version: version.into(),
        environment: None,
        title: Some(format!("{project} {version}")),
        notes: "First notes.".into(),
        url: Some("https://example.com/v1".into()),
        released_at: None,
        deployed_at: None,
    }
}

fn all(project: Option<&str>) -> ReleaseQuery {
    ReleaseQuery {
        visible: Projects::All,
        project: project.map(Into::into),
        version: None,
        since: None,
        limit: 100,
    }
}

fn count(fixture: &Fixture) -> i64 {
    raw(fixture)
        .query_row("SELECT count(*) FROM release", [], |row| row.get(0))
        .unwrap()
}

#[tokio::test]
async fn republishing_the_same_key_updates_in_place() {
    let fixture = open();
    let (first, written) = fixture
        .store
        .publish_release(release("api", "v1"), ci(), at(1_000))
        .await
        .unwrap();
    assert_eq!(written, Written::Created);
    assert_eq!(first.released_at, Some(at(1_000)));
    assert_eq!(first.environment, "production");

    let mut again = release("api", "v1");
    again.title = Some("Fixed title".into());
    again.notes = "Better notes.".into();
    again.url = None;
    let (second, written) = fixture
        .store
        .publish_release(again, Actor::person("alice@example.com"), at(5_000))
        .await
        .unwrap();
    assert_eq!(written, Written::Updated);
    assert_eq!(second.id, first.id);
    assert_eq!(second.title, "Fixed title");
    assert_eq!(second.notes, "Better notes.");
    assert_eq!(second.url, None);
    // Omitted on the re-post: kept, not bumped to the re-post's time.
    assert_eq!(second.released_at, Some(at(1_000)));
    assert_eq!(second.created_by, ci());
    assert_eq!(second.created_at, at(1_000));
    assert_eq!(second.updated_by, Actor::person("alice@example.com"));
    assert_eq!(second.updated_at, at(5_000));
    assert_eq!(count(&fixture), 1);
}

#[tokio::test]
async fn each_environment_is_its_own_release() {
    let fixture = open();
    let mut staging = release("api", "v1");
    staging.environment = Some("staging".into());
    let (staged, _) = fixture
        .store
        .publish_release(staging, ci(), at(1_000))
        .await
        .unwrap();
    let (produced, written) = fixture
        .store
        .publish_release(release("api", "v1"), ci(), at(2_000))
        .await
        .unwrap();
    assert_eq!(written, Written::Created);
    assert_ne!(staged.id, produced.id);
    assert_eq!(count(&fixture), 2);
}

#[tokio::test]
async fn a_deploy_stamped_first_is_a_stub_the_publish_fills() {
    let fixture = open();
    let (stub, written) = fixture
        .store
        .stamp_deploy(
            "api".into(),
            "v2".into(),
            DeployStamp::default(),
            ci(),
            at(1_000),
        )
        .await
        .unwrap();
    assert_eq!(written, Written::Created);
    assert_eq!(stub.title, "");
    assert_eq!(stub.notes, "");
    assert_eq!(stub.released_at, None);
    assert_eq!(stub.deployed_at, Some(at(1_000)));
    // Not a release yet: the log leaves it out.
    let (listed, _) = fixture.store.releases(all(None)).await.unwrap();
    assert!(listed.is_empty(), "{listed:?}");

    let (published, written) = fixture
        .store
        .publish_release(release("api", "v2"), ci(), at(3_000))
        .await
        .unwrap();
    assert_eq!(written, Written::Updated);
    assert_eq!(published.id, stub.id);
    assert_eq!(published.title, "api v2");
    assert_eq!(published.released_at, Some(at(3_000)));
    assert_eq!(published.deployed_at, Some(at(1_000)));
    assert_eq!(count(&fixture), 1);
}

#[tokio::test]
async fn a_deploy_stamped_after_the_publish_lands_on_it() {
    let fixture = open();
    let (published, _) = fixture
        .store
        .publish_release(release("api", "v3"), ci(), at(1_000))
        .await
        .unwrap();
    assert_eq!(published.deployed_at, None);
    let (deployed, written) = fixture
        .store
        .stamp_deploy(
            "api".into(),
            "v3".into(),
            DeployStamp {
                environment: None,
                deployed_at: Some(at(2_500)),
            },
            ci(),
            at(9_000),
        )
        .await
        .unwrap();
    assert_eq!(written, Written::Updated);
    assert_eq!(deployed.id, published.id);
    assert_eq!(deployed.deployed_at, Some(at(2_500)));
    assert_eq!(deployed.released_at, Some(at(1_000)));
    assert_eq!(deployed.title, "api v3");

    // A re-post after the stamp keeps the deploy and the release time.
    let mut again = release("api", "v3");
    again.notes = "Amended.".into();
    let (reposted, _) = fixture
        .store
        .publish_release(again, ci(), at(10_000))
        .await
        .unwrap();
    assert_eq!(reposted.deployed_at, Some(at(2_500)));
    assert_eq!(reposted.released_at, Some(at(1_000)));
    assert_eq!(reposted.notes, "Amended.");

    // A later stamp records the latest deploy.
    let (redeployed, _) = fixture
        .store
        .stamp_deploy(
            "api".into(),
            "v3".into(),
            DeployStamp::default(),
            ci(),
            at(11_000),
        )
        .await
        .unwrap();
    assert_eq!(redeployed.deployed_at, Some(at(11_000)));
    assert_eq!(count(&fixture), 1);
}

#[tokio::test]
async fn released_at_moves_later_on_request_and_never_earlier() {
    let fixture = open();
    let mut first = release("api", "v4");
    first.released_at = Some(at(5_000));
    fixture
        .store
        .publish_release(first, ci(), at(6_000))
        .await
        .unwrap();

    let mut earlier = release("api", "v4");
    earlier.released_at = Some(at(4_999));
    let Err(Error::Invalid(fields)) = fixture
        .store
        .publish_release(earlier, ci(), at(7_000))
        .await
    else {
        panic!("an earlier released_at is refused");
    };
    assert_eq!(fields[0].path, "released_at");

    let mut later = release("api", "v4");
    later.released_at = Some(at(8_000));
    let (moved, _) = fixture
        .store
        .publish_release(later, ci(), at(9_000))
        .await
        .unwrap();
    assert_eq!(moved.released_at, Some(at(8_000)));

    // The schema holds the rule on its own, and keeps the key fixed.
    let conn = raw(&fixture);
    for statement in [
        "UPDATE release SET released_at = 1",
        "UPDATE release SET released_at = NULL",
        "UPDATE release SET version = 'v5'",
        "UPDATE release SET environment = 'staging'",
    ] {
        assert!(conn.execute(statement, []).is_err(), "{statement}");
    }
}

#[tokio::test]
async fn the_schema_refuses_versions_that_are_not_url_safe() {
    let fixture = open();
    let conn = raw(&fixture);
    let insert = |version: &str| {
        conn.execute(
            "INSERT INTO release (project_id, version, environment, title, notes, released_at, \
             created_at, created_by_kind, created_by, updated_at, updated_by_kind, updated_by) \
             VALUES ('api', ?1, 'production', 't', '', 1, 1, 'token', 'ci', 1, 'token', 'ci')",
            [version],
        )
    };
    for bad in ["", ".", "..", "../x", "a/b", "vé", "v 1", &"a".repeat(65)] {
        assert!(insert(bad).is_err(), "{bad:?}");
    }
    insert("v1.0.0+build.5").unwrap();
}

#[tokio::test]
async fn reads_see_only_the_projects_they_are_given() {
    let fixture = open();
    for project in ["public", "private"] {
        let mut new = release(project, "v1");
        new.deployed_at = Some(at(2_000));
        fixture
            .store
            .publish_release(new, ci(), at(1_000))
            .await
            .unwrap();
    }
    let only = || Projects::Only(vec!["public".into()]);
    let (listed, _) = fixture
        .store
        .releases(ReleaseQuery {
            visible: only(),
            ..all(None)
        })
        .await
        .unwrap();
    let projects: Vec<_> = listed.iter().map(|r| r.project.as_str()).collect();
    assert_eq!(projects, ["public"]);
    // Asking for the hidden project by name finds nothing.
    let (asked, _) = fixture
        .store
        .releases(ReleaseQuery {
            visible: only(),
            ..all(Some("private"))
        })
        .await
        .unwrap();
    assert_eq!(asked, []);

    let timeline = fixture
        .store
        .timeline(TimelineQuery {
            visible: only(),
            since: at(0),
            limit: 100,
            component: None,
        })
        .await
        .unwrap();
    assert_eq!(timeline.events.len(), 1);

    let (everything, _) = fixture.store.releases(all(None)).await.unwrap();
    assert_eq!(everything.len(), 2);
    let none = Projects::Only(Vec::new());
    let (nothing, _) = fixture
        .store
        .releases(ReleaseQuery {
            visible: none,
            ..all(None)
        })
        .await
        .unwrap();
    assert_eq!(nothing, []);
}

#[tokio::test]
async fn release_lists_are_newest_first_and_say_when_truncated() {
    let fixture = open();
    for (version, ms) in [("v1", 1_000), ("v2", 3_000), ("v3", 2_000)] {
        fixture
            .store
            .publish_release(release("api", version), ci(), at(ms))
            .await
            .unwrap();
    }
    let (listed, truncated) = fixture
        .store
        .releases(ReleaseQuery {
            limit: 2,
            ..all(Some("api"))
        })
        .await
        .unwrap();
    let versions: Vec<_> = listed.iter().map(|r| r.version.as_str()).collect();
    assert_eq!(versions, ["v2", "v3"]);
    assert!(truncated);
    let (since, truncated) = fixture
        .store
        .releases(ReleaseQuery {
            since: Some(at(2_000)),
            ..all(None)
        })
        .await
        .unwrap();
    assert_eq!(since.len(), 2);
    assert!(!truncated);
}

fn outage(check: &str, component: Option<&str>, started_at_ms: i64) -> NewOutage {
    NewOutage {
        node_id: "node-a".into(),
        check_id: check.into(),
        component_id: component.map(Into::into),
        health: Health::Down,
        started_at_ms,
        alerted: true,
    }
}

fn incident(component: &str) -> NewIncident {
    NewIncident {
        kind: IncidentKind::Incident,
        title: format!("{component} errors"),
        impact: Impact::Major,
        components: vec![component.into()],
        status: None,
        message: "Looking.".into(),
        scheduled_start: None,
        scheduled_end: None,
    }
}

/// `(kind, at)` for each event, to compare orders at a glance.
fn kinds(events: &[TimelineEvent]) -> Vec<(&'static str, i64)> {
    events
        .iter()
        .map(|event| {
            let kind = match event {
                TimelineEvent::Outage { .. } => "outage",
                TimelineEvent::Incident { .. } => "incident",
                TimelineEvent::Deploy { .. } => "deploy",
                TimelineEvent::Release { .. } => "release",
            };
            (kind, event.at().as_millis())
        })
        .collect()
}

/// A history with every kind of event, several at the same instant.
async fn history(fixture: &Fixture) {
    let store = &fixture.store;
    // Before the window.
    store
        .open_outage(outage("api-health", Some("api"), 500))
        .await
        .unwrap();
    // A deploy, an outage and an incident all at 2 000.
    let mut deployed = release("api", "v1");
    deployed.deployed_at = Some(at(2_000));
    store
        .publish_release(deployed, ci(), at(1_500))
        .await
        .unwrap();
    store
        .open_outage(outage("web-health", Some("web"), 2_000))
        .await
        .unwrap();
    store
        .create_incident(incident("api"), ci(), at(2_000))
        .await
        .unwrap();
    // Published, not deployed: a release event.
    store
        .publish_release(release("web", "w1"), ci(), at(3_000))
        .await
        .unwrap();
    // Recorded without a component; the check belongs to api now.
    store
        .open_outage(outage("api-latency", None, 4_000))
        .await
        .unwrap();
    // Deployed before it was published: a deploy at the stamp.
    store
        .stamp_deploy(
            "api".into(),
            "v2".into(),
            DeployStamp::default(),
            ci(),
            at(5_000),
        )
        .await
        .unwrap();
    store
        .create_incident(incident("web"), ci(), at(6_000))
        .await
        .unwrap();
}

fn window(since: i64, limit: u32) -> TimelineQuery {
    TimelineQuery {
        visible: Projects::All,
        since: at(since),
        limit,
        component: None,
    }
}

#[tokio::test]
async fn the_timeline_interleaves_every_kind_in_time_order() {
    let fixture = open();
    history(&fixture).await;
    let timeline = fixture.store.timeline(window(1_000, 100)).await.unwrap();
    assert_eq!(
        kinds(&timeline.events),
        [
            // The tie at 2 000 breaks by kind: outage, incident, deploy.
            ("outage", 2_000),
            ("incident", 2_000),
            ("deploy", 2_000),
            ("release", 3_000),
            ("outage", 4_000),
            ("deploy", 5_000),
            ("incident", 6_000),
        ]
    );
    assert!(!timeline.truncated);
    let TimelineEvent::Deploy { release, .. } = &timeline.events[5] else {
        panic!("a deploy");
    };
    assert_eq!(
        (release.version.as_str(), release.released_at),
        ("v2", None)
    );

    // The limit keeps the oldest and says it cut.
    let first = fixture.store.timeline(window(1_000, 3)).await.unwrap();
    assert_eq!(first.events, timeline.events[..3]);
    assert!(first.truncated);
}

#[tokio::test]
async fn the_timeline_breaks_ties_by_kind_then_id() {
    let fixture = open();
    let store = &fixture.store;
    // Written in the opposite of the expected order, all at one instant.
    for version in ["v2", "v1"] {
        let mut new = release("api", version);
        new.deployed_at = Some(at(1_000));
        store.publish_release(new, ci(), at(1_000)).await.unwrap();
    }
    store
        .create_incident(incident("api"), ci(), at(1_000))
        .await
        .unwrap();
    store.open_outage(outage("b", None, 1_000)).await.unwrap();
    store.open_outage(outage("a", None, 1_000)).await.unwrap();
    let timeline = store.timeline(window(0, 100)).await.unwrap();
    let order: Vec<_> = timeline
        .events
        .iter()
        .map(|event| (event.kind_rank(), event.id()))
        .collect();
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(order, sorted);
    assert_eq!(
        kinds(&timeline.events),
        [
            ("outage", 1_000),
            ("outage", 1_000),
            ("incident", 1_000),
            ("deploy", 1_000),
            ("deploy", 1_000),
        ]
    );
}

#[tokio::test]
async fn the_timeline_filters_to_one_component() {
    let fixture = open();
    history(&fixture).await;
    let timeline = fixture
        .store
        .timeline(TimelineQuery {
            component: Some(ComponentScope {
                id: "api".into(),
                checks: vec!["api-health".into(), "api-latency".into()],
                projects: vec!["api".into()],
            }),
            ..window(1_000, 100)
        })
        .await
        .unwrap();
    assert_eq!(
        kinds(&timeline.events),
        [
            ("incident", 2_000),
            ("deploy", 2_000),
            ("outage", 4_000),
            ("deploy", 5_000),
        ]
    );
    let TimelineEvent::Incident { components, .. } = &timeline.events[0] else {
        panic!("an incident");
    };
    assert_eq!(components, &["api"]);
}
