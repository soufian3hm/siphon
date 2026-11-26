//! The sidebar: the primary action, the library filters with live counts,
//! and the global speed readout.

use egui::{CornerRadius, CursorIcon, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use siphon_core::{Category, Status};

use crate::app::{Action, App, Filter};
use crate::icons::{self, Icon};
use crate::theme::{self, Anchor, Type, Weight};
use crate::widgets::{self, Button, ButtonKind};
use crate::format;

const PAD: f32 = 12.0;

pub fn show(app: &mut App, ui: &mut Ui) {
    let rect = ui.max_rect();
    let mut y = rect.top() + if cfg!(target_os = "macos") { theme::TITLEBAR_H } else { 14.0 };

    // Primary action.
    let button = Rect::from_min_size(egui::pos2(rect.left() + PAD, y), Vec2::new(rect.width() - PAD * 2.0, theme::CONTROL_H + 2.0));
    theme::region(ui, button, |ui| {
        let keys = if cfg!(target_os = "macos") { "\u{2318}N" } else { "Ctrl N" };
        if Button::new("New download", ButtonKind::Primary).icon(Icon::Plus).shortcut(keys).fill().show(ui).clicked() {
            app.actions.push(Action::NewDownload(None));
        }
    });
    y = button.bottom() + 18.0;

    // Counts are taken before drawing: the rows below need `app` mutably.
    let count = |filter: Filter| app.views.iter().filter(|v| filter.matches(v)).count();
    let library = [
        (Filter::All, Icon::Stack),
        (Filter::Active, Icon::Download),
        (Filter::Scheduled, Icon::Clock),
        (Filter::Completed, Icon::CheckCircle),
        (Filter::Attention, Icon::Alert),
    ]
    .map(|(filter, icon)| (filter, icon, count(filter)));
    let types = Category::ALL.map(|category| (category, count(Filter::Category(category))));

    y = heading(ui, rect, y, "LIBRARY");
    for (filter, icon, n) in library {
        let flagged = filter == Filter::Attention && n > 0;
        y = item(app, ui, rect, y, filter, icon, n, flagged);
    }

    y += 14.0;
    y = heading(ui, rect, y, "TYPES");
    for (category, n) in types {
        // Empty families stay out of the way unless one is the current view.
        if n == 0 && app.filter != Filter::Category(category) {
            continue;
        }
        y = item(app, ui, rect, y, Filter::Category(category), Icon::for_category(category), n, false);
    }
    if app.views.is_empty() {
        theme::text_in(
            ui.painter(),
            Rect::from_min_size(egui::pos2(rect.left() + PAD + 8.0, y), Vec2::new(rect.width() - PAD * 2.0 - 8.0, 24.0)),
            Anchor::Left,
            "Appear as you download",
            Type::SMALL,
            theme::TEXT_3,
        );
    }

    footer(app, ui, rect);
}

fn heading(ui: &Ui, rect: Rect, y: f32, text: &str) -> f32 {
    let row = Rect::from_min_size(egui::pos2(rect.left() + PAD + 8.0, y), Vec2::new(rect.width() - PAD * 2.0 - 8.0, 22.0));
    theme::text_in(ui.painter(), row, Anchor::Left, text, Type::LABEL, theme::TEXT_3);
    row.bottom() + 2.0
}

#[allow(clippy::too_many_arguments)]
fn item(app: &mut App, ui: &mut Ui, rect: Rect, y: f32, filter: Filter, icon: Icon, count: usize, flagged: bool) -> f32 {
    let row = Rect::from_min_size(egui::pos2(rect.left() + PAD, y), Vec2::new(rect.width() - PAD * 2.0, 28.0));
    let response = ui.interact(row, ui.scope_id().with(("filter", filter.title())), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let active = app.filter == filter;
    let t = ui.ctx().animate_bool_with_time(response.id.with("a"), active, 0.12);
    let h = ui.ctx().animate_bool_with_time(response.id.with("h"), response.hovered() && !active, 0.1);

    if t > 0.0 {
        // The current view is a raised key, like every other pressed-in control.
        widgets::key_surface(ui, row, theme::R_CONTROL, theme::tint(theme::HOVER, (t * 255.0) as u8), theme::white((t * 26.0) as u8), theme::white((t * 16.0) as u8));
    } else if h > 0.0 {
        ui.painter().rect_filled(row, CornerRadius::same(theme::R_CONTROL), theme::white((h * 10.0) as u8));
    }
    if response.has_focus() {
        ui.painter().rect_stroke(row.expand(1.0), CornerRadius::same(theme::R_CONTROL + 1), Stroke::new(1.5, theme::ACCENT_HI), StrokeKind::Outside);
    }

    let fg = theme::mix(theme::mix(theme::TEXT_2, theme::TEXT, h), theme::TEXT, t);
    let icon_color = if flagged { theme::WARN } else { theme::mix(theme::TEXT_3, theme::ACCENT_HI, t).gamma_multiply(1.0) };
    icons::paint(ui.painter(), Rect::from_min_size(egui::pos2(row.left() + 6.0, row.top()), Vec2::new(20.0, row.height())), icon, 15.0, icon_color);
    let ty = if active { Type::BODY_MEDIUM } else { Type::BODY };
    theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(row.left() + 34.0, row.top()), egui::pos2(row.right() - 40.0, row.bottom())), Anchor::Left, filter.title(), ty, fg);

    if count > 0 {
        let label = count.to_string();
        let slot = Rect::from_min_max(egui::pos2(row.right() - 44.0, row.top()), egui::pos2(row.right() - 9.0, row.bottom()));
        if flagged {
            // Something waits for the user: the count becomes a badge.
            let w = theme::measure(ui.painter(), &label, Type::mono(11.0, Weight::Semibold)) + 12.0;
            let badge = Rect::from_min_size(egui::pos2(slot.right() - w, slot.center().y - 9.0), Vec2::new(w, 18.0));
            ui.painter().rect_filled(badge, CornerRadius::same(9), theme::tint(theme::WARN, 46));
            theme::text_in(ui.painter(), badge, Anchor::Center, &label, Type::mono(11.0, Weight::Semibold), theme::WARN);
        } else {
            theme::text_in(ui.painter(), slot, Anchor::Right, &label, Type::NUM, theme::mix(theme::TEXT_3, theme::TEXT_2, t));
        }
    }

    if response.clicked() {
        app.filter = filter;
    }
    row.bottom() + 2.0
}

/// Total speed, what is running, and the two global switches.
fn footer(app: &mut App, ui: &mut Ui, rect: Rect) {
    let height = 112.0;
    let area = Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - height), rect.max);
    ui.painter().hline(area.x_range().shrink(PAD), area.top(), theme::hairline());

    let speed = app.manager.total_speed();
    let running = app.views.iter().filter(|v| v.status == Status::Downloading).count();
    let queued = app.views.iter().filter(|v| v.status == Status::Queued).count();
    let settings = app.manager.settings();

    let x = area.left() + PAD + 8.0;
    let w = area.width() - (PAD + 8.0) * 2.0;
    theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(x, area.top() + 12.0), Vec2::new(w, 16.0)), Anchor::Left, "SPEED", Type::LABEL, theme::TEXT_3);

    // Big tabular readout; the unit is quieter than the number.
    let text = format::speed(speed);
    let (number, unit) = text.split_once(' ').unwrap_or((text.as_str(), ""));
    let line = Rect::from_min_size(egui::pos2(x, area.top() + 30.0), Vec2::new(w, 26.0));
    let color = if speed > 0.0 { theme::TEXT } else { theme::TEXT_3 };
    let nw = theme::text_in(ui.painter(), line, Anchor::Left, number, Type::mono(20.0, Weight::Medium), color);
    theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(x + nw + 5.0, line.top() + 5.0), line.max), Anchor::Left, unit, Type::mono(12.0, Weight::Regular), theme::TEXT_2);

    let summary = match (running, queued) {
        (0, 0) => "Idle".to_owned(),
        (r, 0) => format!("{r} downloading"),
        (0, q) => format!("{q} queued"),
        (r, q) => format!("{r} downloading  \u{00B7}  {q} queued"),
    };
    theme::text_in(ui.painter(), Rect::from_min_size(egui::pos2(x, area.top() + 58.0), Vec2::new(w, 16.0)), Anchor::Left, &summary, Type::SMALL, theme::TEXT_2);

    // Limiter chip and settings.
    let row = Rect::from_min_size(egui::pos2(area.left() + PAD, area.bottom() - 38.0), Vec2::new(area.width() - PAD * 2.0, theme::CONTROL_H));
    let chip = Rect::from_min_max(row.min, egui::pos2(row.right() - 34.0, row.bottom()));
    let response = ui.interact(chip, ui.scope_id().with("limiter"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
    let on = settings.speed_limit_enabled;
    let t = ui.ctx().animate_bool_with_time(response.id.with("on"), on, 0.12);
    let h = ui.ctx().animate_bool_with_time(response.id.with("h"), response.hovered(), 0.1);
    ui.painter().rect(
        chip,
        CornerRadius::same(theme::R_CONTROL),
        theme::mix(theme::mix(theme::FIELD, theme::RAISED, h), theme::tint(theme::WARN, 30), t),
        Stroke::new(1.0, theme::mix(theme::white(24), theme::tint(theme::WARN, 90), t)),
        StrokeKind::Inside,
    );
    icons::paint(ui.painter(), Rect::from_min_size(chip.min, Vec2::new(28.0, chip.height())), Icon::Bolt, 13.0, theme::mix(theme::TEXT_3, theme::WARN, t));
    let label = if on { format!("Limited to {}", format::speed(settings.speed_limit as f64)) } else { "No speed limit".to_owned() };
    theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(chip.left() + 28.0, chip.top()), egui::pos2(chip.right() - 6.0, chip.bottom())), Anchor::Left, &label, Type::SMALL_MEDIUM, theme::mix(theme::TEXT_2, theme::TEXT, t));
    if response.clicked() {
        let mut next = settings.clone();
        next.speed_limit_enabled = !on;
        if let Err(e) = app.manager.set_settings(next) {
            app.toast_error(e.to_string());
        }
    }
    let response = response.on_hover_text(if on { "Click to lift the speed limit" } else { "Click to limit download speed" });
    let _ = response;

    theme::region(ui, Rect::from_min_size(egui::pos2(row.right() - 28.0, row.top()), Vec2::splat(28.0)), |ui| {
        if widgets::icon_button(ui, Icon::Sliders, "Settings").clicked() {
            app.actions.push(Action::Settings);
        }
    });
}
