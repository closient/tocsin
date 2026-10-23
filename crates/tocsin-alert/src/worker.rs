// SPDX-License-Identifier: AGPL-3.0-only

//! The delivery worker: drains the outbox to the notifiers.
//!
//! Everything it knows lives in the store, so a restart loses nothing: a
//! delivery pending at shutdown is sent after the next start, and repeats
//! come from the open alerts' `next_repeat_at`, not from timers. Each alert
//! has at most one message in flight, and the outbox only offers an alert's
//! oldest pending message, so a resolve never overtakes its fire. Each
//! notifier sends on its own task, so a slow or failing one holds up only
//! its own alerts.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tocsin_engine::ports::Clock;
use tocsin_store::{DeliveryKind, DeliveryOutcome, DeliveryRetention, PendingDelivery, Store};
use tokio::sync::{Notify, watch};
use tokio::task::{JoinHandle, JoinSet};

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
}

impl Backoff {
    /// The wait after `attempts` failed attempts (at least one).
    #[must_use]
    pub fn delay(self, attempts: u32) -> Duration {
        let doublings = attempts.saturating_sub(1).min(20);
        self.base
            .checked_mul(1 << doublings)
            .map_or(self.cap, |delay| delay.min(self.cap))
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
    /// Retries at 5s, 10s, 20s ... capped at 10m, twelve attempts (about an
    /// hour); delivered rows kept a week and failed ones 90 days.
    fn default() -> Self {
        const DAY_MS: i64 = 86_400_000;
        WorkerSettings {
            backoff: Backoff {
                base: Duration::from_secs(5),
                cap: Duration::from_secs(600),
            },
            max_attempts: 12,
            send_timeout: Duration::from_secs(30),
            idle: Duration::from_secs(60),
            store_retry: Duration::from_secs(5),
            prune_every: Duration::from_secs(3600),
            retention: DeliveryRetention {
                delivered_ms: 7 * DAY_MS,
                failed_ms: 90 * DAY_MS,
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

/// What a finished send reports back.
struct Sent {
    delivery_id: i64,
    alert_id: i64,
    notifier_id: String,
    attempts: u32,
    result: Result<(), SendError>,
}

struct Worker {
    store: Arc<Store>,
    notifiers: Notifiers,
    router: Arc<Router>,
    clock: Arc<dyn Clock>,
    metrics: AlertMetrics,
    settings: WorkerSettings,
    wake: Arc<Notify>,
    in_flight: JoinSet<Sent>,
    busy: HashSet<i64>,
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
            busy: HashSet::new(),
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
                Some(joined) = self.in_flight.join_next(), if !self.in_flight.is_empty() => {
                    self.finish(joined).await;
                }
                () = tokio::time::sleep(pause) => {}
            }
        }
        self.shut_down().await;
    }

    /// Queues due repeats and starts every due message whose alert has
    /// nothing in flight; returns when to look again.
    async fn dispatch(&mut self, now: i64) -> tocsin_store::Result<i64> {
        self.store.queue_repeats(now).await?;
        let outbox = self.store.outbox().await?;
        let mut wake_at = now.saturating_add(millis(self.settings.idle));
        if let Some(repeat_at) = outbox.next_repeat_at_ms {
            wake_at = wake_at.min(repeat_at);
        }
        for delivery in outbox.pending {
            if self.busy.contains(&delivery.alert.id) {
                continue;
            }
            if delivery.next_attempt_at_ms > now {
                wake_at = wake_at.min(delivery.next_attempt_at_ms);
                continue;
            }
            self.busy.insert(delivery.alert.id);
            self.spawn(&delivery, now);
        }
        Ok(wake_at.max(now))
    }

    fn spawn(&mut self, delivery: &PendingDelivery, now: i64) {
        let alert = self.message(delivery, now);
        let (delivery_id, alert_id, attempts) = (delivery.id, delivery.alert.id, delivery.attempts);
        let notifier = self.notifiers.get(&delivery.alert.notifier_id).cloned();
        let notifier_id = delivery.alert.notifier_id.clone();
        let send_timeout = self.settings.send_timeout;
        self.in_flight.spawn(async move {
            let result = match notifier {
                None => Err(SendError::permanent(format!(
                    "notifier `{notifier_id}` is no longer configured"
                ))),
                Some(notifier) => tokio::time::timeout(send_timeout, notifier.send(&alert))
                    .await
                    .unwrap_or_else(|_| {
                        Err(SendError::transient(format!(
                            "no answer within {}s",
                            send_timeout.as_secs()
                        )))
                    }),
            };
            Sent {
                delivery_id,
                alert_id,
                notifier_id,
                attempts,
                result,
            }
        });
    }

    /// The message a pending delivery stands for.
    fn message(&self, delivery: &PendingDelivery, now: i64) -> Alert {
        let stored = &delivery.alert;
        Alert {
            event: match delivery.kind {
                DeliveryKind::Fire => AlertEvent::Fire,
                DeliveryKind::Repeat => AlertEvent::Repeat,
                DeliveryKind::Resolve => AlertEvent::Resolve,
            },
            node_id: stored.node_id.clone(),
            subject_kind: stored.subject_kind,
            subject_id: stored.subject_id.clone(),
            subject_name: self.router.subject_name(&stored.subject_id),
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

    /// Records how a send went.
    async fn finish(&mut self, joined: Result<Sent, tokio::task::JoinError>) {
        let sent = match joined {
            Ok(sent) => sent,
            Err(error) => {
                // The row stays pending; its alert is not marked free, so it
                // waits for a restart rather than spinning on a panic.
                tracing::error!(%error, "an alert send panicked");
                return;
            }
        };
        self.busy.remove(&sent.alert_id);
        let now = self.clock.now_ms();
        let attempts = sent.attempts.saturating_add(1);
        let outcome = match sent.result {
            Ok(()) => DeliveryOutcome::Delivered,
            Err(error) => {
                self.metrics.attempt_failed(&sent.notifier_id);
                if error.permanent || attempts >= self.settings.max_attempts {
                    self.metrics.abandoned(&sent.notifier_id);
                    tracing::warn!(
                        notifier = %sent.notifier_id,
                        delivery = sent.delivery_id,
                        attempts,
                        error = %error.message,
                        "alert delivery failed for good"
                    );
                    DeliveryOutcome::Failed {
                        error: error.message,
                    }
                } else {
                    let at_ms = now.saturating_add(millis(self.settings.backoff.delay(attempts)));
                    tracing::info!(
                        notifier = %sent.notifier_id,
                        delivery = sent.delivery_id,
                        attempts,
                        error = %error.message,
                        "alert delivery failed; will retry"
                    );
                    DeliveryOutcome::Retry {
                        at_ms,
                        error: error.message,
                    }
                }
            }
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

    /// Lets sends in flight finish within the grace, then abandons the
    /// rest; their rows stay pending.
    async fn shut_down(mut self) {
        let grace = tokio::time::sleep(self.settings.stop_grace);
        tokio::pin!(grace);
        while !self.in_flight.is_empty() {
            tokio::select! {
                () = &mut grace => break,
                Some(joined) = self.in_flight.join_next() => self.finish(joined).await,
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
}
