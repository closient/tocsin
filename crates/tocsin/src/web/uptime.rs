// SPDX-License-Identifier: AGPL-3.0-only

//! The status page's uptime bars: one cell per UTC day for the last
//! [`DAYS`] days, read from the daily rollups the server recomputes every
//! hour, so today's cell trails the live status by up to an hour. Today is
//! marked as such and says it covers the day so far; until its first
//! rollup it says the first figure is coming within the hour.
//!
//! A day's uptime for one check is the share of its results (or, for a
//! heartbeat, its milliseconds) that were not down, every node summed; a
//! degraded result counts as up. A component's day is the mean over its
//! checks that have data that day, and its tone is the worst health any
//! of them saw. Percentages are floored, so a day with one failure never
//! reads 100%. The arithmetic is in parts per million, without floats.

use std::collections::HashMap;

use tocsin_core::Health;
use tocsin_store::{DailyRollup, Day};

/// How many days a bar covers, today included.
pub const DAYS: i64 = 90;

/// Parts per million in a whole.
const WHOLE: i64 = 1_000_000;

/// What a day cell shows. Each tone has its own fill in the stylesheet,
/// told apart by shape as well as colour, and every cell carries its text,
/// so no tone rests on colour alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Every result up.
    Up,
    /// The worst result degraded.
    Degraded,
    /// At least one result down.
    Down,
    /// No result recorded.
    NoData,
}

impl Tone {
    /// The cell's CSS class.
    pub const fn class(self) -> &'static str {
        match self {
            Tone::Up => "up",
            Tone::Degraded => "degraded",
            Tone::Down => "down",
            Tone::NoData => "none",
        }
    }

    const fn of(worst: Health) -> Self {
        match worst {
            Health::Up => Tone::Up,
            Health::Degraded => Tone::Degraded,
            Health::Down => Tone::Down,
        }
    }
}

/// One day of a bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayCell {
    /// Its fill.
    pub tone: Tone,
    /// What it says on hover, and to a screen reader:
    /// `2026-10-22: 99.95% up, worst degraded`.
    pub text: String,
    /// Whether this is today, which is still going: its figure covers the
    /// day so far, and it has none until the first hourly rollup.
    pub today: bool,
}

/// One component's bar, oldest day first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bar {
    /// [`DAYS`] cells, today last.
    pub days: Vec<DayCell>,
    /// The bar in words: `99.98% up over the last 90 days`.
    pub summary: String,
}

/// One check's results on one day, every node summed.
#[derive(Debug, Clone, Copy)]
struct Tally {
    total: i64,
    down: i64,
    worst: Health,
}

/// The rollups indexed by check and day, built once per page.
#[derive(Debug, Default)]
pub struct Tallies(HashMap<(String, Day), Tally>);

impl Tallies {
    /// `rollups` summed over nodes.
    pub fn new(rollups: &[DailyRollup]) -> Self {
        let mut tallies: HashMap<(String, Day), Tally> = HashMap::new();
        for rollup in rollups {
            tallies
                .entry((rollup.check_id.clone(), rollup.date))
                .and_modify(|tally| {
                    tally.total += rollup.total;
                    tally.down += rollup.down;
                    tally.worst = tally.worst.max(rollup.worst);
                })
                .or_insert(Tally {
                    total: rollup.total,
                    down: rollup.down,
                    worst: rollup.worst,
                });
        }
        Tallies(tallies)
    }

    /// The parts per million up and the worst health on `day` across
    /// `checks`; `None` when none of them has a result that day.
    fn day(&self, checks: &[String], day: Day) -> Option<(i64, Health)> {
        let mut sum = 0;
        let mut counted = 0;
        let mut worst = Health::Up;
        for check in checks {
            let Some(tally) = self.0.get(&(check.clone(), day)) else {
                continue;
            };
            if tally.total <= 0 {
                continue;
            }
            sum += (tally.total - tally.down) * WHOLE / tally.total;
            counted += 1;
            worst = worst.max(tally.worst);
        }
        (counted > 0).then(|| (sum / counted, worst))
    }
}

/// The first day a bar ending `today` shows.
pub const fn first_day(today: Day) -> Day {
    today.plus(1 - DAYS)
}

/// The bar for a component made of `checks`, ending `today`.
pub fn bar(tallies: &Tallies, checks: &[String], today: Day) -> Bar {
    let mut days = Vec::new();
    let mut sum = 0;
    let mut counted = 0;
    let mut day = first_day(today);
    while day <= today {
        let is_today = day == today;
        let label = if is_today {
            format!("{day}, today so far")
        } else {
            day.to_string()
        };
        days.push(match tallies.day(checks, day) {
            Some((up, worst)) => {
                sum += up;
                counted += 1;
                let tone = Tone::of(worst);
                let text = if tone == Tone::Up {
                    format!("{label}: {} up", percent(up))
                } else {
                    format!("{label}: {} up, worst {worst}", percent(up))
                };
                DayCell {
                    tone,
                    text,
                    today: is_today,
                }
            }
            None if is_today => DayCell {
                tone: Tone::NoData,
                text: format!("{day}, today: no data yet, the first figure comes within the hour"),
                today: true,
            },
            None => DayCell {
                tone: Tone::NoData,
                text: format!("{day}: no data"),
                today: false,
            },
        });
        day = day.plus(1);
    }
    let summary = if counted == 0 {
        format!("No data in the last {DAYS} days")
    } else if counted == DAYS {
        format!("{} up over the last {DAYS} days", percent(sum / counted))
    } else {
        format!(
            "{} up over the {counted} of the last {DAYS} days with data",
            percent(sum / counted)
        )
    };
    Bar { days, summary }
}

/// Parts per million as a percentage floored to two places: `99.95%`, or
/// `100%` only when nothing was down.
fn percent(parts_per_million: i64) -> String {
    let hundredths = parts_per_million / 100;
    if hundredths >= 10_000 {
        "100%".to_owned()
    } else {
        format!("{}.{:02}%", hundredths / 100, hundredths % 100)
    }
}

#[cfg(test)]
mod tests {
    use tocsin_core::Health;
    use tocsin_store::{DailyRollup, Day};

    use super::{DAYS, Tallies, Tone, bar, percent};

    fn rollup(
        check: &str,
        node: &str,
        date: Day,
        up: i64,
        down: i64,
        worst: Health,
    ) -> DailyRollup {
        DailyRollup {
            check_id: check.to_owned(),
            node_id: node.to_owned(),
            date,
            kind: tocsin_store::SubjectKind::Check,
            total: up + down,
            up,
            degraded: 0,
            down,
            worst,
        }
    }

    #[test]
    fn a_percentage_is_floored_so_one_failure_never_reads_full() {
        assert_eq!(percent(1_000_000), "100%");
        assert_eq!(percent(999_999), "99.99%");
        assert_eq!(percent(999_500), "99.95%");
        assert_eq!(percent(0), "0.00%");
    }

    #[test]
    fn a_bar_has_a_cell_per_day_today_last_and_says_where_data_is_missing() {
        let today = Day::containing(1_760_000_000_000);
        let checks = vec!["web".to_owned(), "db".to_owned()];
        let tallies = Tallies::new(&[
            // Today: two nodes summed for web, db all up: (0.75 + 1) / 2.
            rollup("web", "a", today, 50, 0, Health::Up),
            rollup("web", "b", today, 25, 25, Health::Down),
            rollup("db", "a", today, 10, 0, Health::Up),
            // Yesterday: only a degraded day on db.
            rollup("db", "a", today.plus(-1), 10, 0, Health::Degraded),
            // Another component's check, and a day before the window.
            rollup("other", "a", today, 0, 10, Health::Down),
            rollup("web", "a", today.plus(-DAYS), 0, 10, Health::Down),
        ]);
        let bar = bar(&tallies, &checks, today);
        assert_eq!(bar.days.len(), 90);
        let last = &bar.days[89];
        assert_eq!(last.tone, Tone::Down);
        assert_eq!(
            last.text,
            format!("{today}, today so far: 87.50% up, worst down")
        );
        assert!(last.today);
        assert!(bar.days[..89].iter().all(|day| !day.today));
        let before = &bar.days[88];
        assert_eq!(before.tone, Tone::Degraded);
        assert_eq!(
            before.text,
            format!("{}: 100% up, worst degraded", today.plus(-1))
        );
        assert_eq!(bar.days[0].tone, Tone::NoData);
        assert_eq!(bar.days[0].text, format!("{}: no data", today.plus(-89)));
        assert_eq!(
            bar.summary,
            "93.75% up over the 2 of the last 90 days with data"
        );
    }

    #[test]
    fn a_component_without_data_says_so() {
        let today = Day::containing(0);
        let bar = bar(&Tallies::default(), &["web".to_owned()], today);
        assert!(bar.days.iter().all(|day| day.tone == Tone::NoData));
        assert_eq!(bar.summary, "No data in the last 90 days");
        // Today is still going: a figure is on its way, not missing.
        assert_eq!(
            bar.days[89].text,
            format!("{today}, today: no data yet, the first figure comes within the hour")
        );
        assert_eq!(bar.days[88].text, format!("{}: no data", today.plus(-1)));
    }
}
