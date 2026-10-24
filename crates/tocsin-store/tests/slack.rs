// SPDX-License-Identifier: AGPL-3.0-only

//! Slack connections through the store's public API: operator connections
//! by name, subscriber connections sharing the email announcements, and a
//! revoked connection that is never posted to again.

use tocsin_store::{
    EVERY, MailOutcome, NewAnnouncement, NewSlackConnection, Preferences, Scope, SlackCounts,
    SlackHook, SlackPurpose, Store, WebhookUrl,
};

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
}

fn open() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    Fixture { _dir: dir, store }
}

fn operator(name: &str, url: &str) -> NewSlackConnection {
    NewSlackConnection {
        purpose: SlackPurpose::Operator,
        name: Some(name.to_owned()),
        webhook_url: WebhookUrl::new(url.to_owned()),
        channel: format!("#{name}"),
        channel_id: Some(format!("C-{name}")),
        team: "Acme".to_owned(),
        team_id: Some("T-acme".to_owned()),
        configuration_url: None,
        created_by: Some("admin@acme.example".to_owned()),
        manage_secret: None,
        preferences: Preferences::default(),
        now_ms: 1_000,
    }
}

fn subscriber(channel: &str, preferences: Preferences) -> NewSlackConnection {
    NewSlackConnection {
        purpose: SlackPurpose::Subscriber,
        name: None,
        webhook_url: WebhookUrl::new(format!("https://hooks.example/{channel}")),
        channel: format!("#{channel}"),
        channel_id: Some(format!("C-{channel}")),
        team: "Acme".to_owned(),
        team_id: Some("T-acme".to_owned()),
        configuration_url: None,
        created_by: None,
        manage_secret: Some(channel.repeat(43).chars().take(43).collect()),
        preferences,
        now_ms: 1_000,
    }
}

fn components(ids: &[&str]) -> Preferences {
    Preferences {
        components: ids.iter().map(|id| (*id).to_owned()).collect(),
        projects: Vec::new(),
    }
}

fn incident(key: &str, targets: &[&str]) -> NewAnnouncement {
    NewAnnouncement {
        key: key.to_owned(),
        scope: Scope::Component,
        targets: targets.iter().map(|id| (*id).to_owned()).collect(),
        subject: format!("subject {key}"),
        body: "body".to_owned(),
        created_at_ms: 2_000,
    }
}

/// Every Slack delivery due, claimed: the webhook URLs, in order.
async fn claim_all(store: &Store) -> Vec<String> {
    let bag = store.slack_mailbag(i64::MAX, 100).await.unwrap();
    let mut urls = Vec::new();
    for id in bag.due {
        if let Some(post) = store.claim_slack(id).await.unwrap() {
            urls.push(post.webhook_url.expose().to_owned());
            store
                .finish_slack(post.id, MailOutcome::Sent, 3_000)
                .await
                .unwrap();
        }
    }
    urls
}

#[tokio::test]
async fn an_operator_connection_is_found_by_name_and_reconnecting_mends_it() {
    let fixture = open();
    let store = &fixture.store;
    assert_eq!(
        store.slack_hook("ops".to_owned()).await.unwrap(),
        SlackHook::Missing
    );
    let first = store
        .connect_slack(operator("ops", "https://hooks.example/one"))
        .await
        .unwrap();
    let SlackHook::Live { id, url } = store.slack_hook("ops".to_owned()).await.unwrap() else {
        panic!("connected");
    };
    assert_eq!((id, url.expose()), (first.id, "https://hooks.example/one"));

    assert!(
        store
            .kill_slack(id, "channel_not_found".to_owned(), 2_000)
            .await
            .unwrap()
    );
    assert!(
        !store
            .kill_slack(id, "channel_not_found".to_owned(), 2_000)
            .await
            .unwrap(),
        "already dead"
    );
    assert_eq!(
        store.slack_hook("ops".to_owned()).await.unwrap(),
        SlackHook::Dead {
            reason: "channel_not_found".to_owned()
        }
    );
    assert_eq!(
        store.slack_census().await.unwrap(),
        (
            SlackCounts {
                operator_dead: 1,
                ..SlackCounts::default()
            },
            vec![("ops".to_owned(), false)]
        )
    );

    let again = store
        .connect_slack(operator("ops", "https://hooks.example/two"))
        .await
        .unwrap();
    assert_eq!(again.id, first.id, "the same name is the same connection");
    assert!(again.is_live());
    let SlackHook::Live { url, .. } = store.slack_hook("ops".to_owned()).await.unwrap() else {
        panic!("mended");
    };
    assert_eq!(url.expose(), "https://hooks.example/two");
}

#[tokio::test]
async fn an_announcement_reaches_matching_live_slack_channels_beside_email() {
    let fixture = open();
    let store = &fixture.store;
    let api = store
        .connect_slack(subscriber("api", components(&["api"])))
        .await
        .unwrap();
    store
        .connect_slack(subscriber("everything", components(&[EVERY])))
        .await
        .unwrap();
    store
        .connect_slack(subscriber("web", components(&["web"])))
        .await
        .unwrap();
    // An operator connection never hears announcements.
    store
        .connect_slack(operator("ops", "https://hooks.example/ops"))
        .await
        .unwrap();

    store
        .queue_announcement(incident("incident:1", &["api"]))
        .await
        .unwrap();
    assert_eq!(store.expand_announcements(2_500).await.unwrap(), 2);
    let mut posted = claim_all(store).await;
    posted.sort();
    assert_eq!(
        posted,
        ["https://hooks.example/api", "https://hooks.example/everything"]
    );
    assert_eq!(store.expand_announcements(2_600).await.unwrap(), 0, "once");

    // A dead connection is skipped, and what was waiting for it fails.
    store
        .queue_announcement(incident("incident:2", &["api"]))
        .await
        .unwrap();
    store.expand_announcements(2_700).await.unwrap();
    store
        .kill_slack(api.id, "channel_is_archived".to_owned(), 2_800)
        .await
        .unwrap();
    assert_eq!(claim_all(store).await, ["https://hooks.example/everything"]);
    let left = store.slack_mailbag(i64::MAX, 100).await.unwrap();
    assert!(left.due.is_empty() && left.next_at_ms.is_none(), "{left:?}");
    store
        .queue_announcement(incident("incident:3", &["api"]))
        .await
        .unwrap();
    assert_eq!(
        store.expand_announcements(2_900).await.unwrap(),
        1,
        "the dead channel is not queued"
    );
}

#[tokio::test]
async fn adding_a_channel_twice_replaces_it_and_removal_forgets_it() {
    let fixture = open();
    let store = &fixture.store;
    store
        .connect_slack(subscriber("api", components(&["api"])))
        .await
        .unwrap();
    let second = store
        .connect_slack(subscriber("api", components(&[EVERY])))
        .await
        .unwrap();
    let listed = store
        .slack_connections(SlackPurpose::Subscriber)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, second.id);
    assert_eq!(listed[0].preferences, components(&[EVERY]));

    assert!(
        store
            .set_slack_preferences(second.id, components(&["web"]))
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .slack_connection(second.id)
            .await
            .unwrap()
            .unwrap()
            .preferences,
        components(&["web"])
    );
    assert!(store.remove_slack(second.id).await.unwrap());
    assert!(store.slack_connection(second.id).await.unwrap().is_none());
}

#[tokio::test]
async fn an_interrupted_slack_post_is_failed_never_resent() {
    let fixture = open();
    let store = &fixture.store;
    store
        .connect_slack(subscriber("api", components(&[EVERY])))
        .await
        .unwrap();
    store
        .queue_announcement(incident("incident:1", &[]))
        .await
        .unwrap();
    store.expand_announcements(2_500).await.unwrap();
    let bag = store.slack_mailbag(i64::MAX, 100).await.unwrap();
    let claimed = store.claim_slack(bag.due[0]).await.unwrap().unwrap();
    assert_eq!(claimed.attempts, 1);
    assert_eq!(store.interrupt_mail(3_000).await.unwrap(), 1);
    assert!(store.claim_slack(claimed.id).await.unwrap().is_none());
    let pruned = store.prune_subscribers(4_000, 3_500).await.unwrap();
    assert_eq!(pruned.deliveries, 1);
}

#[test]
fn inspecting_reads_operator_names_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(tocsin_store::DB_FILE);
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = Store::open(&path).unwrap();
            store
                .connect_slack(operator("ops", "https://hooks.example/ops"))
                .await
                .unwrap();
            store.close().await.unwrap();
        });
    }
    assert_eq!(
        tocsin_store::inspect_slack(&path).unwrap(),
        Some(vec![("ops".to_owned(), true)])
    );
    assert!(matches!(
        tocsin_store::inspect_slack_hook(&path, "ops").unwrap(),
        SlackHook::Live { .. }
    ));
    assert_eq!(
        tocsin_store::inspect_slack_hook(&path, "other").unwrap(),
        SlackHook::Missing
    );
}
