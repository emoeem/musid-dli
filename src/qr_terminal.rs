//! Terminal QR rendering helper shared by the smoke examples.
//!
//! Included via `#[path = "../src/qr_terminal.rs"]` from each example (NOT
//! compiled into the library build); the `qrcode` crate is a dev-dependency.
//! Renders text as ANSI half-block characters so the QR is scannable directly
//! from the terminal — no browser detour.

use qrcode::{Color, QrCode};

/// QR matrix with `border` light modules of quiet zone on every side.
/// `true` = dark module. Row-major; used by both renderers.
fn bordered_grid(text: &str, border: usize) -> Vec<Vec<bool>> {
    let code = QrCode::new(text.as_bytes()).unwrap_or_else(|e| panic!("QR encode failed: {e}"));
    let width = code.width();
    let colors = code.to_colors();
    let bordered = width + border * 2;
    let mut grid: Vec<Vec<bool>> = Vec::new();
    for _ in 0..border {
        grid.push(vec![false; bordered]);
    }
    for row in colors.chunks(width) {
        let mut padded = vec![false; border];
        padded.extend(row.iter().map(|color| *color == Color::Dark));
        padded.extend(vec![false; border]);
        grid.push(padded);
    }
    for _ in 0..border {
        grid.push(vec![false; bordered]);
    }
    grid
}

/// Render text as an ANSI half-block QR with a 2-module white border.
/// The returned string starts with a newline; print it as-is.
///
/// Colors are pinned to black-on-bright-white (SGR 30/107). With default
/// terminal colors the same glyphs render INVERTED on dark themes and phone
/// scanners — including the Bilibili in-app scanner — fail to decode them,
/// leaving the poll loop on Waiting until the 180s budget runs out.
// Shared scaffold: examples call it, the regression test only needs render_png.
#[allow(dead_code)]
pub fn render(text: &str) -> String {
    let grid = bordered_grid(text, 2);
    let bordered = grid[0].len();
    // Two grid rows per terminal line via half-block characters:
    // both dark -> █, top only -> ▀, bottom only -> ▄, neither -> space.
    let mut out = String::from("\n\u{1b}[30;107m");
    for pair in grid.chunks(2) {
        let top = pair.first().cloned().unwrap_or_default();
        let bottom = pair.get(1).cloned().unwrap_or_default();
        let mut line = String::new();
        for x in 0..bordered {
            let t = top.get(x).copied().unwrap_or(false);
            let b = bottom.get(x).copied().unwrap_or(false);
            line.push(match (t, b) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("\u{1b}[0m");
    out
}

/// Save the QR as a large dark-on-white PNG — fallback for terminals where
/// the ANSI rendering is hard to scan (small font, odd line spacing, ...).
pub fn render_png(path: &str, text: &str) -> Result<(), String> {
    use image::{GrayImage, Luma};
    const SCALE: u32 = 10;
    let grid = bordered_grid(text, 4);
    let height = grid.len() as u32;
    let width = grid[0].len() as u32;
    let mut img = GrayImage::from_pixel(width * SCALE, height * SCALE, Luma([255]));
    for (y, row) in grid.iter().enumerate() {
        for (x, dark) in row.iter().enumerate() {
            if !dark {
                continue;
            }
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    img.put_pixel(x as u32 * SCALE + dx, y as u32 * SCALE + dy, Luma([0]));
                }
            }
        }
    }
    img.save(path).map_err(|e| e.to_string())
}
