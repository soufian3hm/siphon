//! The link grabber: explore a page (or take the links the browser sent)
//! and choose what to download.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use egui::{CornerRadius, Key, Modifiers, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use siphon_core::grab::{GrabOptions, LinkBatch, LinkKind};
use siphon_core::{Category, GrabJob, Manager, NewDownload, Request, Settings, filename};

use super::{Outcome, field_label, folder_row, footer, modal, padded, title_bar};
use crate::app::App;
use crate::format;
use crate::icons::{self, Icon};
use crate::theme::{self, Anchor, Type};
use crate::widgets::{self, Button, ButtonKind, Checked, Field};

const ROW_H: f32 = 40.0;
const LIST_H: f32 = 280.0;

/// Which links the list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    /// Everything that is a file.
    Files,
    Family(Category),
    /// Links to other pages, and links with no telling name.
    Pages,
}

struct Row {
    url: String,
    name: String,
    /// Host and link text, the quiet second line.
    origin: String,
    page: bool,
    family: Category,
    haystack: String,
}

pub struct GrabDialog {
    url: String,
    depth: u8,
    job: Option<GrabJob>,
    batch: Option<LinkBatch>,
    error: Option<String>,
    rows: Vec<Row>,
    checked: Vec<bool>,
    group: Group,
    search: String,
    dir: String,
    dir_touched: bool,
    settings: Settings,
    from_browser: bool,
    first_frame: bool,
}

impl GrabDialog {
    pub fn new(manager: &Manager, url: String) -> Self {
        let settings = manager.settings();
        Self {
            url,
            depth: 0,
            job: None,
            batch: None,
            error: None,
            rows: Vec::new(),
            checked: Vec::new(),
            group: Group::Files,
            search: String::new(),
            dir: settings.download_dir.to_string_lossy().into_owned(),
            dir_touched: false,
            settings,
            from_browser: false,
            first_frame: true,
        }
    }

    /// Opens on links the browser collected from a page that is already open.
    pub fn for_batch(manager: &Manager, batch: LinkBatch) -> Self {
        let mut dialog = Self::new(manager, batch.page_url.clone());
        dialog.from_browser = true;
        dialog.adopt(batch);
        dialog
    }

    fn adopt(&mut self, batch: LinkBatch) {
        self.rows = batch
            .links
            .iter()
            .map(|link| {
                let name = filename::from_url(&link.url).unwrap_or_else(|| link.url.clone());
                let host = format::host(&link.url);
                let origin = if link.text.is_empty() { host } else { format!("{host}  \u{00B7}  {}", link.text) };
                Row {
                    haystack: format!("{} {} {}", name, link.text, link.url).to_lowercase(),
                    page: link.kind == LinkKind::Page,
                    family: self.settings.category_of(&name),
                    url: link.url.clone(),
                    name,
                    origin,
                }
            })
            .collect();
        self.checked = vec![false; self.rows.len()];
        // Open on the files; if the page has none, show what it does have.
        self.group = if self.rows.iter().any(|r| !r.page) { Group::Files } else { Group::Pages };
        self.batch = Some(batch);
    }

    fn explore(&mut self, app: &App) {
        self.error = None;
        self.batch = None;
        self.rows.clear();
        self.checked.clear();
        self.from_browser = false;
        self.job = Some(app.manager.explore(Request::new(self.url.trim()), GrabOptions { depth: self.depth, ..Default::default() }));
    }

    /// For the screenshot script: explores to the given depth.
    pub fn script_explore(&mut self, app: &App, depth: u8) {
        self.depth = depth;
        self.explore(app);
    }

    /// For the screenshot script: shows one family and ticks its first rows.
    pub fn script_pick(&mut self, family: Category, count: usize) {
        self.group = Group::Family(family);
        let mut left = count;
        for (row, checked) in self.rows.iter().zip(self.checked.iter_mut()) {
            if left > 0 && !row.page && row.family == family {
                *checked = true;
                left -= 1;
            }
        }
    }

    fn in_group(&self, row: &Row) -> bool {
        match self.group {
            Group::Files => !row.page,
            Group::Family(family) => !row.page && row.family == family,
            Group::Pages => row.page,
        }
    }

    pub(super) fn show(&mut self, app: &mut App, ctx: &egui::Context) -> Outcome {
        if let Some(result) = self.job.as_ref().and_then(|job| job.take()) {
            self.job = None;
            match result {
                Ok(batch) => self.adopt(batch),
                Err(error) => {
                    let text = error.to_string();
                    let mut chars = text.chars();
                    self.error = Some(chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or(text) + ".");
                }
            }
        }
        let exploring = self.job.is_some();
        if exploring {
            ctx.request_repaint_after(Duration::from_millis(80));
        }

        let needle = self.search.trim().to_lowercase();
        let visible: Vec<usize> = (0..self.rows.len()).filter(|&i| self.in_group(&self.rows[i]) && (needle.is_empty() || self.rows[i].haystack.contains(&needle))).collect();
        let chosen = self.checked.iter().filter(|c| **c).count();
        let chosen_visible = visible.iter().filter(|&&i| self.checked[i]).count();

        let subtitle = match &self.batch {
            Some(batch) if !batch.page_title.is_empty() => batch.page_title.clone(),
            _ if self.from_browser => "Sent by your browser".to_owned(),
            _ => "List everything a page links to, then choose what to download.".to_owned(),
        };
        let looks_like_page = {
            let url = self.url.trim().to_ascii_lowercase();
            (url.starts_with("http://") || url.starts_with("https://")) && url.len() > 11
        };

        let mut start = false;
        let mut stop = false;
        let mut go: Option<bool> = None;
        let outcome = modal(ctx, "grab", 780.0, |ui| {
            if title_bar(ui, "Grab links", Some(&subtitle)) {
                return Outcome::Close;
            }
            padded(ui, |ui| {
                field_label(ui, "PAGE");
                ui.horizontal(|ui| {
                    let w = ui.available_width() - 168.0 - 104.0 - 16.0;
                    let response = Field::new(&mut self.url).hint("https://example.com/downloads/").icon(Icon::Link).mono().width(w).id(egui::Id::unique("grab-url")).show(ui);
                    if self.first_frame {
                        self.first_frame = false;
                        if self.url.is_empty() {
                            response.request_focus();
                        }
                    }
                    if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && looks_like_page && !exploring {
                        start = true;
                    }
                    widgets::dropdown(ui, "grab-depth", &[(0u8, "This page only"), (1u8, "And linked pages"), (2u8, "Two levels down")], &mut self.depth, 168.0);
                    if exploring {
                        if Button::new("Stop", ButtonKind::Secondary).min_width(104.0).show(ui).clicked() {
                            stop = true;
                        }
                    } else {
                        ui.add_enabled_ui(looks_like_page, |ui| {
                            let kind = if self.batch.is_some() { ButtonKind::Secondary } else { ButtonKind::Primary };
                            if Button::new("Explore", kind).icon(Icon::Search).min_width(104.0).show(ui).clicked() {
                                start = true;
                            }
                        });
                    }
                });
                ui.add_space(12.0);

                if let Some(error) = &self.error {
                    widgets::callout(ui, theme::BAD, Icon::Alert, "That page could not be read", error);
                    ui.add_space(12.0);
                }

                // Type chips on the left, the search on the right.
                let (strip, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::hover());
                let count = |group: Group| {
                    self.rows
                        .iter()
                        .filter(|row| match group {
                            Group::Files => !row.page,
                            Group::Family(family) => !row.page && row.family == family,
                            Group::Pages => row.page,
                        })
                        .count()
                };
                let mut groups: Vec<(Group, &str, usize)> = vec![(Group::Files, "All files", count(Group::Files))];
                for family in Category::ALL {
                    let n = count(Group::Family(family));
                    if n > 0 {
                        groups.push((Group::Family(family), family.label(), n));
                    }
                }
                let pages = count(Group::Pages);
                if pages > 0 {
                    groups.push((Group::Pages, "Pages", pages));
                }
                // The chips scroll sideways when a page has every kind of file.
                let chips = Rect::from_min_max(strip.min, egui::pos2(strip.right() - 200.0, strip.bottom()));
                let overflow = theme::region(ui, chips, |ui| {
                    egui::ScrollArea::horizontal()
                        .id_salt("grab-chips")
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.horizontal_centered(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                for (group, label, n) in groups {
                                    if widgets::chip(ui, label, n, self.group == group).clicked() {
                                        self.group = group;
                                    }
                                }
                            });
                        })
                        .content_size
                        .x
                        > chips.width() + 1.0
                });
                if overflow {
                    // A soft edge says there is more to the right.
                    let fade = Rect::from_min_max(egui::pos2(chips.right() - 28.0, chips.top()), chips.max);
                    let mut mesh = egui::Mesh::default();
                    mesh.colored_vertex(fade.left_top(), egui::Color32::TRANSPARENT);
                    mesh.colored_vertex(fade.right_top(), theme::PANEL);
                    mesh.colored_vertex(fade.left_bottom(), egui::Color32::TRANSPARENT);
                    mesh.colored_vertex(fade.right_bottom(), theme::PANEL);
                    mesh.add_triangle(0, 1, 2);
                    mesh.add_triangle(1, 3, 2);
                    ui.painter().add(egui::Shape::mesh(mesh));
                }
                theme::place(ui, Rect::from_min_max(egui::pos2(strip.right() - 184.0, strip.top()), strip.max), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    Field::new(&mut self.search).hint("Filter").icon(Icon::Search).width(184.0).id(egui::Id::unique("grab-search")).show(ui);
                });
                ui.add_space(10.0);

                // Header: the box that stands for every visible row, and the tally.
                let (head, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::hover());
                ui.painter().hline(head.x_range(), head.bottom() - 0.5, theme::hairline());
                let state = match (chosen_visible, visible.len()) {
                    (0, _) => Checked::No,
                    (a, b) if a == b => Checked::Yes,
                    _ => Checked::Some,
                };
                let mut toggle_all = false;
                theme::place(ui, Rect::from_min_size(egui::pos2(head.left() + 8.0, head.center().y - 10.0), Vec2::splat(20.0)), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(!visible.is_empty(), |ui| {
                        toggle_all = widgets::check_box(ui, state).clicked();
                    });
                });
                theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(head.left() + 76.0, head.top()), head.max), Anchor::Left, "NAME", Type::LABEL, theme::TEXT_3);
                let tally = match (chosen, self.rows.is_empty()) {
                    (_, true) => String::new(),
                    (0, false) => format!("{} shown", visible.len()),
                    (n, false) => format!("{n} selected  \u{00B7}  {} shown", visible.len()),
                };
                theme::text_in(ui.painter(), head.shrink2(Vec2::new(12.0, 0.0)), Anchor::Right, &tally, Type::SMALL, if chosen > 0 { theme::TEXT } else { theme::TEXT_3 });
                // Select-all also answers to the keyboard, when no field has it.
                if !ctx.text_edit_focused() && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
                    toggle_all = true;
                }
                if toggle_all {
                    let turn_on = state != Checked::Yes;
                    for &index in &visible {
                        self.checked[index] = turn_on;
                    }
                }

                let (list, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), LIST_H), Sense::hover());
                if exploring {
                    let progress = &self.job.as_ref().expect("checked above").progress;
                    let (read, known, found) = (progress.pages_read.load(Ordering::Relaxed), progress.pages_known.load(Ordering::Relaxed), progress.links_found.load(Ordering::Relaxed));
                    let line = if known <= 1 { "Reading the page\u{2026}".to_owned() } else { format!("Reading page {} of {}  \u{00B7}  {found} links so far", (read + 1).min(known), known) };
                    theme::text_in(ui.painter(), Rect::from_center_size(egui::pos2(list.center().x, list.center().y - 12.0), Vec2::new(list.width(), 20.0)), Anchor::Center, &line, Type::BODY, theme::TEXT_2);
                    widgets::progress(ui, Rect::from_center_size(egui::pos2(list.center().x, list.center().y + 12.0), Vec2::new(220.0, 3.0)), if known > 1 { Some(read as f32 / known as f32) } else { None }, theme::ACCENT);
                } else if visible.is_empty() {
                    let (title, hint) = match (&self.batch, self.rows.is_empty(), needle.is_empty()) {
                        (None, _, _) => ("Nothing explored yet", "Enter the address of a page and press Explore."),
                        (Some(_), true, _) => ("This page links to nothing", "Pages that build their links with scripts are better sent from the browser: right-click the page and choose \u{201C}Download all links with Siphon\u{201D}."),
                        (Some(_), false, false) => ("No matches", "Nothing in this group matches the filter."),
                        (Some(_), false, true) => ("Nothing in this group", "Try another group above."),
                    };
                    theme::text_in(ui.painter(), Rect::from_center_size(egui::pos2(list.center().x, list.center().y - 22.0), Vec2::new(list.width(), 22.0)), Anchor::Center, title, Type::TITLE, theme::TEXT);
                    let galley = theme::layout_centered(ui.painter(), hint, Type::BODY, theme::TEXT_2, 440.0, 3);
                    ui.painter().galley(egui::pos2(list.center().x, list.center().y - 2.0), galley, theme::TEXT_2);
                } else {
                    theme::region(ui, list, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        egui::ScrollArea::vertical().id_salt("grab-list").auto_shrink([false, false]).show_rows(ui, ROW_H, visible.len(), |ui, range| {
                            for &index in &visible[range] {
                                self.row(ui, index);
                            }
                        });
                    });
                }
                ui.painter().hline(list.x_range(), list.bottom() + 0.5, theme::hairline());
                ui.add_space(14.0);

                field_label(ui, if self.settings.sort_by_type && !self.dir_touched { "SAVE TO  \u{00B7}  SORTED BY TYPE" } else { "SAVE TO" });
                folder_row(ui, &mut self.dir, &mut self.dir_touched);
            });

            footer(ui, |ui| {
                ui.add_enabled_ui(chosen > 0 && !self.dir.trim().is_empty(), |ui| {
                    let label = if chosen > 1 { format!("Download {chosen}") } else { "Download".to_owned() };
                    if Button::new(&label, ButtonKind::Primary).icon(Icon::Download).min_width(124.0).show(ui).clicked() {
                        go = Some(false);
                    }
                    if Button::new("Download later", ButtonKind::Secondary).icon(Icon::Clock).show(ui).clicked() {
                        go = Some(true);
                    }
                });
                if Button::new("Cancel", ButtonKind::Ghost).show(ui).clicked() {
                    ui.close();
                }
            });
            if ui.should_close() { Outcome::Close } else { Outcome::Stay }
        });

        if stop {
            // Dropping the job cancels it.
            self.job = None;
        }
        if start {
            self.explore(app);
        }
        if let (Some(later), Some(batch)) = (go, &self.batch) {
            let dir = (self.dir_touched || !self.settings.sort_by_type).then(|| PathBuf::from(self.dir.trim()));
            let downloads: Vec<NewDownload> = self
                .rows
                .iter()
                .zip(&self.checked)
                .filter(|(_, checked)| **checked)
                .map(|(row, _)| NewDownload { request: batch.request_for(&row.url), dir: dir.clone(), later, ..Default::default() })
                .collect();
            let n = app.manager.add_many(downloads).len();
            app.toast(match (n, later) {
                (1, false) => "Added 1 download".to_owned(),
                (n, false) => format!("Added {n} downloads"),
                (1, true) => "Kept 1 download for the queue".to_owned(),
                (n, true) => format!("Kept {n} downloads for the queue"),
            });
            if later {
                app.filter = crate::app::Filter::Scheduled;
            }
            return Outcome::Close;
        }
        outcome
    }

    fn row(&mut self, ui: &mut Ui, index: usize) {
        let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
        let on = self.checked[index];
        let row = &self.rows[index];
        let t = ui.ctx().animate_bool_with_time(response.id.with("on"), on, 0.1);
        if t > 0.0 {
            ui.painter().rect_filled(rect, CornerRadius::ZERO, theme::tint(theme::ACCENT, (t * 20.0) as u8));
        } else if response.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::ZERO, theme::white(8));
        }
        ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::white(9)));

        let mut clicked = response.clicked();
        theme::place(ui, Rect::from_min_size(egui::pos2(rect.left() + 8.0, rect.center().y - 10.0), Vec2::splat(20.0)), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            clicked |= widgets::check_box(ui, if on { Checked::Yes } else { Checked::No }).clicked();
        });
        let tile = Rect::from_min_size(egui::pos2(rect.left() + 40.0, rect.center().y - 12.0), Vec2::splat(24.0));
        ui.painter().rect(tile, CornerRadius::same(6), theme::RAISED, Stroke::new(1.0, theme::white(20)), StrokeKind::Inside);
        icons::paint(ui.painter(), tile, if row.page { Icon::Link } else { Icon::for_category(row.family) }, 13.0, theme::mix(theme::TEXT_2, theme::ACCENT_HI, t));

        let kind = if row.page { "Page" } else { row.family.label() };
        let kind_w = 96.0;
        let text = Rect::from_min_max(egui::pos2(tile.right() + 12.0, rect.top()), egui::pos2(rect.right() - kind_w - 24.0, rect.bottom()));
        theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text.left(), rect.top() + 4.0), Vec2::new(text.width(), 18.0)), Anchor::Left, &row.name, Type::BODY, theme::TEXT);
        theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(text.left(), rect.top() + 20.0), Vec2::new(text.width(), 16.0)), Anchor::Left, &row.origin, Type::SMALL, theme::TEXT_3);
        theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(rect.right() - kind_w - 12.0, rect.top()), egui::pos2(rect.right() - 12.0, rect.bottom())), Anchor::Right, kind, Type::SMALL, theme::TEXT_2);

        if clicked {
            self.checked[index] = !on;
        }
    }
}
