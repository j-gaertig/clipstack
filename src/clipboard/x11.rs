//! X11 side, also used through XWayland on Wayland compositors without
//! layer-shell or data-control (GNOME): XFixes reports clipboard changes, the
//! selection is read with `ConvertSelection` (INCR for large data) and owned
//! again when an entry is restored. XTest types the paste shortcut and a
//! passive key grab provides Super+V where the desktop doesn't.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, mpsc},
    time::Duration,
};

use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        Event as XEvent,
        randr::ConnectionExt as _,
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, CreateWindowAux,
            EventMask, GrabMode, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, Keycode, ModMask, PropMode, Property,
            SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use super::{
    Command, CommandSender, CursorPos, Event, EventSink, Formats, MARKER_MIME, MAX_FORMAT_BYTES, Screen, is_terminal,
    wanted_mimes,
};
use crate::history;

/// Larger payloads are sent in INCR chunks of this size.
const CHUNK: usize = 256 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

const XK_V: u32 = 0x0076;
const XK_CONTROL_L: u32 = 0xffe3;
const XK_SHIFT_L: u32 = 0xffe1;

type Error = Box<dyn std::error::Error>;

enum Msg {
    Cmd(Command),
    Paste,
    ReadTimeout(u64),
}

enum Stage {
    Targets,
    Data(String),
}

/// A clipboard read in progress: first the list of targets, then each wanted
/// format in turn.
struct Reading {
    generation: u64,
    time: u32,
    stage: Stage,
    queue: VecDeque<String>,
    incr: Option<Vec<u8>>,
    results: Vec<(String, Vec<u8>)>,
}

/// An INCR transfer to another client: the next chunk goes out whenever the
/// requestor deletes the property.
struct Outgoing {
    requestor: Window,
    property: Atom,
    target: Atom,
    data: Arc<Vec<u8>>,
    offset: usize,
}

struct State {
    conn: Arc<RustConnection>,
    root: Window,
    win: Window,
    clipboard: Atom,
    targets: Atom,
    incr: Atom,
    prop: Atom,
    wake: Atom,
    atoms: HashMap<String, Atom>,
    names: HashMap<Atom, String>,
    tx: mpsc::Sender<Msg>,
    sink: EventSink,
    owned: Option<Formats>,
    reading: Option<Reading>,
    generation: u64,
    outgoing: Vec<Outgoing>,
    ctrl: Option<Keycode>,
    shift: Option<Keycode>,
    v: Option<Keycode>,
    hotkey: Option<Keycode>,
    last_release: u32,
}

pub fn spawn(sink: EventSink) {
    std::thread::Builder::new()
        .name("x11-clipboard".into())
        .spawn(move || {
            if let Err(err) = run(sink.clone()) {
                tracing::error!("clipboard backend failed: {err}");
                sink(Event::Failed(format!("X11: {err}")));
            }
        })
        .expect("failed to spawn clipboard thread");
}

fn run(sink: EventSink) -> Result<(), Error> {
    let (conn, screen) = RustConnection::connect(None)?;
    let conn = Arc::new(conn);
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id()?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let intern = |name: &str| -> Result<Atom, Error> { Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom) };
    let clipboard = intern("CLIPBOARD")?;
    conn.xfixes_query_version(5, 0)?.reply()?;
    conn.xfixes_select_selection_input(
        win,
        clipboard,
        SelectionEventMask::SET_SELECTION_OWNER
            | SelectionEventMask::SELECTION_WINDOW_DESTROY
            | SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )?;

    let (tx, rx) = mpsc::channel();
    let mut state = State {
        root,
        win,
        clipboard,
        targets: intern("TARGETS")?,
        incr: intern("INCR")?,
        prop: intern("CLIPSTACK_SELECTION")?,
        wake: intern("CLIPSTACK_WAKE")?,
        conn: conn.clone(),
        atoms: HashMap::new(),
        names: HashMap::new(),
        tx: tx.clone(),
        sink: sink.clone(),
        owned: None,
        reading: None,
        generation: 0,
        outgoing: Vec::new(),
        ctrl: None,
        shift: None,
        v: None,
        hotkey: None,
        last_release: 0,
    };
    state.load_keycodes()?;
    if crate::setup::runtime_shortcut() && !state.grab_hotkey() {
        tracing::info!("Super+V is taken on this X server; trying the global shortcut portal");
        super::portal::spawn_shortcut(sink.clone());
    }
    conn.flush()?;

    let (wake_conn, wake) = (conn.clone(), state.wake);
    sink(Event::Ready(CommandSender::new(move |cmd| {
        tx.send(Msg::Cmd(cmd)).is_ok() && send_wake(&wake_conn, win, wake).is_ok()
    })));
    tracing::info!("clipboard backend ready (X11)");

    // Whatever is on the clipboard already counts as the latest entry.
    if conn.get_selection_owner(clipboard)?.reply()?.owner != NONE {
        state.start_read(CURRENT_TIME)?;
    }
    loop {
        let event = conn.wait_for_event()?;
        state.on_event(event)?;
        while let Some(event) = conn.poll_for_event()? {
            state.on_event(event)?;
        }
        while let Ok(msg) = rx.try_recv() {
            state.on_msg(msg)?;
        }
        conn.flush()?;
    }
}

/// Wakes up the event loop: a client message to our own window.
fn send_wake(conn: &RustConnection, win: Window, wake: Atom) -> Result<(), Error> {
    conn.send_event(false, win, EventMask::NO_EVENT, ClientMessageEvent::new(32, win, wake, [0u32; 5]))?;
    conn.flush()?;
    Ok(())
}

impl State {
    fn atom(&mut self, name: &str) -> Result<Atom, Error> {
        if let Some(atom) = self.atoms.get(name) {
            return Ok(*atom);
        }
        let atom = self.conn.intern_atom(false, name.as_bytes())?.reply()?.atom;
        self.atoms.insert(name.to_string(), atom);
        self.names.insert(atom, name.to_string());
        Ok(atom)
    }

    fn atom_name(&mut self, atom: Atom) -> Result<String, Error> {
        if let Some(name) = self.names.get(&atom) {
            return Ok(name.clone());
        }
        let name = String::from_utf8_lossy(&self.conn.get_atom_name(atom)?.reply()?.name).into_owned();
        self.names.insert(atom, name.clone());
        self.atoms.insert(name.clone(), atom);
        Ok(name)
    }

    /// Sends `msg` to the event loop after `delay`.
    fn later(&self, delay: Duration, msg: Msg) {
        let (tx, conn, win, wake) = (self.tx.clone(), self.conn.clone(), self.win, self.wake);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            if tx.send(msg).is_ok() {
                let _ = send_wake(&conn, win, wake);
            }
        });
    }

    fn load_keycodes(&mut self) -> Result<(), Error> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = map.keysyms_per_keycode.max(1) as usize;
        let find = |sym: u32| map.keysyms.chunks(per).position(|syms| syms.contains(&sym)).map(|i| min + i as u8);
        (self.ctrl, self.shift, self.v) = (find(XK_CONTROL_L), find(XK_SHIFT_L), find(XK_V));
        Ok(())
    }

    /// Grabs Super+V (with any combination of Caps Lock and Num Lock).
    fn grab_hotkey(&mut self) -> bool {
        let Some(v) = self.v else { return false };
        let ok =
            [ModMask::from(0u16), ModMask::LOCK, ModMask::M2, ModMask::LOCK | ModMask::M2].into_iter().all(|extra| {
                self.conn
                    .grab_key(false, self.root, ModMask::M4 | extra, v, GrabMode::ASYNC, GrabMode::ASYNC)
                    .is_ok_and(|c| c.check().is_ok())
            });
        if ok {
            self.hotkey = Some(v);
        } else {
            let _ = self.conn.ungrab_key(v, self.root, ModMask::ANY);
        }
        ok
    }

    fn on_msg(&mut self, msg: Msg) -> Result<(), Error> {
        match msg {
            Msg::Cmd(Command::Set { formats }) => {
                tracing::debug!("taking the clipboard ({} formats)", formats.len());
                self.owned = Some(formats);
                self.conn.set_selection_owner(self.win, self.clipboard, CURRENT_TIME)?;
            }
            Msg::Cmd(Command::Restore { formats }) => {
                if self.conn.get_selection_owner(self.clipboard)?.reply()?.owner == NONE {
                    return self.on_msg(Msg::Cmd(Command::Set { formats }));
                }
            }
            Msg::Cmd(Command::Paste) => {
                let delay =
                    std::env::var("COSMIC_CLIPBOARD_PASTE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(160);
                self.later(Duration::from_millis(delay), Msg::Paste);
            }
            Msg::Cmd(Command::LocateCursor) => {
                let cursor = self.locate().inspect_err(|e| tracing::debug!("locate: {e}")).ok();
                (self.sink)(Event::Cursor(cursor));
            }
            Msg::Paste => self.paste()?,
            Msg::ReadTimeout(generation) => {
                if self.reading.as_ref().is_some_and(|r| r.generation == generation) {
                    tracing::debug!("clipboard read timed out");
                    self.finish_read();
                }
            }
        }
        Ok(())
    }

    fn on_event(&mut self, event: XEvent) -> Result<(), Error> {
        match event {
            XEvent::XfixesSelectionNotify(e) if e.selection == self.clipboard => {
                if e.owner == self.win {
                    return Ok(());
                }
                if e.owner == NONE {
                    if self.owned.is_none() {
                        (self.sink)(Event::Cleared);
                    }
                    return Ok(());
                }
                self.owned = None;
                self.start_read(e.timestamp)?;
            }
            XEvent::SelectionNotify(e) if e.requestor == self.win && e.selection == self.clipboard => {
                if e.property == NONE {
                    self.got_value(None)?;
                } else {
                    let reply =
                        self.conn.get_property(true, self.win, self.prop, AtomEnum::ANY, 0, u32::MAX / 4)?.reply()?;
                    if reply.type_ == self.incr {
                        tracing::debug!("incremental transfer started");
                        if let Some(r) = &mut self.reading {
                            r.incr = Some(Vec::new());
                        }
                    } else {
                        self.got_value(Some(reply.value))?;
                    }
                }
            }
            XEvent::PropertyNotify(e)
                if e.window == self.win && e.atom == self.prop && e.state == Property::NEW_VALUE =>
            {
                if self.reading.as_ref().is_some_and(|r| r.incr.is_some()) {
                    let chunk = self
                        .conn
                        .get_property(true, self.win, self.prop, AtomEnum::ANY, 0, u32::MAX / 4)?
                        .reply()?
                        .value;
                    let r = self.reading.as_mut().unwrap();
                    let buf = r.incr.as_mut().unwrap();
                    if chunk.is_empty() {
                        tracing::debug!("incremental transfer done ({} bytes)", buf.len());
                        let data = r.incr.take();
                        self.got_value(data)?;
                    } else if buf.len() + chunk.len() > MAX_FORMAT_BYTES {
                        r.incr = None;
                        self.got_value(None)?;
                    } else {
                        buf.extend_from_slice(&chunk);
                    }
                }
            }
            XEvent::PropertyNotify(e) if e.state == Property::DELETE => {
                if let Some(i) = self.outgoing.iter().position(|o| o.requestor == e.window && o.property == e.atom) {
                    let o = &mut self.outgoing[i];
                    let end = (o.offset + CHUNK).min(o.data.len());
                    self.conn.change_property8(
                        PropMode::REPLACE,
                        o.requestor,
                        o.property,
                        o.target,
                        &o.data[o.offset..end],
                    )?;
                    if o.offset == end {
                        self.outgoing.remove(i);
                    } else {
                        o.offset = end;
                    }
                }
            }
            XEvent::SelectionRequest(e) => self.on_request(e)?,
            XEvent::SelectionClear(e) if e.selection == self.clipboard => self.owned = None,
            XEvent::KeyPress(e) if Some(e.detail) == self.hotkey => {
                // Auto-repeat sends release and press with the same time.
                if e.time != self.last_release {
                    (self.sink)(Event::Toggle);
                }
            }
            XEvent::KeyRelease(e) if Some(e.detail) == self.hotkey => self.last_release = e.time,
            XEvent::Error(e) => tracing::debug!("X11 error: {e:?}"),
            _ => {}
        }
        Ok(())
    }

    fn start_read(&mut self, time: u32) -> Result<(), Error> {
        self.generation += 1;
        self.reading = Some(Reading {
            generation: self.generation,
            time,
            stage: Stage::Targets,
            queue: VecDeque::new(),
            incr: None,
            results: Vec::new(),
        });
        self.conn.convert_selection(self.win, self.clipboard, self.targets, self.prop, time)?;
        self.later(READ_TIMEOUT, Msg::ReadTimeout(self.generation));
        Ok(())
    }

    /// A requested target arrived (`None`: refused or failed).
    fn got_value(&mut self, value: Option<Vec<u8>>) -> Result<(), Error> {
        let Some(r) = self.reading.as_mut() else { return Ok(()) };
        match std::mem::replace(&mut r.stage, Stage::Targets) {
            Stage::Targets => {
                let value = value.unwrap_or_default();
                let atoms: Vec<Atom> = value.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes(*c)).collect();
                let mut mimes = Vec::new();
                for atom in atoms {
                    mimes.push(self.atom_name(atom)?);
                }
                tracing::debug!("new selection: {mimes:?}");
                let wanted = if mimes.iter().any(|m| m == MARKER_MIME) { Vec::new() } else { wanted_mimes(&mimes) };
                self.reading.as_mut().unwrap().queue = wanted.into();
            }
            Stage::Data(mime) => {
                if let Some(data) = value.filter(|d| !d.is_empty() && d.len() <= MAX_FORMAT_BYTES) {
                    r.results.push((mime, data));
                }
            }
        }
        self.next_target()
    }

    fn next_target(&mut self) -> Result<(), Error> {
        let Some(next) = self.reading.as_mut().and_then(|r| r.queue.pop_front()) else {
            self.finish_read();
            return Ok(());
        };
        let target = self.atom(&next)?;
        let r = self.reading.as_mut().unwrap();
        r.stage = Stage::Data(next);
        self.conn.convert_selection(self.win, self.clipboard, target, self.prop, r.time)?;
        Ok(())
    }

    fn finish_read(&mut self) {
        let Some(reading) = self.reading.take() else { return };
        if reading.results.is_empty() {
            return;
        }
        let sink = self.sink.clone();
        std::thread::spawn(move || {
            if let Some(ingested) = history::ingest(reading.results) {
                sink(Event::Captured(Arc::new(ingested)));
            }
        });
    }

    fn on_request(&mut self, e: SelectionRequestEvent) -> Result<(), Error> {
        // Obsolete clients leave the property empty and mean the target.
        let property = if e.property == NONE { e.target } else { e.property };
        let target = self.atom_name(e.target)?;
        let mut reply = NONE;
        if let Some(formats) = self.owned.clone().filter(|_| e.selection == self.clipboard) {
            if target == "TARGETS" {
                let mut atoms = vec![self.targets, self.atom("TIMESTAMP")?, self.atom(MARKER_MIME)?];
                for (mime, _) in formats.iter() {
                    atoms.push(self.atom(mime)?);
                }
                self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &atoms)?;
                reply = property;
            } else if target == "TIMESTAMP" {
                self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::INTEGER, &[e.time])?;
                reply = property;
            } else if let Some((_, data)) = formats.iter().find(|(m, _)| *m == target) {
                if data.len() > CHUNK {
                    self.conn.change_window_attributes(
                        e.requestor,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                    )?;
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        e.requestor,
                        property,
                        self.incr,
                        &[data.len() as u32],
                    )?;
                    self.outgoing.retain(|o| !(o.requestor == e.requestor && o.property == property));
                    self.outgoing.push(Outgoing {
                        requestor: e.requestor,
                        property,
                        target: e.target,
                        data: data.clone(),
                        offset: 0,
                    });
                } else {
                    self.conn.change_property8(PropMode::REPLACE, e.requestor, property, e.target, data)?;
                }
                reply = property;
            }
        }
        tracing::debug!("selection request for {target}: {}", if reply == NONE { "refused" } else { "sent" });
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: reply,
        };
        self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
        Ok(())
    }

    /// WM_CLASS of the active window, to recognise terminals.
    fn focused_app_is_terminal(&mut self) -> Result<bool, Error> {
        let active = self.atom("_NET_ACTIVE_WINDOW")?;
        let reply = self.conn.get_property(false, self.root, active, AtomEnum::WINDOW, 0, 1)?.reply()?;
        let Some(window) = reply.value32().and_then(|mut v| v.next()).filter(|w| *w != NONE) else {
            return Ok(false);
        };
        let class = self.conn.get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)?.reply()?;
        let class = String::from_utf8_lossy(&class.value).into_owned();
        tracing::debug!("focused app: {class:?}");
        Ok(class.split('\0').any(|c| !c.is_empty() && is_terminal(c)))
    }

    fn paste(&mut self) -> Result<(), Error> {
        let shift = self.focused_app_is_terminal().unwrap_or(false);
        // XTest only reaches X11 windows; under a Wayland session the
        // compositor has to type the shortcut.
        if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") {
            super::portal::paste(shift);
            return Ok(());
        }
        let (Some(ctrl), Some(v)) = (self.ctrl, self.v) else {
            tracing::warn!("cannot simulate paste: no keycode for Ctrl or V");
            return Ok(());
        };
        let mut keys = vec![ctrl];
        keys.extend(self.shift.filter(|_| shift));
        keys.push(v);
        for key in &keys {
            self.conn.xtest_fake_input(KEY_PRESS_EVENT, *key, CURRENT_TIME, self.root, 0, 0, 0)?;
        }
        for key in keys.iter().rev() {
            self.conn.xtest_fake_input(KEY_RELEASE_EVENT, *key, CURRENT_TIME, self.root, 0, 0, 0)?;
        }
        Ok(())
    }

    /// Pointer position and the work area of the monitor it is on.
    fn locate(&mut self) -> Result<CursorPos, Error> {
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        let (px, py) = (pointer.root_x as i32, pointer.root_y as i32);
        let screen = &self.conn.setup().roots.iter().find(|s| s.root == self.root).cloned();
        let mut area =
            screen.as_ref().map_or((0, 0, 1920, 1080), |s| (0, 0, s.width_in_pixels as i32, s.height_in_pixels as i32));
        if let Ok(monitors) =
            self.conn.randr_get_monitors(self.root, true).map_err(Error::from).and_then(|c| Ok(c.reply()?))
        {
            let rects = monitors.monitors.iter().map(|m| (m.x as i32, m.y as i32, m.width as i32, m.height as i32));
            if let Some(m) = rects.clone().find(|(x, y, w, h)| (*x..x + w).contains(&px) && (*y..y + h).contains(&py)) {
                area = m;
            }
        }
        // Leave out panels and docks (`_NET_WORKAREA` spans all monitors).
        let workarea = self.atom("_NET_WORKAREA")?;
        if let Some(w) =
            self.conn.get_property(false, self.root, workarea, AtomEnum::CARDINAL, 0, 4)?.reply()?.value32()
        {
            let w: Vec<i32> = w.map(|v| v as i32).collect();
            if let [wx, wy, ww, wh] = w[..] {
                let (x0, y0) = (area.0.max(wx), area.1.max(wy));
                let (x1, y1) = ((area.0 + area.2).min(wx + ww), (area.1 + area.3).min(wy + wh));
                if x1 > x0 && y1 > y0 {
                    area = (x0, y0, x1 - x0, y1 - y0);
                }
            }
        }
        Ok(CursorPos {
            output: None,
            x: px as f32,
            y: py as f32,
            above: None,
            screen: Some(Screen {
                x: area.0 as f32,
                y: area.1 as f32,
                width: area.2 as f32,
                height: area.3 as f32,
                scale: self.scale(),
            }),
        })
    }

    /// `Xft.dpi` / 96, the scale factor winit uses for new windows.
    fn scale(&self) -> f32 {
        if let Some(scale) = std::env::var("WINIT_X11_SCALE_FACTOR").ok().and_then(|v| v.parse().ok()) {
            return scale;
        }
        let Ok(reply) = self
            .conn
            .get_property(false, self.root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16)
            .map_err(Error::from)
            .and_then(|c| Ok(c.reply()?))
        else {
            return 1.0;
        };
        String::from_utf8_lossy(&reply.value)
            .lines()
            .find_map(|l| l.strip_prefix("Xft.dpi:")?.trim().parse::<f32>().ok())
            .map_or(1.0, |dpi| (dpi / 96.0).max(0.5))
    }
}
