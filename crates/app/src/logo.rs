//! The application mark, drawn in code.
//!
//! A rounded tile holding a byte map in mid-download: the same grid the
//! inspector shows, with the cells of one row lit and a brighter write head.

use std::path::Path;

/// Renders the mark as straight (non-premultiplied) RGBA at `size` pixels.
pub fn render(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    // Coverage is estimated with 4x4 supersampling so edges stay smooth at 16 px.
    const SUB: u32 = 4;

    let inset = s * 0.085;
    let tile = (inset, inset, s - inset, s - inset);
    let radius = (s - inset * 2.0) * 0.225;

    let (cols, rows) = (5usize, 5usize);
    let pad = s * 0.235;
    let gap = s * 0.034;
    let cell = (s - pad * 2.0 - gap * (cols as f32 - 1.0)) / cols as f32;
    let cell_radius = cell * 0.22;
    // Which cells are filled: three finished rows of varying length.
    let lit = |row: usize, col: usize| matches!((row, col), (0, _) | (1, 0..=3) | (2, 0..=1) | (3, 0));
    let head = |row: usize, col: usize| matches!((row, col), (1, 4) | (2, 2) | (3, 1));

    for y in 0..size {
        for x in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let px = x as f32 + (sx as f32 + 0.5) / SUB as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SUB as f32;
                    if !in_round_rect(px, py, tile, radius) {
                        continue;
                    }
                    // Tile: near-black with a slight lift toward the top.
                    let t = (py - tile.1) / (tile.3 - tile.1);
                    let mut color = [lerp(30.0, 13.0, t), lerp(32.0, 14.0, t), lerp(38.0, 17.0, t), 255.0];
                    // A hairline ring just inside the edge.
                    if !in_round_rect(px, py, (tile.0 + s * 0.012, tile.1 + s * 0.012, tile.2 - s * 0.012, tile.3 - s * 0.012), radius - s * 0.012) {
                        color = [62.0, 65.0, 74.0, 255.0];
                    }
                    for row in 0..rows {
                        for col in 0..cols {
                            let x0 = pad + col as f32 * (cell + gap);
                            let y0 = pad + row as f32 * (cell + gap);
                            if in_round_rect(px, py, (x0, y0, x0 + cell, y0 + cell), cell_radius) {
                                color = if head(row, col) {
                                    [176.0, 208.0, 255.0, 255.0]
                                } else if lit(row, col) {
                                    [76.0, 141.0, 255.0, 255.0]
                                } else {
                                    [44.0, 47.0, 55.0, 255.0]
                                };
                            }
                        }
                    }
                    for (a, c) in acc.iter_mut().zip(color) {
                        *a += c;
                    }
                }
            }
            let samples = (SUB * SUB) as f32;
            let alpha = acc[3] / samples;
            let i = ((y * size + x) * 4) as usize;
            if alpha > 0.0 {
                // Un-premultiply the averaged colour.
                let covered = acc[3] / 255.0;
                pixels[i] = (acc[0] / covered).round().clamp(0.0, 255.0) as u8;
                pixels[i + 1] = (acc[1] / covered).round().clamp(0.0, 255.0) as u8;
                pixels[i + 2] = (acc[2] / covered).round().clamp(0.0, 255.0) as u8;
                pixels[i + 3] = alpha.round() as u8;
            }
        }
    }
    pixels
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

fn in_round_rect(x: f32, y: f32, (x0, y0, x1, y1): (f32, f32, f32, f32), r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

/// Paints the application's mark into `rect`, from a texture made once.
pub fn paint(ui: &egui::Ui, rect: egui::Rect) {
    let id = egui::Id::unique("siphon-mark-texture");
    let kept = ui.ctx().data_mut(|data| data.get_temp::<egui::TextureHandle>(id));
    let texture = kept.unwrap_or_else(|| {
        let side = 128;
        let image = egui::ColorImage::from_rgba_unmultiplied([side, side], &render(side as u32));
        let texture = ui.ctx().load_texture("siphon-mark", image, egui::TextureOptions::LINEAR);
        ui.ctx().data_mut(|data| data.insert_temp(id, texture.clone()));
        texture
    });
    ui.painter().image(texture.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
}

pub fn icon_data(size: u32) -> egui::IconData {
    egui::IconData { rgba: render(size), width: size, height: size }
}

/// Writes the PNG set `iconutil` turns into an `.icns` file.
pub fn write_iconset(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for (name, size) in [
        ("icon_16x16.png", 16),
        ("icon_16x16@2x.png", 32),
        ("icon_32x32.png", 32),
        ("icon_32x32@2x.png", 64),
        ("icon_128x128.png", 128),
        ("icon_128x128@2x.png", 256),
        ("icon_256x256.png", 256),
        ("icon_256x256@2x.png", 512),
        ("icon_512x512.png", 512),
        ("icon_512x512@2x.png", 1024),
    ] {
        let image = image::RgbaImage::from_raw(size, size, render(size)).ok_or("bad icon buffer")?;
        image.save(dir.join(name)).map_err(|e| e.to_string())?;
    }
    Ok(())
}
