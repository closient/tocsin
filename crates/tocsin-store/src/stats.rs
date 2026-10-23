// SPDX-License-Identifier: AGPL-3.0-only

//! What the writer and the checkpointer have done to keep the write-ahead
//! log bounded, counted as they go, so a slow write can be traced to the
//! path that made it slow (TOC-62). The counters are relaxed atomics, a few
//! per write and per pass; the rare slow paths also keep an event each, up
//! to [`EVENTS`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Events kept; later ones are counted but dropped.
const EVENTS: usize = 1024;

/// A pass or a write that takes longer than this is kept as an event.
pub(crate) const SLOW: Duration = Duration::from_millis(10);

/// What the checkpoint paths have done since the store opened. See
/// [`crate::Store::checkpoint_stats`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckpointStats {
    /// Writes that left the log at its hard limit, so the writer
    /// checkpointed it itself.
    pub limit_writes: u64,
    /// Of those, the ones that found another checkpoint holding the lock and
    /// waited for it.
    pub limit_waits: u64,
    /// The longest such wait.
    pub limit_wait_max: Duration,
    /// Checkpoints the writer ran at the limit that got the lock.
    pub inline_copies: u64,
    /// The longest of them.
    pub inline_copy_max: Duration,
    /// The most frames the log held when one of them ran.
    pub inline_frames_max: u64,
    /// Lockouts started: a wait at the limit timed out, or a checkpoint
    /// there failed.
    pub lockouts: u64,
    /// Passes the checkpointer skipped to stand aside for the writer.
    pub stand_asides: u64,
    /// Retention's writes that waited for the checkpointer to copy a log
    /// past the soft limit first.
    pub paced_writes: u64,
    /// The longest such wait.
    pub paced_wait_max: Duration,
    /// Passes the checkpointer ran.
    pub passes: u64,
    /// The longest of them.
    pub pass_max: Duration,
    /// The 99th percentile of their durations, to within a quarter.
    pub pass_p99: Duration,
    /// Passes that ran only to chase the commits past the soft limit.
    pub chase_passes: u64,
    /// The most chase passes in a row.
    pub chase_streak_max: u64,
    /// The most frames the log held after any commit.
    pub frames_high: u64,
    /// Times a commit found the whole log copied and started it again from
    /// the beginning.
    pub rewinds: u64,
    /// The longest write on the writer thread, its commit included and the
    /// checkpoint after it not.
    pub write_max: Duration,
    /// The slow paths, oldest first: up to 1024, then only counted.
    pub events: Vec<CheckpointEvent>,
    /// Events past the first 1024, not kept.
    pub events_dropped: u64,
}

/// One slow path, timed on the thread that took it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckpointEvent {
    /// A write left the log at its hard limit and the writer checkpointed it.
    Limit {
        /// When the write's checkpoint started.
        at: Instant,
        /// Frames in the log then.
        frames: u64,
        /// Time spent waiting for another checkpoint to let go of the lock.
        waited: Duration,
        /// Time the checkpoint that got the lock, or the last try, took.
        copy: Duration,
        /// Frames of the log in the database file when it finished, or
        /// `None` if it was busy or failed.
        copied: Option<u64>,
    },
    /// A pass of the checkpointer's took longer than 10 ms.
    Pass {
        /// When it started.
        at: Instant,
        /// How long it took.
        took: Duration,
        /// It ran only to chase the commits past the soft limit.
        chase: bool,
        /// It copied every frame it found.
        complete: bool,
    },
    /// Retention waited longer than 10 ms for the checkpointer before one
    /// of its writes.
    Paced {
        /// When it started waiting.
        at: Instant,
        /// How long it waited.
        waited: Duration,
    },
    /// A write on the writer thread took longer than 10 ms, its checkpoint
    /// at the limit, if any, not included.
    Write {
        /// When it started.
        at: Instant,
        /// How long it took.
        took: Duration,
        /// Frames in the log after it.
        frames: u64,
    },
}

/// Power-of-two buckets of microseconds, each split in four.
const BUCKETS: usize = 128;

/// The live counters behind [`CheckpointStats`], shared by the writer and
/// the checkpointer.
pub(crate) struct Stats {
    limit_writes: AtomicU64,
    limit_waits: AtomicU64,
    limit_wait_max: AtomicU64,
    inline_copies: AtomicU64,
    inline_copy_max: AtomicU64,
    inline_frames_max: AtomicU64,
    lockouts: AtomicU64,
    stand_asides: AtomicU64,
    paced_writes: AtomicU64,
    paced_wait_max: AtomicU64,
    passes: AtomicU64,
    pass_max: AtomicU64,
    pass_buckets: [AtomicU64; BUCKETS],
    chase_passes: AtomicU64,
    chase_streak_max: AtomicU64,
    frames_high: AtomicU64,
    rewinds: AtomicU64,
    write_max: AtomicU64,
    events: Mutex<(VecDeque<CheckpointEvent>, u64)>,
}

impl Default for Stats {
    fn default() -> Stats {
        Stats {
            limit_writes: AtomicU64::new(0),
            limit_waits: AtomicU64::new(0),
            limit_wait_max: AtomicU64::new(0),
            inline_copies: AtomicU64::new(0),
            inline_copy_max: AtomicU64::new(0),
            inline_frames_max: AtomicU64::new(0),
            lockouts: AtomicU64::new(0),
            stand_asides: AtomicU64::new(0),
            paced_writes: AtomicU64::new(0),
            paced_wait_max: AtomicU64::new(0),
            passes: AtomicU64::new(0),
            pass_max: AtomicU64::new(0),
            pass_buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            chase_passes: AtomicU64::new(0),
            chase_streak_max: AtomicU64::new(0),
            frames_high: AtomicU64::new(0),
            rewinds: AtomicU64::new(0),
            write_max: AtomicU64::new(0),
            events: Mutex::new((VecDeque::new(), 0)),
        }
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// The bucket of `us` microseconds: exact below 4, then four to each power
/// of two.
fn bucket(us: u64) -> usize {
    if us < 4 {
        return usize::try_from(us).unwrap_or(0);
    }
    let power = u64::from(us.ilog2());
    let quarter = (us >> (power - 2)) & 3;
    usize::try_from((power - 1) * 4 + quarter).map_or(BUCKETS - 1, |b| b.min(BUCKETS - 1))
}

/// The largest value in `bucket`.
fn bucket_top(bucket: usize) -> u64 {
    let bucket = bucket as u64;
    if bucket < 4 {
        return bucket;
    }
    let (power, quarter) = (bucket / 4 + 1, bucket % 4);
    ((4 + quarter + 1) << (power - 2)) - 1
}

impl Stats {
    /// A write that left `frames` in the log, `rewound` it first, and took
    /// `took` from `at`.
    pub(crate) fn write(&self, at: Instant, took: Duration, frames: u64, rewound: bool) {
        self.frames_high.fetch_max(frames, Ordering::Relaxed);
        if rewound {
            bump(&self.rewinds);
        }
        self.write_max.fetch_max(micros(took), Ordering::Relaxed);
        if took > SLOW {
            self.event(CheckpointEvent::Write { at, took, frames });
        }
    }

    /// The writer's checkpoint at the hard limit.
    pub(crate) fn limit(&self, event: CheckpointEvent) {
        let CheckpointEvent::Limit {
            frames,
            waited,
            copy,
            copied,
            ..
        } = event
        else {
            return;
        };
        bump(&self.limit_writes);
        if !waited.is_zero() {
            bump(&self.limit_waits);
            self.limit_wait_max
                .fetch_max(micros(waited), Ordering::Relaxed);
        }
        if copied.is_some() {
            bump(&self.inline_copies);
            self.inline_copy_max
                .fetch_max(micros(copy), Ordering::Relaxed);
            self.inline_frames_max.fetch_max(frames, Ordering::Relaxed);
        }
        self.event(event);
    }

    pub(crate) fn lockout(&self) {
        bump(&self.lockouts);
    }

    pub(crate) fn stand_aside(&self) {
        bump(&self.stand_asides);
    }

    /// Retention waited `waited` from `at` for the checkpointer.
    pub(crate) fn paced(&self, at: Instant, waited: Duration) {
        bump(&self.paced_writes);
        self.paced_wait_max
            .fetch_max(micros(waited), Ordering::Relaxed);
        if waited > SLOW {
            self.event(CheckpointEvent::Paced { at, waited });
        }
    }

    /// A pass of the checkpointer's; `streak` is the chase passes in a row
    /// including this one.
    pub(crate) fn pass(&self, event: CheckpointEvent, streak: u64) {
        let CheckpointEvent::Pass { took, chase, .. } = event else {
            return;
        };
        bump(&self.passes);
        let us = micros(took);
        self.pass_max.fetch_max(us, Ordering::Relaxed);
        bump(&self.pass_buckets[bucket(us)]);
        if chase {
            bump(&self.chase_passes);
            self.chase_streak_max.fetch_max(streak, Ordering::Relaxed);
        }
        if took > SLOW {
            self.event(event);
        }
    }

    fn event(&self, event: CheckpointEvent) {
        let mut events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        if events.0.len() < EVENTS {
            events.0.push_back(event);
        } else {
            events.1 += 1;
        }
    }

    pub(crate) fn snapshot(&self) -> CheckpointStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let duration = |counter: &AtomicU64| Duration::from_micros(load(counter));
        let buckets: Vec<u64> = self.pass_buckets.iter().map(load).collect();
        let passes: u64 = buckets.iter().sum();
        let pass_max = duration(&self.pass_max);
        // The bucket holding the pass at the 99th percentile, by rank.
        let rank = passes - passes / 100;
        let mut seen = 0;
        let pass_p99 = buckets
            .iter()
            .position(|count| {
                seen += count;
                passes > 0 && seen >= rank
            })
            .map_or(Duration::ZERO, |bucket| {
                Duration::from_micros(bucket_top(bucket)).min(pass_max)
            });
        let (events, events_dropped) = {
            let events = self.events.lock().unwrap_or_else(PoisonError::into_inner);
            (events.0.iter().copied().collect(), events.1)
        };
        CheckpointStats {
            limit_writes: load(&self.limit_writes),
            limit_waits: load(&self.limit_waits),
            limit_wait_max: duration(&self.limit_wait_max),
            inline_copies: load(&self.inline_copies),
            inline_copy_max: duration(&self.inline_copy_max),
            inline_frames_max: load(&self.inline_frames_max),
            lockouts: load(&self.lockouts),
            stand_asides: load(&self.stand_asides),
            paced_writes: load(&self.paced_writes),
            paced_wait_max: duration(&self.paced_wait_max),
            passes: load(&self.passes),
            pass_max,
            pass_p99,
            chase_passes: load(&self.chase_passes),
            chase_streak_max: load(&self.chase_streak_max),
            frames_high: load(&self.frames_high),
            rewinds: load(&self.rewinds),
            write_max: duration(&self.write_max),
            events,
            events_dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_contiguous_and_within_a_quarter() {
        let mut last = 0;
        for us in 0..100_000 {
            let b = bucket(us);
            assert!(b == last || b == last + 1, "{us} skipped a bucket");
            assert!(bucket_top(b) >= us, "{us} is past its bucket's top");
            assert!(bucket_top(b) <= us + us / 4, "{us}'s bucket is too wide");
            last = b;
        }
    }

    #[test]
    fn the_p99_is_the_pass_at_that_rank() {
        let stats = Stats::default();
        let pass = |ms| CheckpointEvent::Pass {
            at: Instant::now(),
            took: Duration::from_millis(ms),
            chase: false,
            complete: true,
        };
        for _ in 0..99 {
            stats.pass(pass(1), 0);
        }
        stats.pass(pass(400), 0);
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.passes, 100);
        assert_eq!(snapshot.pass_max, Duration::from_millis(400));
        assert!(snapshot.pass_p99 >= Duration::from_millis(1));
        assert!(snapshot.pass_p99 < Duration::from_millis(2));
        stats.pass(pass(400), 0);
        assert_eq!(stats.snapshot().pass_p99, Duration::from_millis(400));
    }
}
