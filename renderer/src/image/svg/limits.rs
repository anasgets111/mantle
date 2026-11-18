//! Input bounds checked before usvg converts a document: element depth, `<use>` hops, links,
//! filters and repeated images, and DTD entity expansion.

/// Element depth (XML nesting plus `<use>` hops), and `clipPath`, `mask` and `marker` elements, one
/// document may hold. usvg and resvg recurse once per level, `<use>` hop and link (a link names an
/// element, so their count bounds any chain however CSS routes it), with no limit on `<tspan>` or
/// links. The deepest installed icon is 10 deep (at most 3 `<use>` hops); the most linked holds 187
/// (Adwaita `user-trash-full`).
pub(super) const MAX_SVG_DEPTH: usize = 256;
const MAX_SVG_LINKS: usize = 256;
/// A pattern link costs about 8 KiB of stack in release, four times a clip's; icons use 2 at most.
const MAX_SVG_PATTERNS: usize = 16;
/// Filter primitives a document may use, QtSvg's set: others overflow or unwrap in usvg and resvg,
/// aborting the renderer, and no installed icon uses one (ADR-0388). ponytail: a huge offset or
/// filter region still panics in tiny-skia, a huge `stdDeviation` overflows resvg's box blur, and
/// CSS `filter: blur()` or `drop-shadow()` reaches it with no `fe*` element for the walk to see.
/// Upgrade: rasterize in a killable helper process (designed, declined: ADR-0389).
const SVG_FILTERS: [&str; 8] =
    ["feGaussianBlur", "feOffset", "feFlood", "feMerge", "feMergeNode", "feComposite", "feColorMatrix", "feBlend"];

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

fn is_svg_use(node: resvg::usvg::roxmltree::Node<'_, '_>) -> bool {
    node.is_element() && node.tag_name().name() == "use" && matches!(node.tag_name().namespace(), None | Some(SVG_NS))
}

/// Resolves `<use>`'s target with usvg's `href` > `xlink:href` precedence and first-`id` map.
fn use_target<'a, 'input>(
    node: resvg::usvg::roxmltree::Node<'a, 'input>,
    by_id: &std::collections::HashMap<&str, resvg::usvg::roxmltree::Node<'a, 'input>>,
) -> Option<resvg::usvg::roxmltree::Node<'a, 'input>> {
    if !is_svg_use(node) {
        return None;
    }
    let href = node
        .attributes()
        .find(|attr| attr.name() == "href" && attr.namespace().is_none())
        .or_else(|| node.attributes().find(|attr| attr.name() == "href" && attr.namespace() == Some(XLINK_NS)))?
        .value();
    let id = svgtypes::IRI::from_str(href).ok()?.0;
    by_id.get(id).copied()
}

/// Whether `doc` stays inside [`MAX_SVG_DEPTH`] (counting `<use>` hops too, and refusing `<use>`
/// cycles), [`MAX_SVG_LINKS`] and [`MAX_SVG_PATTERNS`], walked without recursion, converts each
/// embedded image once and filters only by [`SVG_FILTERS`]. Walks the document usvg converts, so
/// entity-expanded elements count.
///
/// usvg decodes an image's href, base64 and all, per conversion: per `<use>` copy, marker vertex,
/// or shape under an `objectBoundingBox` mask, clip or filter. So none may sit in those, nor in
/// what an `href` names (svgtypes' IRI: spaces, `#`, up to a space); none installed does.
pub(super) fn within_recursion_limits(doc: &resvg::usvg::roxmltree::Document<'_>) -> bool {
    let linked: std::collections::HashSet<&str> = doc
        .descendants()
        .flat_map(|node| node.attributes())
        .filter(|attr| attr.name() == "href")
        .filter_map(|attr| svgtypes::IRI::from_str(attr.value()).ok().map(|iri| iri.0))
        .collect();
    let (mut depths, mut links, mut patterns) = (Vec::new(), 0, 0);
    let mut by_id = std::collections::HashMap::new();
    for node in doc.descendants().filter(|node| node.is_element()) {
        if let Some(id) = node.attribute("id") {
            by_id.entry(id).or_insert(node);
        }
        let refused = match node.tag_name().name() {
            "image" => node.ancestors().any(|node| {
                matches!(node.tag_name().name(), "mask" | "pattern" | "clipPath" | "marker" | "filter")
                    || node.attribute("id").is_some_and(|id| linked.contains(id))
            }),
            name if name.starts_with("fe") => !SVG_FILTERS.contains(&name),
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
    #[derive(Clone, Copy)]
    enum Visit {
        Unvisited,
        Visiting,
        Height(usize),
    }
    let mut state = vec![Visit::Unvisited; depths.len()];
    let mut stack = vec![(doc.root_element(), false)];
    while let Some((node, exiting)) = stack.pop() {
        let idx = node.id().get_usize();
        if exiting {
            let height = if let Some(target) = use_target(node, &by_id) {
                let Visit::Height(h) = state[target.id().get_usize()] else {
                    return false;
                };
                1 + h
            } else if is_svg_use(node) {
                1
            } else {
                1 + node
                    .children()
                    .filter(|child| child.is_element())
                    .filter_map(|child| match state[child.id().get_usize()] {
                        Visit::Height(h) => Some(h),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0)
            };
            if height > MAX_SVG_DEPTH {
                return false;
            }
            state[idx] = Visit::Height(height);
            continue;
        }
        match state[idx] {
            Visit::Height(_) => continue,
            Visit::Visiting => return false,
            Visit::Unvisited => {}
        }
        state[idx] = Visit::Visiting;
        stack.push((node, true));
        if let Some(target) = use_target(node, &by_id) {
            stack.push((target, false));
        } else if !is_svg_use(node) {
            for child in node.children().filter(|child| child.is_element()) {
                stack.push((child, false));
            }
        }
    }
    true
}

pub(super) enum XmlError {
    EntityLimit,
    Parse(String),
}

/// Extracts entity definitions from the prolog's internal DTD subset and returns
/// the byte offset where the document body starts, or `None` when no entity is declared.
fn dtd_entities(text: &str) -> Option<(std::collections::HashMap<&str, &str>, usize)> {
    let body = text.trim_prefix('\u{feff}');
    let mut i = text.len() - body.len();
    let bytes = text.as_bytes();
    let skip_ws = |i: &mut usize| {
        while bytes.get(*i).is_some_and(u8::is_ascii_whitespace) {
            *i += 1;
        }
    };
    let skip_until = |i: &mut usize, pat: &[u8]| -> Option<()> {
        let pos = bytes[*i..].windows(pat.len()).position(|w| w == pat)?;
        *i += pos + pat.len();
        Some(())
    };
    loop {
        skip_ws(&mut i);
        if bytes[i..].starts_with(b"<?") {
            i += 2;
            skip_until(&mut i, b"?>")?;
        } else if bytes[i..].starts_with(b"<!--") {
            i += 4;
            skip_until(&mut i, b"-->")?;
        } else {
            break;
        }
    }
    if !bytes[i..].starts_with(b"<!DOCTYPE") {
        return None;
    }
    i += b"<!DOCTYPE".len();
    skip_ws(&mut i);
    while bytes.get(i).is_some_and(|b| !b.is_ascii_whitespace() && !matches!(b, b'[' | b'>')) {
        i += 1;
    }
    skip_ws(&mut i);
    let skip_quoted = |i: &mut usize| -> Option<&str> {
        let quote = *bytes.get(*i).filter(|b| matches!(b, b'"' | b'\''))?;
        *i += 1;
        let start = *i;
        let end = bytes[start..].iter().position(|&b| b == quote)?;
        *i = start + end + 1;
        text.get(start..start + end)
    };
    if bytes[i..].starts_with(b"SYSTEM") || bytes[i..].starts_with(b"PUBLIC") {
        let is_public = bytes[i..].starts_with(b"PUBLIC");
        i += 6;
        skip_ws(&mut i);
        skip_quoted(&mut i)?;
        if is_public {
            skip_ws(&mut i);
            skip_quoted(&mut i)?;
        }
        skip_ws(&mut i);
    }
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let mut entities = std::collections::HashMap::new();
    let skip_decl = |i: &mut usize| -> Option<()> {
        while let Some(&b) = bytes.get(*i) {
            *i += 1;
            if b == b'>' {
                return Some(());
            }
            if matches!(b, b'"' | b'\'') {
                let end = bytes[*i..].iter().position(|&c| c == b)?;
                *i += end + 1;
            }
        }
        None
    };
    loop {
        skip_ws(&mut i);
        if bytes.get(i) == Some(&b']') {
            i += 1;
            skip_ws(&mut i);
            if bytes.get(i) != Some(&b'>') {
                return None;
            }
            i += 1;
            break;
        }
        if bytes[i..].starts_with(b"<!--") {
            i += 4;
            skip_until(&mut i, b"-->")?;
        } else if bytes[i..].starts_with(b"<?") {
            i += 2;
            skip_until(&mut i, b"?>")?;
        } else if bytes[i..].starts_with(b"<!ENTITY") {
            i += b"<!ENTITY".len();
            skip_ws(&mut i);
            // roxmltree registers parameter entities in the same map as general entities.
            if bytes.get(i) == Some(&b'%') {
                i += 1;
                skip_ws(&mut i);
            }
            let name_start = i;
            while bytes.get(i).is_some_and(|b| !b.is_ascii_whitespace()) {
                i += 1;
            }
            let name = text.get(name_start..i)?;
            skip_ws(&mut i);
            if bytes.get(i).is_some_and(|b| matches!(b, b'"' | b'\'')) {
                let def = skip_quoted(&mut i)?;
                skip_ws(&mut i);
                if bytes.get(i) != Some(&b'>') {
                    return None;
                }
                i += 1;
                entities.entry(name).or_insert(def);
            } else {
                skip_decl(&mut i)?;
            }
        } else if bytes[i..].starts_with(b"<!") {
            skip_decl(&mut i)?;
        } else {
            return None;
        }
    }
    (!entities.is_empty()).then_some((entities, i))
}

#[derive(Clone, Copy)]
enum EntitySize {
    Visiting,
    Done(usize),
}

fn expanded_size<'a>(
    text: &'a str,
    entities: &std::collections::HashMap<&'a str, &'a str>,
    cache: &mut std::collections::HashMap<&'a str, EntitySize>,
    depth: u8,
    cap: usize,
    skip_markup: bool,
) -> Option<usize> {
    let bytes = text.as_bytes();
    let (mut i, mut total) = (0usize, 0usize);
    while i < bytes.len() {
        if skip_markup && bytes[i..].starts_with(b"<!--") {
            let len = bytes[i + 4..].windows(3).position(|w| w == b"-->").map_or(bytes.len() - i, |p| p + 7);
            total = total.checked_add(len).filter(|&t| t <= cap)?;
            i += len;
            continue;
        }
        if skip_markup && bytes[i..].starts_with(b"<![CDATA[") {
            let len = bytes[i + 9..].windows(3).position(|w| w == b"]]>").map_or(bytes.len() - i, |p| p + 12);
            total = total.checked_add(len).filter(|&t| t <= cap)?;
            i += len;
            continue;
        }
        if bytes[i] == b'&'
            && let Some(rel_semi) =
                bytes[i + 1..].iter().position(|&b| matches!(b, b';' | b'<' | b'&' | b' ' | b'\t' | b'\r' | b'\n'))
            && bytes[i + 1 + rel_semi] == b';'
        {
            let name = std::str::from_utf8(&bytes[i + 1..i + 1 + rel_semi]).ok()?;
            let add = if entities.contains_key(name) {
                if depth >= 10 {
                    return None;
                }
                match cache.get(name).copied() {
                    Some(EntitySize::Visiting) => return None,
                    Some(EntitySize::Done(n)) => n,
                    None => {
                        let def = entities[name];
                        cache.insert(name, EntitySize::Visiting);
                        let n = expanded_size(def, entities, cache, depth + 1, cap, false)?;
                        cache.insert(name, EntitySize::Done(n));
                        n
                    }
                }
            } else {
                rel_semi + 2
            };
            total = total.checked_add(add).filter(|&t| t <= cap)?;
            i += rel_semi + 2;
            continue;
        }
        total = total.checked_add(1).filter(|&t| t <= cap)?;
        i += 1;
    }
    Some(total)
}

/// Expanded body byte length, or `None` if DTD entity expansion cycles, nests past `roxmltree`'s
/// 10-level cap, or exceeds `cap` bytes. `roxmltree` bounds per-ref nesting and nested counts,
/// not expanded bytes or repeated root refs.
fn expanded_xml_bytes(text: &str, cap: usize) -> Option<usize> {
    if !text.contains("<!ENTITY") {
        return (text.len() <= cap).then_some(text.len());
    }
    let Some((entities, body_start)) = dtd_entities(text) else {
        return (text.len() <= cap).then_some(text.len());
    };
    let mut cache = std::collections::HashMap::new();
    expanded_size(&text[body_start..], &entities, &mut cache, 0, cap, true)
}

/// `data` parsed as `Tree::from_data` parses it, bounded to `cap` entity-expanded bytes so the
/// limits walk and the conversion share one parse without entity-expansion OOM.
pub(super) fn parse_xml(data: &[u8], cap: u64) -> Result<(resvg::usvg::roxmltree::Document<'_>, u64), XmlError> {
    let text = std::str::from_utf8(data).map_err(|_| XmlError::Parse(resvg::usvg::Error::NotAnUtf8Str.to_string()))?;
    let cap_usize = usize::try_from(cap).unwrap_or(usize::MAX);
    let expanded = expanded_xml_bytes(text, cap_usize).ok_or(XmlError::EntityLimit)?;
    let options = resvg::usvg::roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = resvg::usvg::roxmltree::Document::parse_with_options(text, options)
        .map_err(|err| XmlError::Parse(err.to_string()))?;
    Ok((doc, expanded as u64))
}

#[cfg(test)]
mod tests {
    use super::super::super::decode::DECODE_STACK_BYTES;
    use super::super::super::tests::GRADIENT_SVG;
    use super::super::tests::{
        CLEAR, RED, RED_SVG, all_clear, all_red, data_url, fixture_shaping, gzip, image_doc, rasterize_at,
        rasterize_doc, red_png, side_by_side, svg,
    };
    use super::super::{MAX_SVG_BYTES, MAX_SVG_NESTING, rasterize_svg};
    use super::*;
    use crate::text::shaping::FontDatabase;
    use resvg::usvg::{Options, Tree};

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
        for marker in ["", "% "] {
            assert!(draws(format!("<!DOCTYPE svg [<!ENTITY {marker}ok '{rect}'>]>{}", svg("&ok;"))));
        }
        let entity = nested("g", MAX_SVG_DEPTH - 1, rect);
        let deep_dtd = format!("<!DOCTYPE svg [<!ENTITY deep '{entity}'>]>{}", svg("&deep;"));
        assert!(rasterize_at(&deep_dtd, 2).is_err_and(|err| err.contains("nests past")));
        assert!(!draws(deep_dtd));
        // Direct `<use>` hops and shallow `<g><use/></g>` hops both count against `MAX_SVG_DEPTH`.
        let use_hops = |hops: usize, suffix: &str| {
            let mut body = format!(r#"<rect id="target{suffix}" width="2" height="2" fill="red"/>"#);
            for i in 0..hops {
                let next = if i + 1 == hops { "target".to_string() } else { format!("u{}", i + 1) };
                body.push_str(&format!(r##"<use id="u{i}{suffix}" href="#{next}{suffix}"/>"##));
            }
            svg(&format!(r##"{body}<use href="#u0{suffix}"/>"##))
        };
        assert!(draws(use_hops(MAX_SVG_DEPTH - 3, "")));
        assert!(!draws(use_hops(MAX_SVG_DEPTH - 2, "")));
        // Character references preserve whitespace in IDs; usvg follows those IDs verbatim.
        for suffix in ["&#9;", "&#10;", "&#13;", "&#160;"] {
            assert!(draws(use_hops(1, suffix)));
            let deep = use_hops(MAX_SVG_DEPTH - 2, suffix);
            assert!(rasterize_at(&deep, 2).is_err_and(|err| err.contains("nests past")), "{suffix}");
        }
        let group_use_hops = |hops: usize| {
            let mut defs = String::from("<defs>");
            for i in 0..hops {
                let next = if i + 1 == hops { "leaf".to_string() } else { format!("g{}", i + 1) };
                defs.push_str(&format!(r##"<g id="g{i}"><use href="#{next}"/></g>"##));
            }
            defs.push_str(&format!(r#"<g id="leaf">{rect}</g></defs>"#));
            svg(&format!(r##"{defs}<use href="#g0"/>"##))
        };
        assert!(draws(group_use_hops((MAX_SVG_DEPTH - 3) / 2)));
        assert!(!draws(group_use_hops((MAX_SVG_DEPTH - 3) / 2 + 1)));
        // `href` beats `xlink:href`, first `id` wins, cycles (even 3-hop or in `<defs>`) are refused,
        // and a diamond DAG sharing one target stays linear and draws.
        let deep_defs = format!(r#"<defs><g id="deep">{}</g></defs>"#, nested("g", MAX_SVG_DEPTH - 4, rect));
        assert!(draws(format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="2" height="2">{deep_defs}<rect id="ok" width="2" height="2" fill="red"/><g><use href="#ok" xlink:href="#deep"/></g></svg>"##
        )));
        assert!(!draws(format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="2" height="2">{deep_defs}<rect id="ok" width="2" height="2" fill="red"/><g><use xlink:href="#ok" href="#deep"/></g></svg>"##
        )));
        assert!(!draws(format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="2" height="2">{deep_defs}<g><use xlink:href="#deep"/></g></svg>"##
        )));
        assert!(draws(format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="2" height="2">{deep_defs}<rect id="ok" width="2" height="2" fill="red"/><g><use xlink:title="#deep" xlink:href="#ok"/></g></svg>"##
        )));
        assert!(draws(svg(&format!(
            r##"<rect id="dup" width="2" height="2" fill="red"/><g id="dup">{}</g><g><g><use href="#dup"/></g></g>"##,
            nested("g", MAX_SVG_DEPTH - 4, rect)
        ))));
        for cycle in [
            svg(r##"<use id="a" href="#a"/>"##),
            svg(r##"<g id="a"><use href="#b"/></g><g id="b"><use href="#a"/></g><use href="#a"/>"##),
            svg(
                r##"<g id="a"><use href="#b"/></g><g id="b"><use href="#c"/></g><g id="c"><use href="#a"/></g><use href="#a"/>"##,
            ),
            svg(r##"<defs><g id="c"><use href="#c"/></g></defs><rect width="2" height="2" fill="red"/>"##),
        ] {
            assert!(rasterize_at(&cycle, 2).is_err_and(|err| err.contains("cycles <use>")), "{cycle}");
        }
        assert!(draws(svg(&format!(
            r##"<defs><g id="shared">{rect}</g></defs><use href="#shared"/><use href="#shared"/><use href="#missing">{}</use>"##,
            nested("g", MAX_SVG_DEPTH - 3, rect)
        ))));
        assert!(!draws(svg(&format!(r##"<use href="#missing">{}</use>"##, nested("g", MAX_SVG_DEPTH - 1, rect)))));
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
        let use_too_deep = use_hops(MAX_SVG_DEPTH - 2, "");
        assert!(all_clear(&rasterize_doc(&image_doc(&data_url("image/svg+xml", use_too_deep.as_bytes())))));
    }

    #[test]
    fn xml_entity_expansion_is_bounded_without_breaking_dtd_entities() {
        // Nested entity bombs, both classic 10-level billion-laughs and 7-level binary trees that
        // stay under roxmltree's 10-depth and 255-reference caps, are refused before parsing.
        let mut lol = String::from("<!DOCTYPE svg [<!ENTITY lol0 '0123456789'>");
        for i in 1..=9 {
            let refs = format!("&lol{};", i - 1).repeat(10);
            lol.push_str(&format!("<!ENTITY lol{i} '{refs}'>"));
        }
        lol.push_str(&format!("]>{}", svg("<text>&lol9;</text>")));
        let err = rasterize_at(&lol, 2).expect_err("billion laughs must be refused");
        assert!(err.contains("expands entities past"), "{err}");

        let leaf = "x".repeat(128 * 1024);
        let mut tree = format!("<!DOCTYPE svg [<!ENTITY t0 '{leaf}'>");
        for i in 1..=7 {
            tree.push_str(&format!("<!ENTITY t{i} '&t{};&t{};'>", i - 1, i - 1));
        }
        tree.push_str(&format!("]>{}", svg("<text>&t7;</text>")));
        let err = rasterize_at(&tree, 2).expect_err("16 MiB 254-ref binary entity tree must be refused");
        assert!(err.contains("expands entities past"), "{err}");

        let flat = "x".repeat(2 * 1024 * 1024);
        for marker in ["", "% "] {
            let repeated =
                format!("<!DOCTYPE svg [<!ENTITY {marker}f '{flat}'>]>{}", svg("<text>&f;&f;&f;&f;&f;</text>"));
            assert!(expanded_xml_bytes(&repeated, MAX_SVG_BYTES as usize).is_none(), "{marker}");
            assert!(rasterize_at(&repeated, 2).is_err_and(|err| err.contains("expands entities past")));
        }

        let cycle = format!("<!DOCTYPE svg [<!ENTITY a '&b;'><!ENTITY b '&a;'>]>{}", svg("&a;"));
        assert!(rasterize_at(&cycle, 2).is_err_and(|err| err.contains("expands entities past")));

        // Entity nesting of 10 passes; 11 is refused.
        let chain_doc = |hops: usize| {
            let mut dtd = String::from("<!DOCTYPE svg [<!ENTITY e0 '<rect width=\"2\" height=\"2\" fill=\"red\"/>'>");
            for i in 1..hops {
                dtd.push_str(&format!("<!ENTITY e{i} '&e{};'>", i - 1));
            }
            format!("{dtd}]>{}", svg(&format!("&e{};", hops - 1)))
        };
        assert!(rasterize_at(&chain_doc(10), 2).is_ok_and(|pixels| all_red(&pixels)));
        assert!(rasterize_at(&chain_doc(11), 2).is_err_and(|err| err.contains("expands entities past")));

        // Nested SVG `<image>` holding an entity bomb is refused and drains the nested budget, while
        // entity-expanded bytes count against the shared budget across siblings.
        assert_eq!(
            side_by_side(&[data_url("image/svg+xml", tree.as_bytes()), data_url("image/svg+xml", RED_SVG.as_bytes())]),
            [CLEAR, CLEAR]
        );
        let half_leaf = "x".repeat(1024 * 1024);
        let half_entities = format!(
            "<!DOCTYPE svg [<!ENTITY h '{half_leaf}'>]>{}",
            svg(r#"<rect width="2" height="2" fill="red"/><text>&h;&h;&h;&h;&h;</text>"#)
        );
        let half_url = data_url("image/svg+xml", half_entities.as_bytes());
        assert_eq!(side_by_side(&[half_url.clone(), half_url]), [RED, CLEAR]);

        // Realistic Illustrator-style DTD entities, BOM, PUBLIC/SYSTEM external IDs, `<!ELEMENT>`/`<!ENTITY ... SYSTEM>`,
        // parameter entities, comments and CDATA `&...;` keep drawing, while bombs behind the same prolog are still caught.
        let big = "x".repeat(2 * 1024 * 1024);
        let prolog = "\u{feff}<?xml version='1.0'?><!-- prolog -->\
             <!DOCTYPE svg PUBLIC '-//W3C//DTD SVG 1.1//EN' 'http://example.org/svg11.dtd' [\
             <!-- dtd comment --><!-- dtd2 --><?pi target?><?pi2?>\
             <!ELEMENT svg ANY>\
             <!ENTITY ext SYSTEM 'a>b'>\
             <!ENTITY % pe 'ignored'>\
             <!ENTITY ns_svg 'http://www.w3.org/2000/svg'>\
             <!ENTITY fill 'red'>";
        let valid = format!(
            "{prolog}\
             <!ENTITY unused '{big}'>\
             <!ENTITY shape '<rect width=\"2\" height=\"2\" fill=\"&fill;\"/>'>\
             <!ENTITY shape '<circle r=\"1\"/>'>\
             ]><svg xmlns='&ns_svg;' width='2' height='2'><!-- &unused;&unused;&unused;&unused;&unused; -->\
             <text><![CDATA[&unused;&unused;&unused;&unused;&unused;]]></text>&shape;</svg>"
        );
        assert!(rasterize_at(&valid, 2).is_ok_and(|pixels| all_red(&pixels)));
        let prolog_bomb = format!("{prolog}<!ENTITY f '{flat}'> ] >{}", svg("<!--a--><![CDATA[b]]>&f;&f;&f;&f;&f;"));
        assert!(rasterize_at(&prolog_bomb, 2).is_err_and(|err| err.contains("expands entities past")));
        let bare_prolog_bomb = format!(
            "<?xml version='1.0'?><?pi a?><!--a--><!--b--><!DOCTYPE svg [<!ENTITY f '{flat}'>]>{}",
            svg("&f;&f;&f;&f;&f;")
        );
        assert!(rasterize_at(&bare_prolog_bomb, 2).is_err_and(|err| err.contains("expands entities past")));
        let sys_dtd = format!(
            "<!DOCTYPE svg SYSTEM 'svg.dtd' [<!ENTITY r '<rect width=\"2\" height=\"2\" fill=\"red\"/>'>]>{}",
            svg("&r;")
        );
        assert!(rasterize_at(&sys_dtd, 2).is_ok_and(|pixels| all_red(&pixels)));
        let sys_bomb = format!("<!DOCTYPE svg SYSTEM 'svg.dtd' [<!ENTITY f '{flat}'>]>{}", svg("&f;&f;&f;&f;&f;"));
        assert!(rasterize_at(&sys_bomb, 2).is_err_and(|err| err.contains("expands entities past")));

        // Exact cap bounds across plain SVG, skipped DTD, comments, CDATA, built-in `&apos;` and expanded entities.
        assert_eq!(expanded_xml_bytes("<svg/>", 6), Some(6));
        assert_eq!(expanded_xml_bytes("<svg/>", 5), None);
        let no_dtd = "<!-- <!ENTITY -->";
        assert_eq!(expanded_xml_bytes(no_dtd, no_dtd.len()), Some(no_dtd.len()));
        assert_eq!(expanded_xml_bytes(no_dtd, no_dtd.len() - 1), None);
        let exact = "<!DOCTYPE svg [<!ENTITY e 'abcde'>]><svg><!--xxxxxxxxxx--><![CDATA[yyyyyyyyyy]]>&apos;&e;</svg>";
        let expected_len = "<svg><!--xxxxxxxxxx--><![CDATA[yyyyyyyyyy]]>&apos;abcde</svg>".len();
        assert_eq!(expanded_xml_bytes(exact, expected_len), Some(expected_len));
        assert_eq!(expanded_xml_bytes(exact, expected_len - 1), None);
        assert_eq!(expanded_xml_bytes("<!DOCTYPE svg [<!ENTITY e 'abc'>]>a<!--", 5), Some(5));
        assert_eq!(expanded_xml_bytes("<!DOCTYPE svg [<!ENTITY e 'abc'>]><svg><!--&e;", 12), Some(12));
        assert_eq!(expanded_xml_bytes("<!DOCTYPE svg [<!ENTITY e 'abc'>]><svg><!--&e;", 11), None);
        assert_eq!(expanded_xml_bytes("<!DOCTYPE svg [<!ENTITY e 'abc'>]>a<![CDATA[", 10), Some(10));
        assert_eq!(expanded_xml_bytes("<!DOCTYPE svg [<!ENTITY e 'abc'>]><svg><![CDATA[&e;", 17), Some(17));
        assert_eq!(expanded_xml_bytes("<!DOCTYPE svg [<!ENTITY e 'abc'>]><svg><![CDATA[&e;", 16), None);
    }

    /// The worst document the limits accept, on the smallest stack that decodes one: at every level
    /// `<g>`, `<tspan>` and `<use>` chains as deep as allowed, a pattern chain and a clip chain as
    /// long as allowed at the deepest point, and the nested SVG under the clip chain there: not in
    /// the pattern, where usvg would convert it again.
    #[test]
    fn the_worst_accepted_document_fits_the_smallest_decode_stack() {
        let rect = r#"<rect width="2" height="2" fill="red"/>"#;
        let use_chain = |hops: usize, inner: &str| {
            let mut defs = String::from("<defs>");
            for i in 0..hops {
                defs.push_str(&format!(
                    r##"<symbol id="u{i}" width="2" height="2"><use href="#u{}"/></symbol>"##,
                    i + 1
                ));
            }
            defs.push_str(&format!(r#"<g id="u{hops}">{inner}</g></defs>"#));
            (defs, r##"<use href="#u0"/>"##)
        };
        let clipped = |inner: &str| format!(r#"<g clip-path="url(#clipPath{})">{inner}</g>"#, MAX_SVG_LINKS - 1);
        let level = |inner: &str| {
            let painted = format!(r#"<rect width="2" height="2" fill="url(#pattern{})"/>"#, MAX_SVG_PATTERNS - 1);
            let (defs, root_use) = use_chain((MAX_SVG_DEPTH - 4) / 2, rect);
            svg(&format!(
                r#"<text>{}</text>{defs}{root_use}{}{}{}"#,
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
        // An image drawn once still draws beside a `<use>` naming an element.
        let once = format!(r##"<rect id="r" width="2" height="2" fill="red"/><use href="#r"/>{image}"##);
        assert!(rasterize_at(&doc(once), 2).is_ok_and(|pixels| all_red(&pixels)));
    }

    /// `rect` under a filter of `primitives`.
    fn filtered(primitives: &str) -> String {
        svg(&format!(r#"<filter id="f">{primitives}</filter><rect width="2" height="2" fill="red" filter="url(#f)"/>"#))
    }

    /// Outside QtSvg's set, usvg and resvg overflow or unwrap on some values (`order`, `seed`,
    /// a morphology radius in bounding-box units); in release that aborts the renderer.
    #[test]
    fn a_filter_primitive_outside_the_allowlist_refuses_the_document_without_panicking() {
        for primitive in [
            r#"<feConvolveMatrix order="65536"/>"#,
            r#"<feTurbulence seed="-3e38"/>"#,
            r#"<feMorphology radius="3e38"/>"#,
            r#"<feDiffuseLighting><feDistantLight/></feDiffuseLighting>"#,
            r##"<feImage href="#r"/>"##,
            r#"<feUnknown/>"#,
        ] {
            let doc = filtered(primitive).replace(r#"id="f""#, r#"id="f" primitiveUnits="objectBoundingBox""#);
            let drawn = std::thread::spawn(move || rasterize_at(&doc, 2)).join().expect(primitive);
            assert!(drawn.is_err_and(|err| err.contains("unsupported filter")), "{primitive}");
        }
        let nested = data_url("image/svg+xml", filtered(r#"<feTile/>"#).as_bytes());
        assert!(all_clear(&rasterize_doc(&image_doc(&nested))), "the nested parse takes the same walk");
    }

    #[test]
    fn each_allowlisted_filter_primitive_still_draws() {
        for primitive in [
            r#"<feGaussianBlur stdDeviation="0.1"/>"#,
            r#"<feOffset dx="0"/>"#,
            r#"<feFlood flood-color="red"/>"#,
            r#"<feMerge><feMergeNode in="SourceGraphic"/></feMerge>"#,
            r#"<feComposite in="SourceGraphic" in2="SourceGraphic"/>"#,
            r#"<feColorMatrix type="saturate" values="1"/>"#,
            r#"<feBlend in="SourceGraphic" in2="SourceGraphic"/>"#,
        ] {
            let drawn = rasterize_at(&filtered(primitive), 2);
            assert!(drawn.is_ok_and(|pixels| pixels.as_chunks::<4>().0.iter().any(|px| px[3] > 0)), "{primitive}");
        }
    }

    #[test]
    fn malformed_utf8_or_xml_is_refused_and_xmltree_matches_from_data() {
        let dir = tempfile::tempdir().unwrap();
        let fonts = fixture_shaping().font_database();
        for bad in [&b"\xff\xfe<svg/>"[..], b"<svg><rect"] {
            let path = dir.path().join("bad.svg");
            std::fs::write(&path, bad).unwrap();
            assert!(rasterize_svg(&path, 4, None, &fonts).is_err());
            assert!(all_clear(&rasterize_doc(&image_doc(&data_url("image/svg+xml", bad)))));
        }
        let nested_doc = image_doc(&data_url("image/svg+xml", RED_SVG.as_bytes()));
        let text_doc = svg(r#"<text x="1" y="2" font-size="2">A</text>"#);
        let filter_doc = filtered(r#"<feGaussianBlur stdDeviation="0.2"/>"#);
        for doc in [GRADIENT_SVG, nested_doc.as_str(), text_doc.as_str(), filter_doc.as_str()] {
            let opts =
                Options { fontdb: fonts.db.clone(), font_family: fonts.primary_family.clone(), ..Default::default() };
            let render_tree = |tree: Tree| {
                let mut pixmap = resvg::tiny_skia::Pixmap::new(32, 32).unwrap();
                resvg::render(&tree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());
                pixmap.take()
            };
            let from_data = render_tree(Tree::from_data(doc.as_bytes(), &opts).unwrap());
            let (xml, _) = parse_xml(doc.as_bytes(), MAX_SVG_BYTES).unwrap_or_else(|_| panic!("parse failed"));
            let from_xml = render_tree(Tree::from_xmltree(&xml, &opts).unwrap());
            assert_eq!(from_xml, from_data);
        }
    }
}
