//! Turning measured rows back into caret offsets.
//!
//! A soft-wrapped line is one logical line and several visual rows, and only
//! the shaped text knows where the wraps fell. Everything here is the
//! arithmetic on top of that: which offset is one row up, which end of a
//! wrapped row `⌘←` means, where the caret is drawn, and what a click hit.
//!
//! It reads the layout through [`RowGeometry`] rather than GPUI's `TextLayout`
//! so the arithmetic can be tested against a fake with known wraps. That is
//! worth a trait for one implementor: every caret bug this app has had lived in
//! these few lines, and none of them needed a real font to reproduce.

use std::collections::HashMap;

use gpui::{point, px, Bounds, Pixels, Point};

use crate::index::{self, LineIndex};

/// The measured geometry of one logical line, across however many visual rows
/// it wrapped into. The part of GPUI's `TextLayout` this module needs.
///
/// Offsets are *local* — byte offsets into the line, not the buffer.
pub trait RowGeometry {
    fn bounds(&self) -> Bounds<Pixels>;
    fn line_height(&self) -> Pixels;
    /// Top-left of the glyph at `local`. A wrap boundary reports the end of the
    /// earlier row, never the start of the later one — see [`Affinity`].
    fn position_for_index(&self, local: usize) -> Option<Point<Pixels>>;
    /// The offset a point hit, or the closest one if it missed.
    fn index_for_position(&self, at: Point<Pixels>) -> Result<usize, usize>;
}

/// GPUI's measured text, read the way the arithmetic wants it. The whole
/// adapter — everything below is written against the trait, so it can be tested
/// against a fake with known wraps rather than against real shaping.
impl RowGeometry for gpui::TextLayout {
    fn bounds(&self) -> Bounds<Pixels> {
        gpui::TextLayout::bounds(self)
    }
    fn line_height(&self) -> Pixels {
        gpui::TextLayout::line_height(self)
    }
    fn position_for_index(&self, local: usize) -> Option<Point<Pixels>> {
        gpui::TextLayout::position_for_index(self, local)
    }
    fn index_for_position(&self, at: Point<Pixels>) -> Result<usize, usize> {
        gpui::TextLayout::index_for_position(self, at)
    }
}

/// One visible line: its byte range in the buffer, and where it landed.
pub struct Row<G> {
    pub byte_start: usize,
    pub byte_end: usize,
    pub geometry: G,
}

/// Which side of a soft wrap the caret is on.
///
/// A wrap offset names two places on screen at once: the end of one visual row
/// and the start of the next. They are not interchangeable — walking down into
/// a wrapped row has to land at its start, not back at the end of the row
/// above. The layout resolves such an offset to the earlier row, so the later
/// one is only reachable by carrying this alongside it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Affinity {
    /// The end of the row that wraps — the default, and the only possibility
    /// for an offset that is not a wrap boundary.
    RowEnd,
    /// The start of the row that follows the wrap.
    RowStart,
}

/// Where the caret is. An offset alone does not say, at a wrap.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Caret {
    pub offset: usize,
    pub affinity: Affinity,
    /// The x it is aiming for while stepping through rows, so that walking down
    /// past a short row and back up returns to the column it left. Set only by
    /// row-wise motion.
    pub goal_x: Option<Pixels>,
}

impl Caret {
    /// A caret that is not mid-journey: no goal column, no wrap to be on the
    /// far side of.
    pub fn at(offset: usize) -> Self {
        Self {
            offset,
            affinity: Affinity::RowEnd,
            goal_x: None,
        }
    }
}

/// Byte offset of a point inside a laid-out row, clamped to that row.
fn offset_at<G: RowGeometry>(row: &Row<G>, at: Point<Pixels>) -> usize {
    let local = match row.geometry.index_for_position(at) {
        Ok(index) => index,
        Err(closest) => closest,
    };
    row.byte_start + local.min(row.byte_end - row.byte_start)
}

/// Whether a soft wrap falls exactly at `offset`, so a caret there could sit on
/// either row.
///
/// The layout reports the earlier row for such an offset, so a wrap shows up as
/// the *next* grapheme being a row lower. `next` is that grapheme's offset;
/// callers that already know it pass it in rather than re-deriving it.
fn wraps_at<G: RowGeometry>(row: &Row<G>, offset: usize, next: usize) -> bool {
    if offset <= row.byte_start || offset >= row.byte_end {
        return false;
    }
    let (Some(here), Some(after)) = (
        row.geometry.position_for_index(offset - row.byte_start),
        row.geometry.position_for_index(next - row.byte_start),
    ) else {
        return false;
    };
    after.y > here.y
}

/// Whether a soft wrap falls exactly at `offset` — the public form of the
/// internal test, for a keyboard move deciding its own affinity.
///
/// A horizontal move (←/→, word motion) that lands on a wrap boundary belongs
/// at the head of the wrapped row, the same place a click there would: the
/// offset names both the tail of the row above and the start of the row below,
/// and drawing it at the tail makes the move look like it did nothing. Callers
/// pass the next grapheme's offset, which they already have.
pub fn is_wrap_boundary<G: RowGeometry>(row: &Row<G>, offset: usize, next: usize) -> bool {
    wraps_at(row, offset, next)
}

/// Where the caret is drawn, and the height of the row it is drawn in.
///
/// The one place that knows a caret on the near side of a wrap belongs at the
/// left margin of the row below. Both the motion arithmetic and the painter go
/// through here, so they cannot drift apart.
pub fn caret_origin<G: RowGeometry>(
    row: &Row<G>,
    offset: usize,
    next_grapheme: usize,
    affinity: Affinity,
) -> Option<(Point<Pixels>, Pixels)> {
    let at = row.geometry.position_for_index(offset.checked_sub(row.byte_start)?)?;
    let line_height = row.geometry.line_height();
    if affinity == Affinity::RowStart && wraps_at(row, offset, next_grapheme) {
        // The head of the row below the wrap: hard against the left margin.
        return Some((
            point(row.geometry.bounds().left(), at.y + line_height),
            line_height,
        ));
    }
    Some((at, line_height))
}

/// The side of the wrap an offset arrived on, given the y it was aimed at.
pub fn affinity_at<G: RowGeometry>(
    text: &str,
    index: &LineIndex,
    row: &Row<G>,
    offset: usize,
    aimed_y: Pixels,
) -> Affinity {
    let next = index::next_grapheme(text, index, offset);
    if !wraps_at(row, offset, next) {
        return Affinity::RowEnd;
    }
    let Some(at) = row.geometry.position_for_index(offset - row.byte_start) else {
        return Affinity::RowEnd;
    };
    // Aimed below the row the layout reports: the caret belongs to the row that
    // starts here.
    if aimed_y >= at.y + row.geometry.line_height() {
        Affinity::RowStart
    } else {
        Affinity::RowEnd
    }
}

/// The caret one *visual* row above or below.
///
/// Off-screen lines have no layout, so there it steps logically — the same
/// thing for any line that did not wrap.
pub fn step<G: RowGeometry>(
    text: &str,
    index: &LineIndex,
    rows: &HashMap<usize, Row<G>>,
    caret: Caret,
    down: bool,
) -> Caret {
    let logical = Caret::at(if down {
        logical_below(text, index, caret.offset)
    } else {
        logical_above(text, index, caret.offset)
    });

    let line = index.line_at(caret.offset);
    let Some(row) = rows.get(&line) else {
        return logical;
    };
    let next = index::next_grapheme(text, index, caret.offset);
    let Some((origin, line_height)) = caret_origin(row, caret.offset, next, caret.affinity) else {
        return logical;
    };

    let goal_x = caret.goal_x.unwrap_or(origin.x);
    let bounds = row.geometry.bounds();
    // `caret_origin` reports the top edge of the caret's row, so the row one
    // step away is centred half a row beyond it.
    let target_y = origin.y + line_height * if down { 1.5 } else { -0.5 };

    if target_y >= bounds.top() && target_y < bounds.bottom() {
        // Another row of the same wrapped line.
        let landed = offset_at(row, point(goal_x, target_y));
        return Caret {
            offset: landed,
            affinity: affinity_at(text, index, row, landed, target_y),
            goal_x: Some(goal_x),
        };
    }

    // Off the end of this line: enter the next one at its nearest row.
    let neighbour = if down {
        (line + 1 < index.line_count()).then(|| line + 1)
    } else {
        line.checked_sub(1)
    };
    let Some(neighbour) = neighbour else {
        // The first row of the document, or the last: run to the edge, the way
        // every Mac text view does.
        return Caret::at(if down { text.len() } else { 0 });
    };
    let Some(next_row) = rows.get(&neighbour) else {
        return logical;
    };
    let next_bounds = next_row.geometry.bounds();
    let edge = if down {
        next_bounds.top() + px(1.)
    } else {
        next_bounds.bottom() - px(1.)
    };
    let landed = offset_at(next_row, point(goal_x, edge));
    Caret {
        offset: landed,
        affinity: affinity_at(text, index, next_row, landed, edge),
        goal_x: Some(goal_x),
    }
}

/// The start or end of the caret's *visual* row — for the same reason ↑/↓ step
/// by row: on a wrapped line the logical ends are somewhere off-screen.
pub fn row_edge<G: RowGeometry>(
    text: &str,
    index: &LineIndex,
    rows: &HashMap<usize, Row<G>>,
    caret: Caret,
    to_end: bool,
) -> Caret {
    let line = index.line_at(caret.offset);
    let logical = Caret::at(if to_end {
        index.line_end(line)
    } else {
        index.line_start(line)
    });

    let Some(row) = rows.get(&line) else {
        return logical;
    };
    let next = index::next_grapheme(text, index, caret.offset);
    let Some((origin, line_height)) = caret_origin(row, caret.offset, next, caret.affinity) else {
        return logical;
    };
    let bounds = row.geometry.bounds();
    let middle = origin.y + line_height * 0.5;
    let x = if to_end { bounds.right() } else { bounds.left() };
    let landed = offset_at(row, point(x, middle));
    Caret {
        offset: landed,
        affinity: affinity_at(text, index, row, landed, middle),
        goal_x: None,
    }
}

/// The offset a point in the window hit.
///
/// Only visible rows are laid out — that is what virtualization buys — so a
/// point above or below every one of them clamps to the nearest visible edge
/// rather than jumping to the far end of a 200,000-line document.
pub fn offset_at_point<G: RowGeometry>(
    len: usize,
    rows: &HashMap<usize, Row<G>>,
    at: Point<Pixels>,
) -> usize {
    let mut nearest: Option<(Pixels, &Row<G>)> = None;

    for row in rows.values() {
        let bounds = row.geometry.bounds();
        if at.y >= bounds.top() && at.y < bounds.bottom() {
            return offset_at(row, at);
        }
        // Rows do not tile the window: a heading carries space above it, and
        // the note has margins. A click that lands in one of those gaps belongs
        // to the row it is nearest, not to whichever row happens to be last.
        let distance = if at.y < bounds.top() {
            bounds.top() - at.y
        } else {
            at.y - bounds.bottom()
        };
        if nearest.is_none_or(|(nearest, _)| distance < nearest) {
            nearest = Some((distance, row));
        }
    }

    let Some((_, row)) = nearest else {
        return len;
    };
    // Pull the point into that row so its column still decides where in the
    // line the caret lands.
    let bounds = row.geometry.bounds();
    let y = at.y.max(bounds.top()).min(bounds.bottom() - px(1.));
    offset_at(row, point(at.x, y))
}

/// Offset one logical line above, keeping the grapheme column. The fallback for
/// a line that is not on screen and so has no measured geometry.
fn logical_above(text: &str, index: &LineIndex, offset: usize) -> usize {
    let line = index.line_at(offset);
    if line == 0 {
        return 0;
    }
    let col = index.grapheme_col(text, offset);
    index.offset_at_grapheme_col(text, line - 1, col)
}

/// Offset one logical line below, keeping the grapheme column.
fn logical_below(text: &str, index: &LineIndex, offset: usize) -> usize {
    let line = index.line_at(offset);
    let last = index.line_count().saturating_sub(1);
    if line >= last {
        return text.len();
    }
    let col = index.grapheme_col(text, offset);
    index.offset_at_grapheme_col(text, line + 1, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monospace layout with known wraps. Not a stand-in for real shaping —
    /// where Lilex actually breaks a line only the running app can say — but
    /// every caret bug this app has had was arithmetic, and arithmetic is
    /// exactly what this pins.
    struct Mono {
        len: usize,
        top: Pixels,
    }

    const ADVANCE: f32 = 10.;
    const COLS: usize = 30;
    const LINE_HEIGHT: f32 = 20.;

    impl Mono {
        /// A line exactly `COLS` long is one row, not two: the offset at the
        /// wrap belongs to the end of the row before it.
        fn rows(&self) -> usize {
            self.len.div_ceil(COLS).max(1)
        }
    }

    impl RowGeometry for Mono {
        fn bounds(&self) -> Bounds<Pixels> {
            Bounds::from_corners(
                point(px(0.), self.top),
                point(px(COLS as f32 * ADVANCE), self.top + px(self.rows() as f32 * LINE_HEIGHT)),
            )
        }

        fn line_height(&self) -> Pixels {
            px(LINE_HEIGHT)
        }

        fn position_for_index(&self, local: usize) -> Option<Point<Pixels>> {
            if local > self.len {
                return None;
            }
            // A wrap boundary reports the end of the earlier row, exactly as
            // GPUI's layout does.
            let (row, col) = if local > 0 && local.is_multiple_of(COLS) {
                (local / COLS - 1, COLS)
            } else {
                (local / COLS, local % COLS)
            };
            Some(point(
                px(col as f32 * ADVANCE),
                self.top + px(row as f32 * LINE_HEIGHT),
            ))
        }

        fn index_for_position(&self, at: Point<Pixels>) -> Result<usize, usize> {
            let row = ((at.y - self.top).to_f64() / LINE_HEIGHT as f64).floor().max(0.) as usize;
            let col = (at.x.to_f64() / ADVANCE as f64).round().max(0.) as usize;
            let index = (row * COLS + col.min(COLS)).min(self.len);
            Ok(index)
        }
    }

    /// Three lines: a 90-character line that wraps into three rows, a short
    /// line, then another 90-character line.
    fn fixture() -> (String, LineIndex, HashMap<usize, Row<Mono>>) {
        let long = "a".repeat(90);
        let text = format!("{long}\nabc\n{long}");
        let index = LineIndex::new(&text);
        let mut rows = HashMap::new();
        let mut top = px(0.);
        for line in 0..index.line_count() {
            let (start, end) = index.line_range(line).unwrap();
            let geometry = Mono {
                len: end - start,
                top,
            };
            top += px(geometry.rows() as f32 * LINE_HEIGHT);
            rows.insert(
                line,
                Row {
                    byte_start: start,
                    byte_end: end,
                    geometry,
                },
            );
        }
        (text, index, rows)
    }

    #[test]
    fn down_steps_one_visual_row_not_one_logical_line() {
        let (text, index, rows) = fixture();
        let caret = step(&text, &index, &rows, Caret::at(5), true);
        assert_eq!(caret.offset, 35, "still inside the wrapped line, one row down");
    }

    #[test]
    fn down_from_the_last_visual_row_enters_the_next_line() {
        let (text, index, rows) = fixture();
        // Column 5 of the third row of line 0.
        let caret = step(&text, &index, &rows, Caret::at(65), true);
        assert_eq!(caret.offset, 91 + 3, "clamped to the end of \"abc\"");
    }

    #[test]
    fn up_and_down_keep_the_goal_column_across_a_short_row() {
        let (text, index, rows) = fixture();
        // Column 20 of the last row of the first line.
        let start = Caret::at(80);
        let onto_short = step(&text, &index, &rows, start, true);
        assert_eq!(onto_short.offset, 91 + 3, "\"abc\" has no column 20");
        assert!(onto_short.goal_x.is_some(), "the goal column must survive");
        let onto_long = step(&text, &index, &rows, onto_short, true);
        assert_eq!(
            onto_long.offset,
            95 + 20,
            "the column it left, not the column it landed in"
        );
    }

    #[test]
    fn a_wrap_boundary_offset_lands_on_the_head_of_the_row_below() {
        let (text, index, rows) = fixture();
        // Down the left margin, so the landing offset is the wrap itself.
        let caret = step(&text, &index, &rows, Caret::at(0), true);
        assert_eq!(caret.offset, 30);
        assert_eq!(
            caret.affinity,
            Affinity::RowStart,
            "the head of row 1, not the tail of row 0"
        );
        // Anywhere else in the row, it is the same column one row down.
        assert_eq!(step(&text, &index, &rows, Caret::at(2), true).offset, 32);
    }

    /// The bug that made a wrapped row's first position unreachable: stepping
    /// down onto it produced an offset that drew at the far end of the row
    /// above, so the caret never moved.
    #[test]
    fn stepping_down_a_wrapped_line_always_moves_the_caret() {
        let (text, index, rows) = fixture();
        let mut caret = Caret::at(0);
        let mut seen = vec![caret.offset];
        for _ in 0..3 {
            caret = step(&text, &index, &rows, caret, true);
            assert!(
                !seen.contains(&caret.offset),
                "stuck at {} — {seen:?}",
                caret.offset
            );
            seen.push(caret.offset);
        }
        assert_eq!(seen, vec![0, 30, 60, 91]);
    }

    #[test]
    fn the_caret_at_a_wrap_draws_at_the_left_margin_of_the_row_below() {
        let (_, _, rows) = fixture();
        let row = &rows[&0];
        let head = caret_origin(row, 30, 31, Affinity::RowStart).unwrap().0;
        assert_eq!(head, point(px(0.), px(LINE_HEIGHT)));
        let tail = caret_origin(row, 30, 31, Affinity::RowEnd).unwrap().0;
        assert_eq!(tail, point(px(COLS as f32 * ADVANCE), px(0.)));
    }

    /// Every position the caret can occupy must be reachable by clicking where
    /// that caret is drawn. This is the invariant the affinity bug broke.
    #[test]
    fn every_caret_position_is_reachable_by_clicking_it() {
        let (text, index, rows) = fixture();
        let row = &rows[&0];
        for offset in 0..=90 {
            for affinity in [Affinity::RowEnd, Affinity::RowStart] {
                let next = index::next_grapheme(&text, &index, offset);
                let (origin, height) = caret_origin(row, offset, next, affinity).unwrap();
                let hit = offset_at_point(
                    text.len(),
                    &rows,
                    point(origin.x + px(1.), origin.y + height * 0.5),
                );
                assert_eq!(hit, offset, "offset {offset} with {affinity:?}");
            }
        }
    }

    #[test]
    fn row_edge_stops_at_the_visual_row_not_the_logical_line() {
        let (text, index, rows) = fixture();
        // Offset 45 is in the middle of the second row of the first line.
        let start = row_edge(&text, &index, &rows, Caret::at(45), false);
        assert_eq!(start.offset, 30, "the row's start, not the line's");
        let end = row_edge(&text, &index, &rows, Caret::at(45), true);
        assert_eq!(end.offset, 60, "the row's end, not the line's");
    }

    #[test]
    fn up_from_the_first_row_runs_to_the_start_of_the_document() {
        let (text, index, rows) = fixture();
        assert_eq!(step(&text, &index, &rows, Caret::at(5), false), Caret::at(0));
    }

    #[test]
    fn down_from_the_last_row_runs_to_the_end() {
        let (text, index, rows) = fixture();
        let last = text.len() - 5;
        assert_eq!(
            step(&text, &index, &rows, Caret::at(last), true).offset,
            text.len()
        );
    }

    #[test]
    fn a_line_that_is_not_on_screen_falls_back_to_logical_motion() {
        let (text, index, _) = fixture();
        let empty: HashMap<usize, Row<Mono>> = HashMap::new();
        // No geometry at all: it still moves, by whole lines, keeping the column.
        let caret = step(&text, &index, &empty, Caret::at(5), true);
        assert_eq!(caret.offset, 91 + 3, "column 5 clamped to the end of \"abc\"");
        assert_eq!(caret.goal_x, None);
    }

    /// Rows do not tile the window — a heading carries space above it — and a
    /// click in that space used to fall through to "the bottom-most row", which
    /// threw the caret to the end of the screen and scrolled the view with it.
    #[test]
    fn a_click_in_the_space_above_a_row_goes_to_the_nearer_row() {
        let (text, index, mut rows) = fixture();
        // Push the short middle line down — and everything after it — leaving a
        // 40px gap above it, the way a heading's space above works.
        for line in [1, 2] {
            rows.get_mut(&line).unwrap().geometry.top += px(40.);
        }
        let gap_top = px(3. * LINE_HEIGHT);

        // Just under the first line's last row: still nearest that row.
        let hit = offset_at_point(text.len(), &rows, point(px(0.), gap_top + px(5.)));
        assert_eq!(index.line_at(hit), 0, "nearest row is the line above");

        // Just above the short line: nearest that one, at its own column.
        let hit = offset_at_point(text.len(), &rows, point(px(20.), gap_top + px(38.)));
        assert_eq!(index.line_at(hit), 1, "nearest row is the line below");
        assert_eq!(hit, 91 + 2, "the click's column still decides");
    }

    #[test]
    fn a_click_past_the_visible_rows_clamps_to_the_nearest_one() {
        let (text, index, rows) = fixture();
        let first = rows[&0].byte_start;
        let last = rows[&(index.line_count() - 1)].byte_end;
        assert_eq!(
            offset_at_point(text.len(), &rows, point(px(0.), px(-500.))),
            first,
            "far above everything: the start of the first visible row"
        );
        assert_eq!(
            offset_at_point(text.len(), &rows, point(px(9999.), px(9999.))),
            last,
            "far below everything: the end of the last visible row"
        );
    }

    /// A horizontal arrow lands on an offset, not a y. The affinity it should
    /// carry is decided purely by whether that offset is a wrap boundary — the
    /// head of the wrapped row when it is, the plain tail otherwise. This is the
    /// test the ←/→/word handlers lean on so a move onto a wrap does not draw
    /// the caret at the end of the row above.
    #[test]
    fn is_wrap_boundary_is_true_only_at_a_real_wrap() {
        let (text, index, rows) = fixture();
        let row = &rows[&0];
        for offset in 0..=90 {
            let next = index::next_grapheme(&text, &index, offset);
            let expected = offset == 30 || offset == 60;
            assert_eq!(is_wrap_boundary(row, offset, next), expected, "offset {offset}");
        }
        // The logical line ends are never wrap boundaries: line 1 ("abc") fits on
        // one row, so neither its start nor its end reports a wrap.
        let short = &rows[&1];
        for offset in short.byte_start..=short.byte_end {
            let next = index::next_grapheme(&text, &index, offset);
            assert!(!is_wrap_boundary(short, offset, next), "offset {offset}");
        }
    }

    #[test]
    fn affinity_is_row_start_only_at_a_real_wrap() {
        let (text, index, rows) = fixture();
        let row = &rows[&0];
        for offset in 0..=90 {
            let below = affinity_at(&text, &index, row, offset, px(9999.));
            let expected = if offset == 30 || offset == 60 {
                Affinity::RowStart
            } else {
                Affinity::RowEnd
            };
            assert_eq!(below, expected, "offset {offset}");
        }
    }
}
