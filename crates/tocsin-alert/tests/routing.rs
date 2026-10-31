// SPDX-License-Identifier: AGPL-3.0-only

//! Which notifiers an alert goes to: matching on checks, heartbeats,
//! components, groups and health, and merging routes that name the same
//! notifier.

mod common;

use common::config;
use tocsin_alert::Router;
use tocsin_core::Health;
use tocsin_core::config::{Id, QUORUM_HEARTBEAT};
use tocsin_core::federation::NodeKey;
use tocsin_store::{AlertRouting, SubjectKind};

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;

/// Two components in a group and one outside it, a check on each, a
/// heartbeat, a Slack notifier per id in `notifiers`, an Alertmanager, and
/// the given routes.
fn router(notifiers: &[&str], routes: &str) -> Router {
    Router::new(&config(&fixture(notifiers, routes)))
}

/// The configuration text behind [`router`].
fn fixture(notifiers: &[&str], routes: &str) -> String {
    let mut toml = String::from(
        "[[group]]\nid = \"customer-facing\"\nname = \"Customer-facing\"\n\n\
         [[component]]\nid = \"web\"\nname = \"Website\"\ngroup = \"customer-facing\"\n\n\
         [[component]]\nid = \"api\"\nname = \"API\"\ngroup = \"customer-facing\"\n\n\
         [[component]]\nid = \"jobs\"\nname = \"Jobs\"\n\n\
         [[check]]\nid = \"home\"\nname = \"Home page\"\ntype = \"tcp\"\n\
         target = \"www.example.com:443\"\ncomponent = \"web\"\n\n\
         [[check]]\nid = \"rest\"\ntype = \"tcp\"\n\
         target = \"api.example.com:443\"\ncomponent = \"api\"\n\n\
         [[check]]\nid = \"queue\"\ntype = \"tcp\"\n\
         target = \"queue.example.com:5672\"\ncomponent = \"jobs\"\n\n\
         [[heartbeat]]\nid = \"backup\"\nname = \"Nightly backup\"\n\
         component = \"jobs\"\ninterval = \"1d\"\ngrace = \"1h\"\n\n\
         [[notifier]]\nid = \"am\"\ntype = \"alertmanager\"\n\n\
         [notifier.alertmanager]\nurl = \"http://alertmanager.example.com:9093/\"\n\n",
    );
    for id in notifiers {
        toml.push_str(&common::notifier(id));
    }
    toml.push_str(routes);
    toml
}

/// (notifier, route, repeat) for every target, by notifier.
fn targets(routing: &AlertRouting) -> Vec<(&str, &str, Option<i64>)> {
    let mut targets: Vec<_> = routing
        .targets
        .iter()
        .map(|target| {
            (
                target.notifier_id.as_str(),
                target.route_id.as_str(),
                target.repeat_ms,
            )
        })
        .collect();
    targets.sort_unstable();
    targets
}

fn notified(
    router: &Router,
    kind: SubjectKind,
    subject: &str,
    component: &str,
    health: Health,
) -> Vec<String> {
    let routing = router.routing(kind, subject, Some(component), health);
    targets(&routing)
        .into_iter()
        .map(|(notifier, ..)| notifier.to_owned())
        .collect()
}

#[test]
fn a_route_matches_its_checks_heartbeats_components_and_groups() {
    let router = router(
        &["oncheck", "onbeat", "oncomponent", "ongroup", "all"],
        "[[route]]\nid = \"check\"\nnotifiers = [\"oncheck\"]\nchecks = [\"queue\"]\n\n\
         [[route]]\nid = \"heartbeat\"\nnotifiers = [\"onbeat\"]\nheartbeats = [\"backup\"]\n\n\
         [[route]]\nid = \"component\"\nnotifiers = [\"oncomponent\"]\ncomponents = [\"api\"]\n\n\
         [[route]]\nid = \"group\"\nnotifiers = [\"ongroup\"]\ngroups = [\"customer-facing\"]\n\n\
         [[route]]\nid = \"all\"\nnotifiers = [\"all\"]\n",
    );

    assert_eq!(
        notified(&router, SubjectKind::Check, "home", "web", Health::Down),
        ["all", "ongroup"]
    );
    assert_eq!(
        notified(&router, SubjectKind::Check, "rest", "api", Health::Down),
        ["all", "oncomponent", "ongroup"]
    );
    assert_eq!(
        notified(&router, SubjectKind::Check, "queue", "jobs", Health::Down),
        ["all", "oncheck"]
    );
    // A heartbeat matches `heartbeats`, never `checks`.
    assert_eq!(
        notified(
            &router,
            SubjectKind::Heartbeat,
            "backup",
            "jobs",
            Health::Down
        ),
        ["all", "onbeat"]
    );
}

#[test]
fn routes_and_names_are_looked_up_by_kind() {
    let router = router(
        &["oncheck", "onbeat"],
        "[[route]]\nid = \"check\"\nnotifiers = [\"oncheck\"]\nchecks = [\"queue\"]\n\n\
         [[route]]\nid = \"heartbeat\"\nnotifiers = [\"onbeat\"]\nheartbeats = [\"backup\"]\n",
    );

    // Configuration keeps the ids apart, but the router still never
    // guesses: a check named like the heartbeat matches neither route.
    assert_eq!(
        notified(
            &router,
            SubjectKind::Heartbeat,
            "backup",
            "jobs",
            Health::Down
        ),
        ["onbeat"]
    );
    let nobody: [String; 0] = [];
    assert_eq!(
        notified(&router, SubjectKind::Check, "backup", "jobs", Health::Down),
        nobody
    );
    assert_eq!(
        notified(
            &router,
            SubjectKind::Heartbeat,
            "queue",
            "jobs",
            Health::Down
        ),
        nobody
    );
    assert_eq!(
        router.subject_name(SubjectKind::Heartbeat, "backup"),
        "Nightly backup"
    );
    assert_eq!(router.subject_name(SubjectKind::Check, "backup"), "backup");
}

#[test]
fn a_route_waits_for_its_notify_on_health() {
    let router = router(
        &["pager", "chat"],
        "[[route]]\nid = \"page\"\nnotifiers = [\"pager\"]\nnotify_on = \"down\"\n\n\
         [[route]]\nid = \"chat\"\nnotifiers = [\"chat\"]\nnotify_on = \"degraded\"\n",
    );

    assert_eq!(
        notified(&router, SubjectKind::Check, "home", "web", Health::Degraded),
        ["chat"]
    );
    assert_eq!(
        notified(&router, SubjectKind::Check, "home", "web", Health::Down),
        ["chat", "pager"]
    );
}

#[test]
fn routes_naming_one_notifier_merge_into_one_target() {
    let router = router(
        &["slack"],
        "[[route]]\nid = \"daily\"\nnotifiers = [\"slack\"]\nrepeat = \"1d\"\n\n\
         [[route]]\nid = \"hourly\"\nnotifiers = [\"slack\"]\nrepeat = \"1h\"\n\n\
         [[route]]\nid = \"quiet\"\nnotifiers = [\"slack\"]\nrepeat = \"0s\"\n",
    );

    // One alert per notifier: the first route names it, and the shortest
    // repeat wins, over "never" too.
    assert_eq!(
        targets(&router.routing(SubjectKind::Check, "home", Some("web"), Health::Down)),
        [("slack", "daily", Some(HOUR_MS))]
    );
}

#[test]
fn an_alertmanager_target_reposts_at_least_every_minute() {
    // Alertmanager drops an alert it has not heard of within its
    // resolve_timeout, so it hears every minute whatever the route says.
    for (repeat, slack_repeat) in [("0s", None), ("4h", Some(4 * HOUR_MS))] {
        let router = router(
            &["slack"],
            &format!(
                "[[route]]\nid = \"ops\"\nnotifiers = [\"am\", \"slack\"]\nrepeat = \"{repeat}\"\n"
            ),
        );
        assert_eq!(
            targets(&router.routing(SubjectKind::Check, "home", Some("web"), Health::Down)),
            [
                ("am", "ops", Some(MINUTE_MS)),
                ("slack", "ops", slack_repeat),
            ],
            "repeat = {repeat}"
        );
    }
}

#[test]
fn a_subject_s_topology_node_is_pinned_among_its_labels_only_when_set() {
    let toml = "[[check]]\nid = \"home\"\ntype = \"tcp\"\ntarget = \"www.example.com:443\"\n\
                topology_node = \"production-01/web\"\n\n\
                [[check]]\nid = \"rest\"\ntype = \"tcp\"\ntarget = \"api.example.com:443\"\n\n\
                [[heartbeat]]\nid = \"backup\"\ninterval = \"1d\"\n\
                topology_node = \"replication-03/job:backup\"\n";
    let router = Router::new(&config(toml));
    let node = |kind, subject: &str| {
        router
            .routing(kind, subject, None, Health::Down)
            .labels
            .get("topology_node")
            .cloned()
    };

    assert_eq!(
        node(SubjectKind::Check, "home").as_deref(),
        Some("production-01/web")
    );
    assert_eq!(
        node(SubjectKind::Heartbeat, "backup").as_deref(),
        Some("replication-03/job:backup")
    );
    assert_eq!(node(SubjectKind::Check, "rest"), None);
}

/// A quorum check's `quorum:<check>` heartbeat, down when the check's
/// nodes cannot decide its health, routes as the check: the routes that
/// page for `queue` page for its lost quorum, and a route on heartbeats
/// does not.
#[test]
fn a_lost_quorum_routes_as_its_check() {
    let mut config = config(&fixture(
        &["oncheck", "onbeat", "oncomponent"],
        "[[route]]\nid = \"check\"\nnotifiers = [\"oncheck\"]\nchecks = [\"queue\"]\n\n\
         [[route]]\nid = \"heartbeat\"\nnotifiers = [\"onbeat\"]\nheartbeats = [\"backup\"]\n\n\
         [[route]]\nid = \"component\"\nnotifiers = [\"oncomponent\"]\ncomponents = [\"jobs\"]\n",
    ));
    let mut quorum = config.heartbeats[0].clone();
    quorum.id = Id::synthetic(QUORUM_HEARTBEAT, &Id::parse("queue").unwrap());
    quorum.component = None;
    config.heartbeats.push(quorum);
    let router = Router::new(&config);

    let routing = router.routing(SubjectKind::Heartbeat, "quorum:queue", None, Health::Down);
    let mut found: Vec<&str> = targets(&routing)
        .into_iter()
        .map(|(notifier, ..)| notifier)
        .collect();
    found.sort_unstable();
    assert_eq!(found, ["oncheck", "oncomponent"]);
}

/// The hub `node-a`, which this node runs as, and `spoke` and `edge`
/// pushing to it, so `node-a` keeps `node:spoke` and `node:edge`, and each
/// spoke `upstream:node-a`.
fn federation() -> String {
    let key = |seed: u8| NodeKey::from_seed([seed; 32]).public_key();
    format!(
        "[server]\nnode_id = \"node-a\"\n\n\
         [[node]]\nid = \"node-a\"\nurl = \"https://node-a.example.com\"\npublic_keys = [\"{}\"]\n\n\
         [[node]]\nid = \"spoke\"\npublic_keys = [\"{}\"]\npush_to = [\"node-a\"]\n\n\
         [[node]]\nid = \"edge\"\npublic_keys = [\"{}\"]\npush_to = [\"node-a\"]\n\n\
         [federation]\ndefault_nodes = [\"node-a\"]\n\n",
        key(1),
        key(2),
        key(3),
    )
}

#[test]
fn a_route_matches_the_heartbeats_a_node_keeps_for_other_nodes() {
    let router = router(
        &["pager", "chat", "onbeat"],
        &format!(
            "{}[[route]]\nid = \"spoke\"\nnotifiers = [\"pager\"]\nheartbeats = [\"node:spoke\"]\n\
             notify_on = \"down\"\nrepeat = \"1h\"\n\n\
             [[route]]\nid = \"upstream\"\nnotifiers = [\"chat\"]\nheartbeats = [\"upstream:node-a\"]\n\n\
             [[route]]\nid = \"backup\"\nnotifiers = [\"onbeat\"]\nheartbeats = [\"backup\"]\n",
            federation()
        ),
    );
    let sent_to = |kind, subject: &str, health| {
        targets(&router.routing(kind, subject, None, health))
            .into_iter()
            .map(|(notifier, route, repeat)| (notifier.to_owned(), route.to_owned(), repeat))
            .collect::<Vec<_>>()
    };
    let to =
        |notifier: &str, route: &str, repeat| vec![(notifier.to_owned(), route.to_owned(), repeat)];

    // Each goes where its route says, with that route's repeat, exactly as
    // a `[[heartbeat]]` would.
    assert_eq!(
        sent_to(SubjectKind::Heartbeat, "node:spoke", Health::Down),
        to("pager", "spoke", Some(HOUR_MS))
    );
    assert_eq!(
        sent_to(SubjectKind::Heartbeat, "upstream:node-a", Health::Down),
        to("chat", "upstream", Some(4 * HOUR_MS))
    );
    // Another node's heartbeat, the bare node id, the other kind and the
    // same id as a check match nothing.
    assert_eq!(
        sent_to(SubjectKind::Heartbeat, "node:edge", Health::Down),
        []
    );
    assert_eq!(sent_to(SubjectKind::Heartbeat, "spoke", Health::Down), []);
    assert_eq!(
        sent_to(SubjectKind::Heartbeat, "upstream:spoke", Health::Down),
        []
    );
    assert_eq!(sent_to(SubjectKind::Check, "node:spoke", Health::Down), []);
    // `notify_on` holds too.
    assert_eq!(
        sent_to(SubjectKind::Heartbeat, "node:spoke", Health::Degraded),
        []
    );
}
