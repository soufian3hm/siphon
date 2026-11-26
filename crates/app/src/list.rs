//! The download list: header, column titles, rows and the empty state.

use egui::{Color32, CornerRadius, CursorIcon, Popup, Rect, Sense, Stroke, Ui, Vec2};
use siphon_core::{DownloadId, DownloadView, Status};

use crate::app::{Action, App, Filter, Sort, SortKey, needs_attention, source_line};
use crate::format;
use crate::icons::{self, Icon};
use crate::theme::{self, Anchor, Type, Weight};
use crate::widgets::{self, Button, ButtonKind, Field};

const PAD: f32 = 20.0;
const COL_SIZE: f32 = 86.0;
const COL_STATUS: f32 = 196.0;
const COL_SPEED: f32 = 96.0;
const COL_ETA: f32 = 84.0;
const GAP: f32 = 18.0;

/// Below this width the speed and time columns fold into the status cell.
const COMPACT_BELOW: f32 = 900.0;
const COL_STATUS_COMPACT: f32 = 236.0;

/// Horizontal slots of a row, shared by the header and every row.
struct Columns {
    name: Rect,
    size: Rect,
    status: Rect,
    /// Absent in the compact layout.
    speed: Option<Rect>,
    eta: Option<Rect>,
}

impl Columns {
    fn of(row: Rect) -> Self {
        let y = row.y_range();
        let right = row.right() - PAD;
        if row.width() < COMPACT_BELOW {
            let status = Rect::from_x_y_ranges((right - COL_STATUS_COMPACT)..=right, y);
            let size = Rect::from_x_y_ranges((status.left() - GAP - COL_SIZE)..=(status.left() - GAP), y);
            let name = Rect::from_x_y_ranges((row.left() + PAD)..=(size.left() - GAP), y);
            return Self { name, size, status, speed: None, eta: None };
        }
        let eta = Rect::from_x_y_ranges((right - COL_ETA)..=right, y);
        let speed = Rect::from_x_y_ranges((eta.left() - GAP - COL_SPEED)..=(eta.left() - GAP), y);
        let status = Rect::from_x_y_ranges((speed.left() - GAP - COL_STATUS)..=(speed.left() - GAP), y);
        let size = Rect::from_x_y_ranges((status.left() - GAP - COL_SIZE)..=(status.left() - GAP), y);
        let name = Rect::from_x_y_ranges((row.left() + PAD)..=(size.left() - GAP), y);
        Self { name, size, status, speed: Some(speed), eta: Some(eta) }
    }
}

pub fn show(app: &mut App, ui: &mut Ui) {
    let rect = ui.max_rect();
    toolbar(app, ui, Rect::from_min_size(rect.min, Vec2::new(rect.width(), theme::TITLEBAR_H + 8.0)));

    // Filter, search, sort.
    let needle = app.search.trim().to_lowercase();
    let mut rows: Vec<DownloadView> = app
        .views
        .iter()
        .filter(|v| app.filter.matches(v))
        .filter(|v| needle.is_empty() || v.file_name.to_lowercase().contains(&needle) || v.url.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    sort(&mut rows, app.sort);
    app.visible = rows.iter().map(|v| v.id).collect();

    let header = Rect::from_min_size(egui::pos2(rect.left(), rect.top() + theme::TITLEBAR_H + 8.0), Vec2::new(rect.width(), 30.0));
    column_titles(app, ui, header);

    let body = Rect::from_min_max(header.left_bottom(), rect.max);
    if rows.is_empty() {
        empty_state(app, ui, body, !needle.is_empty());
        return;
    }

    theme::region(ui, body, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical().id_salt("downloads").auto_shrink([false, false]).show_rows(ui, theme::ROW_H, rows.len(), |ui, range| {
            for view in &rows[range] {
                row(app, ui, view);
            }
        });
    });
}

fn sort(rows: &mut [DownloadView], sort: Sort) {
    match sort.key {
        // The manager already lists newest first.
        SortKey::Added => {
            if !sort.descending {
                rows.reverse();
            }
            return;
        }
        SortKey::Name => rows.sort_by_cached_key(|v| v.file_name.to_lowercase()),
        SortKey::Size => rows.sort_by_key(|v| v.total.unwrap_or(0)),
        SortKey::Progress => rows.sort_by(|a, b| a.fraction.unwrap_or(0.0).total_cmp(&b.fraction.unwrap_or(0.0))),
    }
    if sort.descending {
        rows.reverse();
    }
}

fn toolbar(app: &mut App, ui: &mut Ui, rect: Rect) {
    let centre_y = rect.top() + theme::TITLEBAR_H / 2.0 + 6.0;
    let shown = app.views.iter().filter(|v| app.filter.matches(v)).count();

    let title_w = theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(rect.left() + PAD, centre_y - 14.0), Vec2::new(320.0, 28.0)), Anchor::Left, app.filter.title(), Type::HEADING, theme::TEXT);
    let noun = if shown == 1 { "item" } else { "items" };
    theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(rect.left() + PAD + title_w + 10.0, centre_y - 12.0), Vec2::new(120.0, 28.0)), Anchor::Left, &format!("{shown} {noun}"), Type::SMALL, theme::TEXT_3);

    let controls = Rect::from_min_max(egui::pos2(rect.left() + PAD + title_w + 130.0, centre_y - 15.0), egui::pos2(rect.right() - PAD, centre_y + 15.0));
    theme::place(ui, controls, egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;

        let more = widgets::icon_button(ui, Icon::More, "More");
        Popup::menu(&more).frame(widgets::menu_frame()).width(260.0).gap(4.0).show(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            let mac = cfg!(target_os = "macos");
            if widgets::menu_item(ui, Some(Icon::Stack), "Add many links\u{2026}", Some(if mac { "\u{21E7}\u{2318}N" } else { "Ctrl Shift N" }), false, false).clicked() {
                app.actions.push(Action::Batch(String::new()));
                ui.close();
            }
            if widgets::menu_item(ui, Some(Icon::Link), "Grab links from a page\u{2026}", Some(if mac { "\u{2318}G" } else { "Ctrl G" }), false, false).clicked() {
                app.actions.push(Action::GrabLinks(String::new()));
                ui.close();
            }
            if widgets::menu_item(ui, Some(Icon::Document), "Import links from a file\u{2026}", None, false, false).clicked() {
                app.actions.push(Action::ImportLinks);
                ui.close();
            }
            ui.add_enabled_ui(!app.views.is_empty(), |ui| {
                let label = if app.selection.len() > 1 { "Export the selected links\u{2026}" } else { "Export all links\u{2026}" };
                if widgets::menu_item(ui, Some(Icon::External), label, None, false, false).clicked() {
                    let ids = if app.selection.len() > 1 { app.selection.iter().copied().collect() } else { Vec::new() };
                    app.actions.push(Action::ExportLinks(ids));
                    ui.close();
                }
            });
            widgets::menu_separator(ui);
            if widgets::menu_item(ui, Some(Icon::Clock), "Queue and schedule\u{2026}", None, false, false).clicked() {
                app.actions.push(Action::Schedule);
                ui.close();
            }
            if widgets::menu_item(ui, Some(Icon::CheckCircle), "Clear finished from list", None, false, false).clicked() {
                app.actions.push(Action::ClearCompleted);
                ui.close();
            }
            widgets::menu_separator(ui);
            if widgets::menu_item(ui, Some(Icon::Sliders), "Settings\u{2026}", Some(if cfg!(target_os = "macos") { "\u{2318}," } else { "Ctrl ," }), false, false).clicked() {
                app.actions.push(Action::Settings);
                ui.close();
            }
        });

        // The queue's own controls, where the queue is being looked at.
        if app.filter == Filter::Scheduled {
            let queue = app.manager.queue();
            if widgets::icon_button(ui, Icon::Clock, "Queue and schedule").clicked() {
                app.actions.push(Action::Schedule);
            }
            if queue.running {
                if Button::new("Stop queue", ButtonKind::Secondary).icon(Icon::Pause).show(ui).clicked() {
                    app.actions.push(Action::StopQueue);
                }
            } else {
                ui.add_enabled_ui(queue.waiting > 0, |ui| {
                    if Button::new("Start queue", ButtonKind::Primary).icon(Icon::Play).show(ui).clicked() {
                        app.actions.push(Action::StartQueue);
                    }
                });
            }
            ui.add_space(4.0);
        }

        let any_running = app.views.iter().any(|v| v.status.can_pause());
        let any_stopped = app.views.iter().any(|v| v.status.can_resume() && v.status != Status::Scheduled);
        ui.add_enabled_ui(any_running, |ui| {
            if widgets::icon_button(ui, Icon::Pause, "Pause all").clicked() {
                app.actions.push(Action::PauseAll);
            }
        });
        ui.add_enabled_ui(any_stopped, |ui| {
            if widgets::icon_button(ui, Icon::Play, "Resume all").clicked() {
                app.actions.push(Action::ResumeAll);
            }
        });
        ui.add_space(4.0);

        let id = egui::Id::unique("search-field");
        let width = ui.available_width().clamp(120.0, 250.0);
        let response = Field::new(&mut app.search).hint("Search").icon(Icon::Search).width(width).id(id).show(ui);
        if app.focus_search {
            app.focus_search = false;
            response.request_focus();
        }
    });
}

fn column_titles(app: &mut App, ui: &mut Ui, rect: Rect) {
    let cols = Columns::of(rect);
    let mut title = |ui: &mut Ui, slot: Rect, text: &str, anchor: Anchor, key: Option<SortKey>| {
        let active = key.is_some_and(|k| app.sort.key == k);
        let mut color = if active { theme::TEXT_2 } else { theme::TEXT_3 };
        if let Some(key) = key {
            let response = ui.interact(slot, ui.scope_id().with(("sort", text)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
            if response.hovered() {
                color = theme::TEXT;
            }
            if response.clicked() {
                // A second click on the same column flips the direction.
                app.sort = if active { Sort { key, descending: !app.sort.descending } } else { Sort { key, descending: key != SortKey::Name } };
            }
        }
        let w = theme::text_in(ui.painter(), slot, anchor, text, Type::LABEL, color);
        if active {
            let x = match anchor {
                Anchor::Right => slot.right() - w - 13.0,
                _ => slot.left() + w + 4.0,
            };
            let c = egui::pos2(x + 4.0, slot.center().y);
            let dir = if app.sort.descending { 1.0 } else { -1.0 };
            ui.painter().line(vec![egui::pos2(c.x - 3.0, c.y - 1.5 * dir), egui::pos2(c.x, c.y + 1.5 * dir), egui::pos2(c.x + 3.0, c.y - 1.5 * dir)], Stroke::new(1.2, color));
        }
    };
    title(ui, cols.name, "NAME", Anchor::Left, Some(SortKey::Name));
    title(ui, cols.size, "SIZE", Anchor::Right, Some(SortKey::Size));
    title(ui, cols.status, "STATUS", Anchor::Left, Some(SortKey::Progress));
    if let (Some(speed), Some(eta)) = (cols.speed, cols.eta) {
        title(ui, speed, "SPEED", Anchor::Right, None);
        title(ui, eta, "TIME LEFT", Anchor::Right, None);
    }
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, theme::hairline());
}

/// Colour and wording of a status.
pub fn status_style(view: &DownloadView) -> (String, Color32) {
    match &view.status {
        Status::Queued => ("Queued".into(), theme::TEXT_2),
        Status::Scheduled => ("Scheduled".into(), theme::TEXT_2),
        Status::Connecting => ("Connecting".into(), theme::ACCENT),
        Status::Downloading => ("Downloading".into(), theme::ACCENT),
        Status::Assembling => ("Assembling".into(), theme::ACCENT),
        Status::Paused => ("Paused".into(), theme::TEXT_2),
        Status::NeedsRefresh { .. } => ("Link expired".into(), theme::WARN),
        Status::Failed { .. } if view.retry_in.is_some() => ("Retrying".into(), theme::WARN),
        Status::Failed { .. } => ("Failed".into(), theme::BAD),
        Status::Completed => ("Completed".into(), theme::OK),
    }
}

fn row(app: &mut App, ui: &mut Ui, view: &DownloadView) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), theme::ROW_H), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let id = ui.make_persistent_id(("row", view.id));
    let response = ui.interact(rect, id, Sense::click());
    let selected = app.selection.contains(&view.id);
    let hovered = ui.rect_contains_pointer(rect);
    let sel_t = ui.ctx().animate_bool_with_time(id.with("s"), selected, 0.1);
    let hov_t = ui.ctx().animate_bool_with_time(id.with("h"), hovered, 0.09);

    let painter = ui.painter().clone();
    if sel_t > 0.0 {
        painter.rect_filled(rect, 0, theme::tint(theme::ACCENT, (sel_t * 30.0) as u8));
        painter.rect_filled(Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())), 0, theme::tint(theme::ACCENT, (sel_t * 255.0) as u8));
    } else if hov_t > 0.0 {
        painter.rect_filled(rect, 0, theme::white((hov_t * 9.0) as u8));
    }
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::white(11)));

    let cols = Columns::of(rect);
    let (status_text, status_color) = status_style(view);
    let dim = view.status.is_finished() || matches!(view.status, Status::Paused | Status::Queued);

    // Identity: glyph tile, name, source.
    let tile = Rect::from_min_size(egui::pos2(cols.name.left(), rect.center().y - 15.0), Vec2::splat(30.0));
    painter.rect(tile, CornerRadius::same(7), theme::RAISED, Stroke::new(1.0, theme::white(20)), egui::StrokeKind::Inside);
    icons::paint(&painter, tile, Icon::for_category(view.category), 15.0, if needs_attention(&view.status) { status_color } else { theme::TEXT_2 });
    let text_left = tile.right() + 12.0;
    // Quick actions take the tail of the name column while the row is hovered.
    let actions_w = if hovered { 70.0 } else { 0.0 };
    let text_w = cols.name.right() - text_left - actions_w;
    let name_rect = Rect::from_min_size(egui::pos2(text_left, rect.top() + 7.0), Vec2::new(text_w, 17.0));
    theme::text_in(&painter, name_rect, Anchor::Left, &view.file_name, Type::BODY_MEDIUM, theme::TEXT);
    let sub_rect = Rect::from_min_size(egui::pos2(text_left, rect.top() + 25.0), Vec2::new(text_w, 15.0));
    theme::text_in(&painter, sub_rect, Anchor::Left, &source_line(view), Type::SMALL, theme::TEXT_3);

    // Size: hidden when the server never said.
    if let Some(total) = view.total {
        theme::text_in(&painter, cols.size, Anchor::Right, &format::bytes(total), Type::NUM, if dim { theme::TEXT_2 } else { theme::TEXT });
    }

    status_cell(ui, &painter, cols.status, view, &status_text, status_color, cols.speed.is_none());

    if let (Some(speed), Some(eta)) = (cols.speed, cols.eta) {
        if view.status == Status::Downloading {
            theme::text_in(&painter, speed, Anchor::Right, &format::speed(view.bytes_per_sec), Type::NUM, theme::TEXT);
            if let Some(secs) = view.eta_secs {
                theme::text_in(&painter, eta, Anchor::Right, &format::duration(secs), Type::NUM, theme::TEXT_2);
            }
        } else if let Some(secs) = view.retry_in {
            theme::text_in(&painter, eta, Anchor::Right, &format!("in {}", format::duration(secs)), Type::NUM, theme::TEXT_3);
        }
    }

    // Quick actions.
    if hovered {
        let slot = Rect::from_min_size(egui::pos2(cols.name.right() - 62.0, rect.center().y - 14.0), Vec2::new(62.0, 28.0));
        theme::place(ui, slot, egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if view.status.is_finished() {
                if widgets::icon_button(ui, Icon::Folder, "Show in folder").clicked() {
                    app.actions.push(Action::Reveal(view.id));
                }
                if widgets::icon_button(ui, Icon::External, "Open").clicked() {
                    app.actions.push(Action::Open(view.id));
                }
            } else {
                if widgets::icon_button(ui, Icon::Close, "Remove").clicked() {
                    app.actions.push(Action::AskRemove(vec![view.id]));
                }
                if matches!(view.status, Status::NeedsRefresh { .. }) {
                    if widgets::icon_button(ui, Icon::Refresh, "Swap link").clicked() {
                        app.actions.push(Action::SwapLink(view.id));
                    }
                } else if view.status.can_pause() {
                    if widgets::icon_button(ui, Icon::Pause, "Pause").clicked() {
                        app.actions.push(Action::Pause(vec![view.id]));
                    }
                } else if widgets::icon_button(ui, Icon::Play, "Resume").clicked() {
                    app.actions.push(Action::Resume(vec![view.id]));
                }
            }
        });
    }

    // Selection.
    if response.clicked() {
        let modifiers = ui.input(|i| i.modifiers);
        if modifiers.command {
            if !app.selection.remove(&view.id) {
                app.selection.insert(view.id);
            }
            app.focus = Some(view.id);
        } else if modifiers.shift && app.focus.is_some() {
            let anchor = app.focus.and_then(|f| app.visible.iter().position(|v| *v == f));
            let here = app.visible.iter().position(|v| *v == view.id);
            if let (Some(a), Some(b)) = (anchor, here) {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                app.selection = app.visible[lo..=hi].iter().copied().collect();
            }
        } else {
            app.select_only(view.id);
        }
    }
    if response.double_clicked() {
        app.actions.push(if view.status.is_finished() {
            Action::Open(view.id)
        } else if view.status.can_pause() {
            Action::Pause(vec![view.id])
        } else {
            Action::Resume(vec![view.id])
        });
    }
    if response.secondary_clicked() && !app.selection.contains(&view.id) {
        app.select_only(view.id);
    }
    Popup::context_menu(&response).frame(widgets::menu_frame()).width(236.0).show(|ui| context_menu(app, ui, view));
}

/// Status: a percentage over a thin bar while there is progress to show, a
/// tag otherwise.
fn status_cell(ui: &Ui, painter: &egui::Painter, slot: Rect, view: &DownloadView, text: &str, color: Color32, compact: bool) {
    let id = egui::Id::unique(("progress", view.id));
    let moving = matches!(view.status, Status::Downloading | Status::Connecting | Status::Assembling);
    let partial = !view.status.is_finished() && (view.downloaded > 0 || moving);

    if !partial {
        let rect = widgets::pill(ui, egui::pos2(slot.left(), slot.center().y), text, color);
        if let Status::Failed { message, .. } = &view.status {
            ui.interact(rect, id.with("why"), Sense::hover()).on_hover_text(message);
        }
        return;
    }

    let top = Rect::from_min_size(egui::pos2(slot.left(), slot.top() + 9.0), Vec2::new(slot.width(), 16.0));
    match view.fraction {
        Some(fraction) => {
            // Ease toward the real value so the bar glides instead of stepping.
            let shown = ui.ctx().animate_value_with_time(id, fraction, 0.35);
            let w = theme::text_in(painter, top, Anchor::Left, &format::percent(fraction), Type::NUM_MEDIUM, if moving { theme::TEXT } else { theme::TEXT_2 });
            if !moving || view.status != Status::Downloading {
                theme::text_in(painter, Rect::from_min_max(egui::pos2(top.left() + w + 8.0, top.top()), top.max), Anchor::Left, text, Type::new(11.5, Weight::Medium), color);
            } else if compact {
                // No room for separate columns: speed and time ride along here.
                let mut tail = format::speed(view.bytes_per_sec);
                if let Some(secs) = view.eta_secs {
                    tail = format!("{tail}  \u{00B7}  {}", format::duration(secs));
                }
                theme::text_in(painter, top, Anchor::Right, &tail, Type::mono(11.0, Weight::Regular), theme::TEXT_2);
            } else if view.connections > 0 {
                let label = format!("{} conn", view.connections);
                theme::text_in(painter, top, Anchor::Right, &label, Type::mono(11.0, Weight::Regular), theme::TEXT_3);
            }
            let bar = Rect::from_min_size(egui::pos2(slot.left(), slot.bottom() - 14.0), Vec2::new(slot.width(), 3.0));
            widgets::progress(ui, bar, Some(shown), if moving { theme::ACCENT } else { theme::tint(color, 150) });
        }
        None => {
            // No size from the server: show what has arrived instead of a guess.
            let w = theme::text_in(painter, top, Anchor::Left, &format::bytes(view.downloaded), Type::NUM_MEDIUM, theme::TEXT);
            if compact && view.status == Status::Downloading {
                theme::text_in(painter, top, Anchor::Right, &format::speed(view.bytes_per_sec), Type::mono(11.0, Weight::Regular), theme::TEXT_2);
            } else {
                theme::text_in(painter, Rect::from_min_max(egui::pos2(top.left() + w + 8.0, top.top()), top.max), Anchor::Left, text, Type::new(11.5, Weight::Medium), color);
            }
            let bar = Rect::from_min_size(egui::pos2(slot.left(), slot.bottom() - 14.0), Vec2::new(slot.width(), 3.0));
            widgets::progress(ui, bar, if moving { None } else { Some(0.0) }, theme::ACCENT);
        }
    }
}

fn context_menu(app: &mut App, ui: &mut Ui, view: &DownloadView) {
    ui.spacing_mut().item_spacing.y = 1.0;
    let many = app.selection.len() > 1 && app.selection.contains(&view.id);
    let targets: Vec<DownloadId> = if many { app.selection.iter().copied().collect() } else { vec![view.id] };
    let mut pick = |ui: &mut Ui, action: Action| {
        app.actions.push(action);
        ui.close();
    };

    if many {
        if widgets::menu_item(ui, Some(Icon::Play), &format!("Resume {} downloads", targets.len()), None, false, false).clicked() {
            pick(ui, Action::Resume(targets.clone()));
        }
        if widgets::menu_item(ui, Some(Icon::Pause), &format!("Pause {} downloads", targets.len()), None, false, false).clicked() {
            pick(ui, Action::Pause(targets.clone()));
        }
        if widgets::menu_item(ui, Some(Icon::Clock), "Download later", None, false, false).clicked() {
            pick(ui, Action::Later(targets.clone()));
        }
        if widgets::menu_item(ui, Some(Icon::External), "Export links\u{2026}", None, false, false).clicked() {
            pick(ui, Action::ExportLinks(targets.clone()));
        }
        widgets::menu_separator(ui);
        if widgets::menu_item(ui, Some(Icon::Trash), "Remove\u{2026}", Some("\u{232B}"), false, true).clicked() {
            pick(ui, Action::AskRemove(targets));
        }
        return;
    }

    let finished = view.status.is_finished();
    if finished {
        if widgets::menu_item(ui, Some(Icon::External), "Open", Some("\u{21B5}"), false, false).clicked() {
            pick(ui, Action::Open(view.id));
        }
    } else if view.status.can_pause() {
        if widgets::menu_item(ui, Some(Icon::Pause), "Pause", Some("Space"), false, false).clicked() {
            pick(ui, Action::Pause(vec![view.id]));
        }
    } else if matches!(view.status, Status::NeedsRefresh { code: 401 }) {
        if widgets::menu_item(ui, Some(Icon::Play), "Sign in\u{2026}", None, false, false).clicked() {
            pick(ui, Action::Login(view.id));
        }
    } else if widgets::menu_item(ui, Some(Icon::Play), if view.status == Status::Scheduled { "Start now" } else { "Resume" }, Some("Space"), false, false).clicked() {
        pick(ui, Action::Resume(vec![view.id]));
    }
    if !finished
        && view.status != Status::Scheduled
        && !matches!(view.status, Status::NeedsRefresh { .. })
        && widgets::menu_item(ui, Some(Icon::Clock), "Download later", None, false, false).clicked()
    {
        pick(ui, Action::Later(vec![view.id]));
    }
    if finished && widgets::menu_item(ui, Some(Icon::CheckCircle), "Verify checksum\u{2026}", None, false, false).clicked() {
        pick(ui, Action::Verify(view.id));
    }
    if widgets::menu_item(ui, Some(Icon::Folder), if cfg!(target_os = "macos") { "Show in Finder" } else { "Show in folder" }, None, false, false).clicked() {
        pick(ui, Action::Reveal(view.id));
    }
    widgets::menu_separator(ui);

    if !finished && view.protocol == siphon_core::Protocol::Page {
        if matches!(view.status, Status::NeedsRefresh { .. } | Status::Failed { .. }) && widgets::menu_item(ui, Some(Icon::Refresh), "Try again", None, false, false).clicked() {
            pick(ui, Action::Retry(view.id));
        }
    } else if !finished && widgets::menu_item(ui, Some(Icon::Refresh), "Refresh link\u{2026}", None, false, false).clicked() {
        pick(ui, Action::SwapLink(view.id));
    }
    if view.referer.is_some() && widgets::menu_item(ui, Some(Icon::External), "Open source page", None, false, false).clicked() {
        pick(ui, Action::OpenSource(view.id));
    }
    if widgets::menu_item(ui, Some(Icon::Link), "Copy link", Some(if cfg!(target_os = "macos") { "\u{2318}C" } else { "Ctrl C" }), false, false).clicked() {
        pick(ui, Action::CopyLink(view.id));
    }
    if widgets::menu_item(ui, Some(Icon::Copy), "Copy file path", None, false, false).clicked() {
        pick(ui, Action::CopyPath(view.id));
    }
    ui.add_enabled_ui(!view.status.is_active(), |ui| {
        if widgets::menu_item(ui, Some(Icon::Folder), "Move to\u{2026}", None, false, false).clicked() {
            pick(ui, Action::MoveTo(view.id));
        }
    });
    widgets::menu_separator(ui);
    if widgets::menu_item(ui, Some(Icon::Restart), "Download again from the start", None, false, false).clicked() {
        pick(ui, Action::Restart(view.id));
    }
    if widgets::menu_item(ui, Some(Icon::Trash), "Remove\u{2026}", Some("\u{232B}"), false, true).clicked() {
        pick(ui, Action::AskRemove(vec![view.id]));
    }
}

/// Nothing to list: say why, and offer the way forward.
fn empty_state(app: &mut App, ui: &mut Ui, body: Rect, searching: bool) {
    let c = body.center();
    let painter = ui.painter().clone();

    // A small byte map, the app's own motif, with a few cells filled in.
    let (cols, rows, cell, gap) = (12, 4, 9.0, 3.0);
    let grid = Vec2::new(cols as f32 * (cell + gap) - gap, rows as f32 * (cell + gap) - gap);
    let origin = egui::pos2(c.x - grid.x / 2.0, c.y - 112.0);
    for r in 0..rows {
        for col in 0..cols {
            let index = r * cols + col;
            let lit = !searching && matches!(index, 0..=6 | 12..=15 | 24..=26 | 36);
            let rect = Rect::from_min_size(egui::pos2(origin.x + col as f32 * (cell + gap), origin.y + r as f32 * (cell + gap)), Vec2::splat(cell));
            painter.rect_filled(rect, CornerRadius::same(2), if lit { theme::tint(theme::ACCENT, 150) } else { theme::white(14) });
        }
    }

    let (title, hint) = if searching {
        ("No matches", format!("Nothing in this view matches \u{201C}{}\u{201D}.", app.search.trim()))
    } else {
        match app.filter {
            Filter::All => ("No downloads yet", "Paste a link anywhere in this window, or let the browser extension hand them over.".to_owned()),
            Filter::Active => ("Nothing in progress", "Downloads that are running, queued or paused show up here.".to_owned()),
            Filter::Scheduled => ("Nothing set aside", "Choose \u{201C}Download later\u{201D} on a download and it waits here until the queue starts, by hand or on a schedule.".to_owned()),
            Filter::Completed => ("Nothing finished yet", "Completed downloads are listed here.".to_owned()),
            Filter::Attention => ("All clear", "Expired links and failed downloads would wait for you here.".to_owned()),
            Filter::Category(category) => ("Nothing here", format!("No {} in your downloads.", category.label().to_lowercase())),
        }
    };
    theme::text_in(&painter, Rect::from_center_size(egui::pos2(c.x, c.y - 30.0), Vec2::new(body.width() - 80.0, 24.0)), Anchor::Center, title, Type::TITLE, theme::TEXT);
    let hint_galley = theme::layout_centered(&painter, &hint, Type::BODY, theme::TEXT_2, 360.0_f32.min(body.width() - 60.0), 3);
    let hint_size = hint_galley.size();
    painter.galley(egui::pos2(c.x, c.y - 10.0), hint_galley, theme::TEXT_2);

    let buttons = Rect::from_center_size(egui::pos2(c.x, c.y + hint_size.y + 22.0), Vec2::new(360.0, 30.0));
    theme::place(ui, buttons, egui::Layout::left_to_right(egui::Align::Center), |ui| {
        if searching {
            let w = 110.0;
            ui.add_space((buttons.width() - w) / 2.0);
            if Button::new("Clear search", ButtonKind::Secondary).min_width(w).show(ui).clicked() {
                app.search.clear();
            }
        } else if app.filter == Filter::All {
            let w = 150.0;
            ui.add_space((buttons.width() - w) / 2.0);
            if Button::new("New download", ButtonKind::Primary).icon(Icon::Plus).min_width(w).show(ui).clicked() {
                app.actions.push(Action::NewDownload(None));
            }
        } else {
            let w = 150.0;
            ui.add_space((buttons.width() - w) / 2.0);
            if Button::new("Show all downloads", ButtonKind::Secondary).min_width(w).show(ui).clicked() {
                app.filter = Filter::All;
            }
        }
    });
}
