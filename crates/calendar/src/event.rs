//! Events: what a calendar object resource's VEVENTs say, their occurrences in a range
//! (recurrences expanded, EXDATEs and overridden instances applied), and new or changed
//! VEVENTs to write back.

use chrono::{DateTime, Duration, NaiveDate, Utc};

use crate::ical::{Component, Prop};
use crate::recur::{self, Rule};
use crate::time::{self, Input, UserZone, When, Zone};

/// Most characters of a title, place or attendee name shown to the model.
pub const MAX_FIELD_CHARS: usize = 200;
/// Most characters of a description shown.
pub const MAX_NOTE_CHARS: usize = 600;
/// Most attendees listed by name.
pub const MAX_ATTENDEES_SHOWN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attendee {
    pub name: Option<String>,
    /// Without `mailto:`.
    pub email: String,
    /// PARTSTAT (ACCEPTED, DECLINED, TENTATIVE, NEEDS-ACTION).
    pub status: Option<String>,
}

impl Attendee {
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) if !n.trim().is_empty() => n.clone(),
            _ => self.email.clone(),
        }
    }
}

/// One VEVENT.
#[derive(Debug, Clone)]
pub struct Event {
    pub uid: String,
    pub summary: String,
    pub description: String,
    pub location: String,
    pub start: When,
    /// Exclusive (an all-day event's DTEND is the day after its last).
    pub end: When,
    pub rule: Option<Result<Rule, String>>,
    pub exdates: Vec<When>,
    pub rdates: Vec<When>,
    pub recurrence_id: Option<When>,
    pub attendees: Vec<Attendee>,
    pub organizer: Option<String>,
    pub cancelled: bool,
    /// TRANSP:TRANSPARENT (doesn't block time); `None` when not said.
    pub transparent: Option<bool>,
}

impl Event {
    /// A VEVENT's fields; `None` without a usable DTSTART.
    pub fn from_component(c: &Component, timezones: &[Component]) -> Option<Self> {
        let start = time::parse_when(c.prop("DTSTART")?, timezones)?;
        let end = c
            .prop("DTEND")
            .and_then(|p| time::parse_when(p, timezones))
            .or_else(|| {
                let d = time::parse_duration(&c.prop("DURATION")?.value)?;
                Some(add(start, d))
            })
            .filter(|e| e.naive() >= start.naive() || e.is_date() != start.is_date())
            .unwrap_or_else(|| if start.is_date() { add(start, Duration::days(1)) } else { start });
        let attendees = c
            .props_named("ATTENDEE")
            .take(500)
            .map(|p| Attendee {
                name: p.param("CN").map(|n| clip(&n, MAX_FIELD_CHARS)),
                email: clip(strip_mailto(&p.value), MAX_FIELD_CHARS),
                status: p.param("PARTSTAT").map(|s| s.to_ascii_uppercase()),
            })
            .collect();
        let organizer = c.prop("ORGANIZER").map(|p| p.param("CN").unwrap_or_else(|| strip_mailto(&p.value).to_owned())).map(|o| clip(&o, MAX_FIELD_CHARS));
        Some(Self {
            uid: c.text("UID").unwrap_or_default(),
            summary: c.text("SUMMARY").unwrap_or_default(),
            description: c.text("DESCRIPTION").unwrap_or_default(),
            location: c.text("LOCATION").unwrap_or_default(),
            start,
            end,
            rule: c.prop("RRULE").map(|p| recur::parse_rule(&p.value)),
            exdates: c.props_named("EXDATE").flat_map(|p| time::parse_list(p, timezones)).take(2000).collect(),
            rdates: c.props_named("RDATE").flat_map(|p| time::parse_list(p, timezones)).take(2000).collect(),
            recurrence_id: c.prop("RECURRENCE-ID").and_then(|p| time::parse_when(p, timezones)),
            attendees,
            organizer,
            cancelled: c.text("STATUS").is_some_and(|s| s.trim().eq_ignore_ascii_case("CANCELLED")),
            transparent: c.text("TRANSP").map(|t| t.trim().eq_ignore_ascii_case("TRANSPARENT")),
        })
    }

    pub fn all_day(&self) -> bool {
        self.start.is_date()
    }

    /// Blocks time for free/busy: not cancelled, not transparent; all-day events only when
    /// marked busy (TRANSP:OPAQUE), as most are birthdays, holidays or reminders.
    pub fn busy(&self) -> bool {
        !self.cancelled && if self.all_day() { self.transparent == Some(false) } else { self.transparent != Some(true) }
    }

    fn length(&self, user: &UserZone) -> Duration {
        match (self.start, self.end) {
            (When::Date(a), When::Date(b)) => b - a,
            _ => self.end.instant(user) - self.start.instant(user),
        }
    }
}

fn add(w: When, d: Duration) -> When {
    match w {
        When::Date(day) => When::Date(day.checked_add_signed(Duration::days(d.num_days().max(1))).unwrap_or(day)),
        When::At(t, z) => When::At(t.checked_add_signed(d).unwrap_or(t), z),
    }
}

fn strip_mailto(v: &str) -> &str {
    let v = v.trim();
    match v.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("mailto:") => v.get(7..).unwrap_or_default(),
        _ => v,
    }
}

/// One line, at most `max` characters, control characters gone.
pub fn clip(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max { flat } else { format!("{}…", flat.chars().take(max).collect::<String>()) }
}

/// The VEVENTs of a calendar object (a VCALENDAR), with its VTIMEZONEs for TZIDs.
pub fn events_of(object: &[Component]) -> Vec<Event> {
    let mut out = Vec::new();
    for cal in object {
        let zones: Vec<Component> = cal.children_named("VTIMEZONE").cloned().collect();
        for c in cal.children_named("VEVENT").take(1000) {
            if let Some(e) = Event::from_component(c, &zones) {
                out.push(e);
            }
        }
    }
    out
}

/// An event happening once in the asked range.
#[derive(Debug, Clone)]
pub struct Occurrence {
    /// Index into the event list it came from.
    pub event: usize,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// For all-day ones: the first day and the last (inclusive).
    pub days: Option<(NaiveDate, NaiveDate)>,
    /// A note for the model: "repeats weekly on Mon", "changed instance of a repeating event".
    pub recurrence: Option<String>,
}

/// Every occurrence of `events` (one resource's) overlapping [`from`, `to`), sorted.
pub fn occurrences(events: &[Event], from: DateTime<Utc>, to: DateTime<Utc>, user: &UserZone) -> (Vec<Occurrence>, bool) {
    let mut out = Vec::new();
    let mut capped = false;
    let overrides: Vec<(usize, DateTime<Utc>)> = events.iter().enumerate().filter_map(|(i, e)| Some((i, e.recurrence_id?.instant(user)))).collect();
    for (i, e) in events.iter().enumerate() {
        let length = e.length(user);
        let occ = |start: When, recurrence: Option<String>| {
            let s = start.instant(user);
            let (end, days) = match start {
                When::Date(d) => {
                    let last = d.checked_add_signed(length - Duration::days(1)).unwrap_or(d).max(d);
                    (user.day_start(last.succ_opt().unwrap_or(last)), Some((d, last)))
                }
                When::At(..) => (s + length, None),
            };
            Occurrence { event: i, start: s, end, days, recurrence }
        };
        let overlaps = |o: &Occurrence| if o.end == o.start { o.start >= from && o.start < to } else { o.start < to && o.end > from };
        if e.recurrence_id.is_some() {
            let o = occ(e.start, Some("changed instance of a repeating event".into()));
            if overlaps(&o) {
                out.push(o);
            }
            continue;
        }
        let excluded = |w: &When| {
            let at = w.instant(user);
            e.exdates.iter().any(|x| x.instant(user) == at || (x.is_date() && x.date() == w.date()))
                || overrides.iter().any(|(j, r)| *j != i && events.get(*j).is_some_and(|o| o.uid == e.uid) && *r == at)
        };
        match &e.rule {
            None if e.rdates.is_empty() => {
                let o = occ(e.start, None);
                if overlaps(&o) {
                    out.push(o);
                }
            }
            None | Some(Ok(_)) => {
                let (starts, note) = match &e.rule {
                    Some(Ok(rule)) => {
                        let x = recur::expand(rule, e.start, length, from, to, user);
                        capped |= x.capped;
                        (x.starts, format!("repeats {}", recur::describe(rule)))
                    }
                    _ => (vec![e.start], "repeats on extra dates".to_owned()),
                };
                let extra = e.rdates.iter().map(|r| match (e.start, r) {
                    (When::At(t, z), When::Date(d)) => When::At(d.and_time(t.time()), z),
                    (_, r) => *r,
                });
                let mut all: Vec<When> = starts.into_iter().chain(extra).collect();
                all.sort_by_key(|w| w.instant(user));
                all.dedup_by_key(|w| w.instant(user));
                for w in all {
                    if excluded(&w) {
                        continue;
                    }
                    let o = occ(w, Some(note.clone()));
                    if overlaps(&o) {
                        out.push(o);
                    }
                }
            }
            Some(Err(why)) => {
                let o = occ(e.start, Some(format!("repeats by a rule byob can't expand ({why}): only its first time is shown")));
                if overlaps(&o) {
                    out.push(o);
                }
            }
        }
    }
    out.sort_by_key(|o| o.start);
    (out, capped)
}

/// What a create or update sets. `None`: leave as is.
#[derive(Debug, Clone, Default)]
pub struct Fields {
    pub title: Option<String>,
    pub start: Option<Input>,
    pub end: Option<Input>,
    pub duration: Option<Duration>,
    pub all_day: Option<bool>,
    pub location: Option<String>,
    pub description: Option<String>,
    /// (name, email).
    pub attendees: Option<Vec<(Option<String>, String)>>,
}

/// Parse `Name <a@b.c>`, `a@b.c` or `mailto:a@b.c`.
pub fn parse_attendee(s: &str) -> Result<(Option<String>, String), String> {
    let s = s.trim();
    let (name, email) = match (s.rfind('<'), s.ends_with('>')) {
        (Some(i), true) => (Some(s.get(..i).unwrap_or_default().trim().trim_matches('"').to_owned()), s.get(i + 1..s.len() - 1).unwrap_or_default()),
        _ => (None, s),
    };
    let email = strip_mailto(email).trim();
    let ok = email.len() <= 254
        && email.split_once('@').is_some_and(|(a, b)| !a.is_empty() && b.contains('.') && !b.starts_with('.'))
        && !email.contains(|c: char| c.is_whitespace() || c.is_control() || "<>,;:\"".contains(c));
    if !ok {
        return Err(format!("{s:?} isn't an email address (write a@example.com or Name <a@example.com>)"));
    }
    Ok((name.filter(|n| !n.is_empty()).map(|n| clip(&n, MAX_FIELD_CHARS)), email.to_owned()))
}

/// The times an event will have once `f` applies to (`old_start`, `old_end`), checked.
fn new_times(f: &Fields, old: Option<(When, When)>, user: &UserZone) -> Result<(When, When), String> {
    let was_all_day = old.is_some_and(|(s, _)| s.is_date());
    let all_day = f.all_day.unwrap_or_else(|| f.start.map_or(was_all_day, |s| matches!(s, Input::Date(_))));
    let start = match (f.start, old) {
        (Some(s), _) if all_day => When::Date(s.date(user)),
        (Some(Input::Date(d)), _) => When::At(user.day_start(d).naive_utc(), Zone::Utc),
        (Some(Input::At(t)), _) => When::At(t.naive_utc(), Zone::Utc),
        (None, Some((s, _))) if all_day => When::Date(match s {
            When::Date(d) => d,
            When::At(..) => user.local(s.instant(user)).date_naive(),
        }),
        (None, Some((When::Date(d), _))) => When::At(user.resolve(d.and_hms_opt(9, 0, 0).unwrap_or_default()).naive_utc(), Zone::Utc),
        (None, Some((s, _))) => s,
        (None, None) => return Err("Give the event's start (start).".into()),
    };
    let end = if all_day {
        let first = start.date();
        let last = match (f.end, f.duration, old) {
            (Some(e), _, _) => e.date(user),
            (None, Some(d), _) => first.checked_add_signed(Duration::days((d.num_days()).max(1) - 1)).unwrap_or(first),
            (None, None, Some((When::Date(s), When::Date(e)))) if f.start.is_some() => first.checked_add_signed(e - s - Duration::days(1)).unwrap_or(first),
            (None, None, Some((_, When::Date(e)))) => e.pred_opt().unwrap_or(e),
            _ => first,
        };
        if last < first {
            return Err("The end is before the start.".into());
        }
        When::Date(last.succ_opt().unwrap_or(last))
    } else {
        let s = start.instant(user);
        let e = match (f.end, f.duration, old) {
            (Some(e), _, _) => e.start(user),
            (None, Some(d), _) => s + d,
            (None, None, Some((os, oe))) if !os.is_date() && !oe.is_date() => s + (oe.instant(user) - os.instant(user)),
            _ => s + Duration::hours(1),
        };
        if e < s {
            return Err("The end is before the start.".into());
        }
        if e - s > Duration::days(366) {
            return Err("That event would be longer than a year; check the end.".into());
        }
        When::At(e.naive_utc(), Zone::Utc)
    };
    Ok((start, end))
}

/// A time as a property: dates as `VALUE=DATE`; times in `keep` (the zone and TZID the
/// event had) if it had an IANA one, else UTC.
fn time_prop(name: &str, w: When, keep: Option<(&str, Zone)>, user: &UserZone) -> Prop {
    match w {
        When::Date(d) => Prop::new(name, time::ics_date(d)).with_param("VALUE", "DATE"),
        When::At(..) => {
            let at = w.instant(user);
            match keep {
                Some((id, Zone::Iana(tz))) => Prop::new(name, at.with_timezone(&tz).format("%Y%m%dT%H%M%S").to_string()).with_param("TZID", id),
                _ => Prop::new(name, time::ics_utc(at)),
            }
        }
    }
}

fn set_text(ev: &mut Component, name: &str, v: Option<&String>) {
    match v.map(|s| s.trim()) {
        Some("") => ev.remove(name),
        Some(s) => ev.set(Prop::text(name, s)),
        None => {}
    }
}

/// Apply `f` to a VEVENT (the master of its resource), bumping SEQUENCE and the stamps.
/// `organizer`: the user's address, written when attendees are set and none is there.
pub fn apply(ev: &mut Component, f: &Fields, timezones: &[Component], organizer: Option<&str>, user: &UserZone, now: DateTime<Utc>) -> Result<(), String> {
    let old = Event::from_component(ev, timezones).map(|e| (e.start, e.end));
    let keep = ev
        .prop("DTSTART")
        .and_then(|p| Some((p.param("TZID")?, p.clone())))
        .map(|(id, p)| (id.clone(), time::parse_when(&p, timezones).map_or(Zone::Floating, |w| if let When::At(_, z) = w { z } else { Zone::Floating })));
    if f.start.is_some() || f.end.is_some() || f.duration.is_some() || f.all_day.is_some() || old.is_none() {
        let (s, e) = new_times(f, old, user)?;
        let keep = keep.as_ref().map(|(id, z)| (id.as_str(), *z));
        ev.set(time_prop("DTSTART", s, keep, user));
        ev.remove("DURATION");
        ev.set(time_prop("DTEND", e, keep, user));
    }
    if let Some(t) = &f.title {
        if t.trim().is_empty() {
            return Err("The title can't be empty.".into());
        }
        ev.set(Prop::text("SUMMARY", t.trim()));
    }
    set_text(ev, "LOCATION", f.location.as_ref());
    set_text(ev, "DESCRIPTION", f.description.as_ref());
    if let Some(list) = &f.attendees {
        ev.remove("ATTENDEE");
        for (name, email) in list {
            let mut p = Prop::new("ATTENDEE", format!("mailto:{email}"));
            if let Some(n) = name {
                p = p.with_param("CN", n);
            }
            ev.props.push(p.with_param("ROLE", "REQ-PARTICIPANT").with_param("PARTSTAT", "NEEDS-ACTION").with_param("RSVP", "TRUE"));
        }
        if !list.is_empty()
            && ev.prop("ORGANIZER").is_none()
            && let Some(o) = organizer
        {
            ev.set(Prop::new("ORGANIZER", format!("mailto:{o}")));
        }
    }
    let seq = ev.prop("SEQUENCE").and_then(|p| p.value.trim().parse::<u32>().ok());
    if let Some(s) = seq {
        ev.set(Prop::new("SEQUENCE", s.saturating_add(1).to_string()));
    } else if old.is_some() {
        ev.set(Prop::new("SEQUENCE", "1"));
    }
    ev.set(Prop::new("DTSTAMP", time::ics_utc(now)));
    ev.set(Prop::new("LAST-MODIFIED", time::ics_utc(now)));
    Ok(())
}

/// A new calendar object holding one event.
pub fn new_object(uid: &str, f: &Fields, organizer: Option<&str>, user: &UserZone, now: DateTime<Utc>) -> Result<Component, String> {
    if f.title.as_deref().is_none_or(|t| t.trim().is_empty()) {
        return Err("Give the event a title (title).".into());
    }
    let mut ev = Component::new("VEVENT");
    ev.set(Prop::text("UID", uid));
    ev.set(Prop::new("DTSTAMP", time::ics_utc(now)));
    ev.set(Prop::new("CREATED", time::ics_utc(now)));
    apply(&mut ev, f, &[], organizer, user, now)?;
    ev.remove("SEQUENCE");
    let mut cal = Component::new("VCALENDAR");
    cal.set(Prop::new("VERSION", "2.0"));
    cal.set(Prop::new("PRODID", "-//byob//calendar//EN"));
    cal.children.push(ev);
    Ok(cal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ical::parse;
    use chrono::TimeZone;

    fn user() -> UserZone {
        UserZone::Iana(chrono_tz::Europe::Berlin)
    }

    fn utc(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn list(ics: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<String> {
        let events = events_of(&parse(ics));
        let (occ, _) = occurrences(&events, from, to, &user());
        occ.iter()
            .map(|o| {
                let e = &events[o.event];
                match o.days {
                    Some((a, b)) => format!("{} {}..{}", e.summary, a, b),
                    None => format!("{} {} {}", e.summary, time::show(o.start, &user()), time::show_end(o.start, o.end, &user())),
                }
            })
            .collect()
    }

    const WRAP: (&str, &str) = ("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n", "END:VCALENDAR\r\n");

    fn cal(body: &str) -> String {
        format!("{}{body}{}", WRAP.0, WRAP.1)
    }

    #[test]
    fn tzid_utc_and_floating_times() {
        let ics = cal(
            "BEGIN:VTIMEZONE\nTZID:America/New_York\nEND:VTIMEZONE\nBEGIN:VEVENT\nUID:a\nSUMMARY:NY call\nDTSTART;TZID=America/New_York:20261119T100000\nDTEND;TZID=America/New_York:20261119T110000\nEND:VEVENT\nBEGIN:VEVENT\nUID:b\nSUMMARY:UTC sync\nDTSTART:20261119T080000Z\nDURATION:PT30M\nEND:VEVENT\nBEGIN:VEVENT\nUID:c\nSUMMARY:Floating\nDTSTART:20261119T120000\nEND:VEVENT\n",
        );
        assert_eq!(
            list(&ics, utc(2026, 11, 19, 0), utc(2026, 11, 20, 0)),
            ["UTC sync Thu 2026-11-19 09:00 +01:00 09:30", "Floating Thu 2026-11-19 12:00 +01:00 12:00", "NY call Thu 2026-11-19 16:00 +01:00 17:00"]
        );
    }

    #[test]
    fn all_day_and_multi_day_events() {
        let ics = cal(
            "BEGIN:VEVENT\nUID:a\nSUMMARY:Holiday\nDTSTART;VALUE=DATE:20261119\nDTEND;VALUE=DATE:20261122\nEND:VEVENT\nBEGIN:VEVENT\nUID:b\nSUMMARY:Birthday\nDTSTART;VALUE=DATE:20261120\nEND:VEVENT\nBEGIN:VEVENT\nUID:c\nSUMMARY:Conference\nDTSTART:20261121T080000Z\nDTEND:20261123T160000Z\nEND:VEVENT\n",
        );
        assert_eq!(
            list(&ics, utc(2026, 11, 20, 0), utc(2026, 11, 22, 0)),
            ["Holiday 2026-11-19..2026-11-21", "Birthday 2026-11-20..2026-11-20", "Conference Sat 2026-11-21 09:00 +01:00 Mon 2026-11-23 17:00 +01:00"]
        );
        let events = events_of(&parse(&ics));
        assert!(!events[0].busy() && !events[1].busy() && events[2].busy(), "all-day events don't block time unless marked busy");
        // a day the holiday doesn't reach (midnight Berlin, Nov 22) shows only the conference
        assert_eq!(list(&ics, utc(2026, 11, 21, 23), utc(2026, 11, 22, 12)), ["Conference Sat 2026-11-21 09:00 +01:00 Mon 2026-11-23 17:00 +01:00"]);
    }

    #[test]
    fn weekly_with_exdate_and_a_moved_instance() {
        let ics = cal(
            "BEGIN:VEVENT\nUID:standup\nSUMMARY:Standup\nDTSTART;TZID=Europe/Berlin:20261102T093000\nDTEND;TZID=Europe/Berlin:20261102T094500\nRRULE:FREQ=WEEKLY;BYDAY=MO,TH\nEXDATE;TZID=Europe/Berlin:20261116T093000\nEND:VEVENT\n\
             BEGIN:VEVENT\nUID:standup\nSUMMARY:Standup (moved)\nRECURRENCE-ID;TZID=Europe/Berlin:20261119T093000\nDTSTART;TZID=Europe/Berlin:20261119T140000\nDTEND;TZID=Europe/Berlin:20261119T141500\nEND:VEVENT\n",
        );
        assert_eq!(
            list(&ics, utc(2026, 11, 16, 0), utc(2026, 11, 24, 0)),
            ["Standup (moved) Thu 2026-11-19 14:00 +01:00 14:15", "Standup Mon 2026-11-23 09:30 +01:00 09:45"]
        );
        let events = events_of(&parse(&ics));
        let (occ, _) = occurrences(&events, utc(2026, 11, 23, 0), utc(2026, 11, 24, 0), &user());
        assert_eq!(occ[0].recurrence.as_deref(), Some("repeats weekly on Mon, Thu"));
    }

    #[test]
    fn unsupported_rules_show_the_first_time_with_a_note() {
        let ics = cal("BEGIN:VEVENT\nUID:h\nSUMMARY:Pings\nDTSTART:20261119T080000Z\nRRULE:FREQ=HOURLY\nEND:VEVENT\n");
        let events = events_of(&parse(&ics));
        let (occ, _) = occurrences(&events, utc(2026, 11, 19, 0), utc(2026, 11, 20, 0), &user());
        assert_eq!(occ.len(), 1);
        assert!(occ[0].recurrence.as_deref().unwrap().contains("can't expand (FREQ=HOURLY)"));
    }

    #[test]
    fn creates_and_updates_events() {
        let u = user();
        let now = utc(2026, 11, 15, 12);
        let f = Fields {
            title: Some("Dentist".into()),
            start: Some(time::parse_input("2026-11-19T15:00", "start", &u).unwrap()),
            duration: Some(Duration::minutes(45)),
            location: Some("Main St; 4".into()),
            attendees: Some(vec![parse_attendee("Dr. Who <who@example.com>").unwrap()]),
            ..Fields::default()
        };
        let cal = new_object("uid-1", &f, Some("me@example.com"), &u, now).unwrap();
        let text = cal.to_ics();
        assert!(text.contains("DTSTART:20261119T140000Z\r\nDTEND:20261119T144500Z\r\n"), "{text}");
        assert!(text.contains("LOCATION:Main St\\; 4\r\n") && text.contains("ORGANIZER:mailto:me@example.com"), "{text}");
        assert!(
            text.replace("\r\n ", "").contains("ATTENDEE;CN=Dr. Who;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:who@example.com"),
            "{text}"
        );
        // move it an hour later: the length stays
        let mut ev = parse(&text)[0].children[0].clone();
        let later = Fields { start: Some(time::parse_input("2026-11-19T16:00", "start", &u).unwrap()), ..Fields::default() };
        apply(&mut ev, &later, &[], None, &u, now).unwrap();
        assert_eq!(ev.prop("DTEND").unwrap().value, "20261119T154500Z");
        assert_eq!(ev.prop("SEQUENCE").unwrap().value, "1");
        // a zoned event keeps its TZID
        let zoned = parse(&cal_text_zoned())[0].children[0].clone();
        let mut z = zoned.clone();
        apply(&mut z, &Fields { start: Some(time::parse_input("2026-11-20T10:00", "start", &u).unwrap()), ..Fields::default() }, &[], None, &u, now).unwrap();
        assert_eq!(z.prop("DTSTART").unwrap().value, "20261120T030000");
        assert_eq!(z.prop("DTSTART").unwrap().param("TZID").as_deref(), Some("America/Chicago"));
        assert_eq!(z.prop("DTEND").unwrap().value, "20261120T040000");
        assert_eq!(z.prop("SEQUENCE").unwrap().value, "4");
        assert!(z.prop("X-CUSTOM").is_some(), "unknown properties are kept");
        // to all-day, ending inclusive
        let mut d = zoned.clone();
        apply(
            &mut d,
            &Fields { all_day: Some(true), end: Some(Input::Date(NaiveDate::from_ymd_opt(2026, 11, 21).unwrap())), ..Fields::default() },
            &[],
            None,
            &u,
            now,
        )
        .unwrap();
        assert_eq!((d.prop("DTSTART").unwrap().value.as_str(), d.prop("DTEND").unwrap().value.as_str()), ("20261119", "20261122"));
        let mut bad = zoned;
        assert!(
            apply(&mut bad, &Fields { end: Some(time::parse_input("2026-11-01T10:00", "end", &u).unwrap()), ..Fields::default() }, &[], None, &u, now).is_err()
        );
        assert!(new_object("u", &Fields::default(), None, &u, now).unwrap_err().contains("title"));
        assert!(parse_attendee("not an email").is_err());
        assert_eq!(parse_attendee("mailto:a@b.co").unwrap(), (None, "a@b.co".into()));
    }

    fn cal_text_zoned() -> String {
        cal(
            "BEGIN:VEVENT\nUID:z\nSUMMARY:Zoned\nDTSTART;TZID=America/Chicago:20261119T080000\nDTEND;TZID=America/Chicago:20261119T090000\nSEQUENCE:3\nX-CUSTOM:keep me\nEND:VEVENT\n",
        )
    }
}
