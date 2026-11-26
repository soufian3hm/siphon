//! The control library. Every control has hover, pressed, keyboard-focus and
//! disabled states; none of them is a stock egui widget with a new colour.

use egui::{Color32, CornerRadius, CursorIcon, Frame, Id, Popup, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::icons::{self, Icon};
use crate::theme::{self, Anchor, Type, Weight};

const FADE: f32 = 0.11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// The one action the surface is for.
    Primary,
    /// A raised key.
    Secondary,
    /// No chrome until hovered.
    Ghost,
    /// Destructive.
    Danger,
}

/// Animated 0..1 hover amount for a response.
fn hover_t(ui: &Ui, response: &Response) -> f32 {
    ui.ctx().animate_bool_with_time(response.id.with("hover"), response.hovered() && ui.is_enabled(), FADE)
}

/// The keyboard-focus ring, drawn outside the control.
fn focus_ring(ui: &Ui, rect: Rect, radius: u8, response: &Response) {
    if response.has_focus() {
        ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(radius + 2), Stroke::new(1.5, theme::ACCENT_HI), StrokeKind::Outside);
    }
}

/// A raised surface: fill, 1 px ring and a soft highlight along the top edge.
pub fn key_surface(ui: &Ui, rect: Rect, radius: u8, fill: Color32, ring: Color32, highlight: Color32) {
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(radius), fill, Stroke::new(1.0, ring), StrokeKind::Inside);
    let inset = radius as f32 * 0.6;
    painter.hline((rect.left() + inset)..=(rect.right() - inset), rect.top() + 1.5, Stroke::new(1.0, highlight));
}

pub struct Button<'a> {
    label: &'a str,
    kind: ButtonKind,
    icon: Option<Icon>,
    min_width: f32,
    fill_width: bool,
    shortcut: Option<&'a str>,
}

impl<'a> Button<'a> {
    pub fn new(label: &'a str, kind: ButtonKind) -> Self {
        Self { label, kind, icon: None, min_width: 0.0, fill_width: false, shortcut: None }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }

    /// Stretch to the available width.
    pub fn fill(mut self) -> Self {
        self.fill_width = true;
        self
    }

    /// A keyboard hint shown at the trailing edge.
    pub fn shortcut(mut self, keys: &'a str) -> Self {
        self.shortcut = Some(keys);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let ty = Type::BODY_MEDIUM;
        let label_w = theme::measure(ui.painter(), self.label, ty);
        let icon_w = if self.icon.is_some() { 14.0 + 7.0 } else { 0.0 };
        let hint_w = self.shortcut.map(|s| theme::measure(ui.painter(), s, Type::SMALL) + 12.0).unwrap_or(0.0);
        let natural = (label_w + icon_w + hint_w + 24.0).ceil();
        let width = if self.fill_width { ui.available_width() } else { natural.max(self.min_width) };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, theme::CONTROL_H), Sense::click());
        let response = response.on_hover_cursor(if ui.is_enabled() { CursorIcon::PointingHand } else { CursorIcon::Default });
        if !ui.is_rect_visible(rect) {
            return response;
        }

        let enabled = ui.is_enabled();
        let t = hover_t(ui, &response);
        let down = response.is_pointer_button_down_on() && enabled;
        // Pressing sinks the key by a hair.
        let rect = if down { rect.shrink(0.6) } else { rect };

        let (fill, ring, highlight, fg) = match self.kind {
            ButtonKind::Primary => (
                if down { theme::ACCENT_LO } else { theme::mix(theme::ACCENT, theme::ACCENT_HI, t * 0.35) },
                theme::mix(theme::ACCENT_LO, theme::ACCENT, 0.4),
                theme::white(70),
                Color32::WHITE,
            ),
            ButtonKind::Secondary => (
                if down { theme::PRESSED } else { theme::mix(theme::RAISED, theme::HOVER, t) },
                theme::white(26),
                theme::white(16),
                theme::TEXT,
            ),
            ButtonKind::Ghost => (
                if down { theme::PRESSED } else { theme::tint(theme::HOVER, (t * 255.0) as u8) },
                Color32::TRANSPARENT,
                Color32::TRANSPARENT,
                theme::mix(theme::TEXT_2, theme::TEXT, t),
            ),
            ButtonKind::Danger => (
                if down { theme::tint(theme::BAD, 60) } else { theme::tint(theme::BAD, 30 + (t * 26.0) as u8) },
                theme::tint(theme::BAD, 110),
                theme::white(14),
                theme::mix(theme::BAD, Color32::WHITE, 0.25),
            ),
        };
        let fade = if enabled { 1.0 } else { 0.4 };
        let dim = |c: Color32| c.gamma_multiply(fade);
        key_surface(ui, rect, theme::R_CONTROL, dim(fill), dim(ring), dim(highlight));
        focus_ring(ui, rect, theme::R_CONTROL, &response);

        let content_w = label_w + icon_w;
        let mut x = if self.shortcut.is_some() { rect.left() + 12.0 } else { rect.center().x - content_w / 2.0 };
        if let Some(icon) = self.icon {
            icons::paint(ui.painter(), Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(14.0, rect.height())), icon, 14.0, dim(fg));
            x += 21.0;
        }
        theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(x, rect.top()), rect.right_bottom()), Anchor::Left, self.label, ty, dim(fg));
        if let Some(keys) = self.shortcut {
            let hint = theme::mix(fg, fill, 0.45);
            theme::text_in(ui.painter(), rect.shrink2(Vec2::new(10.0, 0.0)), Anchor::Right, keys, Type::SMALL, dim(hint));
        }
        response
    }
}

/// A square icon-only button with a tooltip.
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    icon_button_sized(ui, icon, tooltip, theme::CONTROL_H, theme::TEXT_2)
}

pub fn icon_button_sized(ui: &mut Ui, icon: Icon, tooltip: &str, size: f32, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let response = response.on_hover_cursor(if ui.is_enabled() { CursorIcon::PointingHand } else { CursorIcon::Default });
    if ui.is_rect_visible(rect) {
        let t = hover_t(ui, &response);
        let down = response.is_pointer_button_down_on() && ui.is_enabled();
        let fill = if down { theme::PRESSED } else { theme::tint(theme::HOVER, (t * 255.0) as u8) };
        ui.painter().rect_filled(rect, CornerRadius::same(theme::R_CONTROL), fill);
        focus_ring(ui, rect, theme::R_CONTROL, &response);
        let fg = theme::mix(color, theme::TEXT, t).gamma_multiply(if ui.is_enabled() { 1.0 } else { 0.35 });
        icons::paint(ui.painter(), rect, icon, 15.0, fg);
    }
    if tooltip.is_empty() { response } else { response.on_hover_text(tooltip) }
}

/// A small status tag: the hue as text on a faint wash of itself.
pub fn pill(ui: &Ui, left_center: egui::Pos2, text: &str, color: Color32) -> Rect {
    let ty = Type::new(11.5, Weight::Medium);
    let w = theme::measure(ui.painter(), text, ty) + 16.0;
    let rect = Rect::from_min_size(egui::pos2(left_center.x, left_center.y - 10.0), Vec2::new(w.ceil(), 20.0));
    ui.painter().rect(rect, CornerRadius::same(5), theme::tint(color, 34), Stroke::new(1.0, theme::tint(color, 46)), StrokeKind::Inside);
    theme::text_in(ui.painter(), rect, Anchor::Center, text, ty, theme::mix(color, Color32::WHITE, 0.12));
    rect
}

/// A thin progress line. `fraction` of `None` draws an indeterminate sweep.
pub fn progress(ui: &Ui, rect: Rect, fraction: Option<f32>, color: Color32) {
    let painter = ui.painter();
    let radius = CornerRadius::same((rect.height() / 2.0).round() as u8);
    painter.rect_filled(rect, radius, theme::white(18));
    match fraction {
        Some(f) => {
            let f = f.clamp(0.0, 1.0);
            if f > 0.0 {
                let w = (rect.width() * f).max(rect.height());
                painter.rect_filled(Rect::from_min_size(rect.min, Vec2::new(w, rect.height())), radius, color);
            }
        }
        None => {
            // A segment that travels the track: progress that cannot be measured.
            let time = ui.input(|i| i.time) as f32;
            let span = rect.width() * 0.28;
            let x = rect.left() - span + ((time * 0.55) % 1.0) * (rect.width() + span);
            let segment = Rect::from_min_max(egui::pos2(x.max(rect.left()), rect.top()), egui::pos2((x + span).min(rect.right()), rect.bottom()));
            if segment.width() > 0.0 {
                painter.rect_filled(segment, radius, color);
            }
            ui.ctx().request_repaint();
        }
    }
}

/// An on/off switch.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(32.0, 18.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_with_time(response.id.with("on"), *on, 0.14);
        let hover = hover_t(ui, &response);
        let off_fill = theme::mix(theme::white(30), theme::white(46), hover);
        let on_fill = theme::mix(theme::ACCENT, theme::ACCENT_HI, hover * 0.3);
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(9), theme::mix(off_fill, on_fill, t));
        focus_ring(ui, rect, 9, &response);
        let knob_x = egui::lerp((rect.left() + 9.0)..=(rect.right() - 9.0), t);
        painter.circle_filled(egui::pos2(knob_x, rect.center().y + 0.5), 7.0, Color32::from_black_alpha(50));
        painter.circle_filled(egui::pos2(knob_x, rect.center().y), 7.0, Color32::WHITE);
    }
    response
}

/// A check box with its label. Clicking either toggles it.
pub fn checkbox(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let label_w = theme::measure(ui.painter(), label, Type::BODY);
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(16.0 + 9.0 + label_w, 20.0), Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    if ui.is_rect_visible(rect) {
        let boxr = Rect::from_min_size(egui::pos2(rect.left(), rect.center().y - 8.0), Vec2::splat(16.0));
        let t = ui.ctx().animate_bool_with_time(response.id.with("on"), *on, 0.1);
        let hover = hover_t(ui, &response);
        let painter = ui.painter();
        painter.rect(
            boxr,
            CornerRadius::same(4),
            theme::mix(theme::FIELD, theme::ACCENT, t),
            Stroke::new(1.0, theme::mix(theme::mix(theme::white(48), theme::white(80), hover), theme::ACCENT_HI, t)),
            StrokeKind::Inside,
        );
        if t > 0.3 {
            icons::paint(painter, boxr, Icon::Check, 12.0, Color32::WHITE.gamma_multiply(t));
        }
        focus_ring(ui, boxr, 4, &response);
        theme::text_in(painter, Rect::from_min_max(egui::pos2(boxr.right() + 9.0, rect.top()), rect.right_bottom()), Anchor::Left, label, Type::BODY, theme::TEXT);
    }
    response
}

/// A single-line text field with an optional leading icon.
pub struct Field<'a> {
    text: &'a mut String,
    hint: &'a str,
    icon: Option<Icon>,
    width: Option<f32>,
    password: bool,
    mono: bool,
    id: Option<Id>,
    invalid: bool,
}

impl<'a> Field<'a> {
    pub fn new(text: &'a mut String) -> Self {
        Self { text, hint: "", icon: None, width: None, password: false, mono: false, id: None, invalid: false }
    }
    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = hint;
        self
    }
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }
    pub fn password(mut self) -> Self {
        self.password = true;
        self
    }
    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }
    pub fn id(mut self, id: Id) -> Self {
        self.id = Some(id);
        self
    }
    /// Draws the field in its error state.
    pub fn invalid(mut self, invalid: bool) -> Self {
        self.invalid = invalid;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let width = self.width.unwrap_or_else(|| ui.available_width());
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, theme::CONTROL_H), Sense::hover());
        let pad_left = if self.icon.is_some() { 30.0 } else { 10.0 };
        let inner = Rect::from_min_max(egui::pos2(rect.left() + pad_left, rect.top()), egui::pos2(rect.right() - 8.0, rect.bottom()));

        let font = if self.mono { egui::FontId::monospace(12.0) } else { egui::FontId::proportional(13.0) };
        let mut edit = egui::TextEdit::singleline(self.text)
            .frame(Frame::new())
            .margin(egui::Margin::ZERO)
            .font(font)
            .text_color(theme::TEXT)
            .desired_width(inner.width())
            .align(egui::Align2::LEFT_CENTER)
            .password(self.password)
            .hint_text(egui::RichText::new(self.hint).color(theme::TEXT_3));
        if let Some(id) = self.id {
            edit = edit.id(id);
        }

        // The field's chrome is painted first, so it needs to know about focus
        // before the editor runs: read it from last frame's memory.
        let edit_id = self.id.unwrap_or_else(|| ui.next_auto_id());
        let focused = ui.memory(|m| m.has_focus(edit_id));
        let hovered = ui.rect_contains_pointer(rect);
        let t = ui.ctx().animate_bool_with_time(edit_id.with("focus"), focused, 0.1);
        let border = if self.invalid {
            theme::tint(theme::BAD, 200)
        } else {
            theme::mix(if hovered { theme::white(44) } else { theme::white(26) }, theme::ACCENT, t)
        };
        ui.painter().rect(rect, CornerRadius::same(theme::R_CONTROL), theme::FIELD, Stroke::new(1.0, border), StrokeKind::Inside);
        if t > 0.0 && !self.invalid {
            ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(theme::R_CONTROL + 2), Stroke::new(2.0, theme::tint(theme::ACCENT, (t * 70.0) as u8)), StrokeKind::Outside);
        }
        if let Some(icon) = self.icon {
            let slot = Rect::from_min_size(rect.min, Vec2::new(30.0, rect.height()));
            icons::paint(ui.painter(), slot, icon, 14.0, theme::mix(theme::TEXT_3, theme::TEXT_2, t));
        }
        theme::place(ui, inner, egui::Layout::left_to_right(egui::Align::Center), |ui| ui.add(edit))
    }
}

/// `-  n  +` for small integers.
pub fn stepper(ui: &mut Ui, value: &mut usize, min: usize, max: usize) -> bool {
    let before = *value;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(104.0, theme::CONTROL_H), Sense::hover());
    key_surface(ui, rect, theme::R_CONTROL, theme::FIELD, theme::white(26), Color32::TRANSPARENT);
    let part = |ui: &mut Ui, r: Rect, icon_plus: bool, enabled: bool| -> bool {
        let response = ui.interact(r, ui.scope_id().with((r.left() as i32, r.top() as i32, icon_plus)), Sense::click());
        let response = response.on_hover_cursor(if enabled { CursorIcon::PointingHand } else { CursorIcon::Default });
        let t = ui.ctx().animate_bool_with_time(response.id.with("h"), response.hovered() && enabled, FADE);
        if t > 0.0 {
            ui.painter().rect_filled(r.shrink(2.0), CornerRadius::same(4), theme::tint(theme::HOVER, (t * 255.0) as u8));
        }
        let color = if enabled { theme::mix(theme::TEXT_2, theme::TEXT, t) } else { theme::TEXT_3.gamma_multiply(0.5) };
        let c = r.center();
        ui.painter().line_segment([egui::pos2(c.x - 4.0, c.y), egui::pos2(c.x + 4.0, c.y)], Stroke::new(1.4, color));
        if icon_plus {
            ui.painter().line_segment([egui::pos2(c.x, c.y - 4.0), egui::pos2(c.x, c.y + 4.0)], Stroke::new(1.4, color));
        }
        // Holding the button keeps stepping.
        enabled && (response.clicked() || (response.is_pointer_button_down_on() && ui.input(|i| i.pointer.press_start_time().is_some_and(|t0| i.time - t0 > 0.4)) && ui.input(|i| (i.time * 12.0) as i64 != ((i.time - i.stable_dt as f64) * 12.0) as i64)))
    };
    let minus = Rect::from_min_size(rect.min, Vec2::new(30.0, rect.height()));
    let plus = Rect::from_min_size(egui::pos2(rect.right() - 30.0, rect.top()), Vec2::new(30.0, rect.height()));
    if part(ui, minus, false, *value > min) {
        *value -= 1;
    }
    if part(ui, plus, true, *value < max) {
        *value += 1;
    }
    if ui.input(|i| i.pointer.any_down()) {
        ui.ctx().request_repaint();
    }
    ui.painter().vline(minus.right(), rect.y_range().shrink(6.0), theme::hairline());
    ui.painter().vline(plus.left(), rect.y_range().shrink(6.0), theme::hairline());
    theme::text_in(ui.painter(), Rect::from_min_max(minus.right_top(), plus.left_bottom()), Anchor::Center, &value.to_string(), Type::NUM_MEDIUM, theme::TEXT);
    *value != before
}

/// A row of mutually exclusive choices sharing one container.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, id: &str, options: &[(T, &str)], selected: &mut T) -> bool {
    let ty = Type::SMALL_MEDIUM;
    let widths: Vec<f32> = options.iter().map(|(_, label)| (theme::measure(ui.painter(), label, ty) + 22.0).ceil()).collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, theme::CONTROL_H), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(theme::R_CONTROL), theme::FIELD, Stroke::new(1.0, theme::white(22)), StrokeKind::Inside);

    let mut changed = false;
    let mut x = rect.left() + 2.0;
    for ((value, label), w) in options.iter().zip(&widths) {
        let cell = Rect::from_min_size(egui::pos2(x, rect.top() + 2.0), Vec2::new(*w, rect.height() - 4.0));
        let response = ui.interact(cell, ui.scope_id().with((id, *label)), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let active = *selected == *value;
        let t = ui.ctx().animate_bool_with_time(response.id.with("a"), active, 0.12);
        let h = ui.ctx().animate_bool_with_time(response.id.with("h"), response.hovered() && !active, FADE);
        if t > 0.0 {
            key_surface(ui, cell, 4, theme::tint(theme::HOVER, (t * 255.0) as u8), theme::white((t * 30.0) as u8), theme::white((t * 18.0) as u8));
        }
        focus_ring(ui, cell, 4, &response);
        theme::text_in(ui.painter(), cell, Anchor::Center, label, ty, theme::mix(theme::mix(theme::TEXT_2, theme::TEXT, h), theme::TEXT, t));
        if response.clicked() && !active {
            *selected = *value;
            changed = true;
        }
        x += w;
    }
    changed
}

/// A drop-down that shows the current choice and opens a menu of the rest.
pub fn dropdown<T: PartialEq + Copy>(ui: &mut Ui, id: &str, options: &[(T, &str)], selected: &mut T, width: f32) -> bool {
    let current = options.iter().find(|(v, _)| v == selected).map(|(_, l)| *l).unwrap_or("");
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, theme::CONTROL_H), Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    let t = hover_t(ui, &response);
    key_surface(ui, rect, theme::R_CONTROL, theme::mix(theme::RAISED, theme::HOVER, t), theme::white(26), theme::white(16));
    focus_ring(ui, rect, theme::R_CONTROL, &response);
    theme::text_in(ui.painter(), rect.shrink2(Vec2::new(10.0, 0.0)), Anchor::Left, current, Type::BODY, theme::TEXT);
    icons::paint(ui.painter(), Rect::from_min_size(egui::pos2(rect.right() - 26.0, rect.top()), Vec2::new(22.0, rect.height())), Icon::ChevronDown, 12.0, theme::TEXT_2);

    let mut changed = false;
    Popup::menu(&response).id(ui.scope_id().with(("dropdown", id))).frame(menu_frame()).width(width).gap(4.0).show(|ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        for (value, label) in options {
            if menu_item(ui, None, label, None, *value == *selected, false).clicked() {
                changed = *value != *selected;
                *selected = *value;
                ui.close();
            }
        }
    });
    changed
}

/// The frame of menus and other floating surfaces.
pub fn menu_frame() -> Frame {
    Frame::new()
        .fill(theme::RAISED)
        .stroke(Stroke::new(1.0, theme::white(30)))
        .corner_radius(CornerRadius::same(theme::R_PANEL))
        .inner_margin(egui::Margin::same(5))
        .shadow(egui::Shadow { offset: [0, 10], blur: 28, spread: 0, color: Color32::from_black_alpha(140) })
}

/// One row of a menu.
pub fn menu_item(ui: &mut Ui, icon: Option<Icon>, label: &str, shortcut: Option<&str>, checked: bool, danger: bool) -> Response {
    let width = ui.available_width().max(170.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 28.0), Sense::click());
    let enabled = ui.is_enabled();
    let response = response.on_hover_cursor(if enabled { CursorIcon::PointingHand } else { CursorIcon::Default });
    if ui.is_rect_visible(rect) {
        let hovered = response.hovered() && enabled;
        let base = if danger { theme::BAD } else { theme::TEXT };
        if hovered {
            ui.painter().rect_filled(rect, CornerRadius::same(5), if danger { theme::tint(theme::BAD, 40) } else { theme::white(18) });
        }
        let fg = if !enabled { theme::TEXT_3.gamma_multiply(0.6) } else { base };
        let mut x = rect.left() + 9.0;
        if let Some(icon) = icon {
            let dim = if !enabled { fg } else if danger { theme::BAD } else if hovered { theme::TEXT } else { theme::TEXT_2 };
            icons::paint(ui.painter(), Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(15.0, rect.height())), icon, 14.0, dim);
            x += 24.0;
        }
        theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(rect.right() - 8.0, rect.bottom())), Anchor::Left, label, Type::BODY, fg);
        if checked {
            icons::paint(ui.painter(), Rect::from_min_size(egui::pos2(rect.right() - 26.0, rect.top()), Vec2::new(18.0, rect.height())), Icon::Check, 13.0, theme::ACCENT_HI);
        } else if let Some(keys) = shortcut {
            theme::text_in(ui.painter(), rect.shrink2(Vec2::new(9.0, 0.0)), Anchor::Right, keys, Type::SMALL, theme::TEXT_3);
        }
    }
    response
}

/// A hairline between groups of menu items.
pub fn menu_separator(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::hover());
    ui.painter().hline(rect.x_range().shrink(4.0), rect.center().y, theme::hairline());
}

/// A small-caps section label followed by a rule that runs to the edge.
pub fn section(ui: &mut Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
    let w = theme::text_in(ui.painter(), rect, Anchor::Left, text, Type::LABEL, theme::TEXT_3);
    ui.painter().hline((rect.left() + w + 10.0)..=rect.right(), rect.center().y + 0.5, Stroke::new(1.0, theme::white(14)));
}

/// A label on the left and a control on the right, with an optional
/// explanation under the label.
pub fn setting_row(ui: &mut Ui, title: &str, detail: &str, control_width: f32, control: impl FnOnce(&mut Ui)) {
    setting_row_with(ui, title, detail, control_width, true, control);
}

/// [`setting_row`] with the dividing rule under it optional, for a row that
/// is the last thing before another rule.
pub fn setting_row_with(ui: &mut Ui, title: &str, detail: &str, control_width: f32, divider: bool, control: impl FnOnce(&mut Ui)) {
    let text_w = (ui.available_width() - control_width - 20.0).max(120.0);
    let detail_galley = (!detail.is_empty()).then(|| theme::layout_wrapped(ui.painter(), detail, Type::SMALL, theme::TEXT_2, text_w, 3));
    let height = (20.0 + detail_galley.as_ref().map(|g| g.size().y + 3.0).unwrap_or(0.0)).max(theme::CONTROL_H) + 18.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());

    let top = rect.top() + 9.0;
    let title_rect = Rect::from_min_size(egui::pos2(rect.left(), top), Vec2::new(text_w, 20.0));
    theme::text_in(ui.painter(), title_rect, Anchor::Left, title, Type::BODY_MEDIUM, theme::TEXT);
    if let Some(galley) = detail_galley {
        ui.painter().galley(egui::pos2(rect.left(), top + 22.0), galley, theme::TEXT_2);
    }
    let slot = Rect::from_min_max(egui::pos2(rect.right() - control_width, rect.top() + 9.0), egui::pos2(rect.right(), rect.top() + 9.0 + theme::CONTROL_H));
    theme::place(ui, slot, egui::Layout::right_to_left(egui::Align::Center), control);
    if divider {
        ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, theme::white(10)));
    }
}

/// A boxed note: an icon, a line of emphasis and supporting text.
pub fn callout(ui: &mut Ui, color: Color32, icon: Icon, title: &str, body: &str) {
    let width = ui.available_width();
    let text_w = width - 44.0;
    let title_g = theme::layout_wrapped(ui.painter(), title, Type::BODY_MEDIUM, theme::TEXT, text_w, 2);
    let body_g = (!body.is_empty()).then(|| theme::layout_wrapped(ui.painter(), body, Type::SMALL, theme::TEXT_2, text_w, 6));
    let height = 12.0 + title_g.size().y + body_g.as_ref().map(|g| g.size().y + 4.0).unwrap_or(0.0) + 12.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height.ceil()), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(theme::R_PANEL), theme::tint(color, 22), Stroke::new(1.0, theme::tint(color, 70)), StrokeKind::Inside);
    icons::paint(ui.painter(), Rect::from_min_size(egui::pos2(rect.left() + 10.0, rect.top() + 10.0), Vec2::splat(20.0)), icon, 15.0, color);
    let x = rect.left() + 36.0;
    let title_h = title_g.size().y;
    ui.painter().galley(egui::pos2(x, rect.top() + 12.0), title_g, theme::TEXT);
    if let Some(g) = body_g {
        ui.painter().galley(egui::pos2(x, rect.top() + 12.0 + title_h + 4.0), g, theme::TEXT_2);
    }
}

/// A multi-line text field in the style of [`Field`].
pub fn text_area(ui: &mut Ui, text: &mut String, hint: &str, height: f32, id: Id) -> Response {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let focused = ui.memory(|m| m.has_focus(id));
    let hovered = ui.rect_contains_pointer(rect);
    let t = ui.ctx().animate_bool_with_time(id.with("focus"), focused, 0.1);
    let border = theme::mix(if hovered { theme::white(44) } else { theme::white(26) }, theme::ACCENT, t);
    ui.painter().rect(rect, CornerRadius::same(theme::R_CONTROL), theme::FIELD, Stroke::new(1.0, border), StrokeKind::Inside);
    if t > 0.0 {
        ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(theme::R_CONTROL + 2), Stroke::new(2.0, theme::tint(theme::ACCENT, (t * 70.0) as u8)), StrokeKind::Outside);
    }
    let inner = rect.shrink2(Vec2::new(10.0, 7.0));
    theme::region(ui, inner, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(id.with("scroll"))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(text)
                        .id(id)
                        .frame(Frame::new())
                        .margin(egui::Margin::ZERO)
                        .font(egui::FontId::monospace(12.0))
                        .text_color(theme::TEXT)
                        .desired_width(inner.width())
                        .desired_rows(((inner.height() / 16.0).floor() as usize).max(2))
                        .hint_text(egui::RichText::new(hint).color(theme::TEXT_3)),
                )
            })
            .inner
    })
}

/// What a selection box shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checked {
    No,
    Yes,
    /// Some of what it stands for: a header box over a partly selected list.
    Some,
}

/// A bare selection box, for list rows and their header.
pub fn check_box(ui: &mut Ui, state: Checked) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    if ui.is_rect_visible(rect) {
        let boxr = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
        let t = ui.ctx().animate_bool_with_time(response.id.with("on"), state != Checked::No, 0.1);
        let hover = hover_t(ui, &response);
        let painter = ui.painter();
        painter.rect(
            boxr,
            CornerRadius::same(4),
            theme::mix(theme::FIELD, theme::ACCENT, t),
            Stroke::new(1.0, theme::mix(theme::mix(theme::white(48), theme::white(86), hover), theme::ACCENT_HI, t)),
            StrokeKind::Inside,
        );
        match state {
            Checked::Yes if t > 0.3 => icons::paint(painter, boxr, Icon::Check, 12.0, Color32::WHITE.gamma_multiply(t)),
            Checked::Some => {
                painter.hline((boxr.left() + 4.0)..=(boxr.right() - 4.0), boxr.center().y, Stroke::new(1.6, Color32::WHITE.gamma_multiply(t)));
            }
            _ => {}
        }
        focus_ring(ui, boxr, 4, &response);
    }
    response
}

/// A filter chip: a label and how many things it stands for.
pub fn chip(ui: &mut Ui, label: &str, count: usize, active: bool) -> Response {
    let ty = Type::SMALL_MEDIUM;
    let number = count.to_string();
    let label_w = theme::measure(ui.painter(), label, ty);
    let number_w = theme::measure(ui.painter(), &number, Type::mono(11.0, Weight::Medium));
    let (rect, response) = ui.allocate_exact_size(Vec2::new((label_w + number_w + 34.0).ceil(), 26.0), Sense::click());
    let response = response.on_hover_cursor(CursorIcon::PointingHand);
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_with_time(response.id.with("a"), active, 0.12);
        let h = hover_t(ui, &response);
        if t > 0.0 {
            key_surface(ui, rect, theme::R_CONTROL, theme::tint(theme::HOVER, (t * 255.0) as u8), theme::white((t * 34.0) as u8), theme::white((t * 18.0) as u8));
        }
        if t < 1.0 {
            ui.painter().rect_stroke(rect, CornerRadius::same(theme::R_CONTROL), Stroke::new(1.0, theme::white(((1.0 - t) * (18.0 + h * 16.0)) as u8)), StrokeKind::Inside);
        }
        focus_ring(ui, rect, theme::R_CONTROL, &response);
        let fg = theme::mix(theme::mix(theme::TEXT_2, theme::TEXT, h), theme::TEXT, t);
        theme::text_in(ui.painter(), Rect::from_min_max(egui::pos2(rect.left() + 10.0, rect.top()), rect.max), Anchor::Left, label, ty, fg);
        // The count sits in its own small tablet, one step lighter.
        let tablet = Rect::from_min_size(egui::pos2(rect.left() + 10.0 + label_w + 7.0, rect.center().y - 8.0), Vec2::new(number_w + 10.0, 16.0));
        ui.painter().rect_filled(tablet, CornerRadius::same(4), theme::mix(theme::white(16), theme::tint(theme::ACCENT, 60), t));
        theme::text_in(ui.painter(), tablet, Anchor::Center, &number, Type::mono(11.0, Weight::Medium), theme::mix(theme::TEXT_2, theme::ACCENT_HI, t));
    }
    response
}

/// One key of a strip of on/off keys, such as the days of the week.
pub fn toggle_key(ui: &mut Ui, label: &str, on: &mut bool, width: f32) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::click());
    if response.clicked() && ui.is_enabled() {
        *on = !*on;
        response.mark_changed();
    }
    let response = response.on_hover_cursor(if ui.is_enabled() { CursorIcon::PointingHand } else { CursorIcon::Default });
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_with_time(response.id.with("on"), *on, 0.12);
        let h = hover_t(ui, &response);
        let fade = if ui.is_enabled() { 1.0 } else { 0.4 };
        if t > 0.0 {
            key_surface(ui, rect, theme::R_CONTROL, theme::tint(theme::HOVER, (t * 255.0 * fade) as u8), theme::white((t * 34.0 * fade) as u8), theme::white((t * 18.0 * fade) as u8));
        }
        if t < 1.0 {
            ui.painter().rect_stroke(rect, CornerRadius::same(theme::R_CONTROL), Stroke::new(1.0, theme::white(((1.0 - t) * (16.0 + h * 18.0) * fade) as u8)), StrokeKind::Inside);
        }
        focus_ring(ui, rect, theme::R_CONTROL, &response);
        let fg = theme::mix(theme::mix(theme::TEXT_3, theme::TEXT_2, h), theme::TEXT, t).gamma_multiply(fade);
        theme::text_in(ui.painter(), rect, Anchor::Center, label, Type::SMALL_MEDIUM, fg);
        // A lit mark under the label says "on" without relying on contrast alone.
        if t > 0.0 {
            let mark = Rect::from_center_size(egui::pos2(rect.center().x, rect.bottom() - 5.0), Vec2::new(10.0, 2.0));
            ui.painter().rect_filled(mark, CornerRadius::same(1), theme::ACCENT.gamma_multiply(t * fade));
        }
    }
    response
}
