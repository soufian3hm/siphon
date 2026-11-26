//! Settings → Browser: the browsers on this computer, whether each can reach
//! Siphon, and the extension that has to be added to them by hand.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Rect, Sense, Stroke, StrokeKind, TextureHandle, Ui, Vec2};
use siphon_core::browser::{self, Browser};

use super::{readonly_line, small_note};
use crate::app::App;
use crate::format;
use crate::icons::{self, Icon};
use crate::setup::{self, BrowserState};
use crate::theme::{self, Anchor, Type, Weight};
use crate::widgets::{self, Button, ButtonKind};

const ROW_H: f32 = 56.0;
const ICON: f32 = 32.0;
/// How often the browsers are looked at again while the page is open: adding
/// the extension in a browser shows up here without anything being pressed.
const SURVEY_EVERY: Duration = Duration::from_millis(1500);

pub struct BrowserPanel {
    /// `None` until the first look has come back.
    rows: Option<Vec<BrowserState>>,
    icons: HashMap<&'static str, TextureHandle>,
    survey: Option<Receiver<Vec<BrowserState>>>,
    surveyed: Option<Instant>,
    /// A browser asked to open its extensions page, and how that went.
    opening: Option<(&'static str, Receiver<std::io::Result<()>>)>,
    /// Where the extension was saved for browsers to load.
    folder: Option<PathBuf>,
    copied: Option<Instant>,
}

/// What a row offers, decided before any of it is drawn.
enum Offer {
    Connect,
    Add,
    Manage,
}

impl BrowserPanel {
    pub fn new() -> Self {
        Self { rows: None, icons: HashMap::new(), survey: None, surveyed: None, opening: None, folder: None, copied: None }
    }

    /// Collects finished background work and starts the next look.
    fn poll(&mut self, app: &mut App, ctx: &egui::Context) {
        if self.folder.is_none() {
            self.folder = setup::extension_folder(app.manager.data_dir());
        }
        if let Some(receiver) = &self.survey {
            match receiver.try_recv() {
                Ok(rows) => {
                    for row in &rows {
                        if !self.icons.contains_key(row.browser.name)
                            && let Some(texture) = row.icon.as_deref().and_then(|path| load_icon(ctx, row.browser.name, path))
                        {
                            self.icons.insert(row.browser.name, texture);
                        }
                    }
                    self.rows = Some(rows);
                    self.survey = None;
                    self.surveyed = Some(Instant::now());
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.survey = None;
                    self.surveyed = Some(Instant::now());
                    self.rows.get_or_insert_with(Vec::new);
                }
            }
        }
        if self.survey.is_none() && self.surveyed.is_none_or(|at| at.elapsed() >= SURVEY_EVERY) {
            let (sender, receiver) = channel();
            let data_dir = app.manager.data_dir().to_owned();
            let wake = ctx.clone();
            std::thread::spawn(move || {
                let _ = sender.send(setup::survey(&data_dir));
                wake.request_repaint();
            });
            self.survey = Some(receiver);
        }
        if let Some((name, receiver)) = &self.opening {
            match receiver.try_recv() {
                Ok(Ok(())) => self.opening = None,
                Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                    app.toast_error(format!("{name} did not open its extensions page. Open chrome://extensions there yourself."));
                    self.opening = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        // Nothing else wakes an idle window; the page keeps itself current.
        ctx.request_repaint_after(Duration::from_millis(if self.rows.is_none() { 50 } else { 500 }));
    }

    fn rescan(&mut self) {
        self.surveyed = None;
    }

    /// Opens `browser` on its extensions page with the folder's path on the
    /// clipboard: the two things the user needs in hand to add the extension.
    fn add_to(&mut self, app: &mut App, ctx: &egui::Context, browser: &Browser) {
        let Some(folder) = self.folder.clone() else {
            app.toast_error("This copy of Siphon came without its extension. Install Siphon again.");
            return;
        };
        ctx.copy_text(folder.to_string_lossy().into_owned());
        self.copied = Some(Instant::now());
        self.open_page(browser);
        app.toast(format!("Path copied. In {}: Developer mode, Load unpacked, \u{2318}\u{21E7}G, paste, Select.", browser.name));
    }

    fn open_page(&mut self, browser: &Browser) {
        let (sender, receiver) = channel();
        let target = browser.clone();
        std::thread::spawn(move || {
            let _ = sender.send(target.open_extensions_page());
        });
        self.opening = Some((browser.name, receiver));
    }

    pub fn show(&mut self, app: &mut App, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.poll(app, &ctx);
        ui.add_space(8.0);

        let host = browser::bundled_host();
        if host.is_none() {
            widgets::callout(ui, theme::BAD, Icon::Alert, "The browser helper is missing", "siphon-host should sit next to the Siphon executable. Install Siphon again.");
            ui.add_space(14.0);
        }

        widgets::section(ui, "BROWSERS ON THIS COMPUTER");
        ui.add_space(10.0);
        match self.rows.clone() {
            None => skeleton(ui),
            Some(rows) if rows.is_empty() => empty(ui),
            Some(rows) => {
                let (list, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H * rows.len() as f32), Sense::hover());
                surface(ui, list);
                for (index, state) in rows.iter().enumerate() {
                    let row = Rect::from_min_size(egui::pos2(list.left(), list.top() + ROW_H * index as f32), Vec2::new(list.width(), ROW_H));
                    if index + 1 < rows.len() {
                        ui.painter().hline((row.left() + 56.0)..=row.right(), row.bottom() - 0.5, Stroke::new(1.0, theme::white(12)));
                    }
                    self.row(app, ui, row, state, host.is_some());
                }
            }
        }
        ui.add_space(20.0);

        widgets::section(ui, "ADD IT TO A BROWSER");
        ui.add_space(10.0);
        for (n, text) in [
            "Press \u{201C}Add extension\u{201D} next to your browser. Its extensions page opens, and the folder\u{2019}s path is copied for you.",
            "On that page, switch on \u{201C}Developer mode\u{201D}, then press \u{201C}Load unpacked\u{201D}.",
            "In the window that opens, press \u{2318}\u{21E7}G, paste the path with \u{2318}V, press Return, then \u{201C}Select\u{201D}.",
            "The browser\u{2019}s line above turns green. Click any download link: it arrives in Siphon.",
        ]
        .iter()
        .enumerate()
        {
            let galley = theme::layout_wrapped(ui.painter(), text, Type::BODY, theme::TEXT, ui.available_width() - 34.0, 3);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y.max(20.0) + 10.0), Sense::hover());
            let badge = Rect::from_min_size(rect.min, Vec2::splat(20.0));
            ui.painter().rect(badge, CornerRadius::same(10), theme::RAISED, Stroke::new(1.0, theme::white(24)), StrokeKind::Inside);
            theme::text_in(ui.painter(), badge, Anchor::Center, &(n + 1).to_string(), Type::mono(11.0, Weight::Semibold), theme::TEXT_2);
            ui.painter().galley(egui::pos2(rect.left() + 32.0, rect.top() + 1.0), galley, theme::TEXT);
        }
        ui.add_space(4.0);
        small_note(ui, "Dragging the folder from Finder onto the extensions page does the same as steps 2 and 3. Browsers accept an extension from an installer only when it comes from their store, which is why this one step is yours.", theme::TEXT_3);
        ui.add_space(20.0);

        widgets::section(ui, "EXTENSION FOLDER");
        ui.add_space(10.0);
        match self.folder.clone() {
            Some(folder) => {
                readonly_line(ui, Icon::Folder, &format::home_relative(&folder));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let fresh = self.copied.is_some_and(|at| at.elapsed() < Duration::from_millis(1600));
                    if Button::new(if fresh { "Copied" } else { "Copy path" }, ButtonKind::Secondary).icon(if fresh { Icon::Check } else { Icon::Copy }).min_width(112.0).show(ui).clicked() {
                        ctx.copy_text(folder.to_string_lossy().into_owned());
                        self.copied = Some(Instant::now());
                    }
                    if Button::new("Show in Finder", ButtonKind::Secondary).icon(Icon::Folder).show(ui).clicked()
                        && let Err(e) = setup::reveal(&folder)
                    {
                        app.toast_error(format!("The folder could not be shown: {e}"));
                    }
                });
                ui.add_space(8.0);
                small_note(ui, "Siphon keeps the extension here and brings it up to date with every new version. The folder stays where it is when the application moves.", theme::TEXT_3);
            }
            None => widgets::callout(ui, theme::BAD, Icon::Alert, "The extension is missing", "This copy of Siphon came without its extension. Install Siphon again."),
        }
        ui.add_space(20.0);

        // Video pages lean on two tools that are installed separately.
        widgets::section(ui, "VIDEO PAGES");
        ui.add_space(10.0);
        let (resolver, ffmpeg) = app.manager.video_tools();
        let (color, icon, title, body) = match (resolver.is_some(), ffmpeg.is_some()) {
            (true, true) => (theme::OK, Icon::CheckCircle, "YouTube and other video pages are ready", "yt-dlp finds the streams behind a page and ffmpeg joins picture and sound. Keep yt-dlp up to date: video sites change often."),
            (true, false) => (theme::WARN, Icon::Alert, "ffmpeg is missing", "Video pages above 360p come as separate picture and sound, which ffmpeg joins. In Terminal: brew install ffmpeg"),
            (false, _) => (theme::WARN, Icon::Alert, "Video pages need yt-dlp", "Siphon downloads the streams itself, but finding them behind a YouTube page is yt-dlp\u{2019}s job. In Terminal: brew install yt-dlp ffmpeg"),
        };
        widgets::callout(ui, color, icon, title, body);
    }

    fn row(&mut self, app: &mut App, ui: &mut Ui, row: Rect, state: &BrowserState, helper: bool) {
        let name = state.browser.name;
        let tile = Rect::from_center_size(egui::pos2(row.left() + 12.0 + ICON / 2.0, row.center().y), Vec2::splat(ICON));
        match self.icons.get(name) {
            Some(texture) => {
                ui.painter().image(texture.id(), tile, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                ui.painter().rect(tile.shrink(1.0), CornerRadius::same(7), theme::RAISED, Stroke::new(1.0, theme::white(22)), StrokeKind::Inside);
                icons::paint(ui.painter(), tile, Icon::External, 15.0, theme::TEXT_2);
            }
        }

        let (offer, color, status) = match (state.connected, state.extension) {
            (false, _) => (Offer::Connect, theme::WARN, "Not connected to this copy of Siphon"),
            (true, false) => (Offer::Add, theme::WARN, "Extension not added yet"),
            (true, true) => (Offer::Manage, theme::OK, "Extension added"),
        };
        let text_x = tile.right() + 12.0;
        let text_w = row.right() - text_x - 170.0;
        theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text_x, row.top() + 10.0), Vec2::new(text_w, 18.0)), Anchor::Left, name, Type::BODY_MEDIUM, theme::TEXT);
        let dot = egui::pos2(text_x + 3.5, row.top() + 37.5);
        ui.painter().circle_filled(dot, 5.5, theme::tint(color, 40));
        ui.painter().circle_filled(dot, 3.0, color);
        theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text_x + 13.0, row.top() + 29.0), Vec2::new(text_w - 13.0, 17.0)), Anchor::Left, status, Type::SMALL, theme::TEXT_2);

        let controls = Rect::from_min_max(egui::pos2(row.right() - 170.0, row.center().y - theme::CONTROL_H / 2.0), egui::pos2(row.right() - 12.0, row.center().y + theme::CONTROL_H / 2.0));
        let ctx = ui.ctx().clone();
        theme::place(ui, controls, egui::Layout::right_to_left(egui::Align::Center), |ui| match offer {
            Offer::Connect => {
                ui.add_enabled_ui(helper, |ui| {
                    if Button::new("Connect", ButtonKind::Primary).min_width(128.0).show(ui).clicked()
                        && let Some(host) = browser::bundled_host()
                    {
                        let failed: Vec<String> = browser::register_host(&host).iter().filter_map(|r| r.result.as_ref().err().map(|e| format!("{}: {e}", r.browser))).collect();
                        if failed.is_empty() {
                            app.toast(format!("{name} can reach Siphon now."));
                        } else {
                            app.toast_error(failed.join("  "));
                        }
                        self.rescan();
                    }
                });
            }
            Offer::Add => {
                if Button::new("Add extension", ButtonKind::Primary).icon(Icon::Plus).min_width(128.0).show(ui).clicked() {
                    self.add_to(app, &ctx, &state.browser);
                }
            }
            Offer::Manage => {
                if Button::new("Extensions page", ButtonKind::Secondary).icon(Icon::External).min_width(128.0).show(ui).on_hover_text("After an update of Siphon, press the reload arrow on the extension\u{2019}s card.").clicked() {
                    self.open_page(&state.browser);
                }
            }
        });
    }
}

/// The rounded tray the rows sit in.
fn surface(ui: &Ui, rect: Rect) {
    ui.painter().rect(rect, CornerRadius::same(theme::R_PANEL), theme::tint(theme::RAISED, 150), Stroke::new(1.0, theme::white(16)), StrokeKind::Inside);
    // The lit top edge every raised surface in the application has.
    ui.painter().hline((rect.left() + 6.0)..=(rect.right() - 6.0), rect.top() + 1.5, Stroke::new(1.0, theme::white(10)));
}

/// Two rows in the shape of what is coming, while the first look is under way.
fn skeleton(ui: &mut Ui) {
    let (list, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H * 2.0), Sense::hover());
    surface(ui, list);
    let pulse = ((ui.input(|input| input.time) * 3.2).sin() * 0.5 + 0.5) as f32;
    let fill = theme::white(10 + (pulse * 10.0) as u8);
    for index in 0..2 {
        let row = Rect::from_min_size(egui::pos2(list.left(), list.top() + ROW_H * index as f32), Vec2::new(list.width(), ROW_H));
        if index == 0 {
            ui.painter().hline((row.left() + 56.0)..=row.right(), row.bottom() - 0.5, Stroke::new(1.0, theme::white(12)));
        }
        let bar = |x: f32, y: f32, w: f32, h: f32, radius: u8| ui.painter().rect_filled(Rect::from_min_size(egui::pos2(row.left() + x, row.top() + y), Vec2::new(w, h)), CornerRadius::same(radius), fill);
        bar(12.0, 12.0, ICON, ICON, 8);
        bar(56.0, 13.0, 118.0 - 22.0 * index as f32, 11.0, 4);
        bar(56.0, 33.0, 164.0, 9.0, 4);
        bar(row.width() - 12.0 - 128.0, 14.0, 128.0, theme::CONTROL_H, theme::R_CONTROL);
    }
}

/// No browser that can run the extension is installed.
fn empty(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 104.0), Sense::hover());
    surface(ui, rect);
    icons::paint(ui.painter(), Rect::from_center_size(egui::pos2(rect.center().x, rect.top() + 28.0), Vec2::splat(22.0)), Icon::External, 18.0, theme::TEXT_3);
    theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(rect.left(), rect.top() + 44.0), Vec2::new(rect.width(), 20.0)), Anchor::Center, "No browser found that can run the extension", Type::BODY_MEDIUM, theme::TEXT);
    theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(rect.left(), rect.top() + 65.0), Vec2::new(rect.width(), 18.0)), Anchor::Center, "Chrome, Edge, Brave, Dia, Arc, Vivaldi and Opera appear here once installed.", Type::SMALL, theme::TEXT_2);
}

/// A browser's icon as a texture, sized for sharp drawing on a dense screen.
fn load_icon(ctx: &egui::Context, name: &str, path: &std::path::Path) -> Option<TextureHandle> {
    let picture = image::open(path).ok()?.to_rgba8();
    let side = (ICON * 2.0) as u32;
    let picture = image::imageops::resize(&picture, side, side, image::imageops::FilterType::Lanczos3);
    let pixels = egui::ColorImage::from_rgba_unmultiplied([side as usize, side as usize], picture.as_raw());
    Some(ctx.load_texture(format!("browser-icon-{name}"), pixels, egui::TextureOptions::LINEAR))
}
