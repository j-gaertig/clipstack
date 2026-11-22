// No console window for the GUI app on Windows (CLI output attaches to the
// parent console, see `attach_console`).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod clipboard;
mod drawing;
mod history;
#[cfg(not(target_os = "linux"))]
mod instance;
mod popup;
mod setup;

fn help() -> String {
    let shortcut = setup::SHORTCUT;
    format!(
        "Clipstack: clipboard history ({shortcut})

Usage: cosmic-clipboard [OPTION]

  (no option)     Show or hide the history; starts the service if needed
  --background    Start the service in the background (autostart)
  --install       Set up autostart{menu}
  --uninstall     Remove autostart{menu}
  --no-integrate  Don't set up desktop integration on start
  --help          Show this help
",
        menu = if cfg!(target_os = "linux") { ", menu entry and the Super+V shortcut" } else { "" },
    )
}

fn main() -> cosmic::iced::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    // Only the command line options talk to the terminal; the service
    // itself must not write into the console it was started from.
    #[cfg(windows)]
    if ["--help", "-h", "--install", "--uninstall"].iter().any(|f| has(f)) {
        attach_console();
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,cosmic_clipboard=info".into()),
        )
        .init();

    if has("--help") || has("-h") {
        print!("{}", help());
        return Ok(());
    }
    if has("--uninstall") {
        match setup::uninstall() {
            Ok(()) => println!(
                "Integration removed ({}). The history is still in {}.",
                setup::INTEGRATION,
                history::data_dir().display()
            ),
            Err(e) => eprintln!("Failed to remove integration: {e}"),
        }
        return Ok(());
    }

    // Decided before any window exists (may switch to XWayland).
    let backend = clipboard::init();
    tracing::debug!("clipboard backend: {backend:?}");

    let integrate = has("--install") || (setup::self_integrating() && !has("--no-integrate"));
    if integrate {
        if let Err(e) = setup::install() {
            tracing::error!("desktop integration failed: {e}");
        }
        if has("--install") {
            println!("Done: {} now opens the clipboard history.", setup::SHORTCUT);
        }
    }

    let flags = app::Flags { background: has("--background") || has("--install") };
    #[cfg(target_os = "linux")]
    return cosmic::app::run_single_instance::<app::App>(app::settings(), flags);
    #[cfg(not(target_os = "linux"))]
    {
        let action = if flags.background { app::BACKGROUND_ACTION } else { "toggle" };
        if instance::forward(action) {
            return Ok(());
        }
        cosmic::app::run::<app::App>(app::settings(), flags)
    }
}

/// Lets `--help` and friends print to the terminal they were started from.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: plain Win32 call; fails harmlessly without a parent console.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}
