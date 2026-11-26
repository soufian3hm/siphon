//! Modal dialogs: new download (also used for captured links), swap link,
//! remove, and settings.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Frame, Key, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use siphon_core::net::{ProxyAuth, ProxyKind, ProxyMode};
use siphon_core::zip_peek::ArchiveListing;
use siphon_core::{Capture, Category, Confirmation, DownloadId, DownloadView, Duplicate, Error, Manager, NewDownload, ProbeInfo, Protocol, Request, Settings, Status, Ticket, VariantInfo};

use crate::app::App;
use crate::format;
use crate::icons::{self, Icon};
use crate::theme::{self, Anchor, Type, Weight};
use crate::widgets::{self, Button, ButtonKind, Field};

mod batch;
mod browsers;
mod grab;
mod install;
mod login;
mod schedule;
mod verify;

pub use batch::{BatchDialog, pick_links_file};
pub use grab::GrabDialog;
pub use install::InstallDialog;
pub use login::LoginDialog;
pub use schedule::ScheduleDialog;
pub use verify::VerifyDialog;

pub enum Dialog {
    Add(Box<AddDialog>),
    Swap(SwapDialog),
    Remove(RemoveDialog),
    Settings(Box<SettingsDialog>),
    Batch(Box<BatchDialog>),
    Grab(Box<GrabDialog>),
    Schedule(ScheduleDialog),
    Verify(VerifyDialog),
    Login(LoginDialog),
    Install(InstallDialog),
}

/// What a dialog wants to happen after this frame.
enum Outcome {
    Stay,
    Close,
}

fn dialog_frame() -> Frame {
    Frame::new()
        .fill(theme::PANEL)
        .stroke(Stroke::new(1.0, theme::white(34)))
        .corner_radius(CornerRadius::same(theme::R_DIALOG))
        .inner_margin(egui::Margin::same(0))
        .shadow(egui::Shadow { offset: [0, 18], blur: 48, spread: 0, color: Color32::from_black_alpha(150) })
}

/// Runs a dialog body inside a modal of the given width.
fn modal(ctx: &egui::Context, id: &str, width: f32, body: impl FnOnce(&mut Ui) -> Outcome) -> Outcome {
    let response = egui::Modal::new(egui::Id::unique(id)).frame(dialog_frame()).backdrop_color(Color32::from_black_alpha(150)).show(ctx, |ui| {
        ui.set_width(width);
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 0.0);
        body(ui)
    });
    if response.should_close() { Outcome::Close } else { response.inner }
}

/// Title, optional subtitle and the close button.
fn title_bar(ui: &mut Ui, title: &str, subtitle: Option<&str>) -> bool {
    let width = ui.available_width();
    let height = if subtitle.is_some() { 64.0 } else { 52.0 };
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(rect.left() + 22.0, rect.top() + 16.0), Vec2::new(width - 80.0, 22.0)), Anchor::Left, title, Type::TITLE, theme::TEXT);
    if let Some(subtitle) = subtitle {
        theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(rect.left() + 22.0, rect.top() + 38.0), Vec2::new(width - 80.0, 16.0)), Anchor::Left, subtitle, Type::SMALL, theme::TEXT_2);
    }
    let mut close = false;
    theme::region(ui, Rect::from_min_size(egui::pos2(rect.right() - 12.0 - 28.0, rect.top() + 12.0), Vec2::splat(28.0)), |ui| {
        close = widgets::icon_button(ui, Icon::Close, "").clicked();
    });
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, theme::hairline());
    close
}

/// Padded content area.
fn padded<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new().inner_margin(egui::Margin { left: 22, right: 22, top: 18, bottom: 18 }).show(ui, add).inner
}

/// The button row at the bottom of a dialog, laid out right to left.
fn footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 60.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.top() + 0.5, theme::hairline());
    theme::place(ui, rect.shrink2(Vec2::new(22.0, 15.0)), egui::Layout::right_to_left(egui::Align::Center), add);
}

/// A folder field with its "Browse" button. `touched` is set once the user
/// has chosen a folder themselves.
fn folder_row(ui: &mut Ui, dir: &mut String, touched: &mut bool) {
    ui.horizontal(|ui| {
        let w = ui.available_width() - 96.0;
        let before = dir.clone();
        Field::new(dir).icon(Icon::Folder).width(w).show(ui);
        if *dir != before {
            *touched = true;
        }
        if Button::new("Browse\u{2026}", ButtonKind::Secondary).min_width(88.0).show(ui).clicked()
            && let Some(picked) = rfd::FileDialog::new().set_directory(dir.as_str()).pick_folder()
        {
            *dir = picked.to_string_lossy().into_owned();
            *touched = true;
        }
    });
}

fn field_label(ui: &mut Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::hover());
    theme::text_in(ui.painter(), rect, Anchor::Left, text, Type::LABEL, theme::TEXT_3);
}

/// The modal shown between the queue finishing and its end-of-queue action.
pub fn countdown(ctx: &egui::Context, title: &str, results: &str, act_label: &str, cancel: &mut bool, act_now: &mut bool) {
    let outcome = modal(ctx, "queue-done", 420.0, |ui| {
        padded(ui, |ui| {
            let galley = theme::layout_wrapped(ui.painter(), title, Type::TITLE, theme::TEXT, ui.available_width(), 2);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y), Sense::hover());
            ui.painter().galley(rect.min, galley, theme::TEXT);
            ui.add_space(8.0);
            let galley = theme::layout_wrapped(ui.painter(), results, Type::BODY, theme::TEXT_2, ui.available_width(), 3);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y), Sense::hover());
            ui.painter().galley(rect.min, galley, theme::TEXT_2);
        });
        footer(ui, |ui| {
            if Button::new("Cancel", ButtonKind::Primary).min_width(110.0).show(ui).clicked() {
                *cancel = true;
            }
            if Button::new(act_label, ButtonKind::Secondary).show(ui).clicked() {
                *act_now = true;
            }
        });
        if ui.should_close() { Outcome::Close } else { Outcome::Stay }
    });
    // Escape, or a click outside, means "don't".
    if matches!(outcome, Outcome::Close) {
        *cancel = true;
    }
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(mut dialog) = app.dialog.take() else { return };
    let outcome = match &mut dialog {
        Dialog::Add(d) => d.show(app, ctx),
        Dialog::Swap(d) => d.show(app, ctx),
        Dialog::Remove(d) => d.show(app, ctx),
        Dialog::Settings(d) => d.show(app, ctx),
        Dialog::Batch(d) => d.show(app, ctx),
        Dialog::Grab(d) => d.show(app, ctx),
        Dialog::Schedule(d) => d.show(app, ctx),
        Dialog::Verify(d) => d.show(app, ctx),
        Dialog::Login(d) => d.show(app, ctx),
        Dialog::Install(d) => d.show(app, ctx),
    };
    match outcome {
        Outcome::Stay => app.dialog = Some(dialog),
        Outcome::Close => dismiss(app, dialog),
    }
}

/// Lets a dialog that is going away finish its business.
pub fn dismiss(app: &mut App, dialog: Dialog) {
    match dialog {
        Dialog::Settings(mut d) => d.commit(app),
        // A link fetched ahead of the user's answer is dropped with the dialog.
        Dialog::Add(mut d) => d.closed(app),
        _ => {}
    }
}

// ------------------------------------------------------------------- Add ----

/// The archive preview of the add dialog.
#[derive(Default)]
struct Peek {
    ticket: Option<Ticket<Result<ArchiveListing, Error>>>,
    listing: Option<Result<ArchiveListing, Error>>,
}

pub struct AddDialog {
    url: String,
    capture: Option<Capture>,
    /// The download already fetching this link behind the dialog.
    provisional: Option<DownloadId>,
    /// Its latest state.
    live: Option<DownloadView>,
    /// The user said yes: the background download is theirs now.
    confirmed: bool,
    /// The address the current `info` describes.
    probed: String,
    ticket: Option<Ticket<Result<ProbeInfo, Error>>>,
    info: Option<Result<ProbeInfo, Error>>,
    name: String,
    name_touched: bool,
    pub dir: String,
    dir_touched: bool,
    connections: usize,
    edited: Instant,
    /// A stalled download this link would revive.
    candidate: Option<DownloadId>,
    /// An entry of the list with the same address, and the address it was
    /// looked up for.
    duplicate: Option<Duplicate>,
    duplicate_for: String,
    always_auto: bool,
    first_frame: bool,
    settings: Settings,
    peek: Peek,
    login_user: String,
    login_password: String,
    login_error: Option<String>,
    /// The typed address the background download belongs to, and the one
    /// for which starting ahead was declined.
    ahead_for: String,
    ahead_declined: String,
    /// For a video page: the quality the user picked, if they picked one.
    variant: Option<String>,
}

impl AddDialog {
    pub fn new(manager: &Manager, url: String) -> Self {
        let settings = manager.settings();
        Self {
            url,
            capture: None,
            provisional: None,
            live: None,
            confirmed: false,
            probed: String::new(),
            ticket: None,
            info: None,
            name: String::new(),
            name_touched: false,
            dir: settings.download_dir.to_string_lossy().into_owned(),
            dir_touched: false,
            connections: settings.connections,
            // Already old enough: a pasted address is looked up at once.
            edited: Instant::now() - Duration::from_secs(1),
            candidate: None,
            duplicate: None,
            duplicate_for: String::new(),
            always_auto: false,
            first_frame: true,
            settings,
            peek: Peek::default(),
            login_user: String::new(),
            login_password: String::new(),
            login_error: None,
            ahead_for: String::new(),
            ahead_declined: String::new(),
            variant: None,
        }
    }

    /// For the screenshot script: presses the primary button.
    pub fn script_confirm(&mut self, app: &mut App) {
        self.start(app, false);
    }

    /// A link handed over by the browser. `provisional` is the download
    /// already fetching it, when instant start is on.
    pub fn for_capture(manager: &Manager, capture: Capture, provisional: Option<DownloadId>) -> Self {
        let mut dialog = Self::new(manager, capture.request.url.clone());
        if let Some(name) = capture.suggested_name.as_deref().filter(|n| !n.trim().is_empty()) {
            dialog.name = siphon_core::filename::sanitize(name);
        }
        dialog.capture = Some(capture);
        dialog.provisional = provisional;
        dialog
    }

    /// For the screenshot script: opens the archive preview.
    pub fn script_peek(&mut self, manager: &Manager) {
        self.peek.ticket = Some(manager.peek_archive(self.request()));
    }

    /// Called when the dialog goes away by any route. A background download
    /// the user never accepted is thrown away with everything it fetched.
    fn closed(&mut self, app: &mut App) {
        if let Some(id) = self.provisional.take()
            && !self.confirmed
        {
            app.manager.discard(id);
        }
    }

    /// The request to send: the browser's own (cookies and all) while the
    /// address is still the one it captured.
    pub fn request(&self) -> Request {
        match &self.capture {
            Some(capture) if capture.request.url.trim() == self.url.trim() => capture.request.clone(),
            _ => Request::new(self.url.trim()),
        }
    }

    fn looks_like_a_link(&self) -> bool {
        let url = self.url.trim().to_ascii_lowercase();
        ["http://", "https://", "ftp://", "mms://", "mmsh://"].iter().any(|scheme| url.starts_with(scheme) && url.len() > scheme.len() + 3)
    }

    fn lookup(&mut self, app: &App) {
        let url = self.url.trim().to_owned();
        if url != self.probed && self.looks_like_a_link() && self.edited.elapsed() >= Duration::from_millis(350) {
            self.probed = url;
            self.info = None;
            self.candidate = None;
            self.peek = Peek::default();
            self.ticket = Some(app.manager.probe(self.request()));
        }
        if !self.looks_like_a_link() && !self.probed.is_empty() {
            self.probed.clear();
            self.ticket = None;
            self.info = None;
            self.candidate = None;
            self.peek = Peek::default();
        }
        if let Some(result) = self.ticket.as_ref().and_then(|t| t.take()) {
            self.ticket = None;
            if let Ok(info) = &result {
                if !self.name_touched && self.name.is_empty() {
                    self.name = info.file_name.clone();
                }
                self.candidate = app.manager.refresh_candidate(info.total, Some(&self.name));
            }
            self.info = Some(result);
        }
    }

    /// A link typed or pasted here is treated like one from the browser:
    /// once the server has answered for it, it starts downloading behind the
    /// dialog. Editing the address afterwards throws that away and starts over.
    fn start_ahead(&mut self, app: &App) {
        let answered = matches!(self.info, Some(Ok(_))) && self.url.trim() == self.probed;
        // Links from the browser were considered for this when they arrived.
        if !answered || self.capture.is_some() || self.ahead_declined == self.probed {
            return;
        }
        let capture = Capture { request: self.request(), ..Default::default() };
        match app.manager.start_ahead(&capture) {
            Some(id) => {
                self.provisional = Some(id);
                self.ahead_for = self.probed.clone();
            }
            // Switched off, or already in the list: do not ask again for this address.
            None => self.ahead_declined = self.probed.clone(),
        }
    }

    /// Reads what the background download has found out so far.
    fn follow(&mut self, app: &App) {
        let Some(id) = self.provisional else { return };
        let Some(view) = app.manager.get(id) else {
            self.provisional = None;
            self.live = None;
            return;
        };
        let answered = match &view.status {
            Status::NeedsRefresh { code } => Some(Err(Error::Expired(*code))),
            Status::Failed { message, .. } if view.retry_in.is_none() => Some(Err(Error::Protocol(message.trim_end_matches('.').to_owned()))),
            // The server has answered once its range support is known.
            _ if view.resumable.is_some() || view.status.is_finished() => Some(Ok(ProbeInfo {
                file_name: view.file_name.clone(),
                total: view.total,
                resumable: view.resumable.unwrap_or(false),
                content_type: view.content_type.clone(),
                final_url: view.url.clone(),
                protocol: view.protocol,
                note: view.note.clone(),
                variants: app.manager.variants(id),
                variant: view.variant.clone(),
            })),
            _ => None,
        };
        // A typed link was already looked up once; what that found stays on
        // screen until the background download has its own answer.
        if answered.is_some() || self.capture.is_some() {
            self.info = answered;
        }
        if matches!(self.info, Some(Ok(_))) {
            if !self.name_touched {
                self.name = view.file_name.clone();
            }
            if self.candidate.is_none() && !view.status.is_finished() {
                self.candidate = app.manager.refresh_candidate(view.total, Some(&self.name));
            }
        }
        if !self.dir_touched {
            self.dir = view.dir.to_string_lossy().into_owned();
        }
        self.live = Some(view);
    }

    fn show(&mut self, app: &mut App, ctx: &egui::Context) -> Outcome {
        if self.provisional.is_some() {
            self.follow(app);
        } else {
            self.lookup(app);
            self.start_ahead(app);
            // The folder follows the file's type until the user picks one.
            if !self.dir_touched && !self.name.trim().is_empty() {
                self.dir = self.settings.folder_for(self.name.trim()).to_string_lossy().into_owned();
            }
        }
        if self.duplicate_for != self.url.trim() {
            self.duplicate_for = self.url.trim().to_owned();
            self.duplicate = if self.looks_like_a_link() { app.manager.duplicate_of(&self.duplicate_for) } else { None };
        }
        if let Some(result) = self.peek.ticket.as_ref().and_then(|t| t.take()) {
            self.peek.ticket = None;
            self.peek.listing = Some(result);
        }

        let in_background = self.live.as_ref().is_some_and(|v| v.status.is_active() || v.status == Status::Queued);
        let waiting = if self.provisional.is_some() {
            self.info.is_none()
        } else {
            self.ticket.is_some() || (self.looks_like_a_link() && self.url.trim() != self.probed)
        };
        if waiting || in_background || self.peek.ticket.is_some() {
            ctx.request_repaint_after(Duration::from_millis(60));
        }

        let subtitle = self.capture.as_ref().map(|c| match c.page_title.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(title) => format!("From your browser  \u{00B7}  {title}"),
            None => "Sent by your browser".to_owned(),
        });
        let is_page = Protocol::detect(self.url.trim()) == Protocol::Page;
        let title = match &self.capture {
            _ if is_page => "Save this video",
            Some(c) if c.media => "Save this stream",
            Some(_) => "Download this file",
            None => "New download",
        };
        let mut pick_variant: Option<VariantInfo> = None;

        let mut go: Option<bool> = None;
        let mut revive = false;
        let mut show_existing: Option<DownloadId> = None;
        let mut sign_in = false;
        let mut address_changed = false;
        let arrived = self.live.as_ref().is_some_and(|v| v.status.is_finished());
        let outcome = modal(ctx, "add-download", 560.0, |ui| {
            if title_bar(ui, title, subtitle.as_deref()) {
                return Outcome::Close;
            }
            padded(ui, |ui| {
                field_label(ui, "ADDRESS");
                if self.provisional.is_some() && self.capture.is_some() {
                    // Handed over by the browser and already being fetched:
                    // the address is a fact, not a field.
                    readonly_line(ui, Icon::Link, &format::safe_url(self.url.trim()));
                } else {
                    let id = egui::Id::unique("add-url");
                    let before = self.url.clone();
                    let response = Field::new(&mut self.url).hint("https://example.com/file.zip").icon(Icon::Link).mono().id(id).show(ui);
                    if self.first_frame && self.url.is_empty() {
                        response.request_focus();
                    }
                    if self.url != before {
                        self.edited = Instant::now();
                        if !self.name_touched {
                            self.name.clear();
                        }
                        address_changed = true;
                    }
                }
                self.first_frame = false;
                ui.add_space(14.0);

                self.file_card(ui, waiting);
                self.peek_section(app, ui);

                if matches!(self.info, Some(Err(Error::Expired(401)))) {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 104.0 - 16.0) / 2.0;
                        Field::new(&mut self.login_user).hint("User name").width(w).id(egui::Id::unique("add-login-user")).show(ui);
                        Field::new(&mut self.login_password).hint("Password").password().width(w).id(egui::Id::unique("add-login-pass")).show(ui);
                        ui.add_enabled_ui(!self.login_user.trim().is_empty(), |ui| {
                            if Button::new("Sign in", ButtonKind::Secondary).min_width(104.0).show(ui).clicked() {
                                sign_in = true;
                            }
                        });
                    });
                    if let Some(error) = &self.login_error {
                        ui.add_space(6.0);
                        small_note(ui, error, theme::BAD);
                    }
                }

                if let Some(id) = self.candidate
                    && let Some(view) = app.view(id)
                {
                    ui.add_space(12.0);
                    let done = view.fraction.map(format::percent).unwrap_or_default();
                    widgets::callout(
                        ui,
                        theme::ACCENT,
                        Icon::Refresh,
                        &format!("This is a fresh link for \u{201C}{}\u{201D}", view.file_name),
                        &format!("That download stopped at {done} when its link expired. It can continue with this one instead of starting a new file."),
                    );
                    ui.add_space(8.0);
                    if Button::new("Continue the stalled download", ButtonKind::Secondary).icon(Icon::Play).fill().show(ui).clicked() {
                        revive = true;
                    }
                } else if let Some(duplicate) = &self.duplicate {
                    ui.add_space(12.0);
                    let (title, body) = match (duplicate.status.is_finished(), duplicate.file_exists) {
                        (true, true) => ("You have already downloaded this", "The file is still where it was saved. Downloading again keeps both, the new one under a numbered name."),
                        (true, false) => ("Downloaded before, but the file has since been moved or deleted", "Downloading again saves a fresh copy."),
                        (false, _) => ("This address is already in your list", "It has not finished yet. Downloading again would fetch a second copy."),
                    };
                    widgets::callout(ui, theme::TEXT_2, Icon::Stack, title, body);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if Button::new("Show it in the list", ButtonKind::Secondary).icon(Icon::Stack).show(ui).clicked() {
                            show_existing = Some(duplicate.id);
                        }
                        if duplicate.file_exists && Button::new("Open the file", ButtonKind::Secondary).icon(Icon::External).show(ui).clicked() {
                            let _ = open::that_detached(&duplicate.path);
                            show_existing = Some(duplicate.id);
                        }
                    });
                }
                ui.add_space(16.0);

                if let Some(Ok(info)) = &self.info
                    && !info.variants.is_empty()
                {
                    // What the page offers, best picture first.
                    let labels: Vec<String> = info
                        .variants
                        .iter()
                        .map(|v| match v.size {
                            Some(size) => format!("{}  \u{00B7}  {}  \u{00B7}  {}", v.label, v.detail, format::bytes(size)),
                            None => format!("{}  \u{00B7}  {}", v.label, v.detail),
                        })
                        .collect();
                    let options: Vec<(usize, &str)> = labels.iter().enumerate().map(|(i, label)| (i, label.as_str())).collect();
                    let current = self.variant.as_deref().or(info.variant.as_deref());
                    let mut index = info.variants.iter().position(|v| Some(v.id.as_str()) == current).unwrap_or(0);
                    field_label(ui, "QUALITY");
                    if widgets::dropdown(ui, "add-quality", &options, &mut index, ui.available_width()) {
                        pick_variant = info.variants.get(index).cloned();
                    }
                    ui.add_space(14.0);
                }

                field_label(ui, if self.settings.sort_by_type && !self.dir_touched { "SAVE TO  \u{00B7}  SORTED BY TYPE" } else { "SAVE TO" });
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 96.0;
                    let before = self.dir.clone();
                    Field::new(&mut self.dir).icon(Icon::Folder).width(w).show(ui);
                    if self.dir != before {
                        self.dir_touched = true;
                    }
                    if Button::new("Browse\u{2026}", ButtonKind::Secondary).min_width(88.0).show(ui).clicked()
                        && let Some(dir) = rfd::FileDialog::new().set_directory(&self.dir).pick_folder()
                    {
                        self.dir = dir.to_string_lossy().into_owned();
                        self.dir_touched = true;
                    }
                });
                ui.add_space(14.0);

                let segmentable = !matches!(&self.info, Some(Ok(info)) if !info.resumable);
                let detail = if segmentable {
                    "More connections split the file into more parts fetched at once."
                } else {
                    "This server sends the file in one piece; one connection is all it accepts."
                };
                ui.add_enabled_ui(segmentable, |ui| {
                    widgets::setting_row_with(ui, "Connections", detail, 104.0, self.capture.is_some(), |ui| {
                        widgets::stepper(ui, &mut self.connections, 1, siphon_core::settings::MAX_CONNECTIONS);
                    });
                });
                if self.capture.is_some() {
                    ui.add_space(10.0);
                    widgets::checkbox(ui, &mut self.always_auto, "Save automatically from now on, without asking");
                }
            });

            let can_go = self.looks_like_a_link() && !self.dir.trim().is_empty() && !matches!(self.info, Some(Err(_)));
            footer(ui, |ui| {
                ui.add_enabled_ui(can_go, |ui| {
                    // Fetched in full behind the dialog: all that is left is to keep it.
                    let (label, icon) = match (arrived, self.duplicate.is_some()) {
                        (true, _) => ("Save", Icon::Check),
                        (false, true) => ("Download again", Icon::Download),
                        (false, false) => ("Download", Icon::Download),
                    };
                    if Button::new(label, ButtonKind::Primary).icon(icon).min_width(124.0).show(ui).clicked() {
                        go = Some(false);
                    }
                    if !arrived && Button::new("Download later", ButtonKind::Secondary).icon(Icon::Clock).show(ui).on_hover_text("Keep it for the queue. It starts when the queue does.").clicked() {
                        go = Some(true);
                    }
                });
                if Button::new("Cancel", ButtonKind::Ghost).show(ui).clicked() {
                    go = None;
                    ui.close();
                }
            });
            if can_go && !ctx.text_edit_focused() && ui.input(|i| i.key_pressed(Key::Enter)) {
                go = Some(false);
            }
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        });

        if let Some(variant) = pick_variant {
            self.variant = Some(variant.id.clone());
            match self.provisional {
                // Already being fetched at the other quality: it starts over.
                Some(id) => {
                    app.manager.set_variant(id, &variant.id);
                    self.info = None;
                }
                None => {
                    if let Some(Ok(info)) = &mut self.info {
                        info.total = variant.size;
                        info.variant = Some(variant.id.clone());
                        info.note = Some(variant.label.clone());
                    }
                    // Another quality can be another kind of file.
                    if !self.name_touched {
                        let stem = self.name.rsplit_once('.').map(|(stem, _)| stem.to_owned()).unwrap_or_else(|| self.name.clone());
                        self.name = format!("{stem}.{}", variant.container);
                    }
                }
            }
        }
        // The address under a background download was edited: that download
        // was for another file, so it goes, and the new address is looked up.
        if address_changed
            && self.url.trim() != self.ahead_for
            && let Some(id) = self.provisional.take()
        {
            app.manager.discard(id);
            self.live = None;
            self.info = None;
            self.probed.clear();
            self.dir_touched = false;
        }
        if sign_in {
            self.sign_in(app);
            return Outcome::Stay;
        }
        if let Some(id) = show_existing {
            app.filter = crate::app::Filter::All;
            app.select_only(id);
            return Outcome::Close;
        }
        if revive && let Some(id) = self.candidate {
            // The check runs in the background; the list shows the result.
            let _ = app.manager.refresh_link(id, self.request());
            app.select_only(id);
            app.toast("Continuing with the fresh link");
            return Outcome::Close;
        }
        if let Some(later) = go {
            self.start(app, later);
            return Outcome::Close;
        }
        outcome
    }

    /// Saves the login for this site and asks the server again.
    fn sign_in(&mut self, app: &mut App) {
        let Some(host) = siphon_core::organize::host_of(self.url.trim()) else { return };
        match app.manager.save_login(&host, self.login_user.trim(), &self.login_password) {
            Ok(()) => {
                self.login_error = None;
                self.login_password.clear();
                self.info = None;
                match self.provisional {
                    // The same link again, this time with the login.
                    Some(id) => app.manager.retry(id),
                    None => self.probed.clear(),
                }
            }
            Err(e) => self.login_error = Some(format!("The login could not be saved: {e}")),
        }
    }

    fn start(&mut self, app: &mut App, later: bool) {
        let settings = app.manager.settings();
        let dir = PathBuf::from(self.dir.trim());
        if self.always_auto {
            let mut next = settings.clone();
            next.save_automatically = true;
            // A folder picked here becomes the default, unless files are
            // sorted by type (then this is one type's folder, not the default).
            if self.dir_touched && !settings.sort_by_type {
                next.download_dir = dir.clone();
            }
            if let Err(e) = app.manager.set_settings(next) {
                app.toast_error(e.to_string());
            }
        }
        let name = Some(self.name.trim().to_owned()).filter(|n| !n.is_empty());
        let connections = (self.connections != settings.connections).then_some(self.connections);

        let id = match self.provisional {
            Some(id) => {
                self.confirmed = true;
                let decision = Confirmation { dir: Some(dir), file_name: name.filter(|_| self.name_touched), connections, later };
                if let Err(e) = app.manager.confirm(id, decision) {
                    app.toast_error(e.to_string());
                }
                id
            }
            None => {
                let (total, content_type) = match &self.info {
                    Some(Ok(info)) => (info.total, info.content_type.clone()),
                    _ => (self.capture.as_ref().and_then(|c| c.size), self.capture.as_ref().and_then(|c| c.mime.clone())),
                };
                app.manager.add(NewDownload {
                    request: self.request(),
                    dir: Some(dir),
                    file_name: name,
                    connections,
                    later,
                    total,
                    content_type,
                    variant: self.variant.clone(),
                    ..Default::default()
                })
            }
        };
        if later {
            app.toast("Kept for the queue");
        }
        app.select_only(id);
    }

    /// The contents of a ZIP archive, read from the server without
    /// downloading the file.
    fn peek_section(&mut self, app: &App, ui: &mut Ui) {
        let peekable = matches!(&self.info, Some(Ok(info)) if info.resumable && info.protocol == Protocol::Http) && siphon_core::zip_peek::looks_like_zip(self.name.trim());
        if !peekable {
            return;
        }
        ui.add_space(8.0);
        match (&self.peek.listing, self.peek.ticket.is_some()) {
            (None, false) => {
                if Button::new("See what is inside", ButtonKind::Ghost).icon(Icon::Archive).show(ui).on_hover_text("Reads the archive\u{2019}s table of contents without downloading it").clicked() {
                    self.peek.ticket = Some(app.manager.peek_archive(self.request()));
                }
            }
            (None, true) => small_note(ui, "Reading the archive\u{2019}s table of contents\u{2026}", theme::TEXT_2),
            (Some(Err(error)), _) => small_note(ui, &format!("Could not look inside: {error}."), theme::TEXT_2),
            (Some(Ok(listing)), _) => {
                let summary = format!(
                    "{} {}  \u{00B7}  {} unpacked{}",
                    listing.files,
                    if listing.files == 1 { "file" } else { "files" },
                    format::bytes(listing.unpacked),
                    if listing.truncated { "  \u{00B7}  list cut short" } else { "" }
                );
                Frame::new().fill(theme::FIELD).stroke(Stroke::new(1.0, theme::white(16))).corner_radius(CornerRadius::same(theme::R_PANEL)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let (head, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
                    theme::text_in(ui.painter(), head, Anchor::Left, &summary, Type::SMALL_MEDIUM, theme::TEXT_2);
                    ui.painter().hline(head.x_range(), head.bottom() + 2.5, Stroke::new(1.0, theme::white(12)));
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().id_salt("add-peek").max_height(126.0).auto_shrink([false, true]).show_rows(ui, 21.0, listing.entries.len(), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for entry in &listing.entries[range] {
                            let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 21.0), Sense::hover());
                            let size_w = 84.0;
                            let name_rect = Rect::from_min_max(row.min, egui::pos2(row.right() - size_w - 18.0, row.bottom()));
                            theme::text_in(ui.painter(), name_rect, Anchor::Left, &entry.name, Type::SMALL, if entry.is_dir { theme::TEXT_3 } else { theme::TEXT });
                            if !entry.is_dir {
                                let label = if entry.encrypted { format!("{}  \u{00B7}  locked", format::bytes(entry.size)) } else { format::bytes(entry.size) };
                                theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(row.right() - size_w - 60.0, row.top()), egui::pos2(row.right() - 12.0, row.bottom())), Anchor::Right, &label, Type::NUM, theme::TEXT_2);
                            }
                        }
                    });
                });
            }
        }
    }

    /// What the link points to: a skeleton while asking, the file once
    /// known, or the reason it cannot be fetched.
    fn file_card(&mut self, ui: &mut Ui, waiting: bool) {
        let width = ui.available_width();
        match &self.info {
            Some(Err(Error::Expired(401))) => {
                widgets::callout(ui, theme::WARN, Icon::Alert, "This server asks for a login", "Enter the account for this site. It is remembered for the site, with the password kept in your keychain.");
                return;
            }
            Some(Err(error)) => {
                let (color, title, body) = match error {
                    Error::Expired(code) => (
                        theme::WARN,
                        format!("{} (HTTP {code})", format::expiry_reason(*code)),
                        "Links from file hosts and cloud storage stop working after a while. Start the download again in your browser to get a new one.".to_owned(),
                    ),
                    other => (theme::BAD, "This address could not be reached".to_owned(), {
                        let text = other.to_string();
                        let mut chars = text.chars();
                        chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or(text) + "."
                    }),
                };
                widgets::callout(ui, color, Icon::Alert, &title, &body);
                return;
            }
            Some(Ok(_)) => {}
            None if !waiting => {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 68.0), Sense::hover());
                ui.painter().rect(rect, CornerRadius::same(theme::R_PANEL), theme::FIELD, Stroke::new(1.0, theme::white(16)), StrokeKind::Inside);
                theme::text_in(ui.painter(), rect, Anchor::Center, "Paste an address to see the file name and size", Type::SMALL, theme::TEXT_3);
                return;
            }
            None => {
                // Skeleton with the same geometry as the loaded card.
                let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 68.0), Sense::hover());
                let painter = ui.painter();
                painter.rect(rect, CornerRadius::same(theme::R_PANEL), theme::FIELD, Stroke::new(1.0, theme::white(16)), StrokeKind::Inside);
                let time = ui.input(|i| i.time) as f32;
                let shimmer = |offset: f32| theme::white((16.0 + ((time * 3.2 - offset).sin() * 0.5 + 0.5) * 16.0) as u8);
                painter.rect_filled(Rect::from_min_size(egui::pos2(rect.left() + 14.0, rect.top() + 16.0), Vec2::splat(36.0)), CornerRadius::same(8), shimmer(0.0));
                painter.rect_filled(Rect::from_min_size(egui::pos2(rect.left() + 64.0, rect.top() + 18.0), Vec2::new(width * 0.46, 11.0)), CornerRadius::same(4), shimmer(0.5));
                painter.rect_filled(Rect::from_min_size(egui::pos2(rect.left() + 64.0, rect.top() + 39.0), Vec2::new(width * 0.26, 9.0)), CornerRadius::same(4), shimmer(1.0));
                return;
            }
        }
        let Some(Ok(info)) = self.info.clone() else { return };
        let category = self.settings.category_of(&self.name);

        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 68.0), Sense::hover());
        ui.painter().rect(rect, CornerRadius::same(theme::R_PANEL), theme::FIELD, Stroke::new(1.0, theme::white(20)), StrokeKind::Inside);
        let tile = Rect::from_min_size(egui::pos2(rect.left() + 14.0, rect.top() + 16.0), Vec2::splat(36.0));
        ui.painter().rect(tile, CornerRadius::same(8), theme::RAISED, Stroke::new(1.0, theme::white(22)), StrokeKind::Inside);
        icons::paint(ui.painter(), tile, Icon::for_category(category), 17.0, theme::TEXT_2);

        // The name is editable in place.
        let name_rect = Rect::from_min_size(egui::pos2(tile.right() + 10.0, rect.top() + 10.0), Vec2::new(rect.right() - tile.right() - 24.0, theme::CONTROL_H));
        theme::region(ui, name_rect, |ui| {
            let before = self.name.clone();
            Field::new(&mut self.name).hint("File name").id(egui::Id::unique("add-name")).show(ui);
            if self.name != before {
                self.name_touched = true;
            }
        });

        // Facts, as a quiet line under the name. A size the server did not
        // announce is simply not shown.
        let mut facts: Vec<String> = Vec::new();
        if let Some(total) = info.total {
            facts.push(format::bytes(total));
        }
        if info.protocol != Protocol::Http {
            facts.push(info.protocol.label().to_owned());
        }
        if let Some(note) = &info.note {
            facts.push(note.clone());
        } else {
            // The family is more use to a person than a media type string.
            facts.push(
                match category {
                    Category::Video => "Video",
                    Category::Audio => "Audio",
                    Category::Image => "Image",
                    Category::Document => "Document",
                    Category::Archive => "Archive",
                    Category::Program => "Program",
                    Category::Other => "File",
                }
                .to_owned(),
            );
        }
        // While the user reads this, the file is already arriving.
        let arrived = self.live.as_ref().and_then(|view| match (&view.status, view.fraction) {
            (Status::Completed, _) => Some(("Already downloaded".to_owned(), theme::OK, 1.0)),
            (status, Some(fraction)) if status.is_active() && view.downloaded > 0 => Some((format!("{} downloaded already", format::percent(fraction)), theme::ACCENT_HI, fraction)),
            (Status::Queued, _) => Some(("Waiting for a free slot".to_owned(), theme::TEXT_2, 0.0)),
            _ => None,
        });
        if arrived.is_none() {
            facts.push(if info.resumable { "Can pause and resume".to_owned() } else { "Cannot resume".to_owned() });
        }
        let line = Rect::from_min_size(egui::pos2(tile.right() + 12.0, rect.top() + 44.0), Vec2::new(rect.right() - tile.right() - 26.0, 16.0));
        let used = theme::text_in(ui.painter(), line, Anchor::Left, &facts.join("  \u{00B7}  "), Type::SMALL, theme::TEXT_2);
        if let Some((text, color, fraction)) = arrived {
            let rest = Rect::from_min_max(egui::pos2(line.left() + used, line.top()), line.max);
            let dot = theme::text_in(ui.painter(), rest, Anchor::Left, "  \u{00B7}  ", Type::SMALL, theme::TEXT_3);
            theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(rest.left() + dot, rest.top()), rest.max), Anchor::Left, &text, Type::SMALL_MEDIUM, color);
            // The card's lower edge fills as the file does.
            if fraction > 0.0 {
                let track = Rect::from_min_max(egui::pos2(rect.left() + 10.0, rect.bottom() - 3.0), egui::pos2(rect.right() - 10.0, rect.bottom() - 1.0));
                ui.painter().rect_filled(track, CornerRadius::same(1), theme::white(14));
                ui.painter().rect_filled(Rect::from_min_max(track.min, egui::pos2(track.left() + track.width() * fraction.clamp(0.0, 1.0), track.bottom())), CornerRadius::same(1), color);
            }
        }
    }
}

/// A fact shown in the shape of a field: an icon and one line of mono text.
fn readonly_line(ui: &mut Ui, icon: Icon, text: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), theme::CONTROL_H), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(theme::R_CONTROL), theme::FIELD, Stroke::new(1.0, theme::white(14)), StrokeKind::Inside);
    icons::paint(ui.painter(), Rect::from_min_size(rect.min, Vec2::new(30.0, rect.height())), icon, 14.0, theme::TEXT_3);
    theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(rect.left() + 30.0, rect.top()), egui::pos2(rect.right() - 10.0, rect.bottom())), Anchor::Left, text, Type::mono(12.0, Weight::Regular), theme::TEXT_2);
}

/// One quiet line of small text.
fn small_note(ui: &mut Ui, text: &str, color: Color32) {
    let galley = theme::layout_wrapped(ui.painter(), text, Type::SMALL, color, ui.available_width(), 3);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y.max(16.0)), Sense::hover());
    ui.painter().galley(rect.min, galley, color);
}

// ------------------------------------------------------------------ Swap ----

pub struct SwapDialog {
    id: DownloadId,
    url: String,
    ticket: Option<Ticket<Result<(), Error>>>,
    error: Option<String>,
    first_frame: bool,
    /// The source page was opened and the engine is waiting for the browser
    /// to hand over the new link.
    waiting: bool,
}

impl SwapDialog {
    pub fn new(id: DownloadId) -> Self {
        Self { id, url: String::new(), ticket: None, error: None, first_frame: true, waiting: false }
    }

    fn show(&mut self, app: &mut App, ctx: &egui::Context) -> Outcome {
        let Some(view) = app.view(self.id).cloned() else { return Outcome::Close };
        if let Some(result) = self.ticket.as_ref().and_then(|t| t.take()) {
            self.ticket = None;
            match result {
                Ok(()) => {
                    app.toast("Continuing with the new link");
                    return Outcome::Close;
                }
                Err(e) => {
                    let text = e.to_string();
                    let mut chars = text.chars();
                    self.error = Some(chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or(text) + ".");
                }
            }
        }
        let checking = self.ticket.is_some();
        if checking {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
        if self.waiting {
            // The browser delivered: the download is on its way again.
            if view.status.is_active() || view.status == Status::Queued || view.status.is_finished() {
                return Outcome::Close;
            }
            self.waiting = app.manager.awaiting() == Some(self.id);
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        let mut submit = false;
        let mut open_page = false;
        let mut stop_waiting = false;
        let outcome = modal(ctx, "swap-link", 520.0, |ui| {
            if title_bar(ui, "Swap link", Some(&view.file_name)) {
                return Outcome::Close;
            }
            padded(ui, |ui| {
                let kept = match (view.fraction, view.downloaded) {
                    (Some(f), d) if d > 0 => format!("{} ({}) is already on disk and stays there.", format::bytes(d), format::percent(f)),
                    _ => "Nothing has been downloaded yet.".to_owned(),
                };
                let text = format!("Paste a new address for the same file. Siphon checks that it is the same size, then carries on. {kept}");
                let galley = theme::layout_wrapped(ui.painter(), &text, Type::BODY, theme::TEXT_2, ui.available_width(), 4);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y), Sense::hover());
                ui.painter().galley(rect.min, galley, theme::TEXT_2);
                ui.add_space(16.0);

                if self.waiting {
                    widgets::callout(
                        ui,
                        theme::ACCENT,
                        Icon::Refresh,
                        "Waiting for the browser",
                        "The page this file came from is open. Start the download there again: Siphon takes the new link and carries on from where it stopped.",
                    );
                    ui.add_space(10.0);
                    let (bar, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 3.0), Sense::hover());
                    widgets::progress(ui, bar, None, theme::ACCENT);
                    ui.add_space(16.0);
                }

                field_label(ui, if self.waiting { "OR PASTE THE NEW ADDRESS" } else { "NEW ADDRESS" });
                let before = self.url.clone();
                let response = Field::new(&mut self.url).hint("https://\u{2026}").icon(Icon::Link).mono().invalid(self.error.is_some()).id(egui::Id::unique("swap-url")).show(ui);
                if self.first_frame {
                    self.first_frame = false;
                    response.request_focus();
                }
                if self.url != before {
                    self.error = None;
                }
                if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    submit = true;
                }
                if let Some(error) = &self.error {
                    ui.add_space(12.0);
                    widgets::callout(ui, theme::BAD, Icon::Alert, "That link cannot continue this download", error);
                }
            });
            footer(ui, |ui| {
                let ready = self.url.trim().len() > 10 && !checking;
                ui.add_enabled_ui(ready, |ui| {
                    let label = if checking { "Checking\u{2026}" } else { "Check and continue" };
                    if Button::new(label, ButtonKind::Primary).min_width(160.0).show(ui).clicked() {
                        submit = true;
                    }
                });
                if Button::new("Cancel", ButtonKind::Ghost).show(ui).clicked() {
                    ui.close();
                }
                if view.referer.is_some() {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        if self.waiting {
                            if Button::new("Stop waiting", ButtonKind::Secondary).show(ui).clicked() {
                                stop_waiting = true;
                            }
                        } else if Button::new("Get it from the page", ButtonKind::Secondary)
                            .icon(Icon::External)
                            .show(ui)
                            .on_hover_text("Opens the page in your browser and waits for you to start the download there again")
                            .clicked()
                        {
                            open_page = true;
                        }
                    });
                }
            });
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        });

        if open_page && let Some(page) = &view.referer {
            // The next link the browser hands over is tried on this download first.
            app.manager.await_fresh_link(self.id);
            self.waiting = true;
            ctx.open_url(egui::OpenUrl::new_tab(page));
        }
        if stop_waiting {
            app.manager.stop_awaiting();
            self.waiting = false;
        }
        if submit && !checking && self.url.trim().len() > 10 {
            self.error = None;
            self.ticket = Some(app.manager.refresh_link(self.id, Request::new(self.url.trim())));
        }
        outcome
    }
}

// ---------------------------------------------------------------- Remove ----

pub struct RemoveDialog {
    pub ids: Vec<DownloadId>,
    pub trash: bool,
    pub any_finished: bool,
}

impl RemoveDialog {
    fn show(&mut self, app: &mut App, ctx: &egui::Context) -> Outcome {
        let names: Vec<String> = self.ids.iter().filter_map(|id| app.view(*id)).map(|v| v.file_name.clone()).collect();
        if names.is_empty() {
            return Outcome::Close;
        }
        let unfinished = self.ids.iter().filter_map(|id| app.view(*id)).filter(|v| !v.status.is_finished()).count();
        let title = match names.as_slice() {
            [one] => format!("Remove \u{201C}{one}\u{201D}?"),
            many => format!("Remove {} downloads?", many.len()),
        };

        let mut confirm = false;
        let outcome = modal(ctx, "remove", 440.0, |ui| {
            padded(ui, |ui| {
                let galley = theme::layout_wrapped(ui.painter(), &title, Type::TITLE, theme::TEXT, ui.available_width(), 3);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y), Sense::hover());
                ui.painter().galley(rect.min, galley, theme::TEXT);
                ui.add_space(8.0);

                let body = match (unfinished, self.any_finished) {
                    (0, _) => "It leaves the list. The file stays where it is unless you choose otherwise.".to_owned(),
                    (n, false) => format!("{} not finished. The part already downloaded will be deleted.", if n == 1 { "This download is".to_owned() } else { format!("{n} downloads are") }),
                    (n, true) => format!("{n} unfinished: what they have downloaded so far will be deleted. Finished files stay where they are unless you choose otherwise."),
                };
                let galley = theme::layout_wrapped(ui.painter(), &body, Type::BODY, theme::TEXT_2, ui.available_width(), 4);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y), Sense::hover());
                ui.painter().galley(rect.min, galley, theme::TEXT_2);

                if self.any_finished {
                    ui.add_space(14.0);
                    let label = if cfg!(target_os = "macos") { "Also move the finished file to the Trash" } else { "Also move the finished file to the Recycle Bin" };
                    widgets::checkbox(ui, &mut self.trash, label);
                }
            });
            footer(ui, |ui| {
                if Button::new("Remove", ButtonKind::Danger).min_width(96.0).show(ui).clicked() {
                    confirm = true;
                }
                if Button::new("Cancel", ButtonKind::Ghost).show(ui).clicked() {
                    ui.close();
                }
            });
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                confirm = true;
            }
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        });

        if confirm {
            for id in &self.ids {
                app.manager.remove(*id, self.trash);
            }
            app.selection.clear();
            app.focus = None;
            return Outcome::Close;
        }
        outcome
    }
}

// -------------------------------------------------------------- Settings ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Folders,
    Transfers,
    Sites,
    Finished,
    Network,
    Browser,
}

const TABS: [(Tab, &str, Icon); 7] = [
    (Tab::General, "General", Icon::Sliders),
    (Tab::Folders, "Folders", Icon::Folder),
    (Tab::Transfers, "Transfers", Icon::Bolt),
    (Tab::Sites, "Sites", Icon::Stack),
    (Tab::Finished, "When finished", Icon::CheckCircle),
    (Tab::Network, "Network", Icon::Link),
    (Tab::Browser, "Browser", Icon::External),
];

/// One row of the site list while it is being edited. Kept apart from the
/// settings so a half-typed row is not thrown away as invalid.
struct SiteRow {
    pattern: String,
    connections: String,
    user: String,
    /// Typed but not yet handed to the keychain.
    password: String,
    /// Where a stored password lives, if this row came from the settings.
    stored: Option<(String, String)>,
}

impl SiteRow {
    fn rule(&self) -> siphon_core::organize::SiteRule {
        siphon_core::organize::SiteRule {
            pattern: siphon_core::organize::normalize_pattern(&self.pattern),
            connections: self.connections.trim().parse::<usize>().ok().filter(|n| *n > 0),
            username: self.user.trim().to_owned(),
        }
    }
}

pub struct SettingsDialog {
    draft: Settings,
    saved: Settings,
    tab: Tab,
    dir: String,
    limit: String,
    port: String,
    changed: Option<Instant>,
    browsers: browsers::BrowserPanel,
    /// Extra extensions per family, as typed.
    extensions: Vec<String>,
    sites: Vec<SiteRow>,
}

impl SettingsDialog {
    pub fn new(manager: &Manager) -> Self {
        let draft = manager.settings();
        Self {
            extensions: draft.categories.iter().map(|rule| rule.extensions.join(" ")).collect(),
            sites: draft
                .sites
                .iter()
                .map(|rule| SiteRow {
                    pattern: rule.pattern.clone(),
                    connections: rule.connections.map(|c| c.to_string()).unwrap_or_default(),
                    user: rule.username.clone(),
                    password: String::new(),
                    stored: (!rule.username.is_empty()).then(|| (rule.pattern.clone(), rule.username.clone())),
                })
                .collect(),
            dir: draft.download_dir.to_string_lossy().into_owned(),
            limit: format!("{:.1}", draft.speed_limit as f64 / (1024.0 * 1024.0)).trim_end_matches('0').trim_end_matches('.').to_owned(),
            port: if draft.net.proxy.port == 0 { String::new() } else { draft.net.proxy.port.to_string() },
            saved: draft.clone(),
            draft,
            tab: Tab::General,
            changed: None,
            browsers: browsers::BrowserPanel::new(),
        }
    }

    /// Opens on a specific tab (used by the screenshot script).
    pub fn on_tab(mut self, index: usize) -> Self {
        self.tab = TABS[index.min(TABS.len() - 1)].0;
        self
    }

    /// Copies the text fields into the draft.
    fn sync(&mut self) {
        self.draft.download_dir = PathBuf::from(self.dir.trim());
        if let Ok(mb) = self.limit.trim().replace(',', ".").parse::<f64>()
            && mb > 0.0
        {
            self.draft.speed_limit = (mb * 1024.0 * 1024.0) as u64;
        }
        self.draft.net.proxy.port = self.port.trim().parse().unwrap_or(0);
        for (rule, text) in self.draft.categories.iter_mut().zip(&self.extensions) {
            rule.extensions = siphon_core::organize::parse_extensions(text);
        }
        self.draft.sites = self.sites.iter().map(SiteRow::rule).filter(|rule| !rule.is_inert()).collect();
    }

    /// Hands typed passwords to the keychain and forgets those of rules
    /// that are gone or changed hands.
    fn store_passwords(&mut self, app: &mut App) {
        for row in &mut self.sites {
            let rule = row.rule();
            let now = (!rule.pattern.is_empty() && !rule.username.is_empty()).then(|| (rule.pattern.clone(), rule.username.clone()));
            if row.stored != now
                && let Some((pattern, user)) = row.stored.take()
            {
                let _ = app.manager.forget_site_password(&pattern, &user);
            }
            if let Some((pattern, user)) = &now
                && !row.password.is_empty()
            {
                match app.manager.store_site_password(pattern, user, &row.password) {
                    Ok(()) => row.password.clear(),
                    Err(e) => app.toast_error(format!("The password for {pattern} could not be saved: {e}")),
                }
            }
            if now.is_some() {
                row.stored = now;
            }
        }
    }

    /// Applies the draft if it differs from what is in effect.
    fn commit(&mut self, app: &mut App) {
        self.sync();
        self.store_passwords(app);
        if self.draft != self.saved {
            match app.manager.set_settings(self.draft.clone()) {
                Ok(()) => {}
                Err(e) => app.toast_error(e.to_string()),
            }
            self.saved = app.manager.settings();
        }
        self.changed = None;
    }

    fn show(&mut self, app: &mut App, ctx: &egui::Context) -> Outcome {
        // Settings apply as they change. Typing is given a moment to finish
        // so the network stack is not rebuilt on every keystroke.
        self.sync();
        if self.draft != self.saved {
            let since = *self.changed.get_or_insert_with(Instant::now);
            // Things typed letter by letter are given a moment to settle.
            let net_changed = self.draft.net != self.saved.net
                || self.draft.download_dir != self.saved.download_dir
                || self.draft.sites != self.saved.sites
                || self.draft.categories != self.saved.categories
                || self.draft.on_complete != self.saved.on_complete;
            if !net_changed || since.elapsed() >= Duration::from_millis(700) {
                self.commit(app);
            } else {
                ctx.request_repaint_after(Duration::from_millis(120));
            }
        }

        modal(ctx, "settings", 740.0, |ui| {
            if title_bar(ui, "Settings", None) {
                return Outcome::Close;
            }
            let height = 430.0;
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
            let nav = Rect::from_min_size(rect.min, Vec2::new(168.0, height));
            ui.painter().vline(nav.right(), rect.y_range(), theme::hairline());

            theme::region(ui, nav.shrink(10.0), |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (tab, label, icon) in TABS {
                    let (row, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
                    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                    let active = self.tab == tab;
                    let t = ui.ctx().animate_bool_with_time(response.id.with("a"), active, 0.12);
                    if t > 0.0 {
                        widgets::key_surface(ui, row, theme::R_CONTROL, theme::tint(theme::HOVER, (t * 255.0) as u8), theme::white((t * 26.0) as u8), theme::white((t * 16.0) as u8));
                    } else if response.hovered() {
                        ui.painter().rect_filled(row, CornerRadius::same(theme::R_CONTROL), theme::white(10));
                    }
                    icons::paint(ui.painter(), Rect::from_min_size(egui::pos2(row.left() + 6.0, row.top()), Vec2::new(20.0, row.height())), icon, 15.0, theme::mix(theme::TEXT_3, theme::ACCENT_HI, t));
                    theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(row.left() + 34.0, row.top()), row.max), Anchor::Left, label, if active { Type::BODY_MEDIUM } else { Type::BODY }, theme::mix(theme::TEXT_2, theme::TEXT, t));
                    if response.clicked() {
                        self.tab = tab;
                    }
                }
            });

            let page = Rect::from_min_max(egui::pos2(nav.right() + 24.0, rect.top() + 10.0), egui::pos2(rect.right() - 24.0, rect.bottom()));
            theme::region(ui, page, |ui| {
                egui::ScrollArea::vertical().id_salt(("settings-page", self.tab as u8)).auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.set_width(page.width());
                    match self.tab {
                        Tab::General => self.general(ui),
                        Tab::Folders => self.folders(ui),
                        Tab::Sites => self.sites(app, ui),
                        Tab::Finished => self.finished(ui),
                        Tab::Transfers => self.transfers(ui),
                        Tab::Network => self.network(ui),
                        Tab::Browser => self.browsers.show(app, ui),
                    }
                    ui.add_space(16.0);
                });
            });
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        })
    }

    fn general(&mut self, ui: &mut Ui) {
        ui.add_space(8.0);
        field_label(ui, "DOWNLOAD FOLDER");
        ui.horizontal(|ui| {
            let w = ui.available_width() - 96.0;
            Field::new(&mut self.dir).icon(Icon::Folder).width(w).show(ui);
            if Button::new("Browse\u{2026}", ButtonKind::Secondary).min_width(88.0).show(ui).clicked()
                && let Some(dir) = rfd::FileDialog::new().set_directory(&self.dir).pick_folder()
            {
                self.dir = dir.to_string_lossy().into_owned();
            }
        });
        ui.add_space(10.0);
        widgets::setting_row(
            ui,
            "Save automatically",
            "Links from the browser start at once in the download folder. When off, Siphon shows the file first and asks where it goes.",
            40.0,
            |ui| {
                widgets::toggle(ui, &mut self.draft.save_automatically);
            },
        );
        widgets::setting_row(
            ui,
            "Start downloading while asking",
            "A link is fetched in the background from the moment it arrives, while its dialog waits for you. Accept and it is already under way, or already done; decline and what was fetched is deleted.",
            40.0,
            |ui| {
                widgets::toggle(ui, &mut self.draft.start_immediately);
            },
        );
        widgets::setting_row(ui, "Come forward on capture", "Bring this window to the front when the browser hands over a link.", 40.0, |ui| {
            widgets::toggle(ui, &mut self.draft.focus_on_capture);
        });
        widgets::setting_row(ui, "Continue at launch", "Pick up downloads that were running when Siphon was closed.", 40.0, |ui| {
            widgets::toggle(ui, &mut self.draft.resume_on_launch);
        });
        widgets::setting_row(ui, "Retry after network errors", "Try again by itself, a little later each time, before giving up.", 40.0, |ui| {
            widgets::toggle(ui, &mut self.draft.auto_retry);
        });
    }

    fn folders(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        widgets::setting_row(
            ui,
            "Sort by type",
            "File each download into the folder for its family. A folder you choose for one download always wins.",
            40.0,
            |ui| {
                widgets::toggle(ui, &mut self.draft.sort_by_type);
            },
        );
        ui.add_space(10.0);
        let sorting = self.draft.sort_by_type;
        ui.add_enabled_ui(sorting, |ui| {
            for index in 0..self.draft.categories.len() {
                let category = self.draft.categories[index].category;
                // Files of no particular family stay in the download folder.
                if category.folder_name().is_none() {
                    continue;
                }
                let folder = self.draft.category_folder(category);
                let custom = self.draft.categories[index].folder.is_some();
                let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 48.0), Sense::hover());
                ui.painter().hline(row.x_range(), row.bottom() - 0.5, Stroke::new(1.0, theme::white(10)));
                let fade = if sorting { 1.0 } else { 0.45 };
                let tile = Rect::from_min_size(egui::pos2(row.left(), row.center().y - 15.0), Vec2::splat(30.0));
                ui.painter().rect(tile, CornerRadius::same(7), theme::RAISED, Stroke::new(1.0, theme::white(22)), StrokeKind::Inside);
                icons::paint(ui.painter(), tile, Icon::for_category(category), 15.0, theme::TEXT_2.gamma_multiply(fade));
                let text_x = tile.right() + 12.0;
                let text_w = row.right() - text_x - 200.0;
                theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text_x, row.top() + 7.0), Vec2::new(text_w, 18.0)), Anchor::Left, category.label(), Type::BODY_MEDIUM, theme::TEXT.gamma_multiply(fade));
                theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text_x, row.top() + 25.0), Vec2::new(text_w, 16.0)), Anchor::Left, &format::home_relative(&folder), Type::SMALL, theme::TEXT_2.gamma_multiply(fade));

                let controls = Rect::from_min_max(egui::pos2(row.right() - 190.0, row.center().y - 14.0), egui::pos2(row.right(), row.center().y + 14.0));
                theme::place(ui, controls, egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if widgets::icon_button(ui, Icon::Folder, if custom { "Choose another folder (right-click to go back to the default)" } else { "Choose a folder" }).clicked()
                        && let Some(picked) = rfd::FileDialog::new().set_directory(&folder).pick_folder()
                    {
                        self.draft.categories[index].folder = Some(picked);
                    }
                    if custom && widgets::icon_button(ui, Icon::Restart, "Back to the default folder").clicked() {
                        self.draft.categories[index].folder = None;
                    }
                    let width = if custom { 88.0 } else { 122.0 };
                    Field::new(&mut self.extensions[index]).hint("More types").width(width).mono().id(egui::Id::unique(("ext", index))).show(ui);
                });
            }
        });
        ui.add_space(10.0);
        let (note, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
        let galley = theme::layout_wrapped(ui.painter(), "Each family already knows the usual file types. \u{201C}More types\u{201D} adds others: extensions separated by spaces, such as iso img.", Type::SMALL, theme::TEXT_3, note.width(), 2);
        ui.painter().galley(note.min, galley, theme::TEXT_3);
    }

    fn sites(&mut self, app: &mut App, ui: &mut Ui) {
        ui.add_space(8.0);
        let intro = "Rules for single sites. A connection limit suits servers that allow one or two connections per visitor. A login is sent to that site only (HTTP Basic, or the FTP account); its password is kept in your keychain.";
        let galley = theme::layout_wrapped(ui.painter(), intro, Type::SMALL, theme::TEXT_2, ui.available_width(), 4);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), galley.size().y), Sense::hover());
        ui.painter().galley(rect.min, galley, theme::TEXT_2);
        ui.add_space(14.0);

        let total = ui.available_width();
        let (w_conn, w_user, w_pass, w_del) = (56.0, 104.0, 104.0, 28.0);
        let w_site = total - w_conn - w_user - w_pass - w_del - 6.0 * 4.0;
        if !self.sites.is_empty() {
            let (head, _) = ui.allocate_exact_size(Vec2::new(total, 20.0), Sense::hover());
            let mut x = head.left();
            for (label, width) in [("SITE", w_site), ("LIMIT", w_conn), ("USER", w_user), ("PASSWORD", w_pass)] {
                theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(x + 2.0, head.top()), Vec2::new(width, head.height())), Anchor::Left, label, Type::LABEL, theme::TEXT_3);
                x += width + 6.0;
            }
        }
        let mut remove: Option<usize> = None;
        for (index, row) in self.sites.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                Field::new(&mut row.pattern).hint("files.example.com").width(w_site).mono().id(egui::Id::unique(("site", index))).show(ui);
                Field::new(&mut row.connections).hint("Any").width(w_conn).mono().id(egui::Id::unique(("site-conn", index))).show(ui);
                Field::new(&mut row.user).hint("None").width(w_user).id(egui::Id::unique(("site-user", index))).show(ui);
                let hint = if row.stored.is_some() { "Saved" } else { "None" };
                ui.add_enabled_ui(!row.user.trim().is_empty(), |ui| {
                    Field::new(&mut row.password).hint(hint).password().width(w_pass).id(egui::Id::unique(("site-pass", index))).show(ui);
                });
                if widgets::icon_button(ui, Icon::Trash, "Remove this rule").clicked() {
                    remove = Some(index);
                }
            });
            ui.add_space(6.0);
        }
        if let Some(index) = remove {
            let row = self.sites.remove(index);
            if let Some((pattern, user)) = row.stored {
                let _ = app.manager.forget_site_password(&pattern, &user);
            }
        }
        if self.sites.is_empty() {
            let (empty, _) = ui.allocate_exact_size(Vec2::new(total, 64.0), Sense::hover());
            ui.painter().rect(empty, CornerRadius::same(theme::R_PANEL), theme::FIELD, Stroke::new(1.0, theme::white(16)), StrokeKind::Inside);
            theme::text_in(ui.painter(), empty, Anchor::Center, "No site has rules of its own yet", Type::SMALL, theme::TEXT_3);
            ui.add_space(10.0);
        }
        ui.add_space(4.0);
        if Button::new("Add a site", ButtonKind::Secondary).icon(Icon::Plus).show(ui).clicked() {
            self.sites.push(SiteRow { pattern: String::new(), connections: String::new(), user: String::new(), password: String::new(), stored: None });
        }
    }

    fn finished(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        widgets::setting_row(ui, "Show a card", "A finished download appears in the corner with Open and Show buttons.", 40.0, |ui| {
            widgets::toggle(ui, &mut self.draft.notify_on_complete);
        });
        let (title, detail) = if cfg!(target_os = "macos") {
            ("Mark as downloaded", "As a browser does: macOS checks apps and installers before they first run, and Finder shows where the file came from.")
        } else {
            ("Mark as downloaded", "Record on the file that it came from the internet, where the system supports it.")
        };
        widgets::setting_row(ui, title, detail, 40.0, |ui| {
            widgets::toggle(ui, &mut self.draft.mark_downloaded);
        });
        widgets::setting_row(ui, "Keep the server\u{2019}s date", "Give the file the modification date the server reports, instead of the time it was downloaded.", 40.0, |ui| {
            widgets::toggle(ui, &mut self.draft.keep_server_date);
        });
        widgets::setting_row_with(ui, "Run a command", "For every finished file. [File] stands for the file; without it the file is added at the end.", 0.0, false, |_| {});
        Field::new(&mut self.draft.on_complete).hint("open -a \"The Unarchiver\" [File]").mono().id(egui::Id::unique("on-complete")).show(ui);
    }

    fn transfers(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        widgets::setting_row(ui, "Downloads at once", "The rest wait in the queue and start as slots free up.", 104.0, |ui| {
            widgets::stepper(ui, &mut self.draft.max_active, 1, siphon_core::settings::MAX_ACTIVE);
        });
        widgets::setting_row(
            ui,
            "Connections per download",
            "Each file is fetched in this many parts at once. Idle connections take over half of the largest part still running.",
            104.0,
            |ui| {
                widgets::stepper(ui, &mut self.draft.connections, 1, siphon_core::settings::MAX_CONNECTIONS);
            },
        );
        widgets::setting_row(ui, "Video quality", "The picture quality chosen first for video pages such as YouTube. A page that does not have it gets the best one below; you can change it per download.", 110.0, |ui| {
            let options = [(2160u32, "2160p (4K)"), (1440, "1440p"), (1080, "1080p"), (720, "720p"), (480, "480p"), (360, "360p")];
            // A value set by hand that is not in the list reads as the nearest choice below it.
            let mut shown = options.iter().map(|(h, _)| *h).find(|h| *h <= self.draft.video_quality).unwrap_or(360);
            if widgets::dropdown(ui, "video-quality", &options, &mut shown, 110.0) {
                self.draft.video_quality = shown;
            }
        });
        widgets::setting_row(ui, if cfg!(target_os = "macos") { "Keep the Mac awake" } else { "Keep the computer awake" }, "Hold off sleep while downloads are running.", 40.0, |ui| {
            widgets::toggle(ui, &mut self.draft.keep_awake);
        });
        widgets::setting_row(ui, "Limit speed", "Cap the total across all downloads.", 40.0, |ui| {
            widgets::toggle(ui, &mut self.draft.speed_limit_enabled);
        });
        ui.add_enabled_ui(self.draft.speed_limit_enabled, |ui| {
            widgets::setting_row(ui, "Maximum speed", "In megabytes per second.", 104.0, |ui| {
                let (unit, _) = ui.allocate_exact_size(Vec2::new(38.0, 20.0), Sense::hover());
                theme::text_in(ui.painter(), unit, Anchor::Right, "MB/s", Type::SMALL, theme::TEXT_2);
                Field::new(&mut self.limit).width(60.0).mono().show(ui);
            });
        });
    }

    fn network(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        let proxy = &mut self.draft.net.proxy;
        widgets::setting_row(ui, "Proxy", "How Siphon reaches the internet.", 222.0, |ui| {
            widgets::segmented(ui, "proxy-mode", &[(ProxyMode::System, "System"), (ProxyMode::Direct, "None"), (ProxyMode::Manual, "Manual")], &mut proxy.mode);
        });
        let manual = proxy.mode == ProxyMode::Manual;
        ui.add_enabled_ui(manual, |ui| {
            widgets::setting_row(ui, "Server", "Host and port of the proxy.", 292.0, |ui| {
                Field::new(&mut self.port).hint("8080").width(64.0).mono().show(ui);
                Field::new(&mut proxy.host).hint("proxy.example.com").width(128.0).show(ui);
                let options: Vec<(ProxyKind, &str)> = ProxyKind::ALL.iter().map(|k| (*k, k.label())).collect();
                widgets::dropdown(ui, "proxy-kind", &options, &mut proxy.kind, 88.0);
            });
            widgets::setting_row(
                ui,
                "Authentication",
                "NTLM, Negotiate and Kerberos are handled for HTTP proxies. Kerberos locates the domain controller through DNS.",
                132.0,
                |ui| {
                    let options: Vec<(ProxyAuth, &str)> = ProxyAuth::ALL.iter().map(|a| (*a, a.label())).collect();
                    widgets::dropdown(ui, "proxy-auth", &options, &mut proxy.auth, 132.0);
                },
            );
            ui.add_enabled_ui(proxy.auth != ProxyAuth::None, |ui| {
                widgets::setting_row(ui, "Account", "User name and password.", 292.0, |ui| {
                    Field::new(&mut proxy.password).hint("Password").password().width(142.0).show(ui);
                    Field::new(&mut proxy.username).hint("User").width(142.0).show(ui);
                });
                if proxy.auth.needs_bridge() {
                    widgets::setting_row(ui, "Domain", "Windows domain or Kerberos realm, if the account needs one.", 200.0, |ui| {
                        Field::new(&mut proxy.domain).hint("CORP").width(200.0).show(ui);
                    });
                }
            });
            widgets::setting_row(ui, "Bypass", "Hosts reached directly, separated by commas.", 292.0, |ui| {
                Field::new(&mut proxy.bypass).hint("intranet.example, 10.0.0.0/8").width(292.0).show(ui);
            });
        });
        widgets::setting_row(ui, "Browser identity", "The user agent sent when the browser did not supply one. Empty uses a current Chrome.", 292.0, |ui| {
            Field::new(&mut self.draft.net.user_agent).hint("Default").width(292.0).show(ui);
        });
    }
}

/// Where the unpacked extension lives: inside the application bundle, or in
/// the source tree when running a development build.
pub fn extension_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = exe.parent()?;
    [dir.join("../Resources/extension"), dir.join("extension"), dir.join("../../extension")]
        .into_iter()
        .filter_map(|p| p.canonicalize().ok())
        .find(|p| p.join("manifest.json").is_file())
}
