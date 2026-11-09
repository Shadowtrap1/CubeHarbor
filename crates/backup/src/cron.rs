//! Five-field cron expressions, evaluated in UTC.

use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};

/// How far to look for a matching day: long enough for `0 0 29 2 *` across a skipped leap year.
const SEARCH_DAYS: i32 = 366 * 9;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
    day_restricted: bool,
    weekday_restricted: bool,
}

fn field(text: &str, min: u32, max: u32) -> Result<Vec<u32>, String> {
    let mut values = vec![];
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .map_err(|_| format!("bad step in {part:?}"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            return Err(format!("step 0 in {part:?}"));
        }
        let num = |s: &str| {
            s.parse::<u32>()
                .map_err(|_| format!("bad value in {part:?}"))
        };
        let (from, to) = match range.split_once('-') {
            _ if range == "*" => (min, max),
            Some((a, b)) => (num(a)?, num(b)?),
            None if part.contains('/') => (num(range)?, max),
            None => (num(range)?, num(range)?),
        };
        if from < min || to > max || from > to {
            return Err(format!("{part:?} is outside {min}-{max}"));
        }
        values.extend((from..=to).step_by(step as usize));
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self, String> {
        let expanded = match expr.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            other => other,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = f[..] else {
            return Err(format!(
                "{expr:?} needs five fields: minute hour day month weekday"
            ));
        };
        // 7 is also Sunday.
        let mut weekdays: Vec<u32> = field(weekday, 0, 7)?.into_iter().map(|d| d % 7).collect();
        weekdays.sort_unstable();
        weekdays.dedup();
        Ok(Cron {
            minutes: field(minute, 0, 59)?,
            hours: field(hour, 0, 23)?,
            days: field(day, 1, 31)?,
            months: field(month, 1, 12)?,
            weekdays,
            day_restricted: !day.starts_with('*'),
            weekday_restricted: !weekday.starts_with('*'),
        })
    }

    /// As in standard cron: when both day fields are restricted, either one may match.
    fn matches_date(&self, d: Date) -> bool {
        if !self.months.contains(&(d.month() as u32)) {
            return false;
        }
        let day = self.days.contains(&(d.day() as u32));
        let weekday = self
            .weekdays
            .contains(&(d.weekday().to_sunday_zero_offset() as u32));
        match (self.day_restricted, self.weekday_restricted) {
            (true, true) => day || weekday,
            _ => day && weekday,
        }
    }

    fn at(date: Date, hour: u32, minute: u32) -> Option<Timestamp> {
        let t = Time::new(hour as i8, minute as i8, 0, 0).ok()?;
        date.to_datetime(t)
            .to_zoned(TimeZone::UTC)
            .ok()
            .map(|z| z.timestamp())
    }

    /// The first fire time strictly after `now`.
    pub fn next_after(&self, now: Timestamp) -> Option<Timestamp> {
        let mut date = now.to_zoned(TimeZone::UTC).date();
        for _ in 0..SEARCH_DAYS {
            if self.matches_date(date) {
                for &h in &self.hours {
                    for &m in &self.minutes {
                        if let Some(t) = Self::at(date, h, m).filter(|t| *t > now) {
                            return Some(t);
                        }
                    }
                }
            }
            date = date.checked_add(1.day()).ok()?;
        }
        None
    }

    /// The last fire time at or before `now`.
    pub fn latest_at(&self, now: Timestamp) -> Option<Timestamp> {
        let mut date = now.to_zoned(TimeZone::UTC).date();
        for _ in 0..SEARCH_DAYS {
            if self.matches_date(date) {
                for &h in self.hours.iter().rev() {
                    for &m in self.minutes.iter().rev() {
                        if let Some(t) = Self::at(date, h, m).filter(|t| *t <= now) {
                            return Some(t);
                        }
                    }
                }
            }
            date = date.checked_sub(1.day()).ok()?;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_rejects() {
        assert!(Cron::parse("*/15 3,4 1-7 * 1-5").is_ok());
        assert!(Cron::parse("@daily").is_ok());
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "*/0 * * * *",
            "a * * * *",
            "5-1 * * * *",
            "* * * * 8",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn next_and_latest() {
        let c = Cron::parse("30 4 * * *").unwrap();
        assert_eq!(
            c.next_after(ts("2026-01-01T04:30:00Z")),
            Some(ts("2026-01-02T04:30:00Z"))
        );
        assert_eq!(
            c.next_after(ts("2026-01-01T04:29:59Z")),
            Some(ts("2026-01-01T04:30:00Z"))
        );
        assert_eq!(
            c.latest_at(ts("2026-01-01T04:30:00Z")),
            Some(ts("2026-01-01T04:30:00Z"))
        );
        assert_eq!(
            c.latest_at(ts("2026-01-01T04:29:00Z")),
            Some(ts("2025-12-31T04:30:00Z"))
        );
    }

    #[test]
    fn steps_and_sunday() {
        let c = Cron::parse("*/20 * * * *").unwrap();
        assert_eq!(
            c.next_after(ts("2026-01-01T00:41:00Z")),
            Some(ts("2026-01-01T01:00:00Z"))
        );
        // 2026-01-04 is a Sunday; 0 and 7 both mean Sunday.
        for expr in ["0 0 * * 0", "0 0 * * 7"] {
            assert_eq!(
                Cron::parse(expr)
                    .unwrap()
                    .next_after(ts("2026-01-01T00:00:00Z")),
                Some(ts("2026-01-04T00:00:00Z"))
            );
        }
    }

    #[test]
    fn either_day_field_may_match_when_both_are_set() {
        // The 15th (a Thursday) or any Monday: Monday the 5th comes first.
        let c = Cron::parse("0 0 15 * 1").unwrap();
        assert_eq!(
            c.next_after(ts("2026-01-01T00:00:00Z")),
            Some(ts("2026-01-05T00:00:00Z"))
        );
        assert_eq!(
            c.next_after(ts("2026-01-13T00:00:00Z")),
            Some(ts("2026-01-15T00:00:00Z"))
        );
    }

    #[test]
    fn leap_day() {
        let c = Cron::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            c.next_after(ts("2026-01-01T00:00:00Z")),
            Some(ts("2028-02-29T00:00:00Z"))
        );
        assert_eq!(
            c.latest_at(ts("2026-01-01T00:00:00Z")),
            Some(ts("2024-02-29T00:00:00Z"))
        );
    }
}
