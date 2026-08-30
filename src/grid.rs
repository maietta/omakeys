//! Screen-position math for the keyboard-shaped hint grid.
//!
//! Each cell is addressed by a two-key code: a **coarse** key typed with the left hand
//! picks a broad region of the screen, then a **fine** key typed with the right hand picks
//! an exact spot within that region. Both keys use their own physical position on the
//! keyboard (row = top/home/bottom, column = pinky..index) to decide *where on screen* they
//! point, so the mapping feels spatial: e.g. `a` (home row, left pinky) sits towards the
//! vertical middle of the left side of the screen, while `p` (top row, right pinky) sits
//! towards the upper-right.

/// Ordered row-major layout of the left hand's keys: row 0 = top, row 1 = home, row 2 = bottom.
/// Column 0 = pinky (outermost/leftmost), column 4 = index-stretch (innermost).
pub const COARSE_KEYS: [[char; 5]; 3] = [
    ['q', 'w', 'e', 'r', 't'],
    ['a', 's', 'd', 'f', 'g'],
    ['z', 'x', 'c', 'v', 'b'],
];

/// Ordered row-major layout of the right hand's keys. Column 0 = index-stretch (innermost,
/// closest to the keyboard's center), column 4 = pinky (outermost/rightmost).
pub const FINE_KEYS: [[char; 5]; 3] = [
    ['y', 'u', 'i', 'o', 'p'],
    ['h', 'j', 'k', 'l', ';'],
    ['n', 'm', ',', '.', '/'],
];

pub const COLS: usize = 5;
pub const ROWS: usize = 3;

/// One selectable cell in the grid overlay, addressed by a two-key label like "aj" or "gp".
#[derive(Debug, Clone)]
pub struct Cell {
    pub label: [char; 2],
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Cell {
    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// Look up the (row, col) position of a key within a layout table, if it's present there.
fn find_in_layout(layout: &[[char; 5]; 3], key: char) -> Option<(usize, usize)> {
    for (row, keys) in layout.iter().enumerate() {
        if let Some(col) = keys.iter().position(|&k| k == key) {
            return Some((row, col));
        }
    }
    None
}

pub fn is_coarse_key(key: char) -> bool {
    find_in_layout(&COARSE_KEYS, key).is_some()
}

pub fn is_fine_key(key: char) -> bool {
    find_in_layout(&FINE_KEYS, key).is_some()
}

/// All 30 keys used for both the grid and hint-mode labels (`COARSE_KEYS` + `FINE_KEYS`,
/// flattened in row-major order), so hint labels reuse the same physical keys and muscle
/// memory as the grid rather than a separate alphabet.
pub fn hint_alphabet() -> Vec<char> {
    let mut chars: Vec<char> = COARSE_KEYS.iter().flatten().copied().collect();
    chars.extend(FINE_KEYS.iter().flatten().copied());
    chars
}

pub fn is_hint_key(key: char) -> bool {
    is_coarse_key(key) || is_fine_key(key)
}

/// Build every grid cell over a `screen_w` x `screen_h` area. The coarse key divides the
/// screen into a 5x3 region grid; the fine key subdivides the chosen region into a further
/// 5x3 grid, giving 225 addressable points in total.
pub fn build_grid(screen_w: f64, screen_h: f64) -> Vec<Cell> {
    let region_w = screen_w / COLS as f64;
    let region_h = screen_h / ROWS as f64;
    let sub_w = region_w / COLS as f64;
    let sub_h = region_h / ROWS as f64;

    let mut cells = Vec::with_capacity(COLS * ROWS * COLS * ROWS);
    for (coarse_row, coarse_keys) in COARSE_KEYS.iter().enumerate() {
        for (coarse_col, &coarse_key) in coarse_keys.iter().enumerate() {
            let region_x = coarse_col as f64 * region_w;
            let region_y = coarse_row as f64 * region_h;
            for (fine_row, fine_keys) in FINE_KEYS.iter().enumerate() {
                for (fine_col, &fine_key) in fine_keys.iter().enumerate() {
                    cells.push(Cell {
                        label: [coarse_key, fine_key],
                        x: region_x + fine_col as f64 * sub_w,
                        y: region_y + fine_row as f64 * sub_h,
                        w: sub_w,
                        h: sub_h,
                    });
                }
            }
        }
    }
    cells
}

/// Find the cell matching an exact two-key label.
pub fn find_cell(cells: &[Cell], coarse: char, fine: char) -> Option<&Cell> {
    cells.iter().find(|c| c.label == [coarse, fine])
}

/// All cells whose coarse key matches (used to highlight the chosen region after the first
/// keystroke, before the fine key narrows it down further).
pub fn cells_in_region<'a>(cells: &'a [Cell], coarse: char) -> Vec<&'a Cell> {
    cells.iter().filter(|c| c.label[0] == coarse).collect()
}

/// The center of a whole coarse region (the bounding box of all 15 sub-cells within it) --
/// used to start nudging immediately from a coarse pick alone, without requiring a second,
/// separate fine-key press first (h/j/k/l are reserved for movement, not fine-picking -- see
/// `overlay.rs`'s `Mode::TypingFine` handler).
pub fn coarse_region_center(cells: &[Cell], coarse: char) -> Option<(f64, f64)> {
    let region = cells_in_region(cells, coarse);
    if region.is_empty() {
        return None;
    }
    let min_x = region.iter().map(|c| c.x).fold(f64::MAX, f64::min);
    let min_y = region.iter().map(|c| c.y).fold(f64::MAX, f64::min);
    let max_x = region.iter().map(|c| c.x + c.w).fold(f64::MIN, f64::max);
    let max_y = region.iter().map(|c| c.y + c.h).fold(f64::MIN, f64::max);
    Some(((min_x + max_x) / 2.0, (min_y + max_y) / 2.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_is_middle_left() {
        let cells = build_grid(1920.0, 1080.0);
        // Any cell labelled a* sits in the leftmost column, vertically-middle row.
        let region = cells_in_region(&cells, 'a');
        assert!(region.iter().all(|c| c.x < 1920.0 / 5.0));
        assert!(region.iter().all(|c| c.y >= 1080.0 / 3.0 && c.y < 2.0 * 1080.0 / 3.0));
    }

    #[test]
    fn p_is_upper_right_within_its_region() {
        let cells = build_grid(1920.0, 1080.0);
        let cell = find_cell(&cells, 'a', 'p').unwrap();
        // 'p' is the fine key's top-right corner, so within region "a" it should be the
        // rightmost, topmost sub-cell.
        let region = cells_in_region(&cells, 'a');
        let max_x = region.iter().map(|c| c.x).fold(f64::MIN, f64::max);
        let min_y = region.iter().map(|c| c.y).fold(f64::MAX, f64::min);
        assert_eq!(cell.x, max_x);
        assert_eq!(cell.y, min_y);
    }

    #[test]
    fn grid_has_225_cells_covering_full_area() {
        let cells = build_grid(1920.0, 1080.0);
        assert_eq!(cells.len(), 225);
        let max_x = cells.iter().map(|c| c.x + c.w).fold(f64::MIN, f64::max);
        let max_y = cells.iter().map(|c| c.y + c.h).fold(f64::MIN, f64::max);
        assert!((max_x - 1920.0).abs() < 1e-6);
        assert!((max_y - 1080.0).abs() < 1e-6);
    }
}

