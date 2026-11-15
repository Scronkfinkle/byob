//! Dates and times: iCalendar values (UTC, `TZID` zoned, floating, all-day `DATE`), the
//! user's zone they are shown in, and the times the model writes.

use chrono::{DateTime, Datelike, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Utc, Weekday};
use chrono_tz::Tz;

use crate::ical::{Component, Prop};

/// The zone of an event's time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Utc,
    Iana(Tz),
    /// A `TZID` no IANA name matched: the offset its VTIMEZONE gives (no DST rules).
    Fixed(FixedOffset),
    /// No zone (floating): the user's own.
    Floating,
}

/// A DTSTART/DTEND/EXDATE/RECURRENCE-ID value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// All-day (a `DATE`): the day in whatever zone it's seen from.
    Date(NaiveDate),
    /// A wall-clock time in a zone.
    At(NaiveDateTime, Zone),
}

impl When {
    pub fn is_date(&self) -> bool {
        matches!(self, Self::Date(_))
    }

    pub fn date(&self) -> NaiveDate {
        match self {
            Self::Date(d) => *d,
            Self::At(t, _) => t.date(),
        }
    }

    /// The same kind of value at another wall-clock date-time (for recurrences).
    pub fn with_naive(&self, t: NaiveDateTime) -> Self {
        match self {
            Self::Date(_) => Self::Date(t.date()),
            Self::At(_, z) => Self::At(t, *z),
        }
    }

    pub fn naive(&self) -> NaiveDateTime {
        match self {
            Self::Date(d) => d.and_time(NaiveTime::MIN),
            Self::At(t, _) => *t,
        }
    }

    /// The instant, with all-day and floating values in the user's zone.
    pub fn instant(&self, user: &UserZone) -> DateTime<Utc> {
        match self {
            Self::Date(d) => user.resolve(d.and_time(NaiveTime::MIN)),
            Self::At(t, Zone::Utc) => Utc.from_utc_datetime(t),
            Self::At(t, Zone::Iana(tz)) => resolve_in(tz, *t),
            Self::At(t, Zone::Fixed(off)) => resolve_in(off, *t),
            Self::At(t, Zone::Floating) => user.resolve(*t),
        }
    }
}

/// A wall-clock time in `tz` as an instant: in a DST gap, the time an hour later; when
/// ambiguous, the earlier one.
fn resolve_in<Z: TimeZone>(tz: &Z, t: NaiveDateTime) -> DateTime<Utc> {
    match tz.from_local_datetime(&t) {
        LocalResult::Single(d) | LocalResult::Ambiguous(d, _) => d.with_timezone(&Utc),
        LocalResult::None => match tz.from_local_datetime(&(t + Duration::hours(1))) {
            LocalResult::Single(d) | LocalResult::Ambiguous(d, _) => d.with_timezone(&Utc),
            LocalResult::None => Utc.from_utc_datetime(&t),
        },
    }
}

/// The zone times are shown in and the model's times are read in: the `TIME_ZONE` setting,
/// else the system's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserZone {
    Iana(Tz),
    /// The system's zone, when it has no IANA name we know.
    System,
}

impl Default for UserZone {
    fn default() -> Self {
        Self::detect(None)
    }
}

impl UserZone {
    /// `name` (an IANA zone like `Europe/Berlin`), else `$TZ`, else the system zone.
    pub fn detect(name: Option<&str>) -> Self {
        let from_env = std::env::var("TZ").ok().map(|t| t.trim_start_matches(':').to_owned());
        let system = iana_time_zone::get_timezone().ok();
        [name.map(str::to_owned), from_env, system].into_iter().flatten().find_map(|n| n.trim().parse::<Tz>().ok()).map_or(Self::System, Self::Iana)
    }

    pub fn name(&self) -> String {
        match self {
            Self::Iana(tz) => tz.name().to_owned(),
            Self::System => "the system's zone".into(),
        }
    }

    pub fn resolve(&self, t: NaiveDateTime) -> DateTime<Utc> {
        match self {
            Self::Iana(tz) => resolve_in(tz, t),
            Self::System => resolve_in(&chrono::Local, t),
        }
    }

    /// An instant as the user's wall-clock time and offset.
    pub fn local(&self, t: DateTime<Utc>) -> DateTime<FixedOffset> {
        match self {
            Self::Iana(tz) => {
                let l = t.with_timezone(tz);
                l.with_timezone(&l.offset().fix())
            }
            Self::System => {
                let l = t.with_timezone(&chrono::Local);
                l.with_timezone(&l.offset().fix())
            }
        }
    }

    /// Midnight starting `d` in this zone.
    pub fn day_start(&self, d: NaiveDate) -> DateTime<Utc> {
        self.resolve(d.and_time(NaiveTime::MIN))
    }

    pub fn today(&self) -> NaiveDate {
        self.local(Utc::now()).date_naive()
    }
}

/// `Thu 2026-11-19 15:00 -06:00`.
pub fn show(t: DateTime<Utc>, user: &UserZone) -> String {
    user.local(t).format("%a %Y-%m-%d %H:%M %:z").to_string()
}

/// `Thu 2026-11-19`.
pub fn show_date(d: NaiveDate) -> String {
    d.format("%a %Y-%m-%d").to_string()
}

/// `15:00` when `b` is on `a`'s day (as the user sees it), else the whole time.
pub fn show_end(a: DateTime<Utc>, b: DateTime<Utc>, user: &UserZone) -> String {
    let (la, lb) = (user.local(a), user.local(b));
    if la.date_naive() == lb.date_naive() && la.offset() == lb.offset() { lb.format("%H:%M").to_string() } else { show(b, user) }
}

/// A time the model wrote. `2026-11-19T15:00`, `2026-11-19 15:00:00` (the user's zone),
/// with `Z` or an offset (`+01:00`), or a date `2026-11-19`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Date(NaiveDate),
    At(DateTime<Utc>),
}

impl Input {
    /// As an instant: a date is its midnight in the user's zone.
    pub fn start(&self, user: &UserZone) -> DateTime<Utc> {
        match self {
            Self::Date(d) => user.day_start(*d),
            Self::At(t) => *t,
        }
    }

    /// As the end of a range: a date includes that whole day.
    pub fn end(&self, user: &UserZone) -> DateTime<Utc> {
        match self {
            Self::Date(d) => user.day_start(d.succ_opt().unwrap_or(*d)),
            Self::At(t) => *t,
        }
    }

    pub fn date(&self, user: &UserZone) -> NaiveDate {
        match self {
            Self::Date(d) => *d,
            Self::At(t) => user.local(*t).date_naive(),
        }
    }
}

/// Read a time the model wrote (see [`Input`]); `what` names it in the error.
pub fn parse_input(s: &str, what: &str, user: &UserZone) -> Result<Input, String> {
    let s = s.trim();
    let bad = || format!("{what} {s:?} isn't a time I can read: write 2026-11-19T15:00 (your local time), 2026-11-19T15:00:00+01:00, or a date 2026-11-19.");
    if s.is_empty() || s.len() > 40 {
        return Err(bad());
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Input::Date(d));
    }
    let t = s.replacen(' ', "T", 1);
    if let Ok(d) = DateTime::parse_from_rfc3339(&t) {
        return Ok(Input::At(d.with_timezone(&Utc)));
    }
    for f in ["%Y-%m-%dT%H:%M%:z", "%Y-%m-%dT%H:%M%z", "%Y-%m-%dT%H:%M:%S%z"] {
        if let Ok(d) = DateTime::parse_from_str(&t, f) {
            return Ok(Input::At(d.with_timezone(&Utc)));
        }
    }
    let t = t.strip_suffix('Z').map_or_else(|| (t.clone(), false), |x| (x.to_owned(), true));
    for f in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(&t.0, f) {
            return Ok(Input::At(if t.1 { Utc.from_utc_datetime(&n) } else { user.resolve(n) }));
        }
    }
    Err(bad())
}

/// An iCalendar date or date-time property (`VALUE=DATE`, `...Z`, `TZID=...`, floating).
pub fn parse_when(p: &Prop, timezones: &[Component]) -> Option<When> {
    parse_value(&p.value, p.param("VALUE").as_deref(), p.param("TZID").as_deref(), timezones)
}

/// One value (EXDATE/RDATE carry comma-separated lists: see [`parse_list`]).
pub fn parse_value(v: &str, kind: Option<&str>, tzid: Option<&str>, timezones: &[Component]) -> Option<When> {
    let v = v.trim();
    if kind.is_some_and(|k| k.eq_ignore_ascii_case("DATE")) || (v.len() == 8 && v.bytes().all(|b| b.is_ascii_digit())) {
        return NaiveDate::parse_from_str(v.get(..8)?, "%Y%m%d").ok().map(When::Date);
    }
    let (body, utc) = match v.strip_suffix(['Z', 'z']) {
        Some(b) => (b, true),
        None => (v, false),
    };
    let t = NaiveDateTime::parse_from_str(body, "%Y%m%dT%H%M%S").ok().or_else(|| NaiveDateTime::parse_from_str(body, "%Y%m%dT%H%M").ok())?;
    let zone = if utc {
        Zone::Utc
    } else {
        match tzid {
            Some(id) => resolve_tzid(id, timezones),
            None => Zone::Floating,
        }
    };
    Some(When::At(t, zone))
}

/// Every value of a list property (EXDATE, RDATE), periods skipped.
pub fn parse_list(p: &Prop, timezones: &[Component]) -> Vec<When> {
    let kind = p.param("VALUE");
    let tzid = p.param("TZID");
    p.value.split(',').take(1000).filter(|v| !v.contains('/')).filter_map(|v| parse_value(v, kind.as_deref(), tzid.as_deref(), timezones)).collect()
}

/// Windows zone names some servers (Exchange, Outlook) put in TZID: the common ones.
const WINDOWS: &[(&str, &str)] = &[
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("Mountain Standard Time", "America/Denver"),
    ("Central Standard Time", "America/Chicago"),
    ("Eastern Standard Time", "America/New_York"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("GMT Standard Time", "Europe/London"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("FLE Standard Time", "Europe/Kiev"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("India Standard Time", "Asia/Kolkata"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
    ("UTC", "Etc/UTC"),
];

/// A TZID as a zone: an IANA name (also inside vendor prefixes like
/// `/mozilla.org/20050126_1/Europe/Berlin`), a common Windows name, else the standard offset
/// the object's VTIMEZONE gives, else floating.
pub fn resolve_tzid(id: &str, timezones: &[Component]) -> Zone {
    let id = id.trim().trim_matches('"');
    if let Ok(tz) = id.parse::<Tz>() {
        return Zone::Iana(tz);
    }
    let parts: Vec<&str> = id.split('/').filter(|p| !p.is_empty()).collect();
    for n in (1..parts.len()).rev() {
        if let Some(tail) = parts.get(parts.len() - n..)
            && let Ok(tz) = tail.join("/").parse::<Tz>()
        {
            return Zone::Iana(tz);
        }
    }
    if let Some((_, iana)) = WINDOWS.iter().find(|(w, _)| w.eq_ignore_ascii_case(id))
        && let Ok(tz) = iana.parse::<Tz>()
    {
        return Zone::Iana(tz);
    }
    let vtz = timezones.iter().find(|c| c.text("TZID").is_some_and(|t| t.trim() == id));
    let offset = vtz
        .and_then(|v| v.children_named("STANDARD").next().or_else(|| v.children.first()))
        .and_then(|s| s.prop("TZOFFSETTO"))
        .and_then(|p| parse_offset(&p.value));
    offset.map_or(Zone::Floating, Zone::Fixed)
}

/// `+0100`, `-0530`, `+013000`.
fn parse_offset(v: &str) -> Option<FixedOffset> {
    let v = v.trim();
    let sign = match v.get(..1)? {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let h: i32 = v.get(1..3)?.parse().ok()?;
    let m: i32 = v.get(3..5)?.parse().ok()?;
    let s: i32 = v.get(5..7).and_then(|s| s.parse().ok()).unwrap_or(0);
    FixedOffset::east_opt(sign * (h * 3600 + m * 60 + s))
}

/// An iCalendar DURATION (`PT1H30M`, `P1D`, `-PT15M`, `P2W`).
pub fn parse_duration(v: &str) -> Option<Duration> {
    let v = v.trim();
    let (neg, v) = match v.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, v.strip_prefix('+').unwrap_or(v)),
    };
    let v = v.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut num = String::new();
    let mut in_time = false;
    for c in v.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            u => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                let n = n.min(100_000);
                total += match (u, in_time) {
                    ('W', false) => Duration::weeks(n),
                    ('D', false) => Duration::days(n),
                    ('H', true) => Duration::hours(n),
                    ('M', true) => Duration::minutes(n),
                    ('S', true) => Duration::seconds(n),
                    _ => return None,
                };
            }
        }
    }
    Some(if neg { -total } else { total })
}

/// `20261119T150000Z`.
pub fn ics_utc(t: DateTime<Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

/// `20261119`.
pub fn ics_date(d: NaiveDate) -> String {
    d.format("%Y%m%d").to_string()
}

pub fn weekday(code: &str) -> Option<Weekday> {
    Some(match code.to_ascii_uppercase().as_str() {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

/// Days in `d`'s month.
pub fn month_len(year: i32, month: u32) -> u32 {
    let next = if month == 12 { NaiveDate::from_ymd_opt(year.saturating_add(1), 1, 1) } else { NaiveDate::from_ymd_opt(year, month + 1, 1) };
    match (next, NaiveDate::from_ymd_opt(year, month, 1)) {
        (Some(n), Some(f)) => u32::try_from((n - f).num_days()).unwrap_or(30),
        _ => 30,
    }
}

/// The first day of `d`'s month.
pub fn month_start(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap_or(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn berlin() -> UserZone {
        UserZone::Iana(chrono_tz::Europe::Berlin)
    }

    #[test]
    fn reads_model_times_in_the_users_zone() {
        let u = berlin();
        let at = |s: &str| match parse_input(s, "start", &u).unwrap() {
            Input::At(t) => t.to_rfc3339(),
            Input::Date(d) => d.to_string(),
        };
        assert_eq!(at("2026-11-19T15:00"), "2026-11-19T14:00:00+00:00");
        assert_eq!(at("2026-11-19 15:00:00"), "2026-11-19T14:00:00+00:00");
        assert_eq!(at("2026-07-01T15:00"), "2026-07-01T13:00:00+00:00", "summer time");
        assert_eq!(at("2026-11-19T15:00:00Z"), "2026-11-19T15:00:00+00:00");
        assert_eq!(at("2026-11-19T15:00Z"), "2026-11-19T15:00:00+00:00");
        assert_eq!(at("2026-11-19T15:00-05:00"), "2026-11-19T20:00:00+00:00");
        assert_eq!(at("2026-11-19"), "2026-11-19");
        assert!(parse_input("next thursday", "start", &u).unwrap_err().contains("isn't a time"));
        let d = parse_input("2026-11-19", "to", &u).unwrap();
        assert_eq!(d.end(&u).to_rfc3339(), "2026-11-19T23:00:00+00:00", "a date as an end covers that day");
        // a DST gap moves on an hour
        assert_eq!(at("2026-03-29T02:30"), "2026-03-29T01:30:00+00:00");
    }

    #[test]
    fn reads_ics_values_and_zones() {
        let vtz = crate::ical::parse("BEGIN:VTIMEZONE\nTZID:Custom Zone\nBEGIN:STANDARD\nTZOFFSETTO:+0530\nEND:STANDARD\nEND:VTIMEZONE\n");
        let w = |v: &str, tzid: Option<&str>| parse_value(v, None, tzid, &vtz).unwrap();
        assert_eq!(w("20261119", None), When::Date(NaiveDate::from_ymd_opt(2026, 11, 19).unwrap()));
        let u = berlin();
        assert_eq!(w("20261119T150000Z", None).instant(&u).to_rfc3339(), "2026-11-19T15:00:00+00:00");
        assert_eq!(w("20261119T150000", Some("America/New_York")).instant(&u).to_rfc3339(), "2026-11-19T20:00:00+00:00");
        assert_eq!(w("20261119T150000", Some("/mozilla.org/20050126_1/America/New_York")).instant(&u).to_rfc3339(), "2026-11-19T20:00:00+00:00");
        assert_eq!(w("20261119T150000", Some("Eastern Standard Time")).instant(&u).to_rfc3339(), "2026-11-19T20:00:00+00:00");
        assert_eq!(w("20261119T150000", Some("Custom Zone")).instant(&u).to_rfc3339(), "2026-11-19T09:30:00+00:00");
        assert_eq!(w("20261119T150000", Some("Nowhere/Special")).instant(&u).to_rfc3339(), "2026-11-19T14:00:00+00:00", "floating");
        assert_eq!(w("20261119T150000", None).instant(&u).to_rfc3339(), "2026-11-19T14:00:00+00:00");
        assert!(parse_value("2026-11-19", None, None, &[]).is_none());
        assert_eq!(parse_duration("PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_duration("P1DT2H"), Some(Duration::hours(26)));
        assert_eq!(parse_duration("-PT15M"), Some(Duration::minutes(-15)));
        assert_eq!(parse_duration("P2W"), Some(Duration::weeks(2)));
        assert_eq!(parse_duration("PX"), None);
        assert_eq!(parse_offset("-0530"), FixedOffset::east_opt(-(5 * 3600 + 30 * 60)));
    }

    #[test]
    fn shows_times_with_the_offset() {
        let u = berlin();
        let t = Utc.with_ymd_and_hms(2026, 11, 19, 14, 0, 0).unwrap();
        assert_eq!(show(t, &u), "Thu 2026-11-19 15:00 +01:00");
        assert_eq!(show_end(t, t + Duration::hours(1), &u), "16:00");
        assert_eq!(show_end(t, t + Duration::days(1), &u), "Fri 2026-11-20 15:00 +01:00");
        assert_eq!(month_len(2028, 2), 29);
        assert_eq!(month_len(2026, 12), 31);
    }
}
