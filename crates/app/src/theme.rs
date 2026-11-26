//! Design tokens: colour, type, geometry.
//!
//! One neutral near-black scale, one accent, three semantic hues. Structure
//! comes from 1 px hairlines and surface steps, not from shadows.

use std::sync::Arc;

use egui::epaint::text::{FontData, FontDefinitions, TextWrapping, VariationCoords};
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontFamily, FontId, Galley, Painter, Pos2, Rect, Stroke, Ui};

pub const BG: Color32 = Color32::from_rgb(0x0A, 0x0B, 0x0D);
pub const PANEL: Color32 = Color32::from_rgb(0x10, 0x11, 0x13);
pub const RAISED: Color32 = Color32::from_rgb(0x17, 0x18, 0x1B);
pub const HOVER: Color32 = Color32::from_rgb(0x1D, 0x1E, 0x22);
pub const PRESSED: Color32 = Color32::from_rgb(0x13, 0x14, 0x17);
pub const FIELD: Color32 = Color32::from_rgb(0x0C, 0x0D, 0x0F);

pub const TEXT: Color32 = Color32::from_rgb(0xEC, 0xED, 0xEE);
pub const TEXT_2: Color32 = Color32::from_rgb(0x9A, 0x9C, 0xA5);
pub const TEXT_3: Color32 = Color32::from_rgb(0x62, 0x65, 0x6F);

pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x8D, 0xFF);
pub const ACCENT_HI: Color32 = Color32::from_rgb(0x7F, 0xAE, 0xFF);
pub const ACCENT_LO: Color32 = Color32::from_rgb(0x2B, 0x5C, 0xC4);
pub const OK: Color32 = Color32::from_rgb(0x3D, 0xD6, 0x8C);
pub const WARN: Color32 = Color32::from_rgb(0xF0, 0xA9, 0x3B);
pub const BAD: Color32 = Color32::from_rgb(0xF2, 0x55, 0x5A);

pub const R_CONTROL: u8 = 6;
pub const R_PANEL: u8 = 8;
pub const R_DIALOG: u8 = 12;

pub const ROW_H: f32 = 46.0;
pub const CONTROL_H: f32 = 28.0;
pub const SIDEBAR_W: f32 = 212.0;
pub const INSPECTOR_W: f32 = 340.0;
/// Height of the strip that carries the window controls and drags the window.
pub const TITLEBAR_H: f32 = 46.0;

/// White at the given opacity: hairlines and inner highlights.
pub fn white(alpha: u8) -> Color32 {
    Color32::from_white_alpha(alpha)
}

/// The structural hairline.
pub fn hairline() -> Stroke {
    Stroke::new(1.0, white(20))
}

/// A hue at low opacity, for pill and selection fills.
pub fn tint(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// Linear blend between two colours.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()), lerp(a.a(), b.a()))
}

/// Font weights, as positions on the `wght` axis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Weight {
    Regular,
    Medium,
    Semibold,
}

impl Weight {
    fn axis(self) -> f32 {
        match self {
            Weight::Regular => 400.0,
            Weight::Medium => 510.0,
            Weight::Semibold => 590.0,
        }
    }
}

/// How a run of text is set.
#[derive(Clone, Copy, Debug)]
pub struct Type {
    pub size: f32,
    pub weight: Weight,
    pub mono: bool,
    /// Extra letter spacing in points.
    pub tracking: f32,
}

impl Type {
    pub const fn new(size: f32, weight: Weight) -> Self {
        Self { size, weight, mono: false, tracking: 0.0 }
    }

    /// Fixed-width digits, for numbers that change while you look at them.
    pub const fn mono(size: f32, weight: Weight) -> Self {
        Self { size, weight, mono: true, tracking: 0.0 }
    }

    pub const BODY: Type = Type::new(13.0, Weight::Regular);
    pub const BODY_MEDIUM: Type = Type::new(13.0, Weight::Medium);
    pub const SMALL: Type = Type::new(12.0, Weight::Regular);
    pub const SMALL_MEDIUM: Type = Type::new(12.0, Weight::Medium);
    pub const TITLE: Type = Type::new(15.0, Weight::Semibold);
    pub const HEADING: Type = Type::new(18.0, Weight::Semibold);
    pub const NUM: Type = Type::mono(12.0, Weight::Regular);
    pub const NUM_MEDIUM: Type = Type::mono(12.0, Weight::Medium);
    pub const STAT: Type = Type::mono(17.0, Weight::Medium);
    /// Small capitals label. Call sites pass upper-case text.
    pub const LABEL: Type = Type { size: 10.5, weight: Weight::Semibold, mono: false, tracking: 0.7 };

    pub fn font_id(self) -> FontId {
        FontId::new(self.size, if self.mono { FontFamily::Monospace } else { FontFamily::Proportional })
    }

    pub fn format(self, color: Color32) -> TextFormat {
        TextFormat {
            font_id: self.font_id(),
            color,
            extra_letter_spacing: self.tracking,
            coords: VariationCoords::new([(b"wght", self.weight.axis())]),
            ..Default::default()
        }
    }
}

/// Lays out one line of text, cut with an ellipsis at `max_width`.
pub fn layout(painter: &Painter, text: &str, ty: Type, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), ty.format(color));
    job.wrap = TextWrapping {
        max_width: max_width.max(0.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('\u{2026}'),
    };
    painter.layout_job(job)
}

/// Lays out wrapped text, up to `max_rows` lines.
pub fn layout_wrapped(painter: &Painter, text: &str, ty: Type, color: Color32, max_width: f32, max_rows: usize) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), ty.format(color));
    job.wrap = TextWrapping {
        max_width: max_width.max(0.0),
        max_rows,
        break_anywhere: false,
        overflow_character: Some('\u{2026}'),
    };
    painter.layout_job(job)
}

/// Lays out wrapped text with every line centred. Paint it with the
/// horizontal centre as the x position.
pub fn layout_centered(painter: &Painter, text: &str, ty: Type, color: Color32, max_width: f32, max_rows: usize) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), ty.format(color));
    job.wrap = TextWrapping {
        max_width: max_width.max(0.0),
        max_rows,
        break_anywhere: false,
        overflow_character: Some('\u{2026}'),
    };
    job.halign = egui::Align::Center;
    painter.layout_job(job)
}

/// Where text sits inside a rectangle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    Left,
    Center,
    Right,
}

/// Paints one line of text vertically centred in `rect`. Returns its width.
pub fn text_in(painter: &Painter, rect: Rect, anchor: Anchor, text: &str, ty: Type, color: Color32) -> f32 {
    let galley = layout(painter, text, ty, color, rect.width());
    let size = galley.size();
    let x = match anchor {
        Anchor::Left => rect.left(),
        Anchor::Center => rect.center().x - size.x / 2.0,
        Anchor::Right => rect.right() - size.x,
    };
    painter.galley(Pos2::new(x.round(), (rect.center().y - size.y / 2.0).round()), galley, color);
    size.x
}

/// Width of a line of text without painting it.
pub fn measure(painter: &Painter, text: &str, ty: Type) -> f32 {
    layout(painter, text, ty, TEXT, f32::INFINITY).size().x
}

/// The platform's own UI and fixed-width faces. Both must be variable fonts
/// with a weight axis for the type scale to render as designed; anything
/// else still works, at one weight.
fn system_fonts() -> (&'static [&'static str], &'static [&'static str]) {
    #[cfg(target_os = "macos")]
    return (
        &["/System/Library/Fonts/SFNS.ttf", "/System/Library/Fonts/Helvetica.ttc"],
        &["/System/Library/Fonts/SFNSMono.ttf", "/System/Library/Fonts/Menlo.ttc"],
    );
    #[cfg(target_os = "windows")]
    return (
        &["C:\\Windows\\Fonts\\SegUIVar.ttf", "C:\\Windows\\Fonts\\segoeui.ttf"],
        &["C:\\Windows\\Fonts\\CascadiaMono.ttf", "C:\\Windows\\Fonts\\consola.ttf"],
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return (
        &[
            "/usr/share/fonts/truetype/inter/Inter-Variable.ttf",
            "/usr/share/fonts/inter/InterVariable.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ],
        &[
            "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        ],
    );
}

fn first_readable(paths: &[&str]) -> Option<Vec<u8>> {
    paths.iter().find_map(|path| std::fs::read(path).ok())
}

/// Installs fonts and the global egui style.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let (ui_paths, mono_paths) = system_fonts();
    if let Some(bytes) = first_readable(ui_paths) {
        fonts.font_data.insert("system-ui".to_owned(), Arc::new(FontData::from_owned(bytes)));
        fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "system-ui".to_owned());
    }
    if let Some(bytes) = first_readable(mono_paths) {
        fonts.font_data.insert("system-mono".to_owned(), Arc::new(FontData::from_owned(bytes)));
        fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "system-mono".to_owned());
    }
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.extreme_bg_color = FIELD;
        v.override_text_color = Some(TEXT);
        v.selection.bg_fill = tint(ACCENT, 90);
        v.selection.stroke = Stroke::new(1.0, ACCENT_HI);
        v.window_stroke = Stroke::new(1.0, white(22));
        v.widgets.noninteractive.bg_stroke = hairline();
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_2);
        v.widgets.inactive.bg_fill = RAISED;
        v.widgets.inactive.weak_bg_fill = RAISED;
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.hovered.bg_fill = HOVER;
        v.widgets.hovered.weak_bg_fill = HOVER;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, white(34));
        v.widgets.active.bg_fill = PRESSED;
        v.widgets.active.weak_bg_fill = PRESSED;
        v.text_cursor.stroke = Stroke::new(1.5, ACCENT_HI);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.floating = true;
        style.interaction.selectable_labels = false;
        style.text_styles.insert(egui::TextStyle::Body, FontId::proportional(13.0));
        style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(13.0));
        style.text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
        style.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(12.0));
    });
}

/// A child `Ui` confined to `rect`.
pub fn region<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    add(&mut child)
}

/// Like [`region`], with a chosen layout and without clipping.
///
/// Controls positioned by hand inside an already-allocated row go through
/// here. (`Ui::scope_builder` with an explicit rectangle also moves the
/// parent's layout cursor to the end of that rectangle, which shifts
/// everything that follows.)
pub fn place<R>(ui: &mut Ui, rect: Rect, layout: egui::Layout, add: impl FnOnce(&mut Ui) -> R) -> R {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    add(&mut child)
}
