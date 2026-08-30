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
            log::error!("omg-keys: captured buffer size didn't match its own width/height");
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
            "omg-keys: divider debug window=({wx},{wy},{ww},{wh}) col_threshold={col_threshold} \
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

    log::debug!("omg-keys: merged xs={xs:?} ys={ys:?} for window=({wx},{wy},{ww},{wh})");

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
}
