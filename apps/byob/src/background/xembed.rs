//! The tray icon where no StatusNotifierItem host runs but an X11 system tray does (i3bar,
//! polybar, dwm, stalonetray, older XFCE): the XEmbed system tray protocol, over x11rb.
//!
//! byob docks a small window into the tray (the `_NET_SYSTEM_TRAY_S<n>` selection's owner)
//! and draws its icon there. A left click opens byob; a right click opens a menu byob draws
//! itself (an override-redirect window, since XEmbed has no menus), with the pointer and
//! keyboard grabbed so a click elsewhere or Escape closes it. When the tray goes away (a bar
//! restarts), byob waits for the next one's `MANAGER` announcement and docks again.
//!
//! Everything X happens on one thread; [`XTray`] wakes it with a client message to a window
//! of its own.

use std::sync::{Arc, Mutex, PoisonError};

use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, BackPixmap, ButtonPressEvent, ChangeWindowAttributesAux, ClientMessageEvent, ColormapAlloc, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, EventMask, Gcontext, GrabMode, GrabStatus, ImageFormat, ImageOrder, PropMode, Screen, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};

use super::pixels::{self, Font, MenuLayout, Row};
use super::tray::{Look, Menu, icon_rgba};
use super::{Bus, Event};

const REQUEST_DOCK: u32 = 0;
const WAKE_QUIT: u32 = 1;
const WAKE_DRAW: u32 = 2;
/// XK_Escape.
const ESCAPE: u32 = 0xff1b;

type Fallible<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Atoms {
    selection: Atom,
    opcode: Atom,
    visual: Atom,
    manager: Atom,
    xembed_info: Atom,
    net_wm_name: Atom,
    utf8: Atom,
    wake: Atom,
}

impl Atoms {
    fn new(conn: &RustConnection, screen: usize) -> Fallible<Self> {
        let atom = |name: &str| -> Fallible<Atom> { Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom) };
        Ok(Self {
            selection: atom(&format!("_NET_SYSTEM_TRAY_S{screen}"))?,
            opcode: atom("_NET_SYSTEM_TRAY_OPCODE")?,
            visual: atom("_NET_SYSTEM_TRAY_VISUAL")?,
            manager: atom("MANAGER")?,
            xembed_info: atom("_XEMBED_INFO")?,
            net_wm_name: atom("_NET_WM_NAME")?,
            utf8: atom("UTF8_STRING")?,
            wake: atom("_BYOB_TRAY_WAKE")?,
        })
    }
}

/// What the window thread shares with [`XTray`].
#[derive(Default)]
struct Shared {
    menu: Menu,
}

/// The icon while it's up. Dropping it takes the icon down.
pub struct XTray {
    conn: Arc<RustConnection>,
    control: Window,
    wake: Atom,
    shared: Arc<Mutex<Shared>>,
}

impl XTray {
    /// Connect to the X display and put the icon up (or wait for a tray to appear): on its
    /// own thread, which says [`Event::TrayHost`] as trays come and go.
    pub fn start(bus: Bus, menu: &Menu) -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("no X display: {e}"))?;
        let conn = Arc::new(conn);
        let atoms = Atoms::new(&conn, screen).map_err(|e| format!("X: {e}"))?;
        let root = conn.setup().roots.get(screen).ok_or("no X screen")?.root;
        let control = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(COPY_DEPTH_FROM_PARENT, control, root, -1, -1, 1, 1, 0, WindowClass::INPUT_ONLY, COPY_FROM_PARENT, &CreateWindowAux::new())
            .map_err(|e| e.to_string())?;
        // A new tray announces itself on the root window.
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY)).map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        let shared = Arc::new(Mutex::new(Shared { menu: menu.clone() }));
        let wake = atoms.wake;
        let font = Font::new(byob_ui_egui::theme::INTER).ok_or("the menu's font didn't load")?;
        let icon = icon_rgba().ok_or("no icon")?;
        let mut worker = Worker { conn: conn.clone(), screen, root, control, atoms, bus, shared: shared.clone(), font, icon, dock: None, popup: None };
        std::thread::Builder::new()
            .name("byob-xtray".into())
            .spawn(move || {
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run()));
                match ran {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => log::warn!("the X11 tray icon stopped: {e}"),
                    Err(_) => log::warn!("the X11 tray icon panicked"),
                }
                worker.bus.send(Event::TrayHost(false));
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { conn, control, wake, shared })
    }

    fn wake(&self, what: u32) {
        let ev = ClientMessageEvent::new(32, self.control, self.wake, [what, 0, 0, 0, 0]);
        let sent = self.conn.send_event(false, self.control, EventMask::NO_EVENT, ev).map(drop).and_then(|()| self.conn.flush());
        if let Err(e) = sent {
            log::info!("X11 tray: {e}");
        }
    }

    pub fn set_menu(&self, menu: &Menu) {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner).menu = menu.clone();
        self.wake(WAKE_DRAW);
    }
}

impl Drop for XTray {
    fn drop(&mut self) {
        self.wake(WAKE_QUIT);
    }
}

/// Our window in the tray.
struct Dock {
    win: Window,
    owner: Window,
    gc: Gcontext,
    depth: u8,
    /// A 32-bit visual from the tray: real transparency. Else the tray's own pixels show
    /// through `ParentRelative` and the icon is blended over them.
    argb: bool,
    colormap: Option<u32>,
    size: (u16, u16),
}

/// The open menu.
struct Popup {
    win: Window,
    gc: Gcontext,
    layout: MenuLayout,
    /// What each clickable row does, by row index.
    events: Vec<Option<Event>>,
    hover: Option<usize>,
    look: Look,
}

struct Worker {
    conn: Arc<RustConnection>,
    screen: usize,
    root: Window,
    control: Window,
    atoms: Atoms,
    bus: Bus,
    shared: Arc<Mutex<Shared>>,
    font: Font,
    icon: (u32, u32, Vec<u8>),
    dock: Option<Dock>,
    popup: Option<Popup>,
}

/// The menu's rows and what each does.
fn menu_rows(menu: &Menu) -> (Vec<Row>, Vec<Option<Event>>) {
    let item = |label: &str, checked| Row::Item { label: label.to_owned(), checked };
    let mut rows = vec![(item("Open byob", None), Some(Event::Show))];
    if !menu.routines.is_empty() {
        rows.push((Row::Heading("Run now".into()), None));
        rows.extend(menu.routines.iter().map(|r| (item(&r.label, None), Some(Event::Run(r.id.clone())))));
    }
    rows.push((item("Pause routines", Some(menu.paused)), Some(Event::TogglePause)));
    rows.push((Row::Separator, None));
    rows.push((item("Quit byob", None), Some(Event::Quit)));
    rows.into_iter().unzip()
}

impl Worker {
    fn screen(&self) -> Option<&Screen> {
        self.conn.setup().roots.get(self.screen)
    }

    fn lsb(&self) -> bool {
        self.conn.setup().image_byte_order == ImageOrder::LSB_FIRST
    }

    fn menu(&self) -> Menu {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner).menu.clone()
    }

    fn run(&mut self) -> Fallible {
        if !self.dock()? {
            log::info!("no X11 system tray yet; the icon goes up when one starts");
            self.bus.send(Event::TrayHost(false));
        }
        loop {
            let event = self.conn.wait_for_event()?;
            if !self.handle(event)? {
                self.undock();
                self.close_popup();
                let _ = self.conn.destroy_window(self.control);
                let _ = self.conn.flush();
                return Ok(());
            }
        }
    }

    /// One event; `false`: stop.
    fn handle(&mut self, event: XEvent) -> Fallible<bool> {
        let dock_win = self.dock.as_ref().map(|d| d.win);
        let popup_win = self.popup.as_ref().map(|p| p.win);
        match event {
            XEvent::ClientMessage(m) if m.window == self.control && m.type_ == self.atoms.wake => match m.data.as_data32()[0] {
                WAKE_QUIT => return Ok(false),
                _ => self.redraw()?,
            },
            XEvent::ClientMessage(m) if m.type_ == self.atoms.manager && m.data.as_data32()[1] == self.atoms.selection => {
                log::info!("an X11 system tray started");
                self.undock();
                self.dock()?;
            }
            XEvent::DestroyNotify(e) if self.dock.as_ref().is_some_and(|d| d.owner == e.window) => self.tray_gone(),
            XEvent::ReparentNotify(e) if Some(e.window) == dock_win && e.parent == self.root => self.tray_gone(),
            XEvent::ConfigureNotify(e) if Some(e.window) == dock_win => {
                if let Some(d) = &mut self.dock
                    && d.size != (e.width, e.height)
                {
                    d.size = (e.width, e.height);
                    self.draw_icon()?;
                }
            }
            XEvent::Expose(e) if e.count == 0 && Some(e.window) == dock_win => self.draw_icon()?,
            XEvent::Expose(e) if e.count == 0 && Some(e.window) == popup_win => self.draw_popup()?,
            XEvent::ButtonRelease(e) if Some(e.event) == dock_win && self.popup.is_none() => match e.detail {
                1 => self.bus.send(Event::Show),
                3 => self.open_popup(&e)?,
                _ => {}
            },
            XEvent::MotionNotify(e) if Some(e.event) == popup_win => self.hover(e.event_x.into(), e.event_y.into())?,
            XEvent::ButtonPress(e) if self.popup.is_some() => {
                // With the pointer grabbed, a press anywhere lands here: outside closes.
                let inside = Some(e.event) == popup_win && self.popup.as_ref().and_then(|p| p.layout.hit(e.event_x.into(), e.event_y.into())).is_some();
                if !inside {
                    self.close_popup();
                }
            }
            XEvent::ButtonRelease(e) if self.popup.is_some() && Some(e.event) == popup_win => {
                let chosen =
                    self.popup.as_ref().and_then(|p| p.layout.hit(e.event_x.into(), e.event_y.into()).and_then(|i| p.events.get(i).cloned().flatten()));
                if let Some(ev) = chosen {
                    self.close_popup();
                    self.bus.send(ev);
                }
            }
            XEvent::KeyPress(e) if self.popup.is_some() => {
                if self.keysym(e.detail)? == ESCAPE {
                    self.close_popup();
                }
            }
            XEvent::Error(e) => log::debug!("X11 tray: {e:?}"),
            _ => {}
        }
        Ok(true)
    }

    fn tray_gone(&mut self) {
        if self.dock.is_some() {
            log::info!("the X11 system tray went away; waiting for another");
            self.undock();
            self.close_popup();
            self.bus.send(Event::TrayHost(false));
        }
    }

    fn redraw(&mut self) -> Fallible {
        self.draw_icon()?;
        if self.popup.is_some() {
            // The menu changed under it (a routine came or went): close rather than redraw
            // rows that moved.
            let (rows, _) = menu_rows(&self.menu());
            if self.popup.as_ref().is_some_and(|p| p.events.len() != rows.len()) {
                self.close_popup();
            }
        }
        Ok(())
    }

    /// Dock into the current tray, if there is one.
    fn dock(&mut self) -> Fallible<bool> {
        let owner = self.conn.get_selection_owner(self.atoms.selection)?.reply()?.owner;
        if owner == NONE {
            return Ok(false);
        }
        let conn = self.conn.clone();
        // Hear when it goes.
        conn.change_window_attributes(owner, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY))?;
        let screen = self.screen().ok_or("no X screen")?;
        let (root_depth, root_visual) = (screen.root_depth, screen.root_visual);
        // The tray's visual: 32 bits means it composites and wants ARGB icons.
        let visual = conn.get_property(false, owner, self.atoms.visual, AtomEnum::VISUALID, 0, 1)?.reply()?.value32().and_then(|mut v| v.next());
        let depth_of = |id: u32| screen.allowed_depths.iter().find(|d| d.visuals.iter().any(|v| v.visual_id == id)).map(|d| d.depth);
        let argb_visual = visual.filter(|&v| depth_of(v) == Some(32));
        let win = conn.generate_id()?;
        let events = EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE;
        let (depth, colormap) = if let Some(v) = argb_visual {
            let cmap = conn.generate_id()?;
            conn.create_colormap(ColormapAlloc::NONE, cmap, self.root, v)?;
            let aux = CreateWindowAux::new().background_pixel(0).border_pixel(0).colormap(cmap).event_mask(events);
            conn.create_window(32, win, self.root, 0, 0, 24, 24, 0, WindowClass::INPUT_OUTPUT, v, &aux)?;
            (32, Some(cmap))
        } else {
            let aux = CreateWindowAux::new().background_pixmap(BackPixmap::PARENT_RELATIVE).event_mask(events);
            conn.create_window(root_depth, win, self.root, 0, 0, 24, 24, 0, WindowClass::INPUT_OUTPUT, root_visual, &aux)?;
            (root_depth, None)
        };
        // XEmbed version 0, mapped: the tray maps it.
        conn.change_property32(PropMode::REPLACE, win, self.atoms.xembed_info, self.atoms.xembed_info, &[0, 1])?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"byob")?;
        conn.change_property8(PropMode::REPLACE, win, self.atoms.net_wm_name, self.atoms.utf8, b"byob")?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"byob\0byob\0")?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, win, &CreateGCAux::new())?;
        let dock = ClientMessageEvent::new(32, owner, self.atoms.opcode, [CURRENT_TIME, REQUEST_DOCK, win, 0, 0]);
        conn.send_event(false, owner, EventMask::NO_EVENT, dock)?;
        conn.flush()?;
        self.dock = Some(Dock { win, owner, gc, depth, argb: argb_visual.is_some(), colormap, size: (24, 24) });
        log::info!("tray icon up (X11 system tray{})", if argb_visual.is_some() { ", transparent" } else { "" });
        self.bus.send(Event::TrayHost(true));
        Ok(true)
    }

    fn undock(&mut self) {
        if let Some(d) = self.dock.take() {
            let _ = self.conn.free_gc(d.gc);
            let _ = self.conn.destroy_window(d.win);
            if let Some(c) = d.colormap {
                let _ = self.conn.free_colormap(c);
            }
            let _ = self.conn.flush();
        }
    }

    fn draw_icon(&mut self) -> Fallible {
        let Some(d) = &self.dock else { return Ok(()) };
        let (w, h) = d.size;
        let side = w.min(h).clamp(1, 256);
        let (x0, y0) = ((w - side) / 2, (h - side) / 2);
        let (iw, ih, px) = &self.icon;
        let mut img = pixels::scale_premul(px, *iw as usize, *ih as usize, side.into());
        if self.menu().attention {
            pixels::add_dot(&mut img, side.into(), [229, 72, 77]);
        }
        let lsb = self.lsb();
        let conn = &self.conn;
        conn.clear_area(false, d.win, 0, 0, 0, 0)?;
        let data = if d.argb {
            pixels::pack(img.into_iter(), lsb)
        } else {
            // What the tray painted behind us, with the icon over it.
            let under = conn.get_image(ImageFormat::Z_PIXMAP, d.win, x0.try_into()?, y0.try_into()?, side, side, !0)?.reply();
            let under = match under {
                Ok(r) if r.data.len() == usize::from(side) * usize::from(side) * 4 => pixels::unpack(&r.data, lsb),
                _ => vec![[0, 0, 0]; usize::from(side) * usize::from(side)],
            };
            pixels::pack(
                img.into_iter().zip(under).map(|(p, u)| {
                    let [r, g, b] = pixels::over(p, u);
                    [r, g, b, 255]
                }),
                lsb,
            )
        };
        self.put(d.win, d.gc, side, side, x0.try_into()?, y0.try_into()?, d.depth, &data)?;
        conn.flush()?;
        Ok(())
    }

    /// An image, in strips that fit the server's request size.
    #[allow(clippy::too_many_arguments)]
    fn put(&self, win: Window, gc: Gcontext, w: u16, h: u16, x: i16, y: i16, depth: u8, data: &[u8]) -> Fallible {
        let stride = usize::from(w) * 4;
        let rows = ((self.conn.maximum_request_bytes().saturating_sub(64)) / stride.max(1)).clamp(1, usize::from(h.max(1)));
        for (i, strip) in data.chunks(stride * rows).enumerate() {
            let n = u16::try_from(strip.len() / stride.max(1))?;
            let top = y.saturating_add(i16::try_from(i * rows)?);
            self.conn.put_image(ImageFormat::Z_PIXMAP, win, gc, w, n, x, top, 0, depth, strip)?;
        }
        Ok(())
    }

    /// The screen's scale (Xft.dpi / 96), for the menu.
    fn scale(&self) -> f32 {
        let dpi = (|| -> Fallible<Option<f32>> {
            let r = self.conn.get_property(false, self.root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16)?.reply()?;
            let text = String::from_utf8_lossy(&r.value);
            Ok(text.lines().find_map(|l| l.strip_prefix("Xft.dpi:")).and_then(|v| v.trim().parse::<f32>().ok()))
        })();
        dpi.ok().flatten().map_or(1.0, |d| (d / 96.0).clamp(1.0, 4.0))
    }

    fn open_popup(&mut self, click: &ButtonPressEvent) -> Fallible {
        let menu = self.menu();
        let (rows, events) = menu_rows(&menu);
        let layout = MenuLayout::new(rows, &self.font, self.scale());
        let screen = self.screen().ok_or("no X screen")?;
        let (sw, sh) = (i32::from(screen.width_in_pixels), i32::from(screen.height_in_pixels));
        let (root_depth, root_visual) = (screen.root_depth, screen.root_visual);
        let (w, h) = (i32::try_from(layout.w)?, i32::try_from(layout.h)?);
        // At the pointer, opening away from the screen's edges (a bar at the bottom: upward).
        let (px, py) = (i32::from(click.root_x), i32::from(click.root_y));
        let x = if px + w > sw { (px - w).max(0) } else { px };
        let y = if py + h > sh || py > sh / 2 { (py - h).max(0) } else { py };
        let win = self.conn.generate_id()?;
        let aux = CreateWindowAux::new().override_redirect(1).save_under(1).event_mask(
            EventMask::EXPOSURE
                | EventMask::POINTER_MOTION
                | EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::KEY_PRESS
                | EventMask::LEAVE_WINDOW,
        );
        self.conn.create_window(
            root_depth,
            win,
            self.root,
            i16::try_from(x)?,
            i16::try_from(y)?,
            u16::try_from(w)?,
            u16::try_from(h)?,
            0,
            WindowClass::INPUT_OUTPUT,
            root_visual,
            &aux,
        )?;
        self.conn.change_property8(PropMode::REPLACE, win, self.atoms.net_wm_name, self.atoms.utf8, b"byob")?;
        let gc = self.conn.generate_id()?;
        self.conn.create_gc(gc, win, &CreateGCAux::new())?;
        self.conn.map_window(win)?;
        self.conn.flush()?;
        self.popup = Some(Popup { win, gc, layout, events, hover: None, look: menu.look });
        self.draw_popup()?;
        // Clicks elsewhere and Escape close it.
        let mask = EventMask::POINTER_MOTION | EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE;
        let grabbed = self.conn.grab_pointer(true, win, mask, GrabMode::ASYNC, GrabMode::ASYNC, NONE, NONE, CURRENT_TIME)?.reply()?;
        if grabbed.status != GrabStatus::SUCCESS {
            log::info!("X11 tray: couldn't grab the pointer for the menu ({:?})", grabbed.status);
        }
        let _ = self.conn.grab_keyboard(true, win, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?.reply();
        Ok(())
    }

    fn hover(&mut self, x: i32, y: i32) -> Fallible {
        let Some(p) = &mut self.popup else { return Ok(()) };
        let now = p.layout.hit(x, y);
        if now != p.hover {
            p.hover = now;
            self.draw_popup()?;
        }
        Ok(())
    }

    fn draw_popup(&mut self) -> Fallible {
        let Some(p) = &self.popup else { return Ok(()) };
        let canvas = p.layout.paint(&self.font, &p.look, p.hover);
        let data = pixels::pack(canvas.px.into_iter().map(|[r, g, b]| [r, g, b, 255]), self.lsb());
        let depth = self.screen().map_or(24, |s| s.root_depth);
        self.put(p.win, p.gc, u16::try_from(canvas.w)?, u16::try_from(canvas.h)?, 0, 0, depth, &data)?;
        self.conn.flush()?;
        Ok(())
    }

    fn close_popup(&mut self) {
        if let Some(p) = self.popup.take() {
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            let _ = self.conn.free_gc(p.gc);
            let _ = self.conn.destroy_window(p.win);
            let _ = self.conn.flush();
        }
    }

    /// The first keysym of a keycode.
    fn keysym(&self, code: u8) -> Fallible<u32> {
        let r = self.conn.get_keyboard_mapping(code, 1)?.reply()?;
        Ok(r.keysyms.first().copied().unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::tray::Routine;

    #[test]
    fn the_menu_has_open_run_pause_and_quit() {
        let menu = Menu { routines: vec![Routine { id: "loom".into(), label: "Loom deals".into() }], paused: true, ..Default::default() };
        let (rows, events) = menu_rows(&menu);
        assert_eq!(rows.len(), events.len());
        assert_eq!(events, vec![Some(Event::Show), None, Some(Event::Run("loom".into())), Some(Event::TogglePause), None, Some(Event::Quit)]);
        assert_eq!(rows[3], Row::Item { label: "Pause routines".into(), checked: Some(true) });
        // No routines: no Run now.
        let (rows, _) = menu_rows(&Menu::default());
        assert!(!rows.iter().any(|r| matches!(r, Row::Heading(_))));
    }
}
