//! The inspector: everything about one download, led by its byte map.

use egui::{Color32, CornerRadius, CursorIcon, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use siphon_core::{DownloadView, SegmentSnapshot, Status, unix_now};

use crate::app::{Action, App};
use crate::format;
use crate::icons::{self, Icon};
use crate::list::status_style;
use crate::theme::{self, Anchor, Type, Weight};
use crate::widgets::{self, Button, ButtonKind};

const PAD: f32 = 18.0;
const MAP_COLS: usize = 32;
const MAP_ROWS: usize = 7;

pub fn show(app: &mut App, ui: &mut Ui) {
    let rect = ui.max_rect();
    let Some(view) = app.focus.and_then(|id| app.view(id)).cloned() else { return };

    // Close button, in the title strip.
    theme::region(ui, Rect::from_min_size(egui::pos2(rect.right() - 12.0 - 28.0, rect.top() + 12.0), Vec2::splat(28.0)), |ui| {
        if widgets::icon_button(ui, Icon::Close, "Close").clicked() {
            app.focus = None;
            app.selection.clear();
        }
    });
    if app.selection.len() > 1 {
        many_selected(app, ui, rect);
        return;
    }

    let content = Rect::from_min_max(egui::pos2(rect.left() + PAD, rect.top() + 14.0), egui::pos2(rect.right() - PAD, rect.bottom()));
    theme::region(ui, content, |ui| {
        egui::ScrollArea::vertical().id_salt("inspector").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.set_width(content.width());
            header(ui, &view);
            ui.add_space(14.0);
            actions(app, ui, &view);
            ui.add_space(18.0);

            if let Some(text) = notice(&view) {
                widgets::callout(ui, text.0, text.1, &text.2, &text.3);
                ui.add_space(16.0);
            }

            widgets::section(ui, "BYTE MAP");
            ui.add_space(10.0);
            byte_map(ui, &view, &app.details.details.segments);
            ui.add_space(18.0);

            if view.status == Status::Downloading && app.details.details.segments.iter().any(|s| s.active) {
                widgets::section(ui, "CONNECTIONS");
                ui.add_space(8.0);
                connections(ui, &app.details.details.segments);
                ui.add_space(18.0);
            }

            stats(ui, &view);
            ui.add_space(18.0);

            if !view.status.is_finished() {
                widgets::section(ui, "SPEED");
                ui.add_space(10.0);
                sparkline(ui, &view, &app.details.details.speed_history);
                ui.add_space(18.0);
            }

            widgets::section(ui, "DETAILS");
            ui.add_space(6.0);
            detail(app, ui, "Source", &format::safe_url(&view.url), Some(Action::CopyLink(view.id)));
            if let Some(page) = &view.referer {
                detail(app, ui, "Page", page, Some(Action::OpenSource(view.id)));
            }
            detail(app, ui, if view.status.is_finished() { "Saved to" } else { "Saving to" }, &format::home_relative(&view.dir), Some(Action::Reveal(view.id)));
            if let Some(staged) = view.staged_at.as_ref().and_then(|p| p.parent()) {
                // Chosen after it started: the bytes follow when it is done.
                detail(app, ui, "For now in", &format::home_relative(staged), None);
            }
            if view.status.is_finished() {
                detail(app, ui, "Checksum", "Verify\u{2026}", Some(Action::Verify(view.id)));
            }
            detail(app, ui, "Added", &format::ago(view.created_at, unix_now()), None);
            if let Some(done) = view.finished_at {
                detail(app, ui, "Finished", &format::ago(done, unix_now()), None);
            }
            // The generic "some bytes" type says nothing worth a line.
            if let Some(kind) = view.content_type.as_deref().map(|k| k.split(';').next().unwrap_or(k).trim()).filter(|k| *k != "application/octet-stream" && !k.is_empty()) {
                detail(app, ui, "Type", kind, None);
            }
            ui.add_space(20.0);
        });
    });
}

fn many_selected(app: &mut App, ui: &mut Ui, rect: Rect) {
    let ids: Vec<_> = app.selection.iter().copied().collect();
    let content = Rect::from_min_max(egui::pos2(rect.left() + PAD, rect.top() + 18.0), egui::pos2(rect.right() - PAD, rect.bottom()));
    theme::region(ui, content, |ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        let (title, _) = ui.allocate_exact_size(Vec2::new(content.width() - 40.0, 22.0), Sense::hover());
        theme::text_in(ui.painter(), title, Anchor::Left, &format!("{} downloads selected", ids.len()), Type::TITLE, theme::TEXT);
        let total: u64 = ids.iter().filter_map(|id| app.view(*id)).filter_map(|v| v.total).sum();
        let (sub, _) = ui.allocate_exact_size(Vec2::new(content.width(), 18.0), Sense::hover());
        theme::text_in(ui.painter(), sub, Anchor::Left, &format!("{} in total", format::bytes(total)), Type::SMALL, theme::TEXT_2);
        ui.add_space(8.0);
        if Button::new("Resume", ButtonKind::Secondary).icon(Icon::Play).fill().show(ui).clicked() {
            app.actions.push(Action::Resume(ids.clone()));
        }
        if Button::new("Pause", ButtonKind::Secondary).icon(Icon::Pause).fill().show(ui).clicked() {
            app.actions.push(Action::Pause(ids.clone()));
        }
        if Button::new("Remove\u{2026}", ButtonKind::Danger).icon(Icon::Trash).fill().show(ui).clicked() {
            app.actions.push(Action::AskRemove(ids));
        }
    });
}

fn header(ui: &mut Ui, view: &DownloadView) {
    let width = ui.available_width();
    let (status_text, status_color) = status_style(view);
    let name = theme::layout_wrapped(ui.painter(), &view.file_name, Type::TITLE, theme::TEXT, width - 44.0 - 34.0, 3);
    let height = name.size().y.max(20.0) + 30.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height.max(40.0)), Sense::hover());

    let tile = Rect::from_min_size(rect.min, Vec2::splat(34.0));
    ui.painter().rect(tile, CornerRadius::same(8), theme::RAISED, Stroke::new(1.0, theme::white(22)), StrokeKind::Inside);
    icons::paint(ui.painter(), tile, Icon::for_category(view.category), 17.0, theme::TEXT_2);

    let x = tile.right() + 12.0;
    let name_h = name.size().y;
    ui.painter().galley(egui::pos2(x, rect.top() - 1.0), name, theme::TEXT);
    let pill = widgets::pill(ui, egui::pos2(x, rect.top() + name_h + 15.0), &status_text, status_color);
    let tail = match (&view.status, view.fraction) {
        (Status::Completed, _) => view.total.map(format::bytes),
        (_, Some(f)) => Some(format::percent(f)),
        _ => None,
    };
    if let Some(tail) = tail {
        theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(pill.right() + 9.0, pill.top()), Vec2::new(140.0, pill.height())), Anchor::Left, &tail, Type::NUM, theme::TEXT_2);
    }
}

fn actions(app: &mut App, ui: &mut Ui, view: &DownloadView) {
    let width = ui.available_width();
    let (row, _) = ui.allocate_exact_size(Vec2::new(width, theme::CONTROL_H), Sense::hover());
    theme::place(ui, row, egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if widgets::icon_button(ui, Icon::Trash, "Remove").clicked() {
            app.actions.push(Action::AskRemove(vec![view.id]));
        }
        if widgets::icon_button(ui, Icon::Link, "Copy link").clicked() {
            app.actions.push(Action::CopyLink(view.id));
        }
        if widgets::icon_button(ui, Icon::Folder, "Show in folder").clicked() {
            app.actions.push(Action::Reveal(view.id));
        }
        if view.status.is_finished() {
            if widgets::icon_button(ui, Icon::CheckCircle, "Verify checksum").clicked() {
                app.actions.push(Action::Verify(view.id));
            }
        } else if view.status != Status::Scheduled && !matches!(view.status, Status::NeedsRefresh { .. }) && widgets::icon_button(ui, Icon::Clock, "Download later").clicked() {
            app.actions.push(Action::Later(vec![view.id]));
        }
        ui.add_space(4.0);
        // The primary action is whatever moves this download forward.
        let primary = |ui: &mut Ui, label: &str, icon: Icon, kind: ButtonKind| Button::new(label, kind).icon(icon).fill().show(ui).clicked();
        match &view.status {
            Status::Completed => {
                if primary(ui, "Open", Icon::External, ButtonKind::Primary) {
                    app.actions.push(Action::Open(view.id));
                }
            }
            Status::NeedsRefresh { code: 401 } => {
                if primary(ui, "Sign in", Icon::Play, ButtonKind::Primary) {
                    app.actions.push(Action::Login(view.id));
                }
            }
            // A video page has no link to swap: the page is simply read again.
            Status::NeedsRefresh { .. } if view.protocol == siphon_core::Protocol::Page => {
                if primary(ui, "Try again", Icon::Refresh, ButtonKind::Primary) {
                    app.actions.push(Action::Retry(view.id));
                }
            }
            Status::NeedsRefresh { .. } => {
                if primary(ui, "Refresh link", Icon::Refresh, ButtonKind::Primary) {
                    app.actions.push(Action::SwapLink(view.id));
                }
            }
            Status::Scheduled => {
                if primary(ui, "Start now", Icon::Play, ButtonKind::Primary) {
                    app.actions.push(Action::Resume(vec![view.id]));
                }
            }
            status if status.can_pause() || *status == Status::Assembling => {
                ui.add_enabled_ui(*status != Status::Assembling, |ui| {
                    if primary(ui, "Pause", Icon::Pause, ButtonKind::Secondary) {
                        app.actions.push(Action::Pause(vec![view.id]));
                    }
                });
            }
            Status::Failed { retryable: false, .. } => {
                if primary(ui, "Refresh link", Icon::Refresh, ButtonKind::Secondary) {
                    app.actions.push(Action::SwapLink(view.id));
                }
            }
            _ => {
                let label = if matches!(view.status, Status::Failed { .. }) { "Retry now" } else { "Resume" };
                if primary(ui, label, Icon::Play, ButtonKind::Primary) {
                    app.actions.push(Action::Resume(vec![view.id]));
                }
            }
        }
    });
}

/// What the user should know right now, if anything.
fn notice(view: &DownloadView) -> Option<(Color32, Icon, String, String)> {
    let kept = match view.fraction {
        Some(f) if view.downloaded > 0 => format!(" Siphon keeps the {} already saved and continues from there.", format::percent(f)),
        _ => String::new(),
    };
    match &view.status {
        Status::NeedsRefresh { code: 401 } => Some((
            theme::WARN,
            Icon::Alert,
            "This server asks for a login".to_owned(),
            format!("Sign in with the account for this site and the download carries on.{kept}"),
        )),
        Status::NeedsRefresh { code } => Some((
            theme::WARN,
            Icon::Alert,
            format!("{} (HTTP {code})", format::expiry_reason(*code)),
            format!("Refresh link opens the page it came from and takes the new link when you start the download there again.{kept}"),
        )),
        Status::Scheduled => Some((
            theme::TEXT_2,
            Icon::Clock,
            "Waiting for the queue".to_owned(),
            "It starts when the queue does, by hand or on its schedule. Start now takes it out of the queue.".to_owned(),
        )),
        Status::Failed { message, .. } => Some((
            theme::BAD,
            Icon::Alert,
            message.clone(),
            match view.retry_in {
                Some(secs) => format!("Trying again in {}.{kept}", format::duration(secs)),
                None => kept.trim().to_owned(),
            },
        )),
        status if !status.is_finished() && view.resumable == Some(false) => Some((
            theme::TEXT_2,
            Icon::Clock,
            "This server cannot resume".to_owned(),
            "It sends the file in one piece only. Pausing means starting over.".to_owned(),
        )),
        _ => None,
    }
}

/// The file as a grid of cells. A cell lights up as its bytes land, and the
/// cell each connection is currently writing glows.
fn byte_map(ui: &mut Ui, view: &DownloadView, segments: &[SegmentSnapshot]) {
    let width = ui.available_width();
    let gap = 2.0;
    let cell = ((width + gap) / MAP_COLS as f32 - gap).floor();
    let grid_w = MAP_COLS as f32 * (cell + gap) - gap;
    let grid_h = MAP_ROWS as f32 * (cell + gap) - gap;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, grid_h + 24.0), Sense::hover());
    let origin = egui::pos2(rect.left() + ((width - grid_w) / 2.0).floor(), rect.top());
    let painter = ui.painter();
    let cells = MAP_COLS * MAP_ROWS;
    let time = ui.input(|i| i.time) as f32;

    let finished = view.status.is_finished();
    let live = view.status == Status::Downloading;
    let base = if finished {
        theme::OK
    } else if matches!(view.status, Status::NeedsRefresh { .. }) {
        theme::WARN
    } else {
        theme::ACCENT
    };

    // Share of each cell that is on disk, and whether a write head is in it.
    let mut fill = vec![0.0f32; cells];
    let mut head = vec![false; cells];
    let total = view.total.filter(|t| *t > 0);
    match (total, finished) {
        (_, true) => fill.fill(1.0),
        (Some(total), false) if !segments.is_empty() => {
            let per_cell = total as f64 / cells as f64;
            for segment in segments {
                if segment.pos > segment.start {
                    let first = (segment.start as f64 / per_cell) as usize;
                    let last = (((segment.pos - 1) as f64 / per_cell) as usize).min(cells - 1);
                    for (index, slot) in fill.iter_mut().enumerate().take(last + 1).skip(first) {
                        let lo = (index as f64 * per_cell).max(segment.start as f64);
                        let hi = ((index + 1) as f64 * per_cell).min(segment.pos as f64);
                        *slot += ((hi - lo) / per_cell).max(0.0) as f32;
                    }
                }
                if segment.active && segment.pos < segment.end {
                    head[((segment.pos as f64 / per_cell) as usize).min(cells - 1)] = true;
                }
            }
        }
        (Some(_), false) => {
            // No map (a single stream): bytes arrive front to back.
            let f = view.fraction.unwrap_or(0.0) * cells as f32;
            for (index, slot) in fill.iter_mut().enumerate() {
                *slot = (f - index as f32).clamp(0.0, 1.0);
            }
            if live {
                head[(f as usize).min(cells - 1)] = true;
            }
        }
        (None, false) => {}
    }

    for index in 0..cells {
        let (row, col) = (index / MAP_COLS, index % MAP_COLS);
        let r = Rect::from_min_size(egui::pos2(origin.x + col as f32 * (cell + gap), origin.y + row as f32 * (cell + gap)), Vec2::splat(cell));
        let amount = fill[index].clamp(0.0, 1.0);
        let mut color = theme::mix(theme::white(15), theme::tint(base, if finished { 150 } else { 205 }), amount);
        if total.is_none() && live {
            // Size unknown: a soft wave says "working" without claiming progress.
            let phase = (time * 2.2 - index as f32 * 0.16).sin() * 0.5 + 0.5;
            color = theme::mix(theme::white(13), theme::tint(base, 120), phase * phase);
        }
        painter.rect_filled(r, CornerRadius::same(2), color);
        if head[index] && live {
            let pulse = (time * 5.0 + index as f32).sin() * 0.5 + 0.5;
            painter.rect_filled(r.expand(1.0), CornerRadius::same(3), theme::mix(theme::ACCENT_HI, Color32::WHITE, 0.35 + pulse * 0.45));
        }
    }
    if live {
        ui.ctx().request_repaint();
    }

    let caption_rect = Rect::from_min_size(egui::pos2(rect.left(), origin.y + grid_h + 8.0), Vec2::new(width, 16.0));
    let caption = match (&view.status, total) {
        (Status::Completed, _) => "Every byte is on disk".to_owned(),
        (_, None) if live => "The server did not announce a size".to_owned(),
        (_, None) => "Size not announced".to_owned(),
        (Status::Downloading, Some(_)) => {
            let ranges = segments.iter().filter(|s| !s.is_complete()).count();
            match view.connections {
                0 => "Connecting".to_owned(),
                1 if segments.is_empty() => "1 connection, sequential".to_owned(),
                n => format!("{n} connections  \u{00B7}  {ranges} ranges open"),
            }
        }
        (_, Some(_)) => {
            let ranges = segments.iter().filter(|s| !s.is_complete()).count();
            match ranges {
                0 => "Waiting to start".to_owned(),
                1 => "1 range left to fetch".to_owned(),
                n => format!("{n} ranges left to fetch"),
            }
        }
    };
    theme::text_in(painter, caption_rect, Anchor::Left, &caption, Type::SMALL, theme::TEXT_2);
}

/// Four numbers in one divided strip.
fn stats(ui: &mut Ui, view: &DownloadView) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 112.0), Sense::hover());
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(theme::R_PANEL), theme::FIELD, Stroke::new(1.0, theme::white(18)), StrokeKind::Inside);
    painter.vline(rect.center().x, rect.y_range().shrink(1.0), Stroke::new(1.0, theme::white(14)));
    painter.hline(rect.x_range().shrink(1.0), rect.center().y, Stroke::new(1.0, theme::white(14)));

    let dash = "\u{2014}".to_owned();
    let downloaded = format::bytes(view.downloaded);
    let cells: [(&str, String, Color32); 4] = [
        ("SIZE", view.total.map(format::bytes).unwrap_or_else(|| "Unknown".into()), if view.total.is_some() { theme::TEXT } else { theme::TEXT_3 }),
        ("DOWNLOADED", downloaded, theme::TEXT),
        (
            "SPEED",
            if view.status == Status::Downloading { format::speed(view.bytes_per_sec) } else { dash.clone() },
            if view.status == Status::Downloading { theme::TEXT } else { theme::TEXT_3 },
        ),
        (
            "TIME LEFT",
            match (&view.status, view.eta_secs) {
                (Status::Completed, _) => "Done".into(),
                (_, Some(secs)) => format::duration(secs),
                _ => dash,
            },
            if view.eta_secs.is_some() || view.status.is_finished() { theme::TEXT } else { theme::TEXT_3 },
        ),
    ];
    for (index, (label, value, color)) in cells.iter().enumerate() {
        let cell = Rect::from_min_size(
            egui::pos2(rect.left() + (index % 2) as f32 * rect.width() / 2.0, rect.top() + (index / 2) as f32 * rect.height() / 2.0),
            Vec2::new(rect.width() / 2.0, rect.height() / 2.0),
        );
        let inner = cell.shrink2(Vec2::new(14.0, 0.0));
        theme::text_in(painter, Rect::from_min_size(egui::pos2(inner.left(), cell.top() + 9.0), Vec2::new(inner.width(), 14.0)), Anchor::Left, label, Type::LABEL, theme::TEXT_3);
        // Split number and unit so the unit can step back.
        let (number, unit) = match value.rsplit_once(' ') {
            Some((n, u)) if n.chars().next().is_some_and(|c| c.is_ascii_digit()) && !u.chars().any(|c| c.is_ascii_digit()) => (n, u),
            _ => (value.as_str(), ""),
        };
        let line = Rect::from_min_size(egui::pos2(inner.left(), cell.top() + 26.0), Vec2::new(inner.width(), 22.0));
        let w = theme::text_in(painter, line, Anchor::Left, number, Type::STAT, *color);
        if !unit.is_empty() {
            theme::text_in(painter, Rect::from_min_max(egui::pos2(line.left() + w + 4.0, line.top() + 3.0), line.max), Anchor::Left, unit, Type::mono(11.5, Weight::Regular), theme::TEXT_2);
        }
    }
}

/// One line per connection: which part of the file it is fetching and how
/// far along that part is.
fn connections(ui: &mut Ui, segments: &[SegmentSnapshot]) {
    const SHOWN: usize = 12;
    let active: Vec<&SegmentSnapshot> = segments.iter().filter(|s| s.active).collect();
    for (index, segment) in active.iter().take(SHOWN).enumerate() {
        let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
        let painter = ui.painter();
        theme::text_in(painter, Rect::from_min_size(row.min, Vec2::new(22.0, row.height())), Anchor::Left, &format!("{}", index + 1), Type::mono(11.0, Weight::Regular), theme::TEXT_3);
        let fraction = if segment.is_empty() { 1.0 } else { segment.done() as f32 / segment.len() as f32 };
        let bar = Rect::from_min_size(egui::pos2(row.left() + 22.0, row.center().y - 1.5), Vec2::new(52.0, 3.0));
        widgets::progress(ui, bar, Some(fraction), theme::ACCENT);
        // Where in the file this connection is working, then how far along it is.
        let place = format!("at {}", format::bytes(segment.start));
        let place_w = theme::text_in(painter, row, Anchor::Right, &place, Type::mono(11.0, Weight::Regular), theme::TEXT_3);
        let label = format!("{} of {}", format::bytes(segment.done()), format::bytes(segment.len()));
        let label_rect = Rect::from_min_max(egui::pos2(bar.right() + 10.0, row.top()), egui::pos2(row.right() - place_w - 8.0, row.bottom()));
        theme::text_in(painter, label_rect, Anchor::Left, &label, Type::mono(11.0, Weight::Regular), theme::TEXT_2);
    }
    if active.len() > SHOWN {
        let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
        theme::text_in(ui.painter(), row, Anchor::Left, &format!("and {} more", active.len() - SHOWN), Type::SMALL, theme::TEXT_3);
    }
}

/// Recent speed as a filled line.
fn sparkline(ui: &mut Ui, view: &DownloadView, history: &[f32]) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 64.0), Sense::hover());
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(theme::R_PANEL), theme::FIELD, Stroke::new(1.0, theme::white(18)), StrokeKind::Inside);
    // Inset so the live dot at the right end is not cut by the border.
    let plot = Rect::from_min_max(egui::pos2(rect.left() + 1.0, rect.top() + 20.0), egui::pos2(rect.right() - 10.0, rect.bottom() - 8.0));
    let peak = history.iter().copied().fold(0.0f32, f32::max);

    if history.len() < 2 || peak <= 0.0 {
        let text = if view.status.is_active() { "Measuring\u{2026}" } else { "No recent activity" };
        theme::text_in(painter, rect, Anchor::Center, text, Type::SMALL, theme::TEXT_3);
        return;
    }
    // Headroom above the peak, so the line never rides the border.
    let scale = peak * 1.18;
    let step = plot.width() / (history.len() - 1) as f32;
    let points: Vec<egui::Pos2> = history.iter().enumerate().map(|(i, v)| egui::pos2(plot.left() + i as f32 * step, plot.bottom() - (v / scale) * plot.height())).collect();

    // Fill: one quad per interval, so the shape need not be convex.
    for pair in points.windows(2) {
        painter.add(egui::Shape::convex_polygon(
            vec![pair[0], pair[1], egui::pos2(pair[1].x, plot.bottom()), egui::pos2(pair[0].x, plot.bottom())],
            theme::tint(theme::ACCENT, 34),
            Stroke::NONE,
        ));
    }
    painter.add(egui::Shape::line(points.clone(), Stroke::new(1.5, theme::ACCENT)));
    if let Some(last) = points.last().filter(|_| view.status == Status::Downloading) {
        painter.circle_filled(*last, 3.0, theme::ACCENT_HI);
        painter.circle_stroke(*last, 5.5, Stroke::new(1.0, theme::tint(theme::ACCENT_HI, 90)));
    }
    theme::text_in(painter, Rect::from_min_size(egui::pos2(rect.left() + 8.0, rect.top() + 4.0), Vec2::new(160.0, 14.0)), Anchor::Left, &format!("peak {}", format::speed(peak as f64)), Type::mono(10.5, Weight::Regular), theme::TEXT_3);
}

/// One labelled value. Clicking it performs the obvious action.
fn detail(app: &mut App, ui: &mut Ui, label: &str, value: &str, action: Option<Action>) {
    let width = ui.available_width();
    let value_w = width - 76.0;
    let galley = theme::layout_wrapped(ui.painter(), value, Type::SMALL, theme::TEXT, value_w, 2);
    let height = galley.size().y.max(16.0) + 12.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(rect.left(), rect.top() + 6.0), Vec2::new(70.0, 16.0)), Anchor::Left, label, Type::SMALL, theme::TEXT_3);
    let value_rect = Rect::from_min_size(egui::pos2(rect.left() + 76.0, rect.top() + 6.0), galley.size());

    let mut color = theme::TEXT;
    if let Some(action) = action {
        let response = ui.interact(value_rect.expand(3.0), ui.scope_id().with(("detail", label)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        if response.hovered() {
            color = theme::ACCENT_HI;
        }
        if response.clicked() {
            app.actions.push(action);
        }
        let hint = match label {
            "Source" => "Click to copy",
            "Page" => "Open in the browser",
            "Checksum" => "Compute it and compare it with the published one",
            _ => "Show in folder",
        };
        response.on_hover_text(hint);
    }
    let galley = if color == theme::TEXT { galley } else { theme::layout_wrapped(ui.painter(), value, Type::SMALL, color, value_w, 2) };
    ui.painter().galley(value_rect.min, galley, color);
}
