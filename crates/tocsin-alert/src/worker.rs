// SPDX-License-Identifier: AGPL-3.0-only

//! The delivery worker: drains the outbox to the notifiers.
//!
//! Everything it knows lives in the store, so a restart loses nothing: a
//! delivery pending at shutdown is sent after the next start, and repeats
//! come from the open alerts' `next_repeat_at`, not from timers.
//!
//! Each delivery sends on its own task, so a slow or failing notifier holds
//! up only its own alerts. An alert has at most one message and one cancel
//! in flight, and the outbox only offers its oldest pending message, so a
//! resolve never overtakes its fire; a cancel never holds up a message.
//! Each notifier has at most its `max_in_flight` sends at once, so a storm
//! of alerts queues rather than floods it. Retries back off exponentially
//! with full jitter, and a 429 or 503 `Retry-After` pauses the notifier
//! for as long as it asks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tocsin_engine::ports::Clock;
use tocsin_store::{DeliveryKind, DeliveryOutcome, DeliveryRetention, PendingDelivery, Store};
use tokio::sync::{Notify, watch};
use tokio::task::{self, JoinHandle, JoinSet};

use crate::alert::{Alert, AlertEvent};
use crate::metrics::AlertMetrics;
use crate::notifier::SendError;
use crate::notifiers::Notifiers;
use crate::route::Router;

/// How long to wait before the next attempt after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the first failure; each later failure doubles it.
    pub base: Duration,
    /// The longest wait.
    pub cap: Duration,
    /// How much of the wait is random.
    pub jitter: Jitter,
}

/// Randomness in a retry's wait, so deliveries that failed together do not
/// retry together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jitter {
    /// Anywhere from nothing to the full wait ("full jitter").
    Full,
    /// Exactly the wait.
    None,
}

impl Backoff {
    /// The longest wait after `attempts` failed attempts (at least one).
    #[must_use]
    pub fn delay(self, attempts: u32) -> Duration {
        let doublings = attempts.saturating_sub(1).min(20);
        self.base
            .checked_mul(1 << doublings)
            .map_or(self.cap, |delay| delay.min(self.cap))
    }

    /// The wait after `attempts` failed attempts, jittered.
    #[must_use]
    pub fn wait(self, attempts: u32) -> Duration {
        let delay = self.delay(attempts);
        match self.jitter {
            Jitter::None => delay,
            Jitter::Full => {
                let millis = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
                Duration::from_millis(fastrand::u64(0..=millis))
            }
        }
    }
}

/// How the worker behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSettings {
    /// Retry spacing.
    pub backoff: Backoff,
    /// Attempts before a delivery is recorded as failed.
    pub max_attempts: u32,
    /// The most one send may take, whatever the notifier's own timeouts.
    pub send_timeout: Duration,
    /// The longest the worker sleeps without looking at the outbox.
    pub idle: Duration,
    /// The wait after a store error before trying again.
    pub store_retry: Duration,
    /// How often finished deliveries are pruned.
    pub prune_every: Duration,
    /// How long finished deliveries are kept.
    pub retention: DeliveryRetention,
    /// How long [`Alerter::stop`] lets sends in flight finish; the rest
    /// are abandoned and stay pending.
    pub stop_grace: Duration,
}

impl Default for WorkerSettings {
    /// Retries after up to 5s, 10s, 20s ... capped at 10m, fully jittered,
    /// twelve attempts (up to about an hour); delivered rows kept a week
    /// and failed ones 90 days, pruned 2000 rows at a time.
    fn default() -> Self {
        const DAY_MS: i64 = 86_400_000;
        WorkerSettings {
            backoff: Backoff {
                base: Duration::from_secs(5),
                cap: Duration::from_secs(600),
                jitter: Jitter::Full,
            },
            max_attempts: 12,
            send_timeout: Duration::from_secs(30),
            idle: Duration::from_secs(60),
            store_retry: Duration::from_secs(5),
            prune_every: Duration::from_secs(3600),
            retention: DeliveryRetention {
                delivered_ms: 7 * DAY_MS,
                failed_ms: 90 * DAY_MS,
                chunk_rows: 2000,
                pause: Duration::from_millis(10),
            },
            stop_grace: Duration::from_secs(2),
        }
    }
}

/// The running delivery worker.
#[derive(Debug)]
pub struct Alerter {
    stop: watch::Sender<bool>,
    wake: Arc<Notify>,
    task: JoinHandle<()>,
}

/// An alert's two independent queues: its messages (fire, repeat,
/// resolve) and its cancels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Lane {
    Message,
    Cancel,
}

impl Lane {
    fn of(kind: DeliveryKind) -> Self {
        if kind == DeliveryKind::Cancel {
            Lane::Cancel
        } else {
            Lane::Message
        }
    }
}

/// A send in flight, kept outside its task so a panic can still be
/// recorded.
struct InFlight {
    delivery_id: i64,
    alert_id: i64,
    lane: Lane,
    notifier_id: String,
    /// The alert's identity, for logs.
    identity: String,
    attempts: u32,
}

struct Worker {
    store: Arc<Store>,
    notifiers: Notifiers,
    router: Arc<Router>,
    clock: Arc<dyn Clock>,
    metrics: AlertMetrics,
    settings: WorkerSettings,
    wake: Arc<Notify>,
    in_flight: JoinSet<Result<(), SendError>>,
    sends: HashMap<task::Id, InFlight>,
    busy: HashSet<(i64, Lane)>,
    /// Sends in flight by notifier.
    sending: HashMap<String, usize>,
    /// Notifiers that asked, by `Retry-After`, to be left alone until then
    /// (unix milliseconds).
    paused_until: HashMap<String, i64>,
}

impl Alerter {
    /// Starts draining `store`'s outbox to `notifiers`.
    #[must_use]
    pub fn start(
        store: Arc<Store>,
        notifiers: Notifiers,
        router: Arc<Router>,
        clock: Arc<dyn Clock>,
        metrics: AlertMetrics,
        settings: WorkerSettings,
    ) -> Self {
        let (stop, stopped) = watch::channel(false);
        let wake = Arc::new(Notify::new());
        let worker = Worker {
            store,
            notifiers,
            router,
            clock,
            metrics,
            settings,
            wake: Arc::clone(&wake),
            in_flight: JoinSet::new(),
            sends: HashMap::new(),
            busy: HashSet::new(),
            sending: HashMap::new(),
            paused_until: HashMap::new(),
        };
        let task = tokio::spawn(worker.run(stopped));
        Alerter { stop, wake, task }
    }

    /// What wakes the worker when a delivery is queued; hand it to
    /// [`AlertingHistory`](crate::AlertingHistory).
    #[must_use]
    pub fn waker(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }

    /// Stops the worker. Sends in flight get the stop grace to finish;
    /// the rest are abandoned and stay pending for the next start. When
    /// this returns the worker holds no reference to the store.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        if let Err(error) = self.task.await
            && error.is_panic()
        {
            tracing::error!(%error, "the alert delivery worker panicked");
        }
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl Worker {
    async fn run(mut self, mut stopped: watch::Receiver<bool>) {
        let mut next_prune = self.clock.now_ms();
        loop {
            if *stopped.borrow() {
                break;
            }
            let now = self.clock.now_ms();
            if now >= next_prune {
                next_prune = now.saturating_add(millis(self.settings.prune_every));
                if let Err(error) = self
                    .store
                    .prune_deliveries(self.settings.retention, now)
                    .await
                {
                    tracing::warn!(%error, "pruning old alert deliveries failed");
                }
            }
            let wake_at = match self.dispatch(now).await {
                Ok(wake_at) => wake_at,
                Err(error) => {
                    tracing::warn!(%error, "reading the alert outbox failed; retrying");
                    now.saturating_add(millis(self.settings.store_retry))
                }
            };
            let pause = Duration::from_millis(u64::try_from(wake_at - now).unwrap_or_default());
            tokio::select! {
                _ = stopped.changed() => break,
                () = self.wake.notified() => {}
                Some(joined) = self.in_flight.join_next_with_id(), if !self.in_flight.is_empty() => {
                    self.finish(joined).await;
                }
                () = tokio::time::sleep(pause) => {}
            }
        }
        self.shut_down().await;
    }

    /// Queues due repeats and starts every due delivery whose alert lane
    /// has nothing in flight, while its notifier has a free slot and is not
    /// paused; returns when to look again. Repeats are queued, a write,
    /// only when one is due.
    async fn dispatch(&mut self, now: i64) -> tocsin_store::Result<i64> {
        let mut outbox = self.store.outbox().await?;
        if outbox
            .next_repeat_at_ms
            .is_some_and(|repeat_at| repeat_at <= now)
        {
            self.store.queue_repeats(now).await?;
            outbox = self.store.outbox().await?;
        }
        let mut wake_at = now.saturating_add(millis(self.settings.idle));
        if let Some(repeat_at) = outbox.next_repeat_at_ms {
            wake_at = wake_at.min(repeat_at);
        }
        for delivery in outbox.pending {
            let notifier_id = &delivery.alert.notifier_id;
            if self
                .busy
                .contains(&(delivery.alert.id, Lane::of(delivery.kind)))
            {
                continue;
            }
            if delivery.next_attempt_at_ms > now {
                wake_at = wake_at.min(delivery.next_attempt_at_ms);
                continue;
            }
            if let Some(&until) = self.paused_until.get(notifier_id) {
                if until > now {
                    wake_at = wake_at.min(until);
                    continue;
                }
                self.paused_until.remove(notifier_id);
            }
            // A full notifier is looked at again when one of its sends
            // finishes, which wakes the loop.
            if self.sending.get(notifier_id).copied().unwrap_or_default()
                >= self.router.max_in_flight(notifier_id)
            {
                continue;
            }
            self.spawn(&delivery, now);
        }
        Ok(wake_at.max(now))
    }

    fn spawn(&mut self, delivery: &PendingDelivery, now: i64) {
        let alert = self.message(delivery, now);
        let lane = Lane::of(delivery.kind);
        let notifier = self.notifiers.get(&delivery.alert.notifier_id).cloned();
        let notifier_id = delivery.alert.notifier_id.clone();
        let send_timeout = self.settings.send_timeout;
        let meta = InFlight {
            delivery_id: delivery.id,
            alert_id: delivery.alert.id,
            lane,
            notifier_id: notifier_id.clone(),
            identity: alert.identity(),
            attempts: delivery.attempts,
        };
        self.busy.insert((meta.alert_id, lane));
        *self.sending.entry(notifier_id.clone()).or_default() += 1;
        let handle = self.in_flight.spawn(async move {
            let Some(notifier) = notifier else {
                return Err(SendError::permanent(format!(
                    "notifier `{notifier_id}` is no longer configured"
                )));
            };
            let send = match lane {
                Lane::Message => notifier.send(&alert),
                Lane::Cancel => notifier.cancel(&alert),
            };
            tokio::time::timeout(send_timeout, send)
                .await
                .unwrap_or_else(|_| {
                    Err(SendError::transient(format!(
                        "no answer within {}s",
                        send_timeout.as_secs()
                    )))
                })
        });
        self.sends.insert(handle.id(), meta);
    }

    /// The message a pending delivery stands for; for a cancel, the fire
    /// that rang.
    fn message(&self, delivery: &PendingDelivery, now: i64) -> Alert {
        let stored = &delivery.alert;
        Alert {
            event: match delivery.kind {
                DeliveryKind::Fire | DeliveryKind::Cancel => AlertEvent::Fire,
                DeliveryKind::Repeat => AlertEvent::Repeat,
                DeliveryKind::Resolve => AlertEvent::Resolve,
            },
            node_id: stored.node_id.clone(),
            subject_kind: stored.subject_kind,
            subject_id: stored.subject_id.clone(),
            subject_name: self
                .router
                .subject_name(stored.subject_kind, &stored.subject_id),
            component_id: stored.component_id.clone(),
            component_name: stored
                .component_id
                .as_deref()
                .and_then(|component| self.router.component_name(component)),
            health: delivery.health,
            started_at_ms: stored.started_at_ms,
            ended_at_ms: (delivery.kind == DeliveryKind::Resolve)
                .then_some(stored.ended_at_ms)
                .flatten(),
            sent_at_ms: now,
            delivery_id: Some(delivery.id),
            test: false,
        }
    }

    /// Records how a send went. A send that panicked counts as a failed
    /// attempt, retried with backoff like any other.
    async fn finish(&mut self, joined: Result<(task::Id, Result<(), SendError>), task::JoinError>) {
        let (id, result) = match joined {
            Ok((id, result)) => (id, Some(Ok(result))),
            Err(error) => (error.id(), (!error.is_cancelled()).then_some(Err(error))),
        };
        let Some(sent) = self.sends.remove(&id) else {
            return;
        };
        self.busy.remove(&(sent.alert_id, sent.lane));
        if let Some(sending) = self.sending.get_mut(&sent.notifier_id) {
            *sending = sending.saturating_sub(1);
        }
        let result = match result {
            // Cancelled at shutdown: the row stays pending.
            None => return,
            Some(Ok(Ok(()))) => Ok(()),
            Some(Ok(Err(error))) => Err(error),
            Some(Err(panic)) => {
                tracing::error!(
                    notifier = %sent.notifier_id,
                    alert = %sent.identity,
                    delivery = sent.delivery_id,
                    error = %panic,
                    "an alert send panicked"
                );
                Err(SendError::transient("the send panicked"))
            }
        };
        let now = self.clock.now_ms();
        let attempts = sent.attempts.saturating_add(1);
        let outcome = match result {
            Ok(()) => DeliveryOutcome::Delivered,
            Err(error) => self.failed(&sent, attempts, error, now),
        };
        if let Err(error) = self
            .store
            .finish_delivery(sent.delivery_id, outcome, now)
            .await
        {
            // The row stays pending and is sent again: at least once.
            tracing::warn!(%error, delivery = sent.delivery_id, "recording an alert delivery failed");
        }
    }

    /// What a failed attempt becomes: a retry after the backoff, or after
    /// the destination's `Retry-After`, which also pauses its notifier; or
    /// a failure for good.
    fn failed(
        &mut self,
        sent: &InFlight,
        attempts: u32,
        error: SendError,
        now: i64,
    ) -> DeliveryOutcome {
        self.metrics.attempt_failed(&sent.notifier_id);
        if error.permanent || attempts >= self.settings.max_attempts {
            self.metrics.abandoned(&sent.notifier_id);
            tracing::warn!(
                notifier = %sent.notifier_id,
                alert = %sent.identity,
                delivery = sent.delivery_id,
                attempts,
                error = %error.message,
                "alert delivery failed for good"
            );
            return DeliveryOutcome::Failed {
                error: error.message,
            };
        }
        let wait = if let Some(retry_after) = error.retry_after {
            let until = now.saturating_add(millis(retry_after));
            let paused = self
                .paused_until
                .entry(sent.notifier_id.clone())
                .or_insert(until);
            *paused = (*paused).max(until);
            retry_after
        } else {
            self.settings.backoff.wait(attempts)
        };
        tracing::info!(
            notifier = %sent.notifier_id,
            alert = %sent.identity,
            delivery = sent.delivery_id,
            attempts,
            wait_ms = millis(wait),
            error = %error.message,
            "alert delivery failed; will retry"
        );
        DeliveryOutcome::Retry {
            at_ms: now.saturating_add(millis(wait)),
            error: error.message,
        }
    }

    /// Lets sends in flight finish within the grace, then abandons the
    /// rest; their rows stay pending.
    async fn shut_down(mut self) {
        let grace = tokio::time::sleep(self.settings.stop_grace);
        tokio::pin!(grace);
        while !self.in_flight.is_empty() {
            tokio::select! {
                () = &mut grace => break,
                Some(joined) = self.in_flight.join_next_with_id() => self.finish(joined).await,
            }
        }
        let abandoned = self.in_flight.len();
        self.in_flight.shutdown().await;
        if abandoned > 0 {
            tracing::info!(
                abandoned,
                "stopped with alert sends in flight; they stay queued"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_its_cap() {
        let backoff = WorkerSettings::default().backoff;
        let delays: Vec<u64> = (1..=9).map(|n| backoff.delay(n).as_secs()).collect();
        assert_eq!(delays, [5, 10, 20, 40, 80, 160, 320, 600, 600]);
        assert_eq!(backoff.delay(u32::MAX).as_secs(), 600);
    }

    #[test]
    fn full_jitter_spreads_waits_over_the_whole_delay() {
        let backoff = WorkerSettings::default().backoff;
        assert_eq!(backoff.jitter, Jitter::Full);
        let waits: Vec<Duration> = (0..200).map(|_| backoff.wait(4)).collect();
        let delay = backoff.delay(4);
        assert!(waits.iter().all(|wait| *wait <= delay));
        assert!(waits.iter().any(|wait| *wait < delay / 2));
        assert!(waits.iter().any(|wait| *wait > delay / 2));
        let exact = Backoff {
            jitter: Jitter::None,
            ..backoff
        };
        assert_eq!(exact.wait(4), delay);
    }
}
