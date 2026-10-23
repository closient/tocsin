// SPDX-License-Identifier: AGPL-3.0-only

//! A configuration loader, a store in a temporary folder, and notifiers that
//! record what they are handed. The fakes answer synchronously, so a paused
//! tokio clock only moves when every task waits on a timer or on the store.

#![allow(dead_code, reason = "each test file uses a different subset")]

use std::fmt::Write as _;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tocsin_alert::{Alert, AlertEvent, Notifier, SendError};
use tocsin_core::Health;
use tocsin_core::config::{Config, Environment, Source, load};
use tocsin_engine::ports::BoxFuture;
use tocsin_store::{NewOutage, OutageChange, OutageEnd, Store};
use tokio::sync::mpsc;

/// Wall-clock time when a test starts.
pub const T0: i64 = 1_800_000_000_000;

/// How long, in real time, a test on a paused clock may run.
pub const WATCHDOG: Duration = Duration::from_secs(30);

/// Runs `test` on a paused clock, and fails if it has not finished within
/// [`WATCHDOG`] of real time.
///
/// A paused test cannot bound itself with a tokio timeout: while the
/// store's writer thread works the runtime looks idle, and the clock jumps
/// straight to whatever timer is armed. So the runtime runs on a thread of
/// its own and the test thread waits on it by the real clock. A test that
/// would wait forever for a send that never comes fails instead, and the
/// stuck thread goes when the test binary exits.
pub fn paused(test: impl Future<Output = ()> + Send + 'static) {
    let name = std::thread::current()
        .name()
        .unwrap_or("this test")
        .to_owned();
    let (done, finished) = std::sync::mpsc::channel();
    let runner = std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap()
                .block_on(test);
            let _ = done.send(());
        })
        .unwrap();
    match finished.recv_timeout(WATCHDOG) {
        // Finished, or panicked and dropped `done`: either way it has ended.
        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(panic) = runner.join() {
                std::panic::resume_unwind(panic);
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("{name} was still running after {WATCHDOG:?} of real time");
        }
    }
}

/// Loads `toml` as the only configuration file, on node `node-a`, with
/// every `${ENV:NAME}` secret set to `secret-NAME`.
pub fn config(toml: &str) -> Config {
    let sources = [Source {
        name: "test.toml".to_owned(),
        text: Ok(toml.to_owned()),
    }];
    let environment = Environment {
        hostname: "node-a".to_owned(),
        variable: &|name| Some(format!("secret-{name}")),
    };
    load(&sources, &environment).unwrap_or_else(|errors| panic!("bad config: {errors:?}"))
}

/// A Slack notifier `id` in configuration; tests swap in a fake by id.
pub fn notifier(id: &str) -> String {
    format!(
        "[[notifier]]\nid = \"{id}\"\ntype = \"slack\"\n\n[notifier.slack]\n\
         webhook_url = \"${{ENV:SLACK_{}}}\"\n\n",
        id.to_uppercase()
    )
}

/// The `api` check on the `web` component, every second, and a route
/// sending everything to `notifiers` with `extra` route settings.
pub fn monitored(notifiers: &[&str], extra: &str) -> String {
    let mut toml = String::from(
        "[[component]]\nid = \"web\"\nname = \"Website\"\n\n\
         [[check]]\nid = \"api\"\nname = \"Public API\"\ntype = \"tcp\"\n\
         target = \"api.example.com:443\"\ninterval = \"1s\"\ntimeout = \"500ms\"\n\
         confirm = 1\ncomponent = \"web\"\n\n",
    );
    for id in notifiers {
        toml.push_str(&notifier(id));
    }
    let list: Vec<String> = notifiers.iter().map(|id| format!("\"{id}\"")).collect();
    let _ = writeln!(
        toml,
        "[[route]]\nid = \"ops\"\nnotifiers = [{}]\n{extra}",
        list.join(", ")
    );
    toml
}

/// A store in a temporary folder that lives as long as it does.
pub struct Fixture {
    pub store: Arc<Store>,
    pub dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("tocsin.db")).unwrap());
        Fixture { store, dir }
    }

    /// Closes the store, as `serve` does on the way out, and opens the
    /// same file again. Nothing else may hold the store.
    pub async fn restart(self) -> Self {
        let Fixture { store, dir } = self;
        let Ok(store) = Arc::try_unwrap(store) else {
            panic!("something else still holds the store");
        };
        store.close().await.unwrap();
        let store = Arc::new(Store::open(&dir.path().join("tocsin.db")).unwrap());
        Fixture { store, dir }
    }
}

/// An alerting outage of `api` on `node-a` opening at `at`.
pub fn down(at: i64) -> OutageChange {
    OutageChange {
        end: None,
        open: Some(NewOutage {
            node_id: "node-a".into(),
            check_id: "api".into(),
            component_id: Some("web".into()),
            health: Health::Down,
            started_at_ms: at,
            alerted: true,
        }),
    }
}

/// The recovery ending outage `id` at `at`.
pub fn up(id: i64, at: i64) -> OutageChange {
    OutageChange {
        end: Some(OutageEnd {
            id,
            ended_at_ms: at,
        }),
        open: None,
    }
}

/// What a fake notifier does with an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Accept it.
    Accept,
    /// Fail; worth retrying.
    Fail,
    /// Fail for good.
    Refuse,
    /// Never answer.
    Hang,
}

/// One attempt a fake notifier saw.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub notifier: String,
    pub alert: Alert,
    pub answer: Answer,
}

impl Attempt {
    pub fn event(&self) -> AlertEvent {
        self.alert.event
    }
}

type Script = dyn Fn(&Alert, usize) -> Answer + Send + Sync;

/// A notifier answering from a script given the alert and how many
/// attempts it has seen before; it reports every attempt on a channel.
pub struct Fake {
    id: String,
    script: Box<Script>,
    attempts: AtomicUsize,
    seen: Mutex<Vec<Attempt>>,
    sender: mpsc::UnboundedSender<Attempt>,
}

impl Fake {
    pub fn new(
        id: &str,
        sender: &mpsc::UnboundedSender<Attempt>,
        script: impl Fn(&Alert, usize) -> Answer + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Fake {
            id: id.to_owned(),
            script: Box::new(script),
            attempts: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            sender: sender.clone(),
        })
    }

    /// A notifier that accepts everything.
    pub fn accepting(id: &str, sender: &mpsc::UnboundedSender<Attempt>) -> Arc<Self> {
        Fake::new(id, sender, |_, _| Answer::Accept)
    }

    /// Every attempt so far.
    pub fn seen(&self) -> Vec<Attempt> {
        self.seen.lock().unwrap().clone()
    }
}

impl Notifier for Fake {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let index = self.attempts.fetch_add(1, Ordering::SeqCst);
            let answer = (self.script)(alert, index);
            let attempt = Attempt {
                notifier: self.id.clone(),
                alert: alert.clone(),
                answer,
            };
            self.seen.lock().unwrap().push(attempt.clone());
            let _ = self.sender.send(attempt);
            match answer {
                Answer::Accept => Ok(()),
                Answer::Fail => Err(SendError::transient("HTTP 503: try later")),
                Answer::Refuse => Err(SendError::permanent("HTTP 400: bad request")),
                Answer::Hang => std::future::pending().await,
            }
        })
    }
}

/// Receives attempts until one from `notifier` arrives, and returns it.
pub async fn next_from(receiver: &mut mpsc::UnboundedReceiver<Attempt>, notifier: &str) -> Attempt {
    loop {
        let attempt = receiver.recv().await.expect("the fakes are alive");
        if attempt.notifier == notifier {
            return attempt;
        }
    }
}
