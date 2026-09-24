use super::{Glyph, ShapedLine};

/// How thick a caret draws at `font_size`. Shared, because paint and hit-testing slide a line by
/// the caret's own width and a disagreement there moves every press, not just the ones at the edge.
pub fn caret_thickness(font_size: f32) -> f32 {
    (font_size / 16.0).max(1.0).round()
}

/// `left` slid so a caret at `cx` and its `thickness` stay inside `[x0, x1]`.
///
/// A draft wider than its field would otherwise hang off the end and take the caret with it, so
/// past that width the line follows the caret instead of its alignment.
pub fn caret_visible_left(left: f32, x0: f32, x1: f32, cx: f32, thickness: f32) -> f32 {
    let at = left + cx;
    left + (x0 - at).max(0.0) + (x1 - thickness - at).min(0.0)
}

/// The byte a caret takes when the pointer lands at `x`, line-relative.
///
/// The half of a cluster's box nearer its own leading edge takes its `start`, and a right-to-left
/// cluster leads on the right. Per cluster, never per line: Latin embedded in an Arabic line runs
/// the other way, and reading the line's direction there puts the caret one cluster off at exactly
/// the boundary where the two meet -- which every single-direction test still passes.
pub fn caret_at(line: &ShapedLine, x: f32, text_len: usize) -> usize {
    // By cluster, not by glyph: one cluster can shape to several glyphs, each carrying its whole
    // byte range, and halving them one at a time answers the right half of a letter with the byte
    // belonging to its left.
    for cluster in clusters(&line.glyphs) {
        let (lo, hi) = cluster_span(cluster);
        if x < lo || x >= hi {
            continue;
        }
        return match (x < (lo + hi) / 2.0) != cluster[0].rtl {
            true => cluster[0].start,
            false => cluster[0].end,
        };
    }
    // Past every box, so the line's own direction says which byte each side is.
    match line.glyphs.iter().all(|glyph| x < glyph.x) != line.rtl {
        true => 0,
        false => text_len,
    }
}

/// Where a caret at `offset` draws, line-relative: the inverse of [`caret_at`], whose every answer
/// is some glyph's `start` or `end`.
///
/// An offset where two runs meet ends one glyph and starts another, and both edges are valid: the
/// first in visual order wins, so the caret stays on the run the text before it belongs to.
///
/// ponytail: an offset inside a cluster draws at the cluster's leading edge, so stepping through
/// `لا` moves the caret without moving the mark. Both this and the boundary tie-break above want
/// the same upgrade: a caret that carries which side of a boundary it sits on.
pub fn caret_x(line: &ShapedLine, offset: usize) -> f32 {
    // The first cluster in visual order that `offset` touches decides the edge, so a boundary
    // between two runs stays with the earlier one.
    let Some(cluster) = clusters(&line.glyphs).find(|cluster| (cluster[0].start..=cluster[0].end).contains(&offset))
    else {
        return match line.rtl {
            true => line.width,
            false => 0.0,
        };
    };
    // Across every glyph of it, never just the first: `س` shapes to two in some faces, and where
    // they meet is the middle of the letter.
    let (lo, hi) = cluster_span(cluster);
    // Only the cluster's last byte sits on its trailing edge; every other offset leads it.
    match (offset == cluster[0].end && offset != cluster[0].start) != cluster[0].rtl {
        true => hi,
        false => lo,
    }
}

/// One cluster's glyphs at a time. A cluster can shape to several, each carrying its whole byte
/// range, and they arrive together because the line is in visual order.
fn clusters(glyphs: &[Glyph]) -> impl Iterator<Item = &[Glyph]> {
    glyphs.chunk_by(|a, b| a.start == b.start && a.end == b.end)
}

/// The left and right edges of one cluster's glyphs together.
fn cluster_span(cluster: &[Glyph]) -> (f32, f32) {
    cluster
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), glyph| (lo.min(glyph.x), hi.max(glyph.x + glyph.advance)))
}

#[cfg(test)]
mod tests {
    use super::super::tests::req;
    use super::super::*;
    use super::*;

    /// One cluster can shape to several glyphs, each carrying the cluster's whole byte range --
    /// `س` is two in the face this shell draws with. Measured live: the caret landed at 11.72 in a
    /// letter 23.44 wide, dead centre, because the first of the two glyphs ends where the other
    /// begins.
    #[test]
    fn a_cluster_drawn_as_two_glyphs_puts_the_caret_at_the_whole_letter_s_edge() {
        let half = |x: f32| Glyph {
            face: fontdb::ID::dummy(),
            weight: 400,
            id: 0,
            x,
            y: 0.0,
            advance: 11.72,
            start: 0,
            end: 2,
            rtl: true,
        };
        let line = ShapedLine { rtl: true, width: 23.44, baseline: 0.0, glyphs: vec![half(11.72), half(0.0)].into() };

        assert_eq!(caret_x(&line, 2), 0.0, "after the letter is its left edge, not between its halves");
        assert_eq!(caret_x(&line, 0), 23.44, "and before it is its right");
        assert_eq!(caret_at(&line, 18.0, 2), 0, "a press in the right half of a right-to-left letter precedes it");
        assert_eq!(caret_at(&line, 5.0, 2), 2, "and one in the left half follows it");
    }

    /// A draft too wide for its field follows the caret instead of its alignment. Paint and the
    /// press that reads it both slide by this, so a disagreement here would move every click.
    #[test]
    fn a_line_too_wide_for_its_field_slides_to_keep_the_caret_inside() {
        // A box 100 wide at x = 10, and a caret two pixels thick.
        let (x0, x1, thickness) = (10.0, 110.0, 2.0);

        assert_eq!(caret_visible_left(10.0, x0, x1, 200.0, thickness), -92.0, "a caret past the right edge");
        assert_eq!(caret_visible_left(-50.0, x0, x1, 0.0, thickness), 10.0, "and one past the left");
        assert_eq!(caret_visible_left(10.0, x0, x1, 40.0, thickness), 10.0, "a line that fits keeps its alignment");
    }

    /// ADR-0236: byte 0 is the first letter, which a right-to-left line draws rightmost, so its
    /// caret belongs at the line's right edge and the press that lands there answers with it.
    #[test]
    fn a_right_to_left_caret_leads_on_the_right() {
        let handle = ShapingHandle::spawn();
        let text = "مرحبا";
        let shaped = handle.shape_glyphs(req(text, 20.0));
        let line = &shaped.shaped[0];

        assert_eq!(caret_x(line, 0), line.width, "the first byte draws at the right edge");
        assert_eq!(caret_x(line, text.len()), 0.0, "and the last at the left");
        assert_eq!(caret_at(line, line.width - 1.0, text.len()), 0, "a press at the right edge is byte 0");
        assert_eq!(caret_at(line, 1.0, text.len()), text.len(), "and one at the left is the end");
    }

    /// An embedded Latin run runs the other way inside a right-to-left line, so which side of a
    /// glyph is "before" comes from the glyph and never from the line. Reading `line.rtl` here
    /// answers 11 instead of 12 -- one cluster off, silently, and only where the two meet.
    #[test]
    fn a_caret_in_an_embedded_run_reads_that_runs_direction() {
        let handle = ShapingHandle::spawn();
        let text = "مرحبا abc";
        let shaped = handle.shape_glyphs(req(text, 20.0));
        let line = &shaped.shaped[0];
        assert!(line.rtl, "the line is right to left");
        let a = line.glyphs.iter().find(|glyph| glyph.start == 11).expect("a glyph for 'a'");
        assert!(!a.rtl, "but the letter 'a' in it is not");

        assert_eq!(caret_at(line, a.x + a.advance * 0.75, text.len()), 12, "past 'a' is before 'b'");
        assert_eq!(caret_at(line, a.x + a.advance * 0.25, text.len()), 11, "and short of it is before 'a'");
        // Byte 11 ends the space and starts 'a': two valid carets at a direction change. The
        // first in visual order wins, which keeps it on the Arabic the text before it belongs to.
        let space = line.glyphs.iter().find(|glyph| glyph.end == 11).expect("a glyph for the space");
        assert_eq!(caret_x(line, 11), space.x, "the earlier run's trailing edge, not 'a' s leading one");
        assert_eq!(caret_x(line, 12), a.x + a.advance, "and past 'a' is its own trailing edge");
    }
}
