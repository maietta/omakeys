//! Vision-based hint fallback: classical rectangle detection over a screenshot, for apps
//! AT-SPI can't see into at all (GTK4's position bug, Electron apps without a11y
//! force-enabled -- see atspi_scan.rs's module docs). This is deliberately a *coarse*
//! signal -- "there is a rectangular UI element roughly here" -- not a classification; it
//! exists to catch what AT-SPI structurally misses, not to replace it where AT-SPI works.
//!
//! `DEBUG_SECTIONS_ONLY` below temporarily disables the contour-based button/icon/panel
//! detector so only `detect_dividers_as_grid()`'s section boxes render -- see its doc
//! comment. Flip it back to `false` once section detection is confirmed working the way
//! it's expected to.

use imageproc::contours::{self, Contour};

use crate::active_monitor::FocusedWindow;
use crate::screencap::Capture;

/// A candidate UI region found by edge/contour detection, in the same pixel coordinates as
/// the captured monitor (i.e. already monitor-local, since the screenshot itself is).
#[derive(Debug, Clone, Copy)]
pub struct VisionRegion {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Debug switch: when `true`, `detect_regions()` skips the contour-based button/icon/panel
/// detector entirely and only returns section boxes from `detect_dividers_as_grid()`. Added
/// to isolate-test section detection on its own, since with both detectors active a screen
/// full of small button/icon boxes made it hard to tell at a glance whether the big
/// vertical/horizontal section dividers were being found at all.
const DEBUG_SECTIONS_ONLY: bool = true;

/// Smallest plausible widget size in pixels -- below this, contours are almost always text
/// glyph fragments or icon detail, not a whole clickable element.
const MIN_SIZE: i32 = 14;

/// A box larger than this fraction of the monitor in either dimension is almost always a
/// window/panel background, not something worth hinting on its own.
const MAX_SIZE_FRACTION: f64 = 0.9;

/// Two boxes with IoU (intersection / union) above this are treated as the same detection
/// (e.g. a widget's outer border and an inner highlight line a couple pixels in -- nearly
/// the same rectangle) and only the larger is kept. Deliberately *not* a containment-ratio
/// test: a small button sitting inside a much larger panel also has high containment
/// relative to its own area, but a low IoU (the panel is mostly not-the-button), so IoU
/// correctly leaves genuinely nested-but-distinct elements alone.
const DEDUP_IOU: f64 = 0.7;

/// Grayscale standard deviation above this, inside a candidate box, marks it as "busy" --
/// a code minimap, a photo, a wallpaper -- rather than a UI *control*, which is typically
/// close to a flat fill (maybe a border, an icon, or a couple lines of text) by comparison.
/// Only applied below `MIN_PANEL_SIZE` -- see there for why.
const MAX_STD_DEV: f64 = 45.0;

/// How much of a contour's bounding box its traced outline actually encloses (shoelace-
/// formula area / bbox area). A real UI panel or control -- sharp-cornered or rounded --
/// fills nearly all of its bounding box, losing only a small sliver at rounded corners;
/// organic shapes (photo content, clusters of text) generally don't. This is what lets
/// rounded-corner panels register as clean rectangles instead of being penalized for not
/// being perfectly axis-aligned.
const MIN_RECTANGULARITY: f64 = 0.85;

/// A box at least this big in both dimensions is treated as a structural *panel* (a
/// terminal pane, an editor pane, a sidebar) rather than a discrete control, and skips the
/// flatness check -- a panel is *supposed* to be visually busy inside (that's real content,
/// not noise); only its outline needs to look like a clean rectangle
/// (`MIN_RECTANGULARITY` still applies to contour-traced panel candidates).
const MIN_PANEL_SIZE: i32 = 150;

/// A candidate divider line (see `detect_dividers_as_grid`) must have edge pixels across at
/// least this fraction of its window's height/width to count as a real panel boundary, as
/// opposed to an incidental long-ish line in ordinary content (a horizontal rule, a table
/// border). Tuned against a real capture: VS Code's editor|chat-panel divider only reached
/// ~41% continuous coverage (interrupted by overlapping chat-message-bubble borders
/// crossing it, presumably), so anything close to 0.5 misses real dividers in practice.
const DIVIDER_COVERAGE: f64 = 0.35;

/// Divider positions within `gap` pixels of each other are treated as the same line --
/// Canny commonly gives a real 1px divider a few pixels of width after anti-aliasing.
const DIVIDER_MERGE_GAP: i32 = 4;

/// When recursing into a strip, its own scan area is inset by this many pixels on every
/// side before searching for further dividers. Without this, the divider that *created*
/// this strip is still sitting exactly at its edge (that's what a cut point is) and gets
/// re-detected as if it were internal structure -- confirmed live via a failing test: a
/// column carved out by a real vertical divider re-found that same divider as an
/// "internal" one immediately upon recursing in, which then wrongly suppressed a
/// genuine row divider inside the column (columns take priority over rows -- see
/// `had_x_dividers`/`had_y_dividers` below).
const RECURSE_INSET: i32 = DIVIDER_MERGE_GAP + 2;

/// A candidate line's *span* -- the distance between its first and last edge pixel -- must
/// cover at least this fraction of the window's height/width to count as a real divider,
/// independent of `DIVIDER_COVERAGE`. This is the more important signal for lines that are
/// broken up by overlapping content in the *middle* (a chat bubble crossing a vertical
/// divider, an icon crossing a horizontal one) but still genuinely run edge to edge: such a
/// line can have fairly low density yet still start near y=0 and end near y=height, which
/// `DIVIDER_COVERAGE` alone doesn't credit. A column/row qualifies as a divider if it clears
/// *either* the density check or the span check (`qualifies()`) -- span for interrupted
/// edge-to-edge lines, density for short-but-solid ones that don't quite reach the edges
/// (title bar / status bar chrome can shave a few px off either end).
const DIVIDER_SPAN_COVERAGE: f64 = 0.85;

/// Even a span-qualifying line needs at least this much actual edge-pixel presence within
/// its span, or two unrelated specks near opposite edges (with nothing genuine in between)
/// would count as a "divider" purely by coincidence.
const DIVIDER_MIN_DENSITY: f64 = 0.15;

/// A candidate row/column must be noticeably denser than the rows/columns just outside its
/// own line width, by at least this ratio, to count as a real divider. A single line of
/// left-aligned text (a comment, terminal output) can otherwise pass both the density and
/// span checks purely because it happens to stretch most of the width -- but unlike a real
/// divider, it's part of a broad *band* of similarly-dense rows (every row of text in that
/// paragraph looks much the same), not a narrow spike standing out from near-empty
/// neighbors. Confirmed necessary against a real capture: two separate text lines
/// (a comment, terminal output) both passed density+span until this was added.
const DIVIDER_PEAK_RATIO: f64 = 1.6;

/// The neighborhood sampled on each side of a candidate when checking `DIVIDER_PEAK_RATIO`
/// is `[index ± PEAK_OFFSET_MIN, index ± PEAK_OFFSET_MAX]` -- a *range*, averaged, not a
/// single sample point. A single point (tried first) turned out to be fragile: text has its
/// own internal rhythm at the scale of individual line-heights, so one specific offset can
/// land in a natural gap between text lines and look like "the neighbor is empty" purely by
/// coincidence, without the candidate actually being a divider. Averaging over a wider band
/// smooths that out.
///
/// `PEAK_OFFSET_MIN` must be wider than the gap between the two parallel lines a rounded
/// panel border draws (its outer and inner edge) -- confirmed live: VS Code's rounded
/// sidebar border is two real lines 5px apart, and with `PEAK_OFFSET_MIN = 5` the
/// neighbor-check for one line's peak-ratio landed exactly on its sibling (itself ~93%
/// dense), making a genuine divider look like "a broad band" -- the exact pattern this
/// check exists to reject for a text paragraph -- and rejecting it. Raised past that gap so
/// the sampled neighborhood starts on genuine background just beyond the sibling line
/// instead of on the sibling line itself.
const DIVIDER_PEAK_OFFSET_MIN: usize = 8;
const DIVIDER_PEAK_OFFSET_MAX: usize = 28;

/// Running min/max edge-pixel position and total count for one column or row.
///
/// Two more signals were also tried here and reverted, on the theory that a real divider
/// looks different from a line of text along its own length:
///
/// - Longest single continuous run of edge pixels (a real divider is one long stroke,
///   text is many short character-width strokes). Tested against a live capture: the real
///   editor|chat divider's longest run was only 65px/6% of height (chopped up by
///   chat-bubble borders crossing it constantly -- far more finely interrupted than
///   assumed), while a false positive from terminal-rendered content (likely a
///   box-drawing rule in command output) had a *longer* run at 379px/20% of width. Any
///   threshold that filtered the false positive would also have killed the real divider.
///
/// - Grayscale standard deviation of the *original* capture (not the edge map) sampled
///   along the candidate's own span, reusing the `MAX_STD_DEV` flatness signal already
///   used for small controls (a real divider should be close to one solid color end to
///   end; text should not). This one did kill an actual false positive (a comment line
///   right after VS Code's tab bar) but also measured *backwards* on the real editor|chat
///   divider itself: live capture, x=1497, std_dev=29.9, while a different false-positive
///   text row (y=515, sparse chat/code text coincidentally aligned across two unrelated
///   panels) measured std_dev=7.8-9.8 -- lower than the genuine divider. The real
///   divider's own column isn't flat (crosses the tab bar and differently-styled chat vs.
///   editor backgrounds), while a sparse enough text row can look flatter than a real
///   line purely by having mostly-background pixels in its sample.
///
/// Both are inverted or unreliable for this discrimination, not just imprecise. A
/// terminal-drawn horizontal rule / a stray aligned text row and a real UI divider are
/// pixel-statistically indistinguishable by density, span, peak-ratio, run-length, or
/// flatness -- this specific false-positive category (structural-looking lines rendered as
/// *content*, not chrome) is a known, accepted limitation, not a solved problem.
#[derive(Default, Clone, Copy)]
struct LineStats {
    count: u32,
    first: Option<u32>,
    last: Option<u32>,
}

impl LineStats {
    fn record(&mut self, pos: u32) {
        self.count += 1;
        self.first = Some(self.first.map_or(pos, |f| f.min(pos)));
        self.last = Some(self.last.map_or(pos, |l| l.max(pos)));
    }
}

/// A line qualifies as a divider if it's dense enough on its own (the original check,
/// good for short-but-solid lines) *or* if it spans edge to edge with at least some real
/// presence throughout (good for lines interrupted by overlapping content) -- see
/// `DIVIDER_SPAN_COVERAGE`'s doc comment for why both are needed.
fn qualifies(stats: &LineStats, density_threshold: u32, span_threshold: u32, min_density: u32) -> bool {
    if stats.count >= density_threshold {
        return true;
    }
    match (stats.first, stats.last) {
        (Some(first), Some(last)) => last - first >= span_threshold && stats.count >= min_density,
        _ => false,
    }
}

/// Is `counts[index]` a real spike, not part of a broad band of similarly-dense neighbors?
/// See `DIVIDER_PEAK_RATIO`. Right at the edge of the window (no neighborhood on one side),
/// only the side that exists is checked rather than treating the missing side as "zero and
/// therefore trivially a peak" -- a line whose neighborhood is genuinely out of frame
/// shouldn't need to fake out a nonexistent comparison to qualify.
fn is_local_peak(counts: &[u32], index: usize, min_offset: usize, max_offset: usize, ratio: f64) -> bool {
    let side_avg = |dir: i64| -> Option<f64> {
        let mut sum = 0u64;
        let mut n = 0u64;
        for offset in min_offset..=max_offset {
            let idx = index as i64 + dir * offset as i64;
            if idx < 0 {
                continue;
            }
            if let Some(&c) = counts.get(idx as usize) {
                sum += c as u64;
                n += 1;
            }
        }
        (n > 0).then(|| sum as f64 / n as f64)
    };

    let neighbor_avg = match (side_avg(-1), side_avg(1)) {
        (Some(b), Some(a)) => (b + a) / 2.0,
        (Some(b), None) => b,
        (None, Some(a)) => a,
        (None, None) => return true,
    };
    counts[index] as f64 > neighbor_avg * ratio
}

pub fn detect_regions(capture: &Capture, windows: &[FocusedWindow]) -> Vec<VisionRegion> {
    let mut boxes: Vec<(i32, i32, i32, i32)> = Vec::new();

    if !DEBUG_SECTIONS_ONLY {
        let Some(gray_img) = image::GrayImage::from_raw(capture.width, capture.height, capture.gray.clone())
        else {
            log::error!("omakeys: captured buffer size didn't match its own width/height");
            return Vec::new();
        };
        let edges = imageproc::edges::canny(&gray_img, 20.0, 50.0);
        let contours: Vec<Contour<i32>> = contours::find_contours(&edges);

        let max_w = (capture.width as f64 * MAX_SIZE_FRACTION) as i32;
        let max_h = (capture.height as f64 * MAX_SIZE_FRACTION) as i32;

        boxes.extend(contours.iter().filter_map(|c| {
            let b = bounding_box(&c.points)?;
            let (_, _, w, h) = b;
            if w < MIN_SIZE || h < MIN_SIZE || w > max_w || h > max_h {
                return None;
            }
            let is_panel_scale = w >= MIN_PANEL_SIZE && h >= MIN_PANEL_SIZE;
            if is_panel_scale {
                // Panels are supposed to be busy inside (that's real content, not noise),
                // so skip the flatness check -- but their *outline* must look like a clean
                // rectangle, or this would just be "the biggest blob of edges", not a panel.
                if rectangularity(&c.points, b) < MIN_RECTANGULARITY {
                    return None;
                }
            } else {
                // Small icon-only buttons are commonly borderless -- the only traceable
                // edge is the icon glyph itself (a magnifying glass, a gear, ...), which is
                // rarely rectangular. Rectangularity isn't a meaningful signal at this
                // scale, so only flatness applies here.
                if std_dev(&capture.gray, capture.width, b) > MAX_STD_DEV {
                    return None;
                }
            }
            Some(b)
        }));
    }

    // Structural panels (file explorer, editor, terminal, chat, ...) often don't have a
    // single *closed* border for contour tracing to find at all -- e.g. a sidebar divider
    // is just one vertical line, with the panel's other "edges" being the window's own
    // boundary, never actually drawn. Contour tracing can't see those. Detecting the
    // divider *lines* directly and gridding the window between them catches this instead.
    // Deliberately does *not* reuse the Canny edge map above -- see `DIVIDER_RAW_DIFF_THRESHOLD`
    // for why: Canny's internal Gaussian blur washes out the thin (~1px), low-contrast
    // borders real UI dividers turn out to be in practice, confirmed live against VS Code
    // (its rounded-panel borders measured a real, consistent ~13-14 intensity-unit step that
    // Canny found *zero* edge pixels for, even at drastically lowered thresholds).
    for window in windows {
        boxes.extend(detect_dividers_as_grid(
            &capture.gray,
            capture.width,
            capture.height,
            (window.x as i32, window.y as i32, window.w as i32, window.h as i32),
        ));
    }

    // Largest first, so dedup keeps the outermost box of each near-duplicate cluster.
    boxes.sort_by_key(|&(_, _, w, h)| -(w * h));
    let mut kept: Vec<(i32, i32, i32, i32)> = Vec::new();
    for b in boxes {
        if !kept.iter().any(|&k| iou(b, k) > DEDUP_IOU) {
            kept.push(b);
        }
    }

    kept.into_iter()
        .map(|(x, y, w, h)| VisionRegion { x: x as f64, y: y as f64, w: w as f64, h: h as f64 })
        .collect()
}

/// How many levels deep `detect_dividers_as_grid` will recurse into its own output cells
/// looking for finer dividers -- see there for why recursion is needed at all (a divider
/// like a terminal's top border is often scoped to a sub-panel's width, not the whole
/// window's, so its coverage percentage only clears the threshold once it's measured
/// against that narrower width instead of the full window).
const MAX_DIVIDER_DEPTH: u32 = 2;

/// Minimum absolute grayscale delta between two horizontally- or vertically-adjacent pixels
/// (sampled from the *raw* capture, no blur) to count as a divider edge pixel -- see the
/// module doc comment for why this is a deliberately separate signal from the shared Canny
/// edge map used for contour tracing. Canny applies a Gaussian blur before it ever looks at
/// a threshold, which smooths away a single-pixel-wide step entirely; measured directly
/// against a real capture, VS Code's own rounded-panel border is exactly that -- a genuine,
/// consistent ~13-14 intensity-unit step confined to one pixel -- and Canny found *zero*
/// edge pixels there even with its thresholds pushed down to (2.0, 6.0) (which also
/// introduced enough noise elsewhere to fragment unrelated, previously-clean detections).
/// Plain adjacent-pixel differencing has no blur step in front of it, so it sees this
/// directly; ordinary background noise in the same capture measured within +-1 unit,
/// comfortably below this threshold.
const DIVIDER_RAW_DIFF_THRESHOLD: i32 = 8;

/// Find long, mostly-continuous vertical/horizontal lines within `window` (a column/row
/// projection over raw adjacent-pixel differences -- see `DIVIDER_RAW_DIFF_THRESHOLD` --
/// essentially a cheap, axis-aligned-only stand-in for a full Hough line transform, which is
/// overkill here since UI dividers are never drawn at an angle) and grid the window between
/// them into candidate panel regions using the window's own bounds to close the outer edge.
/// This deliberately over-segments irregular layouts (e.g. a terminal that only spans the
/// editor's width, not the sidebar's, still gets gridded against the sidebar's divider too)
/// rather than under-detect -- consistent with vision's "coarse, extra candidates are fine"
/// philosophy elsewhere.
///
/// Recurses into each resulting cell (up to `MAX_DIVIDER_DEPTH`) to find dividers scoped to
/// that sub-panel alone: a first pass over the whole window might find only the sidebar's
/// vertical divider (a terminal's horizontal border, measured against the *whole window's*
/// width, may fall well short of `DIVIDER_COVERAGE` even though it's a completely real
/// line -- it was never meant to span the sidebar too). Re-scanning within the narrower
/// "editor + terminal" cell measures that same line against just its own width instead,
/// where it reads as a much higher percentage.
fn detect_dividers_as_grid(
    gray: &[u8],
    img_w: u32,
    img_h: u32,
    window: (i32, i32, i32, i32),
) -> Vec<(i32, i32, i32, i32)> {
    let mut regions = Vec::new();
    detect_dividers_recursive(gray, img_w, img_h, window, MAX_DIVIDER_DEPTH, &mut regions);
    regions
}

fn detect_dividers_recursive(
    gray: &[u8],
    img_w: u32,
    img_h: u32,
    (wx, wy, ww, wh): (i32, i32, i32, i32),
    depth: u32,
    out: &mut Vec<(i32, i32, i32, i32)>,
) {
    if depth == 0 || ww < MIN_PANEL_SIZE || wh < MIN_PANEL_SIZE {
        return;
    }
    let (x0, x1) = (wx.max(0) as u32, ((wx + ww).max(0) as u32).min(img_w));
    let (y0, y1) = (wy.max(0) as u32, ((wy + wh).max(0) as u32).min(img_h));
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    // Track both density (how many edge pixels) and *span* (how far apart the first and
    // last edge pixel are) per column/row -- see LineStats and qualifies() below for why
    // span is the more important signal.
    //
    // Unlike the old Canny-based version, a column's signal comes *only* from horizontal
    // differencing (adjacent-pixel steps left-to-right, which is what a vertical line
    // produces) and a row's *only* from vertical differencing -- not the same omnidirectional
    // edge pixel feeding both projections. This is a more targeted match for what each
    // projection is actually looking for, and matters more now that there's no Canny/blur
    // step in front smoothing out the difference between "a real 1px line" and "one noisy
    // pixel" -- see `DIVIDER_RAW_DIFF_THRESHOLD`.
    let px = |x: u32, y: u32| -> i32 { gray[y as usize * img_w as usize + x as usize] as i32 };
    let mut col_stats = vec![LineStats::default(); (x1 - x0) as usize];
    let mut row_stats = vec![LineStats::default(); (y1 - y0) as usize];
    for y in y0..y1 {
        for x in x0..x1 {
            if x > x0 && (px(x, y) - px(x - 1, y)).abs() >= DIVIDER_RAW_DIFF_THRESHOLD {
                col_stats[(x - x0) as usize].record(y - y0);
            }
            if y > y0 && (px(x, y) - px(x, y - 1)).abs() >= DIVIDER_RAW_DIFF_THRESHOLD {
                row_stats[(y - y0) as usize].record(x - x0);
            }
        }
    }
    let col_counts: Vec<u32> = col_stats.iter().map(|s| s.count).collect();
    let row_counts: Vec<u32> = row_stats.iter().map(|s| s.count).collect();

    let col_threshold = ((y1 - y0) as f64 * DIVIDER_COVERAGE) as u32;
    let row_threshold = ((x1 - x0) as f64 * DIVIDER_COVERAGE) as u32;

    // Diagnostic only (RUST_LOG=debug) -- was invaluable for tracking down why real
    // dividers weren't registering (see HANDOFF.md's "Vision pipeline tuning notes" #5/#6),
    // kept around for the next time a divider mysteriously doesn't show up.
    if log::log_enabled!(log::Level::Debug) {
        let mut top_cols: Vec<(usize, u32)> = col_counts.iter().copied().enumerate().collect();
        top_cols.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        top_cols.truncate(12);
        let mut top_rows: Vec<(usize, u32)> = row_counts.iter().copied().enumerate().collect();
        top_rows.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        top_rows.truncate(12);
        log::debug!(
            "omakeys: divider debug window=({wx},{wy},{ww},{wh}) col_threshold={col_threshold} \
             row_threshold={row_threshold}\n  top cols (x, count, %): {}\n  top rows (y, count, %): {}",
            top_cols
                .iter()
                .map(|&(i, c)| format!("({}, {c}, {:.0}%)", x0 as usize + i, c as f64 / (y1 - y0) as f64 * 100.0))
                .collect::<Vec<_>>()
                .join(", "),
            top_rows
                .iter()
                .map(|&(i, c)| format!("({}, {c}, {:.0}%)", y0 as usize + i, c as f64 / (x1 - x0) as f64 * 100.0))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    let col_span_threshold = ((y1 - y0) as f64 * DIVIDER_SPAN_COVERAGE) as u32;
    let row_span_threshold = ((x1 - x0) as f64 * DIVIDER_SPAN_COVERAGE) as u32;
    let col_min_density = ((y1 - y0) as f64 * DIVIDER_MIN_DENSITY) as u32;
    let row_min_density = ((x1 - x0) as f64 * DIVIDER_MIN_DENSITY) as u32;

    let mut xs: Vec<i32> = col_stats
        .iter()
        .enumerate()
        .filter(|&(_, s)| qualifies(s, col_threshold, col_span_threshold, col_min_density))
        .filter(|&(i, _)| {
            is_local_peak(&col_counts, i, DIVIDER_PEAK_OFFSET_MIN, DIVIDER_PEAK_OFFSET_MAX, DIVIDER_PEAK_RATIO)
        })
        .map(|(i, _)| x0 as i32 + i as i32)
        .collect();
    let mut ys: Vec<i32> = row_stats
        .iter()
        .enumerate()
        .filter(|&(_, s)| qualifies(s, row_threshold, row_span_threshold, row_min_density))
        .filter(|&(i, _)| {
            is_local_peak(&row_counts, i, DIVIDER_PEAK_OFFSET_MIN, DIVIDER_PEAK_OFFSET_MAX, DIVIDER_PEAK_RATIO)
        })
        .map(|(i, _)| y0 as i32 + i as i32)
        .collect();
    merge_adjacent(&mut xs, DIVIDER_MERGE_GAP);
    merge_adjacent(&mut ys, DIVIDER_MERGE_GAP);

    log::debug!("omakeys: merged xs={xs:?} ys={ys:?} for window=({wx},{wy},{ww},{wh})");

    // No real divider found in *either* direction -- gridding would just produce the
    // window's own full bounds as a single degenerate "cell", which isn't what this
    // function is for (every ordinary single-pane window would otherwise always emit
    // itself as a panel candidate). Only proceed if there's an actual line to grid against.
    if xs.is_empty() && ys.is_empty() {
        return;
    }
    // Recorded *before* inserting the window's own boundaries below, which would
    // otherwise make an axis with zero real dividers indistinguishable from one with
    // real dividers (both end up as a two-element [start, end] list) -- see the strip
    // loops just below, which must skip an axis that found nothing real.
    let (had_x_dividers, had_y_dividers) = (!xs.is_empty(), !ys.is_empty());

    xs.insert(0, wx);
    xs.push(wx + ww);
    ys.insert(0, wy);
    ys.push(wy + wh);

    // Emit full-height column strips whenever real vertical dividers were found -- this
    // is what actually delivers "the whole editor pane" / "the whole chat panel" as one
    // box (confirmed live: before this, a spurious horizontal line elsewhere in the
    // window was fragmenting both vertical panels in half, since x-splits and y-splits
    // were only ever multiplied together into one grid).
    //
    // Row strips are the asymmetric case: only emitted when there's *no* competing
    // column structure at this level (`!had_x_dividers`). A real top-level window is
    // essentially always organized as side-by-side panels first (sidebar | editor |
    // chat) -- a "row" spanning the *entire* width when strong column dividers already
    // exist doesn't correspond to any real boundary, it's stitching together whatever
    // coincidentally-aligned content each independent column happens to have at a
    // similar height. Confirmed live: with real column dividers present, two spurious
    // full-width lines appeared (y=191, y=594) cutting straight through unrelated code
    // text and chat messages -- there was no genuine top-to-bottom split at the window
    // level, only side-by-side ones. Horizontal dividers scoped to *one* column (e.g. a
    // terminal under just the editor, not the sidebar) are still found correctly -- via
    // the recursive call into each column below, where that column's own width is what
    // gets measured, not the whole window's.
    // Recursion happens *into each clean strip*, not into the finer x*y grid-cell
    // intersections (an earlier version did the latter and it was a mistake -- confirmed
    // live: those intersection cells can be small enough that a single line of ordinary
    // code or chat text trivially dominates them, satisfying density/span/peak all at
    // once purely because the "neighborhood" being compared against is tiny. Recursing
    // into a whole clean column/row instead keeps the denominator meaningful, which is
    // what finding a genuine sub-panel divider -- e.g. a terminal under just the editor --
    // actually requires.
    if had_x_dividers {
        for pair_x in xs.windows(2) {
            let (x0, x1) = (pair_x[0], pair_x[1]);
            let w = x1 - x0;
            if w >= MIN_PANEL_SIZE && wh >= MIN_PANEL_SIZE {
                out.push((x0, wy, w, wh));
                detect_dividers_recursive(gray, img_w, img_h, inset((x0, wy, w, wh)), depth - 1, out);
            }
        }
    }
    if had_y_dividers && !had_x_dividers {
        for pair_y in ys.windows(2) {
            let (y0, y1) = (pair_y[0], pair_y[1]);
            let h = y1 - y0;
            if ww >= MIN_PANEL_SIZE && h >= MIN_PANEL_SIZE {
                out.push((wx, y0, ww, h));
                detect_dividers_recursive(gray, img_w, img_h, inset((wx, y0, ww, h)), depth - 1, out);
            }
        }
    }
}

/// Shrink a region by `RECURSE_INSET` on every side (never below a 1x1 sliver) -- see its
/// doc comment for why this matters before recursing into a strip.
fn inset((x, y, w, h): (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let shrink = RECURSE_INSET.min((w - 1) / 2).min((h - 1) / 2).max(0);
    (x + shrink, y + shrink, w - shrink * 2, h - shrink * 2)
}

/// Collapse a sorted-then-deduped run of nearby positions (within `gap` of the last kept
/// one) down to a single representative -- see `DIVIDER_MERGE_GAP`.
fn merge_adjacent(values: &mut Vec<i32>, gap: i32) {
    values.sort_unstable();
    let mut merged = Vec::new();
    for &v in values.iter() {
        if merged.last().is_none_or(|&last: &i32| v - last > gap) {
            merged.push(v);
        }
    }
    *values = merged;
}

/// Drop regions whose center doesn't fall inside any actual window -- otherwise an empty
/// desktop (wallpaper) reports phantom "buttons" purely from photo/gradient edges, since
/// vision has no other way to know there's nothing there to click.
pub fn filter_to_windows(regions: Vec<VisionRegion>, windows: &[FocusedWindow]) -> Vec<VisionRegion> {
    regions
        .into_iter()
        .filter(|r| {
            let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
            windows.iter().any(|w| cx >= w.x && cx <= w.x + w.w && cy >= w.y && cy <= w.y + w.h)
        })
        .collect()
}

/// How much of `bbox` the polygon traced by `points` actually fills -- see
/// `MIN_RECTANGULARITY`.
fn rectangularity(points: &[imageproc::point::Point<i32>], (_, _, w, h): (i32, i32, i32, i32)) -> f64 {
    let bbox_area = w as f64 * h as f64;
    if bbox_area <= 0.0 {
        return 0.0;
    }
    (polygon_area(points) / bbox_area).min(1.0)
}

/// Shoelace-formula area enclosed by a closed polygon boundary.
fn polygon_area(points: &[imageproc::point::Point<i32>]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    let mut sum = 0i64;
    for i in 0..points.len() {
        let p1 = &points[i];
        let p2 = &points[(i + 1) % points.len()];
        sum += p1.x as i64 * p2.y as i64 - p2.x as i64 * p1.y as i64;
    }
    sum.unsigned_abs() as f64 / 2.0
}

fn bounding_box(points: &[imageproc::point::Point<i32>]) -> Option<(i32, i32, i32, i32)> {
    let mut points = points.iter();
    let first = points.next()?;
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.x, first.x, first.y, first.y);
    for p in points {
        min_x = min_x.min(p.x);
        max_x = max_x.max(p.x);
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y);
    }
    Some((min_x, min_y, max_x - min_x, max_y - min_y))
}

/// Standard deviation of grayscale intensity within a box, sampled from the original
/// (pre-edge-detection) capture -- see `MAX_STD_DEV`.
fn std_dev(gray: &[u8], img_w: u32, (x, y, w, h): (i32, i32, i32, i32)) -> f64 {
    let img_w = img_w as i32;
    let mut sum = 0i64;
    let mut sum_sq = 0i64;
    let mut count = 0i64;
    for row in y.max(0)..(y + h) {
        for col in x.max(0)..(x + w) {
            if col >= img_w {
                continue;
            }
            let Some(&px) = gray.get((row as i64 * img_w as i64 + col as i64) as usize) else {
                continue;
            };
            sum += px as i64;
            sum_sq += (px as i64) * (px as i64);
            count += 1;
        }
    }
    if count == 0 {
        return 0.0;
    }
    let mean = sum as f64 / count as f64;
    let variance = (sum_sq as f64 / count as f64) - mean * mean;
    variance.max(0.0).sqrt()
}

/// Standard intersection-over-union.
fn iou(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> f64 {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    let ix = (ax.max(bx), (ax + aw).min(bx + bw));
    let iy = (ay.max(by), (ay + ah).min(by + bh));
    let iw = (ix.1 - ix.0).max(0);
    let ih = (iy.1 - iy.0).max(0);
    let intersection = (iw * ih) as f64;
    let union = (aw * ah) as f64 + (bw * bh) as f64 - intersection;
    if union <= 0.0 {
        0.0
    } else {
        intersection / union
    }
}

// ===== Seed-growth box detection (experimental alternative to detect_dividers_as_grid) =====
//
// `detect_dividers_as_grid` above finds structure by exhaustively scanning every row/column
// of a window and voting on density -- effectively a brute-force, axis-aligned Hough
// transform. This is a different strategy: scatter a dense field of seed points across the
// window, and from each one, grow outward until it "feels" a boundary -- ray-cast in each of
// the 4 cardinal directions to the nearest strong intensity step, then trace along that step
// perpendicular to the ray to find its actual extent (a single ray-cast only ever finds one
// point on a line; tracing is what turns that into a real, length-bearing `Segment`).
//
// A real box's opposite sides are the same length, so after merging duplicate detections
// (many different seeds commonly rediscover the same real divider), only segments whose
// length is matched by *another* segment of the same orientation survive -- an isolated
// segment with no length partner is discarded as incidental content rather than kept as
// structure. A candidate box only exists where a matched vertical pair (left/right) and a
// matched horizontal pair (top/bottom) are *mutually* consistent -- the horizontal pair's
// span lines up with the vertical pair's x-positions and vice versa -- which is what "finding
// the 4 corners" actually means here, not just crossing every detected x against every
// detected y. A final narrow rescan around each rough edge (reusing the same per-pixel test
// the exhaustive scan above uses, but over a strip only a few pixels wide) snaps it to the
// position of maximum edge-pixel density before the box is finalized.
//
// Not yet wired into `detect_regions()`'s live pipeline -- exercised only by the tests below
// for now, so it can be evaluated on its own before deciding whether it replaces or
// complements `detect_dividers_as_grid`.

/// Spacing between seed points before jitter, in pixels -- "tight density": dense enough that
/// a real divider spanning any reasonable distance gets discovered (and traced) by many
/// independent seeds, which is what makes cross-seed agreement meaningful rather than
/// coincidental.
const SEED_SPACING: i32 = 32;

/// Random jitter applied to each seed's position (+-half spacing). Seeds aren't placed on a
/// perfectly regular grid so there's no single systematic offset where every seed's ray
/// happens to land in the same blind spot relative to periodic content (e.g. text
/// line-height) every time -- true per-seed randomness would do this too, but a seeded PRNG
/// keeps growth deterministic and testable, at the cost of coverage being evenly spread
/// rather than genuinely random.
const SEED_JITTER: i32 = SEED_SPACING / 2;

/// Max ray-cast distance from a seed before giving up in that direction -- bounds the cost of
/// growing from a seed sitting in a large blank area.
const MAX_RAY_DISTANCE: i32 = 2000;

/// Small gap (in the direction *along* an already-found edge) tolerated while tracing it
/// before giving up -- a real divider is occasionally interrupted by overlapping content (see
/// `DIVIDER_SPAN_COVERAGE`'s doc comment above for the same phenomenon), so stopping at the
/// very first miss would chop a real divider into short, useless fragments.
const TRACE_GAP_TOLERANCE: i32 = 6;

/// Two segments of the same orientation, with `fixed` coordinates within this many pixels of
/// each other and overlapping/adjacent ranges, are treated as re-discoveries of the same real
/// line by different seeds and merged into one.
const SEGMENT_MERGE_GAP: i32 = 4;

/// Two segments' lengths are considered "the same" -- candidate opposite sides of one box --
/// if they're within this fraction of the longer one.
const LENGTH_MATCH_TOLERANCE: f64 = 0.08;

/// How far around a rough candidate edge to exhaustively rescan for the true peak-density
/// position -- the "finer sampling" pass: growth finds *roughly* where a real divider is
/// cheaply; this narrow, precise rescan (the same per-column/per-row density counting
/// `detect_dividers_recursive` uses over the whole window, but over a strip only a few pixels
/// wide) nails down its exact pixel position before a box's corners are finalized.
const REFINE_MARGIN: i32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Orientation {
    /// A vertical line/divider -- detected by a strong step between horizontally-adjacent
    /// pixels. Its `Segment::fixed` is an x-coordinate, `start`/`end` a y-range.
    Vertical,
    /// A horizontal line/divider -- detected by a strong step between vertically-adjacent
    /// pixels. Its `Segment::fixed` is a y-coordinate, `start`/`end` an x-range.
    Horizontal,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Segment {
    pub(crate) orientation: Orientation,
    pub(crate) fixed: i32,
    pub(crate) start: i32,
    pub(crate) end: i32,
}

impl Segment {
    pub(crate) fn length(&self) -> i32 {
        self.end - self.start
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum RayDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Deterministic pseudo-random generator (xorshift32) -- only ever used to jitter seed
/// positions a few pixels, so pulling in the `rand` crate for it isn't worth it; determinism
/// also keeps growth reproducible/testable rather than flaky.
struct Xorshift32(u32);

impl Xorshift32 {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform in `[-bound, bound]`.
    fn jitter(&mut self, bound: i32) -> i32 {
        if bound <= 0 {
            return 0;
        }
        (self.next() % (2 * bound as u32 + 1)) as i32 - bound
    }
}

/// Is there a strong raw intensity step at `(x, y)` in the direction implied by
/// `orientation` -- a horizontal step (between x-1 and x) for `Vertical` (what a vertical
/// line produces), a vertical step (between y-1 and y) for `Horizontal`. Deliberately the same
/// raw-adjacent-pixel-difference test `detect_dividers_recursive` uses (see
/// `DIVIDER_RAW_DIFF_THRESHOLD`'s doc comment for why Canny's blur is unsuitable here) --
/// factored out so both the exhaustive scan and this seed-growth path share one definition of
/// "edge pixel".
pub(crate) fn is_edge_pixel(gray: &[u8], img_w: u32, img_h: u32, x: i32, y: i32, orientation: Orientation) -> bool {
    if x < 0 || y < 0 || x >= img_w as i32 || y >= img_h as i32 {
        return false;
    }
    let px = |x: i32, y: i32| -> i32 { gray[y as usize * img_w as usize + x as usize] as i32 };
    match orientation {
        Orientation::Vertical => x > 0 && (px(x, y) - px(x - 1, y)).abs() >= DIVIDER_RAW_DIFF_THRESHOLD,
        Orientation::Horizontal => y > 0 && (px(x, y) - px(x, y - 1)).abs() >= DIVIDER_RAW_DIFF_THRESHOLD,
    }
}

/// Walk from `(sx, sy)` in `dir` until finding a pixel where `is_edge_pixel` holds for the
/// orientation that direction probes for (Left/Right probe for a *vertical* divider, Up/Down
/// for a *horizontal* one), up to `MAX_RAY_DISTANCE` or `window`'s own edge. Returns the hit
/// point and the orientation it implies, if any -- this is the "feeling out" step, one ray at
/// a time.
pub(crate) fn ray_cast(
    gray: &[u8],
    img_w: u32,
    img_h: u32,
    (sx, sy): (i32, i32),
    dir: RayDirection,
    (wx, wy, ww, wh): (i32, i32, i32, i32),
) -> Option<(i32, i32, Orientation)> {
    let (dx, dy, orientation, limit) = match dir {
        RayDirection::Left => (-1, 0, Orientation::Vertical, wx),
        RayDirection::Right => (1, 0, Orientation::Vertical, wx + ww - 1),
        RayDirection::Up => (0, -1, Orientation::Horizontal, wy),
        RayDirection::Down => (0, 1, Orientation::Horizontal, wy + wh - 1),
    };
    let (mut x, mut y) = (sx, sy);
    for _ in 0..MAX_RAY_DISTANCE {
        x += dx;
        y += dy;
        if dx > 0 && x > limit || dx < 0 && x < limit || dy > 0 && y > limit || dy < 0 && y < limit {
            return None;
        }
        if is_edge_pixel(gray, img_w, img_h, x, y, orientation) {
            return Some((x, y, orientation));
        }
    }
    None
}

/// Walk outward from `(hx, hy)` (a point already known to be an edge pixel of `orientation`)
/// in both directions along the edge's own length, extending as far as the edge condition
/// keeps holding (within `TRACE_GAP_TOLERANCE`), to find its full span. A ray-cast only ever
/// finds one point on a divider; this is what turns that into an actual `Segment` with a real
/// length to compare against others.
pub(crate) fn trace_edge(
    gray: &[u8],
    img_w: u32,
    img_h: u32,
    (hx, hy): (i32, i32),
    orientation: Orientation,
    (wx, wy, ww, wh): (i32, i32, i32, i32),
) -> Segment {
    let (fixed, along, lo, hi) = match orientation {
        Orientation::Vertical => (hx, hy, wy, wy + wh - 1),
        Orientation::Horizontal => (hy, hx, wx, wx + ww - 1),
    };
    let test = |along: i32| -> bool {
        let (x, y) = match orientation {
            Orientation::Vertical => (fixed, along),
            Orientation::Horizontal => (along, fixed),
        };
        is_edge_pixel(gray, img_w, img_h, x, y, orientation)
    };

    let mut start = along;
    let mut cursor = along;
    let mut gap = 0;
    while cursor > lo {
        cursor -= 1;
        if test(cursor) {
            start = cursor;
            gap = 0;
        } else {
            gap += 1;
            if gap > TRACE_GAP_TOLERANCE {
                break;
            }
        }
    }

    let mut end = along;
    let mut cursor = along;
    let mut gap = 0;
    while cursor < hi {
        cursor += 1;
        if test(cursor) {
            end = cursor;
            gap = 0;
        } else {
            gap += 1;
            if gap > TRACE_GAP_TOLERANCE {
                break;
            }
        }
    }

    Segment { orientation, fixed, start, end }
}

/// Scatter seeds across `window` on a jittered grid (see `SEED_SPACING`/`SEED_JITTER`) and
/// grow a `Segment` from every ray-cast hit at each one.
/// Jittered-grid seed positions across `window` -- see `SEED_SPACING`/`SEED_JITTER`. Factored
/// out of `grow_segments` so `growth_viz` (an animated step-by-step visualization of this
/// algorithm, for a human to actually watch it run) can drive the exact same seed placement a
/// real detection pass uses, one seed at a time, instead of only ever seeing the final result.
pub(crate) fn seed_positions(window: (i32, i32, i32, i32)) -> Vec<(i32, i32)> {
    let (wx, wy, ww, wh) = window;
    let mut rng = Xorshift32((wx as u32).wrapping_mul(2_654_435_761).wrapping_add(wy as u32) ^ 0x9E37_79B9);
    let mut seeds = Vec::new();

    let mut sy = wy + SEED_SPACING / 2;
    while sy < wy + wh {
        let mut sx = wx + SEED_SPACING / 2;
        while sx < wx + ww {
            seeds.push((
                (sx + rng.jitter(SEED_JITTER)).clamp(wx, wx + ww - 1),
                (sy + rng.jitter(SEED_JITTER)).clamp(wy, wy + wh - 1),
            ));
            sx += SEED_SPACING;
        }
        sy += SEED_SPACING;
    }
    seeds
}

fn grow_segments(gray: &[u8], img_w: u32, img_h: u32, window: (i32, i32, i32, i32)) -> Vec<Segment> {
    let mut segments = Vec::new();
    for seed in seed_positions(window) {
        for dir in [RayDirection::Left, RayDirection::Right, RayDirection::Up, RayDirection::Down] {
            if let Some((hx, hy, orientation)) = ray_cast(gray, img_w, img_h, seed, dir, window) {
                segments.push(trace_edge(gray, img_w, img_h, (hx, hy), orientation, window));
            }
        }
    }
    segments
}

/// Consolidate segments that are almost certainly re-discoveries of the same real divider by
/// different seeds -- same orientation, `fixed` within `SEGMENT_MERGE_GAP`, and
/// overlapping/adjacent ranges get unioned into one.
pub(crate) fn merge_segments(mut segments: Vec<Segment>) -> Vec<Segment> {
    segments.sort_by_key(|s| (matches!(s.orientation, Orientation::Horizontal), s.fixed, s.start));
    let mut merged: Vec<Segment> = Vec::new();
    for seg in segments {
        if let Some(last) = merged.last_mut() {
            if last.orientation == seg.orientation
                && (seg.fixed - last.fixed).abs() <= SEGMENT_MERGE_GAP
                && seg.start <= last.end + SEGMENT_MERGE_GAP
            {
                last.start = last.start.min(seg.start);
                last.end = last.end.max(seg.end);
                continue;
            }
        }
        merged.push(seg);
    }
    merged
}

fn lengths_match(a: i32, b: i32) -> bool {
    if a <= 0 || b <= 0 {
        return false;
    }
    (a - b).abs() as f64 <= a.max(b) as f64 * LENGTH_MATCH_TOLERANCE
}

/// Every pair of same-orientation segments whose lengths match closely enough to be
/// candidate opposite sides of one box -- see the module-level doc comment above.
pub(crate) fn length_matched_pairs(segments: &[Segment], orientation: Orientation) -> Vec<(Segment, Segment)> {
    let same: Vec<&Segment> = segments.iter().filter(|s| s.orientation == orientation).collect();
    let mut pairs = Vec::new();
    for i in 0..same.len() {
        for j in (i + 1)..same.len() {
            if lengths_match(same[i].length(), same[j].length()) {
                let (a, b) = if same[i].fixed <= same[j].fixed { (same[i], same[j]) } else { (same[j], same[i]) };
                pairs.push((*a, *b));
            }
        }
    }
    pairs
}

fn coord_matches(a: i32, b: i32) -> bool {
    (a - b).abs() <= SEGMENT_MERGE_GAP * 2
}

/// Snap a rough vertical edge (candidate x) to the x within `REFINE_MARGIN` with the most
/// edge pixels over `y_range` -- the "finer sampling" pass, see `REFINE_MARGIN`.
fn refine_vertical_edge(gray: &[u8], img_w: u32, img_h: u32, x_rough: i32, y_range: (i32, i32)) -> i32 {
    let (y0, y1) = y_range;
    (x_rough - REFINE_MARGIN..=x_rough + REFINE_MARGIN)
        .filter(|&x| x > 0 && x < img_w as i32)
        .max_by_key(|&x| {
            (y0.max(0)..y1.min(img_h as i32)).filter(|&y| is_edge_pixel(gray, img_w, img_h, x, y, Orientation::Vertical)).count()
        })
        .unwrap_or(x_rough)
}

/// Snap a rough horizontal edge (candidate y) to the y within `REFINE_MARGIN` with the most
/// edge pixels over `x_range` -- see `refine_vertical_edge`.
fn refine_horizontal_edge(gray: &[u8], img_w: u32, img_h: u32, y_rough: i32, x_range: (i32, i32)) -> i32 {
    let (x0, x1) = x_range;
    (y_rough - REFINE_MARGIN..=y_rough + REFINE_MARGIN)
        .filter(|&y| y > 0 && y < img_h as i32)
        .max_by_key(|&y| {
            (x0.max(0)..x1.min(img_w as i32)).filter(|&x| is_edge_pixel(gray, img_w, img_h, x, y, Orientation::Horizontal)).count()
        })
        .unwrap_or(y_rough)
}

/// From matched vertical pairs (candidate left/right box edges) and matched horizontal pairs
/// (candidate top/bottom edges), keep only combinations where all four sides are *mutually*
/// consistent -- the horizontal pair's span matches the vertical pair's x-positions, and the
/// vertical pair's span matches the horizontal pair's y-positions. This is what "finding the
/// 4 corners" means here: an incidental length coincidence on one axis alone, with nothing
/// matching it on the other, never produces a box.
pub(crate) fn boxes_from_pairs(
    gray: &[u8],
    img_w: u32,
    img_h: u32,
    v_pairs: &[(Segment, Segment)],
    h_pairs: &[(Segment, Segment)],
) -> Vec<(i32, i32, i32, i32)> {
    let mut boxes = Vec::new();
    for &(vl, vr) in v_pairs {
        let (x0, x1) = (vl.fixed, vr.fixed);
        let (y0, y1) = (vl.start.max(vr.start), vl.end.min(vr.end));
        if x1 - x0 < MIN_PANEL_SIZE || y1 - y0 < MIN_PANEL_SIZE {
            continue;
        }
        for &(ht, hb) in h_pairs {
            if !coord_matches(ht.fixed, y0) || !coord_matches(hb.fixed, y1) {
                continue;
            }
            let (hx0, hx1) = (ht.start.min(hb.start), ht.end.max(hb.end));
            if !coord_matches(hx0, x0) || !coord_matches(hx1, x1) {
                continue;
            }

            let rx0 = refine_vertical_edge(gray, img_w, img_h, x0, (y0, y1));
            let rx1 = refine_vertical_edge(gray, img_w, img_h, x1, (y0, y1));
            let ry0 = refine_horizontal_edge(gray, img_w, img_h, y0, (rx0, rx1));
            let ry1 = refine_horizontal_edge(gray, img_w, img_h, y1, (rx0, rx1));
            if rx1 > rx0 && ry1 > ry0 {
                boxes.push((rx0, ry0, rx1 - rx0, ry1 - ry0));
            }
        }
    }
    boxes
}

/// Detect box-shaped UI regions by growing outward from a dense field of seed points, rather
/// than exhaustively scanning every row/column of `window` -- see the module-level doc
/// comment above for the full algorithm. Experimental alternative to `detect_dividers_as_grid`,
/// not yet wired into `detect_regions()`'s live pipeline.
fn detect_regions_by_growth(gray: &[u8], img_w: u32, img_h: u32, window: (i32, i32, i32, i32)) -> Vec<(i32, i32, i32, i32)> {
    let (_, _, ww, wh) = window;
    if ww < MIN_PANEL_SIZE || wh < MIN_PANEL_SIZE {
        return Vec::new();
    }
    let segments = grow_segments(gray, img_w, img_h, window);
    let merged = merge_segments(segments);
    let v_pairs = length_matched_pairs(&merged, Orientation::Vertical);
    let h_pairs = length_matched_pairs(&merged, Orientation::Horizontal);
    let mut boxes = boxes_from_pairs(gray, img_w, img_h, &v_pairs, &h_pairs);
    boxes.sort_unstable();
    boxes.dedup();
    boxes
}

// ===== Full-scan line detection (a third, deliberately naive alternative) =====
//
// The most literal version of "slice the screen one pixel-row at a time, left to right, all
// the way from top to bottom, note every place the color changes, then see which positions a
// lot of the slices agree on." Every row is tested for vertical edges (a color-change scanning
// left to right implies a vertical divider at that x) and every column for horizontal edges
// (top to bottom implies a horizontal divider at that y) -- exhaustively, no sampling. A
// position only survives if at least `FULL_SCAN_MATCH_FRACTION` of the rows/columns that
// crossed it agree there's a change there.
//
// This is deliberately undecorated -- unlike `detect_dividers_as_grid` above (density + span +
// local-peak-ratio, tuned over several rounds against real false positives -- see its own
// extensive doc comments), this keeps only the one filter the idea itself describes: count
// matches, keep what a lot of them agree on. Also deliberately distinct from the seed-growth
// detector above: that one only *samples* the image (sparse seeds + short rays); this one
// looks at every single pixel.

/// Fraction of rows/columns that must agree a change happens at the same x/y for it to count
/// as a real line -- "if a lot of them match." The only filter this detector applies.
const FULL_SCAN_MATCH_FRACTION: f64 = 0.5;

/// Scan every row and column of `window` exactly once (reusing `is_edge_pixel`'s raw-
/// intensity-step test -- see `DIVIDER_RAW_DIFF_THRESHOLD` for why raw differencing rather than
/// Canny), and tally how many rows report a color change at each x, and how many columns
/// report one at each y.
pub(crate) fn full_scan_counts(gray: &[u8], img_w: u32, img_h: u32, window: (i32, i32, i32, i32)) -> (Vec<u32>, Vec<u32>) {
    let (wx, wy, ww, wh) = window;
    let mut col_counts = vec![0u32; ww.max(0) as usize];
    let mut row_counts = vec![0u32; wh.max(0) as usize];
    for y in wy..wy + wh {
        for x in wx..wx + ww {
            if is_edge_pixel(gray, img_w, img_h, x, y, Orientation::Vertical) {
                col_counts[(x - wx) as usize] += 1;
            }
            if is_edge_pixel(gray, img_w, img_h, x, y, Orientation::Horizontal) {
                row_counts[(y - wy) as usize] += 1;
            }
        }
    }
    (col_counts, row_counts)
}

/// Keep only the x/y positions enough rows/columns agree on -- see `FULL_SCAN_MATCH_FRACTION`.
pub(crate) fn full_scan_lines(gray: &[u8], img_w: u32, img_h: u32, window: (i32, i32, i32, i32)) -> (Vec<i32>, Vec<i32>) {
    full_scan_lines_impl(gray, img_w, img_h, window, FullScanOptions::default())
}

/// Same density-threshold filter as `full_scan_lines`, plus one more check: a surviving
/// position must be a genuine local peak against its own neighborhood, not just part of a
/// broad band of similarly-dense neighbors (`is_local_peak`/`DIVIDER_PEAK_RATIO` -- the same
/// check `detect_dividers_as_grid` already uses, and for the same reason: ordinary text has
/// enough of its own internal rhythm -- consistent indentation columns, evenly-spaced line
/// baselines -- that a bare "half the rows/columns agree" threshold clears just as easily
/// over a paragraph as over a real divider, which is exactly what `full_scan_lines` alone
/// demonstrated against a live capture -- see growth_viz/fullscan_viz's comparison). Added
/// specifically to test how much of that false-positive problem this one extra check fixes
/// on its own -- turned out, against a live capture, to be "not much": see
/// `full_scan_lines_with_uniformity_check` below for the other, complementary failure mode
/// this one doesn't address.
pub(crate) fn full_scan_lines_with_peak_check(
    gray: &[u8],
    img_w: u32,
    img_h: u32,
    window: (i32, i32, i32, i32),
) -> (Vec<i32>, Vec<i32>) {
    full_scan_lines_impl(gray, img_w, img_h, window, FullScanOptions { peak_check: true, uniformity_check: false })
}

/// Same density-threshold filter as `full_scan_lines`, plus a different extra check: a
/// surviving position's edge *step size* must be reasonably consistent all along its own
/// span, not wildly variable.
///
/// This is a deliberately different signal from sampling the flatness of the underlying
/// pixels themselves (`std_dev`, used elsewhere in this file for small-control candidates) --
/// that was already tried for `detect_dividers_as_grid` and reverted (see `LineStats`'s doc
/// comment): a real full-length divider often crosses several visually distinct background
/// regions along its run (a title bar, then a body, then a different panel's background), so
/// the *underlying pixels* it passes through aren't flat end-to-end, while a sparse line of
/// text can look deceptively flat overall (mostly background, a few text pixels) -- confirmed
/// backwards on a live capture there (the real divider measured a *higher* std-dev than a
/// false-positive text row). What should still hold, even while crossing different
/// backgrounds, is that a real divider's own border contrast is close to constant wherever
/// it's crossed (it's drawn the same way throughout), whereas individual text-glyph edges (a
/// thin serif, a bold stroke, an anti-aliased curve) vary in step size much more than one
/// uniform rule does -- so this measures std-dev of the *edge magnitude*, not the pixel
/// values, sampled only at the pixels that actually registered as an edge.
pub(crate) fn full_scan_lines_with_uniformity_check(
    gray: &[u8],
    img_w: u32,
    img_h: u32,
    window: (i32, i32, i32, i32),
) -> (Vec<i32>, Vec<i32>) {
    full_scan_lines_impl(gray, img_w, img_h, window, FullScanOptions { peak_check: false, uniformity_check: true })
}

/// Std-dev of the raw diff *magnitude* is treated as "uniform enough" up to this value --
/// tuned against a live capture (see fullscan_viz's comparison output), not derived
/// analytically.
const MAX_EDGE_MAGNITUDE_STD_DEV: f64 = 35.0;

/// Std-dev of the intensity-step magnitude among the pixels that registered as an edge along
/// one candidate line (`fixed` = x for a vertical candidate, y for horizontal; `lo..hi` is
/// that line's full span) -- see `full_scan_lines_with_uniformity_check`'s doc comment for why
/// this, not `std_dev` of the underlying pixel values.
fn edge_magnitude_std_dev(gray: &[u8], img_w: u32, img_h: u32, fixed: i32, (lo, hi): (i32, i32), orientation: Orientation) -> f64 {
    let px = |x: i32, y: i32| -> i32 { gray[y as usize * img_w as usize + x as usize] as i32 };
    let mut magnitudes = Vec::new();
    for along in lo..hi {
        let (x, y) = match orientation {
            Orientation::Vertical => (fixed, along),
            Orientation::Horizontal => (along, fixed),
        };
        if !is_edge_pixel(gray, img_w, img_h, x, y, orientation) {
            continue;
        }
        let magnitude = match orientation {
            Orientation::Vertical => (px(x, y) - px(x - 1, y)).abs(),
            Orientation::Horizontal => (px(x, y) - px(x, y - 1)).abs(),
        };
        magnitudes.push(magnitude as f64);
    }
    if magnitudes.len() < 2 {
        return 0.0;
    }
    let mean = magnitudes.iter().sum::<f64>() / magnitudes.len() as f64;
    let variance = magnitudes.iter().map(|m| (m - mean).powi(2)).sum::<f64>() / magnitudes.len() as f64;
    variance.sqrt()
}

#[derive(Clone, Copy, Default)]
struct FullScanOptions {
    peak_check: bool,
    uniformity_check: bool,
}

fn full_scan_lines_impl(
    gray: &[u8],
    img_w: u32,
    img_h: u32,
    window: (i32, i32, i32, i32),
    options: FullScanOptions,
) -> (Vec<i32>, Vec<i32>) {
    let (wx, wy, ww, wh) = window;
    let (col_counts, row_counts) = full_scan_counts(gray, img_w, img_h, window);
    let col_threshold = (wh as f64 * FULL_SCAN_MATCH_FRACTION) as u32;
    let row_threshold = (ww as f64 * FULL_SCAN_MATCH_FRACTION) as u32;

    let survives = |counts: &[u32], i: usize, c: u32, threshold: u32, fixed: i32, span: (i32, i32), orientation: Orientation| -> bool {
        if c < threshold {
            return false;
        }
        if options.peak_check && !is_local_peak(counts, i, DIVIDER_PEAK_OFFSET_MIN, DIVIDER_PEAK_OFFSET_MAX, DIVIDER_PEAK_RATIO) {
            return false;
        }
        if options.uniformity_check
            && edge_magnitude_std_dev(gray, img_w, img_h, fixed, span, orientation) > MAX_EDGE_MAGNITUDE_STD_DEV
        {
            return false;
        }
        true
    };

    let mut xs: Vec<i32> = col_counts
        .iter()
        .enumerate()
        .filter(|&(i, &c)| survives(&col_counts, i, c, col_threshold, wx + i as i32, (wy, wy + wh), Orientation::Vertical))
        .map(|(i, _)| wx + i as i32)
        .collect();
    let mut ys: Vec<i32> = row_counts
        .iter()
        .enumerate()
        .filter(|&(i, &c)| survives(&row_counts, i, c, row_threshold, wy + i as i32, (wx, wx + ww), Orientation::Horizontal))
        .map(|(i, _)| wy + i as i32)
        .collect();
    merge_adjacent(&mut xs, DIVIDER_MERGE_GAP);
    merge_adjacent(&mut ys, DIVIDER_MERGE_GAP);
    (xs, ys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(x: f64, y: f64, w: f64, h: f64) -> FocusedWindow {
        FocusedWindow { title: "test".to_string(), x, y, w, h }
    }

    fn point(x: i32, y: i32) -> imageproc::point::Point<i32> {
        imageproc::point::Point::new(x, y)
    }

    #[test]
    fn merge_adjacent_collapses_a_cluster_to_one_representative() {
        let mut values = vec![100, 101, 102, 200, 400, 401];
        merge_adjacent(&mut values, DIVIDER_MERGE_GAP);
        assert_eq!(values, vec![100, 200, 400]);
    }

    #[test]
    fn detect_dividers_as_grid_keeps_a_clean_column_despite_a_spurious_row() {
        // Confirmed live: a 1920x1080 VS Code capture had two solid vertical dividers
        // (editor|chat) *and* one spurious horizontal line (content that happened to
        // cross the row threshold, the same failure mode as the y=683 false positive) --
        // and because x-splits and y-splits used to only ever get multiplied together
        // into a grid, that one bad row chopped *both* vertical panels in half, so
        // neither the editor nor the chat panel was ever emitted as a single clean
        // full-height box. Reproduced at a smaller scale: a real vertical divider at
        // x=300 plus a spurious horizontal one at y=150 (in a 600x300 window) should
        // still produce the two full-height column strips, not just four quarter cells.
        let mut img = image::GrayImage::new(600, 300);
        for y in 0..300 {
            img.put_pixel(300, y, image::Luma([255]));
        }
        for x in 0..600 {
            img.put_pixel(x, 150, image::Luma([255]));
        }
        let regions = detect_dividers_as_grid(img.as_raw(), img.width(), img.height(), (0, 0, 600, 300));
        assert!(
            regions.contains(&(0, 0, 300, 300)) && regions.contains(&(300, 0, 300, 300)),
            "expected both full-height column strips despite the spurious row, got {regions:?}"
        );
        // The full-width row strip itself must NOT appear: a "row" spanning the entire
        // width when real column dividers already exist doesn't correspond to any real
        // boundary (it would cut straight across two independent, unrelated panels) --
        // this is what actually happened live (two bogus full-width lines, y=191 and
        // y=594, cutting through unrelated code text and chat messages).
        assert!(
            !regions.contains(&(0, 0, 600, 150)) && !regions.contains(&(0, 150, 600, 150)),
            "the spurious full-width row strip should be suppressed when columns exist, got {regions:?}"
        );
    }

    #[test]
    fn detect_dividers_as_grid_splits_a_window_at_a_real_vertical_divider() {
        // A 500x200 window with a single solid vertical line at x=200 (e.g. a sidebar
        // divider) should grid into a 200px-wide panel and a 300px-wide panel -- both
        // comfortably above MIN_PANEL_SIZE.
        let mut img = image::GrayImage::new(500, 200);
        for y in 0..200 {
            img.put_pixel(200, y, image::Luma([255]));
        }
        let regions = detect_dividers_as_grid(img.as_raw(), img.width(), img.height(), (0, 0, 500, 200));
        assert!(
            regions.contains(&(0, 0, 200, 200)),
            "expected a left panel from x=0..200, got {regions:?}"
        );
        assert!(
            regions.contains(&(200, 0, 300, 200)),
            "expected a right panel from x=200..500, got {regions:?}"
        );
    }

    #[test]
    fn detect_dividers_as_grid_finds_a_divider_scoped_to_a_sub_panel() {
        // A 1600x800 window: a vertical divider at x=1200 (e.g. a sidebar), and within the
        // 400px-wide right column only, a horizontal divider at y=500 (e.g. a terminal's
        // top border under just the editor, not the sidebar). That horizontal line covers
        // 400/1600 = 25% of the *whole window's* width (below DIVIDER_COVERAGE) but
        // 400/400 = 100% of *its own column's* width (comfortably above it). Only a
        // recursive scan -- re-measuring against the column's own width -- should find it.
        // Sized generously above MIN_PANEL_SIZE so RECURSE_INSET trimming the recursed
        // scan area on the way in doesn't push a genuine sub-region below the threshold.
        let mut img = image::GrayImage::new(1600, 800);
        for y in 0..800 {
            img.put_pixel(1200, y, image::Luma([255]));
        }
        for x in 1200..1600 {
            img.put_pixel(x, 500, image::Luma([255]));
        }
        let regions = detect_dividers_as_grid(img.as_raw(), img.width(), img.height(), (0, 0, 1600, 800));
        // RECURSE_INSET means the recursed sub-region's own coordinates are shifted in by
        // a few pixels rather than exactly matching the ideal (1200, 0, 400, 500) /
        // (1200, 500, 400, 300) -- check for something close, not an exact match.
        let close = |(x, y, w, h): (i32, i32, i32, i32), (ex, ey, ew, eh): (i32, i32, i32, i32)| {
            (x - ex).abs() <= 10 && (y - ey).abs() <= 10 && (w - ew).abs() <= 20 && (h - eh).abs() <= 20
        };
        assert!(
            regions.iter().any(|&r| close(r, (1200, 0, 400, 500)))
                && regions.iter().any(|&r| close(r, (1200, 500, 400, 300))),
            "expected the right column to be further split at y=500, got {regions:?}"
        );
    }

    #[test]
    fn detect_dividers_as_grid_finds_a_divider_interrupted_in_the_middle() {
        // A vertical line at x=200 in a 400x1000 window that's genuinely edge-to-edge (runs
        // from y=0 almost to y=1000) but has a big gap in the middle (y=300..700, as if a
        // chat bubble or icon crossed it there) -- overall density is only ~60%, well under
        // DIVIDER_COVERAGE, but its span covers ~100% of the window height. This is exactly
        // the shape of divider DIVIDER_COVERAGE alone was missing in practice.
        let mut img = image::GrayImage::new(400, 1000);
        for y in 0..300 {
            img.put_pixel(200, y, image::Luma([255]));
        }
        for y in 700..1000 {
            img.put_pixel(200, y, image::Luma([255]));
        }
        let regions = detect_dividers_as_grid(img.as_raw(), img.width(), img.height(), (0, 0, 400, 1000));
        assert!(
            regions.contains(&(0, 0, 200, 1000)) && regions.contains(&(200, 0, 200, 1000)),
            "expected the interrupted-but-edge-to-edge line to still split the window, got {regions:?}"
        );
    }

    #[test]
    fn detect_dividers_as_grid_ignores_a_short_line_near_one_edge() {
        // A vertical line at x=200 that only covers the top 30% of the window (y=0..300 of
        // 1000) -- even though it touches the top edge, it doesn't reach anywhere near the
        // bottom, so neither density nor span should qualify it as a real divider.
        let mut img = image::GrayImage::new(400, 1000);
        for y in 0..300 {
            img.put_pixel(200, y, image::Luma([255]));
        }
        assert_eq!(detect_dividers_as_grid(img.as_raw(), img.width(), img.height(), (0, 0, 400, 1000)), Vec::new());
    }

    #[test]
    fn detect_dividers_as_grid_returns_nothing_when_no_divider_found() {
        // An ordinary single-pane window with no internal divider shouldn't emit its own
        // full bounds as a degenerate "panel" -- every simple window would otherwise always
        // produce one.
        let img = image::GrayImage::new(300, 200);
        assert_eq!(detect_dividers_as_grid(img.as_raw(), img.width(), img.height(), (0, 0, 300, 200)), Vec::new());
    }

    #[test]
    fn detect_dividers_as_grid_ignores_a_short_incidental_line() {
        // A horizontal rule spanning only ~20% of the window's width shouldn't register as
        // a real divider (see DIVIDER_COVERAGE) or produce any panel-sized grid cells.
        let mut img = image::GrayImage::new(300, 200);
        for x in 50..110 {
            img.put_pixel(x, 100, image::Luma([255]));
        }
        assert_eq!(detect_dividers_as_grid(img.as_raw(), img.width(), img.height(), (0, 0, 300, 200)), Vec::new());
    }

    #[test]
    fn detect_dividers_as_grid_skips_windows_smaller_than_a_panel() {
        let img = image::GrayImage::new(50, 50);
        assert_eq!(detect_dividers_as_grid(img.as_raw(), img.width(), img.height(), (0, 0, 50, 50)), Vec::new());
    }

    #[test]
    fn rectangularity_is_high_for_a_clean_rectangle() {
        // A traced axis-aligned rectangle outline should fill (almost) its whole bbox.
        let points =
            vec![point(0, 0), point(10, 0), point(10, 10), point(0, 10)];
        assert!(rectangularity(&points, (0, 0, 10, 10)) > MIN_RECTANGULARITY);
    }

    #[test]
    fn rectangularity_is_high_for_a_rounded_rectangle() {
        // Approximate a rounded rect by cutting a small corner off a square -- should still
        // fill nearly all of its bounding box, unlike an irregular/organic shape.
        let points = vec![
            point(2, 0),
            point(10, 0),
            point(10, 10),
            point(0, 10),
            point(0, 2),
        ];
        assert!(rectangularity(&points, (0, 0, 10, 10)) > MIN_RECTANGULARITY);
    }

    #[test]
    fn rectangularity_is_low_for_a_thin_diagonal_shape() {
        // A thin diagonal sliver (the kind of edge a photo or text cluster produces) fills
        // only a small fraction of its own bounding box.
        let points = vec![point(0, 0), point(10, 10), point(9, 10), point(0, 1)];
        assert!(rectangularity(&points, (0, 0, 10, 10)) < MIN_RECTANGULARITY);
    }

    #[test]
    fn filter_to_windows_keeps_regions_inside_a_window() {
        let regions = vec![VisionRegion { x: 100.0, y: 100.0, w: 20.0, h: 20.0 }];
        let windows = vec![window(0.0, 0.0, 200.0, 200.0)];
        assert_eq!(filter_to_windows(regions, &windows).len(), 1);
    }

    #[test]
    fn filter_to_windows_drops_regions_on_empty_desktop() {
        // No windows at all -- e.g. a monitor showing only wallpaper -- should drop every
        // vision region, however plausible-looking, since there's nothing there to click.
        let regions = vec![VisionRegion { x: 500.0, y: 300.0, w: 40.0, h: 30.0 }];
        assert_eq!(filter_to_windows(regions, &[]).len(), 0);
    }

    #[test]
    fn filter_to_windows_drops_regions_outside_every_window() {
        let regions = vec![VisionRegion { x: 900.0, y: 900.0, w: 20.0, h: 20.0 }];
        let windows = vec![window(0.0, 0.0, 200.0, 200.0)];
        assert_eq!(filter_to_windows(regions, &windows).len(), 0);
    }

    #[test]
    fn iou_is_high_for_near_duplicate_boxes() {
        // A widget's outer border and an inner highlight line a couple pixels in --
        // should dedup (see DEDUP_IOU).
        let outer = (10, 10, 40, 24);
        let inner = (12, 12, 36, 20);
        assert!(iou(outer, inner) > DEDUP_IOU, "near-duplicate boxes should have high IoU");
    }

    #[test]
    fn iou_is_low_for_a_small_button_inside_a_large_panel() {
        // A real, distinct button sitting inside a much bigger panel/toolbar should *not*
        // look like a duplicate of the panel, even though it's fully contained in it --
        // this is the case the old containment-ratio dedup got wrong.
        let panel = (0, 0, 400, 300);
        let button = (20, 20, 30, 20);
        assert!(
            iou(panel, button) < DEDUP_IOU,
            "a small nested button shouldn't be deduped away by its container"
        );
    }

    #[test]
    fn std_dev_is_near_zero_for_a_flat_region() {
        let w = 10u32;
        let gray = vec![128u8; (w * 10) as usize];
        assert!(std_dev(&gray, w, (0, 0, w as i32, 10)) < 1.0);
    }

    #[test]
    fn std_dev_is_high_for_a_noisy_region() {
        let w = 10u32;
        let gray: Vec<u8> = (0..w * 10).map(|i| if i % 2 == 0 { 0 } else { 255 }).collect();
        assert!(std_dev(&gray, w, (0, 0, w as i32, 10)) > MAX_STD_DEV);
    }

    fn draw_rect_outline(img: &mut image::GrayImage, x: i32, y: i32, w: i32, h: i32, value: u8) {
        for dx in 0..w {
            img.put_pixel((x + dx) as u32, y as u32, image::Luma([value]));
            img.put_pixel((x + dx) as u32, (y + h - 1) as u32, image::Luma([value]));
        }
        for dy in 0..h {
            img.put_pixel(x as u32, (y + dy) as u32, image::Luma([value]));
            img.put_pixel((x + w - 1) as u32, (y + dy) as u32, image::Luma([value]));
        }
    }

    fn close_box((x, y, w, h): (i32, i32, i32, i32), (ex, ey, ew, eh): (i32, i32, i32, i32)) -> bool {
        (x - ex).abs() <= 6 && (y - ey).abs() <= 6 && (w - ew).abs() <= 12 && (h - eh).abs() <= 12
    }

    #[test]
    fn lengths_match_within_tolerance() {
        assert!(lengths_match(240, 238));
        assert!(lengths_match(100, 108));
    }

    #[test]
    fn lengths_match_rejects_dissimilar_lengths() {
        assert!(!lengths_match(240, 180));
        assert!(!lengths_match(0, 50));
    }

    #[test]
    fn merge_segments_unions_overlapping_same_line_detections() {
        // Two seeds independently discovering slightly different, overlapping stretches of
        // the same real vertical divider at x=100/x=101 (a 1px line commonly registers on
        // both sides of its own step -- see `is_edge_pixel`'s doc comment) should collapse
        // into one segment spanning their union.
        let segments = vec![
            Segment { orientation: Orientation::Vertical, fixed: 100, start: 10, end: 150 },
            Segment { orientation: Orientation::Vertical, fixed: 101, start: 120, end: 300 },
        ];
        let merged = merge_segments(segments);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start, 10);
        assert_eq!(merged[0].end, 300);
    }

    #[test]
    fn detect_regions_by_growth_finds_a_clean_rectangle_outline() {
        let mut img = image::GrayImage::new(400, 400);
        draw_rect_outline(&mut img, 80, 80, 240, 240, 255);
        let boxes = detect_regions_by_growth(img.as_raw(), img.width(), img.height(), (0, 0, 400, 400));
        assert!(
            boxes.iter().any(|&b| close_box(b, (80, 80, 240, 240))),
            "expected to find the drawn rectangle, got {boxes:?}"
        );
    }

    #[test]
    fn detect_regions_by_growth_ignores_lines_with_no_length_partner() {
        // A vertical line and a horizontal line of two totally different, non-matching
        // lengths -- neither has a same-orientation partner to pair with, and even if they
        // did, their span/position don't line up as one rectangle's 4 sides. No box should
        // be reported.
        let mut img = image::GrayImage::new(400, 400);
        for y in 50..90 {
            img.put_pixel(120, y, image::Luma([255]));
        }
        for x in 200..340 {
            img.put_pixel(x, 250, image::Luma([255]));
        }
        let boxes = detect_regions_by_growth(img.as_raw(), img.width(), img.height(), (0, 0, 400, 400));
        assert!(boxes.is_empty(), "expected no boxes from two unrelated, unmatched lines, got {boxes:?}");
    }

    #[test]
    fn full_scan_lines_finds_a_solid_vertical_divider() {
        let mut img = image::GrayImage::new(400, 300);
        for y in 0..300 {
            img.put_pixel(200, y, image::Luma([255]));
        }
        let (xs, ys) = full_scan_lines(img.as_raw(), img.width(), img.height(), (0, 0, 400, 300));
        assert!(xs.contains(&200) || xs.iter().any(|&x| (x - 200).abs() <= DIVIDER_MERGE_GAP), "expected x~200, got {xs:?}");
        assert!(ys.is_empty(), "a clean vertical line shouldn't also register as a horizontal one, got {ys:?}");
    }

    #[test]
    fn full_scan_lines_finds_a_solid_horizontal_divider() {
        let mut img = image::GrayImage::new(300, 400);
        for x in 0..300 {
            img.put_pixel(x, 150, image::Luma([255]));
        }
        let (xs, ys) = full_scan_lines(img.as_raw(), img.width(), img.height(), (0, 0, 300, 400));
        assert!(xs.is_empty(), "a clean horizontal line shouldn't also register as a vertical one, got {xs:?}");
        assert!(ys.contains(&150) || ys.iter().any(|&y| (y - 150).abs() <= DIVIDER_MERGE_GAP), "expected y~150, got {ys:?}");
    }

    #[test]
    fn full_scan_lines_ignores_a_short_incidental_line() {
        // A vertical line covering only the top 20% of the window -- far short of the 50%
        // agreement `FULL_SCAN_MATCH_FRACTION` requires -- shouldn't register at all.
        let mut img = image::GrayImage::new(400, 300);
        for y in 0..60 {
            img.put_pixel(200, y, image::Luma([255]));
        }
        let (xs, ys) = full_scan_lines(img.as_raw(), img.width(), img.height(), (0, 0, 400, 300));
        assert!(xs.is_empty() && ys.is_empty(), "expected no lines from a short 20%-coverage line, got xs={xs:?} ys={ys:?}");
    }

    #[test]
    fn full_scan_lines_returns_nothing_on_a_blank_image() {
        let img = image::GrayImage::new(300, 200);
        let (xs, ys) = full_scan_lines(img.as_raw(), img.width(), img.height(), (0, 0, 300, 200));
        assert!(xs.is_empty() && ys.is_empty());
    }

    #[test]
    fn full_scan_lines_with_peak_check_rejects_a_broad_band_but_keeps_a_real_spike() {
        let mut img = image::GrayImage::new(400, 300);
        // A wide "broad band" of alternating columns (x=60..180, 120px -- comfortably wider
        // than DIVIDER_PEAK_OFFSET_MAX*2) -- every column in it registers a vertical edge on
        // every row, mimicking ordinary text's fairly uniform column-to-column structure
        // rather than one real divider.
        for y in 0..300 {
            for x in 60..180 {
                if x % 2 == 0 {
                    img.put_pixel(x, y, image::Luma([255]));
                }
            }
        }
        // One genuinely isolated solid vertical line, far from anything else.
        for y in 0..300 {
            img.put_pixel(300, y, image::Luma([255]));
        }

        let (naive_xs, _) = full_scan_lines(img.as_raw(), img.width(), img.height(), (0, 0, 400, 300));
        let (peaked_xs, _) = full_scan_lines_with_peak_check(img.as_raw(), img.width(), img.height(), (0, 0, 400, 300));

        assert!(
            naive_xs.contains(&120),
            "the plain density threshold should treat an interior band column as a divider too, got {naive_xs:?}"
        );
        assert!(
            !peaked_xs.contains(&120),
            "the peak-ratio check should reject an interior band column surrounded by similarly-dense neighbors, got {peaked_xs:?}"
        );
        assert!(
            peaked_xs.iter().any(|&x| (x - 300).abs() <= DIVIDER_MERGE_GAP),
            "the isolated real divider should still survive the peak check, got {peaked_xs:?}"
        );
    }

    #[test]
    fn full_scan_lines_with_uniformity_check_rejects_a_variable_magnitude_line_but_keeps_a_uniform_one() {
        let mut img = image::GrayImage::new(300, 300);
        // A "real divider": the same intensity step every row (background 0, line always
        // 200) -- perfectly uniform edge magnitude.
        for y in 0..300 {
            img.put_pixel(100, y, image::Luma([200]));
        }
        // A "text-like" line: same density (an edge every row, so the same as a real
        // divider under `full_scan_lines` alone) but the step size alternates wildly row to
        // row, simulating different glyph strokes crossing this column.
        for y in 0..300 {
            let v: u8 = if y % 2 == 0 { 210 } else { 10 };
            img.put_pixel(200, y, image::Luma([v]));
        }

        let (naive_xs, _) = full_scan_lines(img.as_raw(), img.width(), img.height(), (0, 0, 300, 300));
        let (uniform_xs, _) = full_scan_lines_with_uniformity_check(img.as_raw(), img.width(), img.height(), (0, 0, 300, 300));

        assert!(
            naive_xs.contains(&100) && naive_xs.contains(&200),
            "the plain density threshold should treat both equally-dense lines as dividers, got {naive_xs:?}"
        );
        assert!(
            uniform_xs.iter().any(|&x| (x - 100).abs() <= DIVIDER_MERGE_GAP),
            "the uniform-magnitude line should survive the uniformity check, got {uniform_xs:?}"
        );
        assert!(
            !uniform_xs.iter().any(|&x| (x - 200).abs() <= DIVIDER_MERGE_GAP),
            "the wildly-variable-magnitude line should be rejected by the uniformity check, got {uniform_xs:?}"
        );
    }
}
