use std::ops::Range;

use crate::layout::node::{self, PaintStyle, StyleRun};
use crate::text::shaping::{self, ShapeRequest, ShapingHandle};

/// Rewrites a `text`'s content to what its box can actually show, which is the one thing paint
/// cannot work out for itself.
///
/// Runs here rather than in `layout::paint`: the box width isn't known until this node has been
/// sized, and the shaping worker isn't reachable from a display-list build, which is pure by
/// design. Does nothing for a run that neither wraps nor elides, and nothing on a `Content`-sized
/// node under `elide` alone, whose box came from measuring this same string and so always fits it.
pub(super) fn fit_text_to_box(
    paint: &mut Option<PaintStyle>,
    content_width: f32,
    unconstrained_width: Option<f32>,
    shaping: &ShapingHandle,
) {
    let Some(PaintStyle::Text { content, runs, font_size, font, elide, wrap, max_lines, .. }) = paint.as_mut() else {
        return;
    };
    // Before the early returns below, and before any measurement: taffy's `compute_leaf_layout`
    // returns early when a node's width and height are both known, so a fully-sized `text` is never
    // handed to the measure callback, and without `elide` or `wrap` it never reaches the shaping
    // below either. Nothing would then load its family, and it would paint in the declared chain
    // (ADR-0144).
    if let Some(family) = font.as_ref() {
        shaping.ensure_family(family);
    }
    if content.is_empty() || content_width <= 0.0 {
        return;
    }
    let face = Face { size: *font_size, family: font.clone() };
    // The output is taken off the builder before the borrow of `content` ends, which is what lets
    // the same two fields be overwritten below.
    let fitted: Option<(String, Vec<StyleRun>)> = match wrap {
        // The measured-width check is the fast path, not politeness: most strings fit, and
        // skipping the binary search below is the difference on a list of them.
        node::Wrap::None => {
            let width = unconstrained_width.unwrap_or_else(|| measured_width(content, runs, &face, shaping));
            if *elide == node::Elide::End && width > content_width {
                let mut fitted = Fitted::new(content, runs);
                let cut = elide_cut(content, runs, 0..content.len(), &face, content_width, shaping);
                fitted.push_source(0..cut);
                fitted.push_ellipsis(cut);
                Some((fitted.text, fitted.runs))
            } else {
                None
            }
        }
        node::Wrap::Word => {
            let fitted = wrapped_to_fit(content, runs, &face, *elide, *max_lines, content_width, shaping);
            Some((fitted.text, fitted.runs))
        }
    };
    if let Some((text, styled)) = fitted {
        *content = text.into();
        *runs = styled;
    }
}

/// A `text`'s content being rebuilt to fit its box, with its styled runs following it (ADR-0104).
///
/// Runs are byte ranges into `content`, so every rewrite here (joined lines, flattened remainder,
/// ellipsis) goes through this one place that appends source slices and re-bases the runs
/// overlapping each slice.
struct Fitted<'s> {
    source: &'s str,
    source_runs: &'s [StyleRun],
    text: String,
    runs: Vec<StyleRun>,
}

impl<'s> Fitted<'s> {
    fn new(source: &'s str, source_runs: &'s [StyleRun]) -> Self {
        Self { source, source_runs, text: String::new(), runs: Vec::new() }
    }

    /// Appends `source[range]` and the parts of any run that fall inside it, re-based. Newlines in
    /// the slice become spaces when `flatten` is set: a remainder being collapsed onto an elided
    /// last line must not carry the paragraph breaks it spanned, and a space is the same width in
    /// bytes, so the runs need no adjustment for it.
    fn push_source_flattened(&mut self, range: Range<usize>, flatten: bool) {
        let at = self.text.len();
        let slice = &self.source[range.clone()];
        if flatten {
            self.text.extend(slice.chars().map(|c| if c == '\n' { ' ' } else { c }));
        } else {
            self.text.push_str(slice);
        }
        for run in self.source_runs {
            let start = run.range.start.max(range.start);
            let end = run.range.end.min(range.end);
            if start < end {
                self.runs.push(StyleRun { range: at + (start - range.start)..at + (end - range.start), ..run.clone() });
            }
        }
    }

    fn push_source(&mut self, range: Range<usize>) {
        self.push_source_flattened(range, false);
    }

    /// A plain separator, part of no run.
    fn push_plain(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// The ellipsis, in the style of the character it replaced (the run covering `cut`, if any,
    /// else the one just before it): a truncated bold sentence ends in a bold ellipsis.
    fn push_ellipsis(&mut self, cut: usize) {
        let at = self.text.len();
        self.text.push('\u{2026}');
        let style = self
            .source_runs
            .iter()
            .find(|run| run.range.contains(&cut))
            .or_else(|| self.source_runs.iter().rev().find(|run| run.range.end == cut && cut > 0));
        if let Some(run) = style {
            self.runs.push(StyleRun { range: at..self.text.len(), ..run.clone() });
        }
    }
}

/// `content` broken to `content_width` and capped to `max_lines`, joined by `\n` for
/// `text::atlas::TextPainter::draw_text` to walk, with its runs re-based to the result.
///
/// The cap and `elide` compose, which is the notification-body case: keep the lines allowed, and
/// if an ellipsis was asked for, rebuild the last one out of everything that did not fit so it
/// reads as truncated rather than as a sentence that happens to stop.
///
/// That remainder is the source from the last kept line's start to the end, with its paragraph
/// breaks flattened to spaces. A source range rather than the dropped lines rejoined, so the runs
/// follow it and the source's own whitespace survives at the breaks.
fn wrapped_to_fit<'s>(
    content: &'s str,
    runs: &'s [StyleRun],
    face: &Face,
    elide: node::Elide,
    max_lines: Option<usize>,
    content_width: f32,
    shaping: &ShapingHandle,
) -> Fitted<'s> {
    let shaped = shaping.shape(ShapeRequest {
        text: content.to_string(),
        font_size: face.size,
        line_height: shaping::line_height(face.size),
        max_width: Some(content_width),
        runs: node::font_runs(runs),
        font: face.family.clone(),
    });
    let mut fitted = Fitted::new(content, runs);
    let rtl = |index: usize| shaped.shaped.get(index).is_some_and(|line| line.rtl);
    let Some(cap) = max_lines.filter(|cap| *cap < shaped.line_ranges.len()) else {
        for (index, range) in shaped.line_ranges.iter().enumerate() {
            if index > 0 {
                fitted.push_plain("\n");
            }
            push_direction_mark(&mut fitted, &content[range.clone()], rtl(index));
            fitted.push_source(range.clone());
        }
        return fitted;
    };

    // `cap` is at least 1: `MaxLines` maps 0 to no cap at all, so a `Some` cap standing
    // below a nonzero line count always leaves a line to rewrite.
    for (index, range) in shaped.line_ranges[..cap - 1].iter().enumerate() {
        push_direction_mark(&mut fitted, &content[range.clone()], rtl(index));
        fitted.push_source(range.clone());
        fitted.push_plain("\n");
    }
    let last = &shaped.line_ranges[cap - 1];
    if elide == node::Elide::End {
        let rest = last.start..shaped.line_ranges.last().map_or(last.end, |range| range.end);
        let cut = elide_cut(content, runs, rest.clone(), face, content_width, shaping);
        push_direction_mark(&mut fitted, &content[rest.start..cut].replace('\n', " "), rtl(cap - 1));
        fitted.push_source_flattened(rest.start..cut, true);
        fitted.push_ellipsis(cut);
    } else {
        push_direction_mark(&mut fitted, &content[last.clone()], rtl(cap - 1));
        fitted.push_source(last.clone());
    }
    fitted
}

/// Prefixes the mark that makes `line`, shaped alone, read the way its paragraph does (ADR-0211).
fn push_direction_mark(fitted: &mut Fitted<'_>, line: &str, rtl: bool) {
    if matches!(unicode_bidi::get_base_direction(line), unicode_bidi::Direction::Rtl) != rtl {
        fitted.push_plain(if rtl { "\u{200F}" } else { "\u{200E}" });
    }
}

/// What a string is measured in: how big, and in which family (ADR-0144). The two travel together
/// through every fitting helper, and a measurement taken under one pair says nothing about the
/// other.
#[derive(Clone)]
struct Face {
    size: f32,
    family: Option<std::sync::Arc<str>>,
}

/// One string's unconstrained width, the question `elide` is a search over.
fn measured_width(text: &str, runs: &[StyleRun], face: &Face, shaping: &ShapingHandle) -> f32 {
    shaping
        .shape(ShapeRequest {
            text: text.to_string(),
            font_size: face.size,
            line_height: shaping::line_height(face.size),
            max_width: None,
            runs: node::font_runs(runs),
            font: face.family.clone(),
        })
        .width
}

/// Where to cut `region` of `text` so that what precedes the cut, plus an ellipsis, still fits
/// `width`: a byte offset within `region`, at a character boundary. `region.start` -- the ellipsis
/// alone -- is always admissible and is the honest answer for a box too narrow for one character.
///
/// Always ellipsizes, even for a `text` already narrow enough: the callers that want "leave it
/// alone if it fits" ask [`measured_width`] first, and the one that doesn't is truncating a
/// remainder, where the ellipsis is the whole point of the call. Each candidate is measured with
/// the runs it would carry, so a bold prefix is not cut where a regular one would fit.
///
/// ponytail: cuts at a character boundary rather than a grapheme cluster, the real ceiling: an
/// emoji with a skin-tone modifier can lose the modifier and change what it draws. Nothing in this
/// shell's own strings does that yet; window titles arriving from outside it eventually will.
/// Upgrade path: a `unicode-segmentation` pass over grapheme boundaries.
fn elide_cut(
    text: &str,
    runs: &[StyleRun],
    region: Range<usize>,
    face: &Face,
    width: f32,
    shaping: &ShapingHandle,
) -> usize {
    let slice = &text[region.clone()];
    if slice.is_empty() {
        return region.start;
    }
    // Byte offsets a prefix may be cut at, so the search never lands inside a codepoint. The
    // last entry is the start of the final character, the longest prefix worth trying: appending
    // an ellipsis to the whole string is never narrower than the string.
    let cuts: Vec<usize> = slice.char_indices().map(|(index, _)| region.start + index).collect();
    let fits = |cut: usize| {
        let mut candidate = Fitted::new(text, runs);
        candidate.push_source_flattened(region.start..cut, true);
        candidate.push_ellipsis(cut);
        measured_width(&candidate.text, &candidate.runs, face, shaping) <= width
    };
    // Largest index whose prefix plus an ellipsis fits; zero is always admissible.
    let (mut low, mut high) = (0usize, cuts.len() - 1);
    while low < high {
        // Rounded up, so `mid` is always above `low` and the loop cannot stall; `high` is only ever
        // assigned `mid - 1`, and `mid` is at least 1 whenever this body runs.
        let mid = low + (high - low).div_ceil(2);
        if fits(cuts[mid]) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    cuts[low]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::scene::tests::{apply_at, full, surface_from};
    use crate::layout::scene::*;

    fn drawn_text(scene: &Scene) -> String {
        drawn_text_and_runs(scene).0
    }

    fn drawn_text_and_runs(scene: &Scene) -> (String, Vec<StyleRun>) {
        fn find(node: &ResolvedNode) -> Option<(String, Vec<StyleRun>)> {
            if let Some(PaintStyle::Text { content, runs, .. }) = &node.paint {
                return Some((content.to_string(), runs.clone()));
            }
            node.children.iter().find_map(find)
        }
        find(scene.surface("bar@TEST").unwrap()).expect("expected a text node")
    }

    // ---- styled runs following a wrap or an elide (ADR-0104) ----

    fn styled(lua_src: &str) -> (String, Vec<StyleRun>) {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(lua_src);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        drawn_text_and_runs(&scene)
    }

    /// A run that spans a wrap break is split across the two lines it lands on, and every range
    /// still slices the *fitted* string to the text it styled.
    #[test]
    fn a_styled_run_follows_its_text_across_a_wrap() {
        let (content, runs) = styled(
            r##"panel { id = "bar", child = text { width = 90, font_size = 14, wrap = "Word", content = {
                { text = "plain " }, { text = "bold words that wrap", bold = true }, { text = " tail" },
            } } }"##,
        );
        assert!(content.contains('\n'), "the string must have wrapped for this to test anything: {content:?}");
        let styled_text: String = runs.iter().map(|run| &content[run.range.clone()]).collect::<Vec<_>>().join("|");
        // The run's own words, in order, with the break's newline now outside them.
        let rejoined = styled_text.replace('|', " ").replace("  ", " ");
        assert_eq!(rejoined.trim(), "bold words that wrap".trim_end(), "runs: {styled_text:?} in {content:?}");
        assert!(runs.iter().all(|run| run.bold));
        assert!(runs.iter().all(|run| !content[run.range.clone()].contains('\n')), "a run never spans a break");
    }

    #[test]
    fn an_elided_styled_text_ends_in_an_ellipsis_of_the_same_style() {
        let (content, runs) = styled(
            r##"panel { id = "bar", child = text { width = 60, font_size = 14, elide = "End", content = {
                { text = "Alice: ", bold = true, color = "#ff0000" }, { text = "a long message that will not fit" },
            } } }"##,
        );
        assert!(content.ends_with('\u{2026}'));
        // "Alice: " is 7 bytes; if the cut fell inside it the ellipsis inherits its style.
        let cut = content.len() - '\u{2026}'.len_utf8();
        let last = runs.last().expect("the bold prefix survives at least in part");
        if cut <= 7 {
            assert_eq!(last.range.end, content.len(), "the ellipsis is inside the bold run");
            assert!(last.bold);
        } else {
            assert_eq!(&content[runs[0].range.clone()], "Alice: ");
        }
    }

    #[test]
    fn a_plain_string_content_carries_no_runs() {
        let (content, runs) = styled(r##"panel { id = "bar", child = text { width = 60, content = "hello" } }"##);
        assert_eq!((content.as_str(), runs.len()), ("hello", 0));
    }

    fn elided(lua_src: &str) -> String {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(lua_src);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        drawn_text(&scene)
    }

    const LONG: &str = "a window title far too long for the box it was given";

    /// The height half of the same round trip, and the behaviour change wrapping brought with it:
    /// a fixed-width `text` that does not ask to wrap now *measures* the one line it paints.
    /// Before, it measured every line cosmic-text would have broken the string onto and painted
    /// one clipped run into a box several times too tall.
    fn text_box(lua_src: &str) -> (String, f32) {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(lua_src);
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let node = &scene.surface("bar@TEST").unwrap().children[0];
        (drawn_text(&scene), node.rect.height)
    }

    #[test]
    fn a_narrow_text_that_does_not_wrap_is_one_line_tall() {
        let (drawn, height) =
            text_box(&format!(r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}" }} }}"#));
        assert_eq!(height, 12.0 * 1.2, "an unwrapped run occupies one line however long it is");
        assert!(!drawn.contains('\n'), "nothing broke it: {drawn:?}");
    }

    #[test]
    fn a_narrow_text_that_wraps_is_broken_into_lines_and_measured_at_their_height() {
        let (drawn, height) = text_box(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word" }} }}"#
        ));
        let lines: Vec<&str> = drawn.lines().collect();
        assert!(lines.len() > 1, "80px cannot hold {LONG:?} on one line, got {drawn:?}");
        assert_eq!(height, lines.len() as f32 * 12.0 * 1.2, "the box has to be as tall as the lines it holds");
        // Whitespace is where the breaks landed, so the words survive and only the gaps moved.
        assert_eq!(drawn.split_whitespace().collect::<Vec<_>>(), LONG.split_whitespace().collect::<Vec<_>>());
    }

    /// taffy probes a wrapping text at max-content, where it is one line, before offering it the
    /// width it will actually get. The one-entry memo in [`solve`] is keyed on that width for this
    /// reason: blind to it, the node would keep the probe's answer and lay out one line of four.
    #[test]
    fn a_wrapping_text_is_measured_at_the_width_it_is_given_not_the_one_it_was_probed_at() {
        let mut scene = Scene::new();
        let shaping = ShapingHandle::spawn();
        let (lua, surface) = surface_from(&format!(
            r#"panel {{ id = "bar", width = 200,
                child = column {{ children = {{ text {{ content = "{LONG}", wrap = "Word" }} }} }} }}"#
        ));
        apply_at(&mut scene, &[surface], full(), &shaping, &lua).unwrap();
        let laid_out = scene.surface("bar@TEST").unwrap().children[0].children[0].rect.height;

        let line_height = shaping::line_height(12.0);
        let fresh = |max_width| {
            shaping
                .shape(ShapeRequest {
                    text: LONG.to_string(),
                    font_size: 12.0,
                    line_height,
                    max_width,
                    runs: Vec::new(),
                    font: None,
                })
                .lines
                .len()
        };
        assert_eq!(fresh(None), 1, "the probe's answer, which the box must not keep");
        assert!(fresh(Some(200.0)) > 1, "the fixture has to wrap for this to test anything");
        assert_eq!(laid_out, fresh(Some(200.0)) as f32 * line_height);
    }

    /// Paint shapes each wrapped line alone, so a line of an Arabic paragraph that opens on an
    /// English word carries a right-to-left mark, or it would draw left to right (ADR-0211). The
    /// long word cannot share a line with the Arabic either side of it.
    #[test]
    fn a_wrapped_line_that_would_read_against_its_paragraph_is_marked() {
        let (drawn, _) = text_box(
            r#"panel { id = "bar", child = text { width = 80, content = "اول wwwwwwwww ثاني", wrap = "Word" } }"#,
        );
        let lines: Vec<&str> = drawn.lines().collect();
        assert!(!lines[0].starts_with('\u{200F}'), "the line that opens the paragraph already agrees: {drawn:?}");
        assert!(
            lines[1..].iter().any(|line| line.starts_with('\u{200F}')),
            "some line opens on the English word: {drawn:?}"
        );
        for line in &lines[1..] {
            let arabic_first = line.chars().next().is_some_and(|c| ('\u{0600}'..='\u{06FF}').contains(&c));
            assert!(arabic_first || line.starts_with('\u{200F}'), "every other line reads right to left: {drawn:?}");
        }
    }

    #[test]
    fn max_lines_caps_both_what_is_drawn_and_the_height_reserved_for_it() {
        let (drawn, height) = text_box(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word", max_lines = 2 }} }}"#
        ));
        assert_eq!(drawn.lines().count(), 2, "got {drawn:?}");
        assert_eq!(height, 2.0 * 12.0 * 1.2, "a capped run reserves the lines it keeps, not the ones it dropped");
    }

    /// The notification-body case: fill the lines allowed, then say the rest was dropped. The
    /// ellipsis has to land on the last kept line and nowhere else.
    #[test]
    fn a_capped_wrap_that_elides_finishes_its_last_line_with_an_ellipsis() {
        let (drawn, _) = text_box(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word", max_lines = 2, elide = "End" }} }}"#
        ));
        let lines: Vec<&str> = drawn.lines().collect();
        assert_eq!(lines.len(), 2, "got {drawn:?}");
        assert!(lines[1].ends_with('\u{2026}'), "the last kept line must be ellipsized: {drawn:?}");
        assert!(!lines[0].ends_with('\u{2026}'), "no earlier line may be: {drawn:?}");
    }

    /// A cap the text never reaches changes nothing, so a config can set `max_lines`
    /// unconditionally and let the content decide.
    #[test]
    fn a_max_lines_above_the_line_count_leaves_the_run_alone() {
        let (drawn, _) = text_box(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word", max_lines = 40, elide = "End" }} }}"#
        ));
        assert!(!drawn.contains('\u{2026}'), "nothing was dropped, so nothing should say it was: {drawn:?}");
        assert_eq!(drawn.split_whitespace().collect::<Vec<_>>(), LONG.split_whitespace().collect::<Vec<_>>());
    }

    /// `max_lines = 0` is the uncapped spelling a `Bound` needs, since a signal cannot produce
    /// "absent". It has to mean the same thing as leaving the property off.
    #[test]
    fn a_max_lines_of_zero_is_no_cap_at_all() {
        let uncapped =
            format!(r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word" }} }}"#);
        let zero = format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", wrap = "Word", max_lines = 0 }} }}"#
        );
        assert_eq!(text_box(&zero), text_box(&uncapped));
    }

    #[test]
    fn a_text_too_wide_for_its_box_is_cut_short_and_finished_with_an_ellipsis() {
        let drawn = elided(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}", elide = "End" }} }}"#
        ));
        assert!(drawn.ends_with('\u{2026}'), "must end with an ellipsis: {drawn:?}");
        assert!(drawn.chars().count() < LONG.chars().count(), "must be shorter than the original: {drawn:?}");
        assert!(LONG.starts_with(drawn.trim_end_matches('\u{2026}')), "must be a prefix of the original: {drawn:?}");
    }

    #[test]
    fn a_text_that_already_fits_is_left_exactly_as_written() {
        let drawn = elided(r#"panel { id = "bar", child = text { width = 600, content = "short", elide = "End" } }"#);
        assert_eq!(drawn, "short", "an ellipsis on a string that fits would be a lie about the content");
    }

    /// The default: no ellipsis, the clip cuts mid-glyph.
    #[test]
    fn a_text_that_does_not_ask_to_elide_keeps_its_whole_string() {
        let drawn = elided(&format!(r#"panel {{ id = "bar", child = text {{ width = 80, content = "{LONG}" }} }}"#));
        assert_eq!(drawn, LONG);
    }

    /// A `Content`-sized box came from measuring this same string, so it fits by construction.
    #[test]
    fn a_content_sized_text_never_elides_itself() {
        let drawn = elided(&format!(r#"panel {{ id = "bar", child = text {{ content = "{LONG}", elide = "End" }} }}"#));
        assert_eq!(drawn, LONG);
    }

    /// The degenerate end of the search: a box too narrow for even one character leaves the
    /// ellipsis alone rather than panicking on an empty prefix or returning the whole string.
    #[test]
    fn a_box_too_narrow_for_one_character_draws_only_the_ellipsis() {
        let drawn = elided(&format!(
            r#"panel {{ id = "bar", child = text {{ width = 1, content = "{LONG}", elide = "End" }} }}"#
        ));
        assert_eq!(drawn, "\u{2026}");
    }

    /// Cut at character boundaries, so a multi-byte codepoint is kept or dropped whole rather than
    /// sliced into invalid UTF-8. Panics inside the search if this ever regresses.
    #[test]
    fn a_multibyte_string_is_cut_at_character_boundaries() {
        let drawn = elided(
            r#"panel { id = "bar", child = text { width = 40, content = "ααααααααααααααααααααααααα", elide = "End" } }"#,
        );
        assert!(drawn.ends_with('\u{2026}'));
        assert!(drawn.trim_end_matches('\u{2026}').chars().all(|c| c == 'α'), "no partial codepoints: {drawn:?}");
    }
}
