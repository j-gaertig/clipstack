//! Wayland side: watches the clipboard via `ext-data-control-v1` (or
//! `wlr-data-control-unstable-v1`), owns the selection when an entry is
//! restored and simulates the paste shortcut via `zwp-virtual-keyboard-v1`
//! (or the remote desktop portal where that is missing).

use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use cosmic::cctk::{
    self,
    cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1,
    sctk::{
        delegate_output, delegate_registry, delegate_seat,
        output::{OutputHandler, OutputInfo, OutputState},
        reexports::{
            calloop::{
                self, EventLoop,
                channel::{self, Channel},
                timer::{TimeoutAction, Timer},
            },
            calloop_wayland_source::WaylandSource,
            client::{
                Connection, Dispatch, Proxy, QueueHandle, event_created_child,
                globals::{GlobalListContents, registry_queue_init},
                protocol::{
                    wl_callback::{self, WlCallback},
                    wl_compositor::WlCompositor,
                    wl_output::{self, WlOutput},
                    wl_pointer::WlPointer,
                    wl_registry::{self, WlRegistry},
                    wl_seat::WlSeat,
                    wl_surface::WlSurface,
                },
            },
            protocols_misc::zwp_input_method_v2::client::{
                zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
                zwp_input_method_v2::{self, ZwpInputMethodV2},
                zwp_input_popup_surface_v2::{self, ZwpInputPopupSurfaceV2},
            },
            protocols_misc::zwp_virtual_keyboard_v1::client::{
                zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
                zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
            },
            protocols_wlr::data_control::v1::client::{
                zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
                zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
                zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
                zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
            },
        },
        registry::{ProvidesRegistryState, RegistryState},
        registry_handlers,
        seat::{Capability, SeatHandler, SeatState},
    },
    toplevel_info::{ToplevelInfoHandler, ToplevelInfoState},
    wayland_protocols::ext::{
        data_control::v1::client::{
            ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
            ext_data_control_manager_v1::ExtDataControlManagerV1,
            ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
            ext_data_control_source_v1::{self, ExtDataControlSourceV1},
        },
        foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        image_capture_source::v1::client::{
            ext_image_capture_source_v1::ExtImageCaptureSourceV1,
            ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
        },
        image_copy_capture::v1::client::{
            ext_image_copy_capture_cursor_session_v1::{self, ExtImageCopyCaptureCursorSessionV1},
            ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
        },
    },
};

use super::{
    Command, CommandSender, CursorPos, Event, EventSink, Formats, MARKER_MIME, MAX_FORMAT_BYTES, is_terminal,
    wanted_mimes,
};
use crate::history;

/// Our own tiny keymap for the virtual keyboard, so the simulated shortcut
/// is independent of the user's layout. The seat keymap can't be reused: it
/// may currently be the keymap of another virtual keyboard.
const PASTE_KEYMAP: &str = r#"xkb_keymap {
    xkb_keycodes "cosmic-clipboard" {
        minimum = 8;
        maximum = 255;
        <LCTL> = 37;
        <LFSH> = 50;
        <AB04> = 55;
    };
    xkb_types "cosmic-clipboard" { include "complete" };
    xkb_compatibility "cosmic-clipboard" { include "complete" };
    xkb_symbols "cosmic-clipboard" {
        key <LCTL> { [ Control_L ] };
        key <LFSH> { [ Shift_L ] };
        key <AB04> { [ v, V ] };
        modifier_map Control { <LCTL> };
        modifier_map Shift { <LFSH> };
    };
};
"#;

fn paste_keymap() -> std::io::Result<(OwnedFd, u32)> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("cosmic-clipboard-keymap-{}", std::process::id()));
    let mut file = File::options().read(true).write(true).create(true).truncate(true).open(&path)?;
    let _ = std::fs::remove_file(&path);
    let mut data = PASTE_KEYMAP.as_bytes().to_vec();
    data.push(0);
    file.write_all(&data)?;
    Ok((file.into(), data.len() as u32))
}

pub(super) const KEY_LEFTCTRL: u32 = 29;
pub(super) const KEY_LEFTSHIFT: u32 = 42;
pub(super) const KEY_V: u32 = 47;

#[derive(Default)]
struct OfferData(Mutex<Vec<String>>);

struct SourceData(Formats);

/// The data-control objects of either protocol version: `ext` (current) or
/// `wlr` (older KDE Plasma and wlroots compositors).
#[derive(Clone, PartialEq)]
enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

#[derive(Clone, PartialEq)]
enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

#[derive(Clone, PartialEq)]
enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

#[derive(Clone, PartialEq)]
enum Source {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl Manager {
    fn get_data_device(&self, seat: &WlSeat, qh: &QueueHandle<State>) -> Device {
        match self {
            Self::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
            Self::Wlr(m) => Device::Wlr(m.get_data_device(seat, qh, ())),
        }
    }

    fn create_data_source(&self, qh: &QueueHandle<State>, data: SourceData) -> Source {
        match self {
            Self::Ext(m) => Source::Ext(m.create_data_source(qh, data)),
            Self::Wlr(m) => Source::Wlr(m.create_data_source(qh, data)),
        }
    }
}

impl Device {
    fn set_selection(&self, source: &Source) {
        match (self, source) {
            (Self::Ext(d), Source::Ext(s)) => d.set_selection(Some(s)),
            (Self::Wlr(d), Source::Wlr(s)) => d.set_selection(Some(s)),
            _ => {}
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(d) => d.destroy(),
            Self::Wlr(d) => d.destroy(),
        }
    }
}

impl Offer {
    fn mimes(&self) -> Vec<String> {
        let data = match self {
            Self::Ext(o) => o.data::<OfferData>(),
            Self::Wlr(o) => o.data::<OfferData>(),
        };
        data.map(|d| d.0.lock().unwrap().clone()).unwrap_or_default()
    }

    fn receive(&self, mime: String, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(o) => o.receive(mime, fd),
            Self::Wlr(o) => o.receive(mime, fd),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Ext(o) => o.destroy(),
            Self::Wlr(o) => o.destroy(),
        }
    }
}

impl Source {
    fn offer(&self, mime: String) {
        match self {
            Self::Ext(s) => s.offer(mime),
            Self::Wlr(s) => s.offer(mime),
        }
    }
}

/// Whether the compositor has everything the overlay needs (layer-shell and
/// a data-control protocol). `None` if it can't be reached at all.
pub fn probe() -> Option<bool> {
    struct Probe;
    impl Dispatch<WlRegistry, GlobalListContents> for Probe {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    let conn = Connection::connect_to_env().ok()?;
    let (globals, _queue) = registry_queue_init::<Probe>(&conn).ok()?;
    let names: Vec<String> = globals.contents().with_list(|l| l.iter().map(|g| g.interface.clone()).collect());
    let has = |name: &str| names.iter().any(|n| n == name);
    Some(has("zwlr_layer_shell_v1") && (has("ext_data_control_manager_v1") || has("zwlr_data_control_manager_v1")))
}

/// Wayland has no API to query the pointer position, but a cursor capture
/// session reports it right away, even when the pointer doesn't move.
struct CursorProbe {
    sessions: Vec<(ExtImageCopyCaptureCursorSessionV1, ExtImageCaptureSourceV1)>,
    found: Option<CursorPos>,
    /// Temporary input method: reports the text caret of the focused text
    /// field (relative to its window) through an input popup surface.
    ime: Option<(ZwpInputMethodV2, WlSurface, ZwpInputPopupSurfaceV2)>,
    text_active: bool,
    caret: Option<(i32, i32, i32, i32)>,
    /// The roundtrips after binding are done (pointer position is known).
    synced: bool,
    /// The input method reported a complete state with an active text field.
    caret_done: bool,
    timed_out: bool,
    generation: u64,
}

/// cosmic-comp activates a freshly bound input method a moment later, not
/// within the binding roundtrip; without a text field nothing comes at all.
const CARET_WAIT: Duration = Duration::from_millis(90);

struct State {
    conn: Connection,
    qh: QueueHandle<State>,
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    capture_sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    capture: Option<ExtImageCopyCaptureManagerV1>,
    pointer: Option<WlPointer>,
    probe: Option<CursorProbe>,
    toplevels: Option<ToplevelInfoState>,
    manager: Manager,
    vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
    ime_manager: Option<ZwpInputMethodManagerV2>,
    compositor: Option<WlCompositor>,
    seat: Option<WlSeat>,
    device: Option<Device>,
    vk: Option<ZwpVirtualKeyboardV1>,
    offer: Option<Offer>,
    source: Option<Source>,
    sink: EventSink,
    started: Instant,
    pending_paste: bool,
    pending_locate: bool,
    loop_handle: calloop::LoopHandle<'static, State>,
    probe_generation: u64,
}

pub fn spawn(sink: EventSink) {
    std::thread::Builder::new()
        .name("wayland-clipboard".into())
        .spawn(move || {
            if let Err(err) = run(sink.clone()) {
                tracing::error!("clipboard backend failed: {err}");
                sink(Event::Failed(err));
            }
        })
        .expect("failed to spawn clipboard thread");
}

fn run(sink: EventSink) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("wayland connect: {e}"))?;
    let (globals, queue) = registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();

    let manager = globals
        .bind(&qh, 1..=1, ())
        .map(Manager::Ext)
        .or_else(|_| globals.bind(&qh, 1..=2, ()).map(Manager::Wlr))
        .map_err(|_| "The compositor does not support ext-data-control-v1".to_string())?;
    let vk_manager: Option<ZwpVirtualKeyboardManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let ime_manager: Option<ZwpInputMethodManagerV2> = globals.bind(&qh, 1..=1, ()).ok();
    let compositor: Option<WlCompositor> = globals.bind(&qh, 1..=5, ()).ok();
    if vk_manager.is_none() {
        tracing::info!("zwp_virtual_keyboard_manager_v1 unavailable; pasting through the remote desktop portal");
    }

    let registry = RegistryState::new(&globals);
    let seats = SeatState::new(&globals, &qh);
    let outputs = OutputState::new(&globals, &qh);
    let capture_sources = globals.bind(&qh, 1..=1, ()).ok();
    let capture = globals.bind(&qh, 1..=1, ()).ok();
    let toplevels = ToplevelInfoState::try_new(&registry, &qh);

    let mut event_loop: EventLoop<State> = EventLoop::try_new().map_err(|e| format!("event loop: {e}"))?;
    let handle = event_loop.handle();
    WaylandSource::new(conn.clone(), queue).insert(handle.clone()).map_err(|e| format!("wayland source: {e}"))?;

    let (tx, rx): (channel::Sender<Command>, Channel<Command>) = channel::channel();
    handle
        .insert_source(rx, |event, _, state: &mut State| {
            if let channel::Event::Msg(cmd) = event {
                state.handle_command(cmd);
            }
        })
        .map_err(|e| format!("channel: {e}"))?;

    let mut state = State {
        conn,
        qh,
        registry,
        seats,
        outputs,
        capture_sources,
        capture,
        pointer: None,
        probe: None,
        toplevels,
        manager,
        vk_manager,
        ime_manager,
        compositor,
        seat: None,
        device: None,
        vk: None,
        offer: None,
        source: None,
        sink: sink.clone(),
        started: Instant::now(),
        pending_paste: false,
        pending_locate: false,
        loop_handle: event_loop.handle(),
        probe_generation: 0,
    };
    // sctk only reports seats announced after startup via `new_seat`.
    if let Some(seat) = state.seats.seats().next() {
        let (conn, qh) = (state.conn.clone(), state.qh.clone());
        state.new_seat(&conn, &qh, seat);
    }

    // Created up front: the compositor registers the device asynchronously
    // and drops key events sent right after the keymap.
    state.ensure_virtual_keyboard();
    let _ = state.conn.flush();

    tracing::info!(
        "clipboard backend ready (virtual keyboard: {}, toplevel info: {})",
        state.vk_manager.is_some(),
        state.toplevels.is_some()
    );
    sink(Event::Ready(CommandSender::new(move |cmd| tx.send(cmd).is_ok())));

    let handle = event_loop.handle();
    loop {
        // Right after startup the pointer and outputs may not be known yet.
        if state.pending_locate && state.can_locate() {
            state.pending_locate = false;
            state.locate_cursor();
        }
        if state.pending_paste {
            state.pending_paste = false;
            // cosmic-comp hands keyboard focus back to the previous window
            // shortly after our overlay is destroyed; keys sent before that
            // would still go to the (gone) overlay.
            let _ = handle.insert_source(
                Timer::from_duration(Duration::from_millis(
                    std::env::var("COSMIC_CLIPBOARD_PASTE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(160),
                )),
                |_, _, state: &mut State| {
                    state.paste();
                    TimeoutAction::Drop
                },
            );
        }
        event_loop.dispatch(None, &mut state).map_err(|e| format!("dispatch: {e}"))?;
    }
}

impl State {
    fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Set { formats } => self.set_selection(formats),
            Command::Paste => self.pending_paste = true,
            Command::LocateCursor => self.pending_locate = true,
        }
    }

    fn can_locate(&self) -> bool {
        self.capture.is_none()
            || self.capture_sources.is_none()
            || (self.pointer.is_some() && self.outputs.outputs().next().is_some())
    }

    fn locate_cursor(&mut self) {
        let (Some(sources), Some(capture), Some(pointer)) = (&self.capture_sources, &self.capture, &self.pointer)
        else {
            (self.sink)(Event::Cursor(None));
            return;
        };
        let sessions = self
            .outputs
            .outputs()
            .map(|output| {
                let source = sources.create_source(&output, &self.qh, ());
                let session = capture.create_pointer_cursor_session(&source, pointer, &self.qh, output);
                (session, source)
            })
            .collect();
        // Bound only for the probe, so a real IME (fcitx, ibus) isn't blocked.
        let ime = match (&self.ime_manager, &self.compositor, &self.seat) {
            (Some(manager), Some(compositor), Some(seat)) => {
                let im = manager.get_input_method(seat, &self.qh, ());
                let surface = compositor.create_surface(&self.qh, ());
                let popup = im.get_input_popup_surface(&surface, &self.qh, ());
                surface.commit();
                Some((im, surface, popup))
            }
            _ => None,
        };
        self.probe_generation += 1;
        let generation = self.probe_generation;
        let has_ime = ime.is_some();
        let probe = CursorProbe {
            sessions,
            found: None,
            ime,
            text_active: false,
            caret: None,
            synced: false,
            caret_done: false,
            timed_out: false,
            generation,
        };
        if let Some(old) = self.probe.replace(probe) {
            old.destroy();
        }
        if has_ime {
            let _ = self.loop_handle.insert_source(Timer::from_duration(CARET_WAIT), move |_, _, state: &mut State| {
                if let Some(probe) = state.probe.as_mut().filter(|p| p.generation == generation) {
                    probe.timed_out = true;
                    state.finish_probe();
                }
                TimeoutAction::Drop
            });
        }
        // Two roundtrips: the input method's state arrives after it is bound.
        self.conn.display().sync(&self.qh, false);
        let _ = self.conn.flush();
    }

    /// Answers the probe once the pointer is known and the input method had
    /// its chance to report a caret (`timeout` = the wait is over).
    fn finish_probe(&mut self) {
        let Some(probe) = &self.probe else { return };
        if !probe.synced || !(probe.ime.is_none() || probe.caret_done || probe.timed_out) {
            return;
        }
        let mut probe = self.probe.take().unwrap();
        let caret = probe.caret.filter(|_| probe.text_active).and_then(|c| self.caret_position(c));
        let found = caret.or_else(|| probe.found.take());
        tracing::debug!("open position {found:?} (text caret: {})", probe.text_active);
        probe.destroy();
        (self.sink)(Event::Cursor(found));
    }

    /// Screen position below the text caret of the focused window, if a
    /// text field is active and the window position is known.
    fn caret_position(&self, caret: (i32, i32, i32, i32)) -> Option<CursorPos> {
        let toplevel = self
            .toplevels
            .as_ref()?
            .toplevels()
            .find(|t| t.state.contains(&zcosmic_toplevel_handle_v1::State::Activated))?;
        let (output, geo) = toplevel.geometry.iter().next()?;
        let name = self.outputs.info(output).and_then(|i| i.name);
        Some(CursorPos {
            output: name,
            x: (geo.x + caret.0) as f32,
            y: (geo.y + caret.1 + caret.3.max(16)) as f32 + 4.0,
            above: Some((geo.y + caret.1) as f32 - 16.0),
            screen: None,
        })
    }

    fn set_selection(&mut self, formats: Formats) {
        let Some(device) = &self.device else { return };
        let source = self.manager.create_data_source(&self.qh, SourceData(formats.clone()));
        for (mime, _) in formats.iter() {
            source.offer(mime.clone());
        }
        source.offer(MARKER_MIME.to_string());
        device.set_selection(&source);
        // The previous source receives `cancelled` and is destroyed there.
        self.source = Some(source);
        let _ = self.conn.flush();
    }

    fn ensure_virtual_keyboard(&mut self) -> Option<ZwpVirtualKeyboardV1> {
        if let Some(vk) = &self.vk {
            return Some(vk.clone());
        }
        let (manager, seat) = (self.vk_manager.as_ref()?, self.seat.as_ref()?);
        let (fd, size) = paste_keymap().inspect_err(|e| tracing::warn!("cannot create keymap: {e}")).ok()?;
        let vk = manager.create_virtual_keyboard(seat, &self.qh, ());
        vk.keymap(1, fd.as_fd(), size);
        self.vk = Some(vk.clone());
        Some(vk)
    }

    fn focused_app_is_terminal(&self) -> bool {
        let Some(toplevels) = &self.toplevels else {
            return false;
        };
        toplevels.toplevels().find(|t| t.state.contains(&zcosmic_toplevel_handle_v1::State::Activated)).is_some_and(
            |t| {
                tracing::debug!("focused app: {}", t.app_id);
                is_terminal(&t.app_id)
            },
        )
    }

    fn paste(&mut self) {
        let shift = self.focused_app_is_terminal();
        let Some(vk) = self.ensure_virtual_keyboard() else {
            super::portal::paste(shift);
            return;
        };
        let mut t = self.started.elapsed().as_millis() as u32;
        let mut key = |code: u32, pressed: bool| {
            vk.key(t, code, pressed as u32);
            t += 1;
        };
        key(KEY_LEFTCTRL, true);
        if shift {
            key(KEY_LEFTSHIFT, true);
        }
        key(KEY_V, true);
        key(KEY_V, false);
        if shift {
            key(KEY_LEFTSHIFT, false);
        }
        key(KEY_LEFTCTRL, false);
        let _ = self.conn.flush();
    }

    fn on_selection(&mut self, offer: Option<Offer>) {
        if let Some(old) = self.offer.take() {
            old.destroy();
        }
        let Some(offer) = offer else {
            if self.source.is_none() {
                (self.sink)(Event::Cleared);
            }
            return;
        };
        let mimes = offer.mimes();
        tracing::debug!("new selection: {mimes:?}");
        self.offer = Some(offer.clone());

        if mimes.iter().any(|m| m == MARKER_MIME) {
            return;
        }
        let wanted = wanted_mimes(&mimes);
        if wanted.is_empty() {
            tracing::debug!("ignoring selection with mimes {mimes:?}");
            return;
        }

        let mut pipes = Vec::new();
        for mime in wanted {
            let Ok((reader, writer)) = std::io::pipe() else {
                continue;
            };
            offer.receive(mime.clone(), writer.as_fd());
            drop(writer);
            pipes.push((mime, reader));
        }
        let _ = self.conn.flush();

        let sink = self.sink.clone();
        std::thread::spawn(move || {
            let formats = read_all(pipes);
            if formats.is_empty() {
                return;
            }
            if let Some(ingested) = history::ingest(formats) {
                sink(Event::Captured(Arc::new(ingested)));
            }
        });
    }
}

impl CursorProbe {
    fn destroy(self) {
        for (session, source) in self.sessions {
            session.destroy();
            source.destroy();
        }
        if let Some((im, surface, popup)) = self.ime {
            popup.destroy();
            surface.destroy();
            im.destroy();
        }
    }
}

fn to_logical(info: Option<OutputInfo>, x: i32, y: i32) -> CursorPos {
    let (mut fx, mut fy) = (x as f32, y as f32);
    if let Some(info) = &info {
        let mode = info.modes.iter().find(|m| m.current).map(|m| m.dimensions);
        let rotated = matches!(
            info.transform,
            wl_output::Transform::_90
                | wl_output::Transform::_270
                | wl_output::Transform::Flipped90
                | wl_output::Transform::Flipped270
        );
        match (mode, info.logical_size) {
            (Some((pw, ph)), Some((lw, lh))) if pw > 0 && ph > 0 => {
                let (pw, ph) = if rotated { (ph, pw) } else { (pw, ph) };
                fx *= lw as f32 / pw as f32;
                fy *= lh as f32 / ph as f32;
            }
            _ => {
                let scale = info.scale_factor.max(1) as f32;
                fx /= scale;
                fy /= scale;
            }
        }
    }
    CursorPos { output: info.and_then(|i| i.name), x: fx, y: fy, above: None, screen: None }
}

/// Reads every pipe concurrently (sources may write them in any order) with a
/// global timeout so a misbehaving client can't stall us.
fn read_all(pipes: Vec<(String, std::io::PipeReader)>) -> Vec<(String, Vec<u8>)> {
    let (tx, rx) = mpsc::channel();
    let count = pipes.len();
    for (index, (mime, mut reader)) in pipes.into_iter().enumerate() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let ok = (&mut reader).take(MAX_FORMAT_BYTES as u64 + 1).read_to_end(&mut buf).is_ok()
                && buf.len() <= MAX_FORMAT_BYTES;
            let _ = tx.send((index, mime, ok.then_some(buf)));
        });
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut results = Vec::new();
    while results.len() < count {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((index, mime, Some(data))) if !data.is_empty() => results.push((index, mime, data)),
            Ok(_) => results.push((usize::MAX, String::new(), Vec::new())),
            Err(_) => break,
        }
    }
    results.retain(|(i, _, _)| *i != usize::MAX);
    results.sort_by_key(|(i, _, _)| *i);
    results.into_iter().map(|(_, m, d)| (m, d)).collect()
}

/// Dispatch for one data-control protocol version (`ext` or `wlr`); both
/// have the same requests and events.
macro_rules! data_control {
    ($variant:ident, $manager:ty, $device:ty, $device_mod:ident, $offer:ty, $offer_mod:ident, $source:ty, $source_mod:ident) => {
        impl Dispatch<$manager, ()> for State {
            fn event(_: &mut Self, _: &$manager, _: <$manager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }

        impl Dispatch<$device, ()> for State {
            fn event(
                state: &mut Self,
                device: &$device,
                event: $device_mod::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $device_mod::Event::Selection { id } => state.on_selection(id.map(Offer::$variant)),
                    $device_mod::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
                    $device_mod::Event::Finished => {
                        device.destroy();
                        if state.device == Some(Device::$variant(device.clone())) {
                            state.device = None;
                        }
                    }
                    _ => {}
                }
            }

            event_created_child!(State, $device, [
                $device_mod::EVT_DATA_OFFER_OPCODE => ($offer, OfferData::default())
            ]);
        }

        impl Dispatch<$offer, OfferData> for State {
            fn event(_: &mut Self, _: &$offer, event: $offer_mod::Event, data: &OfferData, _: &Connection, _: &QueueHandle<Self>) {
                if let $offer_mod::Event::Offer { mime_type } = event {
                    data.0.lock().unwrap().push(mime_type);
                }
            }
        }

        impl Dispatch<$source, SourceData> for State {
            fn event(
                state: &mut Self,
                source: &$source,
                event: $source_mod::Event,
                data: &SourceData,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $source_mod::Event::Send { mime_type, fd } => {
                        let payload =
                            data.0.iter().find(|(m, _)| *m == mime_type).map(|(_, d)| d.clone()).unwrap_or_default();
                        std::thread::spawn(move || {
                            let mut file = File::from(fd);
                            let _ = file.write_all(&payload);
                        });
                    }
                    $source_mod::Event::Cancelled => {
                        source.destroy();
                        if state.source == Some(Source::$variant(source.clone())) {
                            state.source = None;
                        }
                    }
                    _ => {}
                }
            }
        }
    };
}

data_control!(
    Ext,
    ExtDataControlManagerV1,
    ExtDataControlDeviceV1,
    ext_data_control_device_v1,
    ExtDataControlOfferV1,
    ext_data_control_offer_v1,
    ExtDataControlSourceV1,
    ext_data_control_source_v1
);
data_control!(
    Wlr,
    ZwlrDataControlManagerV1,
    ZwlrDataControlDeviceV1,
    zwlr_data_control_device_v1,
    ZwlrDataControlOfferV1,
    zwlr_data_control_offer_v1,
    ZwlrDataControlSourceV1,
    zwlr_data_control_source_v1
);

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, WlOutput> for State {
    fn event(
        state: &mut Self,
        session: &ExtImageCopyCaptureCursorSessionV1,
        event: ext_image_copy_capture_cursor_session_v1::Event,
        output: &WlOutput,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_image_copy_capture_cursor_session_v1::Event::Position { x, y } = event {
            let info = state.outputs.info(output);
            if let Some(probe) = &mut state.probe
                && probe.sessions.iter().any(|(s, _)| s == session)
            {
                probe.found = Some(to_logical(info, x, y));
            }
        }
    }
}

impl Dispatch<WlCallback, bool> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        second: &bool,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            if !*second {
                conn.display().sync(qh, true);
                let _ = conn.flush();
                return;
            }
            if let Some(probe) = &mut state.probe {
                probe.synced = true;
                state.finish_probe();
            }
        }
    }
}

impl Dispatch<ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(probe) = &mut state.probe else { return };
        match event {
            zwp_input_method_v2::Event::Activate => probe.text_active = true,
            zwp_input_method_v2::Event::Deactivate => probe.text_active = false,
            zwp_input_method_v2::Event::Done if probe.text_active => {
                probe.caret_done = true;
                state.finish_probe();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpInputPopupSurfaceV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputPopupSurfaceV2,
        event: zwp_input_popup_surface_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let (Some(probe), zwp_input_popup_surface_v2::Event::TextInputRectangle { x, y, width, height }) =
            (&mut state.probe, event)
        {
            probe.caret = Some((x, y, width, height));
        }
    }
}

macro_rules! ignore_events {
    ($($ty:ty),*) => {$(
        impl Dispatch<$ty, ()> for State {
            fn event(_: &mut Self, _: &$ty, _: <$ty as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    WlPointer,
    WlCompositor,
    WlSurface,
    ZwpInputMethodManagerV2,
    ExtOutputImageCaptureSourceManagerV1,
    ExtImageCopyCaptureManagerV1,
    ExtImageCaptureSourceV1
);

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat) {
        if self.seat.is_none() {
            self.device = Some(self.manager.get_data_device(&seat, qh));
            self.seat = Some(seat);
        }
    }

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, capability: Capability) {
        if self.seat.as_ref() != Some(&seat) {
            return;
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = Some(seat.get_pointer(qh, ()));
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat, capability: Capability) {
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        if self.seat.as_ref() == Some(&seat) {
            self.seat = None;
            if let Some(device) = self.device.take() {
                device.destroy();
            }
        }
    }
}

impl ToplevelInfoHandler for State {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        self.toplevels.as_mut().expect("toplevel info state")
    }

    fn new_toplevel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ExtForeignToplevelHandleV1) {}

    fn update_toplevel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ExtForeignToplevelHandleV1) {}

    fn toplevel_closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &ExtForeignToplevelHandleV1) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![SeatState, OutputState];
}

delegate_registry!(State);
delegate_seat!(State);
delegate_output!(State);
cctk::delegate_toplevel_info!(State);
