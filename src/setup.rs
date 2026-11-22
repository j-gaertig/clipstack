//! Desktop integration performed when the app is started from a package
//! (AppImage, deb/rpm, Windows installer, macOS app): autostart and, on Linux,
//! the app menu entry, icon and the Super+V shortcut (COSMIC, GNOME). Other
//! desktops and Windows/macOS get the shortcut at runtime.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(not(target_os = "linux"))]
pub use other::{install, uninstall};

/// The global shortcut as written in help texts.
#[cfg(target_os = "linux")]
pub const SHORTCUT: &str = "Super+V";
#[cfg(target_os = "windows")]
pub const SHORTCUT: &str = "Win+V";
#[cfg(target_os = "macos")]
pub const SHORTCUT: &str = "Cmd+Shift+V";

/// What `install` sets up, for messages.
#[cfg(target_os = "linux")]
pub const INTEGRATION: &str = "autostart, menu entry, Super+V";
#[cfg(not(target_os = "linux"))]
pub const INTEGRATION: &str = "autostart";

/// The executable that should be launched: the AppImage itself if we run
/// from one, otherwise the current binary.
pub fn executable() -> PathBuf {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("cosmic-clipboard"))
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
fn write_if_changed(path: &Path, content: &[u8]) -> io::Result<bool> {
    if fs::read(path).is_ok_and(|old| old == content) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp-cosmic-clipboard");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(true)
}

/// Installed copies integrate themselves on every start so the paths stay
/// valid even if the file was moved; builds from the source tree only on
/// request (`--install`).
pub fn self_integrating() -> bool {
    if std::env::var_os("APPIMAGE").is_some() {
        return true;
    }
    let exe = executable();
    if cfg!(target_os = "linux") {
        return exe.starts_with("/usr") || exe.starts_with("/opt");
    }
    // `target/{debug,release}` or `target/<triple>/{debug,release}`
    let dev = exe.ancestors().skip(1).take(3).any(|dir| dir.file_name().is_some_and(|n| n == "target"))
        && exe.parent().is_some_and(|p| p.ends_with("debug") || p.ends_with("release"));
    !dev
}

#[cfg(target_os = "linux")]
pub use linux::{install, runtime_shortcut, uninstall};

/// Autostart on Windows (Run key) and macOS (LaunchAgent).
#[cfg(not(target_os = "linux"))]
mod other {
    use std::io;

    use super::executable;

    #[cfg(target_os = "windows")]
    mod imp {
        use std::io;

        use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};

        const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
        const NAME: &str = "Clipstack";

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain([0]).collect()
        }

        fn check(code: u32) -> io::Result<()> {
            if code == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(code as i32)) }
        }

        pub fn install(exe: &std::path::Path) -> io::Result<()> {
            let value = wide(&format!("\"{}\" --background", exe.display()));
            // SAFETY: valid NUL-terminated wide strings and a matching byte length.
            check(unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    wide(RUN).as_ptr(),
                    wide(NAME).as_ptr(),
                    REG_SZ,
                    value.as_ptr().cast(),
                    (value.len() * 2) as u32,
                )
            })
        }

        pub fn uninstall() -> io::Result<()> {
            // SAFETY: valid NUL-terminated wide strings.
            match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(RUN).as_ptr(), wide(NAME).as_ptr()) } {
                2 => Ok(()), // ERROR_FILE_NOT_FOUND
                code => check(code),
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod imp {
        use std::{io, path::PathBuf};

        use crate::app::APP_ID;

        fn agent() -> PathBuf {
            crate::history::home().join("Library/LaunchAgents").join(format!("{APP_ID}.plist"))
        }

        fn xml_escape(s: &str) -> String {
            s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        }

        pub fn install(exe: &std::path::Path) -> io::Result<()> {
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{APP_ID}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>--background</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
                xml_escape(&exe.display().to_string())
            );
            super::super::write_if_changed(&agent(), plist.as_bytes()).map(|_| ())
        }

        pub fn uninstall() -> io::Result<()> {
            match std::fs::remove_file(agent()) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }

    pub fn install() -> io::Result<()> {
        imp::install(&executable())
    }

    pub fn uninstall() -> io::Result<()> {
        imp::uninstall()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs, io,
        path::{Path, PathBuf},
    };

    use super::{executable, write_if_changed};
    use crate::{app::APP_ID, history::home};

    const ICON_SVG: &[u8] = include_bytes!("../assets/icon.svg");
    const SHORTCUT_MARKER: &str = "cosmic-clipboard: clipboard history";

    fn is_ours(entry: &str) -> bool {
        entry.contains(SHORTCUT_MARKER)
    }

    fn config_dir() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".config"))
    }

    fn data_home() -> PathBuf {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".local/share"))
    }

    fn autostart_file() -> PathBuf {
        config_dir().join("autostart").join(format!("{APP_ID}.desktop"))
    }

    fn desktop_file() -> PathBuf {
        data_home().join("applications").join(format!("{APP_ID}.desktop"))
    }

    fn icon_file() -> PathBuf {
        data_home().join("icons/hicolor/scalable/apps").join(format!("{APP_ID}.svg"))
    }

    fn shortcuts_file() -> PathBuf {
        config_dir().join("cosmic/com.system76.CosmicSettings.Shortcuts/v1/custom")
    }

    fn sh_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    fn desktop_exec_quote(path: &Path) -> String {
        let s = path.display().to_string();
        let escaped = s.replace('\\', r"\\\\").replace('"', r#"\""#).replace('`', r"\`").replace('$', r"\$");
        format!("\"{escaped}\"")
    }

    pub fn toggle_command(exe: &Path) -> String {
        let path = format!("/{}", APP_ID.replace('.', "/"));
        format!(
            "busctl --user call {APP_ID} {path} org.freedesktop.DbusActivation Activate 'a{{sv}}' 0 || exec {}",
            sh_quote(&exe.display().to_string())
        )
    }

    pub fn install() -> io::Result<()> {
        let exe = executable();
        let exec = desktop_exec_quote(&exe);

        write_if_changed(&icon_file(), ICON_SVG)?;

        let desktop = format!(
            "[Desktop Entry]\n\
         Type=Application\n\
         Name=Clipstack\n\
         GenericName=Clipboard History\n\
         Comment=Open the clipboard history with Super+V\n\
         Exec={exec}\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         Categories=Utility;\n\
         Keywords=clipboard;history;paste;\n\
         StartupNotify=false\n\
         X-AppImage-Integrate=false\n\
         Actions=Uninstall;\n\
         \n\
         [Desktop Action Uninstall]\n\
         Name=Remove integration\n\
         Exec={exec} --uninstall\n"
        );
        // AppImage managers (Gear Lever, AppImageLauncher) create their own menu
        // entry; avoid showing the app twice.
        if menu_entry_exists_elsewhere(&exe) {
            let _ = fs::remove_file(desktop_file());
        } else {
            write_if_changed(&desktop_file(), desktop.as_bytes())?;
        }

        let autostart = format!(
            "[Desktop Entry]\n\
         Type=Application\n\
         Name=Clipstack\n\
         Comment=Clipboard history in the background\n\
         Exec={exec} --background\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n"
        );
        write_if_changed(&autostart_file(), autostart.as_bytes())?;

        if is_cosmic() || shortcuts_file().parent().is_some_and(Path::exists) {
            install_shortcut(&toggle_command(&exe))?;
        }
        if is_gnome() {
            gnome::install(&exe);
        }
        Ok(())
    }

    fn menu_entry_exists_elsewhere(exe: &Path) -> bool {
        let ours = desktop_file();
        let needle = exe.display().to_string();
        let Ok(dir) = fs::read_dir(data_home().join("applications")) else {
            return false;
        };
        dir.flatten().map(|e| e.path()).filter(|p| *p != ours && p.extension().is_some_and(|e| e == "desktop")).any(
            |p| {
                fs::read_to_string(&p).is_ok_and(|c| {
                    c.lines().any(|l| l.starts_with("Exec=") && l.contains(&needle) && !l.contains("--uninstall"))
                })
            },
        )
    }

    pub fn uninstall() -> io::Result<()> {
        for file in [autostart_file(), desktop_file(), icon_file()] {
            match fs::remove_file(&file) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        let path = shortcuts_file();
        if let Ok(content) = fs::read_to_string(&path) {
            let entries: Vec<String> = split_entries(&content).into_iter().filter(|e| !is_ours(e)).collect();
            write_if_changed(&path, render(&entries).as_bytes())?;
        }
        gnome::uninstall();
        Ok(())
    }

    fn desktops() -> Vec<String> {
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').map(str::to_lowercase).collect()
    }

    fn is_cosmic() -> bool {
        desktops().iter().any(|d| d == "cosmic")
    }

    /// Desktops whose custom shortcuts live in gnome-settings-daemon.
    fn is_gnome() -> bool {
        desktops().iter().any(|d| matches!(d.as_str(), "gnome" | "unity" | "budgie" | "pantheon"))
    }

    /// Whether the app has to provide Super+V itself while running (key grab on
    /// X11, GlobalShortcuts portal on Wayland): everywhere but COSMIC and GNOME,
    /// whose own settings carry the shortcut.
    pub fn runtime_shortcut() -> bool {
        !is_cosmic() && !(is_gnome() && gnome::available())
    }

    /// Custom shortcut in gnome-settings-daemon. GNOME binds Super+V to the
    /// notification list by default; that binding is moved out of the way (and
    /// restored on uninstall).
    mod gnome {
        use std::{path::Path, process::Command};

        const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";
        const LIST: &str = "custom-keybindings";
        const OURS: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/clipstack/";
        const SHELL: &str = "org.gnome.shell.keybindings";
        const TRAY: &str = "toggle-message-tray";
        const BINDING: &str = "<Super>v";

        fn get(schema: &str, key: &str) -> Option<String> {
            let out = Command::new("gsettings").args(["get", schema, key]).output().ok()?;
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        }

        fn set(schema: &str, key: &str, value: &str) -> bool {
            Command::new("gsettings").args(["set", schema, key, value]).status().is_ok_and(|s| s.success())
        }

        fn custom(path: &str) -> String {
            format!("{MEDIA_KEYS}.custom-keybinding:{path}")
        }

        pub fn available() -> bool {
            get(MEDIA_KEYS, LIST).is_some()
        }

        /// Single-quoted strings of a GVariant string array (`['a', 'b']`).
        pub(super) fn parse_list(value: &str) -> Vec<String> {
            let mut out = Vec::new();
            let mut chars = value.chars();
            while let Some(c) = chars.next() {
                if c != '\'' {
                    continue;
                }
                let mut item = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => item.extend(chars.next()),
                        '\'' => break,
                        c => item.push(c),
                    }
                }
                out.push(item);
            }
            out
        }

        pub(super) fn quote(s: &str) -> String {
            format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
        }

        fn render_list(items: &[String]) -> String {
            format!("[{}]", items.iter().map(|i| quote(i)).collect::<Vec<_>>().join(", "))
        }

        /// Quoting for the command line, which GNOME splits like a shell.
        fn shell_quote(path: &Path) -> String {
            let s = path.display().to_string();
            format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace('`', "\\`"))
        }

        fn marker() -> std::path::PathBuf {
            crate::history::data_dir().join("gnome-message-tray-moved")
        }

        pub fn install(exe: &Path) {
            let Some(list) = get(MEDIA_KEYS, LIST) else { return };
            let mut paths = parse_list(&list);
            let taken = paths.iter().filter(|p| *p != OURS).any(|p| {
                get(&custom(p), "binding")
                    .is_some_and(|b| parse_list(&b).first().is_some_and(|b| b.eq_ignore_ascii_case(BINDING)))
            });
            if taken {
                tracing::warn!("Super+V is already bound to something else; not overriding it");
                return;
            }
            let schema = custom(OURS);
            set(&schema, "name", &quote("Clipstack"));
            set(&schema, "command", &quote(&shell_quote(exe)));
            set(&schema, "binding", &quote(BINDING));
            if !paths.iter().any(|p| p == OURS) {
                paths.push(OURS.to_string());
                set(MEDIA_KEYS, LIST, &render_list(&paths));
            }
            if let Some(tray) = get(SHELL, TRAY) {
                let tray = parse_list(&tray);
                if tray.iter().any(|b| b.eq_ignore_ascii_case(BINDING)) {
                    let rest: Vec<String> = tray.into_iter().filter(|b| !b.eq_ignore_ascii_case(BINDING)).collect();
                    if set(SHELL, TRAY, &render_list(&rest)) {
                        let _ = std::fs::create_dir_all(crate::history::data_dir());
                        let _ = std::fs::write(marker(), b"");
                    }
                }
            }
        }

        pub fn uninstall() {
            let Some(list) = get(MEDIA_KEYS, LIST) else { return };
            let paths = parse_list(&list);
            if paths.iter().any(|p| p == OURS) {
                let rest: Vec<String> = paths.into_iter().filter(|p| p != OURS).collect();
                set(MEDIA_KEYS, LIST, &render_list(&rest));
                let _ = Command::new("gsettings").args(["reset-recursively", &custom(OURS)]).status();
            }
            if marker().exists() {
                if let Some(tray) = get(SHELL, TRAY) {
                    let mut tray = parse_list(&tray);
                    if !tray.iter().any(|b| b.eq_ignore_ascii_case(BINDING)) {
                        tray.insert(0, BINDING.to_string());
                        set(SHELL, TRAY, &render_list(&tray));
                    }
                }
                let _ = std::fs::remove_file(marker());
            }
        }
    }

    fn install_shortcut(command: &str) -> io::Result<()> {
        let path = shortcuts_file();
        let content = fs::read_to_string(&path).unwrap_or_default();
        let mut entries: Vec<String> = split_entries(&content).into_iter().filter(|e| !is_ours(e)).collect();

        let user_binding = entries.iter().any(|e| {
            let compact: String = e.chars().filter(|c| !c.is_whitespace()).collect();
            compact.contains("modifiers:[Super]") && compact.contains("key:\"v\"")
        });
        if user_binding {
            tracing::warn!("Super+V is already bound to something else; not overriding it");
            return Ok(());
        }

        entries.push(format!(
            "(modifiers: [Super], key: \"v\", description: Some({})): Spawn({})",
            ron_string(SHORTCUT_MARKER),
            ron_string(command)
        ));
        write_if_changed(&path, render(&entries).as_bytes())?;
        Ok(())
    }

    fn ron_string(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    fn render(entries: &[String]) -> String {
        let mut out = String::from("{\n");
        for e in entries {
            out.push_str("    ");
            out.push_str(e);
            out.push_str(",\n");
        }
        out.push_str("}\n");
        out
    }

    /// Splits the top-level entries of a RON map (`{ k: v, k: v }`) without
    /// interpreting them, so unknown actions are preserved verbatim.
    fn split_entries(content: &str) -> Vec<String> {
        let Some(start) = content.find('{') else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        let mut current = String::new();
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for c in content[start + 1..].chars() {
            if in_string {
                current.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            match c {
                '"' => {
                    in_string = true;
                    current.push(c);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    current.push(c);
                }
                ')' | ']' => {
                    depth -= 1;
                    current.push(c);
                }
                '}' if depth == 0 => break,
                '}' => {
                    depth -= 1;
                    current.push(c);
                }
                ',' if depth == 0 => {
                    entries.push(std::mem::take(&mut current));
                }
                c => current.push(c),
            }
        }
        entries.push(current);
        entries.into_iter().map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn splits_and_preserves_entries() {
            let content = r#"{
    (
        modifiers: [
            Super,
        ],
        key: "t",
        description: Some("Term, \"x\" {}"),
    ): Spawn("cosmic-term"),
    (modifiers: [Super], key: "q"): Disable,
}
"#;
            let entries = split_entries(content);
            assert_eq!(entries.len(), 2);
            assert!(entries[0].contains("Spawn(\"cosmic-term\")"));
            assert_eq!(entries[1], "(modifiers: [Super], key: \"q\"): Disable");
        }

        #[test]
        fn recognises_own_shortcut() {
            assert!(is_ours(
                r#"(modifiers: [Super], key: "v", description: Some("cosmic-clipboard: clipboard history")): Spawn("x")"#
            ));
            assert!(!is_ours(r#"(modifiers: [Super], key: "t"): Spawn("cosmic-term")"#));
        }

        #[test]
        fn empty_file() {
            assert!(split_entries("").is_empty());
            assert!(split_entries("{}").is_empty());
            assert_eq!(render(&[]), "{\n}\n");
        }

        #[test]
        fn toggle_command_quotes_path() {
            let cmd = toggle_command(Path::new("/home/u/My Apps/it's.AppImage"));
            assert!(cmd.ends_with(r"exec '/home/u/My Apps/it'\''s.AppImage'"));
            assert!(cmd.contains("'a{sv}' 0"));
        }

        #[test]
        fn gvariant_lists() {
            assert!(gnome::parse_list("@as []").is_empty());
            assert_eq!(gnome::parse_list("['<Super>v', '<Super>m']"), ["<Super>v", "<Super>m"]);
            assert_eq!(gnome::parse_list(&gnome::quote("it's")), ["it's"]);
        }
    }
}
