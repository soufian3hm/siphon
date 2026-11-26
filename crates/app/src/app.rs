//! Application state and the frame loop.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Key, Modifiers, Rect, Sense, Stroke, Vec2};
use siphon_core::grab::LinkBatch;
use siphon_core::schedule::WhenDone;
use siphon_core::{Capture, Category, Details, DownloadId, DownloadView, Event, Manager, Status, batch};

use crate::dialogs::{self, Dialog};
use crate::icons::Icon;
use crate::theme::{self, Anchor, Type};
use crate::{format, inspector, list, shots, sidebar};

/// Which downloads the list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Active,
    /// Set aside for the queue, or being run by it.
    Scheduled,
    Completed,
    /// Expired links and failures: things only the user can unblock.
    Attention,
    Category(Category),
}

impl Filter {
    pub fn title(self) -> &'static str {
        match self {
            Filter::All => "All downloads",
            Filter::Active => "In progress",
            Filter::Scheduled => "Scheduled",
            Filter::Completed => "Completed",
            Filter::Attention => "Needs attention",
            Filter::Category(c) => c.label(),
        }
    }

    pub fn matches(self, view: &DownloadView) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => !view.status.is_finished() && !needs_attention(&view.status) && view.status != Status::Scheduled,
            Filter::Scheduled => view.status == Status::Scheduled || (view.queue_run && !view.status.is_finished()),
            Filter::Completed => view.status.is_finished(),
            Filter::Attention => needs_attention(&view.status),
            Filter::Category(c) => view.category == c,
        }
    }
}

pub fn needs_attention(status: &Status) -> bool {
    matches!(status, Status::NeedsRefresh { .. } | Status::Failed { .. })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Added,
    Name,
    Size,
    Progress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

/// Something the user asked for. Collected while drawing, applied after.
#[derive(Clone, Debug)]
pub enum Action {
    NewDownload(Option<String>),
    Pause(Vec<DownloadId>),
    Resume(Vec<DownloadId>),
    AskRemove(Vec<DownloadId>),
    Restart(DownloadId),
    SwapLink(DownloadId),
    Open(DownloadId),
    Reveal(DownloadId),
    CopyLink(DownloadId),
    CopyPath(DownloadId),
    OpenSource(DownloadId),
    MoveTo(DownloadId),
    PauseAll,
    ResumeAll,
    ClearCompleted,
    Settings,
    /// Add many downloads, starting from this text.
    Batch(String),
    /// Explore a page for links.
    GrabLinks(String),
    ImportLinks,
    ExportLinks(Vec<DownloadId>),
    /// Set aside for the queue.
    Later(Vec<DownloadId>),
    StartQueue,
    StopQueue,
    Schedule,
    Verify(DownloadId),
    Login(DownloadId),
    /// Try a stalled download again with the request it has.
    Retry(DownloadId),
}

/// A finished download, announced in the corner of the window.
pub struct DoneCard {
    pub id: DownloadId,
    pub name: String,
    pub path: std::path::PathBuf,
    pub born: Instant,
}

/// The pause between the queue finishing and its end-of-queue action, in
/// which the user can still say no.
pub struct Countdown {
    pub action: WhenDone,
    pub until: Instant,
    pub completed: usize,
    pub failed: usize,
}

pub struct Toast {
    pub text: String,
    pub born: Instant,
    pub bad: bool,
}

/// Byte map and speed history of the inspected download. Refreshed a few
/// times a second rather than every frame: for a stopped download it comes
/// from a file on disk.
#[derive(Default)]
pub struct DetailsCache {
    id: Option<DownloadId>,
    at: Option<Instant>,
    pub details: Details,
}

impl DetailsCache {
    fn refresh(&mut self, manager: &Manager, id: Option<DownloadId>, active: bool) {
        let stale = self.at.is_none_or(|at| at.elapsed() >= Duration::from_millis(if active { 120 } else { 900 }));
        if self.id != id || stale {
            self.id = id;
            self.at = Some(Instant::now());
            self.details = id.and_then(|id| manager.details(id)).unwrap_or_default();
        }
    }
}

pub struct App {
    pub manager: Arc<Manager>,
    pub views: Vec<DownloadView>,
    pub filter: Filter,
    pub search: String,
    pub sort: Sort,
    pub selection: BTreeSet<DownloadId>,
    /// The download the inspector shows and the keyboard moves from.
    pub focus: Option<DownloadId>,
    pub dialog: Option<Dialog>,
    pub toasts: Vec<Toast>,
    pub actions: Vec<Action>,
    pub details: DetailsCache,
    /// Ids currently visible in the list, in display order.
    pub visible: Vec<DownloadId>,
    pub focus_search: bool,
    /// Captured links waiting to be confirmed, each with the download that
    /// is already fetching it in the background, if any.
    pub pending: std::collections::VecDeque<(Capture, Option<DownloadId>)>,
    /// Pages of links sent by the browser, waiting for the link picker.
    pub pending_links: std::collections::VecDeque<LinkBatch>,
    pub cards: Vec<DoneCard>,
    pub countdown: Option<Countdown>,
    /// A download finished since the last frame.
    announce_finished: bool,
    script: Option<shots::Script>,
}

impl App {
    pub fn new(manager: Arc<Manager>, script: Option<shots::Script>) -> Self {
        let views = manager.snapshot();
        Self {
            manager,
            views,
            filter: Filter::All,
            search: String::new(),
            sort: Sort { key: SortKey::Added, descending: true },
            selection: BTreeSet::new(),
            focus: None,
            dialog: None,
            toasts: Vec::new(),
            actions: Vec::new(),
            details: DetailsCache::default(),
            visible: Vec::new(),
            focus_search: false,
            pending: std::collections::VecDeque::new(),
            pending_links: std::collections::VecDeque::new(),
            cards: Vec::new(),
            countdown: None,
            announce_finished: false,
            script,
        }
    }

    /// What a fresh copy does before anything else: offers to move itself
    /// out of its disk image, or, once in place, shows how to add the
    /// extension to the browsers it found, the first time only.
    pub fn first_run(&mut self, needs_installing: bool) {
        if self.script_running() || std::env::var_os("SIPHON_DATA_DIR").is_some() {
            return;
        }
        if needs_installing {
            self.dialog = Some(Dialog::Install(dialogs::InstallDialog::default()));
            return;
        }
        let mut settings = self.manager.settings();
        if !settings.browser_setup_seen {
            settings.browser_setup_seen = true;
            let _ = self.manager.set_settings(settings);
            if siphon_core::browser::present().iter().any(|b| !b.has_extension()) {
                self.dialog = Some(Dialog::Settings(Box::new(dialogs::SettingsDialog::new(&self.manager).on_tab(6))));
            }
        }
    }

    fn script_running(&self) -> bool {
        self.script.is_some()
    }

    pub fn view(&self, id: DownloadId) -> Option<&DownloadView> {
        self.views.iter().find(|v| v.id == id)
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), born: Instant::now(), bad: false });
    }

    pub fn toast_error(&mut self, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), born: Instant::now(), bad: true });
    }

    pub fn select_only(&mut self, id: DownloadId) {
        self.selection.clear();
        self.selection.insert(id);
        self.focus = Some(id);
    }

    /// The downloads an action applies to: the selection, or the focus.
    pub fn targets(&self) -> Vec<DownloadId> {
        if self.selection.is_empty() { self.focus.into_iter().collect() } else { self.selection.iter().copied().collect() }
    }

    /// Shows the next captured link, once nothing else is on screen. Links
    /// that arrive while a dialog is open wait their turn.
    fn present_next_capture(&mut self) {
        if self.dialog.is_none()
            && let Some((capture, provisional)) = self.pending.pop_front()
        {
            self.dialog = Some(Dialog::Add(Box::new(dialogs::AddDialog::for_capture(&self.manager, capture, provisional))));
        }
        if self.dialog.is_none()
            && let Some(batch) = self.pending_links.pop_front()
        {
            self.dialog = Some(Dialog::Grab(Box::new(dialogs::GrabDialog::for_batch(&self.manager, batch))));
        }
    }

    /// Closes whatever dialog is open, as if the user had dismissed it.
    pub fn close_dialog(&mut self) {
        if let Some(dialog) = self.dialog.take() {
            dialogs::dismiss(self, dialog);
        }
    }

    /// Reacts to what happened in the engine since the last frame. Returns
    /// whether the window should come to the front.
    fn absorb_events(&mut self) -> bool {
        let mut raise = false;
        for event in self.manager.drain_events() {
            match event {
                Event::Captured { capture, provisional } => {
                    raise |= self.manager.settings().focus_on_capture;
                    self.pending.push_back((capture, provisional));
                }
                Event::AlreadyDownloading { id, name } => {
                    self.select_only(id);
                    self.toast(format!("{name} is already in your list"));
                }
                Event::Notice { text, bad } => {
                    if bad {
                        self.toast_error(text);
                    } else {
                        self.toast(text);
                    }
                }
                Event::Links(batch) => {
                    raise = true;
                    self.pending_links.push_back(batch);
                }
                Event::QueueStarted { count, by_schedule } => {
                    let what = if count == 1 { "1 download".to_owned() } else { format!("{count} downloads") };
                    self.toast(if by_schedule { format!("The queue started on schedule: {what}") } else { format!("The queue started: {what}") });
                }
                Event::QueueFinished { when_done, completed, failed } => {
                    if completed + failed == 0 {
                        continue;
                    }
                    let summary = match (completed, failed) {
                        (c, 0) => format!("The queue is done: {c} finished"),
                        (c, f) => format!("The queue is done: {c} finished, {f} need attention"),
                    };
                    if when_done == WhenDone::Nothing {
                        self.toast(summary);
                    } else {
                        // Half a minute to say no before the computer sleeps or the app quits.
                        self.countdown = Some(Countdown { action: when_done, until: Instant::now() + Duration::from_secs(30), completed, failed });
                        raise = true;
                    }
                }
                Event::Raise => raise = true,
                Event::AutoStarted { id, name } => {
                    self.select_only(id);
                    self.toast(format!("Downloading {name}"));
                }
                Event::AutoRefreshed { id, name } => {
                    self.select_only(id);
                    self.toast(format!("Fresh link received: {name} continues"));
                }
                Event::Completed { id, name, path } => {
                    if self.manager.settings().notify_on_complete {
                        self.cards.retain(|card| card.id != id);
                        self.cards.push(DoneCard { id, name, path, born: Instant::now() });
                        self.announce_finished = true;
                    } else {
                        self.toast(format!("{name} finished"));
                    }
                }
                Event::Failed { name, message, .. } => self.toast_error(format!("{name}: {message}")),
                Event::Expired { id, name } => {
                    self.toast_error(format!("The link for {name} expired"));
                    if self.focus.is_none() {
                        self.select_only(id);
                    }
                }
            }
        }
        raise
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::NewDownload(url) => {
                self.close_dialog();
                self.dialog = Some(Dialog::Add(Box::new(dialogs::AddDialog::new(&self.manager, url.unwrap_or_default()))));
            }
            Action::Batch(text) => {
                self.close_dialog();
                self.dialog = Some(Dialog::Batch(Box::new(dialogs::BatchDialog::new(&self.manager, text))));
            }
            Action::GrabLinks(url) => {
                self.close_dialog();
                self.dialog = Some(Dialog::Grab(Box::new(dialogs::GrabDialog::new(&self.manager, url))));
            }
            Action::ImportLinks => match dialogs::pick_links_file() {
                Some(Ok(text)) => {
                    let links = batch::extract_links(&text);
                    if links.is_empty() {
                        self.toast("That file does not contain any links");
                    } else {
                        self.actions.push(Action::Batch(links.join("\n")));
                    }
                }
                Some(Err(e)) => self.toast_error(e),
                None => {}
            },
            Action::ExportLinks(ids) => {
                let text = self.manager.export_links(&ids);
                if text.is_empty() {
                    self.toast("There is nothing to export");
                } else if let Some(path) = rfd::FileDialog::new().set_title("Export links").set_file_name("siphon-links.txt").save_file() {
                    match std::fs::write(&path, &text) {
                        Ok(()) => self.toast(format!("Exported {} links", text.lines().count())),
                        Err(e) => self.toast_error(format!("Could not write the file: {e}")),
                    }
                }
            }
            Action::Later(ids) => {
                let n = ids.len();
                self.manager.schedule(&ids);
                self.toast(if n == 1 { "Kept for the queue".to_owned() } else { format!("Kept {n} downloads for the queue") });
            }
            Action::StartQueue => {
                if self.manager.start_queue() == 0 {
                    self.toast("Nothing is waiting in the queue");
                }
            }
            Action::StopQueue => {
                self.manager.stop_queue();
                self.toast("The queue was stopped");
            }
            Action::Schedule => {
                self.close_dialog();
                self.dialog = Some(Dialog::Schedule(dialogs::ScheduleDialog::new(&self.manager)));
            }
            Action::Verify(id) => {
                self.close_dialog();
                self.dialog = Some(Dialog::Verify(dialogs::VerifyDialog::new(&self.manager, id)));
            }
            Action::Retry(id) => self.manager.retry(id),
            Action::Login(id) => {
                if let Some(url) = self.view(id).map(|v| v.url.clone()) {
                    self.close_dialog();
                    self.dialog = Some(Dialog::Login(dialogs::LoginDialog::new(id, &url)));
                }
            }
            Action::Pause(ids) => ids.into_iter().for_each(|id| self.manager.pause(id)),
            Action::Resume(ids) => ids.into_iter().for_each(|id| self.manager.resume(id)),
            Action::AskRemove(ids) if !ids.is_empty() => {
                let any_finished = ids.iter().filter_map(|id| self.view(*id)).any(|v| v.status.is_finished());
                self.dialog = Some(Dialog::Remove(dialogs::RemoveDialog { ids, trash: false, any_finished }));
            }
            Action::AskRemove(_) => {}
            Action::Restart(id) => self.manager.restart(id),
            Action::SwapLink(id) => self.dialog = Some(Dialog::Swap(dialogs::SwapDialog::new(id))),
            Action::Open(id) => {
                if let Some(view) = self.view(id) {
                    let path = view.path();
                    if !view.status.is_finished() {
                        self.toast("This download has not finished yet");
                    } else if !path.exists() {
                        self.toast_error("The file is no longer where it was saved");
                    } else if let Err(e) = open::that_detached(&path) {
                        self.toast_error(format!("Could not open the file: {e}"));
                    }
                }
            }
            Action::Reveal(id) => {
                if let Some(view) = self.view(id) {
                    // An unfinished file is where it is being written, which
                    // may not yet be where it is going.
                    let path = if view.status.is_finished() { view.path() } else { view.disk_path() };
                    let target = if path.exists() { path } else { siphon_core::filename::part_path(&path) };
                    if let Err(e) = reveal(&target, &view.dir) {
                        self.toast_error(format!("Could not show the file: {e}"));
                    }
                }
            }
            Action::CopyLink(id) => {
                if let Some(view) = self.view(id) {
                    ctx.copy_text(view.url.clone());
                    self.toast("Link copied");
                }
            }
            Action::CopyPath(id) => {
                if let Some(view) = self.view(id) {
                    ctx.copy_text(view.path().to_string_lossy().into_owned());
                    self.toast("Path copied");
                }
            }
            Action::OpenSource(id) => {
                if let Some(page) = self.view(id).and_then(|v| v.referer.clone()) {
                    ctx.open_url(egui::OpenUrl::new_tab(page));
                }
            }
            Action::MoveTo(id) => {
                let Some(view) = self.view(id) else { return };
                if view.status.is_active() {
                    self.toast("Pause the download before moving it");
                    return;
                }
                if let Some(dir) = rfd::FileDialog::new().set_directory(&view.dir).set_title("Move to").pick_folder() {
                    match self.manager.move_to(id, &dir) {
                        Ok(()) => self.toast(format!("Moved to {}", dir.display())),
                        Err(e) => self.toast_error(e.to_string()),
                    }
                }
            }
            Action::PauseAll => self.manager.pause_all(),
            Action::ResumeAll => self.manager.resume_all(),
            Action::ClearCompleted => {
                let n = self.manager.clear_completed();
                self.toast(match n {
                    0 => "Nothing to clear".to_owned(),
                    1 => "Removed 1 finished download from the list".to_owned(),
                    n => format!("Removed {n} finished downloads from the list"),
                });
            }
            Action::Settings => {
                self.close_dialog();
                self.dialog = Some(Dialog::Settings(Box::new(dialogs::SettingsDialog::new(&self.manager))));
            }
        }
    }

    /// Keyboard shortcuts that act on the list. Ignored while a dialog is
    /// open or a text field has the keyboard.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.dialog.is_some() {
            return;
        }
        let typing = ctx.text_edit_focused();
        let (many, grab, new, settings, find) = ctx.input_mut(|i| {
            (
                // The more specific chords first: they share a key with the plain ones.
                i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::N),
                i.consume_key(Modifiers::COMMAND, Key::G),
                i.consume_key(Modifiers::COMMAND, Key::N),
                i.consume_key(Modifiers::COMMAND, Key::Comma),
                i.consume_key(Modifiers::COMMAND, Key::F),
            )
        });
        if many {
            self.actions.push(Action::Batch(String::new()));
        }
        if grab {
            self.actions.push(Action::GrabLinks(String::new()));
        }
        if new {
            self.actions.push(Action::NewDownload(None));
        }
        if settings {
            self.actions.push(Action::Settings);
        }
        if find {
            self.focus_search = true;
        }
        if typing {
            return;
        }

        // A pasted address starts a download.
        let pasted: Option<String> = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Paste(text) => Some(text.clone()),
                _ => None,
            })
        });
        if let Some(text) = pasted {
            let links = extract_links(&text);
            match links.as_slice() {
                [] => self.toast("The clipboard does not contain a link"),
                // One plain address: the usual dialog.
                [one] if !batch::is_pattern(one) => self.actions.push(Action::NewDownload(Some(one.clone()))),
                // Several, or a numbered series: look at the list before it starts.
                _ => self.actions.push(Action::Batch(links.join("\n"))),
            }
        }

        let targets = self.targets();
        let (space, delete, enter, copy, escape, up, down, all) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Space),
                i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::COMMAND, Key::A),
            )
        });
        if space && !targets.is_empty() {
            let any_running = targets.iter().filter_map(|id| self.view(*id)).any(|v| v.status.can_pause());
            self.actions.push(if any_running { Action::Pause(targets.clone()) } else { Action::Resume(targets.clone()) });
        }
        if delete {
            self.actions.push(Action::AskRemove(targets.clone()));
        }
        if enter && let Some(id) = self.focus {
            self.actions.push(Action::Open(id));
        }
        if copy && let Some(id) = self.focus {
            self.actions.push(Action::CopyLink(id));
        }
        if escape {
            self.selection.clear();
            self.focus = None;
        }
        if all {
            self.selection = self.visible.iter().copied().collect();
        }
        if (up || down) && !self.visible.is_empty() {
            let at = self.focus.and_then(|id| self.visible.iter().position(|v| *v == id));
            let next = match (at, down) {
                (None, _) => 0,
                (Some(i), true) => (i + 1).min(self.visible.len() - 1),
                (Some(i), false) => i.saturating_sub(1),
            };
            self.select_only(self.visible[next]);
        }
    }
}

/// Pulls every address out of pasted text.
pub fn extract_links(text: &str) -> Vec<String> {
    batch::extract_links(text)
}

/// Shows a file in the platform's file manager.
fn reveal(path: &Path, dir: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = dir;
        std::process::Command::new("/usr/bin/open").arg("-R").arg(path).spawn().map(|_| ())
    }
    #[cfg(target_os = "windows")]
    {
        let _ = dir;
        std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn().map(|_| ())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = path;
        open::that_detached(dir)
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.absorb_events() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            // The window is drawn again even if it was hidden a moment ago.
            ctx.request_repaint();
        }
        if std::mem::take(&mut self.announce_finished) && !ctx.input(|i| i.focused) {
            // The window is in the background: let the Dock say it.
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Informational));
        }
        self.present_next_capture();
        self.views = self.manager.snapshot();
        let alive: BTreeSet<DownloadId> = self.views.iter().map(|v| v.id).collect();
        self.selection.retain(|id| alive.contains(id));
        if self.focus.is_some_and(|id| !alive.contains(&id)) {
            self.focus = None;
        }
        let focus_active = self.focus.and_then(|id| self.view(id)).is_some_and(|v| v.status.is_active());
        self.details.refresh(&self.manager, self.focus, focus_active);
        if let Some(script) = self.script.as_mut() {
            script.before_frame(ctx, &self.manager);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.shortcuts(&ctx);

        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0, theme::BG);

        let inspector_open = self.focus.is_some();
        let inspector_t = ctx.animate_bool_with_time(egui::Id::unique("inspector-open"), inspector_open, 0.16);
        let inspector_w = theme::INSPECTOR_W * ease_out(inspector_t);
        let side = Rect::from_min_size(full.min, Vec2::new(theme::SIDEBAR_W, full.height()));
        let right = Rect::from_min_max(egui::pos2(full.right() - inspector_w, full.top()), full.max);
        let centre = Rect::from_min_max(side.right_top(), right.left_bottom());

        // Dragging the top strip moves the window; double-click zooms it.
        let strip = Rect::from_min_size(full.min, Vec2::new(full.width(), theme::TITLEBAR_H));
        let drag = ui.interact(strip, egui::Id::unique("titlebar"), Sense::click_and_drag());
        if drag.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }

        ui.painter().rect_filled(side, 0, theme::PANEL);
        theme::region(ui, side, |ui| sidebar::show(self, ui));
        theme::region(ui, centre, |ui| list::show(self, ui));
        if inspector_w > 1.0 {
            // The panel slides in at full width so its contents do not reflow.
            let panel = Rect::from_min_size(right.left_top(), Vec2::new(theme::INSPECTOR_W, full.height()));
            ui.painter().rect_filled(right, 0, theme::PANEL);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(panel).layout(egui::Layout::top_down(egui::Align::Min)));
            child.set_clip_rect(right);
            child.set_opacity(inspector_t);
            inspector::show(self, &mut child);
            ui.painter().vline(right.left() + 0.5, full.y_range(), theme::hairline());
        }
        ui.painter().vline(side.right() - 0.5, full.y_range(), theme::hairline());

        dialogs::show(self, &ctx);
        self.show_cards(&ctx, Rect::from_min_max(full.min, egui::pos2(right.left(), full.bottom())));
        self.show_countdown(&ctx);
        self.paint_toasts(&ctx, full);

        for action in std::mem::take(&mut self.actions) {
            self.apply(&ctx, action);
        }

        // Numbers tick four times a second while anything is moving.
        if self.views.iter().any(|v| v.status.is_active() || v.retry_in.is_some()) || !self.cards.is_empty() || self.countdown.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if let Some(mut script) = self.script.take() {
            script.after_frame(self, &ctx);
            self.script = Some(script);
        }
    }
}

/// Asks the operating system to sleep.
fn sleep_now() -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("/usr/bin/pmset");
        c.arg("sleepnow");
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("rundll32.exe");
        c.args(["powrprof.dll,SetSuspendState", "0,1,0"]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut c = std::process::Command::new("systemctl");
        c.arg("suspend");
        c
    };
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().map(|_| ())
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

impl App {
    /// Finished downloads, stacked in the lower right corner. Each stays for
    /// a while, longer if the pointer rests on it.
    fn show_cards(&mut self, ctx: &egui::Context, full: Rect) {
        const LIFE: f32 = 9.0;
        const SIZE: Vec2 = Vec2::new(332.0, 68.0);
        self.cards.retain(|card| card.born.elapsed().as_secs_f32() < LIFE);
        if self.cards.len() > 4 {
            let extra = self.cards.len() - 4;
            self.cards.drain(..extra);
        }
        let mut dismiss: Option<DownloadId> = None;
        let mut actions: Vec<Action> = Vec::new();
        let mut y = full.bottom() - 16.0;
        for index in (0..self.cards.len()).rev() {
            let (id, name, exists) = (self.cards[index].id, self.cards[index].name.clone(), self.cards[index].path.exists());
            let age = self.cards[index].born.elapsed().as_secs_f32();
            let alpha = (age / 0.18).min(1.0) * ((LIFE - age) / 0.35).clamp(0.0, 1.0);
            let slide = (1.0 - ease_out((age / 0.24).min(1.0))) * 14.0;
            let pos = egui::pos2(full.right() - 16.0 - SIZE.x + slide, y - SIZE.y);
            let size = self.view(id).and_then(|v| v.total).map(format::bytes);
            let area = egui::Area::new(egui::Id::unique(("done-card", id))).order(egui::Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
                ui.set_opacity(alpha);
                let (rect, _) = ui.allocate_exact_size(SIZE, Sense::hover());
                let painter = ui.painter().clone();
                let painter = &painter;
                painter.rect_filled(rect.translate(Vec2::new(0.0, 8.0)).expand(3.0), CornerRadius::same(14), Color32::from_black_alpha(90));
                painter.rect(rect, CornerRadius::same(10), theme::RAISED, Stroke::new(1.0, theme::white(34)), egui::StrokeKind::Inside);
                painter.hline((rect.left() + 8.0)..=(rect.right() - 8.0), rect.top() + 1.5, Stroke::new(1.0, theme::white(16)));
                // The check sits in a wash of its own colour.
                let badge = Rect::from_center_size(egui::pos2(rect.left() + 28.0, rect.top() + 24.0), Vec2::splat(26.0));
                painter.circle_filled(badge.center(), 13.0, theme::tint(theme::OK, 40));
                crate::icons::paint(painter, badge, Icon::Check, 14.0, theme::OK);

                let text_x = rect.left() + 52.0;
                let text_w = rect.right() - text_x - 36.0;
                theme::text_in(painter, Rect::from_min_size(egui::pos2(text_x, rect.top() + 9.0), Vec2::new(text_w, 18.0)), Anchor::Left, &name, Type::BODY_MEDIUM, theme::TEXT);
                let line = match &size {
                    Some(size) => format!("Finished  \u{00B7}  {size}"),
                    None => "Finished".to_owned(),
                };
                theme::text_in(painter, Rect::from_min_size(egui::pos2(text_x, rect.top() + 27.0), Vec2::new(text_w, 16.0)), Anchor::Left, &line, Type::SMALL, theme::TEXT_2);

                theme::region(ui, Rect::from_min_size(egui::pos2(rect.right() - 30.0, rect.top() + 6.0), Vec2::splat(24.0)), |ui| {
                    if crate::widgets::icon_button_sized(ui, Icon::Close, "", 24.0, theme::TEXT_3).clicked() {
                        dismiss = Some(id);
                    }
                });
                // Two quiet text buttons under the lines.
                let mut x = text_x - 6.0;
                for (label, action) in [("Open", Action::Open(id)), (if cfg!(target_os = "macos") { "Show in Finder" } else { "Show in folder" }, Action::Reveal(id))] {
                    let w = theme::measure(painter, label, Type::SMALL_MEDIUM) + 12.0;
                    let slot = Rect::from_min_size(egui::pos2(x, rect.bottom() - 25.0), Vec2::new(w, 20.0));
                    let response = ui.interact(slot, ui.scope_id().with(label), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                    let enabled = exists;
                    if response.hovered() && enabled {
                        ui.painter().rect_filled(slot, CornerRadius::same(5), theme::white(16));
                    }
                    theme::text_in(ui.painter(), slot, Anchor::Center, label, Type::SMALL_MEDIUM, if !enabled { theme::TEXT_3 } else if response.hovered() { theme::TEXT } else { theme::ACCENT_HI });
                    if response.clicked() && enabled {
                        actions.push(action);
                        dismiss = Some(id);
                    }
                    x += w + 4.0;
                }
            });
            // Resting the pointer on a card keeps it from leaving.
            if area.response.contains_pointer() && age > 0.4 && age < LIFE - 1.0 {
                self.cards[index].born = Instant::now() - Duration::from_secs_f32(0.5);
            }
            y -= SIZE.y + 8.0;
        }
        if let Some(id) = dismiss {
            self.cards.retain(|card| card.id != id);
        }
        self.actions.extend(actions);
    }

    /// The queue has finished and something is about to happen to the
    /// computer or the application: say so, count down, and take no for an
    /// answer.
    fn show_countdown(&mut self, ctx: &egui::Context) {
        let Some(countdown) = &self.countdown else { return };
        let left = countdown.until.saturating_duration_since(Instant::now());
        let (action, completed, failed) = (countdown.action, countdown.completed, countdown.failed);
        let mut cancel = false;
        let mut now = left.is_zero();
        if !now {
            let (verb, button) = match action {
                WhenDone::Sleep => (if cfg!(target_os = "macos") { "This Mac goes to sleep" } else { "This computer goes to sleep" }, "Sleep now"),
                _ => ("Siphon quits", "Quit now"),
            };
            let results = match (completed, failed) {
                (c, 0) => format!("The queue is done: {c} finished."),
                (c, f) => format!("The queue is done: {c} finished, {f} need attention."),
            };
            dialogs::countdown(ctx, &format!("{verb} in {}", format::duration(left.as_secs().max(1))), &results, button, &mut cancel, &mut now);
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if cancel {
            self.countdown = None;
            self.toast("Cancelled. Nothing will happen");
        } else if now {
            self.countdown = None;
            match action {
                WhenDone::Sleep => {
                    if let Err(e) = sleep_now() {
                        self.toast_error(format!("Could not put the computer to sleep: {e}"));
                    }
                }
                WhenDone::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                WhenDone::Nothing => {}
            }
        }
    }

    fn paint_toasts(&mut self, ctx: &egui::Context, full: Rect) {
        const LIFE: f32 = 3.6;
        self.toasts.retain(|t| t.born.elapsed().as_secs_f32() < LIFE);
        if self.toasts.len() > 3 {
            let extra = self.toasts.len() - 3;
            self.toasts.drain(..extra);
        }
        if self.toasts.is_empty() {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("toasts")));
        let mut y = full.bottom() - 22.0;
        for toast in self.toasts.iter().rev() {
            let age = toast.born.elapsed().as_secs_f32();
            let alpha = (age / 0.16).min(1.0) * ((LIFE - age) / 0.3).clamp(0.0, 1.0);
            let rise = (1.0 - ease_out((age / 0.2).min(1.0))) * 8.0;
            let text_w = theme::measure(&painter, &toast.text, Type::BODY_MEDIUM).min(520.0);
            let size = Vec2::new(text_w + 44.0, 36.0);
            let rect = Rect::from_min_size(egui::pos2(full.center().x - size.x / 2.0, y - size.y + rise), size);
            let fade = |c: Color32| c.gamma_multiply(alpha);
            painter.rect_filled(rect.translate(Vec2::new(0.0, 6.0)).expand(4.0), CornerRadius::same(14), fade(Color32::from_black_alpha(70)));
            painter.rect(rect, CornerRadius::same(9), fade(theme::RAISED), Stroke::new(1.0, fade(theme::white(34))), egui::StrokeKind::Inside);
            let (icon, color) = if toast.bad { (Icon::Alert, theme::WARN) } else { (Icon::CheckCircle, theme::OK) };
            crate::icons::paint(&painter, Rect::from_min_size(rect.min, Vec2::new(34.0, rect.height())), icon, 14.0, fade(color));
            theme::text_in(&painter, Rect::from_min_max(egui::pos2(rect.left() + 32.0, rect.top()), egui::pos2(rect.right() - 12.0, rect.bottom())), Anchor::Left, &toast.text, Type::BODY_MEDIUM, fade(theme::TEXT));
            y -= size.y + 8.0;
        }
        ctx.request_repaint();
    }
}

/// The second line of a row: where the file comes from.
pub fn source_line(view: &DownloadView) -> String {
    let mut parts = vec![format::host(&view.url)];
    if view.protocol != siphon_core::Protocol::Http {
        parts.push(view.protocol.label().to_owned());
    }
    if let Some(note) = &view.note {
        parts.push(note.clone());
    }
    parts.join("  \u{00B7}  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_found_in_messy_text() {
        assert_eq!(extract_links("https://a.example/file.zip"), vec!["https://a.example/file.zip"]);
        assert_eq!(
            extract_links("Get it here: <https://a.example/x.iso>, or ftp://b.example/y.bin.\nthanks"),
            vec!["https://a.example/x.iso", "ftp://b.example/y.bin"]
        );
        assert_eq!(extract_links("href=\"https://a.example/z\" https://a.example/z"), vec!["https://a.example/z"]);
        assert!(extract_links("no links here, just http:// and words").is_empty());
    }
}
