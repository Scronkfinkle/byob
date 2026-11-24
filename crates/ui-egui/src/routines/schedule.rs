//! A routine's "When" as simple controls: how often (every day, weekdays, chosen days, every
//! few hours, a day of the month, or only when you run it) and a time. Read from a schedule's
//! rule and turned back into one; a rule these controls can't show is kept as it is
//! ("Custom") until you pick something else. The engine reads rules and describes them; this
//! only writes the few the controls make.

use byob_protocol::routines::Trigger;

/// How often, as the select says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freq {
    Daily,
    Weekdays,
    /// On the chosen days of the week.
    Weekly,
    /// Every few hours, starting at the time.
    Hourly,
    /// On one day of the month (`-1`: the last).
    Monthly,
    /// No schedule: only Run now (and plugin triggers).
    Manual,
    /// A rule the controls can't show, kept as it is.
    Custom,
}

impl Freq {
    /// What the select offers (Custom only while a routine has such a rule).
    pub const CHOICES: [(Freq, &'static str); 6] = [
        (Freq::Daily, "Every day"),
        (Freq::Weekdays, "Weekdays"),
        (Freq::Weekly, "On chosen days"),
        (Freq::Hourly, "Every few hours"),
        (Freq::Monthly, "Once a month"),
        (Freq::Manual, "Only when I run it"),
    ];

    pub fn label(self) -> &'static str {
        match self {
            Freq::Custom => "Custom",
            f => Self::CHOICES.iter().find(|(c, _)| *c == f).map_or("", |(_, l)| *l),
        }
    }
}

/// Days of the week, Monday first, as rules name them.
pub const DAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
/// Their initials, for the day buttons.
pub const INITIALS: [&str; 7] = ["M", "T", "W", "T", "F", "S", "S"];
/// Their names, for tooltips.
pub const DAY_NAMES: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
/// Every-so-many hours the select offers.
pub const HOURS: [u32; 7] = [1, 2, 3, 4, 6, 8, 12];

/// The controls' state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct When {
    pub freq: Freq,
    pub days: [bool; 7],
    pub hours: u32,
    /// 1 to 31, or -1 for the last day.
    pub month_day: i32,
    /// Hour (0–23) and minute.
    pub at: (u32, u32),
    /// The schedule's zone and first day, kept as they were (`None`: the engine's own).
    pub zone: Option<String>,
    pub start: Option<String>,
    /// A rule the controls can't show (Custom), as it was.
    pub custom: Option<String>,
}

impl Default for When {
    /// Every day at 8:00 AM.
    fn default() -> Self {
        Self { freq: Freq::Daily, days: [true; 7], hours: 2, month_day: 1, at: (8, 0), zone: None, start: None, custom: None }
    }
}

/// `"08:30"` → (8, 30).
fn parse_at(at: &str) -> Option<(u32, u32)> {
    let (h, m) = at.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

impl When {
    /// The controls for a routine's first schedule (none: Only when I run it).
    pub fn of(when: &[Trigger]) -> Self {
        let Some((rule, at, zone, start)) = when.iter().find_map(|t| match t {
            Trigger::Schedule { rule, at, zone, start } => Some((rule, at, zone, start)),
            _ => None,
        }) else {
            return Self { freq: Freq::Manual, ..Self::default() };
        };
        let mut w = Self { zone: zone.clone(), start: start.clone(), at: parse_at(at).unwrap_or((8, 0)), ..Self::default() };
        if parse_at(at).is_none() || !w.read_rule(rule) {
            w.freq = Freq::Custom;
            w.custom = Some(rule.clone());
        }
        w
    }

    /// Set the controls from `rule`; false if they can't show it.
    fn read_rule(&mut self, rule: &str) -> bool {
        let rule = rule.trim().trim_start_matches("RRULE:").to_ascii_uppercase();
        let mut freq = None;
        let (mut by_day, mut interval, mut month_day) = (None, 1u32, None);
        for part in rule.split(';').filter(|p| !p.trim().is_empty()) {
            let Some((k, v)) = part.split_once('=') else { return false };
            match k.trim() {
                "FREQ" => freq = Some(v.trim().to_owned()),
                "BYDAY" => by_day = Some(v.trim().to_owned()),
                "INTERVAL" => match v.trim().parse() {
                    Ok(n) if n >= 1 => interval = n,
                    _ => return false,
                },
                "BYMONTHDAY" => match v.trim().parse::<i32>() {
                    Ok(n) if (1..=31).contains(&n) || n == -1 => month_day = Some(n),
                    _ => return false,
                },
                "WKST" => {}
                _ => return false,
            }
        }
        match (freq.as_deref(), by_day, month_day) {
            (Some("DAILY"), None, None) if interval == 1 => self.set_days([true; 7]),
            (Some("WEEKLY"), Some(days), None) if interval == 1 => {
                let mut set = [false; 7];
                for d in days.split(',') {
                    match DAYS.iter().position(|x| *x == d.trim()) {
                        Some(i) => set[i] = true,
                        None => return false,
                    }
                }
                if !set.iter().any(|d| *d) {
                    return false;
                }
                self.set_days(set);
            }
            (Some("HOURLY"), None, None) if interval <= 24 => {
                self.freq = Freq::Hourly;
                self.hours = interval;
            }
            (Some("MONTHLY"), None, Some(d)) if interval == 1 => {
                self.freq = Freq::Monthly;
                self.month_day = d;
            }
            _ => return false,
        }
        true
    }

    /// Choose these days: every day, weekdays, or those days.
    pub fn set_days(&mut self, days: [bool; 7]) {
        self.days = days;
        self.freq = match days {
            [true, true, true, true, true, true, true] => Freq::Daily,
            [true, true, true, true, true, false, false] => Freq::Weekdays,
            _ => Freq::Weekly,
        };
    }

    /// Pick how often (the select).
    pub fn set_freq(&mut self, freq: Freq) {
        match freq {
            Freq::Daily => self.days = [true; 7],
            Freq::Weekdays => self.days = [true, true, true, true, true, false, false],
            // Chosen days start from what's chosen, or Monday.
            Freq::Weekly if self.days == [true; 7] || !self.days.iter().any(|d| *d) => self.days = [true, false, false, false, false, false, false],
            _ => {}
        }
        if freq != Freq::Custom {
            self.custom = None;
        }
        self.freq = freq;
    }

    /// The day buttons show.
    pub fn shows_days(&self) -> bool {
        matches!(self.freq, Freq::Daily | Freq::Weekdays | Freq::Weekly)
    }

    /// `HH:MM`.
    pub fn at_text(&self) -> String {
        format!("{:02}:{:02}", self.at.0, self.at.1)
    }

    /// The schedule these controls make (`None`: no schedule), or what's missing.
    pub fn trigger(&self) -> Result<Option<Trigger>, String> {
        let rule = match self.freq {
            Freq::Manual => return Ok(None),
            Freq::Daily => "FREQ=DAILY".to_owned(),
            Freq::Weekdays => "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR".to_owned(),
            Freq::Weekly => {
                let days: Vec<&str> = DAYS.iter().zip(self.days).filter(|(_, on)| *on).map(|(d, _)| *d).collect();
                if days.is_empty() {
                    return Err("Pick at least one day.".into());
                }
                format!("FREQ=WEEKLY;BYDAY={}", days.join(","))
            }
            Freq::Hourly if self.hours <= 1 => "FREQ=HOURLY".to_owned(),
            Freq::Hourly => format!("FREQ=HOURLY;INTERVAL={}", self.hours.min(24)),
            Freq::Monthly => format!("FREQ=MONTHLY;BYMONTHDAY={}", self.month_day),
            Freq::Custom => self.custom.clone().unwrap_or_else(|| "FREQ=DAILY".to_owned()),
        };
        Ok(Some(Trigger::Schedule { rule, at: self.at_text(), zone: self.zone.clone(), start: self.start.clone() }))
    }

    /// What it says, in words, for the controls the engine hasn't described yet ("Every day
    /// at 8:00 AM"); Custom has only the engine's words.
    pub fn words(&self) -> String {
        let at = clock(self.at.0, self.at.1);
        match self.freq {
            Freq::Manual => "Only when you run it".to_owned(),
            Freq::Daily => format!("Every day at {at}"),
            Freq::Weekdays => format!("Weekdays at {at}"),
            Freq::Weekly => {
                let names: Vec<&str> = DAY_NAMES.iter().zip(self.days).filter(|(_, on)| *on).map(|(d, _)| *d).collect();
                format!("{} at {at}", list(&names))
            }
            Freq::Hourly if self.hours <= 1 => format!("Every hour from {at}"),
            Freq::Hourly => format!("Every {} hours from {at}", self.hours),
            Freq::Monthly if self.month_day == -1 => format!("The last day of every month at {at}"),
            Freq::Monthly => format!("Every month on the {} at {at}", ordinal(self.month_day)),
            Freq::Custom => String::new(),
        }
    }
}

/// "Mondays, Wednesdays and Fridays".
fn list(days: &[&str]) -> String {
    let plural: Vec<String> = days.iter().map(|d| format!("{d}s")).collect();
    match plural.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// 1st, 2nd, 3rd, 4th, 11th, 21st.
pub fn ordinal(n: i32) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// A time of day as people say it: "8:00 AM", "12:30 PM".
pub fn clock(h: u32, m: u32) -> String {
    let (h12, half) = match h % 24 {
        0 => (12, "AM"),
        h @ 1..=11 => (h, "AM"),
        12 => (12, "PM"),
        h => (h - 12, "PM"),
    };
    format!("{h12}:{m:02} {half}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sched(rule: &str, at: &str) -> Vec<Trigger> {
        vec![Trigger::Schedule { rule: rule.into(), at: at.into(), zone: Some("Europe/Berlin".into()), start: Some("2026-11-24".into()) }]
    }

    fn rule_of(w: &When) -> String {
        match w.trigger().unwrap() {
            Some(Trigger::Schedule { rule, .. }) => rule,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rules_the_controls_can_show_read_back_the_same() {
        for (rule, freq) in [
            ("FREQ=DAILY", Freq::Daily),
            ("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR", Freq::Weekdays),
            ("FREQ=WEEKLY;BYDAY=MO,WE,FR", Freq::Weekly),
            ("FREQ=HOURLY;INTERVAL=2", Freq::Hourly),
            ("FREQ=HOURLY", Freq::Hourly),
            ("FREQ=MONTHLY;BYMONTHDAY=15", Freq::Monthly),
            ("FREQ=MONTHLY;BYMONTHDAY=-1", Freq::Monthly),
        ] {
            let w = When::of(&sched(rule, "08:30"));
            assert_eq!(w.freq, freq, "{rule}");
            assert_eq!(rule_of(&w), rule, "{rule}");
            assert_eq!(w.at, (8, 30));
            // The zone and first day stay as they were.
            assert!(matches!(w.trigger().unwrap(), Some(Trigger::Schedule { zone: Some(z), start: Some(_), .. }) if z == "Europe/Berlin"));
        }
        // Every day of the week reads as every day.
        assert_eq!(When::of(&sched("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR,SA,SU", "08:00")).freq, Freq::Daily);
    }

    #[test]
    fn other_rules_stay_as_they_are_until_changed() {
        let w = When::of(&sched("FREQ=MONTHLY;BYDAY=-1FR", "18:00"));
        assert_eq!(w.freq, Freq::Custom);
        assert_eq!(rule_of(&w), "FREQ=MONTHLY;BYDAY=-1FR");
        let mut w = w;
        w.set_freq(Freq::Daily);
        assert_eq!(rule_of(&w), "FREQ=DAILY");
        assert_eq!(When::of(&[]).freq, Freq::Manual);
        assert_eq!(When::of(&[]).trigger().unwrap(), None, "only when you run it: no schedule");
    }

    #[test]
    fn the_controls_build_the_schedule() {
        let mut w = When { zone: None, start: None, ..When::default() };
        assert_eq!(w.trigger().unwrap(), Some(Trigger::Schedule { rule: "FREQ=DAILY".into(), at: "08:00".into(), zone: None, start: None }));
        w.set_freq(Freq::Weekdays);
        w.at = (7, 15);
        assert_eq!(rule_of(&w), "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR");
        assert!(matches!(w.trigger().unwrap(), Some(Trigger::Schedule { at, .. }) if at == "07:15"));
        // A day off the weekdays: chosen days.
        let mut days = w.days;
        days[2] = false;
        w.set_days(days);
        assert_eq!((w.freq, rule_of(&w)), (Freq::Weekly, "FREQ=WEEKLY;BYDAY=MO,TU,TH,FR".into()));
        w.set_days([false; 7]);
        assert!(w.trigger().is_err(), "no day chosen");
        w.set_freq(Freq::Weekly);
        assert_eq!(rule_of(&w), "FREQ=WEEKLY;BYDAY=MO", "chosen days start on Monday");
        w.set_freq(Freq::Hourly);
        w.hours = 6;
        assert_eq!(rule_of(&w), "FREQ=HOURLY;INTERVAL=6");
        w.set_freq(Freq::Monthly);
        w.month_day = -1;
        assert_eq!(rule_of(&w), "FREQ=MONTHLY;BYMONTHDAY=-1");
    }

    #[test]
    fn words_say_it_plainly() {
        let mut w = When::default();
        assert_eq!(w.words(), "Every day at 8:00 AM");
        w.set_days([true, false, true, false, true, false, false]);
        w.at = (18, 30);
        assert_eq!(w.words(), "Mondays, Wednesdays and Fridays at 6:30 PM");
        w.set_freq(Freq::Monthly);
        w.month_day = 2;
        assert_eq!(w.words(), "Every month on the 2nd at 6:30 PM");
        assert_eq!((clock(0, 5), clock(12, 0), ordinal(11), ordinal(23)), ("12:05 AM".into(), "12:00 PM".into(), "11th".into(), "23rd".into()));
    }
}
