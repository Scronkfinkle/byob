//! Dates and times (from `byob-when`), plus the iCalendar property parts: DTSTART-style
//! properties, EXDATE/RDATE lists, and `TZID`s with the object's own VTIMEZONE as a
//! fallback.

pub use byob_when::time::*;

use crate::ical::{Component, Prop};

/// An iCalendar date or date-time property (`VALUE=DATE`, `...Z`, `TZID=...`, floating).
pub fn parse_when(p: &Prop, timezones: &[Component]) -> Option<When> {
    parse_value_tz(&p.value, p.param("VALUE").as_deref(), p.param("TZID").as_deref(), timezones)
}

/// One value with its `TZID` (EXDATE/RDATE carry comma-separated lists: see [`parse_list`]).
pub fn parse_value_tz(v: &str, kind: Option<&str>, tzid: Option<&str>, timezones: &[Component]) -> Option<When> {
    let zone = tzid.map_or(Zone::Floating, |id| resolve_tzid(id, timezones));
    parse_value(v, kind, zone)
}

/// Every value of a list property (EXDATE, RDATE), periods skipped.
pub fn parse_list(p: &Prop, timezones: &[Component]) -> Vec<When> {
    let kind = p.param("VALUE");
    let tzid = p.param("TZID");
    p.value.split(',').take(1000).filter(|v| !v.contains('/')).filter_map(|v| parse_value_tz(v, kind.as_deref(), tzid.as_deref(), timezones)).collect()
}

/// A TZID as a zone: an IANA or common Windows name ([`zone_named`]), else the standard
/// offset the object's VTIMEZONE gives, else floating.
pub fn resolve_tzid(id: &str, timezones: &[Component]) -> Zone {
    if let Some(z) = zone_named(id) {
        return z;
    }
    let id = id.trim().trim_matches('"');
    let vtz = timezones.iter().find(|c| c.text("TZID").is_some_and(|t| t.trim() == id));
    let offset = vtz
        .and_then(|v| v.children_named("STANDARD").next().or_else(|| v.children.first()))
        .and_then(|s| s.prop("TZOFFSETTO"))
        .and_then(|p| parse_offset(&p.value));
    offset.map_or(Zone::Floating, Zone::Fixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn reads_ics_values_and_zones() {
        let vtz = crate::ical::parse("BEGIN:VTIMEZONE\nTZID:Custom Zone\nBEGIN:STANDARD\nTZOFFSETTO:+0530\nEND:STANDARD\nEND:VTIMEZONE\n");
        let w = |v: &str, tzid: Option<&str>| parse_value_tz(v, None, tzid, &vtz).unwrap();
        assert_eq!(w("20261119", None), When::Date(NaiveDate::from_ymd_opt(2026, 11, 19).unwrap()));
        let u = UserZone::Iana(chrono_tz::Europe::Berlin);
        assert_eq!(w("20261119T150000Z", None).instant(&u).to_rfc3339(), "2026-11-19T15:00:00+00:00");
        assert_eq!(w("20261119T150000", Some("America/New_York")).instant(&u).to_rfc3339(), "2026-11-19T20:00:00+00:00");
        assert_eq!(w("20261119T150000", Some("/mozilla.org/20050126_1/America/New_York")).instant(&u).to_rfc3339(), "2026-11-19T20:00:00+00:00");
        assert_eq!(w("20261119T150000", Some("Eastern Standard Time")).instant(&u).to_rfc3339(), "2026-11-19T20:00:00+00:00");
        assert_eq!(w("20261119T150000", Some("Custom Zone")).instant(&u).to_rfc3339(), "2026-11-19T09:30:00+00:00");
        assert_eq!(w("20261119T150000", Some("Nowhere/Special")).instant(&u).to_rfc3339(), "2026-11-19T14:00:00+00:00", "floating");
        assert_eq!(w("20261119T150000", None).instant(&u).to_rfc3339(), "2026-11-19T14:00:00+00:00");
        assert!(parse_value_tz("2026-11-19", None, None, &[]).is_none());
    }
}
