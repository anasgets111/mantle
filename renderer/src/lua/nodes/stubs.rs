//! Renders `lua-meta/nodes.lua`, `lua-meta/surfaces.lua` and the docs' property tables from
//! the typed fields of `properties`, which the name check and the parsers read. The golden test
//! below is the only caller: `just stubs` rewrites, a stale file fails `cargo test`.
//!
//! The aliases and the classes nothing else describes (`TextRun`, `Transition`) are hand-written in
//! [`NODES_HEADER`]: they document shapes inside a property, which the table does not model.

use super::properties::{ALL, Absent, BOX, KINDS, Property, SURFACES, kind_doc, properties};
use crate::layout::node::prop::Keyword;
use crate::layout::node::{Align, PopupAnchor};

/// Choice sets the stubs name as an alias.
const ALIASES: [(&str, &[&str]); 2] = [("Align", Align::NAMES), ("PopupAnchor", PopupAnchor::NAMES)];

/// The published book: `docs/x/y.md` is served at `x/y.html` under it.
const DOCS: &str = "https://anasgets111.github.io/mantle/";

/// Opens a docs page's generated property table; the table runs to [`END`].
const BEGIN: &str =
    "<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->";
const END: &str = "<!-- End of the generated table. -->";

/// `row` -> `Row`.
fn class(kind: &str) -> String {
    kind[..1].to_ascii_uppercase() + &kind[1..] + "Props"
}

/// `kind`'s docs page, without `.md`.
fn page(kind: &str) -> String {
    match kind {
        "row" | "column" => "nodes/row-column".to_string(),
        kind if kind_bit(kind) & SURFACES != 0 => format!("surfaces/{kind}"),
        kind => format!("nodes/{kind}"),
    }
}

fn kind_bit(kind: &str) -> u16 {
    super::properties::kind_bit(kind).expect("a node kind")
}

fn union(choices: &[&str]) -> String {
    choices.iter().map(|choice| format!("\"{choice}\"")).collect::<Vec<_>>().join("|")
}

/// The row's LuaCATS type: its choices, by alias where one names them, then its field's type.
fn lua_type(row: &Property, alias: bool) -> String {
    let ty = (row.ty)();
    if row.choices.is_empty() {
        return ty;
    }
    let named = ALIASES.iter().find(|(_, choices)| alias && *choices == row.choices);
    let head = named.map_or_else(|| union(row.choices), |(name, _)| name.to_string());
    if ty.is_empty() { head } else { format!("{head}|{ty}") }
}

/// The row's `///` block as the stub's words and the docs table's cell: a `Book:` paragraph is the
/// cell, the rest the words, each paragraph joined onto one line. No `Book:` makes the cell the words.
fn docs(row: &Property) -> (String, String) {
    let mut words = Vec::new();
    let mut cell = None;
    for paragraph in row.doc.split("\n\n") {
        let line = paragraph.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ");
        match line.strip_prefix("Book: ") {
            Some(book) => cell = Some(book.to_string()),
            None if !line.is_empty() => words.push(line),
            None => {}
        }
    }
    let words = words.join(" ");
    (cell.unwrap_or_else(|| words.clone()), words)
}

/// The docs' Default cell, `None` when there is none.
fn default_cell(absent: Absent) -> Option<String> {
    Some(match absent {
        Absent::Unset => return None,
        Absent::Required => "Required".to_string(),
        Absent::Number(n) => format!("`{n}`"),
        Absent::Bool(b) => format!("`{b}`"),
        Absent::Choice(name) => format!("`\"{name}\"`"),
        Absent::Lua(literal) => format!("`{literal}`"),
        Absent::Prose(text) => text[..1].to_ascii_uppercase() + &text[1..],
    })
}

/// `---@field` line: range and default first, then the row's own words.
fn field(row: &Property) -> String {
    let mut facts: Vec<String> = row.range.iter().map(|(low, high)| format!("`[{low}, {high}]`")).collect();
    match row.absent {
        Absent::Unset | Absent::Required => {}
        Absent::Prose(text) => facts.push(format!("default: {text}")),
        other => facts.push(format!("default {}", default_cell(other).expect("a value"))),
    }
    let mut words: Vec<String> = Vec::new();
    if row.absent == Absent::Required {
        words.push("Required.".to_string());
    }
    if !facts.is_empty() {
        let joined = facts.join(", ");
        words.push(joined[..1].to_ascii_uppercase() + &joined[1..] + ".");
    }
    let (_, doc) = docs(row);
    words.extend((!doc.is_empty()).then_some(doc));
    let optional = if row.absent == Absent::Required { "" } else { "?" };
    let words = if words.is_empty() { String::new() } else { format!(" {}", words.join(" ")) };
    format!("---@field {}{optional} {}{words}\n", row.name, lua_type(row, true))
}

/// A kind's own rows: the ones its stub class declares and its page tables, after the common and box
/// rows it inherits.
fn own(kinds: u16) -> impl Iterator<Item = &'static Property> {
    properties().filter(move |row| row.kinds & kinds != 0 && row.kinds != ALL && row.kinds != BOX)
}

fn render_stub(header: &str, kinds: &[&str]) -> String {
    let mut out = header.to_string();
    for kind in kinds {
        let bases = if kind_bit(kind) & BOX != 0 { "NodeBase, BoxBase" } else { "NodeBase" };
        out.push_str(&format!("\n---@class {}: {bases}\n", class(kind)));
        own(kind_bit(kind)).for_each(|row| out.push_str(&field(row)));
    }
    for kind in kinds {
        out.push('\n');
        let blurb = kind_doc(kind_bit(kind)).lines().map(str::trim).collect::<Vec<_>>().join(" ");
        if !blurb.trim().is_empty() {
            out.push_str(&format!("---{}\n", blurb.trim()));
        }
        out.push_str(&format!(
            "---[docs]({DOCS}{}.html)\n---@param props {}\n---@return Node\nfunction {kind}(props) end\n",
            page(kind),
            class(kind)
        ));
    }
    out
}

/// `lua-meta/nodes.lua`.
fn nodes_lua() -> String {
    let percent: Vec<String> = (0..=100).map(|n| format!("\"{n}%\"")).collect();
    let mut header = NODES_HEADER.replace("{PERCENT}", &percent.join("|")).replace("{DOCS}", DOCS);
    header = header.replace("{ALIGN}", &union(Align::NAMES));
    header = header.replace("{EASING}", &union(&crate::layout::node::easing_names().collect::<Vec<_>>()));
    for (class, kinds) in [("NodeBase", ALL), ("BoxBase", BOX)] {
        let marker = format!("{{{class}}}");
        let fields: String = properties().filter(|row| row.kinds == kinds).map(field).collect();
        header = header.replace(&marker, fields.trim_end());
    }
    render_stub(&header, &KINDS[..11])
}

/// `lua-meta/surfaces.lua`.
fn surfaces_lua() -> String {
    render_stub(&SURFACES_HEADER.replace("{POPUP_ANCHOR}", &union(PopupAnchor::NAMES)), &KINDS[11..])
}

/// A doc string as a Markdown table cell: no `(ADR-NNNN)` pointers, which are history, and `|`
/// escaped, which GFM splits on even inside a code span.
fn cell(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(" (ADR-") {
        out.push_str(&rest[..start]);
        rest = rest[start..].split_once(')').map_or("", |(_, after)| after);
    }
    out.push_str(rest);
    out.trim_end_matches('.').replace('|', "\\|")
}

/// The Markdown table of `rows`.
fn table<'a>(rows: impl Iterator<Item = &'a Property>) -> String {
    let mut out = "| Property | Type | Default | Behaviour |\n| :--- | :--- | :--- | :--- |\n".to_string();
    for row in rows {
        let range = row.range.map(|(low, high)| format!(", `[{low}, {high}]`")).unwrap_or_default();
        let (behaviour, _) = docs(row);
        out.push_str(&format!(
            "| `{}` | `{}`{range} | {} | {} |\n",
            row.name,
            cell(&lua_type(row, false).replace(", [string]: \"no such property\"", "")),
            default_cell(row.absent).unwrap_or_else(|| "None".to_string()),
            cell(&behaviour)
        ));
    }
    out
}

/// Every page with a generated table, and that table: the common rows, the box rows, then each
/// kind's own on its page.
fn doc_tables() -> Vec<(String, String)> {
    let mut pages = vec![
        ("nodes/index".to_string(), table(properties().filter(|row| row.kinds == ALL))),
        ("guide/paint".to_string(), table(properties().filter(|row| row.kinds == BOX))),
    ];
    for kind in KINDS {
        let page = page(kind);
        if !pages.iter().any(|(listed, _)| *listed == page) {
            let kinds = KINDS.iter().filter(|other| self::page(other) == page).map(|other| kind_bit(other));
            let rows = table(own(kinds.fold(0, |mask, bit| mask | bit)));
            pages.push((page, rows));
        }
    }
    pages
}

/// `text` with the table between [`BEGIN`] and [`END`] replaced by `table`.
fn splice(path: &str, text: &str, table: &str) -> String {
    let fail = || panic!("{path} has no `{BEGIN}` ... `{END}` pair");
    let (head, rest) = text.split_once(BEGIN).unwrap_or_else(fail);
    let (_, tail) = rest.split_once(END).unwrap_or_else(fail);
    format!("{head}{BEGIN}\n{table}{END}{tail}")
}

#[test]
fn the_generated_node_stubs_match_what_is_checked_in() {
    let mut files =
        vec![("lua-meta/nodes.lua".to_string(), nodes_lua()), ("lua-meta/surfaces.lua".to_string(), surfaces_lua())];
    for (page, table) in doc_tables() {
        let path = format!("docs/{page}.md");
        let text = std::fs::read_to_string(format!("{}/../{path}", env!("CARGO_MANIFEST_DIR")))
            .unwrap_or_else(|err| panic!("{path}: {err}"));
        files.push((path.clone(), splice(&path, &text, &table)));
    }
    shared::check_generated(&files);
}

const NODES_HEADER: &str = r##"---@meta
-- The eleven node kinds and their properties. Surface roles live in `surfaces.lua`.
--
-- GENERATED by `renderer/src/lua/nodes/stubs.rs` from the property table in
-- `renderer/src/lua/nodes/properties.rs`, which the engine's name check and parsers read. Do not
-- edit: change the table, run `just stubs` and commit what changes.
--
-- `Bound` in a union means the property also takes a signal, resolved once per pass. It is
-- `userdata`, not `Signal`, so table payloads are not mistaken for signals. `id` and callbacks take
-- no signal; `hover`, `scroll` and `geometry` take the handle itself. `[string]: "no such property"`
-- makes a misspelled key a type error.

---@alias Node table A node table, as one of the constructors below returns it.
---@alias Align {ALIGN}
-- ponytail: copied from cursor-icon 1.2's `FromStr`, which exposes no list to derive it from; the
-- stub probe catches a name it refuses, not one missing here. Upgrade: derive once the crate lists them.
---@alias Cursor "default"|"pointer"|"text"|"not-allowed"|"grab"|"grabbing"|"move"|"crosshair"|"wait"|"progress"|"help"|"context-menu"|"cell"|"vertical-text"|"alias"|"copy"|"no-drop"|"zoom-in"|"zoom-out"|"all-scroll"|"col-resize"|"row-resize"|"n-resize"|"e-resize"|"s-resize"|"w-resize"|"ne-resize"|"nw-resize"|"se-resize"|"sw-resize"|"ew-resize"|"ns-resize"|"nesw-resize"|"nwse-resize" CSS cursor name (same as `wp_cursor_shape_v1`).
---@alias Edges { top?: number, right?: number, bottom?: number, left?: number, [string]: "no such property" } Per-edge pixels; a missing edge is `0`.
-- ponytail: whole percents only, so a fraction (`"12.5%"`) or one above `"100%"`, which the engine
-- accepts, is flagged. Upgrade: a pattern type, which LuaLS lacks.
---@alias Percent {PERCENT} `"NN%"` of the parent's box (the output's, on a panel).
---@alias Length number|"Fill"|Percent Pixels `[0, 8192]`, the remaining space, or a percent.
---@alias Color string `"#RRGGBB"` or `"#RRGGBBAA"`. No shorthand or names.
---@alias BorderColors { top?: Color, right?: Color, bottom?: Color, left?: Color, [string]: "no such property" } Per-edge colours; a signal inside is refused.
---@alias Axes { x?: number, y?: number, [string]: "no such property" } A missing axis takes the property's default.
---@alias GradientStop [number, Color] Position `[0, 1]` and colour. Positions ascend.
---@alias Gradient { gradient: "Linear"|"Radial"|"Conic", angle?: number, stops: GradientStop[], [string]: "no such property" } At least 2 stops. `angle` is degrees clockwise from the top: Linear default `180`, Conic default `0`, Radial refuses it.
---@alias Mask { gradient?: "Linear"|"Radial"|"Conic", angle?: number, stops?: GradientStop[], source?: string, invert?: boolean, [string]: "no such property" } Exactly one of a `Gradient` or an image `source` path (alpha only, stretched over the box). `invert` swaps kept and cut.
---@alias EasingName {EASING} `Back` and `Elastic` overshoot, as does a Bezier `y` outside `[0, 1]`; the property's range clamps them.
---@alias Easing EasingName|[number, number, number, number]|{ steps: integer } A name, CSS `cubic-bezier` `{ x1, y1, x2, y2 }` with `x1`, `x2` in `[0, 1]`, or `{ steps = n }`, `n` in `[1, 1000]` (ADR-0151).
---@alias Keyframe number|string|Edges|Axes|{ value: number|string|Edges|Axes, duration?: number, easing?: Easing, [string]: "no such property" } A bare value, or a frame with its own timing. `duration = 0` jumps; repeating the previous value holds.
---@alias Spring { stiffness: number, damping: number, [string]: "no such property" } Both required: `stiffness` `(0, 100000]`, `damping` `(0, 10000]`; `2 * math.sqrt(stiffness)` is critical damping. Keeps its velocity when the target changes (ADR-0154).
---@alias Animation number|{ duration?: number, delay?: number, easing?: Easing, from?: number|string|Edges|Axes, spring?: Spring, keyframes?: Keyframe[], loops?: integer|"Infinite" } A bare number is `duration`.
--- - `duration`: ms `[1, 60000]`, required unless `spring`. `easing` defaults to `"InOutQuad"`.
--- - `delay`: ms `[0, 60000]` before it starts; offsets a sequence once, not per loop (ADR-0153).
--- - `from`: start value when the node did not display the property last pass (a new node, or one that lacked it); otherwise the first value snaps (ADR-0146). Refused beside `keyframes`.
--- - `spring`: replaces `duration`, `easing`, `keyframes` and `loops`, which are refused beside it.
--- - `keyframes`: at least 2 values, no holes, at least one segment with time; walks instead of easing to the resolved value (ADR-0152). `loops` `[1, 10000]` or `"Infinite"`, default `1`, only with `keyframes`. Bind `animate` to start or stop one.
---@alias Animations table<string, Animation> Property name to animation. Names the node does not accept, `z` and `animate` are refused. Numbers, percents, colours and numeric `Edges`/`Axes` tween against the same shape; anything else snaps.
---@alias Exit { duration?: number, delay?: number, easing?: Easing, spring?: Spring, [string]: any } `animate.exit`: timing as in `Animation` (`duration` or `spring` required once a target is named) plus `property = target` pairs the node eases to after a pass drops it (ADR-0150). A target starts from the shown value, or from the identity: `1` for `opacity`/`scale`, `0.5` for `origin`, alpha 0 for a colour, `0` otherwise.

---[docs]({DOCS}nodes/index.html#common-properties)
---@class NodeBase
{NodeBase}
---@field [string] "no such property"

---Box paint for `rect`, `row`, `column`, `button` and every surface role.
---[docs]({DOCS}guide/paint.html#box-properties)
---@class BoxBase
{BoxBase}

---One styled stretch of `text.content` (ADR-0104). A notification body's text spans fit as-is;
---drop image spans, which have no `text` and are refused.
---@class TextRun
---@field text string Empty runs are skipped.
---@field bold? boolean Uses the family's bold face when fontconfig has one.
---@field italic? boolean Uses the family's italic face when fontconfig has one.
---@field underline? boolean Underline in the run's colour.
---@field color? Color Overrides the node's `foreground`.
---@field href? string Passed to the node's `on_link` when clicked; never opened by the engine (ADR-0106).

---`image.transition`. Unknown keys are refused.
---@class Transition
---@field duration number Required, ms `[1, 60000]`.
---@field easing? Easing Default `"InOutQuad"`; drives `u_progress`.
---@field shader? string Absolute `.frag` path replacing the built-in dissolve, e.g. `mantle.config_dir .. "/shaders/wipe.frag"` (ADR-0184). Recompiled when the file changes.
--- Shader contract. The engine prepends `#version 300 es`, `highp` precision, its declarations and `#line 1`; write `void main()`:
--- - `v_uv`: box coordinates `0..1`, top-left origin, y down.
--- - `u_progress`: eased progress, clamped to `0..1`. `u_size`: node size in logical px.
--- - `mantle_from(uv)`, `mantle_to(uv)`: outgoing and incoming pictures, premultiplied and already placed by `fit`; transparent outside the picture.
--- - `u_from_rect`, `u_to_rect`: each picture's `(x, y, w, h)` in box fractions (may exceed `0..1` under `"cover"`).
--- - Output: premultiplied RGBA in `fragColor`, same colour space as the inputs. The engine applies `opacity` after.
--- - Names starting `u_` or `mantle_` are reserved. A shader that fails to compile or link, or declares a uniform other than `float`/`vec2`-`vec4`, logs once and falls back to the dissolve. A shader that hangs the GPU hangs the session.
---@field params? table<string, number|number[]> Uniform values by name: a finite number for `float`, 2-4 numbers for `vec2`-`vec4`. Missing uniforms are `0`; unknown names are ignored. Refused without `shader`.
---@field [string] "no such property"
"##;

const SURFACES_HEADER: &str = r##"---@meta
-- The four surface roles (ADR-0040), one constructor each. `shell.lua` returns the set, re-read on
-- every reload (ADR-0038). A root takes `rect`'s node and box properties, plus its own topology.
--
-- GENERATED on `nodes.lua`'s terms. Structural fields, the ones without `Bound` (`id`, `layer`,
-- `anchor`, `monitor`, `namespace`, a popup's `parent`), refuse a `Signal`: they are read once per
-- evaluation (ADR-0216).

---@alias Rect { x: number, y: number, width: number, height: number, [string]: "no such property" }
---@alias PopupAnchor {POPUP_ANCHOR}
"##;
