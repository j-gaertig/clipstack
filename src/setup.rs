//! Desktop integration performed when the AppImage is started: autostart
//! entry, app menu entry, icon and the Super+V shortcut in COSMIC.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

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

/// The executable that should be launched: the AppImage itself if we run
/// from one, otherwise the current binary.
pub fn executable() -> PathBuf {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("cosmic-clipboard"))
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn desktop_exec_quote(path: &Path) -> String {
    let s = path.display().to_string();
    let escaped = s.replace('\\', r"\\\\").replace('"', r#"\""#).replace('`', r"\`").replace('$', r"\$");
    format!("\"{escaped}\"")
}

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

    install_shortcut(&toggle_command(&exe))?;
    Ok(())
}

fn menu_entry_exists_elsewhere(exe: &Path) -> bool {
    let ours = desktop_file();
    let needle = exe.display().to_string();
    let Ok(dir) = fs::read_dir(data_home().join("applications")) else {
        return false;
    };
    dir.flatten().map(|e| e.path()).filter(|p| *p != ours && p.extension().is_some_and(|e| e == "desktop")).any(|p| {
        fs::read_to_string(&p).is_ok_and(|c| {
            c.lines().any(|l| l.starts_with("Exec=") && l.contains(&needle) && !l.contains("--uninstall"))
        })
    })
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
    Ok(())
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
}
