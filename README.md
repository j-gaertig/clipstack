<p align="center">
  <img src="./assets/icon.svg" width="88" height="88" alt="Clipstack logo" />
</p>

<h1 align="center">Clipstack</h1>

<p align="center">
  <strong>Super+V clipboard history for Linux, Windows and macOS.</strong><br />
  Everything you copy, one shortcut away, plus your iPad as a drawing pad.
</p>

<p align="center">
  <a href="#installation"><strong>Install</strong></a> ·
  <a href="#drawing-on-the-ipad">iPad</a> ·
  <a href="LICENSE">License</a>
</p>

<a href="./.github/screenshot.png">
  <img src="./.github/screenshot.png" width="100%" alt="The Clipstack popup on the desktop and a screenshot being annotated with the Apple Pencil on an iPad" />
</a>

Clipstack runs in the background and keeps a history of what you copy. Press Super+V (Win+V on Windows, ⌘⇧V on macOS), pick an entry, and it is pasted where you were typing.

- **Opens where you work**, next to the text cursor or the mouse pointer.
- **Text, images and files**, with previews.
- **Search and pin** entries you need often.
- **Private**: everything stays on your machine, and passwords from password managers are never stored.
- **Draw on the iPad**: send an image to your iPad, annotate it with the Apple Pencil and get it back on the clipboard.

> [!NOTE]
> Clipstack is made for the COSMIC desktop and runs on other Linux desktops (GNOME, KDE Plasma, Xfce, Cinnamon, Sway, Hyprland, …, Wayland or X11), on Windows 10/11 and on macOS 11+. The drawing pad is made for iPad with Safari and Apple Pencil; other browsers and tablets are untested.

## Installation

Download the file for your system from the [releases](https://github.com/j-gaertig/clipstack/releases) and start Clipstack once. It sets up autostart and the shortcut, then runs in the background. To remove the integration again, run it with `--uninstall`.

| System | File | Shortcut |
| --- | --- | --- |
| Linux, any distribution | `Clipstack-x86_64.AppImage` / `Clipstack-aarch64.AppImage` | Super+V |
| Debian, Ubuntu, Mint, Pop!_OS | `Clipstack-amd64.deb` / `Clipstack-arm64.deb` | Super+V |
| Fedora, openSUSE, RHEL | `Clipstack-x86_64.rpm` / `Clipstack-aarch64.rpm` | Super+V |
| Windows 10/11 | `Clipstack-Setup-x64.exe` (or the portable `Clipstack-windows-x64.zip`) | Win+V |
| macOS (Apple Silicon and Intel) | `Clipstack-macos.dmg` | ⌘⇧V |

AppImage:

```bash
chmod +x Clipstack-x86_64.AppImage
./Clipstack-x86_64.AppImage
```

deb/rpm: install the package and start Clipstack from the app menu.

<details>
<summary>Notes per desktop</summary>

- **COSMIC**: everything works out of the box: overlay at the text cursor, auto paste.
- **GNOME**: Super+V is set up in the GNOME keyboard settings (it replaces GNOME's own Super+V for the notification list, which stays on Super+M). GNOME has no clipboard protocol for Wayland apps, so Clipstack runs through XWayland; the first paste asks once for permission to type (remote desktop portal).
- **KDE Plasma, Hyprland and other Wayland desktops**: the shortcut is requested through the desktop's global shortcuts dialog on first start; KDE asks once for permission to type when pasting. If your desktop has no such dialog (e.g. Sway), bind `cosmic-clipboard` to Super+V yourself.
- **X11 desktops** (Xfce, MATE, Cinnamon, i3, …): Clipstack grabs Super+V itself.
- **Windows**: Clipstack takes over Win+V from the built-in clipboard history while it runs. Windows Firewall asks once about the iPad drawing server.
- **macOS**: pasting needs the Accessibility permission (System Settings → Privacy & Security → Accessibility); macOS asks on the first paste. The app isn't notarized: open it the first time with right click → Open.

</details>

## Drawing on the iPad

The iPad and the PC need to be on the same network.

1. Open the history, open the iPad panel (tablet icon or Ctrl+I) and scan the QR code.
2. Add the page to the Home Screen and enter the pairing code shown on the PC. This is needed only once.
3. Click the pen icon on an image in the history (or press Ctrl+E) to send it to the iPad.
4. Draw, then tap **Send to PC**. The result lands on the clipboard and in the history.

## License

MIT, see [LICENSE](LICENSE). Includes the [Inter](https://rsms.me/inter/) font (OFL) and [Lucide](https://lucide.dev) icons (ISC).
