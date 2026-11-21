mod app;
mod clipboard;
mod drawing;
mod history;
mod setup;

const HELP: &str = "Clipstack: clipboard history for COSMIC (Super+V)

Usage: cosmic-clipboard [OPTION]

  (no option)     Show or hide the history; starts the service if needed
  --background    Start the service in the background (autostart)
  --install       Set up autostart, menu entry and the Super+V shortcut
  --uninstall     Remove autostart, menu entry and the Super+V shortcut
  --no-integrate  Don't set up desktop integration on start
  --help          Show this help
";

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,cosmic_clipboard=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--help") || has("-h") {
        print!("{HELP}");
        return Ok(());
    }
    if has("--uninstall") {
        match setup::uninstall() {
            Ok(()) => println!(
                "Integration removed (autostart, menu entry, Super+V). The history is still in {}.",
                history::data_dir().display()
            ),
            Err(e) => eprintln!("Failed to remove integration: {e}"),
        }
        return Ok(());
    }

    // The AppImage integrates itself on every start so the paths stay valid
    // even if the file was moved. Plain binaries only do so on request.
    let integrate = has("--install") || (std::env::var_os("APPIMAGE").is_some() && !has("--no-integrate"));
    if integrate {
        if let Err(e) = setup::install() {
            tracing::error!("desktop integration failed: {e}");
        }
        if has("--install") {
            println!("Done: Super+V now opens the clipboard history.");
        }
    }

    let flags = app::Flags { background: has("--background") || has("--install") };
    cosmic::app::run_single_instance::<app::App>(app::settings(), flags)
}
