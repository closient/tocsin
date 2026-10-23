// SPDX-License-Identifier: AGPL-3.0-only

//! Which notifiers an alert goes to: matching on checks, heartbeats,
//! components, groups and health, and merging routes that name the same
//! notifier.

mod common;

use common::config;
use tocsin_alert::Router;
use tocsin_core::Health;
use tocsin_store::{AlertRouting, SubjectKind};

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;

/// Two components in a group and one outside it, a check on each, a
/// heartbeat, a Slack notifier per id in `notifiers`, an Alertmanager, and
/// the given routes.
fn router(notifiers: &[&str], routes: &str) -> Router {
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
    Router::new(&config(&toml))
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
