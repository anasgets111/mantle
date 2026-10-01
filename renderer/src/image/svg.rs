//! SVG rasterization, with `currentColor` tinting for symbolic icons (ADR-0072).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::decode::{read_capped, take_capped};
use crate::layout::node::Rgba;
use crate::text::shaping::FontDatabase;

/// Bytes an SVG source may occupy before it is refused unparsed. `usvg` parses the whole document
/// into a tree with no ceiling of its own, and an icon that is not a few hundred kilobytes is not
/// an icon.
const MAX_SVG_BYTES: u64 = 8 * 1024 * 1024;

/// Rasterizes with longest edge `box_px`; [`fitted_rect`](super::fitted_rect) handles placement.
/// The `bool` says whether the document asked for a font, so only those pixels go stale when the
/// loaded fonts change.
pub(super) fn rasterize_svg(
    path: &Path,
    box_px: u32,
    tint: Option<Rgba>,
    fonts: &FontDatabase,
) -> Result<(Vec<u8>, u32, u32, bool), String> {
    // Read through a limited reader rather than checking `metadata` and then reading: the file can
    // grow between the two, and the read is what allocates. `usvg` parses whatever it is handed
    // into a tree with no ceiling of its own (see `MAX_SVG_BYTES`).
    let data = read_capped(path, MAX_SVG_BYTES)
        .map_err(|err| format!("{}: {err}", path.display()))?
        .ok_or_else(|| format!("svg is over the {MAX_SVG_BYTES}-byte limit and was not parsed"))?;
    // gzip magic. Inflated here rather than by `usvg::decompress_svgz`, whose `read_to_end` has no
    // ceiling: deflate reaches 1032:1, so a file that passed the cap above can still ask for 8GB.
    // Also what puts plaintext in front of `tinted_svg`, which rewrites bytes.
    let data = match data.starts_with(&[0x1f, 0x8b]) {
        true => take_capped(flate2::read::GzDecoder::new(&data[..]), MAX_SVG_BYTES)
            .map_err(|err| format!("{}: {err}", path.display()))?
            .ok_or_else(|| format!("svgz inflates past the {MAX_SVG_BYTES}-byte limit and was not parsed"))?,
        false => data,
    };
    let data = match tint {
        Some(tint) => tinted_svg(&data, tint),
        None => data,
    };
    // ponytail: SVG text uses the fonts already loaded by Mantle. Assets needing other families
    // declare them in fonts {}; runtime discovery would need to join the shaping worker's queue.
    // Asked once per text span, nested SVG images included, even when no face matches. The tree
    // cannot answer instead: usvg drops text it could not lay out, which a later font may fix.
    let asked_for_a_font = AtomicBool::new(false);
    let select_font = resvg::usvg::FontResolver::default_font_selector();
    let options = resvg::usvg::Options {
        fontdb: fonts.db.clone(),
        font_family: fonts.primary_family.clone(),
        font_resolver: resvg::usvg::FontResolver {
            select_font: Box::new(|font, db| {
                asked_for_a_font.store(true, Ordering::Relaxed);
                select_font(font, db)
            }),
            select_fallback: resvg::usvg::FontResolver::default_fallback_selector(),
        },
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_data(&data, &options).map_err(|err| err.to_string())?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    // Check finiteness as well as sign: `<= 0.0` lets NaN reach `scale` and a zero-sized pixmap,
    // producing a worse error farther from the cause.
    if !longest.is_finite() || longest <= 0.0 {
        return Err(format!("svg declares a {}x{} viewport", size.width(), size.height()));
    }
    let scale = box_px as f32 / longest;
    let width = ((size.width() * scale).round() as u32).max(1);
    let height = ((size.height() * scale).round() as u32).max(1);
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).ok_or_else(|| format!("no pixmap for {width}x{height}"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Ok((pixmap.take(), width, height, asked_for_a_font.load(Ordering::Relaxed)))
}

/// `0x00RRGGBB` for [`CacheKey`](super::CacheKey). Drop alpha: CSS `color` is `#RRGGBB`; draw-call `alpha` owns
/// icon transparency, not the SVG.
pub(super) fn packed_rgb(color: Rgba) -> u32 {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(color.r) << 16) | (channel(color.g) << 8) | channel(color.b)
}

/// Replace `currentColor` with `tint`, or leave data untouched without one (ADR-0072). Symbolic
/// icons use two shapes. Breeze/Adwaita ship `<style id="current-color-scheme">` with
/// `color:#232629` on each path's class; Plasma rewrites it at load, and so does this, avoiding
/// near-black icons on dark bars. The other shape has `fill="currentColor"` and no `color`, whose
/// CSS initial value is black; a root presentation attribute wins, while a defined root `color`
/// is handled by the first pass. Rewrite bytes, not a parse, because `usvg` resolves
/// `currentColor` while building with no earlier hook. `from_utf8`, not lossy conversion, lets
/// `usvg` reject non-UTF-8 input itself.
fn tinted_svg(data: &[u8], tint: Rgba) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(data) else {
        return data.to_vec();
    };
    if !text.contains("currentColor") {
        return data.to_vec();
    }
    let hex = format!("#{:06x}", packed_rgb(tint));
    let rewritten = rewrite_color_declarations(text, &hex);
    match rewritten.find("<svg") {
        Some(at) => {
            let mut out = String::with_capacity(rewritten.len() + hex.len() + 10);
            out.push_str(&rewritten[..at + 4]);
            out.push_str(&format!(" color=\"{hex}\""));
            out.push_str(&rewritten[at + 4..]);
            out.into_bytes()
        }
        None => rewritten.into_bytes(),
    }
}

/// Repoint bare CSS `color:` declarations to `hex`, not `stop-color`, `flood-color`, or
/// `lighting-color`, whose paints are not `currentColor`; changing them flattens gradients. The
/// preceding character must not continue an identifier. ponytail: textual matching also rewrites
/// `color:` inside XML comments or attribute values. Upgrade: parse the `<style>` body with a CSS
/// pass, a parser this crate does not want.
fn rewrite_color_declarations(text: &str, hex: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("color:") {
        let after = at + "color:".len();
        let continues_an_identifier =
            rest[..at].chars().next_back().is_some_and(|c| c == '-' || c == '_' || c.is_alphanumeric());
        out.push_str(&rest[..after]);
        if continues_an_identifier {
            rest = &rest[after..];
            continue;
        }
        // Replace the whole value, so spaced and unspaced declarations behave the same.
        let value_len = rest[after..].find([';', '}', '"', '\'']).unwrap_or(rest.len() - after);
        out.push_str(hex);
        rest = &rest[after + value_len..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::tests::GRADIENT_SVG;
    use super::*;

    pub(crate) fn fixture_shaping() -> crate::text::shaping::ShapingHandle {
        let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/shots/fonts/fonts.conf");
        let shaping = crate::text::shaping::ShapingHandle::spawn_with(Some(config));
        shaping.set_chain(&["Noto Sans".into()]);
        shaping
    }

    #[test]
    fn svg_text_uses_the_configured_faces_and_survives_chain_replacement() {
        let shaping = fixture_shaping();
        let fonts = shaping.font_database();
        assert_eq!(fonts.primary_family, "Noto Sans");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text.svg");
        let render = |attrs: &str, snapshot: &FontDatabase| {
            std::fs::write(&path, format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 240 60"><text x="4" y="45" font-size="40" {attrs}>Mantle ffi</text></svg>"#
            )).unwrap();
            rasterize_svg(&path, 240, None, snapshot).unwrap().0
        };
        let plain = render("", &fonts);
        let (_, _, _, text) = rasterize_svg(&path, 240, None, &FontDatabase::default()).unwrap();
        assert!(text, "text no loaded face could draw still waits on a later font");
        assert!(plain.as_chunks::<4>().0.iter().filter(|p| p[3] > 0).count() > 500);
        assert_eq!(plain, render(r#"font-family="Noto Sans""#, &fonts));
        assert_eq!(plain, render(r#"font-family="Missing Fixture Family""#, &fonts));
        assert_ne!(plain, render(r#"font-weight="700""#, &fonts));
        assert_ne!(plain, render(r#"font-style="italic""#, &fonts));
        shaping.set_chain(&["Noto Sans Symbols 2".into()]);
        let replacement = shaping.font_database();
        assert!(replacement.generation > fonts.generation);
        assert_eq!(replacement.primary_family, "Noto Sans Symbols 2");
        assert_ne!(plain, render("", &replacement));
        drop(shaping);
        assert_eq!(plain, render("", &fonts), "a queued decode retains its original mapped faces");
    }

    #[test]
    fn svg_generic_families_keep_the_declared_alias_and_serif_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("fonts.conf");
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/shots/fonts");
        std::fs::write(
            &config,
            format!(
                r#"<fontconfig><dir>{}</dir>
            <alias binding="strong"><family>monospace</family><prefer><family>Noto Sans Symbols 2</family></prefer></alias>
            <alias binding="strong"><family>serif</family><prefer><family>Noto Sans Symbols 2</family></prefer></alias>
            </fontconfig>"#,
                fixtures.display()
            ),
        )
        .unwrap();
        let shaping = crate::text::shaping::ShapingHandle::spawn_with(Some(config));
        shaping.set_chain(&["Noto Sans".into(), "monospace".into(), "serif".into()]);
        let fonts = shaping.font_database();
        let named = fonts
            .db
            .query(&fontdb::Query { families: &[fontdb::Family::Name("Noto Sans Symbols 2")], ..Default::default() })
            .unwrap();
        for family in [fontdb::Family::Monospace, fontdb::Family::Serif] {
            assert_eq!(fonts.db.query(&fontdb::Query { families: &[family], ..Default::default() }), Some(named));
        }
        let svg = dir.path().join("text.svg");
        let render = |family: &str| {
            std::fs::write(&svg, format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 240 60"><text x="4" y="45" font-size="40" font-family="{family}">Mantle</text></svg>"#)).unwrap();
            rasterize_svg(&svg, 240, None, &fonts).unwrap().0
        };
        assert_eq!(render("Missing Fixture Family"), render("Noto Sans Symbols 2"));
        assert_eq!(render("monospace"), render("Noto Sans Symbols 2"));
        assert_eq!(render("serif"), render("Noto Sans Symbols 2"));
    }

    #[test]
    fn svg_text_obeys_current_color_and_svg_masks() {
        let fonts = fixture_shaping().font_database();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mask.svg");
        std::fs::write(
            &path,
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 240 60">
            <defs><mask id="letters"><text x="4" y="45" font-size="40" fill="white">Mantle</text></mask></defs>
            <rect width="240" height="60" fill="currentColor" mask="url(#letters)"/>
        </svg>"##,
        )
        .unwrap();
        let red = Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        let (pixels, _, _, text) = rasterize_svg(&path, 240, Some(red), &fonts).unwrap();
        assert!(text, "text inside a mask still depends on the fonts");
        assert!(pixels.as_chunks::<4>().0.iter().filter(|p| p[3] > 0).count() > 500);
        assert!(pixels.as_chunks::<4>().0.iter().all(|p| p[0] == p[3] && p[1] == 0 && p[2] == 0));
        assert_eq!(&pixels[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn a_kde_symbolic_icon_is_recoloured_through_its_own_stylesheet() {
        // Telegram's Breeze *Light* file bakes its text colour; without toolkit rewriting it is
        // near-black on a dark bar.
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 22 22">
  <defs><style id="current-color-scheme" type="text/css">
      .ColorScheme-Text { color:#232629; }
  </style></defs>
  <path class="ColorScheme-Text" style="fill:currentColor" d="M0 0h1v1h-1z"/>
</svg>"##;
        let out = String::from_utf8(tinted_svg(svg, tint())).unwrap();
        assert!(out.contains("color:#cdd6f4"), "the stylesheet's own declaration is repointed: {out}");
        assert!(!out.contains("#232629"), "and the shipped colour is gone: {out}");
    }

    #[test]
    fn an_icon_that_defines_no_colour_gets_one_on_the_root() {
        // hicolor and Adwaita ship this shape. CSS's initial `color` is black, so the root
        // attribute is required for the caller's tint.
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg"><path fill="currentColor" d="M0 0h1v1h-1z"/></svg>"##;
        let out = String::from_utf8(tinted_svg(svg, tint())).unwrap();
        assert!(out.starts_with(r##"<svg color="#cdd6f4""##), "the root carries the colour: {out}");
    }

    #[test]
    fn a_full_colour_icon_is_handed_back_byte_for_byte() {
        // App icons receive `foreground`; only symbolic icons may change, or a themed Slack logo
        // would flatten.
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg"><path fill="#2eb67d" d="M0 0h1v1h-1z"/></svg>"##;
        assert_eq!(tinted_svg(svg, tint()), svg.to_vec());
    }

    #[test]
    fn a_gradient_stop_is_not_a_colour_declaration() {
        // `stop-color:` contains `color:`; rewriting it flattens every gradient.
        let out = rewrite_color_declarations("stop-color:#ff0000;color:#232629;flood-color:#00ff00", "#cdd6f4");
        assert_eq!(out, "stop-color:#ff0000;color:#cdd6f4;flood-color:#00ff00");
    }

    #[test]
    fn a_spaced_declaration_is_replaced_whole_rather_than_prefixed() {
        // The value runs to its terminator, so `color: #232629 ` is replaced whole; CSS ignores
        // the internal whitespace and needs no trimming rules.
        assert_eq!(rewrite_color_declarations("{ color: #232629 }", "#cdd6f4"), "{ color:#cdd6f4}");
    }
    #[test]
    fn a_tint_packs_to_rgb_and_drops_alpha() {
        assert_eq!(packed_rgb(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 0.25 }), 0xff0000);
        assert_eq!(format!("#{:06x}", packed_rgb(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 })), "#0000ff");
    }

    /// `#cdd6f4`, so a test asserting on the hex asserts on a value it can read.
    fn tint() -> Rgba {
        Rgba { r: 0xcd as f32 / 255.0, g: 0xd6 as f32 / 255.0, b: 0xf4 as f32 / 255.0, a: 1.0 }
    }
    #[test]
    fn an_svg_rasterizes_opaque_to_its_longest_edge_keeping_its_aspect_ratio() {
        // Exercises resvg (ADR-0055): a tree parsing to nothing renders a transparent pixmap rather
        // than an error, so "it did not fail" proves nothing on its own. A fixture rather than the
        // shipped wallpaper, whose art is free to change without breaking an engine test.
        let dir = tempfile::tempdir().unwrap();
        let svg = dir.path().join("gradient.svg");
        std::fs::write(&svg, GRADIENT_SVG).unwrap();
        let (pixels, width, height, text) =
            rasterize_svg(&svg, 128, None, &FontDatabase::default()).expect("the fixture should parse");
        assert!(!text, "a text-free document survives a font change");
        // 1920x1080 viewBox, longest edge 128, preserves the aspect ratio.
        assert_eq!((width, height), (128, 72));
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        let opaque = pixels.as_chunks::<4>().0.iter().filter(|px| px[3] > 0).count();
        assert_eq!(opaque, (width * height) as usize, "the fill covers its whole viewBox");
        // More than one colour proves the gradient survived rather than flattening to its first stop.
        let distinct: std::collections::HashSet<[u8; 3]> =
            pixels.as_chunks::<4>().0.iter().map(|px| [px[0], px[1], px[2]]).collect();
        assert!(distinct.len() > 16, "expected a gradient, got {} colours", distinct.len());
    }

    #[test]
    fn a_gzipped_svgz_rasterizes_to_the_same_pixels_as_its_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("gradient.svg");
        let zipped = dir.path().join("gradient.svgz");
        std::fs::write(&plain, GRADIENT_SVG).unwrap();
        std::fs::write(&zipped, gzip(GRADIENT_SVG.as_bytes())).unwrap();
        let fonts = FontDatabase::default();
        assert_eq!(rasterize_svg(&zipped, 64, None, &fonts).unwrap(), rasterize_svg(&plain, 64, None, &fonts).unwrap());
    }

    #[test]
    fn an_svgz_that_inflates_past_the_limit_is_refused_rather_than_allocated_for() {
        // Deflate reaches 1032:1, so the read cap alone bounds only the file on disk: this one is
        // a few kilobytes and asks for 8MB+1. `usvg::decompress_svgz` would hand over all of it.
        let dir = tempfile::tempdir().unwrap();
        let bomb = dir.path().join("bomb.svgz");
        std::fs::write(&bomb, gzip(&vec![b' '; MAX_SVG_BYTES as usize + 1])).unwrap();
        assert!(std::fs::metadata(&bomb).unwrap().len() < MAX_SVG_BYTES, "the compressed file passes the read cap");
        let err =
            rasterize_svg(&bomb, 24, None, &FontDatabase::default()).expect_err("an inflating svgz must be refused");
        assert!(err.contains("inflates past"), "the refusal should say why: {err}");
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn an_oversized_svg_is_refused_before_it_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let bloated = dir.path().join("huge.svg");
        // One byte over, so the refusal is the size check and not a parse failure.
        std::fs::write(&bloated, vec![b' '; MAX_SVG_BYTES as usize + 1]).unwrap();
        let err =
            rasterize_svg(&bloated, 24, None, &FontDatabase::default()).expect_err("an oversized svg must be refused");
        assert!(err.contains("over the"), "the refusal should say why: {err}");
    }
}
