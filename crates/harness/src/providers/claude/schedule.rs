//! Port of src/integrations/harness/providers/claude/claudeSchedule.ts: when
//! a job Claude scheduled with CronCreate fires next.

use std::collections::BTreeSet;

use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};

/// `nextClaudeCronFire`: the next local time after `now_ms` (epoch ms) that
/// a five-field cron expression matches, or `None` when the expression is
/// invalid or uses syntax Claude does not accept, such as day names.
pub fn next_claude_cron_fire(cron: &str, now_ms: i64) -> Option<i64> {
    let parts: Vec<&str> = cron.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    let bounds = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    let mut fields = Vec::with_capacity(5);
    for (part, (min, max)) in parts.iter().zip(bounds) {
        fields.push(cron_values(part, min, max)?);
    }
    let (minutes, hours, days, months, weekdays) =
        (&fields[0], &fields[1], &fields[2], &fields[3], &fields[4]);
    let start = Local.timestamp_millis_opt(now_ms).single()?.date_naive();
    // Eight years include the next leap day even across a non-leap century.
    for offset in 0..8 * 366 {
        let date = start.checked_add_days(chrono::Days::new(offset))?;
        if !months.contains(&date.month()) {
            continue;
        }
        let weekday = date.weekday().num_days_from_sunday();
        let day_matches = days.contains(&date.day());
        let weekday_matches =
            weekdays.contains(&weekday) || (weekday == 0 && weekdays.contains(&7));
        // Standard cron: when both day fields are restricted, either one
        // matching is enough.
        let matches = if parts[2] == "*" {
            weekday_matches
        } else if parts[4] == "*" {
            day_matches
        } else {
            day_matches || weekday_matches
        };
        if !matches {
            continue;
        }
        if let Some(fire) = first_fire_on(date, hours, minutes, now_ms) {
            return Some(fire);
        }
    }
    None
}

/// The first matching time on `date` after `now_ms`. A time that a daylight
/// saving change skips does not fire.
fn first_fire_on(
    date: NaiveDate,
    hours: &BTreeSet<u32>,
    minutes: &BTreeSet<u32>,
    now_ms: i64,
) -> Option<i64> {
    for &hour in hours {
        for &minute in minutes {
            let Some(candidate) = date
                .and_hms_opt(hour, minute, 0)
                .and_then(|time| Local.from_local_datetime(&time).earliest())
            else {
                continue;
            };
            if candidate.hour() != hour || candidate.minute() != minute {
                continue;
            }
            let fire = candidate.timestamp_millis();
            if fire > now_ms {
                return Some(fire);
            }
        }
    }
    None
}

/// `cronValues`: the values one field allows. Supports `*`, numbers, ranges,
/// lists, and steps.
fn cron_values(part: &str, min: u32, max: u32) -> Option<BTreeSet<u32>> {
    let mut values = BTreeSet::new();
    for entry in part.split(',') {
        let (range, step) = match entry.split_once('/') {
            Some((range, step)) => (range, Some(step)),
            None => (entry, None),
        };
        let step: u32 = match step {
            Some(step) if !step.is_empty() && step.bytes().all(|b| b.is_ascii_digit()) => {
                step.parse().ok()?
            }
            Some(_) => return None,
            None => 1,
        };
        let number = |text: &str| -> Option<u32> {
            (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
                .then(|| text.parse().ok())
                .flatten()
        };
        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((low, high)) = range.split_once('-') {
            (number(low)?, number(high)?)
        } else {
            let start = number(range)?;
            (start, if entry.contains('/') { max } else { start })
        };
        if step < 1 || start < min || end > max || start > end {
            return None;
        }
        values.extend((start..=end).step_by(step as usize));
    }
    (!values.is_empty()).then_some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .earliest()
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn finds_the_next_local_fire_across_lists_ranges_and_steps() {
        let now = local(2026, 10, 3, 12, 6);
        assert_eq!(
            next_claude_cron_fire("*/5 12-14 * * 0,6", now),
            Some(local(2026, 10, 3, 12, 10))
        );
    }

    #[test]
    fn uses_either_constrained_day_field_and_accepts_sunday_as_seven() {
        let now = local(2026, 10, 3, 12, 0);
        assert_eq!(
            next_claude_cron_fire("0 9 15 * 7", now),
            Some(local(2026, 10, 4, 9, 0))
        );
    }

    #[test]
    fn keeps_a_reminder_scheduled_for_the_next_leap_day() {
        let now = local(2025, 3, 1, 0, 0);
        assert_eq!(
            next_claude_cron_fire("0 9 29 2 *", now),
            Some(local(2028, 2, 29, 9, 0))
        );
    }

    #[test]
    fn does_not_guess_a_fire_time_for_invalid_or_unsupported_cron() {
        for cron in [
            "* * * *",
            "*/0 * * * *",
            "60 * * * *",
            "0 24 * * *",
            "0 9 * 13 *",
            "0 9 * * MON",
        ] {
            assert_eq!(next_claude_cron_fire(cron, 0), None, "{cron}");
        }
    }
}
