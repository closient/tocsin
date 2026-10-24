// SPDX-License-Identifier: AGPL-3.0-only

//! Operator alerting.
//!
//! The engine decides whether a change alerts and whether a recovery
//! resolves; this crate only routes and delivers. [`AlertingHistory`]
//! writes each alert's outbox rows in the same store transaction as the
//! outage change, and [`Alerter`] drains the outbox to the [`Notifier`]s,
//! with retries, repeats and a resolve that never overtakes its fire. See
//! `docs/alerting.md`.

pub mod alert;
pub mod history;
pub mod mail;
pub mod metrics;
pub mod notifier;
pub mod notifiers;
pub mod route;
pub mod worker;

pub use alert::{Alert, AlertEvent};
pub use history::AlertingHistory;
pub use mail::{Mailer, Outgoing};
pub use metrics::AlertMetrics;
pub use notifier::{HTTP_TIMEOUT, Notifier, Redactor, SendError};
pub use notifiers::slack::SlackHooks;
pub use notifiers::{Notifiers, build, build_all};
pub use route::Router;
pub use worker::{Alerter, Backoff, Jitter, WorkerSettings};
