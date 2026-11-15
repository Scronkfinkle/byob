//! Expanding recurring events (RRULE, RFC 5545 3.3.10) within a range.
//!
//! Covered: FREQ=DAILY/WEEKLY/MONTHLY/YEARLY with INTERVAL, COUNT, UNTIL, BYDAY (with
//! ordinals like `2TU`/`-1FR` for monthly and yearly rules), BYMONTHDAY (negative too),
//! BYMONTH, BYSETPOS and WKST; plus EXDATE, RDATE (date and date-time lists) and overridden
//! instances (RECURRENCE-ID) handled by the caller. Occurrences are generated on the event's
//! own wall clock (a weekly 09:00 in Europe/Berlin stays 09:00 across DST).
//!
//! Not covered (the event is shown once, at its start, with a note): FREQ=SECONDLY,
//! MINUTELY, HOURLY, BYYEARDAY, BYWEEKNO, and BYHOUR/BYMINUTE/BYSECOND (ignored: each
//! occurrence keeps DTSTART's time of day).

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};

use crate::time::{UserZone, When, month_len, month_start, weekday};

/// Most periods (days, weeks, months, years) walked for one rule.
const MAX_PERIODS: u32 = 40_000;
/// Most occurrences produced for one event in one range.
pub const MAX_OCCURRENCES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// A parsed RRULE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub freq: Freq,
    pub interval: u32,
    pub count: Option<u32>,
    pub until: Option<When>,
    /// (ordinal, weekday): `0` is every such weekday in the period.
    pub by_day: Vec<(i32, Weekday)>,
    pub by_month_day: Vec<i32>,
    pub by_month: Vec<u32>,
    pub by_set_pos: Vec<i32>,
    pub week_start: Weekday,
}

/// Why a rule can't be expanded.
pub type Unsupported = String;

pub fn parse_rule(v: &str) -> Result<Rule, Unsupported> {
    let mut rule = Rule {
        freq: Freq::Daily,
        interval: 1,
        count: None,
        until: None,
        by_day: vec![],
        by_month_day: vec![],
        by_month: vec![],
        by_set_pos: vec![],
        week_start: Weekday::Mon,
    };
    let mut freq = None;
    for part in v.trim().split(';').take(32) {
        let Some((k, val)) = part.split_once('=') else { continue };
        let list = || val.split(',').take(64).map(str::trim);
        match k.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = Some(match val.trim().to_ascii_uppercase().as_str() {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    other => return Err(format!("FREQ={other}")),
                })
            }
            "INTERVAL" => rule.interval = val.trim().parse::<u32>().unwrap_or(1).clamp(1, 1000),
            "COUNT" => rule.count = val.trim().parse().ok(),
            "UNTIL" => rule.until = crate::time::parse_value(val, None, None, &[]),
            "BYDAY" => {
                for d in list() {
                    let split = d.len().saturating_sub(2);
                    let (n, code) = (d.get(..split).unwrap_or_default(), d.get(split..).unwrap_or_default());
                    let wd = weekday(code).ok_or_else(|| format!("BYDAY={d}"))?;
                    let n: i32 = if n.is_empty() { 0 } else { n.trim_start_matches('+').parse().map_err(|_| format!("BYDAY={d}"))? };
                    rule.by_day.push((n.clamp(-53, 53), wd));
                }
            }
            "BYMONTHDAY" => rule.by_month_day = list().filter_map(|x| x.parse().ok()).filter(|n: &i32| *n != 0 && n.abs() <= 31).collect(),
            "BYMONTH" => rule.by_month = list().filter_map(|x| x.parse().ok()).filter(|n| (1..=12).contains(n)).collect(),
            "BYSETPOS" => rule.by_set_pos = list().filter_map(|x| x.parse().ok()).filter(|n: &i32| *n != 0 && n.abs() <= 366).collect(),
            "WKST" => rule.week_start = weekday(val.trim()).unwrap_or(Weekday::Mon),
            "BYYEARDAY" | "BYWEEKNO" => return Err(k.to_owned()),
            // BYHOUR/BYMINUTE/BYSECOND: each occurrence keeps DTSTART's time (see module docs)
            _ => {}
        }
    }
    rule.freq = freq.ok_or_else(|| "no FREQ".to_owned())?;
    Ok(rule)
}

/// The days of one period that match the rule, sorted.
fn period_days(rule: &Rule, start: NaiveDate, period: NaiveDate) -> Vec<NaiveDate> {
    let mut days: Vec<NaiveDate> = match rule.freq {
        Freq::Daily => vec![period],
        Freq::Weekly => {
            let wanted: Vec<Weekday> = if rule.by_day.is_empty() { vec![start.weekday()] } else { rule.by_day.iter().map(|(_, w)| *w).collect() };
            (0..7).filter_map(|i| period.checked_add_signed(Duration::days(i))).filter(|d| wanted.contains(&d.weekday())).collect()
        }
        Freq::Monthly => month_days(rule, start, period),
        Freq::Yearly => {
            let months: Vec<u32> = if rule.by_month.is_empty() { vec![start.month()] } else { rule.by_month.clone() };
            // BYDAY alone in a yearly rule counts weekdays through the whole year
            if rule.by_month.is_empty() && rule.by_month_day.is_empty() && !rule.by_day.is_empty() {
                year_days(rule, period.year())
            } else {
                months.iter().filter_map(|m| NaiveDate::from_ymd_opt(period.year(), *m, 1)).flat_map(|m| month_days(rule, start, m)).collect()
            }
        }
    };
    // BYMONTH limits the other frequencies; BYMONTHDAY/BYDAY limit DAILY.
    if !rule.by_month.is_empty() && rule.freq != Freq::Yearly {
        days.retain(|d| rule.by_month.contains(&d.month()));
    }
    if rule.freq == Freq::Daily {
        if !rule.by_month_day.is_empty() {
            days.retain(|d| month_day_matches(&rule.by_month_day, *d));
        }
        if !rule.by_day.is_empty() {
            days.retain(|d| rule.by_day.iter().any(|(_, w)| *w == d.weekday()));
        }
    }
    days.sort();
    days.dedup();
    if !rule.by_set_pos.is_empty() {
        let n = days.len();
        let picked: Vec<NaiveDate> = rule
            .by_set_pos
            .iter()
            .filter_map(|&p| {
                let i = if p > 0 { usize::try_from(p - 1).ok()? } else { n.checked_sub(usize::try_from(-p).ok()?)? };
                days.get(i).copied()
            })
            .collect();
        days = picked;
        days.sort();
        days.dedup();
    }
    days
}

fn month_day_matches(list: &[i32], d: NaiveDate) -> bool {
    let len = i32::try_from(month_len(d.year(), d.month())).unwrap_or(30);
    let day = i32::try_from(d.day()).unwrap_or(0);
    list.iter().any(|&n| if n > 0 { n == day } else { len + n + 1 == day })
}

/// The matching days of the month starting `first`. With neither BYDAY nor BYMONTHDAY,
/// DTSTART's day of month (months without it are skipped).
fn month_days(rule: &Rule, start: NaiveDate, first: NaiveDate) -> Vec<NaiveDate> {
    let len = month_len(first.year(), first.month());
    let all: Vec<NaiveDate> = (0..len).filter_map(|i| first.checked_add_signed(Duration::days(i64::from(i)))).collect();
    if rule.by_day.is_empty() && rule.by_month_day.is_empty() {
        return all.into_iter().filter(|d| d.day() == start.day()).collect();
    }
    let mut out = all;
    if !rule.by_month_day.is_empty() {
        out.retain(|d| month_day_matches(&rule.by_month_day, *d));
    }
    if !rule.by_day.is_empty() {
        out.retain(|d| {
            rule.by_day.iter().any(|&(n, w)| {
                if d.weekday() != w {
                    return false;
                }
                if n == 0 {
                    return true;
                }
                let nth = i32::try_from((d.day() - 1) / 7 + 1).unwrap_or(0);
                let from_end = i32::try_from((len - d.day()) / 7 + 1).unwrap_or(0);
                if n > 0 { nth == n } else { from_end == -n }
            })
        });
    }
    out
}

/// YEARLY;BYDAY=20MO (the nth weekday of the year), or every such weekday.
fn year_days(rule: &Rule, year: i32) -> Vec<NaiveDate> {
    let Some(first) = NaiveDate::from_ymd_opt(year, 1, 1) else { return vec![] };
    let days: Vec<NaiveDate> = (0..366).filter_map(|i| first.checked_add_signed(Duration::days(i))).filter(|d| d.year() == year).collect();
    let mut out = Vec::new();
    for &(n, w) in &rule.by_day {
        let of: Vec<NaiveDate> = days.iter().copied().filter(|d| d.weekday() == w).collect();
        let pick = match n {
            0 => {
                out.extend(of);
                continue;
            }
            n if n > 0 => usize::try_from(n - 1).ok().and_then(|i| of.get(i)),
            n => usize::try_from(-n).ok().and_then(|k| of.len().checked_sub(k)).and_then(|i| of.get(i)),
        };
        out.extend(pick.copied());
    }
    out
}

/// The first day of the period after `p` (stepping by the interval).
fn next_period(rule: &Rule, p: NaiveDate) -> Option<NaiveDate> {
    let n = rule.interval;
    match rule.freq {
        Freq::Daily => p.checked_add_signed(Duration::days(i64::from(n))),
        Freq::Weekly => p.checked_add_signed(Duration::weeks(i64::from(n))),
        Freq::Monthly => p.checked_add_months(chrono::Months::new(n)),
        Freq::Yearly => p.checked_add_months(chrono::Months::new(n.saturating_mul(12))),
    }
}

fn first_period(rule: &Rule, start: NaiveDate) -> NaiveDate {
    match rule.freq {
        Freq::Daily => start,
        Freq::Weekly => {
            let back = (7 + start.weekday().num_days_from_monday() - rule.week_start.num_days_from_monday()) % 7;
            start.checked_sub_signed(Duration::days(i64::from(back))).unwrap_or(start)
        }
        Freq::Monthly => month_start(start),
        Freq::Yearly => NaiveDate::from_ymd_opt(start.year(), 1, 1).unwrap_or(start),
    }
}

/// The result of expanding one rule.
#[derive(Debug, Default)]
pub struct Expanded {
    /// Occurrence starts (as `When`s in the event's own zone), in order.
    pub starts: Vec<When>,
    /// Stopped early (the period or occurrence caps).
    pub capped: bool,
}

/// Occurrence starts of `rule` from `start` whose [start, start+length) overlaps
/// [`from`, `to`). EXDATE and overrides are the caller's.
pub fn expand(rule: &Rule, start: When, length: Duration, from: DateTime<Utc>, to: DateTime<Utc>, user: &UserZone) -> Expanded {
    let mut out = Expanded::default();
    let time = start.naive().time();
    let until = rule.until.map(|u| u.instant(user));
    let start_at = start.instant(user);
    let mut period = first_period(rule, start.date());
    let mut seen: u32 = 0;
    for _ in 0..MAX_PERIODS {
        for day in period_days(rule, start.date(), period) {
            if day < start.date() {
                continue;
            }
            let when = start.with_naive(day.and_time(time));
            let at = when.instant(user);
            if at < start_at {
                continue;
            }
            if until.is_some_and(|u| at > u) || rule.count.is_some_and(|c| seen >= c) || at >= to {
                return out;
            }
            seen = seen.saturating_add(1);
            let end = at + length;
            if end > from || (length.is_zero() && at >= from) {
                if out.starts.len() >= MAX_OCCURRENCES {
                    out.capped = true;
                    return out;
                }
                out.starts.push(when);
            }
        }
        match next_period(rule, period) {
            Some(p) => period = p,
            None => return out,
        }
    }
    out.capped = true;
    out
}

/// A short account of a rule for the model: `weekly on Mon, Wed`, `every 2 weeks`, `monthly
/// (the 2nd Tue)`, `until 2026-12-31`, `5 times`.
pub fn describe(rule: &Rule) -> String {
    let unit = match rule.freq {
        Freq::Daily => "day",
        Freq::Weekly => "week",
        Freq::Monthly => "month",
        Freq::Yearly => "year",
    };
    let mut s = if rule.interval == 1 {
        match rule.freq {
            Freq::Daily => "daily".to_owned(),
            Freq::Weekly => "weekly".to_owned(),
            Freq::Monthly => "monthly".to_owned(),
            Freq::Yearly => "yearly".to_owned(),
        }
    } else {
        format!("every {} {unit}s", rule.interval)
    };
    if !rule.by_day.is_empty() {
        let days: Vec<String> = rule
            .by_day
            .iter()
            .map(|(n, w)| match n {
                0 => format!("{w}"),
                -1 => format!("last {w}"),
                n => format!("#{n} {w}"),
            })
            .collect();
        s.push_str(&format!(" on {}", days.join(", ")));
    }
    if !rule.by_month_day.is_empty() {
        let days: Vec<String> = rule.by_month_day.iter().map(i32::to_string).collect();
        s.push_str(&format!(" on day {}", days.join(", ")));
    }
    if let Some(u) = rule.until {
        s.push_str(&format!(" until {}", u.date()));
    }
    if let Some(c) = rule.count {
        s.push_str(&format!(", {c} times"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Zone;
    use chrono::{NaiveDateTime, TimeZone};

    fn user() -> UserZone {
        UserZone::Iana(chrono_tz::Europe::Berlin)
    }

    fn at(s: &str) -> When {
        When::At(NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), Zone::Iana(chrono_tz::Europe::Berlin))
    }

    fn utc(s: &str) -> DateTime<Utc> {
        Utc.from_utc_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
    }

    fn run(rule: &str, start: When, from: &str, to: &str) -> Vec<String> {
        let r = parse_rule(rule).unwrap();
        expand(&r, start, Duration::hours(1), utc(from), utc(to), &user()).starts.iter().map(|w| w.naive().format("%a %Y-%m-%d %H:%M").to_string()).collect()
    }

    #[test]
    fn weekly_keeps_wall_clock_time_across_dst() {
        let got = run("FREQ=WEEKLY;BYDAY=MO,WE", at("2026-10-19 09:00"), "2026-10-19 00:00", "2026-11-03 00:00");
        assert_eq!(got, ["Mon 2026-10-19 09:00", "Wed 2026-10-21 09:00", "Mon 2026-10-26 09:00", "Wed 2026-10-28 09:00", "Mon 2026-11-02 09:00"]);
        // 09:00 Berlin is 07:00 UTC in summer time, 08:00 after it ends (Oct 25).
        let r = parse_rule("FREQ=WEEKLY").unwrap();
        let e = expand(&r, at("2026-10-19 09:00"), Duration::hours(1), utc("2026-10-19 00:00"), utc("2026-10-27 00:00"), &user());
        let utcs: Vec<String> = e.starts.iter().map(|w| w.instant(&user()).format("%H:%M").to_string()).collect();
        assert_eq!(utcs, ["07:00", "08:00"]);
    }

    #[test]
    fn count_until_and_interval() {
        assert_eq!(run("FREQ=DAILY;COUNT=3", at("2026-11-01 10:00"), "2026-10-01 00:00", "2026-12-01 00:00").len(), 3);
        // COUNT counts from the start, not from the range
        assert_eq!(run("FREQ=DAILY;COUNT=3", at("2026-11-01 10:00"), "2026-11-02 12:00", "2026-12-01 00:00"), ["Tue 2026-11-03 10:00"]);
        assert_eq!(run("FREQ=DAILY;UNTIL=20261103T090000Z", at("2026-11-01 10:00"), "2026-10-01 00:00", "2026-12-01 00:00").len(), 3);
        assert_eq!(
            run("FREQ=WEEKLY;INTERVAL=2", at("2026-11-02 10:00"), "2026-11-01 00:00", "2026-12-01 00:00"),
            ["Mon 2026-11-02 10:00", "Mon 2026-11-16 10:00", "Mon 2026-11-30 10:00"]
        );
    }

    #[test]
    fn monthly_and_yearly_rules() {
        assert_eq!(
            run("FREQ=MONTHLY;BYDAY=2TU", at("2026-01-13 10:00"), "2026-03-01 00:00", "2026-05-01 00:00"),
            ["Tue 2026-03-10 10:00", "Tue 2026-04-14 10:00"]
        );
        assert_eq!(run("FREQ=MONTHLY;BYDAY=-1FR", at("2026-01-30 10:00"), "2026-02-01 00:00", "2026-03-01 00:00"), ["Fri 2026-02-27 10:00"]);
        assert_eq!(
            run("FREQ=MONTHLY;BYMONTHDAY=-1", at("2026-01-31 10:00"), "2026-02-01 00:00", "2026-04-01 00:00"),
            ["Sat 2026-02-28 10:00", "Tue 2026-03-31 10:00"]
        );
        // the 31st: months without one are skipped
        assert_eq!(run("FREQ=MONTHLY", at("2026-01-31 10:00"), "2026-02-01 00:00", "2026-04-01 00:00"), ["Tue 2026-03-31 10:00"]);
        // last weekday of the month
        assert_eq!(
            run("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1", at("2026-01-30 17:00"), "2026-05-01 00:00", "2026-06-01 00:00"),
            ["Fri 2026-05-29 17:00"]
        );
        assert_eq!(run("FREQ=YEARLY", at("2024-02-29 10:00"), "2025-01-01 00:00", "2029-01-01 00:00"), ["Tue 2028-02-29 10:00"]);
        assert_eq!(run("FREQ=YEARLY;BYMONTH=11;BYDAY=4TH", at("2026-11-26 12:00"), "2027-01-01 00:00", "2028-01-01 00:00"), ["Thu 2027-11-25 12:00"]);
    }

    #[test]
    fn all_day_and_unsupported_rules() {
        let d = When::Date(NaiveDate::from_ymd_opt(2026, 11, 16).unwrap());
        let r = parse_rule("FREQ=DAILY;COUNT=2").unwrap();
        let e = expand(&r, d, Duration::days(1), utc("2026-11-16 12:00"), utc("2026-11-30 00:00"), &user());
        assert_eq!(e.starts, [d, When::Date(NaiveDate::from_ymd_opt(2026, 11, 17).unwrap())]);
        assert!(parse_rule("FREQ=HOURLY").is_err());
        assert!(parse_rule("FREQ=YEARLY;BYWEEKNO=20").is_err());
        assert!(parse_rule("INTERVAL=2").is_err());
        assert!(parse_rule("FREQ=WEEKLY;BYDAY=XX").is_err());
        // a daily rule from long ago still reaches today, within the caps
        let old =
            expand(&parse_rule("FREQ=DAILY").unwrap(), at("1990-01-01 08:00"), Duration::hours(1), utc("2026-11-16 00:00"), utc("2026-11-17 00:00"), &user());
        assert_eq!(old.starts.len(), 1);
        assert_eq!(describe(&parse_rule("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,-1FR;COUNT=4").unwrap()), "every 2 weeks on Mon, last Fri, 4 times");
    }
}
