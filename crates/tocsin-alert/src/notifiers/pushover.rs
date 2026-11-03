// SPDX-License-Identifier: AGPL-3.0-only

//! Pushover, with each health mapped to a priority.
//!
//! A firing message at the emergency priority rings until acknowledged. It
//! carries a tag, and the delivery worker cancels it by that tag once the
//! alert resolves or re-fires at a priority that does not ring: a separate
//! delivery, so a failed cancel never holds up the resolve.
//!
//! Optional quiet hours hold every message to a lower priority inside a
//! daily local-time window, so the phone stays silent overnight while the
//! messages still arrive. Inside the window nothing goes out at the
//! emergency priority, and an emergency already ringing for the alert is
//! cancelled when the alert next fires or repeats there.

use sha2::{Digest, Sha256};
use tocsin_core::Health;
use tocsin_core::config::{self, PushoverSettings, TimeOfDay};
use tocsin_engine::ports::BoxFuture;

use crate::alert::Alert;
use crate::notifier::{Notifier, Redactor, SendError};

/// Pushover's API base URL.
pub const API_BASE: &str = "https://api.pushover.net";

/// The emergency priority, which repeats on the phone until acknowledged.
const EMERGENCY: i8 = 2;
/// How often an emergency notification repeats (seconds; the minimum is 30).
const EMERGENCY_RETRY_SECS: u32 = 60;
/// How long an emergency notification keeps repeating (seconds).
const EMERGENCY_EXPIRE_SECS: u32 = 3600;
/// The shortest expiry sent, when quiet hours are moments away: Pushover's
/// own floor for a retry, and under it, so the message rings only the once.
const MIN_EXPIRE_SECS: i64 = 30;
/// Pushover's limits on a title and a message, in characters.
const MAX_TITLE: usize = 250;
const MAX_MESSAGE: usize = 1024;

/// The priority for each kind of message, -2 (silent) to 2 (emergency).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Priorities {
    /// A down alert, and its repeats.
    pub down: i8,
    /// A degraded alert, and its repeats.
    pub degraded: i8,
    /// A resolve.
    pub resolved: i8,
}

impl Default for Priorities {
    fn default() -> Self {
        Priorities {
            down: 1,
            degraded: 0,
            resolved: 0,
        }
    }
}

impl From<&PushoverSettings> for Priorities {
    fn from(settings: &PushoverSettings) -> Self {
        Priorities {
            down: settings.down_priority,
            degraded: settings.degraded_priority,
            resolved: settings.resolved_priority,
        }
    }
}

impl Priorities {
    fn firing(self, health: Health) -> i8 {
        if health == Health::Down {
            self.down
        } else {
            self.degraded
        }
    }

    /// Whether a firing message at `health` rings until acknowledged.
    #[must_use]
    pub fn rings(self, health: Health) -> bool {
        self.firing(health) == EMERGENCY
    }
}

/// A daily window, in local time, inside which no message goes out above
/// a lower priority. `[start, end)`, past midnight when `start` is later
/// than `end`, read on the wall clock of a time zone, so it follows
/// daylight saving time.
#[derive(Debug, Clone)]
pub struct QuietHours {
    start: TimeOfDay,
    end: TimeOfDay,
    zone: jiff::tz::TimeZone,
    priority: i8,
}

impl QuietHours {
    /// Quiet from `start` until `end` in `zone`, at no more than `priority`.
    #[must_use]
    pub fn new(start: TimeOfDay, end: TimeOfDay, zone: jiff::tz::TimeZone, priority: i8) -> Self {
        QuietHours {
            start,
            end,
            zone,
            priority,
        }
    }

    /// The window `[notifier.pushover.quiet_hours]` describes.
    ///
    /// # Errors
    ///
    /// The time zone is not in the bundled database.
    pub fn from_settings(settings: &config::QuietHours) -> Result<Self, String> {
        let zone = jiff::tz::TimeZone::get(&settings.timezone)
            .map_err(|error| format!("quiet hours time zone `{}`: {error}", settings.timezone))?;
        Ok(QuietHours::new(
            settings.start,
            settings.end,
            zone,
            settings.priority,
        ))
    }

    /// Whether the instant `at_ms` (unix milliseconds) falls inside the
    /// window, by the local wall clock at that instant.
    #[must_use]
    pub fn contains(&self, at_ms: i64) -> bool {
        let Ok(instant) = jiff::Timestamp::from_millisecond(at_ms) else {
            return false;
        };
        let local = instant.to_zoned(self.zone.clone());
        let minute =
            u16::from(local.hour().unsigned_abs()) * 60 + u16::from(local.minute().unsigned_abs());
        let (start, end) = (self.start.minutes(), self.end.minutes());
        if start < end {
            start <= minute && minute < end
        } else {
            minute >= start || minute < end
        }
    }

    /// Whole seconds from `at_ms` until the window next opens, or `None`
    /// when that cannot be worked out (an instant out of range).
    #[must_use]
    pub fn seconds_until_open(&self, at_ms: i64) -> Option<i64> {
        let now = jiff::Timestamp::from_millisecond(at_ms)
            .ok()?
            .to_zoned(self.zone.clone());
        let minutes = self.start.minutes();
        let opening = |day: &jiff::Zoned| {
            day.with()
                .hour(i8::try_from(minutes / 60).ok()?)
                .minute(i8::try_from(minutes % 60).ok()?)
                .second(0)
                .subsec_nanosecond(0)
                .build()
                .ok()
        };
        let mut next = opening(&now)?;
        if next <= now {
            next = opening(&now.tomorrow().ok()?)?;
        }
        Some(now.timestamp().duration_until(next.timestamp()).as_secs())
    }

    /// `priority`, held down to the window's at `at_ms`.
    #[must_use]
    pub fn hold(&self, priority: i8, at_ms: i64) -> i8 {
        if self.contains(at_ms) {
            priority.min(self.priority)
        } else {
            priority
        }
    }
}

/// Sends Pushover notifications.
#[derive(Debug)]
pub struct Pushover {
    client: reqwest::Client,
    api_base: String,
    api_token: String,
    user_key: String,
    priorities: Priorities,
    quiet_hours: Option<QuietHours>,
    redactor: Redactor,
}

impl Pushover {
    /// A notifier sending to `user_key` as the application with `api_token`,
    /// both secret, through the API at `api_base`.
    #[must_use]
    pub fn new(
        client: reqwest::Client,
        api_base: &str,
        api_token: String,
        user_key: String,
        priorities: Priorities,
    ) -> Self {
        let redactor = Redactor::new([api_token.as_str(), user_key.as_str()]);
        Pushover {
            client,
            api_base: api_base.trim_end_matches('/').to_owned(),
            api_token,
            user_key,
            priorities,
            quiet_hours: None,
            redactor,
        }
    }

    /// The same notifier, holding every message inside `quiet_hours` to
    /// the window's priority.
    #[must_use]
    pub fn with_quiet_hours(mut self, quiet_hours: QuietHours) -> Self {
        self.quiet_hours = Some(quiet_hours);
        self
    }

    /// The tag an emergency notification carries, so it can be cancelled:
    /// `tocsin-` and the SHA-256, in hex, of the user key and the alert's
    /// identity. Hashed, it holds only URL-safe characters whatever the node
    /// and subject ids hold (an FQDN node's dots included), and two
    /// notifiers sharing an application never cancel each other's.
    #[must_use]
    pub fn tag(&self, alert: &Alert) -> String {
        let mut hash = Sha256::new();
        hash.update(self.user_key.as_bytes());
        hash.update([0]);
        hash.update(alert.identity().as_bytes());
        format!("tocsin-{}", hex::encode(hash.finalize()))
    }

    /// How long an emergency sent at `at_ms` repeats: an hour, cut short so
    /// that it stops before quiet hours open. Pushover rings at once and
    /// then every [`EMERGENCY_RETRY_SECS`], so an expiry under that rings
    /// only the once.
    fn expire(&self, at_ms: i64) -> i64 {
        let hour = i64::from(EMERGENCY_EXPIRE_SECS);
        self.quiet_hours
            .as_ref()
            .and_then(|quiet| quiet.seconds_until_open(at_ms))
            .map_or(hour, |until| until.clamp(MIN_EXPIRE_SECS, hour))
    }

    /// Stops the alert's emergency notifications from ringing.
    async fn cancel_by_tag(&self, alert: &Alert) -> Result<(), SendError> {
        let mut url = url::Url::parse(&self.api_base)
            .map_err(|error| SendError::permanent(format!("the Pushover API base: {error}")))?;
        url.path_segments_mut()
            .map_err(|()| SendError::permanent("the Pushover API base cannot take a path"))?
            .pop_if_empty()
            .extend(["1", "receipts", "cancel_by_tag"])
            .push(&format!("{}.json", self.tag(alert)));
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", &self.api_token)
            .finish();
        let request = self
            .client
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form);
        self.redactor.send(request).await.map(drop)
    }
}

impl Notifier for Pushover {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let usual = if alert.is_resolve() {
                self.priorities.resolved
            } else {
                self.priorities.firing(alert.health)
            };
            // The window is judged at the moment of this attempt, the same
            // instant the message carries as its timestamp.
            let priority = self
                .quiet_hours
                .as_ref()
                .map_or(usual, |quiet| quiet.hold(usual, alert.sent_at_ms));
            if usual == EMERGENCY && priority != EMERGENCY && !alert.is_resolve() {
                // Held below the emergency by quiet hours: an emergency that
                // began before the window opened must not keep ringing into
                // it. Cancelled first, so a failed cancel is retried before
                // the quiet message goes out, never after it.
                self.cancel_by_tag(alert).await?;
            }
            let title: String = alert.title().chars().take(MAX_TITLE).collect();
            let message: String = alert.body().chars().take(MAX_MESSAGE).collect();
            // Built in a block: the serializer is not `Send`.
            let form = {
                let mut form = url::form_urlencoded::Serializer::new(String::new());
                form.append_pair("token", &self.api_token)
                    .append_pair("user", &self.user_key)
                    .append_pair("title", &title)
                    .append_pair("message", &message)
                    .append_pair("priority", &priority.to_string())
                    .append_pair("timestamp", &(alert.sent_at_ms / 1000).to_string());
                if priority == EMERGENCY {
                    form.append_pair("retry", &EMERGENCY_RETRY_SECS.to_string())
                        .append_pair("expire", &self.expire(alert.sent_at_ms).to_string());
                    // Only a firing message is cancelled. A resolve at the
                    // emergency priority, if configured, rings until
                    // acknowledged or it expires.
                    if !alert.is_resolve() {
                        form.append_pair("tags", &self.tag(alert));
                    }
                }
                form.finish()
            };
            let request = self
                .client
                .post(format!("{}/1/messages.json", self.api_base))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(form);
            self.redactor.send(request).await.map(drop)
        })
    }

    fn cancel<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(self.cancel_by_tag(alert))
    }
}
