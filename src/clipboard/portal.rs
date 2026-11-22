//! XDG desktop portals for desktops that lack the Wayland protocols: the
//! global shortcut (KDE Plasma, Hyprland, …) and simulated key presses for
//! pasting (KDE Plasma, GNOME). Both ask the user once for permission.

use std::sync::OnceLock;

use ashpd::desktop::{
    PersistMode, Session,
    global_shortcuts::{GlobalShortcuts, NewShortcut},
    remote_desktop::{DeviceType, KeyState, RemoteDesktop},
};
use cosmic::iced::futures::StreamExt;
use tokio::sync::mpsc;

use super::{
    Event, EventSink,
    wayland::{KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_V},
};
use crate::{app::APP_ID, history};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime")
}

/// Binds Super+V through the GlobalShortcuts portal for as long as the app runs.
pub fn spawn_shortcut(sink: EventSink) {
    let _ = std::thread::Builder::new().name("portal-shortcut".into()).spawn(move || {
        if let Err(err) = runtime().block_on(shortcut(sink)) {
            tracing::warn!(
                "global shortcut portal unavailable ({err}); bind `cosmic-clipboard` to Super+V in the system settings"
            );
        }
    });
}

async fn shortcut(sink: EventSink) -> ashpd::Result<()> {
    if let Ok(app_id) = ashpd::AppID::try_from(APP_ID) {
        let _ = ashpd::register_host_app(app_id).await;
    }
    let portal = GlobalShortcuts::new().await?;
    let session = portal.create_session().await?;
    let toggle = NewShortcut::new("toggle", "Show the clipboard history").preferred_trigger("LOGO+v");
    let bound = portal.bind_shortcuts(&session, &[toggle], None).await?.response()?;
    let triggers: Vec<&str> = bound.shortcuts().iter().map(|s| s.trigger_description()).collect();
    tracing::info!("global shortcut bound: {triggers:?}");
    let mut activated = portal.receive_activated().await?;
    while let Some(event) = activated.next().await {
        if event.shortcut_id() == "toggle" {
            sink(Event::Toggle);
        }
    }
    Ok(())
}

static PASTE: OnceLock<mpsc::UnboundedSender<bool>> = OnceLock::new();

/// Presses Ctrl+V (Ctrl+Shift+V with `shift`) through the RemoteDesktop portal.
pub fn paste(shift: bool) {
    let tx = PASTE.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = std::thread::Builder::new().name("portal-paste".into()).spawn(move || runtime().block_on(paster(rx)));
        tx
    });
    let _ = tx.send(shift);
}

type Remote = (RemoteDesktop<'static>, Session<'static, RemoteDesktop<'static>>);

async fn paster(mut rx: mpsc::UnboundedReceiver<bool>) {
    let mut remote: Option<Remote> = None;
    while let Some(shift) = rx.recv().await {
        if remote.is_none() {
            match connect().await {
                Ok(r) => remote = Some(r),
                Err(err) => {
                    tracing::warn!("cannot simulate paste: remote desktop portal: {err}");
                    continue;
                }
            }
        }
        let Some((portal, session)) = &remote else { continue };
        let keys: &[u32] = if shift { &[KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_V] } else { &[KEY_LEFTCTRL, KEY_V] };
        let mut result = Ok(());
        for (key, state) in
            keys.iter().map(|k| (k, KeyState::Pressed)).chain(keys.iter().rev().map(|k| (k, KeyState::Released)))
        {
            result = result.and(portal.notify_keyboard_keycode(session, *key as i32, state).await);
        }
        if let Err(err) = result {
            tracing::warn!("paste through the portal failed: {err}");
            remote = None;
        }
    }
}

fn token_file() -> std::path::PathBuf {
    history::data_dir().join("portal-restore-token")
}

async fn connect() -> ashpd::Result<Remote> {
    let portal = RemoteDesktop::new().await?;
    let session = portal.create_session().await?;
    let token = std::fs::read_to_string(token_file()).ok();
    portal
        .select_devices(&session, DeviceType::Keyboard.into(), token.as_deref(), PersistMode::ExplicitlyRevoked)
        .await?;
    let selected = portal.start(&session, None).await?.response()?;
    if let Some(token) = selected.restore_token() {
        let _ = std::fs::create_dir_all(history::data_dir());
        let _ = std::fs::write(token_file(), token);
    }
    Ok((portal, session))
}
