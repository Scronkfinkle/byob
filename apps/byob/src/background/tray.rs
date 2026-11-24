//! The tray icon: Open byob · Run ▸ (each routine) · Pause routines · Quit byob, and an
//! attention state when a run waits for you.
//!
//! - Linux and the BSDs: a StatusNotifierItem (ksni) on its own thread. Without a tray host
//!   (GNOME without the AppIndicator extension, some tiling setups) it reports
//!   [`Event::TrayHost`]`(false)` and byob carries on without one.
//! - macOS and Windows: tray-icon on the main thread (made from `App::logic`, inside the
//!   event loop); its clicks come through its global handlers onto the [`Bus`].

use super::{Bus, Event};

/// What the menu shows. The tray redraws it when this changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Menu {
    /// Routines to start from Run ▸; none: no Run submenu.
    pub routines: Vec<Routine>,
    pub paused: bool,
    /// Something waits for you (a run asks for approval): the icon says so.
    pub attention: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routine {
    pub id: String,
    pub label: String,
}

const OPEN: &str = "open";
const PAUSE: &str = "pause";
const QUIT: &str = "quit";
const RUN: &str = "run:";

/// The event a menu item's id stands for.
pub fn event_for(id: &str) -> Option<Event> {
    match id {
        OPEN => Some(Event::Show),
        PAUSE => Some(Event::TogglePause),
        QUIT => Some(Event::Quit),
        _ => id.strip_prefix(RUN).filter(|r| !r.is_empty()).map(|r| Event::Run(r.to_owned())),
    }
}

/// The icon, 64×64 RGBA.
fn icon_rgba() -> Option<(u32, u32, Vec<u8>)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(&include_bytes!("../../assets/tray-icon.png")[..]));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Some((info.width, info.height, buf))
}

pub struct Tray {
    imp: imp::Tray,
    menu: Menu,
}

impl Tray {
    /// Put the icon up. On Linux this returns at once; whether a tray host took it arrives
    /// as [`Event::TrayHost`].
    pub fn start(bus: &Bus, menu: Menu) -> Result<Self, String> {
        Ok(Self { imp: imp::Tray::start(bus, &menu)?, menu })
    }

    pub fn set_menu(&mut self, menu: Menu) {
        if menu != self.menu {
            self.imp.set_menu(&menu);
            self.menu = menu;
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::sync::{Arc, Mutex, PoisonError};

    use ksni::blocking::TrayMethods;

    use super::{Menu, OPEN, PAUSE, QUIT, RUN, event_for, icon_rgba};
    use crate::background::{Bus, Event};

    struct Sni {
        bus: Bus,
        menu: Menu,
        icon: Vec<ksni::Icon>,
    }

    impl Sni {
        fn item(&self, id: &str, label: &str) -> ksni::MenuItem<Self> {
            let id = id.to_owned();
            ksni::menu::StandardItem {
                label: label.to_owned(),
                activate: Box::new(move |this: &mut Self| {
                    if let Some(e) = event_for(&id) {
                        this.bus.send(e);
                    }
                }),
                ..Default::default()
            }
            .into()
        }
    }

    impl ksni::Tray for Sni {
        fn id(&self) -> String {
            "byob".into()
        }
        fn title(&self) -> String {
            "byob".into()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icon.clone()
        }
        fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icon.clone()
        }
        fn status(&self) -> ksni::Status {
            if self.menu.attention { ksni::Status::NeedsAttention } else { ksni::Status::Active }
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            let description = if self.menu.attention { "Something is waiting for you" } else { "Running in the background" };
            ksni::ToolTip { title: "byob".into(), description: description.into(), ..Default::default() }
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            self.bus.send(Event::Show);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            let mut items = vec![self.item(OPEN, "Open byob")];
            if !self.menu.routines.is_empty() {
                let submenu = self.menu.routines.iter().map(|r| self.item(&format!("{RUN}{}", r.id), &r.label)).collect();
                items.push(ksni::menu::SubMenu { label: "Run".into(), submenu, ..Default::default() }.into());
            }
            items.push(
                ksni::menu::CheckmarkItem {
                    label: "Pause routines".into(),
                    checked: self.menu.paused,
                    activate: Box::new(|this: &mut Self| {
                        if let Some(e) = event_for(PAUSE) {
                            this.bus.send(e);
                        }
                    }),
                    ..Default::default()
                }
                .into(),
            );
            items.push(ksni::MenuItem::Separator);
            items.push(self.item(QUIT, "Quit byob"));
            items
        }
        fn watcher_online(&self) {
            self.bus.send(Event::TrayHost(true));
        }
        fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
            log::info!("the tray host went away ({reason:?})");
            self.bus.send(Event::TrayHost(false));
            // Keep the item: it comes back when a host does.
            true
        }
    }

    #[derive(Default)]
    struct Shared {
        handle: Option<ksni::blocking::Handle<Sni>>,
        menu: Menu,
        dropped: bool,
    }

    pub struct Tray {
        shared: Arc<Mutex<Shared>>,
    }

    /// ARGB32, big-endian, as the spec wants.
    fn argb(rgba: &[u8]) -> Vec<u8> {
        rgba.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, a]| [a, r, g, b]).collect()
    }

    impl Tray {
        pub fn start(bus: &Bus, menu: &Menu) -> Result<Self, String> {
            let shared = Arc::new(Mutex::new(Shared { menu: menu.clone(), ..Default::default() }));
            let icon = icon_rgba()
                .and_then(|(w, h, px)| Some(ksni::Icon { width: i32::try_from(w).ok()?, height: i32::try_from(h).ok()?, data: argb(&px) }))
                .into_iter()
                .collect();
            let sni = Sni { bus: bus.clone(), menu: menu.clone(), icon };
            let (bus, slot) = (bus.clone(), shared.clone());
            // Registering waits on D-Bus: not on the window's thread.
            std::thread::Builder::new()
                .name("byob-tray".into())
                .spawn(move || {
                    let spawned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sni.spawn()));
                    match spawned {
                        Ok(Ok(handle)) => {
                            let mut s = slot.lock().unwrap_or_else(PoisonError::into_inner);
                            if s.dropped {
                                drop(handle.shutdown());
                                return;
                            }
                            let menu = s.menu.clone();
                            handle.update(|t| t.menu = menu);
                            s.handle = Some(handle);
                            drop(s);
                            log::info!("tray icon up");
                            bus.send(Event::TrayHost(true));
                        }
                        Ok(Err(e)) => {
                            log::info!("no tray icon: {e}");
                            bus.send(Event::TrayHost(false));
                        }
                        Err(_) => {
                            log::warn!("no tray icon: it panicked");
                            bus.send(Event::TrayHost(false));
                        }
                    }
                })
                .map_err(|e| format!("couldn't start the tray: {e}"))?;
            Ok(Self { shared })
        }

        pub fn set_menu(&mut self, menu: &Menu) {
            let mut s = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            s.menu = menu.clone();
            if let Some(h) = &s.handle {
                let menu = menu.clone();
                h.update(|t| t.menu = menu);
            }
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            let mut s = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            s.dropped = true;
            if let Some(h) = s.handle.take() {
                drop(h.shutdown());
            }
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn pixels_become_argb() {
            assert_eq!(super::argb(&[1, 2, 3, 4, 5, 6, 7, 8]), [4, 1, 2, 3, 8, 5, 6, 7]);
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use tray_icon::menu::{CheckMenuItem, IsMenuItem, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use super::{Menu, OPEN, PAUSE, QUIT, RUN, event_for, icon_rgba};
    use crate::background::{Bus, Event};

    pub struct Tray {
        icon: TrayIcon,
    }

    fn log_err<E: std::fmt::Display>(r: Result<(), E>) {
        if let Err(e) = r {
            log::warn!("tray menu: {e}");
        }
    }

    fn build(menu: &Menu) -> tray_icon::menu::Menu {
        let m = tray_icon::menu::Menu::new();
        log_err(m.append(&MenuItem::with_id(OPEN, "Open byob", true, None)));
        if !menu.routines.is_empty() {
            let items: Vec<MenuItem> = menu.routines.iter().map(|r| MenuItem::with_id(format!("{RUN}{}", r.id), &r.label, true, None)).collect();
            let refs: Vec<&dyn IsMenuItem> = items.iter().map(|i| i as &dyn IsMenuItem).collect();
            let sub = Submenu::new("Run", true);
            log_err(sub.append_items(&refs));
            log_err(m.append(&sub));
        }
        log_err(m.append(&CheckMenuItem::with_id(PAUSE, "Pause routines", true, menu.paused, None)));
        log_err(m.append(&PredefinedMenuItem::separator()));
        log_err(m.append(&MenuItem::with_id(QUIT, "Quit byob", true, None)));
        m
    }

    fn tooltip(menu: &Menu) -> &'static str {
        if menu.attention { "byob: something is waiting for you" } else { "byob is running in the background" }
    }

    impl Tray {
        pub fn start(bus: &Bus, menu: &Menu) -> Result<Self, String> {
            // The handlers are global and set once; every tray of this process uses the same bus.
            let b = bus.clone();
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                if let Some(ev) = event_for(&e.id.0) {
                    b.send(ev);
                }
            }));
            let b = bus.clone();
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| match e {
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } | TrayIconEvent::DoubleClick { .. } => {
                    b.send(Event::Show)
                }
                _ => {}
            }));
            let mut builder = TrayIconBuilder::new().with_menu(Box::new(build(menu))).with_tooltip(tooltip(menu));
            if let Some(icon) = icon_rgba().and_then(|(w, h, px)| tray_icon::Icon::from_rgba(px, w, h).ok()) {
                builder = builder.with_icon(icon);
            }
            // Windows: a left click opens byob, the menu is on the right button.
            #[cfg(target_os = "windows")]
            let builder = builder.with_menu_on_left_click(false);
            let icon = builder.build().map_err(|e| format!("couldn't make the tray icon: {e}"))?;
            bus.send(Event::TrayHost(true));
            Ok(Self { icon })
        }

        pub fn set_menu(&mut self, menu: &Menu) {
            self.icon.set_menu(Some(Box::new(build(menu))));
            log_err(self.icon.set_tooltip(Some(tooltip(menu))));
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::Menu;
    use crate::background::Bus;

    pub struct Tray;

    impl Tray {
        pub fn start(_bus: &Bus, _menu: &Menu) -> Result<Self, String> {
            Err("no tray on this system".into())
        }
        pub fn set_menu(&mut self, _menu: &Menu) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_stand_for_events() {
        assert_eq!(event_for(OPEN), Some(Event::Show));
        assert_eq!(event_for(PAUSE), Some(Event::TogglePause));
        assert_eq!(event_for(QUIT), Some(Event::Quit));
        assert_eq!(event_for("run:morning-news"), Some(Event::Run("morning-news".into())));
        assert_eq!(event_for("run:"), None);
        assert_eq!(event_for("nope"), None);
    }

    #[test]
    fn the_icon_is_there() {
        let (w, h, px) = icon_rgba().unwrap();
        assert_eq!((w, h, px.len()), (64, 64, 64 * 64 * 4));
    }
}
