//! Animated GIF visualization of `vision`'s experimental seed-growth box detector, so a human
//! can actually watch the algorithm run against a real screenshot rather than only see its
//! final output. Not part of the daemon's normal runtime -- invoked directly via
//! `omakeys visualize-growth`.
//!
//! Drives the exact same functions `vision::detect_regions_by_growth` itself calls (same seed
//! positions, same ray-cast/trace/merge/pair/refine logic) -- this module only adds recording
//! and drawing in between each stage, it doesn't reimplement any of the algorithm.

use std::path::Path;
use std::time::Duration;

use image::codecs::gif::{GifEncoder, Repeat};
use image::{Delay, Frame, Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_hollow_rect_mut, draw_line_segment_mut};
use imageproc::rect::Rect;

use crate::vision::{self, Orientation, RayDirection, Segment};

/// Seeds are revealed in this many batches, however many seeds `window` actually contains --
/// keeps the "growth" stage watchable (a dozen or so quick reveals) regardless of whether
/// `window` is a small crop or a whole monitor.
const SEED_REVEAL_FRAMES: usize = 14;

/// How many times each "settled" stage (raw segments, merged, paired) is repeated, so it's
/// actually visible to a human rather than flashing by in one frame.
const HOLD_FRAME_COUNT: usize = 5;

/// The final "boxes found" stage is the payoff -- held noticeably longer than the others.
const FINAL_HOLD_FRAME_COUNT: usize = 10;

const REVEAL_DELAY_MS: u64 = 90;
const HOLD_DELAY_MS: u64 = 140;

const HISTORICAL_SEED: Rgba<u8> = Rgba([60, 60, 160, 255]);
const ACTIVE_SEED: Rgba<u8> = Rgba([255, 220, 0, 255]);
const RAY: Rgba<u8> = Rgba([255, 140, 0, 255]);
const HIT: Rgba<u8> = Rgba([255, 60, 60, 255]);
const RAW_SEGMENT: Rgba<u8> = Rgba([60, 220, 90, 255]);
const MERGED_SEGMENT: Rgba<u8> = Rgba([0, 220, 220, 255]);
const SURVIVING_SEGMENT: Rgba<u8> = Rgba([255, 60, 220, 255]);
const DISCARDED_SEGMENT: Rgba<u8> = Rgba([90, 90, 90, 255]);
const FINAL_BOX: Rgba<u8> = Rgba([255, 255, 0, 255]);

/// Build a base RGBA canvas (grayscale capture, cropped to `window`) that every frame is drawn
/// on top of.
fn base_canvas(gray: &[u8], img_w: u32, window: (i32, i32, i32, i32)) -> RgbaImage {
    let (wx, wy, ww, wh) = window;
    let mut canvas = RgbaImage::new(ww as u32, wh as u32);
    for y in 0..wh {
        for x in 0..ww {
            let v = gray[((wy + y) as u32 * img_w + (wx + x) as u32) as usize];
            canvas.put_pixel(x as u32, y as u32, Rgba([v, v, v, 255]));
        }
    }
    canvas
}

fn to_local((x, y): (i32, i32), (wx, wy, _, _): (i32, i32, i32, i32)) -> (i32, i32) {
    (x - wx, y - wy)
}

fn draw_segment(canvas: &mut RgbaImage, seg: &Segment, color: Rgba<u8>, window: (i32, i32, i32, i32), thickness: i32) {
    let (fixed, start, end) = (seg.fixed, seg.start, seg.end);
    for t in -(thickness / 2)..=(thickness / 2) {
        let (a, b) = match seg.orientation {
            Orientation::Vertical => (to_local((fixed + t, start), window), to_local((fixed + t, end), window)),
            Orientation::Horizontal => (to_local((start, fixed + t), window), to_local((end, fixed + t), window)),
        };
        draw_line_segment_mut(canvas, (a.0 as f32, a.1 as f32), (b.0 as f32, b.1 as f32), color);
    }
}

fn draw_dot(canvas: &mut RgbaImage, point: (i32, i32), window: (i32, i32, i32, i32), color: Rgba<u8>, size: i32) {
    let (lx, ly) = to_local(point, window);
    let (_, _, ww, wh) = window;
    if lx < 0 || ly < 0 || lx >= ww || ly >= wh {
        return;
    }
    let rect = Rect::at((lx - size / 2).max(0), (ly - size / 2).max(0)).of_size(size as u32, size as u32);
    draw_filled_rect_mut(canvas, rect, color);
}

fn push_frame(frames: &mut Vec<Frame>, canvas: &RgbaImage, delay_ms: u64) {
    frames.push(Frame::from_parts(canvas.clone(), 0, 0, Delay::from_saturating_duration(Duration::from_millis(delay_ms))));
}

/// Run the seed-growth detector against `gray` (a full captured monitor, `img_w`x`img_h`)
/// restricted to `window`, recording each stage as GIF frames, and write the result to
/// `out_path`:
///
/// 1. seeds + rays revealed in batches, with every raw traced segment found so far drawn
///    cumulatively underneath (the actual "feeling out the boundaries" step)
/// 2. all raw traced segments, cleanly
/// 3. merged (duplicate re-discoveries by different seeds consolidated)
/// 4. length-matched survivors (bright) vs. discarded singletons (dim) -- the "share the same
///    length" filter
/// 5. the final detected box(es), refined -- the payoff, held longest
pub fn render(gray: &[u8], img_w: u32, img_h: u32, window: (i32, i32, i32, i32), out_path: &Path) -> anyhow::Result<()> {
    let base = base_canvas(gray, img_w, window);
    let mut frames: Vec<Frame> = Vec::new();

    let seeds = vision::seed_positions(window);
    let batch_size = (seeds.len() / SEED_REVEAL_FRAMES).max(1);
    let mut all_segments: Vec<Segment> = Vec::new();
    let mut historical_seeds: Vec<(i32, i32)> = Vec::new();

    for batch in seeds.chunks(batch_size) {
        let mut canvas = base.clone();
        for &s in &historical_seeds {
            draw_dot(&mut canvas, s, window, HISTORICAL_SEED, 2);
        }
        for seg in &all_segments {
            draw_segment(&mut canvas, seg, RAW_SEGMENT, window, 1);
        }
        for &seed in batch {
            draw_dot(&mut canvas, seed, window, ACTIVE_SEED, 4);
            for dir in [RayDirection::Left, RayDirection::Right, RayDirection::Up, RayDirection::Down] {
                if let Some((hx, hy, orientation)) = vision::ray_cast(gray, img_w, img_h, seed, dir, window) {
                    let (sx, sy) = to_local(seed, window);
                    let (lx, ly) = to_local((hx, hy), window);
                    draw_line_segment_mut(&mut canvas, (sx as f32, sy as f32), (lx as f32, ly as f32), RAY);
                    draw_dot(&mut canvas, (hx, hy), window, HIT, 3);
                    all_segments.push(vision::trace_edge(gray, img_w, img_h, (hx, hy), orientation, window));
                }
            }
        }
        historical_seeds.extend(batch);
        push_frame(&mut frames, &canvas, REVEAL_DELAY_MS);
    }

    let mut raw_canvas = base.clone();
    for seg in &all_segments {
        draw_segment(&mut raw_canvas, seg, RAW_SEGMENT, window, 1);
    }
    for _ in 0..HOLD_FRAME_COUNT {
        push_frame(&mut frames, &raw_canvas, HOLD_DELAY_MS);
    }

    let raw_segment_count = all_segments.len();
    let merged = vision::merge_segments(all_segments);
    let mut merged_canvas = base.clone();
    for seg in &merged {
        draw_segment(&mut merged_canvas, seg, MERGED_SEGMENT, window, 2);
    }
    for _ in 0..HOLD_FRAME_COUNT {
        push_frame(&mut frames, &merged_canvas, HOLD_DELAY_MS);
    }

    let v_pairs = vision::length_matched_pairs(&merged, Orientation::Vertical);
    let h_pairs = vision::length_matched_pairs(&merged, Orientation::Horizontal);
    let is_surviving = |seg: &Segment| {
        v_pairs.iter().chain(h_pairs.iter()).any(|(a, b)| segment_eq(a, seg) || segment_eq(b, seg))
    };
    let mut pairs_canvas = base.clone();
    for seg in &merged {
        let color = if is_surviving(seg) { SURVIVING_SEGMENT } else { DISCARDED_SEGMENT };
        draw_segment(&mut pairs_canvas, seg, color, window, 2);
    }
    for _ in 0..HOLD_FRAME_COUNT {
        push_frame(&mut frames, &pairs_canvas, HOLD_DELAY_MS);
    }

    let boxes = vision::boxes_from_pairs(gray, img_w, img_h, &v_pairs, &h_pairs);
    let mut box_canvas = base.clone();
    for &(bx, by, bw, bh) in &boxes {
        let (lx, ly) = to_local((bx, by), window);
        draw_hollow_rect_mut(&mut box_canvas, Rect::at(lx, ly).of_size(bw as u32, bh as u32), FINAL_BOX);
        if bw > 4 && bh > 4 {
            // A 1px hollow rect is easy to miss in a GIF -- a second, inset one gives a
            // visibly thick outline without needing real per-pixel line-width control.
            draw_hollow_rect_mut(&mut box_canvas, Rect::at(lx + 1, ly + 1).of_size((bw - 2) as u32, (bh - 2) as u32), FINAL_BOX);
        }
    }
    for _ in 0..FINAL_HOLD_FRAME_COUNT {
        push_frame(&mut frames, &box_canvas, HOLD_DELAY_MS);
    }

    let file = std::fs::File::create(out_path)?;
    let mut encoder = GifEncoder::new(file);
    encoder.set_repeat(Repeat::Infinite)?;
    encoder.encode_frames(frames)?;
    log::info!(
        "omakeys: wrote growth visualization to {} ({} seeds, {} raw segments -> {} merged, {} boxes)",
        out_path.display(),
        seeds.len(),
        raw_segment_count,
        merged.len(),
        boxes.len()
    );
    Ok(())
}

fn segment_eq(a: &Segment, b: &Segment) -> bool {
    a.orientation == b.orientation && a.fixed == b.fixed && a.start == b.start && a.end == b.end
}
