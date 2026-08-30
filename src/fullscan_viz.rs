//! Animated GIF visualization of `vision`'s full-scan line detector (the literal "slice every
//! row left to right, every column top to bottom, note every color change, keep the positions
//! a lot of them agree on" idea) -- so a human can watch it sweep the image and see the
//! agreement histogram build up, the same way `growth_viz` visualizes the seed-growth
//! detector. Not part of the daemon's normal runtime -- invoked via
//! `omakeys visualize-fullscan`.
//!
//! Drives the exact same functions `vision::full_scan_lines` itself calls (`is_edge_pixel` via
//! its own per-row/per-column loop here, then `vision::full_scan_counts`/`full_scan_lines` for
//! the final tally) -- this module only adds recording and drawing in between.

use std::path::Path;
use std::time::Duration;

use image::codecs::gif::{GifEncoder, Repeat};
use image::{Delay, Frame, Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_line_segment_mut};
use imageproc::rect::Rect;

use crate::vision;

/// Which extra filter (beyond the plain density threshold) `render()` applies for its final
/// stage -- see `render`'s doc comment.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CheckKind {
    /// Density threshold only -- `vision::full_scan_lines`.
    None,
    /// Also requires a local peak against the neighborhood -- `full_scan_lines_with_peak_check`.
    Peak,
    /// Also requires a consistent edge magnitude along the line's own span --
    /// `full_scan_lines_with_uniformity_check`.
    Uniformity,
}

/// Width of the right-hand column-count histogram strip / height of the bottom row-count
/// histogram strip, in pixels.
const MARGIN: u32 = 64;

/// The sweep is revealed in this many batches, however many rows/columns `window` actually
/// has -- keeps it watchable regardless of crop size.
const SWEEP_REVEAL_FRAMES: usize = 16;

const HOLD_FRAME_COUNT: usize = 5;
const FINAL_HOLD_FRAME_COUNT: usize = 10;
const REVEAL_DELAY_MS: u64 = 70;
const HOLD_DELAY_MS: u64 = 140;

const CURSOR: Rgba<u8> = Rgba([255, 255, 255, 255]);
const HIT: Rgba<u8> = Rgba([255, 70, 70, 255]);
const BAR: Rgba<u8> = Rgba([80, 170, 255, 255]);
const BAR_REJECTED: Rgba<u8> = Rgba([255, 130, 30, 255]);
const BAR_SURVIVING: Rgba<u8> = Rgba([255, 220, 0, 255]);
const THRESHOLD_LINE: Rgba<u8> = Rgba([255, 70, 70, 255]);
const GLOW_HALO: Rgba<u8> = Rgba([140, 120, 0, 255]);
const GLOW_CORE: Rgba<u8> = Rgba([255, 255, 90, 255]);
const MARGIN_BG: Rgba<u8> = Rgba([16, 16, 20, 255]);

/// Build the base canvas: the grayscale crop in the top-left `ww` x `wh` area, a dark margin
/// strip reserved along the bottom (`MARGIN` tall, for the column-count histogram) and right
/// (`MARGIN` wide, for the row-count histogram).
fn base_canvas(gray: &[u8], img_w: u32, window: (i32, i32, i32, i32)) -> RgbaImage {
    let (wx, wy, ww, wh) = window;
    let mut canvas = RgbaImage::from_pixel(ww as u32 + MARGIN, wh as u32 + MARGIN, MARGIN_BG);
    for y in 0..wh {
        for x in 0..ww {
            let v = gray[((wy + y) as u32 * img_w + (wx + x) as u32) as usize];
            canvas.put_pixel(x as u32, y as u32, Rgba([v, v, v, 255]));
        }
    }
    canvas
}

fn push_frame(frames: &mut Vec<Frame>, canvas: &RgbaImage, delay_ms: u64) {
    frames.push(Frame::from_parts(canvas.clone(), 0, 0, Delay::from_saturating_duration(Duration::from_millis(delay_ms))));
}

/// A bar's fate, decided per x/y position -- drives its histogram color. `NeverCleared` is
/// the default (plain blue) for a position whose count never reached the match-fraction
/// threshold at all.
#[derive(Clone, Copy, PartialEq)]
enum BarState {
    NeverCleared,
    RejectedByPeakCheck,
    Surviving,
}

fn bar_color(state: BarState) -> Rgba<u8> {
    match state {
        BarState::NeverCleared => BAR,
        BarState::RejectedByPeakCheck => BAR_REJECTED,
        BarState::Surviving => BAR_SURVIVING,
    }
}

/// Draw the bottom histogram strip (one bar per x, height proportional to `col_counts[x]` /
/// `max_count`) and the right histogram strip (one bar per y, width proportional to
/// `row_counts[y]` / `max_count`) onto `canvas`. `states` colors each bar by its fate (see
/// `BarState`); `None` means "not decided yet" (every bar plain blue).
fn draw_histograms(
    canvas: &mut RgbaImage,
    col_counts: &[u32],
    row_counts: &[u32],
    max_col: u32,
    max_row: u32,
    states_x: Option<&[BarState]>,
    states_y: Option<&[BarState]>,
) {
    let ww = col_counts.len() as u32;
    let wh = row_counts.len() as u32;
    draw_filled_rect_mut(canvas, Rect::at(0, wh as i32).of_size(ww, MARGIN), MARGIN_BG);
    draw_filled_rect_mut(canvas, Rect::at(ww as i32, 0).of_size(MARGIN, wh), MARGIN_BG);

    for (x, &c) in col_counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let h = (c as f64 / max_col.max(1) as f64 * MARGIN as f64).round() as u32;
        let color = states_x.map(|s| bar_color(s[x])).unwrap_or(BAR);
        draw_filled_rect_mut(canvas, Rect::at(x as i32, (wh + MARGIN).saturating_sub(h) as i32).of_size(1, h.max(1)), color);
    }
    for (y, &c) in row_counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let w = (c as f64 / max_row.max(1) as f64 * MARGIN as f64).round() as u32;
        let color = states_y.map(|s| bar_color(s[y])).unwrap_or(BAR);
        draw_filled_rect_mut(canvas, Rect::at((ww + MARGIN).saturating_sub(w) as i32, y as i32).of_size(w.max(1), 1), color);
    }
}

/// Draw a thick "glowing" line: a dim, wide halo underneath a bright, thin core -- image crate
/// drawing here is a plain overwrite (no real alpha blending), so this fakes a glow rather than
/// computing one.
fn draw_glow_line(canvas: &mut RgbaImage, (x0, y0): (f32, f32), (x1, y1): (f32, f32)) {
    for offset in [-2.0, -1.0, 0.0, 1.0, 2.0] {
        let (a, b) = if (x1 - x0).abs() < 0.5 {
            ((x0 + offset, y0), (x1 + offset, y1))
        } else {
            ((x0, y0 + offset), (x1, y1 + offset))
        };
        draw_line_segment_mut(canvas, a, b, GLOW_HALO);
    }
    draw_line_segment_mut(canvas, (x0, y0), (x1, y1), GLOW_CORE);
}

/// Run the full-scan line detector against `gray` (a full captured monitor, `img_w`x`img_h`)
/// restricted to `window`, recording each stage as GIF frames, and write the result to
/// `out_path`:
///
/// 1. row sweep, top to bottom -- a scan cursor advances down the frame; every color change it
///    finds marks a hit and grows that x's bar in the bottom histogram
/// 2. column sweep, left to right -- same, growing the right-hand histogram
/// 3. both histograms held with the 50%-agreement threshold line drawn across them
/// 4. the final result -- glowing lines on the image, held longest. With `check:
///    CheckKind::None`, that's every bar that cleared the threshold line (blue -> yellow).
///    With `CheckKind::Peak` or `CheckKind::Uniformity`, the matching `vision::full_scan_lines_*`
///    variant applies its extra filter on top -- a bar that cleared the density threshold but
///    got rejected there turns orange instead of yellow, so the two runs are directly
///    comparable.
pub fn render(
    gray: &[u8],
    img_w: u32,
    img_h: u32,
    window: (i32, i32, i32, i32),
    check: CheckKind,
    out_path: &Path,
) -> anyhow::Result<()> {
    let (wx, wy, ww, wh) = window;
    let base = base_canvas(gray, img_w, window);
    let mut frames: Vec<Frame> = Vec::new();

    let mut col_counts = vec![0u32; ww as usize];
    let mut row_counts = vec![0u32; wh as usize];

    // Stage 1: row sweep, top to bottom.
    let row_batch = ((wh as usize) / SWEEP_REVEAL_FRAMES).max(1);
    let mut y = 0usize;
    while y < wh as usize {
        let batch_end = (y + row_batch).min(wh as usize);
        let mut canvas = base.clone();
        for row in y..batch_end {
            for col in 0..ww as usize {
                if vision::is_edge_pixel(gray, img_w, img_h, wx + col as i32, wy + row as i32, vision::Orientation::Vertical) {
                    col_counts[col] += 1;
                    canvas.put_pixel(col as u32, row as u32, HIT);
                }
            }
        }
        draw_line_segment_mut(&mut canvas, (0.0, batch_end as f32 - 1.0), (ww as f32, batch_end as f32 - 1.0), CURSOR);
        draw_histograms(&mut canvas, &col_counts, &row_counts, wh as u32, ww as u32, None, None);
        push_frame(&mut frames, &canvas, REVEAL_DELAY_MS);
        y = batch_end;
    }

    // Stage 2: column sweep, left to right.
    let col_batch = ((ww as usize) / SWEEP_REVEAL_FRAMES).max(1);
    let mut x = 0usize;
    while x < ww as usize {
        let batch_end = (x + col_batch).min(ww as usize);
        let mut canvas = base.clone();
        for row in 0..wh as usize {
            for col in x..batch_end {
                if vision::is_edge_pixel(gray, img_w, img_h, wx + col as i32, wy + row as i32, vision::Orientation::Horizontal) {
                    row_counts[row] += 1;
                }
            }
        }
        // Re-mark every vertical hit already found (stage 1 is done, its canvas is gone).
        for row in 0..wh as usize {
            for col in 0..ww as usize {
                if vision::is_edge_pixel(gray, img_w, img_h, wx + col as i32, wy + row as i32, vision::Orientation::Vertical) {
                    canvas.put_pixel(col as u32, row as u32, HIT);
                }
            }
        }
        draw_line_segment_mut(&mut canvas, (batch_end as f32 - 1.0, 0.0), (batch_end as f32 - 1.0, wh as f32), CURSOR);
        draw_histograms(&mut canvas, &col_counts, &row_counts, wh as u32, ww as u32, None, None);
        push_frame(&mut frames, &canvas, REVEAL_DELAY_MS);
        x = batch_end;
    }

    // Stage 3: both histograms complete, threshold line drawn across each.
    let (col_threshold, row_threshold) = ((wh as f64 * 0.5) as u32, (ww as f64 * 0.5) as u32);
    let mut threshold_canvas = base.clone();
    for row in 0..wh as usize {
        for col in 0..ww as usize {
            if vision::is_edge_pixel(gray, img_w, img_h, wx + col as i32, wy + row as i32, vision::Orientation::Vertical) {
                threshold_canvas.put_pixel(col as u32, row as u32, HIT);
            }
        }
    }
    draw_histograms(&mut threshold_canvas, &col_counts, &row_counts, wh as u32, ww as u32, None, None);
    let bottom_threshold_y = (wh + (MARGIN as i32) - (col_threshold as f64 / wh as f64 * MARGIN as f64) as i32).max(0);
    draw_line_segment_mut(&mut threshold_canvas, (0.0, bottom_threshold_y as f32), (ww as f32, bottom_threshold_y as f32), THRESHOLD_LINE);
    let right_threshold_x = (ww + (MARGIN as i32) - (row_threshold as f64 / ww as f64 * MARGIN as f64) as i32).max(0);
    draw_line_segment_mut(&mut threshold_canvas, (right_threshold_x as f32, 0.0), (right_threshold_x as f32, wh as f32), THRESHOLD_LINE);
    for _ in 0..HOLD_FRAME_COUNT {
        push_frame(&mut frames, &threshold_canvas, HOLD_DELAY_MS);
    }

    // Stage 4: the final result -- surviving positions drawn as glowing lines. Always compute
    // the threshold-only result too (even when an extra check is active) so a bar that
    // cleared the density threshold but got rejected by the check can be told apart from one
    // that never cleared the threshold at all -- orange vs. plain blue.
    let (threshold_xs, threshold_ys) = vision::full_scan_lines(gray, img_w, img_h, window);
    let (xs, ys) = match check {
        CheckKind::None => (threshold_xs.clone(), threshold_ys.clone()),
        CheckKind::Peak => vision::full_scan_lines_with_peak_check(gray, img_w, img_h, window),
        CheckKind::Uniformity => vision::full_scan_lines_with_uniformity_check(gray, img_w, img_h, window),
    };

    let bar_state = |pos: i32, cleared: &[i32], survived: &[i32]| -> BarState {
        if survived.iter().any(|&s| s == pos) {
            BarState::Surviving
        } else if cleared.iter().any(|&s| s == pos) {
            BarState::RejectedByPeakCheck
        } else {
            BarState::NeverCleared
        }
    };
    let states_x: Vec<BarState> = (0..ww).map(|c| bar_state(wx + c, &threshold_xs, &xs)).collect();
    let states_y: Vec<BarState> = (0..wh).map(|r| bar_state(wy + r, &threshold_ys, &ys)).collect();

    let mut final_canvas = base.clone();
    draw_histograms(&mut final_canvas, &col_counts, &row_counts, wh as u32, ww as u32, Some(&states_x), Some(&states_y));
    for &lx in &xs {
        let local_x = (lx - wx) as f32;
        draw_glow_line(&mut final_canvas, (local_x, 0.0), (local_x, wh as f32));
    }
    for &ly in &ys {
        let local_y = (ly - wy) as f32;
        draw_glow_line(&mut final_canvas, (0.0, local_y), (ww as f32, local_y));
    }
    for _ in 0..FINAL_HOLD_FRAME_COUNT {
        push_frame(&mut frames, &final_canvas, HOLD_DELAY_MS);
    }

    let file = std::fs::File::create(out_path)?;
    let mut encoder = GifEncoder::new(file);
    encoder.set_repeat(Repeat::Infinite)?;
    encoder.encode_frames(frames)?;
    let check_name = match check {
        CheckKind::None => "none",
        CheckKind::Peak => "peak",
        CheckKind::Uniformity => "uniformity",
    };
    log::info!(
        "omakeys: wrote full-scan visualization to {} (check={check_name}, {} vertical lines, \
         {} horizontal lines found -- threshold alone found {} / {})",
        out_path.display(),
        xs.len(),
        ys.len(),
        threshold_xs.len(),
        threshold_ys.len()
    );
    Ok(())
}
