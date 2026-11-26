//! The icon set: line glyphs drawn on a 16-unit grid.
//!
//! Drawn with the painter rather than shipped as a font or images, so they
//! stay crisp at any scale factor and take the colour of whatever they sit in.

use std::f32::consts::{PI, TAU};

use egui::epaint::{PathShape, PathStroke};
use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke};

use siphon_core::Category;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Plus,
    Search,
    Play,
    Pause,
    Close,
    Check,
    ChevronDown,
    Download,
    More,
    Copy,
    Link,
    Refresh,
    Trash,
    Folder,
    External,
    Sliders,
    Alert,
    Clock,
    Bolt,
    Restart,
    // Library
    Stack,
    CheckCircle,
    // File families
    Video,
    Audio,
    Image,
    Document,
    Archive,
    Program,
    File,
}

impl Icon {
    pub fn for_category(category: Category) -> Icon {
        match category {
            Category::Video => Icon::Video,
            Category::Audio => Icon::Audio,
            Category::Image => Icon::Image,
            Category::Document => Icon::Document,
            Category::Archive => Icon::Archive,
            Category::Program => Icon::Program,
            Category::Other => Icon::File,
        }
    }
}

/// Maps the 16-unit design grid onto a square inside the target rectangle.
struct Pen<'a> {
    painter: &'a Painter,
    origin: Pos2,
    unit: f32,
    stroke: Stroke,
}

impl Pen<'_> {
    fn at(&self, x: f32, y: f32) -> Pos2 {
        Pos2::new(self.origin.x + x * self.unit, self.origin.y + y * self.unit)
    }

    fn line(&self, points: &[(f32, f32)]) {
        let points: Vec<Pos2> = points.iter().map(|&(x, y)| self.at(x, y)).collect();
        self.painter.add(Shape::line(points, PathStroke::new(self.stroke.width, self.stroke.color)));
    }

    fn closed(&self, points: &[(f32, f32)]) {
        let points: Vec<Pos2> = points.iter().map(|&(x, y)| self.at(x, y)).collect();
        self.painter.add(Shape::closed_line(points, PathStroke::new(self.stroke.width, self.stroke.color)));
    }

    fn filled(&self, points: &[(f32, f32)]) {
        let points: Vec<Pos2> = points.iter().map(|&(x, y)| self.at(x, y)).collect();
        self.painter.add(Shape::Path(PathShape::convex_polygon(points, self.stroke.color, PathStroke::NONE)));
    }

    fn circle(&self, cx: f32, cy: f32, r: f32) {
        self.painter.circle_stroke(self.at(cx, cy), r * self.unit, self.stroke);
    }

    fn dot(&self, cx: f32, cy: f32, r: f32) {
        self.painter.circle_filled(self.at(cx, cy), r * self.unit, self.stroke.color);
    }

    /// An arc from `from` to `to` (radians, clockwise from the positive x axis).
    fn arc(&self, cx: f32, cy: f32, r: f32, from: f32, to: f32) {
        let steps = ((to - from).abs() / TAU * 28.0).ceil().max(4.0) as usize;
        let points: Vec<(f32, f32)> = (0..=steps)
            .map(|i| {
                let a = from + (to - from) * i as f32 / steps as f32;
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        self.line(&points);
    }

    /// A rectangle with rounded corners.
    fn round_rect(&self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) {
        let mut points = Vec::with_capacity(24);
        for (cx, cy, start) in [(x1 - r, y0 + r, -PI / 2.0), (x1 - r, y1 - r, 0.0), (x0 + r, y1 - r, PI / 2.0), (x0 + r, y0 + r, PI)] {
            for i in 0..=4 {
                let a = start + (PI / 2.0) * i as f32 / 4.0;
                points.push((cx + r * a.cos(), cy + r * a.sin()));
            }
        }
        self.closed(&points);
    }
}

/// Paints `icon` centred in `rect`, `size` points across.
pub fn paint(painter: &Painter, rect: Rect, icon: Icon, size: f32, color: Color32) {
    let unit = size / 16.0;
    let origin = Pos2::new(rect.center().x - size / 2.0, rect.center().y - size / 2.0);
    // Line weight follows size, staying near 1.4 px at the usual 16.
    let pen = Pen { painter, origin, unit, stroke: Stroke::new((size / 16.0 * 1.4).clamp(1.2, 2.0), color) };
    let p = &pen;

    match icon {
        Icon::Plus => {
            p.line(&[(8.0, 3.0), (8.0, 13.0)]);
            p.line(&[(3.0, 8.0), (13.0, 8.0)]);
        }
        Icon::Search => {
            p.circle(7.0, 7.0, 4.2);
            p.line(&[(10.2, 10.2), (13.4, 13.4)]);
        }
        Icon::Play => p.filled(&[(5.0, 3.2), (13.0, 8.0), (5.0, 12.8)]),
        Icon::Pause => {
            p.filled(&[(4.2, 3.4), (6.8, 3.4), (6.8, 12.6), (4.2, 12.6)]);
            p.filled(&[(9.2, 3.4), (11.8, 3.4), (11.8, 12.6), (9.2, 12.6)]);
        }
        Icon::Close => {
            p.line(&[(4.0, 4.0), (12.0, 12.0)]);
            p.line(&[(12.0, 4.0), (4.0, 12.0)]);
        }
        Icon::Check => p.line(&[(3.5, 8.4), (6.6, 11.4), (12.6, 4.8)]),
        Icon::ChevronDown => p.line(&[(3.8, 6.0), (8.0, 10.2), (12.2, 6.0)]),
        Icon::Download => {
            p.line(&[(8.0, 2.6), (8.0, 10.2)]);
            p.line(&[(4.6, 7.0), (8.0, 10.4), (11.4, 7.0)]);
            p.line(&[(3.0, 13.2), (13.0, 13.2)]);
        }
        Icon::More => {
            p.dot(3.6, 8.0, 1.15);
            p.dot(8.0, 8.0, 1.15);
            p.dot(12.4, 8.0, 1.15);
        }
        Icon::Copy => {
            p.round_rect(5.6, 5.6, 13.2, 13.2, 1.6);
            p.line(&[(10.4, 3.0), (4.4, 3.0), (3.0, 4.4), (3.0, 10.4)]);
        }
        Icon::Link => {
            p.line(&[(6.6, 9.4), (9.4, 6.6)]);
            p.arc(4.9, 11.1, 2.6, PI * 0.25, PI * 1.25);
            p.line(&[(3.06, 9.26), (5.6, 6.7)]);
            p.line(&[(6.74, 12.94), (9.3, 10.4)]);
            p.arc(11.1, 4.9, 2.6, -PI * 0.75, PI * 0.25);
            p.line(&[(9.26, 3.06), (6.7, 5.6)]);
            p.line(&[(12.94, 6.74), (10.4, 9.3)]);
        }
        Icon::Refresh => {
            p.arc(8.0, 8.0, 5.0, -PI * 0.15, PI * 1.25);
            p.line(&[(9.6, 2.4), (12.9, 5.3), (9.0, 7.0)]);
        }
        Icon::Restart => {
            p.arc(8.0, 8.4, 4.8, -PI * 0.4, PI * 1.4);
            p.line(&[(4.0, 2.6), (4.0, 5.9), (7.3, 5.9)]);
        }
        Icon::Trash => {
            p.line(&[(3.0, 4.6), (13.0, 4.6)]);
            p.line(&[(6.2, 4.6), (6.6, 2.8), (9.4, 2.8), (9.8, 4.6)]);
            p.line(&[(4.2, 4.6), (4.9, 13.2), (11.1, 13.2), (11.8, 4.6)]);
            p.line(&[(6.8, 7.0), (6.9, 10.8)]);
            p.line(&[(9.2, 7.0), (9.1, 10.8)]);
        }
        Icon::Folder => {
            p.closed(&[(2.6, 4.2), (6.4, 4.2), (7.8, 5.8), (13.4, 5.8), (13.4, 12.4), (2.6, 12.4)]);
        }
        Icon::External => {
            p.line(&[(7.0, 3.6), (3.6, 3.6), (3.6, 12.4), (12.4, 12.4), (12.4, 9.0)]);
            p.line(&[(9.4, 3.2), (12.8, 3.2), (12.8, 6.6)]);
            p.line(&[(12.6, 3.4), (7.6, 8.4)]);
        }
        Icon::Sliders => {
            p.line(&[(2.8, 4.6), (13.2, 4.6)]);
            p.line(&[(2.8, 11.4), (13.2, 11.4)]);
            p.painter.circle_filled(p.at(10.2, 4.6), 2.0 * unit, crate::theme::PANEL);
            p.circle(10.2, 4.6, 1.9);
            p.painter.circle_filled(p.at(5.8, 11.4), 2.0 * unit, crate::theme::PANEL);
            p.circle(5.8, 11.4, 1.9);
        }
        Icon::Alert => {
            p.closed(&[(8.0, 2.6), (14.0, 13.0), (2.0, 13.0)]);
            p.line(&[(8.0, 6.6), (8.0, 9.6)]);
            p.dot(8.0, 11.3, 0.8);
        }
        Icon::Clock => {
            p.circle(8.0, 8.0, 5.4);
            p.line(&[(8.0, 4.8), (8.0, 8.0), (10.4, 9.6)]);
        }
        Icon::Bolt => p.closed(&[(9.0, 2.2), (3.6, 9.0), (7.4, 9.0), (6.8, 13.8), (12.4, 6.8), (8.6, 6.8)]),
        Icon::Stack => {
            p.line(&[(3.0, 4.4), (13.0, 4.4)]);
            p.line(&[(3.0, 8.0), (13.0, 8.0)]);
            p.line(&[(3.0, 11.6), (13.0, 11.6)]);
        }
        Icon::CheckCircle => {
            p.circle(8.0, 8.0, 5.4);
            p.line(&[(5.4, 8.2), (7.3, 10.1), (10.7, 6.2)]);
        }
        Icon::Video => {
            p.round_rect(2.4, 3.6, 13.6, 12.4, 1.8);
            p.filled(&[(6.7, 5.9), (10.5, 8.0), (6.7, 10.1)]);
        }
        Icon::Audio => {
            p.line(&[(6.4, 11.2), (6.4, 3.8), (12.2, 2.8), (12.2, 10.0)]);
            p.circle(4.8, 11.4, 1.7);
            p.circle(10.6, 10.2, 1.7);
        }
        Icon::Image => {
            p.round_rect(2.4, 3.2, 13.6, 12.8, 1.8);
            p.dot(5.6, 6.4, 1.1);
            p.line(&[(2.8, 11.4), (6.4, 8.2), (8.8, 10.4), (10.8, 8.8), (13.2, 11.0)]);
        }
        Icon::Document => {
            p.closed(&[(4.0, 2.4), (9.6, 2.4), (12.4, 5.2), (12.4, 13.6), (4.0, 13.6)]);
            p.line(&[(6.2, 7.6), (10.2, 7.6)]);
            p.line(&[(6.2, 10.4), (10.2, 10.4)]);
        }
        Icon::Archive => {
            p.round_rect(2.6, 2.8, 13.4, 5.8, 0.9);
            p.line(&[(3.6, 5.8), (3.6, 13.2), (12.4, 13.2), (12.4, 5.8)]);
            p.line(&[(6.6, 8.6), (9.4, 8.6)]);
        }
        Icon::Program => {
            p.round_rect(2.4, 3.2, 13.6, 12.8, 1.8);
            p.line(&[(2.4, 6.2), (13.6, 6.2)]);
            p.line(&[(5.0, 8.6), (6.8, 10.0), (5.0, 11.4)]);
            p.line(&[(8.4, 11.2), (11.0, 11.2)]);
        }
        Icon::File => {
            p.closed(&[(4.0, 2.4), (9.6, 2.4), (12.4, 5.2), (12.4, 13.6), (4.0, 13.6)]);
            p.line(&[(9.4, 2.6), (9.4, 5.4), (12.2, 5.4)]);
        }
    }
}
