//! Persistent clipboard history: index in `index.json`, payloads per entry in
//! `items/<id>/<n>` plus a pre-rendered `thumb.png` for images.

use std::{
    fs,
    io::Cursor,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::clipboard::Formats;

pub const MAX_ENTRIES: usize = 50;
const PREVIEW_CHARS: usize = 4000;
const THUMB_MAX_W: u32 = 680;
const THUMB_MAX_H: u32 = 360;
const TEXT_MIME: &str = "text/plain;charset=utf-8";
const TEXT_ALIASES: &[&str] = &["text/plain", "UTF8_STRING", "STRING", "TEXT"];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
    Files,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub id: u64,
    pub kind: Kind,
    pub created: u64,
    #[serde(default)]
    pub pinned: bool,
    pub preview: String,
    pub mimes: Vec<String>,
    #[serde(default)]
    pub image_size: Option<(u32, u32)>,
    /// Drawing sent back from the iPad.
    #[serde(default)]
    pub from_ipad: bool,
    /// Where an image came from (decides its title).
    #[serde(default)]
    pub origin: Origin,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    #[default]
    Other,
    Screenshot,
    /// Edited on the iPad.
    Edited,
    EditedScreenshot,
    /// Drawn on a blank page on the iPad.
    Drawing,
}

#[derive(Clone)]
pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Thumb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Thumb({}x{})", self.width, self.height)
    }
}

#[derive(Debug)]
pub struct Ingested {
    pub entry: Entry,
    pub thumb: Option<Thumb>,
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local/share"))
        .join("cosmic-clipboard")
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into())
}

fn item_dir(id: u64) -> PathBuf {
    data_dir().join("items").join(format!("{id:016x}"))
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

/// Classifies freshly read clipboard data, stores it on disk and prepares a
/// thumbnail. Runs on a worker thread.
pub fn ingest(formats: Vec<(String, Vec<u8>)>) -> Option<Ingested> {
    let mut formats: Vec<(String, Vec<u8>)> = formats
        .into_iter()
        .map(
            |(mime, data)| {
                if TEXT_ALIASES.contains(&mime.as_str()) { (TEXT_MIME.to_string(), data) } else { (mime, data) }
            },
        )
        .collect();
    let get =
        |formats: &[(String, Vec<u8>)], mime: &str| formats.iter().find(|(m, _)| m == mime).map(|(_, d)| d.clone());

    let uris = get(&formats, "text/uri-list")
        .map(|d| String::from_utf8_lossy(&d).into_owned())
        .filter(|s| s.lines().any(|l| l.starts_with("file://")));
    let image_index = formats.iter().position(|(m, _)| m.starts_with("image/"));
    let text =
        get(&formats, TEXT_MIME).map(|d| String::from_utf8_lossy(&d).into_owned()).filter(|t| !t.trim().is_empty());

    let mut thumb = None;
    let mut image_size = None;
    let (kind, preview, hash_data) = if let Some(uris) = uris {
        let paths: Vec<String> = uris
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| uri_to_path(l.trim()))
            .collect();
        (Kind::Files, truncate(&paths.join("\n")), uris.into_bytes())
    } else if let Some(index) = image_index {
        let (mime, data) = formats[index].clone();
        let decoded = image::load_from_memory(&data).ok()?;
        image_size = Some((decoded.width(), decoded.height()));
        if mime != "image/png" && get(&formats, "image/png").is_none() {
            let mut png = Vec::new();
            if decoded.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).is_ok() {
                formats.insert(index, ("image/png".to_string(), png));
            }
        }
        let small = decoded.thumbnail(THUMB_MAX_W, THUMB_MAX_H).to_rgba8();
        thumb = Some(Thumb { width: small.width(), height: small.height(), rgba: small.into_raw() });
        (Kind::Image, String::new(), data)
    } else {
        let text = text?;
        (Kind::Text, truncate(&text), text.into_bytes())
    };

    // Image entries only need image formats and an optional HTML fallback.
    if kind == Kind::Image {
        formats.retain(|(m, _)| m.starts_with("image/") || m == "text/html");
    }

    let id = xxhash_rust::xxh3::xxh3_64(&hash_data) ^ kind as u64;
    let dir = item_dir(id);
    if !dir.join("done").exists() {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).ok()?;
        for (n, (_, data)) in formats.iter().enumerate() {
            fs::write(dir.join(n.to_string()), data).ok()?;
        }
        if let Some(t) = &thumb
            && let Some(img) = image::RgbaImage::from_raw(t.width, t.height, t.rgba.clone())
        {
            let _ = img.save_with_format(dir.join("thumb.png"), image::ImageFormat::Png);
        }
        fs::write(dir.join("done"), b"").ok()?;
    }

    Some(Ingested {
        entry: Entry {
            id,
            kind,
            created: now(),
            pinned: false,
            preview,
            mimes: formats.into_iter().map(|(m, _)| m).collect(),
            image_size,
            from_ipad: false,
            origin: Origin::Other,
        },
        thumb,
    })
}

fn truncate(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((i, _)) => text[..i].to_string(),
        None => text.to_string(),
    }
}

fn uri_to_path(uri: &str) -> String {
    match url::Url::parse(uri) {
        Ok(url) if url.scheme() == "file" => {
            url.to_file_path().map(|p| p.display().to_string()).unwrap_or_else(|_| uri.to_string())
        }
        _ => percent_encoding::percent_decode_str(uri).decode_utf8_lossy().into_owned(),
    }
}

pub fn load_thumb(id: u64) -> Option<Thumb> {
    let img = image::open(item_dir(id).join("thumb.png")).ok()?.to_rgba8();
    Some(Thumb { width: img.width(), height: img.height(), rgba: img.into_raw() })
}

/// Reads all stored formats of an entry and expands them to the mime types
/// applications commonly ask for.
pub fn formats(entry: &Entry) -> Option<Formats> {
    let dir = item_dir(entry.id);
    let mut out: Vec<(String, Arc<Vec<u8>>)> = Vec::new();
    for (n, mime) in entry.mimes.iter().enumerate() {
        let data = Arc::new(fs::read(dir.join(n.to_string())).ok()?);
        out.push((mime.clone(), data.clone()));
        if mime == TEXT_MIME {
            for alias in TEXT_ALIASES {
                out.push((alias.to_string(), data.clone()));
            }
        }
    }
    if entry.kind == Kind::Files
        && !entry.mimes.iter().any(|m| m == "x-special/gnome-copied-files")
        && let Some((_, uris)) = out.iter().find(|(m, _)| m == "text/uri-list")
    {
        let uris: Vec<&str> = std::str::from_utf8(uris)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .collect();
        let gnome = format!("copy\n{}", uris.join("\n")).into_bytes();
        out.push(("x-special/gnome-copied-files".to_string(), Arc::new(gnome)));
    }
    (!out.is_empty()).then(|| Arc::new(out))
}

#[derive(Default)]
pub struct History {
    pub entries: Vec<Entry>,
}

impl History {
    pub fn load() -> Self {
        let entries: Vec<Entry> = fs::read(data_dir().join("index.json"))
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default();
        let entries = entries.into_iter().filter(|e| item_dir(e.id).join("done").exists()).collect();
        Self { entries }
    }

    pub fn save(&self) {
        let dir = data_dir();
        let _ = fs::create_dir_all(&dir);
        let tmp = dir.join("index.json.tmp");
        if let Ok(data) = serde_json::to_vec_pretty(&self.entries)
            && fs::write(&tmp, data).is_ok()
        {
            let _ = fs::rename(&tmp, dir.join("index.json"));
        }
    }

    /// Inserts a new entry at the top (or moves an identical one up).
    /// Returns ids whose data was evicted.
    pub fn add(&mut self, mut entry: Entry) -> Vec<u64> {
        if let Some(pos) = self.entries.iter().position(|e| e.id == entry.id) {
            let old = self.entries.remove(pos);
            entry.pinned = old.pinned;
            entry.from_ipad |= old.from_ipad;
            if entry.origin == Origin::Other {
                entry.origin = old.origin;
            }
        }
        self.entries.insert(0, entry);
        let mut evicted = Vec::new();
        let mut unpinned = 0;
        self.entries.retain(|e| {
            if e.pinned {
                return true;
            }
            unpinned += 1;
            if unpinned > MAX_ENTRIES {
                evicted.push(e.id);
                false
            } else {
                true
            }
        });
        for id in &evicted {
            remove_data(*id);
        }
        self.save();
        evicted
    }

    pub fn bump(&mut self, id: u64) {
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            let mut entry = self.entries.remove(pos);
            entry.created = now();
            self.entries.insert(0, entry);
            self.save();
        }
    }

    pub fn remove(&mut self, id: u64) {
        self.entries.retain(|e| e.id != id);
        remove_data(id);
        self.save();
    }

    pub fn toggle_pin(&mut self, id: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.pinned = !e.pinned;
            self.save();
        }
    }

    /// Removes all unpinned entries, returning their ids.
    pub fn clear(&mut self) -> Vec<u64> {
        let removed: Vec<u64> = self.entries.iter().filter(|e| !e.pinned).map(|e| e.id).collect();
        self.entries.retain(|e| e.pinned);
        for id in &removed {
            remove_data(*id);
        }
        self.save();
        removed
    }
}

fn remove_data(id: u64) {
    let _ = fs::remove_dir_all(item_dir(id));
}
