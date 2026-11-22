//! Small web server for drawing on an iPad: the PC offers one image at a time
//! (`/api/state`, `/api/image/<id>`, long poll `/api/events`), the web app
//! (`web/`) sends the finished drawing back to `/api/submit`.
//!
//! The URL never changes. A device is paired once with a short code shown on
//! the PC (`/api/pair`) and then keeps a long-lived device token (cookie and
//! localStorage) that is remembered in `devices.json`.

use std::{
    io::{Cursor, Read},
    net::{Shutdown, TcpListener, TcpStream, UdpSocket},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use tiny_http::{Header, Method, Request, Response, Server};

use crate::history::home;

pub const DEFAULT_PORT: u16 = 8765;
const MAX_UPLOAD: usize = 64 * 1024 * 1024;
const PAIRING_VALIDITY: Duration = Duration::from_secs(10 * 60);
const PAIRING_ATTEMPTS: u32 = 5;
const COOKIE: &str = "cc_device";

const ASSETS: &[(&str, &str, &[u8])] = &[
    ("/", "text/html; charset=utf-8", include_bytes!("../web/index.html")),
    ("/index.html", "text/html; charset=utf-8", include_bytes!("../web/index.html")),
    ("/app.js", "text/javascript; charset=utf-8", include_bytes!("../web/app.js")),
    ("/app.css", "text/css; charset=utf-8", include_bytes!("../web/app.css")),
    ("/draw.js", "text/javascript; charset=utf-8", include_bytes!("../web/draw.js")),
    ("/icon-180.png", "image/png", include_bytes!("../web/icon-180.png")),
    ("/icon-512.png", "image/png", include_bytes!("../web/icon-512.png")),
];

struct Job {
    id: u64,
    png: Arc<Vec<u8>>,
    width: u32,
    height: u32,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Device {
    token: String,
    name: String,
    paired: u64,
    /// User agent at pairing time (browser, OS version).
    #[serde(default)]
    agent: String,
    /// Unix time of the last authorized request.
    #[serde(default)]
    last_seen: u64,
    /// `last_seen` as written to disk (saved at most every few minutes).
    #[serde(skip)]
    saved_seen: u64,
}

/// A paired device as shown in the iPad panel.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Opaque id for `forget_device`.
    pub id: String,
    /// iPad, iPhone, Android or Browser.
    pub kind: String,
    /// Browser and OS, e.g. "Safari, iPadOS 18.5".
    pub detail: String,
    pub paired: u64,
    pub last_seen: u64,
    /// Polled within the last half minute (the long poll repeats every 20 s).
    pub online: bool,
}

/// "Safari, iPadOS 18.5" from a user agent (best effort).
fn describe_agent(kind: &str, agent: &str) -> String {
    let browser = if agent.contains("CriOS") || (agent.contains("Chrome/") && !agent.contains("Edg")) {
        "Chrome"
    } else if agent.contains("FxiOS") || agent.contains("Firefox/") {
        "Firefox"
    } else if agent.contains("Edg") {
        "Edge"
    } else if agent.contains("Safari/") {
        "Safari"
    } else if matches!(kind, "iPad" | "iPhone") && agent.contains("AppleWebKit") {
        // Web apps opened from the Home Screen drop "Safari" from the agent.
        "Home Screen app"
    } else {
        ""
    };
    let version = |marker: &str| {
        let rest = agent.split_once(marker)?.1;
        let v: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '_' || *c == '.').collect();
        (!v.is_empty()).then(|| v.replace('_', "."))
    };
    let os = match kind {
        // iPadOS usually poses as a Mac ("Mac OS X 10_15_7"); then only
        // Safari's "Version/x" matches the iPadOS release.
        "iPad" if agent.contains("Macintosh") => version("Version/").map(|v| format!("iPadOS {v}")),
        "iPad" => version("CPU OS ").map(|v| format!("iPadOS {v}")),
        "iPhone" => version("iPhone OS ").map(|v| format!("iOS {v}")),
        "Android" => version("Android ").map(|v| format!("Android {v}")),
        _ => None,
    };
    [Some(browser.to_string()).filter(|b| !b.is_empty()), os].into_iter().flatten().collect::<Vec<_>>().join(", ")
}

struct Pairing {
    code: String,
    expires: Instant,
    attempts: u32,
}

#[derive(Default)]
struct Shared {
    job: Option<Job>,
    next_id: u64,
    devices: Vec<Device>,
    pairing: Option<Pairing>,
    /// Recent events for the web app's long poll (`/api/events`).
    events: Vec<(u64, &'static str)>,
    seq: u64,
}

impl Shared {
    fn push_event(&mut self, kind: &'static str) {
        self.seq += 1;
        self.events.push((self.seq, kind));
        if self.events.len() > 32 {
            self.events.remove(0);
        }
    }
}

/// Handle used by the UI to offer images and show connection info.
#[derive(Clone)]
pub struct DrawingServer {
    shared: Arc<Mutex<Shared>>,
    changed: Arc<Condvar>,
    port: u16,
}

impl std::fmt::Debug for DrawingServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DrawingServer(port {})", self.port)
    }
}

impl DrawingServer {
    /// Offers a PNG to the iPad. Returns false if it isn't a decodable image.
    pub fn offer_png(&self, png: Vec<u8>) -> bool {
        let Ok(size) = image::ImageReader::new(Cursor::new(&png))
            .with_guessed_format()
            .map_err(|_| ())
            .and_then(|r| r.into_dimensions().map_err(|_| ()))
        else {
            return false;
        };
        let mut shared = self.shared.lock().unwrap();
        shared.next_id += 1;
        let id = shared.next_id;
        shared.job = Some(Job { id, png: Arc::new(png), width: size.0, height: size.1 });
        shared.push_event("image");
        self.changed.notify_all();
        true
    }

    /// Creates a fresh one-time pairing code (valid for 10 minutes).
    pub fn new_pairing_code(&self) -> String {
        let mut bytes = [0u8; 4];
        random(&mut bytes);
        let code = format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000);
        self.shared.lock().unwrap().pairing =
            Some(Pairing { code: code.clone(), expires: Instant::now() + PAIRING_VALIDITY, attempts: 0 });
        code
    }

    /// Paired devices, most recently seen first.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let now = crate::history::now();
        let mut list: Vec<DeviceInfo> = self
            .shared
            .lock()
            .unwrap()
            .devices
            .iter()
            .map(|d| DeviceInfo {
                id: d.token.clone(),
                kind: d.name.clone(),
                detail: describe_agent(&d.name, &d.agent),
                paired: d.paired,
                last_seen: d.last_seen,
                online: d.last_seen > 0 && now.saturating_sub(d.last_seen) < 30,
            })
            .collect();
        list.sort_by_key(|d| std::cmp::Reverse(d.last_seen.max(d.paired)));
        list
    }

    /// Unpairs one device; its web app asks for a new code on the next request.
    pub fn forget_device(&self, id: &str) {
        let mut shared = self.shared.lock().unwrap();
        shared.devices.retain(|d| d.token != id);
        save_devices(&shared.devices);
    }

    /// URL to open on the iPad: the LAN address (the `.local` name doesn't
    /// resolve on devices running Tailscale), else the mDNS name.
    pub fn url(&self) -> String {
        let host = lan_ip().or_else(mdns_name).unwrap_or_else(|| "localhost".into());
        self.url_for(&host)
    }

    fn url_for(&self, host: &str) -> String {
        format!("http://{host}:{}/", self.port)
    }
}

fn mdns_name() -> Option<String> {
    if cfg!(target_os = "linux") && !std::path::Path::new("/run/avahi-daemon/socket").exists() {
        return None;
    }
    let name = hostname();
    (!name.is_empty()).then(|| format!("{}.local", name.trim_end_matches(".local")))
}

/// The computer's name, shown in the web app.
fn hostname() -> String {
    #[cfg(target_os = "windows")]
    return std::env::var("COMPUTERNAME").unwrap_or_default();
    #[cfg(target_os = "macos")]
    return std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    #[cfg(target_os = "linux")]
    std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_string()).unwrap_or_default()
}

fn lan_ip() -> Option<String> {
    // Connecting a UDP socket sends nothing but selects the outgoing interface.
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

/// `XDG_PICTURES_DIR` from user-dirs.dirs, e.g. `~/Pictures` (the system's
/// pictures folder on Windows and macOS).
pub fn pictures_dir() -> PathBuf {
    if !cfg!(target_os = "linux") {
        return dirs::picture_dir().unwrap_or_else(|| home().join("Pictures"));
    }
    let home = home();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                let value = line.trim().strip_prefix("XDG_PICTURES_DIR=")?.trim_matches('"');
                let path = match value.strip_prefix("$HOME") {
                    Some(rest) => home.join(rest.trim_start_matches('/')),
                    None => PathBuf::from(value),
                };
                path.is_absolute().then_some(path)
            })
        })
        .unwrap_or_else(|| home.join("Pictures"))
}

/// Decides whether a file name belongs to a screenshot.
pub type NameFilter = fn(&str) -> bool;

/// Where screenshot tools save their files (the screenshot is copied to the
/// clipboard at the same time), with a filter for the file names.
pub fn screenshot_dirs() -> Vec<(PathBuf, NameFilter)> {
    let any: NameFilter = |_| true;
    let mut out = vec![(pictures_dir().join("Screenshots"), any)];
    if cfg!(target_os = "macos") {
        // macOS puts them on the desktop by default.
        let named: fn(&str) -> bool =
            |n| ["Screenshot", "Bildschirmfoto", "Screen Shot"].iter().any(|p| n.starts_with(p));
        out.push((dirs::desktop_dir().unwrap_or_else(|| home().join("Desktop")), named));
    }
    out
}

fn devices_file() -> PathBuf {
    if !cfg!(target_os = "linux") {
        return dirs::config_dir().unwrap_or_else(home).join("Clipstack/devices.json");
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
        .join("cosmic-clipboard/devices.json")
}

fn load_devices() -> Vec<Device> {
    std::fs::read(devices_file()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
}

fn save_devices(devices: &[Device]) {
    let path = devices_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    let Ok(data) = serde_json::to_vec_pretty(devices) else { return };
    if std::fs::write(&tmp, data).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("no randomness available");
}

fn new_device_token() -> String {
    let mut bytes = [0u8; 24];
    random(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A drawing from the iPad: the PNG and whether it was a blank page.
pub type Received = Arc<dyn Fn(Vec<u8>, bool) + Send + Sync>;

pub fn start(on_received: Received) -> Result<DrawingServer, String> {
    let port = std::env::var("COSMIC_CLIPBOARD_DRAWING_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("Port {port} unavailable: {e}"))?;
    let server = Server::http("127.0.0.1:0").map_err(|e| format!("drawing server: {e}"))?;
    let inner = server.server_addr().to_ip().ok_or("drawing server has no address")?;
    std::thread::Builder::new()
        .name("drawing-front".into())
        .spawn(move || {
            for client in listener.incoming().flatten() {
                std::thread::spawn(move || front(client, inner));
            }
        })
        .map_err(|e| e.to_string())?;
    let handle = DrawingServer {
        shared: Arc::new(Mutex::new(Shared {
            devices: load_devices(),
            // Unique across restarts, so a page restored on the iPad is never
            // mistaken for a new image with the same id.
            next_id: crate::history::now() * 1000,
            ..Default::default()
        })),
        changed: Arc::default(),
        port,
    };
    let state = handle.clone();
    std::thread::Builder::new()
        .name("drawing-server".into())
        .spawn(move || {
            for request in server.incoming_requests() {
                let state = state.clone();
                let on_received = on_received.clone();
                std::thread::spawn(move || handle_request(&state, request, &on_received));
            }
        })
        .map_err(|e| e.to_string())?;
    tracing::info!("drawing server listening on port {port}");
    Ok(handle)
}

/// Safari first tries `https://` on the same port. tiny_http would wait
/// forever for a request line in the TLS handshake and the iPad stayed black
/// for ~10 s, so TLS connections are closed right away (Safari then falls
/// back to plain HTTP) and everything else is passed on to tiny_http.
fn front(client: TcpStream, inner: std::net::SocketAddr) {
    let _ = client.set_read_timeout(Some(Duration::from_secs(30)));
    let mut first = [0u8; 1];
    match client.peek(&mut first) {
        Ok(1) if first[0] != 0x16 => {}
        _ => return,
    }
    let _ = client.set_read_timeout(None);
    let Ok(server) = TcpStream::connect(inner) else { return };
    let _ = client.set_nodelay(true);
    let _ = server.set_nodelay(true);
    let (Ok(mut client_r), Ok(mut server_w)) = (client.try_clone(), server.try_clone()) else { return };
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_r, &mut server_w);
        let _ = server_w.shutdown(Shutdown::Write);
    });
    let (mut server_r, mut client_w) = (server, client);
    let _ = std::io::copy(&mut server_r, &mut client_w);
    let _ = client_w.shutdown(Shutdown::Both);
    let _ = up.join();
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn query_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    url.split_once('?')?.1.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn request_header(request: &Request, name: &'static str) -> Option<String> {
    request.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str().to_string())
}

/// The device token from the `X-Token` header, the cookie or `?t=`.
fn device_token(request: &Request) -> Option<String> {
    request_header(request, "X-Token")
        .filter(|t| !t.is_empty())
        .or_else(|| {
            request_header(request, "Cookie").and_then(|cookies| {
                cookies.split(';').find_map(|c| {
                    let (k, v) = c.trim().split_once('=')?;
                    (k == COOKIE).then(|| v.to_string())
                })
            })
        })
        .or_else(|| query_param(request.url(), "t").map(str::to_string))
        .filter(|t| !t.is_empty())
}

fn authorized(state: &DrawingServer, request: &Request) -> bool {
    let Some(given) = device_token(request) else {
        return false;
    };
    let mut shared = state.shared.lock().unwrap();
    let Some(device) = shared.devices.iter_mut().find(|d| constant_eq(&d.token, &given)) else {
        return false;
    };
    let now = crate::history::now();
    device.last_seen = now;
    // The long poll comes every 20 s: don't rewrite the file each time.
    if now.saturating_sub(device.saved_seen) > 300 {
        device.saved_seen = now;
        save_devices(&shared.devices);
    }
    true
}

fn pair(state: &DrawingServer, code: &str, user_agent: &str) -> Result<String, &'static str> {
    let mut shared = state.shared.lock().unwrap();
    let pairing = shared.pairing.as_mut().ok_or("No pairing code active. Open the iPad panel on the PC.")?;
    if pairing.expires < Instant::now() || pairing.attempts >= PAIRING_ATTEMPTS {
        shared.pairing = None;
        return Err("Code expired. Open the iPad panel on the PC again.");
    }
    if !constant_eq(&pairing.code, code) {
        pairing.attempts += 1;
        return Err("Wrong code");
    }
    shared.pairing = None;
    let name = if user_agent.contains("iPad") || user_agent.contains("Macintosh") {
        "iPad"
    } else if user_agent.contains("iPhone") {
        "iPhone"
    } else if user_agent.contains("Android") {
        "Android"
    } else {
        "Browser"
    };
    let token = new_device_token();
    let now = crate::history::now();
    shared.devices.push(Device {
        token: token.clone(),
        name: name.to_string(),
        paired: now,
        agent: user_agent.chars().take(300).collect(),
        last_seen: now,
        saved_seen: now,
    });
    save_devices(&shared.devices);
    Ok(token)
}

fn text(status: u16, body: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

fn handle_request(state: &DrawingServer, mut request: Request, on_received: &Received) {
    let path = request.url().split('?').next().unwrap_or("/").to_string();
    let no_store = header("Cache-Control", "no-store");

    let method = request.method().clone();
    let response = match (method, path.as_str()) {
        (Method::Get, "/manifest.webmanifest") => {
            let manifest = serde_json::json!({
                "name": "Clipstack",
                "short_name": "Clipstack",
                "start_url": "/",
                "scope": "/",
                "display": "standalone",
                "orientation": "any",
                "background_color": "#111113",
                "theme_color": "#111113",
                "icons": [
                    { "src": "/icon-180.png", "sizes": "180x180", "type": "image/png" },
                    { "src": "/icon-512.png", "sizes": "512x512", "type": "image/png" }
                ]
            });
            Response::from_string(manifest.to_string()).with_header(header("Content-Type", "application/manifest+json"))
        }
        (Method::Get, p) if ASSETS.iter().any(|(a, _, _)| *a == p) => {
            let (_, mime, data) = ASSETS.iter().find(|(a, _, _)| *a == p).unwrap();
            Response::from_data(data.to_vec()).with_header(header("Content-Type", mime)).with_header(no_store)
        }
        (Method::Post, "/api/pair") => {
            let mut body = String::new();
            let _ = request.as_reader().take(256).read_to_string(&mut body);
            let code: String = body.chars().filter(char::is_ascii_digit).collect();
            let agent = request_header(&request, "User-Agent").unwrap_or_default();
            match pair(state, &code, &agent) {
                Ok(token) => Response::from_string(serde_json::json!({ "token": token }).to_string())
                    .with_header(header("Content-Type", "application/json"))
                    .with_header(header(
                        "Set-Cookie",
                        &format!("{COOKIE}={token}; Max-Age=315360000; Path=/; HttpOnly; SameSite=Strict"),
                    )),
                Err(msg) => text(403, msg),
            }
        }
        _ if path.starts_with("/api/") && !authorized(state, &request) => text(401, "Device not paired"),
        (Method::Get, "/api/state") => {
            let shared = state.shared.lock().unwrap();
            let host = hostname();
            let body = match &shared.job {
                Some(job) => serde_json::json!({
                    "id": job.id, "width": job.width, "height": job.height, "host": host
                }),
                None => serde_json::json!({ "id": null, "host": host }),
            };
            Response::from_string(body.to_string())
                .with_header(header("Content-Type", "application/json"))
                .with_header(no_store)
        }
        // Long poll: answers as soon as a new image is offered or after 20 s.
        // `since=-1` only returns the current seq.
        (Method::Get, "/api/events") => {
            let since: i64 = query_param(request.url(), "since").and_then(|v| v.parse().ok()).unwrap_or(-1);
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut shared = state.shared.lock().unwrap();
            while since >= 0 && since as u64 == shared.seq {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                shared = state.changed.wait_timeout(shared, left).unwrap().0;
            }
            let events: Vec<&str> = if since < 0 || since as u64 > shared.seq {
                Vec::new()
            } else {
                shared.events.iter().filter(|(n, _)| *n as i64 > since).map(|(_, k)| *k).collect()
            };
            let body = serde_json::json!({ "seq": shared.seq, "events": events });
            Response::from_string(body.to_string())
                .with_header(header("Content-Type", "application/json"))
                .with_header(no_store)
        }
        (Method::Get, p) if p.starts_with("/api/image/") => {
            let id: Option<u64> = p.trim_start_matches("/api/image/").parse().ok();
            let png = {
                let shared = state.shared.lock().unwrap();
                shared.job.as_ref().filter(|j| Some(j.id) == id).map(|j| j.png.clone())
            };
            match png {
                Some(png) => Response::from_data(png.to_vec())
                    .with_header(header("Content-Type", "image/png"))
                    .with_header(no_store),
                None => text(404, "Image no longer available"),
            }
        }
        (Method::Post, "/api/submit") => {
            let mut body = Vec::new();
            let read = request.as_reader().take(MAX_UPLOAD as u64 + 1).read_to_end(&mut body);
            if read.is_err() || body.len() > MAX_UPLOAD {
                text(413, "Image too large")
            } else if !body.starts_with(b"\x89PNG\r\n\x1a\n") {
                text(400, "Not a PNG")
            } else {
                on_received(body, query_param(request.url(), "blank") == Some("1"));
                text(200, "ok")
            }
        }
        _ => text(404, "Not found"),
    };
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_query() {
        assert_eq!(query_param("/api/x?a=1&t=abc", "t"), Some("abc"));
        assert_eq!(query_param("/api/x", "t"), None);
    }

    #[test]
    fn describes_user_agents() {
        let ipad = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.5 Safari/605.1.15";
        assert_eq!(describe_agent("iPad", ipad), "Safari, iPadOS 18.5");
        let home_screen = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)";
        assert_eq!(describe_agent("iPad", home_screen), "Home Screen app");
        let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_4_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/124.0 Mobile/15E148 Safari/604.1";
        assert_eq!(describe_agent("iPhone", iphone), "Chrome, iOS 17.4.1");
        let android = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36";
        assert_eq!(describe_agent("Android", android), "Chrome, Android 14");
        assert_eq!(describe_agent("iPad", ""), "");
    }
}
