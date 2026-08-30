//! Assigns short, typeable key-sequence labels to hint targets, Vimium-link-hint style, so
//! the overlay can let the user pick one by typing its label. Targets come from two sources
//! merged together: precise AT-SPI elements where the toolkit supports it, and coarser
//! vision-detected regions (see vision.rs) filling the gap where it doesn't.

use crate::atspi_scan::{Category, Element};
use crate::grid;
use crate::vision::VisionRegion;

/// Where a hint target's geometry/identity came from.
pub enum Source {
    Atspi(Element),
    /// A candidate region found by edge/contour detection, not AT-SPI -- we know
    /// *something* is here, not what it is (see vision.rs).
    Vision(VisionRegion),
}

/// A hint target plus the key sequence that selects it.
pub struct HintTarget {
    pub source: Source,
    pub label: String,
}

impl HintTarget {
    pub fn geometry(&self) -> (f64, f64, f64, f64) {
        match &self.source {
            Source::Atspi(e) => (e.x, e.y, e.w, e.h),
            Source::Vision(v) => (v.x, v.y, v.w, v.h),
        }
    }

    pub fn center(&self) -> (f64, f64) {
        let (x, y, w, h) = self.geometry();
        (x + w / 2.0, y + h / 2.0)
    }

    pub fn category(&self) -> Category {
        match &self.source {
            Source::Atspi(e) => e.category,
            Source::Vision(_) => Category::Other,
        }
    }
}

/// Intersection area divided by the *smaller* box's area, so a small AT-SPI element fully
/// inside a coarser vision box (or vice versa) still counts as "the same thing" even though
/// a plain IoU wouldn't rate it highly.
fn overlap_ratio((ax, ay, aw, ah): (f64, f64, f64, f64), (bx, by, bw, bh): (f64, f64, f64, f64)) -> f64 {
    let ix = (ax.max(bx), (ax + aw).min(bx + bw));
    let iy = (ay.max(by), (ay + ah).min(by + bh));
    let iw = (ix.1 - ix.0).max(0.0);
    let ih = (iy.1 - iy.0).max(0.0);
    let intersection = iw * ih;
    let smaller = (aw * ah).min(bw * bh);
    if smaller <= 0.0 {
        0.0
    } else {
        intersection / smaller
    }
}

/// Merge AT-SPI elements with vision-detected regions -- dropping vision regions that
/// substantially overlap an AT-SPI element, since AT-SPI's is more precise and carries a
/// real name/category -- then assign a label to each, reusing the grid's 30-key alphabet
/// (see `grid::hint_alphabet`). Single-character labels are used when there are few enough
/// targets to fit one alphabet's worth (the common case); if there are more, every label
/// uniformly becomes two characters instead of mixing lengths, so no label is ever a prefix
/// of another and matching a keystroke against them is unambiguous.
pub fn assign_labels(atspi_elements: Vec<Element>, vision_regions: Vec<VisionRegion>) -> Vec<HintTarget> {
    let vision_regions: Vec<VisionRegion> = vision_regions
        .into_iter()
        .filter(|v| {
            let v_geom = (v.x, v.y, v.w, v.h);
            !atspi_elements
                .iter()
                .any(|e| overlap_ratio((e.x, e.y, e.w, e.h), v_geom) > 0.5)
        })
        .collect();

    let mut sources: Vec<Source> = atspi_elements.into_iter().map(Source::Atspi).collect();
    sources.extend(vision_regions.into_iter().map(Source::Vision));

    let alphabet = grid::hint_alphabet();
    let max_single = alphabet.len();
    let max_double = alphabet.len() * alphabet.len();
    sources.truncate(max_double);
    let two_chars = sources.len() > max_single;

    sources
        .into_iter()
        .enumerate()
        .map(|(i, source)| {
            let label = if two_chars {
                format!("{}{}", alphabet[i / alphabet.len()], alphabet[i % alphabet.len()])
            } else {
                alphabet[i].to_string()
            };
            HintTarget { source, label }
        })
        .collect()
}
