//! Clipboard backends. Each one watches the system clipboard, owns it when an
//! entry is restored, simulates the paste shortcut and reports where the
//! pointer (or text caret) is:
//!
//! - `wayland`: compositors with layer-shell and data-control (COSMIC, KDE
//!   Plasma, wlroots based ones); the history opens as an overlay.
//! - `x11`: X11 sessions and Wayland compositors without those protocols
//!   (GNOME), through XWayland; the history opens as a popup window.
//! - `windows` and `macos`: the native clipboards; popup window as well.

use std::sync::{Arc, OnceLock};

use crate::history::Ingested;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
pub mod portal;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::notify;
#[cfg(target_os = "linux")]
mod x11;

/// Mime type offered by our own sources so we can recognise (and ignore) them.
pub const MARKER_MIME: &str = "application/x-cosmic-clipboard-history";
const PASSWORD_HINT_MIME: &str = "x-kde-passwordManagerHint";
const MAX_FORMAT_BYTES: usize = 64 * 1024 * 1024;

const TERMINALS: &[&str] = &[
    "com.system76.cosmicterm",
    "alacritty",
    "kitty",
    "foot",
    "footclient",
    "org.wezfurlong.wezterm",
    "wezterm",
    "org.gnome.terminal",
    "org.gnome.console",
    "org.gnome.ptyxis",
    "org.kde.konsole",
    "konsole",
    "com.mitchellh.ghostty",
    "ghostty",
    "xterm",
    "terminator",
    "tilix",
    "com.gexperts.tilix",
    "xfce4-terminal",
    "dev.warp.warp",
    "io.elementary.terminal",
    "blackbox",
    "com.raggesilver.blackbox",
    "rio",
];

/// Terminals paste with Ctrl+Shift+V instead of Ctrl+V.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_terminal(app_id: &str) -> bool {
    let app_id = app_id.to_lowercase();
    TERMINALS.iter().any(|term| app_id == *term) || app_id.contains("terminal") || app_id.ends_with(".term")
}

pub type Formats = Arc<Vec<(String, Arc<Vec<u8>>)>>;

pub enum Command {
    /// Take ownership of the clipboard with the given formats.
    Set { formats: Formats },
    /// Simulate the paste shortcut in the focused window.
    Paste,
    /// Report the current pointer position (answered with `Event::Cursor`).
    LocateCursor,
}

/// Work area of the monitor the pointer is on, in the backend's screen
/// coordinates (physical pixels on Windows and X11, points on macOS).
#[derive(Debug, Clone, Copy)]
pub struct Screen {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Logical pixels per screen coordinate unit.
    pub scale: f32,
}

/// Pointer position in logical coordinates of the output it is on, or (with
/// `screen`) in screen coordinates for platforms that open a popup window.
#[derive(Debug, Clone)]
pub struct CursorPos {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub output: Option<String>,
    pub x: f32,
    pub y: f32,
    /// Top of the text caret: if the card doesn't fit below, it goes above
    /// this line instead of covering the text being typed.
    pub above: Option<f32>,
    pub screen: Option<Screen>,
}

#[derive(Clone)]
pub struct CommandSender(Arc<dyn Fn(Command) -> bool + Send + Sync>);

impl CommandSender {
    pub fn new(send: impl Fn(Command) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(send))
    }

    pub fn send(&self, cmd: Command) -> bool {
        (self.0)(cmd)
    }
}

impl std::fmt::Debug for CommandSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CommandSender")
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Ready(CommandSender),
    Captured(Arc<Ingested>),
    /// The clipboard became empty (e.g. the owning application quit).
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Cleared,
    Cursor(Option<CursorPos>),
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Failed(String),
    /// The global shortcut was pressed.
    Toggle,
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    #[cfg(target_os = "linux")]
    Wayland,
    #[cfg(target_os = "linux")]
    X11,
    #[cfg(target_os = "windows")]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

/// Picks the backend. Must run before any window is created: on Wayland
/// compositors without layer-shell the app switches to XWayland.
pub fn init() -> Backend {
    *BACKEND.get_or_init(detect)
}

pub fn backend() -> Backend {
    init()
}

/// The history opens as a popup window instead of a layer-shell overlay.
pub fn popup() -> bool {
    #[cfg(target_os = "linux")]
    return backend() != Backend::Wayland;
    #[cfg(not(target_os = "linux"))]
    true
}

#[cfg(target_os = "linux")]
fn detect() -> Backend {
    let has = |var: &str| std::env::var_os(var).is_some_and(|v| !v.is_empty());
    match std::env::var("CLIPSTACK_BACKEND").as_deref() {
        Ok("x11") => {
            // SAFETY: called from `main` before any other thread exists.
            unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
            return Backend::X11;
        }
        Ok("wayland") => return Backend::Wayland,
        _ => {}
    }
    if has("WAYLAND_DISPLAY") || has("WAYLAND_SOCKET") {
        match wayland::probe() {
            Some(true) => return Backend::Wayland,
            Some(false) if has("DISPLAY") => {
                tracing::info!("compositor lacks layer-shell or data-control; using XWayland");
                // SAFETY: called from `main` before any other thread exists.
                unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
                return Backend::X11;
            }
            _ => return Backend::Wayland,
        }
    }
    if has("DISPLAY") { Backend::X11 } else { Backend::Wayland }
}

#[cfg(target_os = "windows")]
fn detect() -> Backend {
    Backend::Windows
}

#[cfg(target_os = "macos")]
fn detect() -> Backend {
    Backend::MacOs
}

pub fn spawn(sink: EventSink) {
    match backend() {
        #[cfg(target_os = "linux")]
        Backend::Wayland => {
            wayland::spawn(sink.clone());
            if crate::setup::runtime_shortcut() {
                portal::spawn_shortcut(sink);
            }
        }
        #[cfg(target_os = "linux")]
        Backend::X11 => x11::spawn(sink),
        #[cfg(target_os = "windows")]
        Backend::Windows => windows::spawn(sink),
        #[cfg(target_os = "macos")]
        Backend::MacOs => macos::spawn(sink),
    }
}

/// Formats worth storing, in order of preference; nothing for passwords.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn wanted_mimes(mimes: &[String]) -> Vec<String> {
    let has = |m: &str| mimes.iter().any(|x| x == m);
    if has(PASSWORD_HINT_MIME) {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Some(text) =
        ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"].into_iter().find(|m| has(m))
    {
        out.push(text.to_string());
    }
    for extra in ["text/html", "text/uri-list", "x-special/gnome-copied-files"] {
        if has(extra) {
            out.push(extra.to_string());
        }
    }
    let image = ["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp"]
        .into_iter()
        .find(|m| has(m))
        .map(str::to_string)
        .or_else(|| mimes.iter().find(|m| m.starts_with("image/") && !m.contains("svg")).cloned());
    out.extend(image);
    out
}

/// The stored data for `mime`, if the entry has it.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn find<'a>(formats: &'a [(String, Arc<Vec<u8>>)], mime: &str) -> Option<&'a [u8]> {
    formats.iter().find(|(m, _)| m == mime).map(|(_, d)| d.as_slice())
}

/// Paths of a `text/uri-list` (only `file://` URIs).
#[cfg_attr(not(windows), allow(dead_code))]
fn uri_paths(uris: &[u8]) -> Vec<std::path::PathBuf> {
    String::from_utf8_lossy(uris)
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| url::Url::parse(l).ok()?.to_file_path().ok())
        .collect()
}

/// A `text/uri-list` for local paths.
#[cfg_attr(not(windows), allow(dead_code))]
fn uri_list(paths: impl IntoIterator<Item = std::path::PathBuf>) -> Option<Vec<u8>> {
    let uris: Vec<String> =
        paths.into_iter().filter_map(|p| url::Url::from_file_path(p).ok()).map(String::from).collect();
    (!uris.is_empty()).then(|| uris.join("\r\n").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_utf8_text_and_skips_passwords() {
        let mimes: Vec<String> =
            ["TEXT", "text/plain;charset=utf-8", "image/png", "text/html"].into_iter().map(String::from).collect();
        assert_eq!(wanted_mimes(&mimes), ["text/plain;charset=utf-8", "text/html", "image/png"]);
        let secret: Vec<String> = ["text/plain", PASSWORD_HINT_MIME].into_iter().map(String::from).collect();
        assert!(wanted_mimes(&secret).is_empty());
    }

    #[test]
    fn detects_terminals() {
        assert!(is_terminal("Alacritty"));
        assert!(is_terminal("gnome-terminal-server"));
        assert!(!is_terminal("firefox"));
    }

    #[test]
    fn uri_list_round_trip() {
        #[cfg(windows)]
        let path = std::path::PathBuf::from(r"C:\Users\me\My File.txt");
        #[cfg(not(windows))]
        let path = std::path::PathBuf::from("/home/me/My File.txt");
        let list = uri_list([path.clone()]).unwrap();
        assert_eq!(uri_paths(&list), [path]);
    }
}
