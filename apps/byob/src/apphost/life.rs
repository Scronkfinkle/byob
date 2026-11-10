//! When an app's helper process runs, renders, or goes: a small state machine fed once per
//! frame with whether the app was drawn on screen and whether something held it in the
//! background (a minimized chip, a background tab, the collapsed pane's rail).
//!
//! - Drawn: [`Verdict::Show`] — running and rendering.
//! - Not drawn, not held: still shown for [`HIDE_AFTER`] (a scroll past), then
//!   [`Verdict::Pause`] — snapshot kept, process exits.
//! - Held: [`Verdict::Background`] — the process keeps running (page state kept) but paints
//!   and reads back nothing, until it has been held for [`BACKGROUND_PAUSE_AFTER`] (or
//!   nothing has held it for [`HIDE_AFTER`]); then [`Verdict::Pause`].
//!
//! Holding never starts a process; only drawing does (restoring a paused app starts a fresh
//! one, its page state lost).

use std::time::{Duration, Instant};

/// Out of view this long: snapshot and stop the process.
pub const HIDE_AFTER: Duration = Duration::from_secs(3);
/// Minimized (or in a background tab) this long: snapshot and stop the process too. A
/// minute keeps a quick minimize → restore lossless while still freeing the ~110–150 MB a
/// live app costs soon after you move on.
pub const BACKGROUND_PAUSE_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Running and rendering.
    Show,
    /// Running, rendering nothing.
    Background,
    /// Snapshot and stop the process.
    Pause,
}

#[derive(Debug, Clone, Copy)]
pub struct Life {
    last_drawn: Instant,
    /// Held in the background since (`None`: in the foreground).
    held_since: Option<Instant>,
    last_held: Instant,
}

impl Life {
    pub fn new(now: Instant) -> Self {
        Self { last_drawn: now, held_since: None, last_held: now }
    }

    /// Last time the app was drawn or held (for dropping old snapshots).
    pub fn last_active(&self) -> Instant {
        self.last_drawn.max(self.last_held)
    }

    pub fn in_background(&self) -> bool {
        self.held_since.is_some()
    }

    /// This frame: was the app drawn on screen, was it held in the background?
    pub fn frame(&mut self, now: Instant, drawn: bool, held: bool) -> Verdict {
        if drawn {
            self.last_drawn = now;
            self.held_since = None;
            return Verdict::Show;
        }
        if held {
            self.held_since.get_or_insert(now);
            self.last_held = now;
        }
        match self.held_since {
            Some(since) => {
                let too_long = now.saturating_duration_since(since) >= BACKGROUND_PAUSE_AFTER;
                let let_go = now.saturating_duration_since(self.last_held) > HIDE_AFTER;
                if too_long || let_go { Verdict::Pause } else { Verdict::Background }
            }
            None if now.saturating_duration_since(self.last_drawn) > HIDE_AFTER => Verdict::Pause,
            None => Verdict::Show,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(16);

    #[test]
    fn drawn_apps_show_and_scrolled_away_ones_pause_after_a_while() {
        let t0 = Instant::now();
        let mut l = Life::new(t0);
        assert_eq!(l.frame(t0, true, false), Verdict::Show);
        assert_eq!(l.frame(t0 + HIDE_AFTER, false, false), Verdict::Show, "a scroll past keeps it");
        assert_eq!(l.frame(t0 + HIDE_AFTER + FRAME, false, false), Verdict::Pause);
    }

    #[test]
    fn minimized_keeps_running_without_rendering_then_pauses() {
        let t0 = Instant::now();
        let mut l = Life::new(t0);
        l.frame(t0, true, false);
        // Minimized: held every frame from now on.
        let mut now = t0 + FRAME;
        assert_eq!(l.frame(now, false, true), Verdict::Background);
        assert!(l.in_background());
        while now < t0 + BACKGROUND_PAUSE_AFTER {
            assert_eq!(l.frame(now, false, true), Verdict::Background, "{:?}", now - t0);
            now += Duration::from_secs(1);
        }
        assert_eq!(l.frame(t0 + FRAME + BACKGROUND_PAUSE_AFTER, false, true), Verdict::Pause);
    }

    #[test]
    fn restoring_brings_it_back_and_restarts_the_clock() {
        let t0 = Instant::now();
        let mut l = Life::new(t0);
        l.frame(t0, false, true);
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(l.frame(t1, true, false), Verdict::Show, "restored");
        assert!(!l.in_background());
        // Minimized again: a fresh minute.
        assert_eq!(l.frame(t1 + FRAME, false, true), Verdict::Background);
        assert_eq!(l.frame(t1 + Duration::from_secs(59), false, true), Verdict::Background);
        assert_eq!(l.last_active(), t1 + Duration::from_secs(59));
    }

    #[test]
    fn drawn_wins_over_held_in_the_same_frame() {
        // The pane draws it while its chat card is minimized.
        let t0 = Instant::now();
        let mut l = Life::new(t0);
        for i in 0..100u32 {
            assert_eq!(l.frame(t0 + BACKGROUND_PAUSE_AFTER * i, true, true), Verdict::Show);
        }
    }

    #[test]
    fn nothing_holding_it_any_more_pauses_soon() {
        // Its chip went away (another chat was opened).
        let t0 = Instant::now();
        let mut l = Life::new(t0);
        l.frame(t0, false, true);
        assert_eq!(l.frame(t0 + HIDE_AFTER, false, false), Verdict::Background);
        assert_eq!(l.frame(t0 + HIDE_AFTER + FRAME, false, false), Verdict::Pause);
    }
}
