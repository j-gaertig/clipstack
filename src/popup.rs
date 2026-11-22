//! Native touches for the popup window on platforms without layer-shell:
//! position it in physical pixels, bring it to the front with keyboard focus
//! and, on macOS, give it rounded corners and hand focus back when it closes.

use cosmic::iced::window::Window;

/// Called once at startup on the main thread.
pub fn init() {
    #[cfg(target_os = "macos")]
    macos::init();
}

/// Shows the freshly opened popup at `at` (physical screen coordinates).
#[allow(unused_variables)]
pub fn show(window: &dyn Window, at: Option<(i32, i32)>) {
    #[cfg(target_os = "windows")]
    windows::show(window, at);
    #[cfg(target_os = "macos")]
    macos::show(window);
    #[cfg(target_os = "linux")]
    x11::show(window);
}

#[cfg(target_os = "linux")]
mod x11 {
    use std::{sync::OnceLock, time::Duration};

    use cosmic::iced::window::{Window, raw_window_handle::RawWindowHandle};
    use x11rb::{
        CURRENT_TIME,
        connection::Connection,
        protocol::xproto::{ConnectionExt, InputFocus, MapState},
        rust_connection::RustConnection,
    };

    /// Takes the keyboard focus once the window is mapped: window managers
    /// may refuse focus to windows that weren't clicked, and without one
    /// nothing hands it out at all.
    pub fn show(window: &dyn Window) {
        let id = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Xlib(h)) => h.window as u32,
            Ok(RawWindowHandle::Xcb(h)) => h.window.get(),
            _ => return,
        };
        static CONN: OnceLock<Option<RustConnection>> = OnceLock::new();
        let Some(conn) = CONN.get_or_init(|| RustConnection::connect(None).ok().map(|(c, _)| c)).as_ref() else {
            return;
        };
        // Mapping happens asynchronously; wait for it off the UI thread.
        std::thread::spawn(move || {
            for _ in 0..50 {
                let viewable = conn
                    .get_window_attributes(id)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some_and(|a| a.map_state == MapState::VIEWABLE);
                if viewable {
                    let _ = conn.set_input_focus(InputFocus::PARENT, id, CURRENT_TIME);
                    let _ = conn.flush();
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }
}

/// Called after the popup was closed.
pub fn hidden() {
    #[cfg(target_os = "macos")]
    macos::hidden();
}

#[cfg(target_os = "windows")]
mod windows {
    use cosmic::iced::window::{Window, raw_window_handle::RawWindowHandle};
    use windows_sys::Win32::{
        System::Threading::{AttachThreadInput, GetCurrentThreadId},
        UI::{
            Input::KeyboardAndMouse::{
                INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, SetFocus,
            },
            WindowsAndMessaging::{
                BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE,
                SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos,
            },
        },
    };

    pub fn show(window: &dyn Window, at: Option<(i32, i32)>) {
        let Ok(handle) = window.window_handle() else { return };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
        let hwnd = handle.hwnd.get() as _;
        let (x, y) = at.unwrap_or_default();
        let flags = SWP_NOSIZE | SWP_SHOWWINDOW | if at.is_none() { SWP_NOMOVE } else { 0 };
        // SAFETY: plain Win32 calls on a window owned by this thread.
        unsafe {
            SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, flags);
            // Windows lets only the source of the last input take the
            // foreground: type an unassigned key, then also borrow the input
            // state of the foreground thread for a moment.
            let dummy = |up| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: 0xE8,
                        wScan: 0,
                        dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let keys = [dummy(false), dummy(true)];
            SendInput(keys.len() as u32, keys.as_ptr(), size_of::<INPUT>() as i32);
            let me = GetCurrentThreadId();
            let fg = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
            let attached = fg != 0 && fg != me && AttachThreadInput(me, fg, 1) != 0;
            SetForegroundWindow(hwnd);
            BringWindowToTop(hwnd);
            SetFocus(hwnd);
            if attached {
                AttachThreadInput(me, fg, 0);
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use cosmic::iced::window::{Window, raw_window_handle::RawWindowHandle};
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSColor, NSPopUpMenuWindowLevel, NSView};

    /// No Dock icon and no menu bar: Clipstack lives in the background.
    pub fn init() {
        let Some(mtm) = MainThreadMarker::new() else { return };
        NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }

    pub fn show(window: &dyn Window) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let Ok(handle) = window.window_handle() else { return };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return };
        // SAFETY: the handle points to the live NSView of this window.
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        let Some(window) = view.window() else { return };
        // winit switches to a regular app once it has launched.
        init();
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(true);
        view.setWantsLayer(true);
        if let Some(layer) = view.layer() {
            layer.setCornerRadius(12.0);
            layer.setMasksToBounds(true);
        }
        window.setLevel(NSPopUpMenuWindowLevel);
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        window.makeKeyAndOrderFront(None);
        window.invalidateShadow();
    }

    /// Gives focus back to the app that was active before the popup.
    pub fn hidden() {
        let Some(mtm) = MainThreadMarker::new() else { return };
        NSApplication::sharedApplication(mtm).hide(None);
    }
}
