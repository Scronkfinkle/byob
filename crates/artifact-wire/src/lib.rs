//! The wire protocol between byob and `byob-artifact-host`, the helper process that renders
//! one HTML app artifact with Servo (see `host/artifact-host/`).
//!
//! byob writes [`Request`]s to the helper's stdin; the helper writes [`Event`]s to its stdout.
//! Every message is one frame: a little-endian `u32` body length, then the body — a tag byte
//! and the fields in order (`u32`/`u64`/`f32` little-endian, `bool` as one byte, strings and
//! byte strings as a `u32` length + bytes, lists as a `u32` count + items). Bodies over
//! [`MAX_FRAME`] are refused before anything is allocated.
//!
//! The helper's stdout is shared with every library inside it, and some print there (surfman
//! does on Windows). So when byob sets [`MARKED_ENV`], the helper starts each frame with
//! [`MARK`] ([`send_marked`]), and byob reads with a [`Reader`], which passes anything between
//! frames to a callback instead of losing the stream. Unmarked frames still read, so an older
//! helper keeps working.
//!
//! No dependencies on purpose: both workspaces build it, and the helper's must not pull
//! anything else of byob's.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

use std::fmt;
use std::io::{self, Read, Write};

/// Largest frame body either side accepts (a 4K-ish frame of RGBA plus slack).
pub const MAX_FRAME: usize = 64 << 20;
/// Largest HTML document byob sends.
pub const MAX_HTML: usize = 16 << 20;
/// Largest bridge message either side sends ([`Request::Message`], [`Event::Message`]): the
/// JSON text of one JSON-RPC message between the page and byob.
pub const MAX_MESSAGE: usize = 256 << 10;

/// Size of the rendering surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Device pixels.
    pub width: u32,
    pub height: u32,
    /// Device pixels per CSS pixel.
    pub scale: f32,
}

/// The host's theme: `--byob-*` CSS variables and whether it is dark.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Theme {
    pub vars: Vec<(String, String)>,
    pub dark: bool,
}

/// Show this document.
#[derive(Debug, Clone, PartialEq)]
pub struct Load {
    /// The whole document (byob has already injected its theme defaults and helper script).
    pub html: String,
    pub theme: Theme,
    pub viewport: Viewport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Primary,
    Secondary,
    Middle,
}

/// Modifier keys, as bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods(pub u8);

impl Mods {
    pub const SHIFT: u8 = 1;
    pub const CTRL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const META: u8 = 8;

    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

/// A key press or release, named as the DOM does (`key` like `"ArrowUp"` or `"a"`, `code`
/// like `"KeyA"`; empty `code` for unknown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    pub down: bool,
    pub key: String,
    pub code: String,
    pub mods: Mods,
    pub repeat: bool,
}

/// Input for the page. Coordinates are device pixels from the surface's top-left.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        button: Button,
        down: bool,
        x: f32,
        y: f32,
    },
    /// Device pixels to scroll, in Servo's (and egui's) sign: positive scrolls the view
    /// up / left, revealing content above / to the left.
    Wheel {
        dx: f32,
        dy: f32,
        x: f32,
        y: f32,
    },
    MouseLeave,
    Key(KeyEvent),
    /// Typed text (sent as key presses of each character).
    Text(String),
}

/// byob → helper.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Load(Box<Load>),
    Resize(Viewport),
    Input(Input),
    Focus(bool),
    Theme(Theme),
    /// Answered with [`Event::Pong`] once the page's script thread has run a no-op, so a
    /// runaway script shows up as missing pongs.
    Ping(u64),
    Shutdown,
    /// `false`: the app is minimized or in the background — keep the page running but stop
    /// painting and reading back frames; `true`: show it again (a frame follows). Only sent
    /// to helpers that announced [`Features::VISIBILITY`] (older ones would refuse the tag).
    Visible(bool),
    /// A bridge message for the page (JSON text, at most [`MAX_MESSAGE`] bytes): delivered as
    /// a `message` event on its window, as if its parent had posted it. Only sent to helpers
    /// that announced [`Features::BRIDGE`].
    Message(String),
}

/// What a helper supports beyond the base protocol, as bits (see [`Event::Features`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features(pub u32);

impl Features {
    /// Understands [`Request::Visible`].
    pub const VISIBILITY: u32 = 1;
    /// The page bridge: understands [`Request::Message`] and sends [`Event::Message`] for
    /// what the page posts to its parent.
    pub const BRIDGE: u32 = 2;
    /// Everything this version of the protocol knows.
    pub const ALL: Self = Self(Self::VISIBILITY | Self::BRIDGE);

    pub fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
}

/// Mouse cursor the page asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Default,
    Pointer,
    Text,
    Crosshair,
    Move,
    Grab,
    Grabbing,
    NotAllowed,
    None,
}

/// One rendered frame: premultiplied RGBA8, rows top to bottom, `width * height * 4` bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Frame({}x{}, {} bytes)", self.width, self.height, self.rgba.len())
    }
}

/// helper → byob.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Servo is up; [`Request::Load`] may follow (the helper queues it otherwise anyway).
    Ready,
    /// Only sent when the pixels changed.
    Frame(Frame),
    /// The document's height in CSS pixels (sent when it changes).
    ContentHeight(f32),
    /// A script error the page reported (untrusted text).
    Error {
        message: String,
        line: u32,
    },
    Pong(u64),
    Title(String),
    Cursor(Cursor),
    /// Sent right after [`Event::Ready`] by helpers that support more than the base
    /// protocol (a helper that never sends it supports none of it).
    Features(Features),
    /// A bridge message the page posted to its parent (untrusted JSON text, at most
    /// [`MAX_MESSAGE`] bytes; only from helpers with [`Features::BRIDGE`]).
    Message(String),
}

/// A malformed frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad frame: {}", self.0)
    }
}

impl std::error::Error for WireError {}

impl From<WireError> for io::Error {
    fn from(e: WireError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

/// A message that goes in one frame.
pub trait Message: Sized {
    fn encode(&self, out: &mut Enc);
    fn decode(d: &mut Dec<'_>) -> Result<Self, WireError>;

    fn to_bytes(&self) -> Vec<u8> {
        let mut e = Enc(Vec::new());
        self.encode(&mut e);
        e.0
    }

    fn from_bytes(body: &[u8]) -> Result<Self, WireError> {
        let mut d = Dec { buf: body, pos: 0 };
        let m = Self::decode(&mut d)?;
        if d.pos != body.len() {
            return Err(WireError(format!("{} trailing bytes", body.len().saturating_sub(d.pos))));
        }
        Ok(m)
    }
}

/// Write `msg` as one frame (and flush).
pub fn send<M: Message>(w: &mut impl Write, msg: &M) -> io::Result<()> {
    let body = msg.to_bytes();
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("frame of {} bytes is too large", body.len())));
    }
    let len = u32::try_from(body.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Set (to `1`) in the helper's environment by a byob that reads marked frames.
pub const MARKED_ENV: &str = "BYOB_WIRE_MARKED";

/// Starts every marked frame. Never confused with text (0xFF isn't UTF-8) nor with an
/// unmarked header, whose last byte is at most 0x04 ([`MAX_FRAME`] < 2^27) where text and
/// this mark have a printable byte or 0xFF.
pub const MARK: [u8; 4] = [0xFF, b'B', b'Y', b'B'];

/// Longest stretch of stray output passed on at once (longer lines arrive in pieces).
const MAX_NOISE: usize = 4096;

/// Write one frame preceded by [`MARK`], in one write (so a `println!` elsewhere in the
/// process lands before or after it, not inside).
pub fn send_marked<M: Message>(w: &mut impl Write, msg: &M) -> io::Result<()> {
    let body = msg.to_bytes();
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("frame of {} bytes is too large", body.len())));
    }
    let len = u32::try_from(body.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut frame = Vec::with_capacity(body.len().saturating_add(8));
    frame.extend_from_slice(&MARK);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame)?;
    w.flush()
}

/// Reads frames, marked or not, from a stream other things may also write text to.
#[derive(Debug, Default)]
pub struct Reader {
    /// A marked frame has arrived: from now on only marked frames count.
    marked: bool,
    /// Bytes read past a line of stray output that belong to what follows.
    back: Vec<u8>,
}

impl Reader {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next frame; stray output before it goes to `noise` (a line at a time, at most
    /// [`MAX_NOISE`] bytes). `Ok(None)` at a clean end of stream.
    pub fn recv<M: Message>(&mut self, r: &mut impl io::BufRead, noise: &mut impl FnMut(&[u8])) -> io::Result<Option<M>> {
        loop {
            let mut head = [0u8; 4];
            if !self.fill(r, &mut head)? {
                return Ok(None);
            }
            if head == MARK {
                self.marked = true;
                let mut len = [0u8; 4];
                if !self.fill(r, &mut len)? {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                return self.body(r, u32::from_le_bytes(len)).map(Some);
            }
            let len = u32::from_le_bytes(head);
            if !self.marked && usize::try_from(len).is_ok_and(|l| l <= MAX_FRAME) {
                return self.body(r, len).map(Some);
            }
            // Stray output: up to the end of its line. A newline inside the four bytes means
            // what follows it is the next thing on the stream.
            if let Some(i) = head.iter().position(|&b| b == b'\n') {
                let (line, rest) = head.split_at(i.saturating_add(1));
                let mut back = rest.to_vec();
                back.append(&mut self.back);
                self.back = back;
                noise(line);
                continue;
            }
            let mut line = head.to_vec();
            self.until_newline(r, &mut line)?;
            noise(&line);
        }
    }

    /// Exactly `buf.len()` bytes; `false` at a clean end of stream before the first.
    fn fill(&mut self, r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
        let mut got = 0;
        while got < buf.len() {
            if !self.back.is_empty() {
                let n = self.back.len().min(buf.len().saturating_sub(got));
                if let (Some(dst), Some(src)) = (buf.get_mut(got..got.saturating_add(n)), self.back.get(..n)) {
                    dst.copy_from_slice(src);
                }
                self.back.drain(..n);
                got = got.saturating_add(n);
                continue;
            }
            match r.read(buf.get_mut(got..).unwrap_or_default()) {
                Ok(0) if got == 0 => return Ok(false),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => got = got.saturating_add(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    fn body<M: Message>(&mut self, r: &mut impl Read, len: u32) -> io::Result<M> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len > MAX_FRAME {
            return Err(WireError(format!("frame of {len} bytes is over the limit")).into());
        }
        let mut body = vec![0u8; len];
        if len > 0 && !self.fill(r, &mut body)? {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(M::from_bytes(&body)?)
    }

    /// Append to `line` up to and including a newline (or [`MAX_NOISE`] bytes, or the end).
    fn until_newline(&mut self, r: &mut impl io::BufRead, line: &mut Vec<u8>) -> io::Result<()> {
        while line.len() < MAX_NOISE {
            let mut b = [0u8; 1];
            if !self.fill(r, &mut b)? {
                return Ok(());
            }
            line.push(b[0]);
            if b[0] == b'\n' {
                return Ok(());
            }
        }
        Ok(())
    }
}

/// Read one frame; `Ok(None)` at a clean end of stream.
pub fn recv<M: Message>(r: &mut impl Read) -> io::Result<Option<M>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(len.get_mut(got..).unwrap_or_default()) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got = got.saturating_add(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = usize::try_from(u32::from_le_bytes(len)).unwrap_or(usize::MAX);
    if len > MAX_FRAME {
        return Err(WireError(format!("frame of {len} bytes is over the limit")).into());
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(M::from_bytes(&body)?))
}

/// Field writer.
pub struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn bool(&mut self, v: bool) {
        self.0.push(u8::from(v));
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }
    fn bytes(&mut self, v: &[u8]) {
        self.len(v.len());
        self.0.extend_from_slice(v);
    }
    fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
}

/// Field reader over one frame body.
pub struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Dec<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], WireError> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.buf.len()).ok_or_else(|| WireError("truncated".into()))?;
        let s = self.buf.get(self.pos..end).ok_or_else(|| WireError("truncated".into()))?;
        self.pos = end;
        Ok(s)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        self.take(N)?.try_into().map_err(|_| WireError("truncated".into()))
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.array::<1>()?[0])
    }
    fn bool(&mut self) -> Result<bool, WireError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(WireError(format!("bad bool {b}"))),
        }
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn f32(&mut self) -> Result<f32, WireError> {
        let v = f32::from_le_bytes(self.array()?);
        if v.is_finite() { Ok(v) } else { Err(WireError("non-finite number".into())) }
    }
    fn len(&mut self) -> Result<usize, WireError> {
        let n = usize::try_from(self.u32()?).unwrap_or(usize::MAX);
        // Never trust a length beyond what is left in the frame.
        if n > self.buf.len().saturating_sub(self.pos) {
            return Err(WireError("length past the end".into()));
        }
        Ok(n)
    }
    fn bytes(&mut self) -> Result<Vec<u8>, WireError> {
        let n = self.len()?;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Result<String, WireError> {
        String::from_utf8(self.bytes()?).map_err(|_| WireError("invalid UTF-8".into()))
    }
}

/// A bridge message: a string of at most [`MAX_MESSAGE`] bytes.
fn message(d: &mut Dec<'_>) -> Result<String, WireError> {
    let m = d.str()?;
    if m.len() > MAX_MESSAGE {
        return Err(WireError(format!("bridge message of {} bytes is over the limit", m.len())));
    }
    Ok(m)
}

fn bad_tag<T>(what: &str, tag: u8) -> Result<T, WireError> {
    Err(WireError(format!("unknown {what} tag {tag}")))
}

impl Viewport {
    fn encode(&self, e: &mut Enc) {
        e.u32(self.width);
        e.u32(self.height);
        e.f32(self.scale);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, WireError> {
        Ok(Self { width: d.u32()?, height: d.u32()?, scale: d.f32()? })
    }
}

impl Theme {
    fn encode(&self, e: &mut Enc) {
        e.len(self.vars.len());
        for (k, v) in &self.vars {
            e.str(k);
            e.str(v);
        }
        e.bool(self.dark);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, WireError> {
        let n = d.len()?;
        let mut vars = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            vars.push((d.str()?, d.str()?));
        }
        Ok(Self { vars, dark: d.bool()? })
    }
}

impl Button {
    fn tag(self) -> u8 {
        match self {
            Self::Primary => 0,
            Self::Secondary => 1,
            Self::Middle => 2,
        }
    }
    fn from_tag(t: u8) -> Result<Self, WireError> {
        match t {
            0 => Ok(Self::Primary),
            1 => Ok(Self::Secondary),
            2 => Ok(Self::Middle),
            t => bad_tag("button", t),
        }
    }
}

impl Input {
    fn encode(&self, e: &mut Enc) {
        match self {
            Self::MouseMove { x, y } => {
                e.u8(0);
                e.f32(*x);
                e.f32(*y);
            }
            Self::MouseButton { button, down, x, y } => {
                e.u8(1);
                e.u8(button.tag());
                e.bool(*down);
                e.f32(*x);
                e.f32(*y);
            }
            Self::Wheel { dx, dy, x, y } => {
                e.u8(2);
                for v in [dx, dy, x, y] {
                    e.f32(*v);
                }
            }
            Self::MouseLeave => e.u8(3),
            Self::Key(k) => {
                e.u8(4);
                e.bool(k.down);
                e.str(&k.key);
                e.str(&k.code);
                e.u8(k.mods.0);
                e.bool(k.repeat);
            }
            Self::Text(t) => {
                e.u8(5);
                e.str(t);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, WireError> {
        Ok(match d.u8()? {
            0 => Self::MouseMove { x: d.f32()?, y: d.f32()? },
            1 => Self::MouseButton { button: Button::from_tag(d.u8()?)?, down: d.bool()?, x: d.f32()?, y: d.f32()? },
            2 => Self::Wheel { dx: d.f32()?, dy: d.f32()?, x: d.f32()?, y: d.f32()? },
            3 => Self::MouseLeave,
            4 => Self::Key(KeyEvent { down: d.bool()?, key: d.str()?, code: d.str()?, mods: Mods(d.u8()?), repeat: d.bool()? }),
            5 => Self::Text(d.str()?),
            t => return bad_tag("input", t),
        })
    }
}

impl Message for Request {
    fn encode(&self, e: &mut Enc) {
        match self {
            Self::Load(l) => {
                e.u8(0);
                e.str(&l.html);
                l.theme.encode(e);
                l.viewport.encode(e);
            }
            Self::Resize(v) => {
                e.u8(1);
                v.encode(e);
            }
            Self::Input(i) => {
                e.u8(2);
                i.encode(e);
            }
            Self::Focus(f) => {
                e.u8(3);
                e.bool(*f);
            }
            Self::Theme(t) => {
                e.u8(4);
                t.encode(e);
            }
            Self::Ping(n) => {
                e.u8(5);
                e.u64(*n);
            }
            Self::Shutdown => e.u8(6),
            Self::Visible(v) => {
                e.u8(7);
                e.bool(*v);
            }
            Self::Message(m) => {
                e.u8(8);
                e.str(m);
            }
        }
    }

    fn decode(d: &mut Dec<'_>) -> Result<Self, WireError> {
        Ok(match d.u8()? {
            0 => Self::Load(Box::new(Load { html: d.str()?, theme: Theme::decode(d)?, viewport: Viewport::decode(d)? })),
            1 => Self::Resize(Viewport::decode(d)?),
            2 => Self::Input(Input::decode(d)?),
            3 => Self::Focus(d.bool()?),
            4 => Self::Theme(Theme::decode(d)?),
            5 => Self::Ping(d.u64()?),
            6 => Self::Shutdown,
            7 => Self::Visible(d.bool()?),
            8 => Self::Message(message(d)?),
            t => return bad_tag("request", t),
        })
    }
}

const CURSORS: [Cursor; 9] =
    [Cursor::Default, Cursor::Pointer, Cursor::Text, Cursor::Crosshair, Cursor::Move, Cursor::Grab, Cursor::Grabbing, Cursor::NotAllowed, Cursor::None];

impl Message for Event {
    fn encode(&self, e: &mut Enc) {
        match self {
            Self::Ready => e.u8(0),
            Self::Frame(f) => {
                e.u8(1);
                e.u32(f.width);
                e.u32(f.height);
                e.bytes(&f.rgba);
            }
            Self::ContentHeight(h) => {
                e.u8(2);
                e.f32(*h);
            }
            Self::Error { message, line } => {
                e.u8(3);
                e.str(message);
                e.u32(*line);
            }
            Self::Pong(n) => {
                e.u8(4);
                e.u64(*n);
            }
            Self::Title(t) => {
                e.u8(5);
                e.str(t);
            }
            Self::Cursor(c) => {
                e.u8(6);
                e.u8(CURSORS.iter().position(|x| x == c).and_then(|i| u8::try_from(i).ok()).unwrap_or(0));
            }
            Self::Features(f) => {
                e.u8(7);
                e.u32(f.0);
            }
            Self::Message(m) => {
                e.u8(8);
                e.str(m);
            }
        }
    }

    fn decode(d: &mut Dec<'_>) -> Result<Self, WireError> {
        Ok(match d.u8()? {
            0 => Self::Ready,
            1 => {
                let (width, height, rgba) = (d.u32()?, d.u32()?, d.bytes()?);
                let want = u64::from(width).saturating_mul(u64::from(height)).saturating_mul(4);
                if u64::try_from(rgba.len()).ok() != Some(want) {
                    return Err(WireError(format!("{width}x{height} frame with {} bytes", rgba.len())));
                }
                Self::Frame(Frame { width, height, rgba })
            }
            2 => Self::ContentHeight(d.f32()?),
            3 => Self::Error { message: d.str()?, line: d.u32()? },
            4 => Self::Pong(d.u64()?),
            5 => Self::Title(d.str()?),
            6 => {
                let t = d.u8()?;
                Self::Cursor(*CURSORS.get(usize::from(t)).ok_or_else(|| WireError(format!("unknown cursor {t}")))?)
            }
            // Unknown bits are kept (and ignored): a newer helper may know more.
            7 => Self::Features(Features(d.u32()?)),
            8 => Self::Message(message(d)?),
            t => return bad_tag("event", t),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything a [`Reader`] gets from `bytes`: frames, and the stray text it skipped.
    fn read_all(bytes: &[u8]) -> (Vec<Event>, Vec<String>) {
        let mut r = io::BufReader::new(bytes);
        let mut reader = Reader::new();
        let (mut events, mut noise) = (Vec::new(), Vec::new());
        while let Some(e) = reader.recv::<Event>(&mut r, &mut |t| noise.push(String::from_utf8_lossy(t).into_owned())).unwrap() {
            events.push(e);
        }
        (events, noise)
    }

    fn marked(e: &Event) -> Vec<u8> {
        let mut b = Vec::new();
        send_marked(&mut b, e).unwrap();
        b
    }

    #[test]
    fn stray_output_between_marked_frames_is_skipped_and_reported() {
        // surfman prints a warning to stdout on Windows; shorter and longer lines too.
        let mut s = b"surfman: Could not find the NVIDIA and/or AMD GPU selection symbols.\n".to_vec();
        s.extend(marked(&Event::Ready));
        s.extend(b"ok\n");
        s.extend(marked(&Event::Pong(7)));
        s.extend(b"\n");
        s.extend(b"x\r\n");
        s.extend(marked(&Event::Message("{\"a\":1}".into())));
        s.extend(b"no newline at the end");
        let (events, noise) = read_all(&s);
        assert_eq!(events, vec![Event::Ready, Event::Pong(7), Event::Message("{\"a\":1}".into())]);
        assert_eq!(noise, vec!["surfman: Could not find the NVIDIA and/or AMD GPU selection symbols.\n", "ok\n", "\n", "x\r\n", "no newline at the end"]);
    }

    #[test]
    fn unmarked_frames_still_read_until_a_marked_one_arrives() {
        // An older helper doesn't mark its frames.
        let mut s = Vec::new();
        send(&mut s, &Event::Ready).unwrap();
        send(&mut s, &Event::Pong(1)).unwrap();
        assert_eq!(read_all(&s).0, vec![Event::Ready, Event::Pong(1)]);
        // Once frames are marked, text that happens to look like a small header is text.
        let mut s = marked(&Event::Ready);
        s.extend(b"\x05\x00\x00\x00\n");
        s.extend(marked(&Event::Pong(2)));
        let (events, noise) = read_all(&s);
        assert_eq!(events, vec![Event::Ready, Event::Pong(2)]);
        assert_eq!(noise.len(), 1, "{noise:?}");
    }

    #[test]
    fn long_stray_output_comes_in_pieces_and_a_cut_frame_is_an_error() {
        let mut s = vec![b'y'; MAX_NOISE * 2 + 10];
        s.push(b'\n');
        s.extend(marked(&Event::Ready));
        let (events, noise) = read_all(&s);
        assert_eq!(events, vec![Event::Ready]);
        assert_eq!(noise.iter().map(String::len).sum::<usize>(), MAX_NOISE * 2 + 11);
        assert!(noise.iter().all(|n| n.len() <= MAX_NOISE));
        let mut cut = marked(&Event::Message("hello".into()));
        cut.truncate(cut.len() - 2);
        let mut r = io::BufReader::new(cut.as_slice());
        assert!(Reader::new().recv::<Event>(&mut r, &mut |_| {}).is_err());
    }

    fn round_trip<M: Message + PartialEq + fmt::Debug>(m: M) {
        let mut buf = Vec::new();
        send(&mut buf, &m).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(recv::<M>(&mut r).unwrap().as_ref(), Some(&m));
        assert!(recv::<M>(&mut r).unwrap().is_none(), "clean EOF after the frame");
    }

    fn theme() -> Theme {
        Theme { vars: vec![("--byob-bg".into(), "#16171a".into()), ("--byob-fg".into(), "#e4e4e7".into())], dark: true }
    }

    #[test]
    fn requests_round_trip() {
        let vp = Viewport { width: 1200, height: 800, scale: 2.0 };
        round_trip(Request::Load(Box::new(Load { html: "<p>héllo</p>".into(), theme: theme(), viewport: vp })));
        round_trip(Request::Resize(vp));
        round_trip(Request::Focus(true));
        round_trip(Request::Theme(Theme::default()));
        round_trip(Request::Ping(u64::MAX));
        round_trip(Request::Shutdown);
        round_trip(Request::Visible(false));
        round_trip(Request::Visible(true));
        round_trip(Request::Message(r#"{"jsonrpc":"2.0","method":"ui/notifications/tool-input","params":{"arguments":{"q":"é"}}}"#.into()));
        for input in [
            Input::MouseMove { x: 1.5, y: -2.0 },
            Input::MouseButton { button: Button::Secondary, down: true, x: 3.0, y: 4.0 },
            Input::Wheel { dx: 0.0, dy: 38.0, x: 10.0, y: 10.0 },
            Input::MouseLeave,
            Input::Key(KeyEvent { down: false, key: "ArrowUp".into(), code: "ArrowUp".into(), mods: Mods(Mods::SHIFT | Mods::CTRL), repeat: true }),
            Input::Text("añ😀".into()),
        ] {
            round_trip(Request::Input(input));
        }
    }

    #[test]
    fn events_round_trip() {
        round_trip(Event::Ready);
        round_trip(Event::Frame(Frame { width: 2, height: 1, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] }));
        round_trip(Event::ContentHeight(512.5));
        round_trip(Event::Error { message: "x is not defined".into(), line: 12 });
        round_trip(Event::Pong(7));
        round_trip(Event::Title("Loan".into()));
        round_trip(Event::Features(Features::ALL));
        round_trip(Event::Features(Features(u32::MAX)));
        round_trip(Event::Message(r#"{"jsonrpc":"2.0","id":1,"method":"ui/initialize","params":{}}"#.into()));
        for c in CURSORS {
            round_trip(Event::Cursor(c));
        }
    }

    #[test]
    fn hostile_frames_are_refused() {
        // Length over the cap: refused without allocating it.
        let mut r: &[u8] = &u32::MAX.to_le_bytes();
        assert_eq!(recv::<Event>(&mut r).unwrap_err().kind(), io::ErrorKind::InvalidData);
        // Truncated length prefix and body.
        let mut r: &[u8] = &[1, 0];
        assert!(recv::<Event>(&mut r).is_err());
        let mut r: &[u8] = &[5, 0, 0, 0, 1];
        assert!(recv::<Event>(&mut r).is_err());
        for body in [
            vec![99],                                    // unknown tag
            vec![1, 2, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0], // frame with the wrong byte count
            vec![5, 255, 255, 255, 255],                 // string length past the end
            vec![5, 1, 0, 0, 0, 0xff],                   // invalid UTF-8
            vec![2, 0, 0, 0xc0, 0x7f],                   // NaN height
            vec![6, 200],                                // unknown cursor
            vec![0, 0],                                  // trailing byte
        ] {
            assert!(Event::from_bytes(&body).is_err(), "{body:?}");
        }
        assert!(Request::from_bytes(&[2, 4, 7]).is_err(), "bad bool");
        assert!(Request::from_bytes(&[]).is_err());
    }

    /// The additions are new tags only: every frame the base protocol had decodes as before.
    #[test]
    fn visibility_is_additive() {
        assert_eq!(Request::Shutdown.to_bytes(), vec![6]);
        assert_eq!(Request::Visible(true).to_bytes(), vec![7, 1]);
        assert_eq!(Event::Features(Features(Features::VISIBILITY)).to_bytes(), vec![7, 1, 0, 0, 0]);
        assert_eq!(Event::Features(Features::ALL).to_bytes(), vec![7, 3, 0, 0, 0]);
        assert!(Request::from_bytes(&[7]).is_err(), "Visible needs its flag");
        assert!(Request::from_bytes(&[7, 2]).is_err(), "bad bool");
        assert!(Event::from_bytes(&[7, 1, 0]).is_err(), "short features");
        assert!(Features(Features::VISIBILITY | 8).has(Features::VISIBILITY) && !Features::default().has(Features::VISIBILITY));
    }

    /// The bridge is new tags only, negotiated by its feature bit; messages are capped.
    #[test]
    fn bridge_is_additive_and_capped() {
        assert_eq!(Request::Message("{}".into()).to_bytes(), vec![8, 2, 0, 0, 0, b'{', b'}']);
        assert_eq!(Event::Message("{}".into()).to_bytes(), vec![8, 2, 0, 0, 0, b'{', b'}']);
        // An older helper announced only VISIBILITY: no bridge.
        assert!(!Features(Features::VISIBILITY).has(Features::BRIDGE) && Features::ALL.has(Features::BRIDGE));
        let big = "x".repeat(MAX_MESSAGE + 1);
        assert!(Event::from_bytes(&Event::Message(big.clone()).to_bytes()).is_err(), "over the cap");
        assert!(Request::from_bytes(&Request::Message(big).to_bytes()).is_err());
        let fits = "x".repeat(MAX_MESSAGE);
        assert!(Event::from_bytes(&Event::Message(fits).to_bytes()).is_ok());
        assert!(Event::from_bytes(&[8, 1, 0, 0, 0, 0xff]).is_err(), "invalid UTF-8");
    }

    #[test]
    fn mods() {
        let m = Mods(Mods::ALT | Mods::META);
        assert!(m.has(Mods::ALT) && m.has(Mods::META) && !m.has(Mods::SHIFT));
    }
}
