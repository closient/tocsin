// SPDX-License-Identifier: AGPL-3.0-only

//! Subscribers through the store's public API: sign-up, confirmation, the
//! matching of announcements to subscriptions, and the delivery log.

use tocsin_store::{
    EVERY, MailOutcome, NewAnnouncement, NewSignup, Preferences, Scope, Signup, Store,
    SubscriberCounts,
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

fn hash(seed: char) -> String {
    seed.to_string().repeat(64)
}

fn signup(email: &str, seed: char, preferences: Preferences) -> NewSignup {
    NewSignup {
        email: email.to_owned(),
        confirm_hash: hash(seed),
        confirm_expires_at_ms: 10_000,
        manage_secret: format!("{seed}").repeat(43),
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

/// Signs up and confirms an address; resolves with its id.
async fn confirmed(store: &Store, email: &str, seed: char, preferences: Preferences) -> i64 {
    let Signup::New(subscriber) = store
        .sign_up(signup(email, seed, preferences))
        .await
        .unwrap()
    else {
        panic!("{email} is new");
    };
    store
        .confirm_subscriber(hash(seed), 1_500)
        .await
        .unwrap()
        .expect("the token works");
    subscriber.id
}

/// Every delivery due, claimed: the recipients, in order.
async fn claim_all(store: &Store) -> Vec<String> {
    let bag = store.mailbag(i64::MAX, 100).await.unwrap();
    let mut emails = Vec::new();
    for id in bag.due {
        if let Some(mail) = store.claim_mail(id).await.unwrap() {
            emails.push(mail.email);
        }
    }
    emails
}

#[tokio::test]
async fn a_second_sign_up_never_changes_a_confirmed_subscriber() {
    let fixture = open();
    let store = &fixture.store;
    let id = confirmed(store, "a@example.com", 'a', components(&["api"])).await;

    let again = store
        .sign_up(signup("a@example.com", 'b', components(&[EVERY])))
        .await
        .unwrap();
    let Signup::Confirmed(subscriber) = again else {
        panic!("already confirmed: {again:?}");
    };
    assert_eq!(subscriber.id, id);
    assert_eq!(subscriber.preferences, components(&["api"]));
    assert!(
        store
            .confirm_subscriber(hash('b'), 1_500)
            .await
            .unwrap()
            .is_none(),
        "the second token was never stored"
    );
}

#[tokio::test]
async fn an_unconfirmed_sign_up_is_refreshed_and_its_token_expires() {
    let fixture = open();
    let store = &fixture.store;
    store
        .sign_up(signup("b@example.com", 'a', components(&["api"])))
        .await
        .unwrap();
    let again = store
        .sign_up(signup("b@example.com", 'b', components(&["web"])))
        .await
        .unwrap();
    let Signup::Unconfirmed(subscriber) = again else {
        panic!("not yet confirmed: {again:?}");
    };
    assert_eq!(subscriber.preferences, components(&["web"]));
    assert!(
        store
            .confirm_subscriber(hash('a'), 1_500)
            .await
            .unwrap()
            .is_none(),
        "the old token was replaced"
    );
    assert!(
        store
            .confirm_subscriber(hash('b'), 10_000)
            .await
            .unwrap()
            .is_none(),
        "a token stops working when it expires"
    );
    let prune = store.prune_subscribers(10_000, 0).await.unwrap();
    assert_eq!(prune.expired, 1);
    assert_eq!(
        store.subscriber_counts().await.unwrap(),
        SubscriberCounts::default()
    );
}

#[tokio::test]
async fn an_announcement_reaches_each_matching_confirmed_subscriber_once() {
    let fixture = open();
    let store = &fixture.store;
    confirmed(store, "api@example.com", 'a', components(&["api"])).await;
    confirmed(store, "all@example.com", 'b', components(&[EVERY])).await;
    confirmed(store, "web@example.com", 'c', components(&["web"])).await;
    confirmed(
        store,
        "releases@example.com",
        'd',
        Preferences {
            components: Vec::new(),
            projects: vec![EVERY.to_owned()],
        },
    )
    .await;
    store
        .sign_up(signup("pending@example.com", 'e', components(&[EVERY])))
        .await
        .unwrap();

    assert!(
        store
            .queue_announcement(incident("incident:1", &["api"]))
            .await
            .unwrap()
    );
    assert!(
        !store
            .queue_announcement(incident("incident:1", &["api"]))
            .await
            .unwrap(),
        "a key is queued once"
    );
    assert_eq!(store.expand_announcements(3_000).await.unwrap(), 2);
    assert_eq!(
        store.expand_announcements(3_000).await.unwrap(),
        0,
        "an announcement expands once"
    );
    assert_eq!(
        claim_all(store).await,
        ["api@example.com", "all@example.com"]
    );

    // An incident on no component reaches every incident subscriber.
    store
        .queue_announcement(incident("incident:2", &[]))
        .await
        .unwrap();
    store.expand_announcements(3_000).await.unwrap();
    assert_eq!(
        claim_all(store).await,
        ["api@example.com", "all@example.com", "web@example.com"]
    );
    assert_eq!(
        store.subscriber_counts().await.unwrap(),
        SubscriberCounts {
            confirmed: 4,
            unconfirmed: 1
        }
    );
}

#[tokio::test]
async fn a_removed_subscriber_is_never_claimed_and_an_interrupted_send_is_never_retried() {
    let fixture = open();
    let store = &fixture.store;
    let gone = confirmed(store, "gone@example.com", 'a', components(&[EVERY])).await;
    confirmed(store, "kept@example.com", 'b', components(&[EVERY])).await;
    store
        .queue_announcement(incident("incident:1", &["api"]))
        .await
        .unwrap();
    store.expand_announcements(3_000).await.unwrap();
    let bag = store.mailbag(3_000, 100).await.unwrap();
    assert_eq!(bag.due.len(), 2);

    assert!(store.remove_subscriber(gone).await.unwrap());
    let kept = store.claim_mail(bag.due[1]).await.unwrap().unwrap();
    assert_eq!(kept.email, "kept@example.com");
    assert_eq!(kept.attempts, 1);
    assert!(
        store.claim_mail(bag.due[0]).await.unwrap().is_none(),
        "the removed subscriber's delivery went with them"
    );
    assert!(
        store.claim_mail(bag.due[1]).await.unwrap().is_none(),
        "a claimed delivery is not claimed again"
    );

    // The process stops mid-send: the delivery is failed, not resent.
    assert_eq!(store.interrupt_mail(4_000).await.unwrap(), 1);
    store
        .finish_mail(kept.id, MailOutcome::Sent, 4_000)
        .await
        .unwrap();
    assert_eq!(store.mailbag(i64::MAX, 100).await.unwrap().due.len(), 0);
}

#[tokio::test]
async fn a_retried_delivery_waits_its_turn() {
    let fixture = open();
    let store = &fixture.store;
    confirmed(store, "a@example.com", 'a', components(&[EVERY])).await;
    store
        .queue_announcement(incident("incident:1", &[]))
        .await
        .unwrap();
    store.expand_announcements(3_000).await.unwrap();
    let id = store.mailbag(3_000, 10).await.unwrap().due[0];
    store.claim_mail(id).await.unwrap().unwrap();
    store
        .finish_mail(
            id,
            MailOutcome::Retry {
                at_ms: 9_000,
                error: "421 try later".to_owned(),
            },
            3_000,
        )
        .await
        .unwrap();
    let bag = store.mailbag(3_000, 10).await.unwrap();
    assert_eq!(bag.due.len(), 0);
    assert_eq!(bag.next_at_ms, Some(9_000));
    let again = store.claim_mail(id).await.unwrap().unwrap();
    assert_eq!(again.attempts, 2);
    store
        .finish_mail(id, MailOutcome::Sent, 9_500)
        .await
        .unwrap();
    let prune = store.prune_subscribers(9_600, 9_600).await.unwrap();
    assert_eq!(prune.deliveries, 1);
    assert_eq!(
        store.expand_announcements(10_000).await.unwrap(),
        0,
        "a pruned delivery is never queued again"
    );
}
