//! A `multiline` `textfield`'s visual rows: its draft wrapped where paint wraps it, and the caret
//! geometry that keys, presses, the wheel and the input method read from those same rows.

use crate::layout::node::{TextAlign, Typeface};
use crate::text::shaping::{self, ShapeResult, ShapedLine, ShapingHandle};

/// The width a field wraps at: its own less the caret bar, so a caret after a full row stays inside.
pub(crate) fn wrap_width(field_width: f32, bar: f32) -> f32 {
    (field_width - bar).max(0.0)
}

/// `text`'s rows wrapped at `width` as paint draws them, handed to `f`.
// ponytail: re-wraps the whole draft per key, press and measure; reuse paragraphs past a few KiB.
pub(crate) fn with_rows<T>(
    text: &str,
    face: &Typeface,
    width: f32,
    shaping: &ShapingHandle,
    f: impl FnOnce(&[Row<'_>]) -> T,
) -> T {
    let paragraphs = shaping.shape_lines(text, &[], face.shaping_style(), face.font.as_ref(), Some(width));
    f(&rows(text, &paragraphs))
}

/// How many rows `text` wraps to at `width`.
pub(crate) fn count(text: &str, face: &Typeface, width: f32, shaping: &ShapingHandle) -> usize {
    with_rows(text, face, width, shaping, |rows| rows.len())
}

/// One visual row: the bytes a caret on it can take, and its glyphs, whose offsets count from `para`.
pub(crate) struct Row<'a> {
    pub start: usize,
    pub end: usize,
    para: usize,
    line: &'a ShapedLine,
}

/// The rows of paragraphs `shape_lines` wrapped. A soft-wrapped row ends before the spaces at its
/// break; a paragraph's last row ends at its newline, so End there keeps trailing spaces.
// ponytail: a mid-word wrap's end is the next row's start, so End lands there; caret affinity is the upgrade.
pub(crate) fn rows<'a>(text: &str, paragraphs: &'a [(usize, ShapeResult)]) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for &(para, ref shaped) in paragraphs {
        let para_end = text[para..].find('\n').map_or(text.len(), |len| para + len);
        let last = shaped.shaped.len().saturating_sub(1);
        for (index, (range, line)) in shaped.line_ranges.iter().zip(shaped.shaped.iter()).enumerate() {
            let end = if index == last { para_end } else { para + range.end };
            rows.push(Row { start: para + range.start, end, para, line });
        }
    }
    rows
}

/// The row a caret at `at` draws on: the last that starts at or before it.
pub(crate) fn row_of(rows: &[Row<'_>], at: usize) -> usize {
    rows.iter().rposition(|row| row.start <= at).unwrap_or(0)
}

impl Row<'_> {
    /// Where the row starts under `align` in a box from `x0`, `width` wide.
    pub(crate) fn left(&self, align: TextAlign, x0: f32, width: f32) -> f32 {
        align.line_left(self.line.rtl, x0, x0 + width, self.line.width)
    }

    /// A caret at `at`, row-relative.
    pub(crate) fn caret_x(&self, at: usize) -> f32 {
        shaping::caret_x(self.line, at - self.para)
    }

    /// The byte on this row nearest row-relative `x`.
    pub(crate) fn caret_at(&self, x: f32) -> usize {
        (self.para + shaping::caret_at(self.line, x, self.end - self.para)).clamp(self.start, self.end)
    }
}

/// How far `rows` rows of `line_height` overflow a `height` box.
pub(crate) fn max_scroll(rows: usize, line_height: f32, height: f32) -> f32 {
    (rows as f32 * line_height - height).max(0.0)
}

/// `scroll` kept within [`max_scroll`].
pub(crate) fn clamp_scroll(scroll: f32, rows: usize, line_height: f32, height: f32) -> f32 {
    scroll.clamp(0.0, max_scroll(rows, line_height, height))
}

/// `scroll` after an edit: moved the least that shows row `caret` whole, if given, then clamped.
pub(crate) fn fitted(scroll: f32, caret: Option<usize>, rows: usize, line_height: f32, height: f32) -> f32 {
    let scroll = caret.map_or(scroll, |row| {
        let top = row as f32 * line_height;
        scroll.max(top + line_height - height).min(top)
    });
    clamp_scroll(scroll, rows, line_height, height)
}

/// `scroll` moved by `delta`, or `None` at its limit, where the wheel goes on outward.
pub(crate) fn wheeled(scroll: f32, delta: f32, rows: usize, line_height: f32, height: f32) -> Option<f32> {
    let next = clamp_scroll(scroll + delta, rows, line_height, height);
    (next != scroll).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_wrap_at_words_and_end_a_paragraph_at_its_newline() {
        let shaping = ShapingHandle::spawn();
        let face = crate::layout::hit::tests::face(14.0, 0.0);
        let text = "hello world foo bar  \n\nend";
        let paragraphs = shaping.shape_lines(text, &[], face.shaping_style(), None, Some(60.0));
        let rows = rows(text, &paragraphs);
        let spans: Vec<_> = rows.iter().map(|row| &text[row.start..row.end]).collect();
        assert_eq!(spans, ["hello", "world", "foo bar  ", "", "end"]);
        assert_eq!(row_of(&rows, 6), 1, "the byte after a soft break is the next row's");
        assert_eq!(row_of(&rows, 5), 0);
        assert_eq!(row_of(&rows, 22), 3, "an empty paragraph is a row");
        assert_eq!(rows[1].caret_at(1000.0), 11, "past a soft-wrapped row is its end, not the next row");
        assert_eq!(count(text, &face, 60.0, &shaping), 5);
    }

    #[test]
    fn fitted_shows_the_caret_row_and_drops_a_stale_offset_when_the_text_shrinks() {
        assert_eq!(fitted(0.0, Some(4), 5, 10.0, 30.0), 20.0, "row 4 at the bottom of three");
        assert_eq!(fitted(20.0, Some(3), 5, 10.0, 30.0), 20.0, "a visible row does not move it");
        assert_eq!(fitted(20.0, Some(1), 5, 10.0, 30.0), 10.0, "row 1 at the top");
        assert_eq!(fitted(50.0, None, 4, 10.0, 30.0), 10.0, "held inside the overflow");
        assert_eq!(fitted(20.0, None, 2, 10.0, 30.0), 0.0, "rows that fit again do not scroll");
    }

    #[test]
    fn the_wheel_moves_inside_the_overflow_and_reports_none_at_its_limit() {
        assert_eq!(wheeled(0.0, 15.0, 5, 10.0, 30.0), Some(15.0));
        assert_eq!(wheeled(15.0, 15.0, 5, 10.0, 30.0), Some(20.0), "stops at the last row");
        assert_eq!(wheeled(20.0, 15.0, 5, 10.0, 30.0), None, "at the limit the wheel goes outward");
        assert_eq!(wheeled(0.0, 15.0, 3, 10.0, 30.0), None, "and from a field that fits");
    }
}
