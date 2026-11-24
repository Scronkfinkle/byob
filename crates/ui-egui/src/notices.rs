//! The notices above the message box: a few at a time, the same text shown once, and each
//! gone after a while (errors stay longer).

use std::time::Duration;

use byob_protocol::NoticeLevel;
use web_time::Instant;

/// Notices kept on screen.
const MAX: usize = 3;
/// How long an info notice stays.
const INFO_FOR: Duration = Duration::from_secs(8);
/// How long an error stays.
const ERROR_FOR: Duration = Duration::from_secs(20);

#[derive(Debug, Default)]
pub struct Notices {
    items: Vec<((NoticeLevel, String), Instant)>,
}

fn lasts(level: NoticeLevel) -> Duration {
    if level == NoticeLevel::Error { ERROR_FOR } else { INFO_FOR }
}

impl Notices {
    pub fn push(&mut self, level: NoticeLevel, text: String) {
        self.push_at(level, text, Instant::now());
    }

    /// A repeat of a notice on screen replaces it (shown once, its time restarted).
    fn push_at(&mut self, level: NoticeLevel, text: String, now: Instant) {
        self.items.retain(|((_, t), _)| *t != text);
        self.items.push(((level, text), now));
        let excess = self.items.len().saturating_sub(MAX);
        self.items.drain(..excess);
    }

    /// Drops the notices whose time is up; returns how long until the next one goes.
    pub fn prune(&mut self, now: Instant) -> Option<Duration> {
        self.items.retain(|((level, _), at)| now.saturating_duration_since(*at) < lasts(*level));
        self.items.iter().map(|((level, _), at)| lasts(*level).saturating_sub(now.saturating_duration_since(*at))).min()
    }

    pub fn iter(&self) -> impl Iterator<Item = &(NoticeLevel, String)> {
        self.items.iter().map(|(n, _)| n)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HINT: &str = "sheets: Select the cells to chart first";

    #[test]
    fn a_repeated_notice_shows_once() {
        let mut n = Notices::default();
        let t0 = Instant::now();
        for i in 0..4 {
            n.push_at(NoticeLevel::Info, HINT.into(), t0 + Duration::from_secs(i));
        }
        assert_eq!(n.iter().count(), 1);
    }

    #[test]
    fn notices_go_away_by_themselves() {
        let mut n = Notices::default();
        let t0 = Instant::now();
        n.push_at(NoticeLevel::Info, HINT.into(), t0);
        n.push_at(NoticeLevel::Error, "it broke".into(), t0);
        assert_eq!(n.prune(t0 + Duration::from_secs(1)), Some(INFO_FOR - Duration::from_secs(1)));
        n.prune(t0 + INFO_FOR);
        assert_eq!(n.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["it broke"]);
        assert_eq!(n.prune(t0 + ERROR_FOR), None);
        assert!(n.is_empty());
    }

    #[test]
    fn a_repeat_restarts_its_time() {
        let mut n = Notices::default();
        let t0 = Instant::now();
        n.push_at(NoticeLevel::Info, HINT.into(), t0);
        n.push_at(NoticeLevel::Info, HINT.into(), t0 + Duration::from_secs(5));
        n.prune(t0 + INFO_FOR);
        assert_eq!(n.iter().count(), 1);
    }

    #[test]
    fn only_the_newest_few_stay() {
        let mut n = Notices::default();
        for i in 0..5 {
            n.push(NoticeLevel::Info, format!("n{i}"));
        }
        assert_eq!(n.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), ["n2", "n3", "n4"]);
    }
}
