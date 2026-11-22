use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use cosmic::{
    Action, Application, Element, Task, app,
    iced::{
        self, Alignment, Background, Border, Color, Length, Padding, Point, Shadow, Size, Subscription, Vector,
        advanced::text::{Ellipsize, EllipsizeHeightLimit},
        event::{self, Event},
        futures::{SinkExt, StreamExt, channel::mpsc},
        keyboard::{self, Key, key::Named},
        mouse,
        widget::scrollable::{AbsoluteOffset, Viewport},
        window,
    },
    theme,
    widget::{self, button, column, container, icon, image, mouse_area, qr_code, row, scrollable},
};
#[cfg(target_os = "linux")]
use cosmic::{
    cctk::sctk::reexports::client::protocol::wl_output::WlOutput,
    iced::{
        Limits,
        event::wayland,
        platform_specific::{
            runtime::wayland::layer_surface::{IcedOutput, SctkLayerSurfaceSettings},
            shell::commands::layer_surface::{
                Anchor, KeyboardInteractivity, Layer, destroy_layer_surface, get_layer_surface,
            },
        },
    },
};

use crate::{
    clipboard::{self, Command, CommandSender, CursorPos},
    drawing::{self, DrawingServer},
    history::{self, Entry, History, Kind, Origin},
    popup,
};

/// A Wayland output (overlay mode only).
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone, PartialEq)]
pub struct WlOutput;

pub const APP_ID: &str = "com.github.jgaertig.CosmicClipboard";
pub const BACKGROUND_ACTION: &str = "background";

const CARD_W: f32 = 440.0;
const CARD_H: f32 = 420.0;
const MARGIN: f32 = 10.0;
const ITEM_SPACING: f32 = 1.0;
/// Every entry has the same height, whatever its kind.
const ROW_H: f32 = 48.0;
const THUMB_W: f32 = 60.0;
const THUMB_H: f32 = 38.0;
const GROUP_H: f32 = 26.0;
/// Search row + divider above and footer below the list.
const CHROME_H: f32 = 44.0 + 1.0 + 8.0 + 36.0;

const INTER: cosmic::iced::Font = cosmic::iced::Font::with_name("Inter");
const INTER_MED: cosmic::iced::Font = cosmic::iced::Font { weight: cosmic::iced::font::Weight::Medium, ..INTER };
const FONTS: [&[u8]; 2] =
    [include_bytes!("../assets/fonts/Inter-Regular.ttf"), include_bytes!("../assets/fonts/Inter-Medium.ttf")];

/// Text in Inter, 13 px by default.
fn txt<'a>(
    s: impl cosmic::iced::widget::text::IntoFragment<'a>,
) -> cosmic::iced::widget::Text<'a, cosmic::Theme, cosmic::Renderer> {
    cosmic::iced::widget::text(s).font(INTER).size(13)
}

/// Lucide icon, tinted with the surrounding text color.
fn lucide(name: &str) -> icon::Handle {
    let bytes: &'static [u8] = match name {
        "search" => include_bytes!("../assets/icons/search.svg"),
        "pin" => include_bytes!("../assets/icons/pin.svg"),
        "pin-off" => include_bytes!("../assets/icons/pin-off.svg"),
        "trash" => include_bytes!("../assets/icons/trash.svg"),
        "tablet-smartphone" => include_bytes!("../assets/icons/tablet-smartphone.svg"),
        "pen-line" => include_bytes!("../assets/icons/pen-line.svg"),
        "folder" => include_bytes!("../assets/icons/folder.svg"),
        "image" => include_bytes!("../assets/icons/image.svg"),
        "list-x" => include_bytes!("../assets/icons/list-x.svg"),
        "x" => include_bytes!("../assets/icons/x.svg"),
        "file" => include_bytes!("../assets/icons/file.svg"),
        "files" => include_bytes!("../assets/icons/files.svg"),
        "text" => include_bytes!("../assets/icons/text.svg"),
        "tablet" => include_bytes!("../assets/icons/tablet.svg"),
        "smartphone" => include_bytes!("../assets/icons/smartphone.svg"),
        "monitor" => include_bytes!("../assets/icons/monitor.svg"),
        "copy" => include_bytes!("../assets/icons/copy.svg"),
        "check" => include_bytes!("../assets/icons/check.svg"),
        "maximize-2" => include_bytes!("../assets/icons/maximize-2.svg"),
        "minimize-2" => include_bytes!("../assets/icons/minimize-2.svg"),
        _ => include_bytes!("../assets/icons/file-text.svg"),
    };
    let mut handle = icon::from_svg_bytes(bytes);
    handle.symbolic = true;
    handle
}

mod pal {
    use cosmic::iced::Color;
    const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::from_rgb8(r, g, b)
    }
    // Flat, neutral dark palette: near-black surfaces, hairline borders, small radii.
    pub const CARD: Color = rgb(0x0e, 0x0e, 0x0e);
    pub const BORDER: Color = rgb(0x26, 0x26, 0x26);
    pub const ACCENT: Color = rgb(0x1d, 0x1d, 0x1d);
    pub const ACCENT_STRONG: Color = rgb(0x24, 0x24, 0x24);
    pub const KBD: Color = rgb(0x26, 0x26, 0x26);
    pub const KBD_TEXT: Color = rgb(0xe5, 0xe5, 0xe5);
    pub const TEXT: Color = rgb(0xf5, 0xf5, 0xf5);
    pub const MUTED: Color = rgb(0xa3, 0xa3, 0xa3);
    pub const ICON: Color = rgb(0x8a, 0x8a, 0x8a);
    pub const PRIMARY: Color = rgb(0x4d, 0x6b, 0xfa);
    pub const R_CARD: f32 = 12.0;
    pub const R_ROW: f32 = 6.0;
    pub const R_BUTTON: f32 = 6.0;
}

/// A quick eased transition between 0 (closed) and 1 (open).
#[derive(Debug, Clone, Copy)]
struct Anim {
    from: f32,
    to: f32,
    start: std::time::Instant,
}

const ANIM_SECS: f32 = 0.13;

impl Default for Anim {
    fn default() -> Self {
        Self { from: 0.0, to: 0.0, start: std::time::Instant::now() }
    }
}

impl Anim {
    fn progress(&self) -> f32 {
        (self.start.elapsed().as_secs_f32() / ANIM_SECS).min(1.0)
    }
    /// Current value, ease-out cubic.
    fn value(&self) -> f32 {
        let t = self.progress();
        self.from + (self.to - self.from) * (1.0 - (1.0 - t).powi(3))
    }
    fn done(&self) -> bool {
        self.progress() >= 1.0
    }
    fn go(&mut self, to: f32) {
        if self.to != to {
            *self = Self { from: self.value(), to, start: std::time::Instant::now() };
        }
    }
}

/// Something that can be shown as a picture: a thumbnail in the list and,
/// enlarged, in the hover preview.
#[derive(Debug, Clone)]
enum Visual {
    Raster(image::Handle),
    Svg(cosmic::iced::widget::svg::Handle),
}

const RASTER_EXT: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp", "ico"];

/// The first file of a file entry that can be previewed (images, SVG).
fn file_visual(entry: &Entry) -> Option<(PathBuf, Visual)> {
    entry.preview.lines().find_map(|p| {
        let path = std::path::Path::new(p);
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        std::fs::metadata(path).ok().filter(|m| m.is_file() && m.len() < 64 * 1024 * 1024)?;
        if RASTER_EXT.contains(&ext.as_str()) {
            Some((path.to_path_buf(), Visual::Raster(image::Handle::from_path(path))))
        } else if ext == "svg" {
            Some((path.to_path_buf(), Visual::Svg(cosmic::iced::widget::svg::Handle::from_path(path))))
        } else {
            None
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Placement {
    /// Top-left corner; flips up above `.1` (or the point) if it doesn't fit.
    At(Point, Option<f32>),
    Center,
}

/// Computes the card rectangle inside the overlay: at the pointer, flipped
/// upwards/leftwards when it wouldn't fit, like a context menu.
fn place(placement: Placement, offset: Vector, size: Size) -> (f32, f32, f32, f32) {
    let card_w = CARD_W.min(size.width - 2.0 * MARGIN).max(1.0);
    let card_h = CARD_H.min(size.height - 2.0 * MARGIN).max(1.0);
    let (x, y) = match placement {
        Placement::Center => ((size.width - card_w) / 2.0, (size.height - card_h) / 2.0),
        Placement::At(p, above) => {
            let x = if p.x + card_w + MARGIN > size.width { p.x - card_w } else { p.x };
            let y = if p.y + card_h + MARGIN > size.height { above.unwrap_or(p.y) - card_h } else { p.y };
            (x, y)
        }
    };
    let clamp_x = |x: f32| x.min(size.width - card_w - MARGIN).max(MARGIN);
    let clamp_y = |y: f32| y.min(size.height - card_h - MARGIN).max(MARGIN);
    (clamp_x(clamp_x(x) + offset.x), clamp_y(clamp_y(y) + offset.y), card_w, card_h)
}

#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub background: bool,
}

impl cosmic::app::CosmicFlags for Flags {
    type SubCommand = String;
    type Args = Vec<String>;

    fn action(&self) -> Option<&String> {
        static BG: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| BACKGROUND_ACTION.to_string());
        self.background.then(|| &*BG)
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Clipboard(clipboard::Event),
    Toggle,
    Hide,
    LocateTimeout(u64),
    Opened,
    /// The popup window (platforms without layer-shell) exists.
    PopupOpened(window::Id),
    /// The popup window gained (true) or lost keyboard focus.
    PopupFocus(bool),
    /// The popup lost focus a moment ago.
    FocusLost(Option<window::Id>),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    OutputAdded(String, WlOutput),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    OutputRemoved(WlOutput),
    Search(String),
    Paste(u64),
    Delete(u64),
    TogglePin(u64),
    ClearAll,
    Key(Key, keyboard::Modifiers),
    Scrolled(Viewport),
    DrawingReady(Result<DrawingServer, String>),
    DrawingReceived(Arc<Vec<u8>>, bool),
    DrawingSaved(Option<(Arc<history::Ingested>, PathBuf)>),
    ToggleIpadPanel,
    SendToIpad(u64),
    ForgetDevice(String),
    QrHover(bool),
    ThumbEnter(u64),
    ThumbLeave(u64),
    PreviewOpen(u64, u64),
    QrZoom(bool),
    Frame,
    CopyPairCode,
    Hover(u64),
    DragStart,
    CursorMoved(window::Id, Point),
    DragEnd,
    ThumbsLoaded(Arc<Vec<(u64, history::Thumb)>>),
    Noop,
}

pub struct App {
    core: cosmic::Core,
    history: History,
    thumbs: HashMap<u64, image::Handle>,
    backend: Option<CommandSender>,
    backend_error: Option<String>,
    surface: Option<window::Id>,
    generation: u64,
    locating: Option<u64>,
    placement: Placement,
    outputs: HashMap<String, WlOutput>,
    query: String,
    selected: usize,
    scroll_y: f32,
    search_id: widget::Id,
    scroll_id: widget::Id,
    drawing: Option<Result<DrawingServer, String>>,
    ipad_panel: bool,
    ipad_qr: Option<qr_code::Data>,
    pair_code: Option<String>,
    qr_hover: bool,
    /// The QR code fills the whole card.
    qr_zoom: bool,
    /// When the pairing code was copied (shows a check mark for a moment).
    code_copied: Option<std::time::Instant>,
    /// Origin of the image last sent to the iPad (names what comes back).
    offered: Option<Origin>,
    /// Pointer resting on a thumbnail: (entry, where it rests, rest generation).
    thumb_hover: Option<(u64, Point, u64)>,
    /// Enlarged image while the pointer rests on its thumbnail.
    /// (entry, pointer position when opened, picture, its size)
    preview: Option<(u64, Point, Visual, (f32, f32))>,
    preview_anim: Anim,
    qr_anim: Anim,
    /// Lowercased search text per entry (same order as `history.entries`).
    haystack: Vec<String>,
    cursor: Point,
    offset: Vector,
    drag: Option<(Point, Vector)>,
    /// Where the popup window goes, in physical screen coordinates.
    popup_at: Option<(i32, i32)>,
    /// The popup has keyboard focus, and had it since it opened.
    popup_focus: (bool, bool),
}

impl Application for App {
    type Executor = cosmic::executor::Default;
    type Flags = Flags;
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, flags: Flags) -> (Self, Task<Action<Message>>) {
        let history = History::load();
        // Thumbnails are decoded in parallel off the UI thread.
        let images: Vec<u64> = history.entries.iter().filter(|e| e.kind == Kind::Image).map(|e| e.id).collect();
        let load_thumbs = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    std::thread::scope(|scope| {
                        let workers: Vec<_> = images
                            .chunks(images.len().div_ceil(4).max(1))
                            .map(|chunk| {
                                scope.spawn(move || {
                                    chunk
                                        .iter()
                                        .filter_map(|id| history::load_thumb(*id).map(|t| (*id, t)))
                                        .collect::<Vec<_>>()
                                })
                            })
                            .collect();
                        workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect::<Vec<_>>()
                    })
                })
                .await
                .unwrap_or_default()
            },
            |thumbs| Action::App(Message::ThumbsLoaded(Arc::new(thumbs))),
        );
        let mut app = App {
            core,
            history,
            thumbs: HashMap::new(),
            backend: None,
            backend_error: None,
            surface: None,
            generation: 0,
            locating: None,
            placement: Placement::Center,
            outputs: HashMap::new(),
            query: String::new(),
            selected: 0,
            scroll_y: 0.0,
            search_id: widget::Id::new("search"),
            scroll_id: widget::Id::new("history"),
            drawing: None,
            ipad_panel: false,
            ipad_qr: None,
            pair_code: None,
            qr_hover: false,
            qr_zoom: false,
            code_copied: None,
            offered: None,
            thumb_hover: None,
            preview: None,
            preview_anim: Anim::default(),
            qr_anim: Anim::default(),
            haystack: Vec::new(),
            cursor: Point::ORIGIN,
            offset: Vector::ZERO,
            drag: None,
            popup_at: None,
            popup_focus: (false, false),
        };
        popup::init();
        app.reindex();
        let task = if flags.background { Task::none() } else { app.show() };
        let fonts = Task::batch(FONTS.map(|f| cosmic::iced::font::load(f).map(|_| Action::App(Message::Noop))));
        (app, Task::batch([fonts, load_thumbs, task]))
    }

    fn view(&self) -> Element<'_, Message> {
        widget::space().into()
    }

    fn view_window(&self, id: window::Id) -> Element<'_, Message> {
        if Some(id) != self.surface {
            return widget::space().into();
        }
        // A window of its own: the card fills it, the system moves it.
        if clipboard::popup() {
            return mouse_area(
                container(self.card())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .class(theme::Container::custom(card_style)),
            )
            .on_middle_press(Message::DragStart)
            .into();
        }
        let (placement, offset) = (self.placement, self.offset);
        let content = widget::responsive(move |size| {
            let (x, y, card_w, card_h) = place(placement, offset, size);
            let card = mouse_area(
                container(self.card())
                    .width(Length::Fixed(card_w))
                    .height(Length::Fixed(card_h))
                    .class(theme::Container::custom(card_style)),
            )
            .on_press(Message::Noop)
            .on_right_press(Message::Noop)
            .on_middle_press(Message::DragStart);
            container(card)
                .padding(Padding { top: y, left: x, right: 0.0, bottom: 0.0 })
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        });
        mouse_area(content).on_press(Message::Hide).on_right_press(Message::Hide).on_middle_press(Message::Hide).into()
    }

    fn update(&mut self, message: Message) -> Task<Action<Message>> {
        match message {
            Message::Clipboard(event) => return self.on_clipboard(event),
            Message::Toggle => {
                return if self.surface.is_some() { self.hide() } else { self.show() };
            }
            Message::Hide => {
                tracing::debug!("hide requested");
                return self.hide();
            }
            Message::LocateTimeout(generation) => {
                if self.locating == Some(generation) {
                    tracing::info!("pointer position not reported in time");
                    return self.open_surface(None);
                }
            }
            Message::Opened => return self.focus_search(),
            Message::PopupOpened(id) => {
                let at = self.popup_at;
                tracing::debug!("popup opened at {at:?}");
                return Task::batch([
                    window::run(id, move |w| popup::show(w, at)).discard(),
                    window::gain_focus(id),
                    self.focus_search(),
                ]);
            }
            Message::PopupFocus(focused) => {
                self.popup_focus = (focused, self.popup_focus.1 || focused);
                // Clicking elsewhere closes the popup, like a click outside the
                // overlay. Focus that flickers while the window appears (key
                // grabs, focus handed over) doesn't count.
                if !focused && self.popup_focus.1 {
                    let surface = self.surface;
                    return Task::perform(async { tokio::time::sleep(Duration::from_millis(150)).await }, move |_| {
                        Action::App(Message::FocusLost(surface))
                    });
                }
            }
            Message::FocusLost(surface) if surface == self.surface && !self.popup_focus.0 => {
                tracing::debug!("popup lost focus");
                return self.hide();
            }
            Message::FocusLost(_) => {}
            Message::OutputAdded(name, output) => {
                self.outputs.insert(name, output);
            }
            Message::OutputRemoved(output) => self.outputs.retain(|_, o| *o != output),
            Message::Search(query) => {
                self.query = query;
                self.selected = 0;
                return self.scroll_to_selected();
            }
            Message::Paste(id) => return self.paste(id),
            Message::Delete(id) => {
                self.history.remove(id);
                self.reindex();
                self.thumbs.remove(&id);
                self.clamp_selection();
                return self.focus_search();
            }
            Message::TogglePin(id) => {
                self.history.toggle_pin(id);
                return self.focus_search();
            }
            Message::ClearAll => {
                for id in self.history.clear() {
                    self.thumbs.remove(&id);
                }
                self.reindex();
                self.clamp_selection();
                return self.focus_search();
            }
            Message::Key(key, modifiers) => {
                tracing::trace!("key {key:?}");
                return self.on_key(key, modifiers);
            }
            Message::Scrolled(viewport) => {
                let y = viewport.absolute_offset().y;
                if (y - self.scroll_y).abs() > 0.5 {
                    self.close_preview();
                }
                self.scroll_y = y;
            }
            Message::DrawingReady(server) => {
                if let Err(err) = &server {
                    tracing::warn!("drawing server: {err}");
                }
                self.drawing = Some(server);
            }
            Message::DrawingReceived(png, blank) => {
                // Named after what was edited: the image last sent to the iPad.
                let origin = match (blank, self.offered) {
                    (true, _) => Origin::Drawing,
                    (false, Some(Origin::Screenshot | Origin::EditedScreenshot)) => Origin::EditedScreenshot,
                    (false, Some(Origin::Drawing)) => Origin::Drawing,
                    _ => Origin::Edited,
                };
                return Task::perform(
                    async move { tokio::task::spawn_blocking(move || drawing_received(&png, origin)).await.ok().flatten() },
                    |saved| Action::App(Message::DrawingSaved(saved)),
                );
            }
            Message::DrawingSaved(saved) => {
                let Some((ingested, path)) = saved else {
                    notify("Drawing received, but it could not be saved", "");
                    return Task::none();
                };
                let id = ingested.entry.id;
                let title = image_title(&ingested.entry);
                self.add_ingested(ingested);
                if let Some(entry) = self.history.entries.iter().find(|e| e.id == id) {
                    self.send(entry);
                }
                notify(
                    &format!("{title} received from the iPad"),
                    &format!("Copied to the clipboard and saved to {}", path.display()),
                );
            }
            Message::ToggleIpadPanel => {
                self.ipad_panel = !self.ipad_panel;
                self.qr_zoom = false;
                self.qr_anim = Anim::default();
                self.code_copied = None;
                let server = self.server().filter(|_| self.ipad_panel).cloned();
                let server = server.as_ref();
                self.ipad_qr = server.and_then(|s| qr_code::Data::new(s.url()).ok());
                self.pair_code = server.map(DrawingServer::new_pairing_code);
                return self.focus_search();
            }
            Message::SendToIpad(id) => {
                let png = self
                    .history
                    .entries
                    .iter()
                    .find(|e| e.id == id && e.kind == Kind::Image)
                    .and_then(history::formats)
                    .and_then(|f| f.iter().find(|(m, _)| m == "image/png").map(|(_, d)| d.to_vec()));
                if let (Some(server), Some(png)) = (self.server(), png)
                    && server.offer_png(png)
                {
                    self.offered = self.history.entries.iter().find(|e| e.id == id).map(|e| e.origin);
                    return self.hide();
                }
            }
            // The pointer moves the single highlight, like a command palette.
            Message::Hover(id) => {
                if let Some(i) = self.filtered().iter().position(|e| e.id == id) {
                    self.selected = i;
                }
            }
            Message::DragStart if clipboard::popup() => {
                if let Some(id) = self.surface {
                    return window::drag(id);
                }
            }
            Message::DragStart => self.drag = Some((self.cursor, self.offset)),
            // Between real positions cosmic also reports the pointer at exactly
            // 0,0, and hidden windows send their own positions: ignore both.
            Message::CursorMoved(id, p) if Some(id) != self.surface || p == Point::ORIGIN => {}
            Message::CursorMoved(_, p) => {
                self.cursor = p;
                // The enlarged image folds away as soon as the pointer moves a bit
                // and reopens only after the pointer enters a thumbnail again.
                if self.preview_anim.to == 1.0 && self.preview.as_ref().is_some_and(|(_, at, ..)| p.distance(*at) > 4.0)
                {
                    self.close_preview();
                    self.thumb_hover = None;
                }
                if let Some((id, at, generation)) = self.thumb_hover
                    && p.distance(at) > 2.0
                {
                    self.thumb_hover = Some((id, p, generation + 1));
                    return preview_after_rest(id, generation + 1);
                }
                if let Some((start, offset)) = self.drag {
                    self.offset = offset + (p - start);
                }
            }
            Message::DragEnd => self.drag = None,
            Message::ThumbsLoaded(thumbs) => {
                let thumbs = Arc::try_unwrap(thumbs).unwrap_or_else(|a| (*a).clone());
                self.thumbs.extend(thumbs.into_iter().map(|(id, t)| (id, thumb_handle(t))));
            }
            Message::QrHover(on) => self.qr_hover = on,
            Message::ThumbEnter(id) => {
                self.thumb_hover = Some((id, self.cursor, 0));
                return preview_after_rest(id, 0);
            }
            Message::ThumbLeave(id) => {
                if self.thumb_hover.is_some_and(|(h, ..)| h == id) {
                    self.thumb_hover = None;
                }
            }
            Message::PreviewOpen(id, generation) => {
                let resting = self.thumb_hover.is_some_and(|(h, _, g)| h == id && g == generation);
                let open = self.preview_anim.to == 1.0 && self.preview.is_some();
                if resting && self.drag.is_none() && !open {
                    let size = |w: u32, h: u32| (w.max(1) as f32, h.max(1) as f32);
                    let visual = self.history.entries.iter().find(|e| e.id == id).and_then(|e| match e.kind {
                        Kind::Image => history::formats(e)
                            .and_then(|f| f.iter().find(|(m, _)| m.starts_with("image/")).map(|(_, d)| d.to_vec()))
                            .map(|png| {
                                let dims = e.image_size.map_or((4.0, 3.0), |(w, h)| size(w, h));
                                (Visual::Raster(image::Handle::from_bytes(png)), dims)
                            }),
                        // SVGs scale freely and unknown sizes get a large box;
                        // `Contain` keeps the aspect ratio inside it.
                        Kind::Files => file_visual(e).map(|(path, v)| {
                            let dims = match v {
                                Visual::Svg(_) => (1000.0, 1000.0),
                                Visual::Raster(_) => {
                                    ::image::image_dimensions(&path).map_or((1000.0, 750.0), |(w, h)| size(w, h))
                                }
                            };
                            (v, dims)
                        }),
                        Kind::Text => None,
                    });
                    if let Some((visual, dims)) = visual {
                        if self.preview.as_ref().is_none_or(|(p, ..)| *p != id) {
                            self.preview_anim = Anim::default();
                        }
                        self.preview = Some((id, self.cursor, visual, dims));
                        self.preview_anim.go(1.0);
                    }
                }
            }
            Message::QrZoom(on) => {
                if on {
                    self.qr_zoom = true;
                }
                self.qr_anim.go(if on { 1.0 } else { 0.0 });
                self.qr_hover = false;
            }
            // Finished closing animations drop what they showed.
            Message::Frame => {
                if self.preview_anim.to == 0.0 && self.preview_anim.done() {
                    self.preview = None;
                }
                if self.qr_anim.to == 0.0 && self.qr_anim.done() {
                    self.qr_zoom = false;
                }
            }
            Message::CopyPairCode => {
                if let (Some(code), Some(backend)) = (&self.pair_code, &self.backend) {
                    let data = Arc::new(code.clone().into_bytes());
                    let formats = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"]
                        .into_iter()
                        .map(|m| (m.to_string(), data.clone()))
                        .collect();
                    // Our own selection never ends up in the history.
                    let _ = backend.send(Command::Set { formats: Arc::new(formats) });
                    self.code_copied = Some(std::time::Instant::now());
                }
            }
            Message::ForgetDevice(id) => {
                if let Some(server) = self.server() {
                    server.forget_device(&id);
                }
            }
            Message::Noop => {}
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        let backend = Subscription::batch([Subscription::run(backend_stream), Subscription::run(drawing_stream)]);
        #[cfg(target_os = "linux")]
        let outputs = event::listen_with(|event, _, _| match event {
            Event::PlatformSpecific(event::PlatformSpecific::Wayland(wayland::Event::Output(event, output))) => {
                match event {
                    wayland::OutputEvent::Created(Some(info)) | wayland::OutputEvent::InfoUpdate(info) => {
                        info.name.map(|name| Message::OutputAdded(name, output))
                    }
                    wayland::OutputEvent::Removed => Some(Message::OutputRemoved(output)),
                    wayland::OutputEvent::Created(None) => None,
                }
            }
            _ => None,
        });
        // Later starts of the app ask the running one to toggle.
        #[cfg(not(target_os = "linux"))]
        let outputs = Subscription::run(instance_stream);
        if self.surface.is_none() {
            return Subscription::batch([backend, outputs]);
        }
        let keys = event::listen_with(|event, _status, _id| match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => Some(Message::Key(key, modifiers)),
            Event::Mouse(mouse::Event::CursorMoved { position }) => Some(Message::CursorMoved(_id, position)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left | mouse::Button::Middle)) => {
                Some(Message::DragEnd)
            }
            #[cfg(target_os = "linux")]
            Event::PlatformSpecific(event::PlatformSpecific::Wayland(wayland::Event::Layer(
                wayland::LayerEvent::Focused,
                ..,
            ))) => Some(Message::Opened),
            Event::Window(window::Event::Opened { .. }) => Some(Message::Opened),
            Event::Window(window::Event::Focused) if clipboard::popup() => Some(Message::PopupFocus(true)),
            Event::Window(window::Event::Unfocused) if clipboard::popup() => Some(Message::PopupFocus(false)),
            _ => None,
        });
        let animating = !self.preview_anim.done() || !self.qr_anim.done();
        let frames = if animating {
            cosmic::iced::time::every(Duration::from_millis(8)).map(|_| Message::Frame)
        } else {
            Subscription::none()
        };
        let tick = if self.ipad_panel {
            cosmic::iced::time::every(Duration::from_secs(1)).map(|_| Message::Noop)
        } else {
            Subscription::none()
        };
        Subscription::batch([backend, outputs, keys, tick, frames])
    }

    #[cfg(target_os = "linux")]
    fn dbus_activation(&mut self, msg: cosmic::dbus_activation::Message) -> Task<Action<Message>> {
        match msg.msg {
            cosmic::dbus_activation::Details::ActivateAction { action, .. } if action == BACKGROUND_ACTION => {
                Task::none()
            }
            _ => self.update(Message::Toggle),
        }
    }

    fn on_escape(&mut self) -> Task<Action<Message>> {
        self.hide()
    }
}

impl App {
    /// Asks the backend where the pointer is; the overlay is opened once the
    /// answer arrives (or after a short timeout).
    fn show(&mut self) -> Task<Action<Message>> {
        if self.surface.is_some() || self.locating.is_some() {
            return Task::none();
        }
        self.generation += 1;
        let generation = self.generation;
        self.locating = Some(generation);
        if let Some(backend) = &self.backend {
            let _ = backend.send(Command::LocateCursor);
        }
        Task::perform(async { tokio::time::sleep(Duration::from_millis(300)).await }, move |_| {
            Action::App(Message::LocateTimeout(generation))
        })
    }

    fn open_surface(&mut self, cursor: Option<CursorPos>) -> Task<Action<Message>> {
        self.locating = None;
        if self.surface.is_some() {
            return Task::none();
        }
        self.query.clear();
        self.selected = 0;
        self.scroll_y = 0.0;
        self.offset = Vector::ZERO;
        self.drag = None;
        let reset_scroll = cosmic::iced::widget::scrollable::scroll_to(
            self.scroll_id.clone(),
            AbsoluteOffset { x: None, y: Some(0.0) },
        );
        if clipboard::popup() {
            let (id, opened) = window::open(self.popup_settings(cursor));
            self.surface = Some(id);
            self.popup_focus = (false, false);
            return Task::batch([opened.map(|id| Action::App(Message::PopupOpened(id))), reset_scroll]);
        }
        #[cfg(target_os = "linux")]
        return Task::batch([
            self.open_overlay(cursor),
            widget::text_input::focus(self.search_id.clone()),
            reset_scroll,
        ]);
        #[cfg(not(target_os = "linux"))]
        Task::none()
    }

    /// The popup window: card sized, next to the pointer (or caret), flipped
    /// like a context menu where it wouldn't fit on the monitor.
    fn popup_settings(&mut self, cursor: Option<CursorPos>) -> window::Settings {
        let size = Size::new(CARD_W, CARD_H);
        let spot = cursor.and_then(|c| c.screen.map(|s| (c, s)));
        self.popup_at = None;
        let position = match spot {
            Some((c, s)) => {
                // Placement works in logical pixels relative to the monitor.
                let k = s.scale.max(0.25);
                let area = Size::new(s.width / k, s.height / k);
                let at = Point::new((c.x - s.x) / k, (c.y - s.y) / k);
                let above = c.above.map(|a| (a - s.y) / k);
                let (x, y, _, _) = place(Placement::At(at, above), Vector::ZERO, area);
                let (px, py) = (s.x + x * k, s.y + y * k);
                self.popup_at = Some((px.round() as i32, py.round() as i32));
                window::Position::Specific(Point::new(px / k, py / k))
            }
            None => window::Position::Centered,
        };
        #[allow(unused_mut)]
        let mut platform_specific = window::settings::PlatformSpecific::default();
        #[cfg(target_os = "windows")]
        {
            platform_specific.skip_taskbar = true;
            platform_specific.undecorated_shadow = true;
            platform_specific.drag_and_drop = false;
            platform_specific.corner_preference = window::settings::platform::CornerPreference::Round;
        }
        #[cfg(target_os = "linux")]
        {
            platform_specific.application_id = APP_ID.to_string();
        }
        window::Settings {
            size,
            position,
            resizable: false,
            decorations: false,
            // The software renderer can't draw into translucent X11 windows;
            // rounded corners come from the system (Windows 11, macOS).
            transparent: false,
            level: window::Level::AlwaysOnTop,
            exit_on_close_request: false,
            platform_specific,
            ..Default::default()
        }
    }

    #[cfg(target_os = "linux")]
    fn open_overlay(&mut self, cursor: Option<CursorPos>) -> Task<Action<Message>> {
        let output = cursor
            .as_ref()
            .and_then(|c| c.output.as_ref())
            .and_then(|name| self.outputs.get(name))
            .cloned()
            .map_or(IcedOutput::Active, IcedOutput::Output);
        self.placement = cursor.map_or(Placement::Center, |c| Placement::At(Point::new(c.x, c.y), c.above));
        let id = window::Id::unique();
        self.surface = Some(id);
        get_layer_surface(SctkLayerSurfaceSettings {
            id,
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            input_zone: None,
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            output,
            namespace: "cosmic-clipboard".into(),
            size: Some((None, None)),
            exclusive_zone: -1,
            size_limits: Limits::NONE.min_width(1.0).min_height(1.0),
            ..Default::default()
        })
    }

    fn close_preview(&mut self) {
        if self.preview.is_some() {
            self.preview_anim.go(0.0);
        }
    }

    fn add_ingested(&mut self, ingested: Arc<history::Ingested>) {
        let mut ingested = Arc::try_unwrap(ingested)
            .unwrap_or_else(|arc| history::Ingested { entry: arc.entry.clone(), thumb: arc.thumb.clone() });
        let e = &mut ingested.entry;
        if e.kind == Kind::Image && e.origin == Origin::Other && !e.from_ipad && screenshot_just_taken() {
            e.origin = Origin::Screenshot;
        }
        if let Some(thumb) = ingested.thumb {
            self.thumbs.insert(ingested.entry.id, thumb_handle(thumb));
        }
        for id in self.history.add(ingested.entry) {
            self.thumbs.remove(&id);
        }
        self.reindex();
        self.clamp_selection();
    }

    fn server(&self) -> Option<&DrawingServer> {
        self.drawing.as_ref().and_then(|d| d.as_ref().ok())
    }

    fn focus_search(&self) -> Task<Action<Message>> {
        widget::text_input::focus(self.search_id.clone())
    }

    fn hide(&mut self) -> Task<Action<Message>> {
        // Next time the history opens again, not the devices page.
        self.ipad_panel = false;
        self.qr_zoom = false;
        self.qr_anim = Anim::default();
        self.preview = None;
        self.preview_anim = Anim::default();
        self.thumb_hover = None;
        let Some(id) = self.surface.take() else { return Task::none() };
        if clipboard::popup() {
            popup::hidden();
            return window::close(id);
        }
        #[cfg(target_os = "linux")]
        return destroy_layer_surface(id);
        #[cfg(not(target_os = "linux"))]
        Task::none()
    }

    fn on_clipboard(&mut self, event: clipboard::Event) -> Task<Action<Message>> {
        match event {
            clipboard::Event::Ready(sender) => {
                if self.locating.is_some() {
                    let _ = sender.send(Command::LocateCursor);
                }
                self.backend = Some(sender);
                self.backend_error = None;
            }
            clipboard::Event::Failed(err) => {
                self.backend_error = Some(err);
                if self.locating.is_some() {
                    return self.open_surface(None);
                }
            }
            clipboard::Event::Cursor(cursor) => {
                if self.locating.is_some() {
                    return self.open_surface(cursor);
                }
            }
            clipboard::Event::Captured(ingested) => self.add_ingested(ingested),
            clipboard::Event::Toggle => return self.update(Message::Toggle),
            clipboard::Event::Cleared => {
                if let Some(entry) = self.history.entries.first() {
                    self.send(entry);
                }
            }
        }
        Task::none()
    }

    fn send(&self, entry: &Entry) -> bool {
        let (Some(backend), Some(formats)) = (&self.backend, history::formats(entry)) else {
            return false;
        };
        backend.send(Command::Set { formats })
    }

    /// Owns the clipboard with the entry, closes the overlay and lets the
    /// backend type the paste shortcut once focus is back on the target.
    fn paste(&mut self, id: u64) -> Task<Action<Message>> {
        if let Some(entry) = self.history.entries.iter().find(|e| e.id == id)
            && self.send(entry)
        {
            if let Some(backend) = &self.backend {
                let _ = backend.send(Command::Paste);
            }
            self.history.bump(id);
            self.reindex();
        }
        self.hide()
    }

    /// Rebuilds the lowercased search text; call after every history change.
    fn reindex(&mut self) {
        self.haystack = self
            .history
            .entries
            .iter()
            .map(|e| match e.kind {
                Kind::Image => format!("image picture {}", image_title(e)).to_lowercase(),
                _ => e.preview.to_lowercase(),
            })
            .collect();
    }

    /// Visible entries in display order: pinned first, then recent.
    fn filtered(&self) -> Vec<&Entry> {
        let query = self.query.trim().to_lowercase();
        let mut out: Vec<&Entry> = self
            .history
            .entries
            .iter()
            .zip(&self.haystack)
            .filter(|(_, h)| query.is_empty() || h.contains(&query))
            .map(|(e, _)| e)
            .collect();
        out.sort_by_key(|e| !e.pinned);
        out
    }

    fn clamp_selection(&mut self) {
        let len = self.filtered().len();
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    fn on_key(&mut self, key: Key, modifiers: keyboard::Modifiers) -> Task<Action<Message>> {
        self.close_preview();
        // Ctrl shortcuts; on macOS Cmd works as well.
        let modifiers = if modifiers.logo() && cfg!(target_os = "macos") {
            modifiers | keyboard::Modifiers::CTRL
        } else {
            modifiers
        };
        let selected_id = self.filtered().get(self.selected).map(|e| e.id);
        match key {
            Key::Named(Named::Escape) if self.qr_zoom => return self.update(Message::QrZoom(false)),
            Key::Named(Named::Escape) => return self.hide(),
            Key::Named(Named::ArrowDown) => {
                let len = self.filtered().len();
                if len > 0 {
                    self.selected = (self.selected + 1) % len;
                }
                return self.scroll_to_selected();
            }
            Key::Named(Named::ArrowUp) => {
                let len = self.filtered().len();
                if len > 0 {
                    self.selected = (self.selected + len - 1) % len;
                }
                return self.scroll_to_selected();
            }
            Key::Named(Named::PageDown) | Key::Named(Named::End)
                if modifiers.is_empty() || matches!(key, Key::Named(Named::PageDown)) =>
            {
                let len = self.filtered().len();
                self.selected = if matches!(key, Key::Named(Named::End)) {
                    len.saturating_sub(1)
                } else {
                    (self.selected + 4).min(len.saturating_sub(1))
                };
                return self.scroll_to_selected();
            }
            Key::Named(Named::PageUp) => {
                self.selected = self.selected.saturating_sub(4);
                return self.scroll_to_selected();
            }
            Key::Named(Named::Enter) => {
                if let Some(id) = selected_id {
                    return self.paste(id);
                }
            }
            Key::Named(Named::Delete) if self.query.is_empty() || modifiers.shift() => {
                if let Some(id) = selected_id {
                    return self.update(Message::Delete(id));
                }
            }
            Key::Character(c) if modifiers.control() && c.as_str().eq_ignore_ascii_case("e") => {
                if let Some(id) = selected_id {
                    return self.update(Message::SendToIpad(id));
                }
            }
            Key::Character(c) if modifiers.control() && c.as_str().eq_ignore_ascii_case("i") => {
                return self.update(Message::ToggleIpadPanel);
            }
            Key::Character(c) if modifiers.control() && c.as_str().eq_ignore_ascii_case("p") => {
                if let Some(id) = selected_id {
                    self.history.toggle_pin(id);
                }
            }
            _ => {}
        }
        Task::none()
    }

    /// Group label shown before entry `i` of the display list, if any.
    fn group_label(entries: &[&Entry], i: usize) -> Option<&'static str> {
        let has_pinned = entries.first().is_some_and(|e| e.pinned);
        match (i, entries[i].pinned) {
            (0, true) => Some("Pinned"),
            (0, false) if has_pinned => Some("Recent"),
            (i, false) if has_pinned && entries[i - 1].pinned => Some("Recent"),
            _ => None,
        }
    }

    fn scroll_to_selected(&self) -> Task<Action<Message>> {
        let entries = self.filtered();
        if self.selected >= entries.len() {
            return Task::none();
        }
        let label = |i: usize| if Self::group_label(&entries, i).is_some() { GROUP_H } else { 0.0 };
        let top: f32 =
            8.0 + (0..self.selected).map(|i| label(i) + ROW_H + ITEM_SPACING).sum::<f32>() + label(self.selected);
        let bottom = top + ROW_H;
        let viewport = CARD_H - CHROME_H;
        let target = if top < self.scroll_y {
            top
        } else if bottom > self.scroll_y + viewport {
            bottom - viewport
        } else {
            return Task::none();
        };
        cosmic::iced::widget::scrollable::scroll_to(
            self.scroll_id.clone(),
            AbsoluteOffset { x: None, y: Some(target.max(0.0)) },
        )
    }

    fn card(&self) -> Element<'_, Message> {
        let ghost = |name: &'static str, label: &'static str, msg: Message, on: bool| {
            tip(button::icon(lucide(name)).class(ghost_style(on)).padding(6).on_press(msg), label)
        };
        // Footer: keyboard hints and actions; it doubles as drag handle.
        let hints: Element<'_, Message> = if self.ipad_panel {
            row![kbd("Esc"), hint("close")].spacing(6).align_y(Alignment::Center).into()
        } else {
            let image_selected =
                self.filtered().get(self.selected).is_some_and(|e| e.kind == Kind::Image && self.server().is_some());
            row![
                kbd("↑↓"),
                hint("navigate"),
                kbd("↵"),
                hint("paste"),
                kbd("Del"),
                hint("delete"),
                kbd("Ctrl P"),
                hint("pin")
            ]
            .push_maybe(image_selected.then(|| kbd("Ctrl E")))
            .push_maybe(image_selected.then(|| hint("iPad")))
            .spacing(6)
            .align_y(Alignment::Center)
            .into()
        };
        let can_clear = !self.ipad_panel && self.history.entries.iter().any(|e| !e.pinned);
        let footer = mouse_area(
            container(
                row![hints, widget::space().width(Length::Fill)]
                    .push_maybe(can_clear.then(|| ghost("list-x", "Clear unpinned", Message::ClearAll, false)))
                    .push(ghost(
                        "tablet-smartphone",
                        "Draw on iPad (Ctrl+I)",
                        Message::ToggleIpadPanel,
                        self.ipad_panel,
                    ))
                    .spacing(2)
                    .align_y(Alignment::Center),
            )
            .padding([0, 6, 0, 12])
            .height(Length::Fixed(36.0))
            .align_y(Alignment::Center)
            .class(theme::Container::custom(footer_style)),
        )
        .on_press(Message::DragStart)
        .interaction(if self.drag.is_some() { mouse::Interaction::Grabbing } else { mouse::Interaction::Grab });

        let body: Element<'_, Message> = if self.ipad_panel {
            container(self.ipad_view()).padding([4, 8]).height(Length::Fill).into()
        } else {
            let mut search = widget::text_input("Search clipboard…", &self.query)
                .leading_icon(icon::icon(lucide("search")).size(16).into())
                .on_input(Message::Search)
                .id(self.search_id.clone())
                .style(search_style())
                .size(14)
                .padding([12, 12])
                .width(Length::Fill);
            if !self.query.is_empty() {
                search = search.trailing_icon(
                    button::icon(lucide("x"))
                        .class(ghost_style(false))
                        .padding(2)
                        .on_press(Message::Search(String::new()))
                        .into(),
                );
            }
            let entries = self.filtered();
            let list: Element<'_, Message> = if let Some(err) = &self.backend_error {
                placeholder("file-text", "Clipboard unavailable", err)
            } else if self.history.entries.is_empty() {
                placeholder("file-text", "Nothing copied yet", "Copied text, images and files show up here.")
            } else if entries.is_empty() {
                placeholder("search", "No results", "")
            } else {
                let mut col = column![].spacing(ITEM_SPACING).padding([8, 8]);
                for (i, e) in entries.iter().enumerate() {
                    if let Some(label) = Self::group_label(&entries, i) {
                        col = col.push(
                            container(txt(label).size(11).class(pal::MUTED))
                                .padding([8, 8, 4, 8])
                                .height(Length::Fixed(GROUP_H)),
                        );
                    }
                    col = col.push(self.item(e, i == self.selected));
                }
                scrollable(col)
                    .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
                        cosmic::iced::widget::scrollable::Scrollbar::new().width(4).scroller_width(4).margin(2),
                    ))
                    .id(self.scroll_id.clone())
                    .on_scroll(Message::Scrolled)
                    .height(Length::Fill)
                    .into()
            };
            column![container(search).padding([0, 4]).height(Length::Fixed(44.0)), divider(), list].into()
        };

        // Resting on a thumbnail shows the image large over the list. Always a
        // stack (empty top layer without preview): swapping the widget type would
        // reset the list's scroll state, whose "scrolled" message then closed the
        // preview right away.
        let layer: Element<'_, Message> = match &self.preview {
            Some((_, _, visual, (w, h))) => {
                // Grows from thumbnail size to fill the list area.
                let v = self.preview_anim.value();
                let (w, h) = (*w, *h);
                let fit = |bw: f32, bh: f32, up: f32| {
                    let k = (bw / w).min(bh / h).min(up);
                    (w * k, h * k)
                };
                let (tw, th) = fit(CARD_W - 20.0, CARD_H - 36.0 - 20.0, 2.0);
                let (sw, sh) = fit(THUMB_W, THUMB_H, f32::INFINITY);
                let lerp = |a: f32, b: f32| a + (b - a) * v;
                let (pw, ph) = (Length::Fixed(lerp(sw, tw)), Length::Fixed(lerp(sh, th)));
                let alpha = v.clamp(0.0, 1.0);
                let pic: Element<'_, Message> = match visual {
                    Visual::Raster(handle) => image(handle.clone())
                        .content_fit(iced::ContentFit::Contain)
                        .border_radius(6.0)
                        .opacity(alpha)
                        .width(pw)
                        .height(ph)
                        .into(),
                    Visual::Svg(handle) => cosmic::iced::widget::svg(handle.clone())
                        .content_fit(iced::ContentFit::Contain)
                        .opacity(alpha)
                        .width(pw)
                        .height(ph)
                        .into(),
                };
                container(container(pic).center(Length::Fill))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .class(theme::Container::custom(move |_| cosmic::iced::widget::container::Style {
                        background: Some(Background::Color(Color { a: 0.97 * v.clamp(0.0, 1.0), ..pal::CARD })),
                        border: Border { radius: [pal::R_CARD, pal::R_CARD, 0.0, 0.0].into(), ..Default::default() },
                        ..Default::default()
                    }))
                    .into()
            }
            None => widget::space().into(),
        };
        let body: Element<'_, Message> = cosmic::iced::widget::stack(vec![body, layer]).into();
        column![body, footer].into()
    }

    fn ipad_view(&self) -> Element<'_, Message> {
        let server = match &self.drawing {
            None => return placeholder("tablet-smartphone", "Starting …", ""),
            Some(Err(err)) => return placeholder("file-text", "iPad server unavailable", err),
            Some(Ok(server)) => server,
        };
        let white = |_: &cosmic::Theme| cosmic::iced::widget::container::Style {
            background: Some(Background::Color(Color::WHITE)),
            border: Border { radius: pal::R_BUTTON.into(), ..Default::default() },
            ..Default::default()
        };
        let zoom: Option<Element<'_, Message>> = match (self.qr_zoom, &self.ipad_qr) {
            (true, Some(data)) => {
                // Maximized: the code grows over the panel, a click anywhere closes it.
                let v = self.qr_anim.value().clamp(0.0, 1.0);
                let small = 90.0;
                let big = container(qr_code(data).total_size(small + (300.0 - small) * v))
                    .padding(5.0 + 3.0 * v)
                    .class(theme::Container::custom(white));
                Some(
                    mouse_area(
                        container(
                            column![
                                big,
                                row![
                                    icon::icon(lucide("minimize-2")).size(14).class(theme::Svg::Custom(
                                        std::rc::Rc::new(|_| {
                                            cosmic::iced::widget::svg::Style { color: Some(pal::MUTED) }
                                        })
                                    )),
                                    txt(server.url()).size(12).class(pal::MUTED),
                                ]
                                .spacing(8)
                                .align_y(Alignment::Center),
                            ]
                            .spacing(12)
                            .align_x(Alignment::Center),
                        )
                        .center(Length::Fill)
                        .class(theme::Container::custom(move |_| {
                            cosmic::iced::widget::container::Style {
                                background: Some(Background::Color(Color { a: 0.98 * v, ..pal::CARD })),
                                ..Default::default()
                            }
                        })),
                    )
                    .on_press(Message::QrZoom(false))
                    .interaction(mouse::Interaction::Pointer)
                    .into(),
                )
            }
            _ => None,
        };
        let qr: Element<'_, Message> = match &self.ipad_qr {
            Some(data) => {
                let code = container(qr_code(data).cell_size(3)).padding(5).class(theme::Container::custom(white));
                let mut layers: Vec<Element<'_, Message>> = vec![code.into()];
                if self.qr_hover {
                    // Enlarge button in the corner while hovered.
                    let enlarge = button::icon(lucide("maximize-2"))
                        .padding(5)
                        .class(button_style(|hovered, _, _| {
                            flat_style(
                                Some(Color::from_rgba8(0x0e, 0x0e, 0x0e, if hovered { 0.95 } else { 0.8 })),
                                [pal::R_BUTTON; 4],
                                pal::TEXT,
                                (0.0, Color::TRANSPARENT),
                            )
                        }))
                        .on_press(Message::QrZoom(true));
                    layers.push(
                        container(tip(enlarge, "Enlarge"))
                            .padding(4)
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .align_x(Alignment::End)
                            .align_y(Alignment::End)
                            .into(),
                    );
                }
                mouse_area(cosmic::iced::widget::stack(layers))
                    .on_enter(Message::QrHover(true))
                    .on_exit(Message::QrHover(false))
                    .on_press(Message::QrZoom(true))
                    .interaction(mouse::Interaction::Pointer)
                    .into()
            }
            None => widget::space().into(),
        };
        let code: String = self.pair_code.as_deref().unwrap_or_default().chars().filter(char::is_ascii_digit).collect();
        let digits = row(code.chars().map(|c| {
            container(txt(c.to_string()).size(14).font(INTER_MED).class(pal::TEXT))
                .center_x(Length::Fixed(22.0))
                .center_y(Length::Fixed(28.0))
                .class(theme::Container::custom(|_| cosmic::iced::widget::container::Style {
                    text_color: Some(pal::TEXT),
                    background: Some(Background::Color(pal::ACCENT)),
                    border: Border { radius: pal::R_BUTTON.into(), width: 1.0, color: pal::BORDER },
                    ..Default::default()
                }))
                .into()
        }))
        .spacing(3);
        let copied = self.code_copied.is_some_and(|t| t.elapsed() < Duration::from_millis(1500));
        let copy = tip(
            button::icon(lucide(if copied { "check" } else { "copy" }))
                .class(ghost_style(copied))
                .padding(6)
                .on_press(Message::CopyPairCode),
            if copied { "Copied" } else { "Copy code" },
        );
        let digits = row![digits, copy].spacing(6).align_y(Alignment::Center);
        let pair = row![
            qr,
            column![
                txt("Pair a new device").size(13).font(INTER_MED).class(pal::TEXT),
                txt("Scan the code or open the address in Safari, then enter the code.").size(12).class(pal::MUTED),
                txt(server.url()).size(12).font(INTER_MED).class(pal::TEXT),
                widget::space().height(Length::Fixed(2.0)),
                digits,
            ]
            .spacing(3)
            .width(Length::Fill),
        ]
        .spacing(14)
        .align_y(Alignment::Center);

        let devices = server.devices();
        let now = history::now();
        let list: Element<'_, Message> = if devices.is_empty() {
            container(txt("No devices paired yet.").size(12).class(pal::MUTED)).padding([10, 8]).into()
        } else {
            let mut col = column![].spacing(ITEM_SPACING).padding([0, 0, 4, 0]);
            for d in devices {
                let icon_name = match d.kind.as_str() {
                    "iPad" => "tablet",
                    "iPhone" | "Android" => "smartphone",
                    _ => "monitor",
                };
                let mut title = row![txt(d.kind.clone()).size(13).font(INTER_MED).class(pal::TEXT)]
                    .spacing(6)
                    .align_y(Alignment::End);
                if !d.detail.is_empty() {
                    title = title.push(txt(d.detail.clone()).size(12).class(pal::MUTED));
                }
                let seen: Element<'_, Message> = if d.online {
                    row![dot(Color::from_rgb8(0x4a, 0xde, 0x80)), txt("Connected").size(12).class(pal::MUTED)]
                        .spacing(5)
                        .align_y(Alignment::Center)
                        .into()
                } else if d.last_seen > 0 {
                    txt(format!("Last seen {}", ago_long(now.saturating_sub(d.last_seen))))
                        .size(12)
                        .class(pal::MUTED)
                        .into()
                } else {
                    txt("Not seen since pairing").size(12).class(pal::MUTED).into()
                };
                let info = row![seen, txt(format!("Paired {}", date(d.paired))).size(12).class(pal::ICON)]
                    .spacing(10)
                    .align_y(Alignment::Center);
                let remove = tip(
                    button::icon(lucide("trash"))
                        .class(danger_style())
                        .padding(5)
                        .on_press(Message::ForgetDevice(d.id.clone())),
                    "Remove",
                );
                col = col.push(
                    container(
                        row![
                            icon::icon(lucide(icon_name)).size(18).class(theme::Svg::Custom(std::rc::Rc::new(|_| {
                                cosmic::iced::widget::svg::Style { color: Some(pal::ICON) }
                            }))),
                            column![title, info].spacing(3).width(Length::Fill),
                            remove,
                        ]
                        .spacing(12)
                        .align_y(Alignment::Center),
                    )
                    .padding([8, 8])
                    .width(Length::Fill),
                );
            }
            scrollable(col)
                .direction(cosmic::iced::widget::scrollable::Direction::Vertical(
                    cosmic::iced::widget::scrollable::Scrollbar::new().width(4).scroller_width(4).margin(2),
                ))
                .height(Length::Fill)
                .into()
        };
        let section = |label: &'static str| container(txt(label).size(11).class(pal::MUTED)).padding([0, 8]);
        let panel: Element<'_, Message> = column![
            container(pair).padding([12, 8, 12, 8]),
            divider(),
            container(section("Paired devices")).padding([10, 0, 2, 0]),
            list,
        ]
        .into();
        let top = zoom.unwrap_or_else(|| widget::space().into());
        cosmic::iced::widget::stack(vec![panel, top]).into()
    }

    fn item<'a>(&'a self, entry: &'a Entry, selected: bool) -> Element<'a, Message> {
        let main: Element<'a, Message> = match entry.kind {
            Kind::Text => {
                let line: String = entry.preview.split_whitespace().collect::<Vec<_>>().join(" ");
                row![
                    icon_tile("text"),
                    txt(line).size(13).ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1))).width(Length::Fill),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into()
            }
            Kind::Image => {
                let thumb: Element<'a, Message> = match self.thumbs.get(&entry.id) {
                    Some(handle) => thumb(entry.id, Visual::Raster(handle.clone())),
                    None => icon_tile("image"),
                };
                let size = entry.image_size.map(|(w, h)| format!("{w} × {h}")).unwrap_or_default();
                row![
                    container(thumb)
                        .width(Length::Fixed(THUMB_W))
                        .height(Length::Fixed(THUMB_H))
                        .align_x(Alignment::Start),
                    column![txt(image_title(entry)).size(13), txt(size).size(11).class(pal::MUTED),]
                        .spacing(2)
                        .width(Length::Fill),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into()
            }
            Kind::Files => {
                let paths: Vec<&str> = entry.preview.lines().collect();
                let names: Vec<String> = paths
                    .iter()
                    .take(3)
                    .map(|p| {
                        std::path::Path::new(p)
                            .file_name()
                            .map_or_else(|| p.to_string(), |n| n.to_string_lossy().into_owned())
                    })
                    .collect();
                let mut label = names.join(", ");
                if paths.len() > 3 {
                    label.push_str(&format!(" +{}", paths.len() - 3));
                }
                let dir = paths
                    .first()
                    .and_then(|p| std::path::Path::new(p).parent())
                    .map(|d| d.display().to_string())
                    .unwrap_or_default();
                // Images and SVGs show (and enlarge) themselves; the rest gets an icon.
                let lead = match file_visual(entry) {
                    Some((_, visual)) => thumb(entry.id, visual),
                    None if paths.len() > 1 => icon_tile("files"),
                    None if paths.first().is_some_and(|p| std::path::Path::new(p).is_dir()) => icon_tile("folder"),
                    None => icon_tile("file"),
                };
                row![
                    lead,
                    column![
                        txt(label).size(13).ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1))),
                        txt(dir)
                            .size(11)
                            .class(pal::MUTED)
                            .ellipsize(Ellipsize::Middle(EllipsizeHeightLimit::Lines(1))),
                    ]
                    .width(Length::Fill),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into()
            }
        };

        // Right side: time, or the actions while hovered/selected.
        let trailing: Element<'a, Message> = if selected {
            let action = |name: &'static str, label: &'static str, msg: Message, on: bool| {
                tip(button::icon(lucide(name)).class(ghost_style(on)).padding(4).on_press(msg), label)
            };
            row![]
                .push_maybe(
                    (entry.kind == Kind::Image && self.server().is_some())
                        .then(|| action("pen-line", "Edit on iPad (Ctrl+E)", Message::SendToIpad(entry.id), false)),
                )
                .push(action(
                    "pin",
                    if entry.pinned { "Unpin (Ctrl+P)" } else { "Pin (Ctrl+P)" },
                    Message::TogglePin(entry.id),
                    entry.pinned,
                ))
                .push(tip(
                    button::icon(lucide("trash")).class(danger_style()).padding(4).on_press(Message::Delete(entry.id)),
                    "Delete (Del)",
                ))
                .spacing(0)
                .into()
        } else {
            txt(ago(entry.created)).size(11).class(pal::MUTED).into()
        };

        // A container, not a button: libcosmic's button offsets the tooltips
        // of its children by its own position.
        let row = container(row![main, trailing].spacing(8).height(Length::Fill).align_y(Alignment::Center))
            .padding([4, 8])
            .width(Length::Fill)
            .height(Length::Fixed(ROW_H))
            .class(theme::Container::custom(move |_| cosmic::iced::widget::container::Style {
                text_color: Some(pal::TEXT),
                icon_color: Some(pal::TEXT),
                background: selected.then_some(Background::Color(pal::ACCENT)),
                border: Border { radius: pal::R_ROW.into(), ..Default::default() },
                ..Default::default()
            }));
        mouse_area(row)
            .on_press(Message::Paste(entry.id))
            .interaction(mouse::Interaction::Pointer)
            .on_enter(Message::Hover(entry.id))
            .into()
    }
}

/// Tooltip: a small dark pill, always centred above the element.
fn tip<'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl cosmic::iced::widget::text::IntoFragment<'a>,
) -> Element<'a, Message> {
    let pill = container(txt(label).size(12).class(pal::TEXT)).padding([4, 8]).class(theme::Container::custom(|_| {
        cosmic::iced::widget::container::Style {
            text_color: Some(pal::TEXT),
            background: Some(Background::Color(rgb8(0x17, 0x17, 0x17))),
            border: Border { radius: pal::R_BUTTON.into(), width: 1.0, color: pal::BORDER },
            shadow: Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.35),
                offset: Vector::new(0.0, 2.0),
                blur_radius: 8.0,
            },
            ..Default::default()
        }
    }));
    cosmic::iced::widget::Tooltip::new(content, pill, widget::tooltip::Position::Top)
        .gap(6.0)
        .padding(0.0)
        .class(theme::Container::custom(|_| cosmic::iced::widget::container::Style::default()))
        .into()
}

fn rgb8(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}

/// Thumbnail in a list row; resting on it opens the large preview.
fn thumb<'a>(id: u64, visual: Visual) -> Element<'a, Message> {
    let (w, h) = (Length::Fixed(THUMB_W), Length::Fixed(THUMB_H));
    let pic: Element<'a, Message> = match visual {
        Visual::Raster(handle) => {
            image(handle).content_fit(iced::ContentFit::Contain).border_radius(3.0).width(w).height(h).into()
        }
        Visual::Svg(handle) => {
            cosmic::iced::widget::svg(handle).content_fit(iced::ContentFit::Contain).width(w).height(h).into()
        }
    };
    mouse_area(pic).on_enter(Message::ThumbEnter(id)).on_exit(Message::ThumbLeave(id)).into()
}

/// Placeholder as wide as a thumbnail (no preview, no zoom).
fn icon_tile<'a>(name: &str) -> Element<'a, Message> {
    container(
        icon::icon(lucide(name)).size(18).class(theme::Svg::Custom(std::rc::Rc::new(|_| {
            cosmic::iced::widget::svg::Style { color: Some(pal::ICON) }
        }))),
    )
    .center_x(Length::Fixed(THUMB_W))
    .center_y(Length::Fixed(THUMB_H))
    .class(theme::Container::custom(|_| cosmic::iced::widget::container::Style {
        background: Some(Background::Color(pal::ACCENT)),
        border: Border { radius: 4.0.into(), ..Default::default() },
        ..Default::default()
    }))
    .into()
}

fn kbd<'a>(label: &'a str) -> Element<'a, Message> {
    container(txt(label).size(11).font(INTER_MED).class(pal::KBD_TEXT))
        .padding([1, 5])
        .class(theme::Container::custom(|_| cosmic::iced::widget::container::Style {
            background: Some(Background::Color(pal::KBD)),
            border: Border { radius: 4.0.into(), ..Default::default() },
            ..Default::default()
        }))
        .into()
}

fn hint<'a>(label: &'a str) -> Element<'a, Message> {
    txt(label).size(12).class(pal::MUTED).into()
}

fn dot<'a>(color: Color) -> Element<'a, Message> {
    container(widget::space().width(Length::Fixed(7.0)).height(Length::Fixed(7.0)))
        .class(theme::Container::custom(move |_| cosmic::iced::widget::container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 4.0.into(), ..Default::default() },
            ..Default::default()
        }))
        .into()
}

fn divider<'a>() -> Element<'a, Message> {
    container(widget::space().height(Length::Fixed(1.0)).width(Length::Fill))
        .class(theme::Container::custom(|_| cosmic::iced::widget::container::Style {
            background: Some(Background::Color(pal::BORDER)),
            ..Default::default()
        }))
        .into()
}

fn footer_style(_: &cosmic::Theme) -> cosmic::iced::widget::container::Style {
    cosmic::iced::widget::container::Style {
        background: Some(Background::Color(Color::from_rgb8(0x0d, 0x0d, 0x0d))),
        border: Border { radius: [0.0, 0.0, pal::R_CARD, pal::R_CARD].into(), width: 1.0, color: pal::BORDER },
        ..Default::default()
    }
}

fn backend_stream() -> impl cosmic::iced::futures::Stream<Item = Message> {
    cosmic::iced::stream::channel(64, async move |mut output: mpsc::Sender<Message>| {
        let (tx, mut rx) = mpsc::unbounded();
        clipboard::spawn(Arc::new(move |event| {
            let _ = tx.unbounded_send(event);
        }));
        while let Some(event) = rx.next().await {
            let _ = output.send(Message::Clipboard(event)).await;
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn instance_stream() -> impl cosmic::iced::futures::Stream<Item = Message> {
    cosmic::iced::stream::channel(8, async move |mut output: mpsc::Sender<Message>| {
        let (tx, mut rx) = mpsc::unbounded();
        if let Err(err) = crate::instance::listen(move |action| {
            if action != BACKGROUND_ACTION {
                let _ = tx.unbounded_send(Message::Toggle);
            }
        }) {
            tracing::warn!("single instance listener: {err}");
        }
        while let Some(message) = rx.next().await {
            let _ = output.send(message).await;
        }
        cosmic::iced::futures::future::pending::<()>().await;
    })
}

fn drawing_stream() -> impl cosmic::iced::futures::Stream<Item = Message> {
    cosmic::iced::stream::channel(8, async move |mut output: mpsc::Sender<Message>| {
        let (tx, mut rx) = mpsc::unbounded();
        let server = drawing::start(Arc::new(move |png, blank| {
            let _ = tx.unbounded_send((png, blank));
        }));
        let _ = output.send(Message::DrawingReady(server)).await;
        while let Some((png, blank)) = rx.next().await {
            let _ = output.send(Message::DrawingReceived(Arc::new(png), blank)).await;
        }
        cosmic::iced::futures::future::pending::<()>().await;
    })
}

/// Saves a drawing sent back from the iPad and prepares its history entry,
/// e.g. `Pictures/Drawings/Edited screenshot 2026-11-21 17-12-03.png`.
fn drawing_received(png: &[u8], origin: Origin) -> Option<(Arc<history::Ingested>, PathBuf)> {
    let dir = drawing::pictures_dir().join("Drawings");
    std::fs::create_dir_all(&dir).ok()?;
    let stamp = jiff::Zoned::now().strftime("%Y-%m-%d %H-%M-%S").to_string();
    let name = origin_name(origin, true);
    let mut path = dir.join(format!("{name} {stamp}.png"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{name} {stamp} ({n}).png"));
        n += 1;
    }
    std::fs::write(&path, png).ok()?;
    let mut ingested = history::ingest(vec![("image/png".to_string(), png.to_vec())])?;
    ingested.entry.from_ipad = true;
    ingested.entry.origin = origin;
    Some((Arc::new(ingested), path))
}

fn origin_name(origin: Origin, from_ipad: bool) -> &'static str {
    match origin {
        Origin::Screenshot => "Screenshot",
        Origin::EditedScreenshot => "Edited screenshot",
        Origin::Drawing => "Drawing",
        Origin::Edited => "Edited image",
        Origin::Other if from_ipad => "Edited image",
        Origin::Other => "Image",
    }
}

/// "Screenshot 17:12" (today) or "Edited screenshot 20 Nov, 17:12".
fn image_title(e: &Entry) -> String {
    let name = origin_name(e.origin, e.from_ipad);
    if e.origin == Origin::Other && !e.from_ipad {
        return name.to_string();
    }
    let tz = jiff::tz::TimeZone::system();
    let Ok(t) = jiff::Timestamp::from_second(e.created as i64) else { return name.to_string() };
    let t = t.to_zoned(tz.clone());
    let today = jiff::Timestamp::now().to_zoned(tz).date() == t.date();
    let when = if today { t.strftime("%H:%M").to_string() } else { t.strftime("%-d %b, %H:%M").to_string() };
    format!("{name} {when}")
}

/// A screenshot tool saved a file in Pictures/Screenshots a moment ago (COSMIC
/// copies the screenshot to the clipboard at the same time).
fn screenshot_just_taken() -> bool {
    drawing::screenshot_dirs().into_iter().any(|(dir, named)| {
        let Ok(dir) = std::fs::read_dir(dir) else { return false };
        dir.flatten().filter(|f| named(&f.file_name().to_string_lossy())).any(|f| {
            f.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < Duration::from_secs(15))
        })
    })
}

/// Opens the large preview if the pointer still rests on the thumbnail
/// after a moment (`generation` changes whenever it moves).
fn preview_after_rest(id: u64, generation: u64) -> Task<Action<Message>> {
    Task::perform(async { tokio::time::sleep(Duration::from_millis(350)).await }, move |_| {
        Action::App(Message::PreviewOpen(id, generation))
    })
}

fn notify(summary: &str, body: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("notify-send").args(["-a", "Clipstack", "-i", APP_ID, summary, body]).spawn();
    #[cfg(target_os = "macos")]
    {
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let script =
            format!("display notification {} with title \"Clipstack\" subtitle {}", quote(body), quote(summary));
        let _ = std::process::Command::new("osascript").args(["-e", &script]).spawn();
    }
    #[cfg(target_os = "windows")]
    clipboard::notify(summary, body);
}

fn thumb_handle(thumb: history::Thumb) -> image::Handle {
    image::Handle::from_rgba(thumb.width, thumb.height, thumb.rgba)
}

fn placeholder<'a>(name: &'static str, title: &'a str, body: &'a str) -> Element<'a, Message> {
    container(
        column![
            icon::icon(lucide(name)).size(28).class(theme::Svg::Custom(std::rc::Rc::new(|_| {
                cosmic::iced::widget::svg::Style { color: Some(pal::ICON) }
            }))),
            txt(title).size(14).font(INTER_MED).class(pal::TEXT),
            txt(body).size(12).class(pal::MUTED).align_x(Alignment::Center),
        ]
        .spacing(6)
        .align_x(Alignment::Center),
    )
    .center(Length::Fill)
    .padding(16)
    .into()
}

fn card_style(_: &cosmic::Theme) -> cosmic::iced::widget::container::Style {
    cosmic::iced::widget::container::Style {
        icon_color: Some(pal::TEXT),
        text_color: Some(pal::TEXT),
        background: Some(Background::Color(pal::CARD)),
        border: Border { radius: pal::R_CARD.into(), width: 1.0, color: pal::BORDER },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.45),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        snap: false,
    }
}

fn button_style(style: impl Fn(bool, bool, &cosmic::Theme) -> button::Style + Clone + 'static) -> theme::Button {
    let (a, b, c, d) = (style.clone(), style.clone(), style.clone(), style);
    theme::Button::Custom {
        active: Box::new(move |_, t| a(false, false, t)),
        disabled: Box::new(move |t| b(false, false, t)),
        hovered: Box::new(move |_, t| c(true, false, t)),
        pressed: Box::new(move |_, t| d(true, true, t)),
    }
}

fn flat_style(background: Option<Color>, radius: [f32; 4], fg: Color, border: (f32, Color)) -> button::Style {
    button::Style {
        shadow_offset: Vector::default(),
        background: background.map(Background::Color),
        overlay: None,
        border_radius: radius.into(),
        border_width: border.0,
        border_color: border.1,
        outline_width: 0.0,
        outline_color: Color::TRANSPARENT,
        icon_color: Some(fg),
        text_color: Some(fg),
    }
}

/// Ghost icon/text button; `on` = active (primary tint).
fn ghost_style(on: bool) -> theme::Button {
    button_style(move |hovered, pressed, _| {
        let bg = (hovered || pressed || on).then_some(if pressed { pal::ACCENT_STRONG } else { pal::ACCENT });
        let fg = if on {
            pal::PRIMARY
        } else if hovered {
            pal::TEXT
        } else {
            pal::MUTED
        };
        flat_style(bg, [pal::R_BUTTON; 4], fg, (0.0, Color::TRANSPARENT))
    })
}

/// Red ghost icon button for destructive actions.
fn danger_style() -> theme::Button {
    button_style(move |hovered, pressed, _| {
        let bg = (hovered || pressed).then_some(Color::from_rgba8(0xef, 0x44, 0x44, 0.14));
        flat_style(bg, [pal::R_BUTTON; 4], Color::from_rgb8(0xf8, 0x71, 0x71), (0.0, Color::TRANSPARENT))
    })
}

fn search_style() -> theme::TextInput {
    let look = || cosmic::widget::text_input::Appearance {
        background: Background::Color(Color::TRANSPARENT),
        border_radius: 0.0.into(),
        border_offset: None,
        border_width: 0.0,
        border_color: Color::TRANSPARENT,
        label_color: pal::MUTED,
        placeholder_color: pal::ICON,
        selected_text_color: pal::TEXT,
        icon_color: Some(pal::ICON),
        text_color: Some(pal::TEXT),
        selected_fill: Color::from_rgba(0.30, 0.42, 0.98, 0.35),
    };
    theme::TextInput::Custom {
        active: Box::new(move |_| look()),
        error: Box::new(move |_| look()),
        hovered: Box::new(move |_| look()),
        focused: Box::new(move |_| look()),
        disabled: Box::new(move |_| look()),
    }
}

/// "just now", "5 min ago", "3 h ago", "yesterday", "4 days ago".
fn ago_long(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        86400..172800 => "yesterday".into(),
        _ => format!("{} days ago", secs / 86400),
    }
}

/// Local date, e.g. "21 Nov 2026".
fn date(unix: u64) -> String {
    jiff::Timestamp::from_second(unix as i64)
        .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%-d %b %Y").to_string())
        .unwrap_or_default()
}

fn ago(created: u64) -> String {
    let secs = history::now().saturating_sub(created);
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

pub fn settings() -> app::Settings {
    app::Settings::default().no_main_window(true).exit_on_close(false).transparent(true)
}
