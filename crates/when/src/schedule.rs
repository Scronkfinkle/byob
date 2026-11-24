//! Schedules: "every day at 8:00", "weekdays at 7:30", "every 2 hours". A schedule is an
//! RRULE (the subset [`crate::recur`] reads, plus `FREQ=HOURLY` and `FREQ=MINUTELY`), a
//! time of day, a zone and a first day. [`Schedule::next_after`] finds the next time it's
//! due; [`Schedule::describe`] says it in plain words for a person.
//!
//! Daily and longer rules keep their wall-clock time across DST (08:00 stays 08:00); a time
//! that doesn't exist that day (02:30 when the clocks spring forward) is an hour later, and a
//! time that happens twice (when they fall back) is the first one. Hourly and minutely
//! rules step in real time from the first day's time of day, so "every 2 hours" is two
//! hours apart even across a change.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc, Weekday};

use crate::recur::{self, Freq, Rule};
use crate::time::{UserZone, When, Zone, month_len};

/// The shortest gap a schedule may have (minutes).
pub const MIN_MINUTES: u32 = 15;
/// The longest rule text read.
const MAX_RULE: usize = 500;
/// Most steps walked looking for a sub-daily time on an allowed weekday.
const MAX_STEPS: u32 = 3_000;

/// A parsed schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    rule: Rule,
    /// Set for HOURLY/MINUTELY: the gap in minutes (BYDAY, UNTIL and COUNT still apply).
    every_minutes: Option<u32>,
    at: NaiveTime,
    zone: UserZone,
    start: Option<NaiveDate>,
}

impl Schedule {
    /// Read a schedule. `rule` is an RRULE (`FREQ=DAILY`, `FREQ=WEEKLY;BYDAY=MO,WE`,
    /// `FREQ=HOURLY;INTERVAL=2`, a leading `RRULE:` is fine), `at` a 24-hour `HH:MM`, `zone` an
    /// IANA name (none: the user's zone), `start` the first day `YYYY-MM-DD` (none: any day;
    /// rules whose phase depends on it, like every 2 weeks, need one to stay put). Errors
    /// say what's wrong in plain words.
    pub fn parse(rule: &str, at: &str, zone: Option<&str>, start: Option<&str>) -> Result<Self, String> {
        let text = rule.trim();
        let text = text.strip_prefix("RRULE:").unwrap_or(text);
        if text.len() > MAX_RULE {
            return Err(format!("the schedule's rule is longer than {MAX_RULE} characters"));
        }
        let freq =
            text.split(';').filter_map(|p| p.split_once('=')).find(|(k, _)| k.trim().eq_ignore_ascii_case("FREQ")).map(|(_, v)| v.trim().to_ascii_uppercase());
        let (rule, every_minutes) = match freq.as_deref() {
            Some(sub @ ("HOURLY" | "MINUTELY")) => {
                // Read the rest of the rule as a daily one: BYDAY, UNTIL and COUNT mean the same.
                let daily: Vec<String> =
                    text.split(';').map(|p| if p.trim().to_ascii_uppercase().starts_with("FREQ=") { "FREQ=DAILY".to_owned() } else { p.to_owned() }).collect();
                let rule = recur::parse_rule(&daily.join(";")).map_err(|e| format!("the schedule's rule has {e}, which schedules don't support"))?;
                let unit = if sub == "HOURLY" { 60 } else { 1 };
                let minutes = rule.interval.saturating_mul(unit);
                if minutes < MIN_MINUTES {
                    return Err(format!("a routine can run at most every {MIN_MINUTES} minutes (this rule is every {minutes})"));
                }
                (rule, Some(minutes))
            }
            Some("SECONDLY") => return Err(format!("a routine can run at most every {MIN_MINUTES} minutes")),
            _ => (
                recur::parse_rule(text).map_err(|e| {
                    if e == "no FREQ" {
                        "the schedule's rule needs a FREQ (like FREQ=DAILY)".to_owned()
                    } else {
                        format!("the schedule's rule has {e}, which schedules don't support")
                    }
                })?,
                None,
            ),
        };
        let at = parse_at(at)?;
        let zone = match zone.map(str::trim).filter(|z| !z.is_empty()) {
            Some(name) => match crate::time::zone_named(name) {
                Some(Zone::Iana(tz)) => UserZone::Iana(tz),
                _ => return Err(format!("{name:?} isn't a time zone I know (write an IANA name like Europe/Berlin)")),
            },
            None => UserZone::default(),
        };
        let start = match start.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Some(NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("the start {s:?} isn't a date (write YYYY-MM-DD)"))?),
            None => None,
        };
        Ok(Self { rule, every_minutes, at, zone, start })
    }

    /// The zone it runs in.
    pub fn zone(&self) -> UserZone {
        self.zone
    }

    /// The gap between runs when it's hourly or minutely.
    pub fn every_minutes(&self) -> Option<u32> {
        self.every_minutes
    }

    /// The first time it's due strictly after `t`, if it's ever due again.
    pub fn next_after(&self, t: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self.every_minutes {
            Some(m) => self.next_sub_daily(t, m),
            None => self.next_daily(t),
        }
    }

    /// Roughly how many times it runs in 30 days from `t` (for "about 30 runs a month").
    pub fn runs_in_30_days(&self, t: DateTime<Utc>) -> u32 {
        let end = t + Duration::days(30);
        let mut n = 0u32;
        let mut at = t;
        while let Some(next) = self.next_after(at) {
            if next > end || n >= 3_000 {
                break;
            }
            n = n.saturating_add(1);
            at = next;
        }
        n
    }

    fn local_date(&self, t: DateTime<Utc>) -> NaiveDate {
        self.zone.local(t).date_naive()
    }

    fn next_daily(&self, t: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // Without a first day, rules that need one (a weekday, a day of the month) take today's.
        let start = self.start.unwrap_or_else(|| self.local_date(t));
        let first = When::At(start.and_time(self.at), Zone::Floating);
        let from = t + Duration::seconds(1);
        for days in [40, 400, 3_000] {
            let found = recur::expand(&self.rule, first, Duration::zero(), from, from + Duration::days(days), &self.zone);
            if let Some(w) = found.starts.first() {
                return Some(w.instant(&self.zone));
            }
        }
        None
    }

    fn next_sub_daily(&self, t: DateTime<Utc>, minutes: u32) -> Option<DateTime<Utc>> {
        // Without a first day, start from yesterday's time of day so today's slots before it count.
        let day = self.start.unwrap_or_else(|| self.local_date(t).pred_opt().unwrap_or_else(|| self.local_date(t)));
        let anchor = self.zone.resolve(day.and_time(self.at));
        let step = Duration::minutes(i64::from(minutes));
        let step_secs = step.num_seconds().max(60);
        let until = self.rule.until.map(|u| u.instant(&self.zone));
        let mut k: i64 = if t < anchor { 0 } else { (t - anchor).num_seconds() / step_secs + 1 };
        for _ in 0..MAX_STEPS {
            if self.rule.count.is_some_and(|c| k >= i64::from(c)) {
                return None;
            }
            let at = anchor.checked_add_signed(Duration::seconds(step_secs.checked_mul(k)?))?;
            if until.is_some_and(|u| at > u) {
                return None;
            }
            if at > t && (self.rule.by_day.is_empty() || self.rule.by_day.iter().any(|(_, w)| *w == self.zone.local(at).weekday())) {
                return Some(at);
            }
            k = k.checked_add(1)?;
        }
        None
    }

    /// The schedule in plain words: `Every day at 8:00 AM`, `Weekdays at 7:30 AM`,
    /// `Every 2 hours`, `Every month on the last Friday at 5:00 PM`.
    pub fn describe(&self) -> String {
        let r = &self.rule;
        let at = clock(self.at);
        let days: Vec<Weekday> = r.by_day.iter().map(|(_, w)| *w).collect();
        let mut s = if let Some(m) = self.every_minutes {
            let gap = if m % 60 == 0 { plural_bare(m / 60, "hour") } else { plural_bare(m, "minute") };
            let mut s = format!("Every {gap}");
            if !days.is_empty() {
                s.push_str(&format!(" on {}", day_set(&days, true)));
            }
            s
        } else {
            match r.freq {
                Freq::Daily | Freq::Weekly if is_every_day(&days) && r.interval == 1 => format!("Every day at {at}"),
                Freq::Daily | Freq::Weekly if r.interval == 1 && !days.is_empty() && matches!(day_set(&days, false).as_str(), "Weekdays" | "Weekends") => {
                    format!("{} at {at}", day_set(&days, false))
                }
                Freq::Daily if days.is_empty() => format!("Every {} at {at}", plural_bare(r.interval, "day")),
                Freq::Daily => format!("Every {} on {} at {at}", plural_bare(r.interval, "day"), day_set(&days, true)),
                Freq::Weekly => {
                    let on = if !days.is_empty() { Some(day_set(&days, true)) } else { self.start.map(|d| day_name(d.weekday()).to_owned()) };
                    match (r.interval, on) {
                        (1, Some(on)) => format!("Every {on} at {at}"),
                        (1, None) => format!("Every week at {at}"),
                        (n, Some(on)) => format!("Every {n} weeks on {on} at {at}"),
                        (n, None) => format!("Every {n} weeks at {at}"),
                    }
                }
                Freq::Monthly => {
                    let on = self.month_part();
                    format!("Every {}{} at {at}", plural_bare(r.interval, "month"), on.map(|o| format!(" on {o}")).unwrap_or_default())
                }
                Freq::Yearly => {
                    let on = self.year_part();
                    format!("Every {}{} at {at}", plural_bare(r.interval, "year"), on.map(|o| format!(" on {o}")).unwrap_or_default())
                }
            }
        };
        if let Some(u) = r.until {
            s.push_str(&format!(", until {}", u.date().format("%b %-d, %Y")));
        }
        if let Some(c) = r.count {
            s.push_str(&format!(", {}", plural(c, "time")));
        }
        s
    }

    /// `the 1st`, `the last day`, `the second Tuesday`, `the last weekday`.
    fn month_part(&self) -> Option<String> {
        let r = &self.rule;
        let days: Vec<Weekday> = r.by_day.iter().map(|(_, w)| *w).collect();
        if !r.by_set_pos.is_empty() {
            let pos = r.by_set_pos.first().copied().unwrap_or(1);
            let what = match day_set(&days, false).as_str() {
                "Weekdays" => "weekday".to_owned(),
                "Weekends" => "weekend day".to_owned(),
                _ if days.is_empty() => "day".to_owned(),
                _ => format!("day of {}", day_set(&days, true)),
            };
            return Some(format!("the {} {what}", ordinal_word(pos)));
        }
        if !r.by_month_day.is_empty() {
            let list: Vec<String> = r
                .by_month_day
                .iter()
                .map(|&n| {
                    if n == -1 {
                        "last day".to_owned()
                    } else if n < 0 {
                        format!("{} to last day", ordinal_word(n.saturating_neg()))
                    } else {
                        ordinal(n)
                    }
                })
                .collect();
            let mut s = format!("the {}", join_and(&list));
            if !days.is_empty() {
                s.push_str(&format!(" if it's a {}", day_set(&days, true)));
            }
            return Some(s);
        }
        if !r.by_day.is_empty() {
            let list: Vec<String> = r
                .by_day
                .iter()
                .map(|&(n, w)| if n == 0 { format!("every {}", day_name(w)) } else { format!("the {} {}", ordinal_word(n), day_name(w)) })
                .collect();
            return Some(join_and(&list));
        }
        self.start.map(|d| format!("the {}", ordinal(i32::try_from(d.day()).unwrap_or(1))))
    }

    /// `March 3`, `the fourth Thursday of November`.
    fn year_part(&self) -> Option<String> {
        let r = &self.rule;
        let months: Vec<String> = r.by_month.iter().map(|&m| month_name(m).to_owned()).collect();
        if r.by_day.is_empty() && r.by_month_day.is_empty() {
            let d = self.start?;
            let ms = if months.is_empty() { month_name(d.month()).to_owned() } else { join_and(&months) };
            return Some(format!("{ms} {}", d.day()));
        }
        let inner = self.month_part()?;
        Some(if months.is_empty() { format!("{inner} of the year") } else { format!("{inner} of {}", join_and(&months)) })
    }
}

/// `08:00`, `8:00`, `7:30`, `17:05` (24-hour).
fn parse_at(at: &str) -> Result<NaiveTime, String> {
    let s = at.trim();
    let bad = || format!("the time {s:?} isn't a time of day (write 24-hour HH:MM, like 08:00 or 17:30)");
    if s.len() > 8 {
        return Err(bad());
    }
    NaiveTime::parse_from_str(s, "%H:%M").map_err(|_| bad())
}

/// `8:00 AM`, `12:30 PM`, `12:00 AM`.
pub fn clock(t: NaiveTime) -> String {
    t.format("%-I:%M %p").to_string()
}

fn plural(n: u32, unit: &str) -> String {
    if n == 1 { format!("1 {unit}") } else { format!("{n} {unit}s") }
}

/// `day`, `2 days` (after "Every").
fn plural_bare(n: u32, unit: &str) -> String {
    if n == 1 { unit.to_owned() } else { format!("{n} {unit}s") }
}

fn is_every_day(days: &[Weekday]) -> bool {
    [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat, Weekday::Sun].iter().all(|w| days.contains(w))
}

/// `Weekdays`, `Weekends`, else `Monday and Wednesday` (`weekdays` when `lower`).
fn day_set(days: &[Weekday], lower: bool) -> String {
    let mut sorted: Vec<Weekday> = days.to_vec();
    sorted.sort_by_key(Weekday::num_days_from_monday);
    sorted.dedup();
    let weekdays = [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri];
    let named = if sorted == weekdays {
        "Weekdays"
    } else if sorted == [Weekday::Sat, Weekday::Sun] {
        "Weekends"
    } else {
        let names: Vec<String> = sorted.iter().map(|w| day_name(*w).to_owned()).collect();
        return join_and(&names);
    };
    if lower { named.to_lowercase() } else { named.to_owned() }
}

fn day_name(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

fn month_name(m: u32) -> &'static str {
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"]
        .get(usize::try_from(m.saturating_sub(1)).unwrap_or(0))
        .copied()
        .unwrap_or("January")
}

/// `1st`, `2nd`, `23rd`, `11th`.
fn ordinal(n: i32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// `first` … `fifth`, `last`, `second to last`, else `7th`.
fn ordinal_word(n: i32) -> String {
    match n {
        1 => "first".into(),
        2 => "second".into(),
        3 => "third".into(),
        4 => "fourth".into(),
        5 => "fifth".into(),
        -1 => "last".into(),
        -2 => "second to last".into(),
        n if n < 0 => format!("{} to last", ordinal(n.saturating_neg())),
        n => ordinal(n),
    }
}

/// `a`, `a and b`, `a, b and c`.
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Days in a month (re-exported for callers checking month ends).
pub fn days_in_month(year: i32, month: u32) -> u32 {
    month_len(year, month)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDateTime, TimeZone};

    fn sched(rule: &str, at: &str, start: Option<&str>) -> Schedule {
        Schedule::parse(rule, at, Some("Europe/Berlin"), start).unwrap()
    }

    /// A Berlin wall-clock time as an instant.
    fn berlin(s: &str) -> DateTime<Utc> {
        UserZone::Iana(chrono_tz::Europe::Berlin).resolve(NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
    }

    fn utc(s: &str) -> DateTime<Utc> {
        Utc.from_utc_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
    }

    /// The next `n` due times after `from`, as Berlin wall-clock text.
    fn nexts(s: &Schedule, from: DateTime<Utc>, n: usize) -> Vec<String> {
        let mut out = vec![];
        let mut t = from;
        for _ in 0..n {
            let Some(next) = s.next_after(t) else { break };
            out.push(s.zone().local(next).format("%a %Y-%m-%d %H:%M %:z").to_string());
            t = next;
        }
        out
    }

    #[test]
    fn daily_keeps_its_wall_clock_time_across_dst() {
        let s = sched("FREQ=DAILY", "08:00", Some("2026-10-01"));
        assert_eq!(nexts(&s, berlin("2026-10-24 09:00"), 3), ["Sun 2026-10-25 08:00 +01:00", "Mon 2026-10-26 08:00 +01:00", "Tue 2026-10-27 08:00 +01:00"]);
        // strictly after: at 08:00 sharp the next one is tomorrow
        assert_eq!(nexts(&s, berlin("2026-10-26 08:00"), 1), ["Tue 2026-10-27 08:00 +01:00"]);
        assert_eq!(nexts(&s, berlin("2026-10-26 07:59"), 1), ["Mon 2026-10-26 08:00 +01:00"]);
        // before the first day: the first day
        assert_eq!(nexts(&s, berlin("2026-01-01 12:00"), 1), ["Thu 2026-10-01 08:00 +02:00"]);
    }

    #[test]
    fn a_time_lost_to_spring_forward_runs_an_hour_later_and_a_repeated_one_runs_once() {
        // Berlin springs forward 2026-03-29 02:00 -> 03:00: 02:30 doesn't exist that day.
        let s = sched("FREQ=DAILY", "02:30", Some("2026-03-01"));
        assert_eq!(nexts(&s, berlin("2026-03-28 12:00"), 3), ["Sun 2026-03-29 03:30 +02:00", "Mon 2026-03-30 02:30 +02:00", "Tue 2026-03-31 02:30 +02:00"]);
        // It falls back 2026-10-25 03:00 -> 02:00: 02:30 happens twice; it runs at the first.
        let got = nexts(&s, berlin("2026-10-24 12:00"), 2);
        assert_eq!(got, ["Sun 2026-10-25 02:30 +02:00", "Mon 2026-10-26 02:30 +01:00"]);
        let first = s.next_after(berlin("2026-10-24 12:00")).unwrap();
        assert_eq!(first, utc("2026-10-25 00:30"));
        // not again at the second 02:30 (01:30 UTC)
        assert_eq!(s.next_after(first).unwrap(), utc("2026-10-26 01:30"));
    }

    #[test]
    fn weekdays_weekly_and_intervals() {
        let s = sched("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR", "07:30", Some("2026-11-01"));
        assert_eq!(s.describe(), "Weekdays at 7:30 AM");
        // Friday evening: next is Monday
        assert_eq!(nexts(&s, berlin("2026-11-20 18:00"), 2), ["Mon 2026-11-23 07:30 +01:00", "Tue 2026-11-24 07:30 +01:00"]);
        let two = sched("FREQ=WEEKLY;INTERVAL=2", "09:00", Some("2026-11-02"));
        assert_eq!(two.describe(), "Every 2 weeks on Monday at 9:00 AM");
        assert_eq!(nexts(&two, berlin("2026-11-03 00:00"), 2), ["Mon 2026-11-16 09:00 +01:00", "Mon 2026-11-30 09:00 +01:00"]);
        let daily_by_day = sched("FREQ=DAILY;BYDAY=SA,SU", "10:00", None);
        assert_eq!(daily_by_day.describe(), "Weekends at 10:00 AM");
        assert_eq!(nexts(&daily_by_day, berlin("2026-11-24 12:00"), 2), ["Sat 2026-11-28 10:00 +01:00", "Sun 2026-11-29 10:00 +01:00"]);
    }

    #[test]
    fn month_ends_and_years() {
        let last = sched("FREQ=MONTHLY;BYMONTHDAY=-1", "18:00", Some("2026-01-31"));
        assert_eq!(last.describe(), "Every month on the last day at 6:00 PM");
        assert_eq!(nexts(&last, berlin("2027-01-31 19:00"), 2), ["Sun 2027-02-28 18:00 +01:00", "Wed 2027-03-31 18:00 +02:00"]);
        // the 31st: months without one are skipped
        let d31 = sched("FREQ=MONTHLY", "08:00", Some("2026-01-31"));
        assert_eq!(d31.describe(), "Every month on the 31st at 8:00 AM");
        assert_eq!(nexts(&d31, berlin("2026-04-01 00:00"), 2), ["Sun 2026-05-31 08:00 +02:00", "Fri 2026-07-31 08:00 +02:00"]);
        let leap = sched("FREQ=YEARLY", "09:00", Some("2024-02-29"));
        assert_eq!(leap.describe(), "Every year on February 29 at 9:00 AM");
        assert_eq!(nexts(&leap, berlin("2024-03-01 00:00"), 1), ["Tue 2028-02-29 09:00 +01:00"]);
        let fri = sched("FREQ=MONTHLY;BYDAY=-1FR", "17:00", Some("2026-01-01"));
        assert_eq!(fri.describe(), "Every month on the last Friday at 5:00 PM");
        assert_eq!(nexts(&fri, berlin("2026-11-24 00:00"), 1), ["Fri 2026-11-27 17:00 +01:00"]);
        let thanks = sched("FREQ=YEARLY;BYMONTH=11;BYDAY=4TH", "12:00", Some("2026-01-01"));
        assert_eq!(thanks.describe(), "Every year on the fourth Thursday of November at 12:00 PM");
        let lastwd = sched("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1", "16:00", Some("2026-01-01"));
        assert_eq!(lastwd.describe(), "Every month on the last weekday at 4:00 PM");
        assert_eq!(nexts(&lastwd, berlin("2026-05-01 00:00"), 1), ["Fri 2026-05-29 16:00 +02:00"]);
    }

    #[test]
    fn hourly_steps_in_real_time_with_a_floor() {
        let s = sched("FREQ=HOURLY;INTERVAL=2", "08:00", Some("2026-11-01"));
        assert_eq!(s.describe(), "Every 2 hours");
        assert_eq!(nexts(&s, berlin("2026-11-24 09:15"), 3), ["Tue 2026-11-24 10:00 +01:00", "Tue 2026-11-24 12:00 +01:00", "Tue 2026-11-24 14:00 +01:00"]);
        // across fall-back the gap stays two real hours
        let dst = sched("FREQ=HOURLY;INTERVAL=2", "00:00", Some("2026-10-24"));
        let a = dst.next_after(berlin("2026-10-25 01:30")).unwrap();
        let b = dst.next_after(a).unwrap();
        assert_eq!(b - a, Duration::hours(2));
        let m = sched("FREQ=MINUTELY;INTERVAL=30", "00:00", None);
        assert_eq!(m.describe(), "Every 30 minutes");
        assert_eq!(nexts(&m, berlin("2026-11-24 09:15"), 2), ["Tue 2026-11-24 09:30 +01:00", "Tue 2026-11-24 10:00 +01:00"]);
        assert_eq!(m.runs_in_30_days(berlin("2026-11-01 00:00")), 30 * 48);
        let err = Schedule::parse("FREQ=MINUTELY;INTERVAL=5", "00:00", None, None).unwrap_err();
        assert!(err.contains("every 15 minutes"), "{err}");
        assert!(Schedule::parse("FREQ=SECONDLY", "00:00", None, None).is_err());
        assert!(Schedule::parse("FREQ=MINUTELY;INTERVAL=15", "00:00", None, None).is_ok());
        // weekdays only, every hour: Friday night skips to Monday
        let wd = sched("FREQ=HOURLY;BYDAY=MO,TU,WE,TH,FR", "00:00", Some("2026-11-01"));
        assert_eq!(wd.describe(), "Every hour on weekdays");
        assert_eq!(nexts(&wd, berlin("2026-11-27 23:30"), 1), ["Mon 2026-11-30 00:00 +01:00"]);
        // COUNT and UNTIL end it
        let c = sched("FREQ=HOURLY;COUNT=2", "08:00", Some("2026-11-24"));
        assert_eq!(nexts(&c, berlin("2026-11-24 00:00"), 5), ["Tue 2026-11-24 08:00 +01:00", "Tue 2026-11-24 09:00 +01:00"]);
    }

    #[test]
    fn zones_and_bad_input() {
        let ny = Schedule::parse("FREQ=DAILY", "08:00", Some("America/New_York"), None).unwrap();
        // 08:00 New York is 13:00 UTC in winter
        assert_eq!(ny.next_after(utc("2026-11-24 12:00")).unwrap(), utc("2026-11-24 13:00"));
        assert_eq!(ny.next_after(utc("2026-11-24 13:00")).unwrap(), utc("2026-11-25 13:00"));
        // a zone far east: 08:00 Tokyo is 23:00 UTC the day before
        let tokyo = Schedule::parse("FREQ=DAILY", "08:00", Some("Asia/Tokyo"), None).unwrap();
        assert_eq!(tokyo.next_after(utc("2026-11-24 12:00")).unwrap(), utc("2026-11-24 23:00"));
        assert!(Schedule::parse("FREQ=DAILY", "8am", None, None).unwrap_err().contains("HH:MM"));
        assert!(Schedule::parse("FREQ=DAILY", "25:00", None, None).is_err());
        assert!(Schedule::parse("FREQ=DAILY", "08:00", Some("Mars/Olympus"), None).unwrap_err().contains("time zone"));
        assert!(Schedule::parse("FREQ=DAILY", "08:00", None, Some("tomorrow")).unwrap_err().contains("date"));
        assert!(Schedule::parse("BYDAY=MO", "08:00", None, None).unwrap_err().contains("FREQ"));
        assert!(Schedule::parse(&"FREQ=DAILY;".repeat(100), "08:00", None, None).is_err());
        assert!(Schedule::parse("RRULE:FREQ=DAILY", "08:00", None, None).is_ok());
        // a finished rule is never due again
        let done = sched("FREQ=DAILY;UNTIL=20261101T000000Z", "08:00", Some("2026-10-01"));
        assert_eq!(done.next_after(berlin("2026-11-24 00:00")), None);
    }

    #[test]
    fn describes_in_plain_words() {
        assert_eq!(sched("FREQ=DAILY", "08:00", None).describe(), "Every day at 8:00 AM");
        assert_eq!(sched("FREQ=DAILY;INTERVAL=3", "20:15", None).describe(), "Every 3 days at 8:15 PM");
        assert_eq!(sched("FREQ=WEEKLY;BYDAY=MO,WE", "09:00", None).describe(), "Every Monday and Wednesday at 9:00 AM");
        assert_eq!(sched("FREQ=WEEKLY", "00:30", None).describe(), "Every week at 12:30 AM");
        assert_eq!(sched("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR,SA,SU", "12:00", None).describe(), "Every day at 12:00 PM");
        assert_eq!(sched("FREQ=MONTHLY;BYMONTHDAY=1,15", "08:00", None).describe(), "Every month on the 1st and 15th at 8:00 AM");
        assert_eq!(sched("FREQ=MONTHLY;BYDAY=2TU", "08:00", None).describe(), "Every month on the second Tuesday at 8:00 AM");
        assert_eq!(sched("FREQ=DAILY;COUNT=5", "08:00", None).describe(), "Every day at 8:00 AM, 5 times");
        assert_eq!(sched("FREQ=DAILY;UNTIL=20261231T000000Z", "08:00", None).describe(), "Every day at 8:00 AM, until Dec 31, 2026");
        assert_eq!(sched("FREQ=HOURLY", "08:00", None).describe(), "Every hour");
        assert_eq!(ordinal(22), "22nd");
        assert_eq!(ordinal(13), "13th");
        assert_eq!(days_in_month(2028, 2), 29);
    }
}
