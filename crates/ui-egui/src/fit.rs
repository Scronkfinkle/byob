//! Keeping the chat readable as the window narrows. The chat is what matters: when the
//! sidebar and the artifact pane would leave it less than [`CHAT_MIN`], byob tucks them away
//! (the sidebar first, then the pane to its rail) and brings back what it tucked away once
//! there is room again. Something you open yourself while it's tight stays open until the
//! window gets narrower still. Phones (below `sidebar::NARROW`) have their own one-thing-at-
//! a-time layout and don't use this.

/// The narrowest the chat column gets before panels make way.
pub const CHAT_MIN: f32 = 560.0;
/// Extra room needed before a tucked-away panel comes back (no flicker at the edge).
const SLACK: f32 = 40.0;

/// What to do this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Do {
    CloseSidebar,
    OpenSidebar,
    CollapsePane,
    OpenPane,
}

#[derive(Debug, Clone)]
pub struct Fit {
    /// Byob closed these (so it may bring them back).
    closed_sidebar: bool,
    closed_pane: bool,
    /// You opened it while tight, at this window width: left alone until narrower.
    forced_sidebar: Option<f32>,
    forced_pane: Option<f32>,
    /// Their widths when last shown.
    pub sidebar_w: f32,
    pub pane_w: f32,
    /// Whether each was open after the last step (`None` before the first): one that opens
    /// between steps was opened by you.
    was: Option<(bool, bool)>,
}

impl Default for Fit {
    fn default() -> Self {
        Self { closed_sidebar: false, closed_pane: false, forced_sidebar: None, forced_pane: None, sidebar_w: 260.0, pane_w: 420.0, was: None }
    }
}

impl Fit {
    /// Forget it all (the window became a phone layout, or the settings took over).
    pub fn reset(&mut self) {
        let (s, p) = (self.sidebar_w, self.pane_w);
        *self = Self { sidebar_w: s, pane_w: p, ..Self::default() };
    }

    /// One frame at window `width`, with what is open now; returns what to change.
    pub fn step(&mut self, width: f32, sidebar_open: bool, pane_open: bool) -> Vec<Do> {
        // Opened by you since the last step (also after byob closed it): yours now, at this
        // width, until the window gets narrower.
        if let Some((was_sidebar, was_pane)) = self.was {
            if sidebar_open && !was_sidebar {
                self.closed_sidebar = false;
                self.forced_sidebar = Some(width);
            }
            if pane_open && !was_pane {
                self.closed_pane = false;
                self.forced_pane = Some(width);
            }
        }
        let narrower = |forced: Option<f32>| forced.is_none_or(|f| width < f - 1.0);
        let mut room = width - if sidebar_open { self.sidebar_w } else { 0.0 } - if pane_open { self.pane_w } else { 0.0 };
        let mut out = Vec::new();
        if room < CHAT_MIN {
            if sidebar_open && narrower(self.forced_sidebar) {
                out.push(Do::CloseSidebar);
                self.closed_sidebar = true;
                self.forced_sidebar = None;
                room += self.sidebar_w;
            }
            if room < CHAT_MIN && pane_open && narrower(self.forced_pane) {
                out.push(Do::CollapsePane);
                self.closed_pane = true;
                self.forced_pane = None;
            }
        } else {
            // Back in the reverse order: the pane, then the sidebar; one at a time.
            if self.closed_pane && room - self.pane_w >= CHAT_MIN + SLACK {
                out.push(Do::OpenPane);
                self.closed_pane = false;
            } else if self.closed_sidebar && room - self.sidebar_w >= CHAT_MIN + SLACK {
                out.push(Do::OpenSidebar);
                self.closed_sidebar = false;
            }
            if room >= CHAT_MIN + SLACK {
                self.forced_sidebar = None;
                self.forced_pane = None;
            }
        }
        // What will be open once these are done.
        let sidebar_after = (sidebar_open || out.contains(&Do::OpenSidebar)) && !out.contains(&Do::CloseSidebar);
        let pane_after = (pane_open || out.contains(&Do::OpenPane)) && !out.contains(&Do::CollapsePane);
        self.was = Some((sidebar_after, pane_after));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidebar_makes_way_first_and_comes_back_with_room() {
        let mut f = Fit::default();
        assert!(f.step(1200.0, true, false).is_empty());
        // 860 − 260 = 600: fine. 800 − 260 = 540: too tight.
        assert!(f.step(860.0, true, false).is_empty());
        assert_eq!(f.step(800.0, true, false), [Do::CloseSidebar]);
        // Not back at the edge (no flicker), back with room to spare.
        assert!(f.step(840.0, false, false).is_empty());
        assert_eq!(f.step(880.0, false, false), [Do::OpenSidebar]);
    }

    #[test]
    fn the_pane_goes_to_its_rail_once_the_sidebar_is_gone() {
        let mut f = Fit::default();
        // 1200 − 260 − 420 = 520: the sidebar goes; 1200 − 420 = 780 is enough.
        assert_eq!(f.step(1200.0, true, true), [Do::CloseSidebar]);
        // 900 − 420 = 480: the pane goes too.
        assert_eq!(f.step(900.0, false, true), [Do::CollapsePane]);
        // Back: the pane first, then the sidebar.
        assert_eq!(f.step(1040.0, false, false), [Do::OpenPane]);
        assert_eq!(f.step(1400.0, false, true), [Do::OpenSidebar]);
    }

    #[test]
    fn what_you_open_while_tight_stays_until_narrower() {
        let mut f = Fit::default();
        assert_eq!(f.step(800.0, true, false), [Do::CloseSidebar]);
        // You open it again: left alone at this width and wider.
        assert!(f.step(800.0, true, false).is_empty());
        assert!(f.step(810.0, true, false).is_empty());
        // Narrower still: it makes way again.
        assert_eq!(f.step(760.0, true, false), [Do::CloseSidebar]);
    }

    /// A pane you open while the window is tight stays: the sidebar makes way instead.
    #[test]
    fn a_pane_you_open_stays_and_the_sidebar_makes_way() {
        let mut f = Fit { pane_w: 500.0, ..Fit::default() };
        assert!(f.step(1000.0, true, false).is_empty());
        // You open the pane: 1000 − 260 − 500 is too little, the sidebar goes, the pane stays.
        assert_eq!(f.step(1000.0, true, true), [Do::CloseSidebar]);
        assert!(f.step(1000.0, false, true).is_empty());
    }

    #[test]
    fn closed_by_you_is_never_reopened() {
        let mut f = Fit::default();
        assert!(f.step(1200.0, false, false).is_empty());
        assert!(f.step(1600.0, false, false).is_empty());
    }
}
