//! Windows side: a message-only window listens for `WM_CLIPBOARDUPDATE`,
//! reads and writes the native clipboard formats (Unicode text, HTML Format,
//! file drops, PNG/DIB images), types Ctrl+V with `SendInput` and provides
//! Win+V (`RegisterHotKey`, or a keyboard hook since Explorer owns Win+V).

use std::{
    cell::RefCell,
    io::Cursor,
    ptr::{null, null_mut},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{GlobalFree, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
    System::{
        DataExchange::{
            AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
            GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
        },
        LibraryLoader::GetModuleHandleW,
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY},
    },
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, MOD_NOREPEAT, MOD_WIN,
            RegisterHotKey, SendInput, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
        },
        Shell::{
            DROPFILES, DragQueryFileW, NIF_ICON, NIF_INFO, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
            Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GUITHREADINFO, GetCursorPos,
            GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetWindowThreadProcessId, HWND_MESSAGE,
            IDI_APPLICATION, KBDLLHOOKSTRUCT, KillTimer, LLKHF_INJECTED, LoadIconW, MSG, PostMessageW, RegisterClassW,
            SetTimer, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL, WM_APP, WM_CLIPBOARDUPDATE, WM_HOTKEY,
            WM_KEYDOWN, WM_SYSKEYDOWN, WM_TIMER, WNDCLASSW,
        },
    },
};

use super::{
    Command, CommandSender, CursorPos, Event, EventSink, Formats, MARKER_MIME, MAX_FORMAT_BYTES, Screen, find,
    uri_list, uri_paths,
};
use crate::history;

const WM_COMMAND_QUEUED: u32 = WM_APP;
const WM_TOGGLE: u32 = WM_APP + 1;
const WM_NOTIFY_USER: u32 = WM_APP + 2;
const TIMER_PASTE: usize = 1;
const TIMER_NOTIFY_DONE: usize = 2;
const TIMER_READ: usize = 3;
/// Clipboard changes are read a moment later: apps that render formats on
/// demand (OLE, .NET) are still busy with the clipboard right after.
const READ_DELAY_MS: u32 = 120;
const VK_V: u16 = b'V' as u16;
/// Unassigned virtual key, typed so a swallowed Win+V doesn't open Start.
const VK_DUMMY: u16 = 0xE8;

static HWND_SELF: AtomicUsize = AtomicUsize::new(0);
static SINK: OnceLock<EventSink> = OnceLock::new();

struct State {
    rx: mpsc::Receiver<Command>,
    sink: EventSink,
    owned: bool,
    html: u32,
    png: u32,
    drop_effect: u32,
    marker: u32,
    /// Formats with which apps (password managers) ask not to be recorded.
    exclude: [u32; 2],
    can_include: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn hwnd() -> HWND {
    HWND_SELF.load(Ordering::Relaxed) as HWND
}

pub fn spawn(sink: EventSink) {
    std::thread::Builder::new()
        .name("windows-clipboard".into())
        .spawn(move || {
            if let Err(err) = run(sink.clone()) {
                tracing::error!("clipboard backend failed: {err}");
                sink(Event::Failed(err));
            }
        })
        .expect("failed to spawn clipboard thread");
}

fn register(name: &str) -> u32 {
    // SAFETY: NUL-terminated wide string.
    unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) }
}

fn run(sink: EventSink) -> Result<(), String> {
    let class = wide("ClipstackClipboard");
    // SAFETY: plain Win32 setup of a message-only window owned by this thread.
    let hwnd = unsafe {
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), instance, null())
    };
    if hwnd.is_null() {
        return Err("cannot create the clipboard window".into());
    }
    HWND_SELF.store(hwnd as usize, Ordering::Relaxed);
    let _ = SINK.set(sink.clone());
    // SAFETY: valid window handle.
    if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
        return Err("cannot listen to clipboard changes".into());
    }
    let (tx, rx) = mpsc::channel();
    STATE.with_borrow_mut(|s| {
        *s = Some(State {
            rx,
            sink: sink.clone(),
            owned: false,
            html: register("HTML Format"),
            png: register("PNG"),
            drop_effect: register("Preferred DropEffect"),
            marker: register(MARKER_MIME),
            exclude: [register("ExcludeClipboardContentFromMonitorProcessing"), register("Clipboard Viewer Ignore")],
            can_include: register("CanIncludeInClipboardHistory"),
        })
    });
    register_hotkey(hwnd);

    let window = hwnd as usize;
    sink(Event::Ready(CommandSender::new(move |cmd| {
        // SAFETY: posting to our own window; it outlives the process.
        tx.send(cmd).is_ok() && unsafe { PostMessageW(window as HWND, WM_COMMAND_QUEUED, 0, 0) } != 0
    })));
    tracing::info!("clipboard backend ready (Windows)");
    with_state(State::on_update);

    let mut msg = MSG::default();
    // SAFETY: standard message loop.
    unsafe {
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

fn with_state(f: impl FnOnce(&mut State)) {
    STATE.with_borrow_mut(|s| {
        if let Some(s) = s {
            f(s)
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // SAFETY: timer on our own window (re-arming it restarts the delay).
        WM_CLIPBOARDUPDATE => unsafe {
            SetTimer(hwnd, TIMER_READ, READ_DELAY_MS, None);
        },
        WM_TIMER if wparam == TIMER_READ => {
            // SAFETY: our timer on our window.
            unsafe { KillTimer(hwnd, TIMER_READ) };
            with_state(State::on_update);
        }
        WM_COMMAND_QUEUED => with_state(|s| {
            while let Ok(cmd) = s.rx.try_recv() {
                s.on_command(cmd);
            }
        }),
        WM_HOTKEY | WM_TOGGLE => {
            if let Some(sink) = SINK.get() {
                sink(Event::Toggle);
            }
        }
        WM_TIMER if wparam == TIMER_PASTE => {
            // SAFETY: our timer on our window.
            unsafe { KillTimer(hwnd, TIMER_PASTE) };
            paste();
        }
        WM_TIMER if wparam == TIMER_NOTIFY_DONE => {
            // SAFETY: our timer and notification icon.
            unsafe {
                KillTimer(hwnd, TIMER_NOTIFY_DONE);
                let data = NOTIFYICONDATAW {
                    cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: 1,
                    ..Default::default()
                };
                Shell_NotifyIconW(NIM_DELETE, &data);
            }
        }
        WM_NOTIFY_USER => {
            // SAFETY: the pointer was leaked by `notify` for this message.
            let (summary, body) = *unsafe { Box::from_raw(lparam as *mut (String, String)) };
            show_notification(hwnd, &summary, &body);
        }
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    0
}

impl State {
    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Set { formats } => {
                if let Err(err) = self.set(&formats) {
                    tracing::warn!("cannot set the clipboard: {err}");
                }
            }
            Command::Paste => {
                let delay =
                    std::env::var("COSMIC_CLIPBOARD_PASTE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(160);
                // SAFETY: timer on our own window.
                unsafe { SetTimer(hwnd(), TIMER_PASTE, delay, None) };
            }
            Command::LocateCursor => (self.sink)(Event::Cursor(locate())),
        }
    }

    fn on_update(&mut self) {
        // SAFETY: clipboard calls between Open/CloseClipboard on this thread.
        unsafe {
            if GetClipboardOwner() == hwnd() {
                return;
            }
            self.owned = false;
            if !open() {
                return;
            }
            let formats = self.read();
            CloseClipboard();
            let Some(formats) = formats else { return };
            if formats.is_empty() {
                return;
            }
            let sink = self.sink.clone();
            std::thread::spawn(move || {
                if let Some(ingested) = history::ingest(formats) {
                    sink(Event::Captured(Arc::new(ingested)));
                }
            });
        }
    }

    /// The interesting formats of the open clipboard; `None` for our own
    /// data and for content apps ask not to record.
    unsafe fn read(&self) -> Option<Vec<(String, Vec<u8>)>> {
        // SAFETY: the clipboard is open (caller).
        unsafe {
            if EnumClipboardFormats(0) == 0 {
                if !self.owned {
                    (self.sink)(Event::Cleared);
                }
                return None;
            }
            let available = |f: u32| IsClipboardFormatAvailable(f) != 0;
            if available(self.marker) || self.exclude.iter().any(|f| available(*f)) {
                return None;
            }
            if available(self.can_include) && get(self.can_include).is_some_and(|d| d.iter().all(|b| *b == 0)) {
                return None;
            }
            let mut out = Vec::new();
            if let Some(text) = get(CF_UNICODETEXT as u32) {
                let units: Vec<u16> = text.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
                let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
                out.push((
                    "text/plain;charset=utf-8".to_string(),
                    String::from_utf16_lossy(&units[..end]).into_bytes(),
                ));
            }
            if let Some(html) = get(self.html).and_then(|h| html_fragment(&h)) {
                out.push(("text/html".to_string(), html));
            }
            if available(CF_HDROP as u32) {
                let drop = GetClipboardData(CF_HDROP as u32);
                let count = DragQueryFileW(drop, u32::MAX, null_mut(), 0);
                let paths = (0..count).map(|i| {
                    let len = DragQueryFileW(drop, i, null_mut(), 0) as usize;
                    let mut buf = vec![0u16; len + 1];
                    DragQueryFileW(drop, i, buf.as_mut_ptr(), buf.len() as u32);
                    std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len]))
                });
                out.extend(uri_list(paths).map(|l| ("text/uri-list".to_string(), l)));
            }
            if let Some(png) = get(self.png) {
                out.push(("image/png".to_string(), png));
            } else if let Some(bmp) = get(CF_DIBV5 as u32).or_else(|| get(CF_DIB as u32)).and_then(|d| dib_to_bmp(&d)) {
                out.push(("image/bmp".to_string(), bmp));
            }
            Some(out)
        }
    }

    fn set(&mut self, formats: &Formats) -> Result<(), String> {
        let text = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"]
            .into_iter()
            .find_map(|m| find(formats, m));
        let mut items: Vec<(u32, Vec<u8>)> = Vec::new();
        if let Some(text) = text {
            let units: Vec<u16> = String::from_utf8_lossy(text).encode_utf16().chain([0]).collect();
            items.push((CF_UNICODETEXT as u32, units.iter().flat_map(|u| u.to_le_bytes()).collect()));
        }
        if let Some(html) = find(formats, "text/html") {
            items.push((self.html, cf_html(&String::from_utf8_lossy(html))));
        }
        if let Some(uris) = find(formats, "text/uri-list") {
            let paths = uri_paths(uris);
            if !paths.is_empty() {
                items.push((CF_HDROP as u32, hdrop(&paths)));
                items.push((self.drop_effect, DROPEFFECT_COPY.to_le_bytes().to_vec()));
            }
        }
        if let Some((_, data)) = formats.iter().find(|(m, _)| m.starts_with("image/"))
            && let Ok(img) = image::load_from_memory(data)
        {
            let mut png = Vec::new();
            if img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).is_ok() {
                items.push((self.png, png));
            }
            items.push((CF_DIB as u32, rgba_to_dib(&img.to_rgba8())));
        }
        items.push((self.marker, vec![1]));
        // SAFETY: clipboard calls between Open/CloseClipboard; ownership of
        // each global moves to the clipboard on success.
        unsafe {
            if !open() {
                return Err("the clipboard is busy".into());
            }
            EmptyClipboard();
            for (format, data) in items {
                let mem = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1));
                if mem.is_null() {
                    continue;
                }
                let ptr = GlobalLock(mem) as *mut u8;
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
                GlobalUnlock(mem);
                if SetClipboardData(format, mem).is_null() {
                    GlobalFree(mem);
                }
            }
            CloseClipboard();
        }
        self.owned = true;
        Ok(())
    }
}

/// Opens the clipboard, retrying while another app holds it.
unsafe fn open() -> bool {
    for _ in 0..20 {
        // SAFETY: our own window.
        if unsafe { OpenClipboard(hwnd()) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Copies the data of a clipboard format (clipboard must be open).
unsafe fn get(format: u32) -> Option<Vec<u8>> {
    // SAFETY: the handle comes from the open clipboard and is only read.
    unsafe {
        let mem = GetClipboardData(format);
        if mem.is_null() {
            return None;
        }
        let size = GlobalSize(mem);
        let ptr = GlobalLock(mem) as *const u8;
        if ptr.is_null() || size > MAX_FORMAT_BYTES {
            GlobalUnlock(mem);
            return None;
        }
        let data = std::slice::from_raw_parts(ptr, size).to_vec();
        GlobalUnlock(mem);
        Some(data)
    }
}

/// The fragment of a CF_HTML ("HTML Format") payload.
fn html_fragment(data: &[u8]) -> Option<Vec<u8>> {
    let header = String::from_utf8_lossy(&data[..data.len().min(512)]);
    let offset = |key: &str| -> Option<usize> {
        let rest = header.split_once(key)?.1;
        rest.trim_start_matches(':').lines().next()?.trim().parse().ok()
    };
    let (start, end) = (offset("StartFragment")?, offset("EndFragment")?);
    let fragment = data.get(start..end.min(data.len()))?;
    (!fragment.is_empty()).then(|| fragment.to_vec())
}

/// Wraps HTML into CF_HTML with its byte offset header.
fn cf_html(html: &str) -> Vec<u8> {
    let (before, fragment, after) = match (html.find("<!--StartFragment-->"), html.find("<!--EndFragment-->")) {
        (Some(s), Some(e)) if s < e => {
            let s = s + "<!--StartFragment-->".len();
            (html[..s].to_string(), &html[s..e], html[e..].to_string())
        }
        _ => ("<html><body><!--StartFragment-->".to_string(), html, "<!--EndFragment--></body></html>".to_string()),
    };
    let header_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
    let start_fragment = header_len + before.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + after.len();
    format!(
        "Version:0.9\r\nStartHTML:{header_len:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n{before}{fragment}{after}\0"
    )
    .into_bytes()
}

/// A CF_HDROP payload: `DROPFILES` and the NUL-separated wide paths.
fn hdrop(paths: &[std::path::PathBuf]) -> Vec<u8> {
    let header = DROPFILES { pFiles: size_of::<DROPFILES>() as u32, fWide: 1, ..Default::default() };
    // SAFETY: DROPFILES is plain old data.
    let mut out =
        unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<DROPFILES>()) }.to_vec();
    for path in paths {
        out.extend(path.display().to_string().encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()));
    }
    out.extend([0, 0]);
    out
}

/// A BMP file for a packed DIB (`BITMAPINFOHEADER` or a later version).
fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    let u32_at = |i: usize| Some(u32::from_le_bytes(dib.get(i..i + 4)?.try_into().ok()?));
    let u16_at = |i: usize| Some(u16::from_le_bytes(dib.get(i..i + 2)?.try_into().ok()?));
    let header = u32_at(0)? as usize;
    let bits = u16_at(14)?;
    let compression = u32_at(16)?;
    let used = u32_at(32)? as usize;
    let masks = if header == 40 && compression == 3 { 12 } else { 0 };
    let colors = if bits <= 8 && used == 0 { 1usize << bits } else { used };
    let palette = colors * 4;
    let offset = 14 + header + masks + palette;
    let mut out = Vec::with_capacity(14 + dib.len());
    out.extend(b"BM");
    out.extend(((14 + dib.len()) as u32).to_le_bytes());
    out.extend([0u8; 4]);
    out.extend((offset as u32).to_le_bytes());
    out.extend(dib);
    Some(out)
}

/// A packed 32-bit bottom-up DIB for apps that don't read PNG.
fn rgba_to_dib(img: &image::RgbaImage) -> Vec<u8> {
    let (w, h) = img.dimensions();
    let mut out = Vec::with_capacity(40 + (w * h * 4) as usize);
    out.extend(40u32.to_le_bytes());
    out.extend((w as i32).to_le_bytes());
    out.extend((h as i32).to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // BI_RGB
    out.extend((w * h * 4).to_le_bytes());
    out.extend([0u8; 16]);
    for row in img.rows().rev() {
        for p in row {
            out.extend([p[2], p[1], p[0], p[3]]);
        }
    }
    out
}

fn key(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(keys: &[INPUT]) {
    // SAFETY: a valid array of keyboard inputs.
    unsafe { SendInput(keys.len() as u32, keys.as_ptr(), size_of::<INPUT>() as i32) };
}

fn paste() {
    send(&[key(VK_CONTROL, false), key(VK_V, false), key(VK_V, true), key(VK_CONTROL, true)]);
}

fn rect(r: &RECT) -> (f32, f32, f32, f32) {
    (r.left as f32, r.top as f32, (r.right - r.left) as f32, (r.bottom - r.top) as f32)
}

/// The text caret of the focused window (if it uses the system caret),
/// else the mouse pointer; with the work area of that monitor.
fn locate() -> Option<CursorPos> {
    // SAFETY: plain queries with properly sized out structs.
    unsafe {
        let mut gui = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        let thread = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
        let caret = (GetGUIThreadInfo(thread, &mut gui) != 0
            && !gui.hwndCaret.is_null()
            && gui.rcCaret.bottom > gui.rcCaret.top)
            .then(|| {
                let mut top = POINT { x: gui.rcCaret.left, y: gui.rcCaret.top };
                let mut bottom = POINT { x: gui.rcCaret.left, y: gui.rcCaret.bottom };
                ClientToScreen(gui.hwndCaret, &mut top);
                ClientToScreen(gui.hwndCaret, &mut bottom);
                (top, bottom)
            });
        let mut pointer = POINT::default();
        GetCursorPos(&mut pointer);
        let anchor = caret.map_or(pointer, |(_, bottom)| bottom);
        let monitor = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        GetMonitorInfoW(monitor, &mut info);
        let (mut dpi_x, mut dpi_y) = (96, 96);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        let scale = dpi_x.max(48) as f32 / 96.0;
        let (x, y, width, height) = rect(&info.rcWork);
        let screen = Some(Screen { x, y, width, height, scale });
        Some(match caret {
            Some((top, bottom)) => CursorPos {
                output: None,
                x: bottom.x as f32,
                y: bottom.y as f32 + 4.0 * scale,
                above: Some(top.y as f32 - 16.0 * scale),
                screen,
            },
            None => CursorPos { output: None, x: pointer.x as f32, y: pointer.y as f32, above: None, screen },
        })
    }
}

static V_SWALLOWED: AtomicBool = AtomicBool::new(false);
static WIN_COMBO: AtomicBool = AtomicBool::new(false);

/// Win+V: a hotkey if Explorer doesn't own it (clipboard history off),
/// else a low-level keyboard hook that takes the keys first.
fn register_hotkey(hwnd: HWND) {
    // SAFETY: our own window; the hook runs on this thread's message loop.
    unsafe {
        if RegisterHotKey(hwnd, 1, MOD_WIN | MOD_NOREPEAT, VK_V as u32) != 0 {
            return;
        }
        if SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), GetModuleHandleW(null()), 0).is_null() {
            tracing::warn!("cannot register Win+V");
        }
    }
}

fn pressed(vk: u16) -> bool {
    // SAFETY: plain query.
    unsafe { GetAsyncKeyState(vk as i32) < 0 }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL, lparam points to a KBDLLHOOKSTRUCT.
        let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let vk = info.vkCode as u16;
        if info.flags & LLKHF_INJECTED == 0 {
            if vk == VK_V {
                let win = pressed(VK_LWIN) || pressed(VK_RWIN);
                if down && win && !pressed(VK_CONTROL) && !pressed(VK_MENU) && !pressed(VK_SHIFT) {
                    if !V_SWALLOWED.swap(true, Ordering::Relaxed) {
                        // SAFETY: posting to our own window.
                        unsafe { PostMessageW(hwnd(), WM_TOGGLE, 0, 0) };
                    }
                    return 1;
                }
                if !down && V_SWALLOWED.swap(false, Ordering::Relaxed) {
                    WIN_COMBO.store(true, Ordering::Relaxed);
                    return 1;
                }
            }
            if (vk == VK_LWIN || vk == VK_RWIN) && !down && WIN_COMBO.swap(false, Ordering::Relaxed) {
                send(&[key(VK_DUMMY, false), key(VK_DUMMY, true)]);
            }
        }
    }
    // SAFETY: passing the event on.
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

/// Shows a notification (a balloon, shown as a toast on Windows 10/11).
pub fn notify(summary: &str, body: &str) {
    let hwnd = hwnd();
    if hwnd.is_null() {
        return;
    }
    let data = Box::into_raw(Box::new((summary.to_string(), body.to_string())));
    // SAFETY: the message handler takes the box back.
    if unsafe { PostMessageW(hwnd, WM_NOTIFY_USER, 0, data as LPARAM) } == 0 {
        // SAFETY: not posted, so still ours.
        drop(unsafe { Box::from_raw(data) });
    }
}

fn copy_wide<const N: usize>(dst: &mut [u16; N], s: &str) {
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(N - 1)) {
        *d = c;
    }
}

fn show_notification(hwnd: HWND, summary: &str, body: &str) {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO | NIF_ICON | NIF_TIP,
        dwInfoFlags: NIIF_INFO,
        ..Default::default()
    };
    // SAFETY: plain Win32 calls with a fully initialised struct.
    unsafe {
        let instance = GetModuleHandleW(null());
        // The exe's icon (resource 1), else the generic application icon.
        data.hIcon = LoadIconW(instance, 1 as _);
        if data.hIcon.is_null() {
            data.hIcon = LoadIconW(null_mut(), IDI_APPLICATION);
        }
        copy_wide(&mut data.szTip, "Clipstack");
        copy_wide(&mut data.szInfoTitle, summary);
        copy_wide(&mut data.szInfo, if body.is_empty() { " " } else { body });
        Shell_NotifyIconW(NIM_DELETE, &data);
        Shell_NotifyIconW(NIM_ADD, &data);
        SetTimer(hwnd, TIMER_NOTIFY_DONE, 15_000, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_html_round_trip() {
        let data = cf_html("<b>Hi</b>");
        assert_eq!(html_fragment(&data).as_deref(), Some(&b"<b>Hi</b>"[..]));
    }
}
