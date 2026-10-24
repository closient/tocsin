// SPDX-License-Identifier: AGPL-3.0-only

//! The postman: turns queued announcements into deliveries and sends them,
//! by email and to Slack channels.
//!
//! Each delivery is claimed (marked `sending` and committed) before it is
//! handed to SMTP and finished after. On start the postman fails every
//! delivery a stopped process left `sending`: the server may already have
//! it, and a subscriber missing one email is better than getting two. So
//! delivery is at most once; one failed before reaching SMTP is retried
//! with backoff. Slack posts work the same way, except that a webhook
//! Slack says is revoked marks its channel dead and is never retried.

use std::sync::Arc;
use std::time::Duration;

use tocsin_alert::SendError;
use tocsin_store::{ClaimedMail, ClaimedSlack, MailOutcome, Store};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{Subscriptions, message, paths, token};

/// Deliveries read per round.
const BATCH: u32 = 50;
/// The first retry's wait; each later one doubles it.
const FIRST_RETRY: Duration = Duration::from_secs(5);
/// The longest wait between attempts.
const LONGEST_RETRY: Duration = Duration::from_mins(10);
/// Attempts before a delivery is given up.
const MAX_ATTEMPTS: u32 = 12;
/// How long the postman sleeps with nothing due, in case a wake is lost.
const IDLE: Duration = Duration::from_mins(1);
/// How long after a store error the postman tries again.
const AFTER_ERROR: Duration = Duration::from_secs(30);
/// How often expired sign-ups and old deliveries are pruned.
const PRUNE_EVERY: Duration = Duration::from_hours(1);
/// How long a finished delivery is kept.
const RETENTION: Duration = Duration::from_hours(30 * 24);
/// How long [`Postman::stop`] waits for a send under way.
const GRACE: Duration = Duration::from_secs(10);

/// The running postman.
pub struct Postman {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Postman {
    /// Starts sending what `store` has queued.
    pub fn start(store: Arc<Store>, subscriptions: Arc<Subscriptions>) -> Self {
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run(store, subscriptions, stopped));
        Postman { stop, task }
    }

    /// Stops after the send under way, waiting at most ten seconds for it.
    pub async fn stop(self) {
        self.stop_within(GRACE).await;
    }

    /// Stops after the send under way, waiting at most `grace` for it. A
    /// send cut off is given up on the next start.
    pub async fn stop_within(self, grace: Duration) {
        let _ = self.stop.send(true);
        let mut task = self.task;
        if tokio::time::timeout(grace, &mut task).await.is_err() {
            tracing::warn!("subscriber mail still sending after {grace:?}; cutting it off");
            task.abort();
            let _ = task.await;
        }
    }
}

async fn run(
    store: Arc<Store>,
    subscriptions: Arc<Subscriptions>,
    mut stopped: watch::Receiver<bool>,
) {
    match store.interrupt_mail(tocsin_store::now_ms()).await {
        Ok(0) => {}
        Ok(count) => tracing::warn!(
            count,
            "subscriber mail interrupted by the last stop was given up, not resent"
        ),
        Err(error) => {
            tracing::error!(%error, "could not settle interrupted subscriber mail; not sending");
            return;
        }
    }
    let mut next_prune = Instant::now();
    loop {
        if *stopped.borrow() {
            return;
        }
        if Instant::now() >= next_prune {
            prune(&store).await;
            next_prune = Instant::now() + PRUNE_EVERY;
        }
        let wait = match round(&store, &subscriptions, &stopped).await {
            Ok(wait) => wait,
            Err(error) => {
                tracing::error!(%error, "subscriber mail round failed");
                AFTER_ERROR
            }
        };
        if wait.is_zero() {
            continue;
        }
        let wait = wait.min(next_prune.saturating_duration_since(Instant::now()));
        tokio::select! {
            () = subscriptions.wake().notified() => {}
            () = tokio::time::sleep(wait) => {}
            _ = stopped.changed() => return,
        }
    }
}

/// Expands what was queued, then sends what is due. Resolves with how long
/// to wait before the next round.
async fn round(
    store: &Store,
    subscriptions: &Subscriptions,
    stopped: &watch::Receiver<bool>,
) -> tocsin_store::Result<Duration> {
    let now_ms = tocsin_store::now_ms();
    let expanded = store.expand_announcements(now_ms).await?;
    if expanded > 0 {
        tracing::info!(deliveries = expanded, "subscriber deliveries queued");
    }
    let bag = store.mailbag(now_ms, BATCH).await?;
    let mut full = bag.due.len() >= BATCH as usize;
    for id in bag.due {
        if *stopped.borrow() {
            return Ok(Duration::ZERO);
        }
        let Some(mail) = store.claim_mail(id).await? else {
            continue;
        };
        let outcome = deliver(subscriptions, &mail).await;
        store
            .finish_mail(mail.id, outcome, tocsin_store::now_ms())
            .await?;
    }
    let posts = store.slack_mailbag(now_ms, BATCH).await?;
    full |= posts.due.len() >= BATCH as usize;
    for id in posts.due {
        if *stopped.borrow() {
            return Ok(Duration::ZERO);
        }
        let Some(post) = store.claim_slack(id).await? else {
            continue;
        };
        let outcome = post_slack(store, subscriptions, &post).await;
        store
            .finish_slack(post.id, outcome, tocsin_store::now_ms())
            .await?;
    }
    if full {
        return Ok(Duration::ZERO);
    }
    let next_at_ms = match (bag.next_at_ms, posts.next_at_ms) {
        (Some(mail), Some(slack)) => Some(mail.min(slack)),
        (mail, slack) => mail.or(slack),
    };
    Ok(match next_at_ms {
        Some(at_ms) => Duration::from_millis(
            u64::try_from(at_ms.saturating_sub(tocsin_store::now_ms())).unwrap_or(0),
        )
        .max(Duration::from_millis(1)),
        None => IDLE,
    })
}

/// Sends one claimed delivery.
async fn deliver(subscriptions: &Subscriptions, mail: &ClaimedMail) -> MailOutcome {
    let now_ms = tocsin_store::now_ms();
    let manage = token::manage(mail.subscriber_id, &mail.manage_secret);
    let base = subscriptions.base();
    let text = message::Text {
        subject: mail.subject.clone(),
        body: format!("{}{}", mail.body, message::footer(base, &manage)),
    };
    let headers = vec![
        (
            "List-Unsubscribe",
            format!("<{base}{}>", paths::unsubscribe(&manage)),
        ),
        (
            "List-Unsubscribe-Post",
            "List-Unsubscribe=One-Click".to_owned(),
        ),
    ];
    let composed = subscriptions.compose(
        &mail.email,
        &text,
        &format!("{}.{}", mail.id, mail.key),
        headers,
        now_ms,
    );
    let message = match composed {
        Ok(message) => message,
        Err(error) => return MailOutcome::Failed { error },
    };
    let sent = subscriptions.outbox().send(&mail.email, &message).await;
    let to = token::mask(&mail.email);
    judge(sent, mail.id, &to, mail.attempts, now_ms)
}

/// Posts one claimed delivery to a subscriber's Slack channel.
async fn post_slack(
    store: &Store,
    subscriptions: &Subscriptions,
    post: &ClaimedSlack,
) -> MailOutcome {
    let now_ms = tocsin_store::now_ms();
    let manage = token::manage(post.connection_id, &post.manage_secret);
    let text = message::Text {
        subject: post.subject.clone(),
        body: post.body.clone(),
    };
    let posted = subscriptions
        .post_slack(
            store,
            post.connection_id,
            post.webhook_url.expose(),
            &manage,
            &text,
        )
        .await;
    let to = format!("Slack channel {}", post.connection_id);
    judge(posted, post.id, &to, post.attempts, now_ms)
}

/// What becomes of delivery `id` to `to` after attempt `attempts` ended
/// with `result`: sent, given up, or retried with backoff.
fn judge(
    result: Result<(), SendError>,
    id: i64,
    to: &str,
    attempts: u32,
    now_ms: i64,
) -> MailOutcome {
    match result {
        Ok(()) => {
            tracing::debug!(delivery = id, to, "subscriber delivery sent");
            MailOutcome::Sent
        }
        Err(error) if error.permanent || attempts >= MAX_ATTEMPTS => {
            tracing::warn!(
                delivery = id,
                to,
                attempts,
                error = %error.message,
                "subscriber delivery given up"
            );
            MailOutcome::Failed {
                error: error.message,
            }
        }
        Err(error) => {
            let wait = backoff(attempts).max(error.retry_after.unwrap_or_default());
            tracing::info!(
                delivery = id,
                to,
                attempts,
                error = %error.message,
                "subscriber delivery will be retried"
            );
            MailOutcome::Retry {
                at_ms: now_ms.saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)),
                error: error.message,
            }
        }
    }
}

/// The wait after attempt `attempts` failed.
fn backoff(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(16);
    FIRST_RETRY
        .saturating_mul(1 << doublings)
        .min(LONGEST_RETRY)
}

async fn prune(store: &Store) {
    let now_ms = tocsin_store::now_ms();
    let retention_ms = i64::try_from(RETENTION.as_millis()).unwrap_or(i64::MAX);
    match store
        .prune_subscribers(now_ms, now_ms.saturating_sub(retention_ms))
        .await
    {
        Ok(pruned) if pruned.expired > 0 || pruned.deliveries > 0 => tracing::info!(
            expired = pruned.expired,
            deliveries = pruned.deliveries,
            "subscribers pruned"
        ),
        Ok(_) => {}
        Err(error) => tracing::error!(%error, "subscriber prune failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_to_ten_minutes() {
        assert_eq!(backoff(1), Duration::from_secs(5));
        assert_eq!(backoff(2), Duration::from_secs(10));
        assert_eq!(backoff(5), Duration::from_secs(80));
        assert_eq!(backoff(8), Duration::from_secs(600));
        assert_eq!(backoff(40), Duration::from_secs(600));
    }
}
