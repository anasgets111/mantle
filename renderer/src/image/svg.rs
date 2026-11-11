//! SVG rasterization, with `currentColor` tinting for symbolic icons (ADR-0072).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use resvg::usvg::{ImageHrefResolver, ImageKind, Options, Tree};

use shared::warn;

use super::MAX_DECODE_EDGE;
use super::decode::{plausible, read_capped, take_capped};
use crate::layout::node::{Rgba, byte};
use crate::text::shaping::FontDatabase;

/// Bytes an SVG source may occupy before it is refused unparsed. `usvg` parses the whole document
/// into a tree with no ceiling of its own, and an icon that is not a few hundred kilobytes is not
/// an icon.
const MAX_SVG_BYTES: u64 = 8 * 1024 * 1024;

/// SVG `<image>` data URLs one document may nest. Each level recurses through usvg and resvg up to
/// 1024 `<g>` deep, and two levels overflowed the main thread's 8 MiB stack in a release build.
const MAX_SVG_NESTING: u32 = 1;

/// Element depth, and `clipPath`, `mask` and `marker` elements, one document may hold. usvg and
/// resvg recurse once per level and per link (a link names an element, so their count bounds any
/// chain however CSS routes it), with no limit on `<tspan>` or links. The deepest installed icon is
/// 10 deep; the most linked holds 187 (Adwaita `user-trash-full`).
const MAX_SVG_DEPTH: usize = 256;
const MAX_SVG_LINKS: usize = 256;
/// A pattern link costs about 8 KiB of stack in release, four times a clip's; icons use 2 at most.
const MAX_SVG_PATTERNS: usize = 16;

/// Rasterizes with longest edge `box_px`; [`fitted_rect`](super::fitted_rect) handles placement.
/// The `bool` says whether the document asked for a font, so only those pixels go stale when the
/// loaded fonts change; the last field is the document's own size, rounded.
pub(super) fn rasterize_svg(
    path: &Path,
    box_px: u32,
    tint: Option<Rgba>,
    fonts: &FontDatabase,
) -> Result<(Vec<u8>, u32, u32, bool, super::Natural), String> {
    // Read through a limited reader rather than checking `metadata` and then reading: the file can
    // grow between the two, and the read is what allocates. `usvg` parses whatever it is handed
    // into a tree with no ceiling of its own (see `MAX_SVG_BYTES`).
    let data = read_capped(path, MAX_SVG_BYTES)
        .map_err(|err| format!("{}: {err}", path.display()))?
        .ok_or_else(|| format!("svg is over the {MAX_SVG_BYTES}-byte limit and was not parsed"))?;
    // Inflated before `tinted_svg`, which rewrites bytes.
    let data = plaintext(data, MAX_SVG_BYTES)
        .map_err(|err| format!("{}: {err}", path.display()))?
        .ok_or_else(|| format!("svgz inflates past the {MAX_SVG_BYTES}-byte limit and was not parsed"))?;
    // GTK's recolour for a symbolic icon without `currentColor` (Tela): `!important` beats attributes
    // and inline `style`. One with it (Adwaita) keeps its hard-coded accents.
    // ponytail: `<mask>` fills and `fill="none"` stroke-only shapes get filled too, as GTK does.
    let hard_coded = is_symbolic(path) && !uses_current_color(&data);
    let data = match tint {
        Some(tint) => tinted_svg(&data, tint),
        None => data,
    };
    let style_sheet = tint
        .filter(|_| hard_coded)
        .map(|tint| format!("rect,circle,ellipse,path,polygon,polyline{{fill:#{:06x} !important}}", packed_rgb(tint)));
    // ponytail: SVG text uses the fonts already loaded by Mantle. Assets needing other families
    // declare them in fonts {}; runtime discovery would need to join the shaping worker's queue.
    // Asked once per text span, nested SVG images included, even when no face matches. The tree
    // cannot answer instead: usvg drops text it could not lay out, which a later font may fix.
    let asked_for_a_font = AtomicBool::new(false);
    let select_font = resvg::usvg::FontResolver::default_font_selector();
    let (nested_budget, depth) = (AtomicU64::new(MAX_SVG_BYTES), AtomicU32::new(0));
    // resvg decodes every embedded raster `Image`, `<use>` copies included, at its native size.
    let pixels = AtomicU64::new(u64::from(MAX_DECODE_EDGE).pow(2));
    let named_a_file = AtomicBool::new(false);
    let options = Options {
        fontdb: fonts.db.clone(),
        font_family: fonts.primary_family.clone(),
        font_resolver: resvg::usvg::FontResolver {
            select_font: Box::new(|font, db| {
                asked_for_a_font.store(true, Ordering::Relaxed);
                select_font(font, db)
            }),
            select_fallback: resvg::usvg::FontResolver::default_fallback_selector(),
        },
        style_sheet,
        // Only `data:` hrefs (ADR-0384): usvg's default reads any path whole, `/dev/zero` or a
        // FIFO included. Its nested parse already refuses file hrefs below the top level.
        image_href_resolver: ImageHrefResolver {
            resolve_data: Box::new(|mime, data, options| {
                embedded_image(mime, data, options, &nested_budget, &pixels, &depth)
            }),
            resolve_string: Box::new(|_, _| {
                named_a_file.store(true, Ordering::Relaxed);
                None
            }),
        },
        ..Default::default()
    };
    if !within_recursion_limits(&data) {
        return Err(format!(
            "svg nests past {MAX_SVG_DEPTH} elements, links past its limits, repeats an image or overflows a filter"
        ));
    }
    let tree = Tree::from_data(&data, &options).map_err(|err| err.to_string())?;
    if named_a_file.load(Ordering::Relaxed) {
        warn!("{:?}: an <image> href that is not a data: URL draws nothing; embed the picture as one", path);
    }
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
    let natural = (size.width().round() as u32, size.height().round() as u32);
    Ok((pixmap.take(), width, height, asked_for_a_font.load(Ordering::Relaxed), natural))
}

/// `data`, inflated if it has the gzip magic, or `None` past `cap`. Inflated here rather than by
/// `usvg::decompress_svgz`, whose `read_to_end` has no ceiling: deflate reaches 1032:1. A second
/// gzip layer is refused, since `Tree::from_data` would hand it to that same `read_to_end`.
fn plaintext(data: Vec<u8>, cap: u64) -> std::io::Result<Option<Vec<u8>>> {
    const GZIP: [u8; 2] = [0x1f, 0x8b];
    match data.starts_with(&GZIP) {
        true => Ok(take_capped(flate2::read::GzDecoder::new(&data[..]), cap)?.filter(|svg| !svg.starts_with(&GZIP))),
        false => Ok((data.len() as u64 <= cap).then_some(data)),
    }
}

/// usvg's default data resolver under a file's caps. A raster is typed by the header `image`
/// sniffs, never the MIME, so the header checked is the one decoded: its edges must pass
/// [`plausible`] and its area comes out of `pixels`. Nested SVG draws on one
/// `budget`, which a refused inflation empties: an image repeated at each level, or by `<use>`,
/// would otherwise multiply what one document can ask for. `depth` counts the nested parses in
/// progress, up to [`MAX_SVG_NESTING`].
#[allow(clippy::rc_buffer)] // usvg's `ImageHrefDataResolverFn` hands over an `Arc<Vec<u8>>`.
fn embedded_image(
    mime: &str,
    data: Arc<Vec<u8>>,
    options: &Options,
    budget: &AtomicU64,
    pixels: &AtomicU64,
    depth: &AtomicU32,
) -> Option<ImageKind> {
    use ::image::ImageFormat;
    let header = ::image::ImageReader::new(std::io::Cursor::new(data.as_slice())).with_guessed_format().ok()?;
    let Some(format) = header.format() else {
        if !matches!(mime, "image/svg+xml" | "text/plain") || depth.load(Ordering::Relaxed) == MAX_SVG_NESTING {
            return None;
        }
        let Some(svg) = plaintext(Arc::unwrap_or_clone(data), budget.load(Ordering::Relaxed)).ok().flatten() else {
            budget.store(0, Ordering::Relaxed);
            return None;
        };
        budget.fetch_sub(svg.len() as u64, Ordering::Relaxed);
        if !within_recursion_limits(&svg) {
            return None;
        }
        depth.fetch_add(1, Ordering::Relaxed);
        let tree = Tree::from_data_nested(&svg, options);
        depth.fetch_sub(1, Ordering::Relaxed);
        return tree.ok().map(ImageKind::SVG);
    };
    let kind = match format {
        ImageFormat::Png => ImageKind::PNG,
        ImageFormat::Jpeg => ImageKind::JPEG,
        ImageFormat::Gif => ImageKind::GIF,
        ImageFormat::WebP => ImageKind::WEBP,
        _ => return None,
    };
    let (mut width, mut height) = header.into_dimensions().ok()?;
    if format == ImageFormat::Gif {
        // resvg allocates the first frame at its own size, which the gif crate never fits to the
        // screen `image` reports.
        let mut gif = gif::DecodeOptions::new().read_info(data.as_slice()).ok()?;
        let frame = gif.next_frame_info().ok()??;
        width = width.max(u32::from(frame.left) + u32::from(frame.width));
        height = height.max(u32::from(frame.top) + u32::from(frame.height));
    }
    let (width, height) = plausible((width, height))?;
    let area = u64::from(width) * u64::from(height);
    pixels.try_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(area)).ok()?;
    Some(kind(data))
}

/// Whether `svg` stays inside [`MAX_SVG_DEPTH`], [`MAX_SVG_LINKS`] and [`MAX_SVG_PATTERNS`],
/// walked without recursion, converts each embedded image once and overflows no filter integer.
/// Parsed as usvg parses it, so entity-expanded elements count; a document that does not parse is
/// left for usvg to refuse.
///
/// usvg decodes an image's href, base64 and all, per conversion: per `<use>` copy, marker vertex,
/// or shape under an `objectBoundingBox` mask, clip or filter. So none may sit in those, nor in
/// what an `href` names (svgtypes' IRI: spaces, `#`, up to a space); none installed does.
fn within_recursion_limits(svg: &[u8]) -> bool {
    let options = resvg::usvg::roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let Some(doc) = std::str::from_utf8(svg)
        .ok()
        .and_then(|text| resvg::usvg::roxmltree::Document::parse_with_options(text, options).ok())
    else {
        return true;
    };
    let linked: std::collections::HashSet<&str> = doc
        .descendants()
        .flat_map(|node| node.attributes())
        .filter(|attr| attr.name() == "href")
        .filter_map(|attr| attr.value().trim_start().strip_prefix('#')?.split(' ').next())
        .collect();
    let (mut depths, mut links, mut patterns) = (Vec::new(), 0, 0);
    for node in doc.descendants().filter(|node| node.is_element()) {
        let refused = match node.tag_name().name() {
            "image" => node.ancestors().any(|node| {
                matches!(node.tag_name().name(), "mask" | "pattern" | "clipPath" | "marker" | "filter")
                    || node.attribute("id").is_some_and(|id| linked.contains(id))
            }),
            "feImage" => {
                node.attributes().any(|attr| attr.name() == "href" && !attr.value().trim_start().starts_with('#'))
            }
            // usvg multiplies `order`'s components and resvg negates the seed unchecked, which
            // aborts the renderer. Kernels are 3 or 5 wide in practice; no installed icon has one.
            "feConvolveMatrix" => past(node, "order", 32.0),
            "feTurbulence" => past(node, "seed", 1e9),
            _ => false,
        };
        let parent = node.parent().and_then(|parent| depths.get(parent.id().get_usize())).copied();
        let depth = parent.unwrap_or(0) + 1;
        depths.resize(depths.len().max(node.id().get_usize() + 1), 0);
        depths[node.id().get_usize()] = depth;
        match node.tag_name().name() {
            "clipPath" | "mask" | "marker" => links += 1,
            "pattern" => patterns += 1,
            _ => {}
        }
        if refused || depth > MAX_SVG_DEPTH || links > MAX_SVG_LINKS || patterns > MAX_SVG_PATTERNS {
            return false;
        }
    }
    true
}

/// Whether `node`'s `attr` holds a number past `bound`, or one this cannot read: split on signs and
/// separators, so `7e4+7e4`, which svgtypes reads as two numbers, is two here.
fn past(node: resvg::usvg::roxmltree::Node, attr: &str, bound: f64) -> bool {
    let values = node.attributes().filter(|named| named.name() == attr).map(|named| named.value());
    let pieces = values.flat_map(|value| value.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.')));
    pieces.filter(|piece| !piece.is_empty()).any(|piece| !piece.parse::<f64>().is_ok_and(|n| n <= bound))
}

/// A `-symbolic` file name, or a theme's `symbolic/` directory (Adwaita), as GTK decides it.
fn is_symbolic(path: &Path) -> bool {
    path.file_stem().is_some_and(|stem| stem.as_encoded_bytes().ends_with(b"-symbolic"))
        || path.components().any(|part| part.as_os_str() == "symbolic")
}

/// `0x00RRGGBB` for [`CacheKey`](super::CacheKey). Drop alpha: CSS `color` is `#RRGGBB`; draw-call `alpha` owns
/// icon transparency, not the SVG.
pub(super) fn packed_rgb(color: Rgba) -> u32 {
    let channel = |v: f32| u32::from(byte(v));
    (channel(color.r) << 16) | (channel(color.g) << 8) | channel(color.b)
}

/// CSS keywords are case-insensitive.
fn uses_current_color(data: &[u8]) -> bool {
    const KEYWORD: &[u8] = b"currentColor";
    data.windows(KEYWORD.len()).any(|w| w.eq_ignore_ascii_case(KEYWORD))
}

/// Replace `currentColor` with `tint`, or leave data untouched without one (ADR-0072). Symbolic
/// icons use two shapes. Breeze/Adwaita ship `<style id="current-color-scheme">` with
/// `color:#232629` on each path's class; Plasma rewrites it at load, and so does this, avoiding
/// near-black icons on dark bars. The other shape has `fill="currentColor"` and no `color`, whose
/// CSS initial value is black; a root presentation attribute wins, replacing one the root already
/// has. Rewrite bytes, not a parse, because `usvg` resolves `currentColor` while building with no
/// earlier hook. `from_utf8`, not lossy conversion, lets `usvg` reject non-UTF-8 input itself.
fn tinted_svg(data: &[u8], tint: Rgba) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(data) else {
        return data.to_vec();
    };
    if !uses_current_color(data) {
        return data.to_vec();
    }
    let hex = format!("#{:06x}", packed_rgb(tint));
    let rewritten = rewrite_color_declarations(text, &hex);
    let Some(at) = rewritten.find("<svg") else { return rewritten.into_bytes() };
    let tag = &rewritten[at..at + rewritten[at..].find('>').unwrap_or(rewritten.len() - at)];
    // A second `color` on the root is a duplicate attribute, which fails the whole parse.
    let (start, end, value) = match root_color_value(tag) {
        Some(value) => (at + value.start, at + value.end, hex),
        None => (at + 4, at + 4, format!(" color=\"{hex}\"")),
    };
    let mut out = String::with_capacity(rewritten.len() + value.len());
    out.push_str(&rewritten[..start]);
    out.push_str(&value);
    out.push_str(&rewritten[end..]);
    out.into_bytes()
}

/// The byte range of the quoted value of `tag`'s own `color` attribute, not `stop-color` or
/// `flood-color`.
fn root_color_value(tag: &str) -> Option<std::ops::Range<usize>> {
    tag.match_indices("color").find_map(|(at, _)| {
        tag[..at].ends_with(char::is_whitespace).then_some(())?;
        let rest = tag[at + "color".len()..].trim_start().strip_prefix('=')?.trim_start();
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let start = tag.len() - rest.len() + 1;
        Some(start..start + tag[start..].find(quote)?)
    })
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
    use super::super::decode::DECODE_STACK_BYTES;
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
        let (_, _, _, text, _) = rasterize_svg(&path, 240, None, &FontDatabase::default()).unwrap();
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
        let (pixels, _, _, text, _) = rasterize_svg(&path, 240, Some(red), &fonts).unwrap();
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
    fn only_a_symbolic_icon_without_current_color_recolours_its_hard_coded_fills() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 2 2"><path fill="#222" style="fill:#333" d="M0 0h2v2h-2z"/></svg>"##;
        let dir = tempfile::tempdir().unwrap();
        let render = |name: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, svg).unwrap();
            rasterize_svg(&path, 2, Some(tint()), &FontDatabase::default()).unwrap().0
        };
        assert_eq!(&render("mic-symbolic.svg")[..4], &[0xcd, 0xd6, 0xf4, 0xff]);
        assert_eq!(&render("slack.svg")[..4], &[0x33, 0x33, 0x33, 0xff]);
        let accent = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 2 2" fill="currentColor"><path fill="#ff7800" d="M0 0h2v2h-2z"/></svg>"##;
        let path = dir.path().join("caution-symbolic.svg");
        std::fs::write(&path, accent).unwrap();
        let pixels = rasterize_svg(&path, 2, Some(tint()), &FontDatabase::default()).unwrap().0;
        assert_eq!(&pixels[..4], &[0xff, 0x78, 0x00, 0xff], "a currentColor icon keeps its accent");
        let path = dir.path().join("lower-symbolic.svg");
        std::fs::write(&path, accent.replace("currentColor", "currentcolor")).unwrap();
        let pixels = rasterize_svg(&path, 2, Some(tint()), &FontDatabase::default()).unwrap().0;
        assert_eq!(&pixels[..4], &[0xff, 0x78, 0x00, 0xff], "`currentcolor` is the same keyword");
        assert!(is_symbolic(Path::new("/i/Adwaita/symbolic/apps/x.svg")));
        assert!(!is_symbolic(Path::new("/i/hicolor/apps/symbolic-ish.svg")));
    }

    #[test]
    fn a_root_colour_is_added_when_missing_and_replaced_rather_than_duplicated_when_present() {
        // hicolor and Adwaita ship this shape. CSS's initial `color` is black, so the root
        // attribute is required for the caller's tint.
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg"><path fill="currentColor" d="M0 0h1v1h-1z"/></svg>"##;
        let out = String::from_utf8(tinted_svg(svg, tint())).unwrap();
        assert!(out.starts_with(r##"<svg color="#cdd6f4""##), "the root carries the colour: {out}");

        // An existing root colour is replaced rather than duplicated.
        let svg = br##"<svg stop-color="#00ff00" color='#232629' xmlns="http://www.w3.org/2000/svg"><path fill="currentColor" d="M0 0h1v1h-1z"/></svg>"##;
        let out = String::from_utf8(tinted_svg(svg, tint())).unwrap();
        assert!(out.starts_with(r##"<svg stop-color="#00ff00" color='#cdd6f4' xmlns"##), "{out}");
        assert!(resvg::usvg::Tree::from_str(&out, &resvg::usvg::Options::default()).is_ok(), "{out}");
    }

    #[test]
    fn a_full_colour_icon_is_handed_back_byte_for_byte() {
        // App icons receive `foreground`; only symbolic icons may change, or a themed Slack logo
        // would flatten.
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg"><path fill="#2eb67d" d="M0 0h1v1h-1z"/></svg>"##;
        assert_eq!(tinted_svg(svg, tint()), svg.to_vec());
    }

    #[test]
    fn color_declarations_are_replaced_whole_and_a_gradient_stop_is_left_alone() {
        // `stop-color:` contains `color:`; rewriting it flattens every gradient. The value runs to
        // its terminator, so `color: #232629 ` is replaced whole, with no trimming rules.
        for (input, expected) in [
            (
                "stop-color:#ff0000;color:#232629;flood-color:#00ff00",
                "stop-color:#ff0000;color:#cdd6f4;flood-color:#00ff00",
            ),
            ("{ color: #232629 }", "{ color:#cdd6f4}"),
        ] {
            assert_eq!(rewrite_color_declarations(input, "#cdd6f4"), expected, "{input}");
        }
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
        let (pixels, width, height, text, natural) =
            rasterize_svg(&svg, 128, None, &FontDatabase::default()).expect("the fixture should parse");
        assert!(!text, "a text-free document survives a font change");
        // 1920x1080 viewBox, longest edge 128, preserves the aspect ratio.
        assert_eq!((width, height), (128, 72));
        assert_eq!(natural, (1920, 1080), "the document's own size, not the raster's");
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        let opaque = pixels.as_chunks::<4>().0.iter().filter(|px| px[3] > 0).count();
        assert_eq!(opaque, (width * height) as usize, "the fill covers its whole viewBox");
        // More than one colour proves the gradient survived rather than flattening to its first stop.
        let distinct: std::collections::HashSet<[u8; 3]> =
            pixels.as_chunks::<4>().0.iter().map(|px| [px[0], px[1], px[2]]).collect();
        assert!(distinct.len() > 16, "expected a gradient, got {} colours", distinct.len());
    }

    const RED_SVG: &str =
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" fill="red"/></svg>"#;

    fn red_png(width: u32, height: u32) -> Vec<u8> {
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.write_header().unwrap().write_image_data(&[255, 0, 0, 255].repeat((width * height) as usize)).unwrap();
        png
    }

    /// Percent-encoded rather than base64, which this crate has no encoder for.
    fn data_url(mime: &str, bytes: &[u8]) -> String {
        use std::fmt::Write;
        bytes.iter().fold(format!("data:{mime},"), |mut url, byte| {
            write!(url, "%{byte:02x}").unwrap();
            url
        })
    }

    fn image_doc(href: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><image href="{href}" width="2" height="2" preserveAspectRatio="none"/></svg>"#
        )
    }

    fn rasterize_doc(doc: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.svg");
        std::fs::write(&path, doc).unwrap();
        rasterize_svg(&path, 4, None, &FontDatabase::default()).unwrap().0
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const CLEAR: [u8; 4] = [0; 4];

    /// The top-left pixel of each `href`, drawn 2x2 side by side in one document.
    fn side_by_side(hrefs: &[String]) -> Vec<[u8; 4]> {
        let images: String = hrefs
            .iter()
            .enumerate()
            .map(|(i, href)| format!(r#"<image href="{href}" x="{}" width="2" height="2"/>"#, 2 * i))
            .collect();
        let width = 2 * hrefs.len();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("row.svg");
        let doc = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="2">{images}</svg>"#);
        std::fs::write(&path, doc).unwrap();
        let pixels = rasterize_svg(&path, width as u32, None, &FontDatabase::default()).unwrap().0;
        (0..hrefs.len()).map(|i| pixels.as_chunks::<4>().0[2 * i]).collect()
    }

    fn all_red(pixels: &[u8]) -> bool {
        pixels.as_chunks::<4>().0.iter().all(|px| *px == [255, 0, 0, 255])
    }

    fn all_clear(pixels: &[u8]) -> bool {
        pixels.iter().all(|&channel| channel == 0)
    }

    #[test]
    fn an_embedded_png_or_svg_image_is_drawn_from_a_data_url() {
        // Without resvg's `raster-images` the `<image>` parses but draws nothing, leaving it clear.
        for href in [
            data_url("image/png", &red_png(2, 2)),
            data_url("image/svg+xml", RED_SVG.as_bytes()),
            data_url("image/svg+xml", &gzip(RED_SVG.as_bytes())),
            data_url("image/svg+xml", image_doc(&data_url("image/png", &red_png(2, 2))).as_bytes()),
        ] {
            assert!(all_red(&rasterize_doc(&image_doc(&href))), "{href}");
        }
    }

    /// A FIFO href would block the read forever, on the Wayland dispatch thread for an inline
    /// `icon`, and `/dev/zero` would read until memory ran out. Joined within a bound, so a
    /// regression fails here rather than hanging the suite.
    #[test]
    fn an_image_href_naming_a_file_is_never_opened() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.png");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("the test needs a real FIFO");
        let png = dir.path().join("red.png");
        std::fs::write(&png, red_png(2, 2)).unwrap();
        for href in [fifo, png] {
            let doc = image_doc(&href.display().to_string());
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || sender.send(rasterize_doc(&doc)));
            let pixels = receiver.recv_timeout(std::time::Duration::from_secs(10)).expect("the load must not block");
            assert!(all_clear(&pixels), "{} was read", href.display());
        }
    }

    #[test]
    fn a_nested_svg_is_held_to_the_size_cap_across_every_image() {
        // A few kilobytes of gzip inflating past the cap.
        let bomb = gzip(&[RED_SVG.as_bytes(), &vec![b' '; MAX_SVG_BYTES as usize]].concat());

        // Each fits alone, the pair does not: the cap is one budget for the whole document, or
        // repeating an image, at each level, multiplies what one file can ask for.
        let half =
            data_url("image/svg+xml", &gzip(&[RED_SVG.as_bytes(), &vec![b' '; MAX_SVG_BYTES as usize / 2]].concat()));
        assert_eq!(side_by_side(&[half.clone(), half]), [RED, CLEAR], "the second is over the budget");

        // A refused inflation spends the budget it inflated, or `<use>` copies of one bomb, each
        // calling the resolver, would inflate the cap again per copy.
        let refused = data_url("image/svg+xml", &bomb);
        assert_eq!(side_by_side(&[refused, data_url("image/svg+xml", RED_SVG.as_bytes())]), [CLEAR, CLEAR]);
    }

    #[test]
    fn a_gzip_inside_a_gzip_is_refused_at_the_top_level_and_nested() {
        // usvg inflates whatever gzip reaches it with no ceiling, so a second layer skips the cap.
        let twice = gzip(&gzip(RED_SVG.as_bytes()));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twice.svgz");
        std::fs::write(&path, &twice).unwrap();
        assert!(rasterize_svg(&path, 4, None, &FontDatabase::default()).is_err());
        assert!(all_clear(&rasterize_doc(&image_doc(&data_url("image/svg+xml", &twice)))));
    }

    #[test]
    fn sibling_nested_svgs_each_draw() {
        // Siblings sit at the same depth: one that never left would stop the next.
        let sibling = data_url("image/svg+xml", RED_SVG.as_bytes());
        let siblings = vec![sibling; MAX_SVG_NESTING as usize + 1];
        assert_eq!(side_by_side(&siblings), vec![RED; siblings.len()]);
    }

    fn svg(body: &str) -> String {
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">{body}</svg>"#)
    }

    /// `inner` inside `depth` nested `tag` elements.
    fn nested(tag: &str, depth: usize, inner: &str) -> String {
        format!("{}{inner}{}", format!("<{tag}>").repeat(depth), format!("</{tag}>").repeat(depth))
    }

    /// `links` `tag` elements, each but the first naming the previous through `attr`; the last is
    /// `{tag}{links - 1}`. `content` fills each.
    fn chain(tag: &str, attr: &str, links: usize, content: &str) -> String {
        (0..links)
            .map(|i| match i {
                0 => format!(r#"<{tag} id="{tag}0" width="2" height="2" patternUnits="userSpaceOnUse">{content}</{tag}>"#),
                _ => format!(
                    r#"<{tag} id="{tag}{i}" {attr}="url(#{tag}{})" width="2" height="2" patternUnits="userSpaceOnUse">{content}</{tag}>"#,
                    i - 1
                ),
            })
            .collect()
    }

    /// usvg stops `<g>` at 1024 deep, and its own stack ran out first: in release, about 800 deep on
    /// a 2 MiB thread. It has no limit at all on nested `<tspan>` (700 overflowed 2 MiB in debug,
    /// 1000 in release) or on clip-path, mask and pattern links (2000, 2000 and 256 in release).
    #[test]
    fn a_document_past_the_recursion_limits_is_refused_before_usvg_parses_it() {
        let rect = r#"<rect width="2" height="2" fill="red"/>"#;
        let draws = |doc: String| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("deep.svg");
            std::fs::write(&path, &doc).unwrap();
            let thread = std::thread::Builder::new().stack_size(DECODE_STACK_BYTES);
            let drawn = thread.spawn(move || rasterize_svg(&path, 2, None, &FontDatabase::default())).unwrap();
            drawn.join().unwrap().is_ok_and(|(pixels, ..)| all_red(&pixels))
        };
        // The root counts, so the rect is at depth `MAX_SVG_DEPTH`.
        assert!(draws(svg(&nested("g", MAX_SVG_DEPTH - 2, rect))));
        assert!(!draws(svg(&nested("g", MAX_SVG_DEPTH - 1, rect))));
        assert!(!draws(svg(&format!("<text>{}</text>", nested("tspan", 200_000, "x")))));
        // Elements an entity expands to count: usvg parses with a DTD allowed.
        let entity = nested("g", MAX_SVG_DEPTH - 1, rect);
        assert!(!draws(format!("<!DOCTYPE svg [<!ENTITY deep '{entity}'>]>{}", svg("&deep;"))));
        let clip = |links| {
            format!(
                r#"{}<rect width="2" height="2" fill="red" clip-path="url(#clipPath{})"/>"#,
                chain("clipPath", "clip-path", links, rect),
                links - 1
            )
        };
        assert!(draws(svg(&clip(MAX_SVG_LINKS))));
        assert!(!draws(svg(&clip(MAX_SVG_LINKS + 1))));
        let pattern = |links| {
            format!(
                r#"{}<rect width="2" height="2" fill="url(#pattern{})"/>"#,
                chain("pattern", "fill", links, rect),
                links - 1
            )
        };
        assert!(draws(svg(&pattern(MAX_SVG_PATTERNS))));
        assert!(!draws(svg(&pattern(MAX_SVG_PATTERNS + 1))));
        let too_deep = svg(&nested("g", MAX_SVG_DEPTH - 1, rect));
        assert!(all_clear(&rasterize_doc(&image_doc(&data_url("image/svg+xml", too_deep.as_bytes())))));
    }

    /// The worst document the limits accept, on the smallest stack that decodes one: at every level
    /// `<g>` and `<tspan>` as deep as allowed, a pattern chain and a clip chain as long as allowed
    /// at the deepest point, and the nested SVG under the clip chain there: not in the pattern,
    /// where usvg would convert it again.
    #[test]
    fn the_worst_accepted_document_fits_the_smallest_decode_stack() {
        let rect = r#"<rect width="2" height="2" fill="red"/>"#;
        let clipped = |inner: &str| format!(r#"<g clip-path="url(#clipPath{})">{inner}</g>"#, MAX_SVG_LINKS - 1);
        let level = |inner: &str| {
            let painted = format!(r#"<rect width="2" height="2" fill="url(#pattern{})"/>"#, MAX_SVG_PATTERNS - 1);
            svg(&format!(
                r#"<text>{}</text>{}{}{}"#,
                nested("tspan", MAX_SVG_DEPTH - 2, "x"),
                chain("clipPath", "clip-path", MAX_SVG_LINKS, rect),
                chain("pattern", "fill", MAX_SVG_PATTERNS, &clipped(rect)),
                nested("g", MAX_SVG_DEPTH - 3, &format!("{}{painted}", clipped(inner))),
            ))
        };
        let doc = (0..MAX_SVG_NESTING).fold(level(rect), |inner, _| {
            level(&format!(
                r#"<image href="{}" width="2" height="2"/>"#,
                data_url("image/svg+xml", &gzip(inner.as_bytes()))
            ))
        });
        // A decode worker's, smaller than the 8 MiB main thread where an inline `icon` decodes.
        let thread = std::thread::Builder::new().stack_size(DECODE_STACK_BYTES.min(8 << 20));
        assert!(all_red(&thread.spawn(move || rasterize_doc(&doc)).unwrap().join().unwrap()));
    }

    #[test]
    fn nested_svg_stops_at_the_nesting_limit() {
        // Each level recurses through usvg's parser: 81 levels of base64 gzip, under 1 MB in all,
        // overflowed a 2 MiB decode worker's stack in a debug build and aborted the renderer.
        let nest = |levels| {
            (0..levels).fold(RED_SVG.to_string(), |svg, _| image_doc(&data_url("image/svg+xml", &gzip(svg.as_bytes()))))
        };
        assert!(all_red(&rasterize_doc(&nest(MAX_SVG_NESTING))));
        assert!(all_clear(&rasterize_doc(&nest(MAX_SVG_NESTING + 1))));
    }

    /// `rasterize_svg` of `doc` at `edge` pixels: `Ok` with its pixels or the refusal.
    fn rasterize_at(doc: &str, edge: u32) -> Result<Vec<u8>, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.svg");
        std::fs::write(&path, doc).unwrap();
        rasterize_svg(&path, edge, None, &FontDatabase::default()).map(|(pixels, ..)| pixels)
    }

    #[test]
    fn many_full_bleed_shapes_and_layers_draw_at_a_large_size() {
        // A removed draw-cost estimate refused both at 1024 px, and real icons at 512 px.
        let rects = r#"<rect width="2" height="2" fill="red"/>"#.repeat(300);
        assert!(rasterize_at(&svg(&rects), 1024).is_ok_and(|pixels| all_red(&pixels)));
        let layers = r#"<g opacity="0.5"><rect x="-20" y="-20" width="50" height="50" fill="red"/></g>"#.repeat(4);
        assert!(rasterize_at(&svg(&layers), 1024).is_ok_and(|pixels| pixels.len() == 4 * 1024 * 1024));
    }

    #[test]
    fn an_image_usvg_would_decode_again_refuses_the_whole_document() {
        // usvg decodes the href, base64 and all, per conversion, before any resolver can refuse
        // it: per `<use>` copy, per shape under an objectBoundingBox mask, per marker vertex.
        let png = data_url("image/png", &red_png(2, 2));
        let image = format!(r#"<image href="{png}" width="2" height="2"/>"#);
        let doc = |body: String| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="2" height="2">{body}</svg>"#
            )
        };
        for body in [
            format!(r##"<g id="a">{image}</g><use href="#a"/>"##),
            format!(r##"<defs><g id="a"><g>{image}</g></g></defs><use xlink:href="  #a"/>"##),
            format!(r#"<mask id="m">{image}</mask><rect width="2" height="2" fill="red" mask="url(#m)"/>"#),
            format!(r#"<marker id="m">{image}</marker><path d="M0 0L1 1L2 2" marker-mid="url(#m)"/>"#),
            format!(r#"<filter id="f"><feImage href="{png}"/></filter><rect width="2" height="2" filter="url(#f)"/>"#),
        ] {
            let refused = rasterize_at(&doc(body.clone()), 2).expect_err(&body);
            assert!(refused.contains("repeats an image"), "{refused}");
        }
        // An image drawn once still draws beside a `<use>` and an `feImage` naming an element.
        let once = format!(
            r##"<filter id="f"><feImage href="#r"/></filter><rect id="r" width="2" height="2" fill="red"/><use href="#r"/>{image}"##
        );
        assert!(rasterize_at(&doc(once), 2).is_ok_and(|pixels| all_red(&pixels)));
    }

    /// usvg multiplies `order`'s components and resvg negates `seed` unchecked; with overflow
    /// checks on and `panic = "abort"` in release, either would abort the renderer.
    #[test]
    fn a_filter_attribute_that_would_overflow_is_refused_before_usvg_parses_it() {
        let filtered = |primitive: &str| {
            svg(&format!(
                r#"<filter id="f">{primitive}</filter><rect width="2" height="2" fill="red" filter="url(#f)"/>"#
            ))
        };
        for (primitive, refused) in [
            (r#"<feConvolveMatrix order="3" kernelMatrix="0 0 0 0 1 0 0 0 0"/>"#, false),
            (r#"<feTurbulence seed="-7"/>"#, false),
            (r#"<feConvolveMatrix order="65536"/>"#, true),
            (r#"<feConvolveMatrix order="7e4+7e4"/>"#, true),
            (r#"<feTurbulence seed="-3e38"/>"#, true),
        ] {
            let doc = filtered(primitive);
            let drawn = std::thread::spawn(move || rasterize_at(&doc, 2)).join().expect(primitive);
            assert_eq!(drawn.is_err_and(|err| err.contains("overflows a filter")), refused, "{primitive}");
        }
    }

    #[test]
    fn an_embedded_raster_past_the_decode_edge_is_refused_from_its_header() {
        // resvg decodes an embedded raster at its native size, outside `decode_within_limits`.
        let wide = data_url("image/png", &red_png(MAX_DECODE_EDGE + 1, 1));
        assert!(all_clear(&rasterize_doc(&image_doc(&wide))));
    }

    #[test]
    fn a_gif_is_charged_for_its_first_frame_when_that_outgrows_its_screen() {
        // resvg allocates the first frame at its own size; `image` reports the logical screen.
        let gif = |width: u16, height: u16| {
            let [w0, w1] = width.to_le_bytes();
            let [h0, h1] = height.to_le_bytes();
            // A 1x1 screen with a two-colour global table, then one image descriptor.
            let mut gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\x00\x00\x00\x00\x00".to_vec();
            gif.extend([0x2c, 0, 0, 0, 0, w0, w1, h0, h1, 0, 0x02, 0x00, 0x3b]);
            gif
        };
        let resolve = |data: Vec<u8>, pixels: u64| {
            let (bytes, pixels, depth) = (AtomicU64::new(MAX_SVG_BYTES), AtomicU64::new(pixels), AtomicU32::new(0));
            embedded_image("image/gif", Arc::new(data), &Options::default(), &bytes, &pixels, &depth).is_some()
        };
        assert!(resolve(gif(1, 1), 1));
        assert!(resolve(gif(2, 3), 6));
        assert!(!resolve(gif(2, 3), 5), "a 2x3 frame on a 1x1 screen is 6 pixels");
        assert!(!resolve(gif(MAX_DECODE_EDGE as u16 + 1, 1), u64::MAX));
    }

    #[test]
    fn an_embedded_raster_is_typed_by_its_own_header_and_shares_one_pixel_budget() {
        let resolve = |mime: &str, data: Vec<u8>, pixels: &AtomicU64| {
            let (bytes, depth) = (AtomicU64::new(MAX_SVG_BYTES), AtomicU32::new(0));
            embedded_image(mime, Arc::new(data), &Options::default(), &bytes, pixels, &depth)
                .map(|kind| format!("{kind:?}"))
        };
        // zune-jpeg skips bytes between SOI and the first marker, `image` sniffs only `FF D8 FF`:
        // this SOF0 claims 8193x1 to a decoder the MIME chose and the header check never saw.
        let stuffed = [0xff, 0xd8, 0x00, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01, 0x20, 0x01, 0x01, 0x01, 0x11, 0x00];
        assert_eq!(resolve("image/jpeg", stuffed.to_vec(), &AtomicU64::new(u64::MAX)), None);
        for (format, kind) in [
            (::image::ImageFormat::Png, Some("ImageKind::PNG(..)")),
            (::image::ImageFormat::Jpeg, Some("ImageKind::JPEG(..)")),
            (::image::ImageFormat::Gif, Some("ImageKind::GIF(..)")),
            (::image::ImageFormat::WebP, Some("ImageKind::WEBP(..)")),
            (::image::ImageFormat::Bmp, None),
        ] {
            let mut bytes = std::io::Cursor::new(Vec::new());
            ::image::RgbImage::from_pixel(2, 2, ::image::Rgb([255, 0, 0])).write_to(&mut bytes, format).unwrap();
            let resolved = resolve("image/png", bytes.into_inner(), &AtomicU64::new(u64::MAX));
            assert_eq!(resolved.as_deref(), kind, "{format:?}");
        }
        // 3x2 is 6 pixels: two fill an allowance of 12, and a 1x1 after them does not fit.
        let pixels = AtomicU64::new(12);
        assert!(resolve("image/png", red_png(3, 2), &pixels).is_some());
        assert!(resolve("image/png", red_png(3, 2), &pixels).is_some());
        assert_eq!(resolve("image/png", red_png(1, 1), &pixels), None);
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
    fn an_svgz_that_inflates_or_an_svg_that_is_oversized_is_refused_before_it_is_allocated_or_parsed() {
        // Deflate reaches 1032:1, so the read cap alone bounds only the file on disk: this one is
        // a few kilobytes and asks for 8MB+1. `usvg::decompress_svgz` would hand over all of it.
        let dir = tempfile::tempdir().unwrap();
        let bomb = dir.path().join("bomb.svgz");
        std::fs::write(&bomb, gzip(&vec![b' '; MAX_SVG_BYTES as usize + 1])).unwrap();
        assert!(std::fs::metadata(&bomb).unwrap().len() < MAX_SVG_BYTES, "the compressed file passes the read cap");
        let err =
            rasterize_svg(&bomb, 24, None, &FontDatabase::default()).expect_err("an inflating svgz must be refused");
        assert!(err.contains("inflates past"), "the refusal should say why: {err}");

        let bloated = dir.path().join("huge.svg");
        // One byte over, so the refusal is the size check and not a parse failure.
        std::fs::write(&bloated, vec![b' '; MAX_SVG_BYTES as usize + 1]).unwrap();
        let err =
            rasterize_svg(&bloated, 24, None, &FontDatabase::default()).expect_err("an oversized svg must be refused");
        assert!(err.contains("over the"), "the refusal should say why: {err}");
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }
}
