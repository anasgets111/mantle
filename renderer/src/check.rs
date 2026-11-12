//! `mantle check`: evaluate config, lay it out, report declared surfaces, and exit through the
//! Renderer's Lua loader. The Supervisor has no `mlua` runtime, so it re-execs this binary with
//! `shared::CHECK_ENV` and forwards the exit code.
//!
//! No Wayland, surfaces, or GPU: layout runs through the production `Scene` apply on stand-in
//! outputs. Evaluation matches a boot's, before the first `StateSnapshot`: every capability signal
//! reads `nil` (ADR-0044). Layout then runs again on each sample set (ADR-0267).

use std::path::Path;

use crate::layout::instance::{OutputGeometry, SurfaceInstance, expand_instances};
use crate::layout::node::{self, ClipShape, PaintStyle, PanelSpec, SurfaceSpec, SurfaceTopology};
use crate::layout::paint::shadow_reach;
use crate::layout::scene::{LogicalSize, Scene};
use crate::lua::LoadOutput;
use crate::lua::capability::CommandSender;
use crate::lua::file::FileRegistry;
use crate::lua::namespace::{self, Namespace};
use crate::lua::palette::PaletteRegistry;
use crate::lua::process::ProcessRegistry;
use crate::lua::signal::DirtyFlag;
use crate::lua::{Loader, surfaces::evaluate_and_specs};
use crate::text::shaping::ShapingHandle;

/// Evaluates and lays out `shell.lua` under `config_dir` and returns the report, or the error a
/// config author needs to read.
///
/// Lays out with every capability `nil`, as a boot does, then after one push per capability of each
/// sample set (ADR-0267): the first, so an `each` runs on a row, the alternate, so a `false` or
/// last-variant branch runs, and the empty, so an empty-list branch runs. Each failing pass is
/// reported.
pub fn run(config_dir: &Path) -> Result<String, String> {
    check(config_dir).map_err(|err| with_source_lines(config_dir, &err))
}

/// Puts the trimmed source line under each error line that starts with `<file>.lua:<line>:`, since
/// Lua gives no column to point at. Traceback frames (tab-led) and unreadable files stay as they are.
fn with_source_lines(config_dir: &Path, err: &str) -> String {
    let mut out = Vec::new();
    for line in err.lines() {
        out.push(line.to_owned());
        let at = line.trim_start().strip_prefix(&format!("{}: ", config_dir.display())).unwrap_or(line.trim_start());
        let mut parts = at.splitn(3, ':');
        let (Some(file), Some(number), Some(_)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let relative = Path::new(file).components().all(|part| matches!(part, std::path::Component::Normal(_)));
        let Ok(number) = number.parse::<usize>() else { continue };
        if !relative || !file.ends_with(".lua") || line.starts_with('\t') {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(config_dir.join(file)) else { continue };
        if let Some(code) = source.lines().nth(number.saturating_sub(1)).map(str::trim).filter(|code| !code.is_empty())
        {
            out.push(format!("    | {code}"));
        }
    }
    out.join("\n")
}

fn check(config_dir: &Path) -> Result<String, String> {
    let shell_lua = config_dir.join("shell.lua");
    let (output, specs, namespace, loader) = evaluate(config_dir)?;
    let size = LogicalSize { width: 1920.0, height: 1080.0 };
    let warnings =
        lay_out_passes(&output, &specs, &namespace, &loader, &ShapingHandle::spawn(), size).map_err(|failures| {
            failures.iter().map(|failure| format!("{}: {failure}", config_dir.display())).collect::<Vec<_>>().join("\n")
        })?;
    let mut report = format!("{}: ok, {} surface(s)\n", shell_lua.display(), specs.len());
    for spec in &specs {
        report.push_str(&format!("  {:<7} {}\n", spec.role(), spec.declared_id()));
    }
    for warning in warnings {
        report.push_str(&format!("warning: {warning}\n"));
    }
    Ok(report)
}

/// Lays out with every capability `nil`, then after [`push_samples`] of each sample set; each
/// failing pass is one `<error>` entry, which may span lines, then `(<passes>)` on its own line,
/// unless every pass failed alike: a static error does not depend on the data. On success, the
/// warnings of every pass, one per cut shadow ([`shadow_cuts`]).
fn lay_out_passes(
    output: &crate::lua::LoadOutput,
    specs: &[SurfaceSpec],
    namespace: &Namespace,
    loader: &Loader,
    shaping: &ShapingHandle,
    size: LogicalSize,
) -> Result<Vec<String>, Vec<String>> {
    let mut cuts = Cuts::new();
    let mut failures: Vec<(String, Vec<&str>)> = Vec::new();
    let passes = [
        ("before capability data", None),
        ("with sample capability data", Some("first")),
        ("with alternate sample data", Some("alternate")),
        ("with empty sample lists", Some("empty")),
    ];
    let shape = sample_shape();
    for (pass, set) in passes {
        if let Some(set) = set {
            push_samples(namespace, loader, set, &shape)
                .map_err(|err| vec![format!("{pass}: {}", crate::lua::location::describe(&err))])?;
        }
        match lay_out(output, specs, loader, shaping, size) {
            Ok((scene, instances)) => shadow_cuts(&scene, &instances, specs, &mut cuts),
            Err(err) => match failures.iter_mut().find(|(seen, _)| *seen == err) {
                Some((_, in_passes)) => in_passes.push(pass),
                None => failures.push((err, vec![pass])),
            },
        }
    }
    let each = |(err, in_passes): (String, Vec<&str>)| {
        if in_passes.len() == passes.len() { err } else { format!("{err}\n({})", in_passes.join(", ")) }
    };
    if !failures.is_empty() {
        return Err(failures.into_iter().map(each).collect());
    }
    Ok(cuts.into_iter().map(|(node, sides)| format!("{node}'s {}", cut_line(&sides))).collect())
}

/// A cut under this many px stays quiet. A blurred shadow's 3 sigma tail fades to nothing and a
/// 1 to 3 px spread is a hairline; 4 px is the first cut that reads as an edge.
const SHADOW_CUT_MIN: f32 = 4.0;

/// Per `<role> "<surface>": <node>`: the worst `(missing, needed, available)` px seen on each of
/// above, right, below, left, so the passes and the outputs of one panel make one line.
type Cuts = std::collections::BTreeMap<String, [Option<(f32, i32, i32)>; 4]>;

/// `needs 54 px above, 90 below (the surface has 0 on each)`; the room is per side when it differs.
fn cut_line(sides: &[Option<(f32, i32, i32)>; 4]) -> String {
    let cut: Vec<_> = ["above", "right", "below", "left"]
        .into_iter()
        .zip(sides)
        .filter_map(|(s, c)| Some((s, c.as_ref()?)))
        .collect();
    let same = cut.iter().all(|(_, (_, _, has))| *has == cut[0].1.2);
    let parts: Vec<String> = cut
        .iter()
        .enumerate()
        .map(|(i, (side, (_, need, has)))| {
            let px = if i == 0 { " px" } else { "" };
            if same { format!("{need}{px} {side}") } else { format!("{need}{px} {side} (has {has})") }
        })
        .collect();
    let room = match (same, cut.len()) {
        (false, _) => String::new(),
        (true, 1) => format!(" (the surface has {})", cut[0].1.2),
        (true, _) => format!(" (the surface has {} on each)", cut[0].1.2),
    };
    format!("shadow is cut: needs {}{room}; grow the surface and pad the content", parts.join(", "))
}

/// Records every outer shadow that reaches past its surface's bounds by [`SHADOW_CUT_MIN`] or
/// more, on a side the output does not already cut (a panel anchored there with margin <= 0).
/// The reach is paint's own: `shadow_rect` grown by 3 sigma. A `clip` below the root cuts first,
/// deliberately, so nothing under one is checked.
fn shadow_cuts(scene: &Scene, instances: &[SurfaceInstance], specs: &[SurfaceSpec], cuts: &mut Cuts) {
    for instance in instances {
        let (Some(root), Some(spec)) = (
            scene.surface(&instance.instance_id),
            specs.iter().find(|spec| spec.declared_id() == instance.declared_id),
        ) else {
            continue;
        };
        // top, right, bottom, left
        let at_output_edge = match spec {
            SurfaceSpec::Panel(PanelSpec { topology: SurfaceTopology { anchor: a, .. }, margin: m, .. }) => {
                [a.top && m.top <= 0.0, a.right && m.right <= 0.0, a.bottom && m.bottom <= 0.0, a.left && m.left <= 0.0]
            }
            _ => [false; 4],
        };
        let (width, height) = (root.rect.width, root.rect.height);
        let mut stack = vec![(root, 0.0, 0.0)];
        while let Some((node, ox, oy)) = stack.pop() {
            let rect = node.at(ox, oy);
            let mut name = None;
            for shadow in node.layout_style.effect.shadows.iter().filter(|shadow| !shadow.inset) {
                let seen = shadow_reach(rect, *shadow);
                let (right, bottom) = (rect.x + rect.width, rect.y + rect.height);
                // top, right, bottom, left: (room the shadow needs, room the surface has)
                let sides = [
                    (rect.y - seen.y, rect.y),
                    (seen.x + seen.width - right, width - right),
                    (seen.y + seen.height - bottom, height - bottom),
                    (rect.x - seen.x, rect.x),
                ];
                for (cut, (need, has)) in sides.into_iter().enumerate() {
                    if at_output_edge[cut] || need - has < SHADOW_CUT_MIN {
                        continue;
                    }
                    let key = name.get_or_insert_with(|| {
                        let named = match (node::fields::common::id.read(&node.properties), node.site) {
                            (Ok(Some(id)), _) => format!(" \"{id}\""),
                            (_, Some(site)) => format!(" at {site}"),
                            _ => String::new(),
                        };
                        format!("{} \"{}\": {}{named}", spec.role(), instance.declared_id, node.kind)
                    });
                    let worst = &mut cuts.entry(key.clone()).or_default()[cut];
                    if worst.is_none_or(|(missing, ..)| need - has > missing) {
                        *worst = Some((need - has, need.ceil() as i32, has.max(0.0).floor() as i32));
                    }
                }
            }
            let clips = !std::ptr::eq(node, root)
                && matches!(node.paint, Some(PaintStyle::Box { clip: ClipShape::Box | ClipShape::Rounded, .. }));
            if !clips {
                stack.extend(
                    node.children
                        .iter()
                        .filter(|child| child.visible && !child.leaving)
                        .map(|child| (child, rect.x, rect.y)),
                );
            }
        }
    }
}

/// Lays the evaluated scene out through the production `Scene::apply_locked` on one `size` output
/// named `DP-1`, the name a screenshot shows, plus one per other `output` a panel names, so a
/// monitor-pinned panel is laid out too.
fn lay_out(
    output: &crate::lua::LoadOutput,
    specs: &[SurfaceSpec],
    loader: &Loader,
    shaping: &ShapingHandle,
    size: LogicalSize,
) -> Result<(Scene, Vec<SurfaceInstance>), String> {
    let mut outputs = vec![OutputGeometry { name: "DP-1".into(), size }];
    for spec in specs {
        if let SurfaceSpec::Panel(panel) = spec
            && !matches!(panel.topology.output.as_str(), "all" | "active")
            && outputs.iter().all(|output| output.name != panel.topology.output)
        {
            outputs.push(OutputGeometry { name: panel.topology.output.clone(), size });
        }
    }
    let instances = expand_instances(specs, &outputs);
    let mut scene = Scene::new();
    scene
        .apply_locked(&output.surfaces, &instances, shaping, loader.lua(), false, true)
        .map_err(|err| err.to_string())?;
    Ok((scene, instances))
}

/// One sample `StateSnapshot` payload per capability from `set` (`first`, `alternate` or
/// `empty`), generated at build time from shared schemas.
pub(crate) fn samples(set: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut sets: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(include_str!(concat!(env!("OUT_DIR"), "/check_samples.json")))
            .expect("the generated samples are a JSON object");
    match sets.remove(set) {
        Some(serde_json::Value::Object(samples)) => samples,
        _ => panic!("no `{set}` sample set"),
    }
}

/// One `StateSnapshot`-shaped push per capability from [`samples`] of `set`, each payload
/// [`strict`]. An `on_change` handler's raise fails the pass, where a running shell only logs it.
/// ponytail: each set takes one value per field, so boolean combinations and enum variants between
/// the first and last never run; a pass per combination or variant is the upgrade.
fn push_samples(namespace: &Namespace, loader: &Loader, set: &str, shape: &serde_json::Value) -> mlua::Result<()> {
    for (capability, payload) in &samples(set) {
        let Some(handle) = namespace.capabilities.get(capability) else { continue };
        let value = crate::lua::json::to_lua(loader.lua(), payload)?;
        strict(loader.lua(), &value, &shape[capability], &format!("mantle.{capability}:get()"))?;
        let previous = handle.hydrate(value);
        handle.check_change(loader.lua(), previous)?;
    }
    Ok(())
}

/// Every sample set merged: each object holds the keys it has in any set, each array one element
/// holding its items' keys, so both tagged variants the first and alternate sets pick count.
fn sample_shape() -> serde_json::Value {
    fn merge(into: &mut serde_json::Value, from: &serde_json::Value) {
        use serde_json::Value;
        match from {
            Value::Object(fields) => {
                if !into.is_object() {
                    *into = Value::Object(serde_json::Map::new());
                }
                let Value::Object(into) = into else { return };
                for (field, value) in fields {
                    merge(into.entry(field.as_str()).or_insert(Value::Null), value);
                }
            }
            Value::Array(items) => {
                if !into.is_array() {
                    *into = Value::Array(vec![Value::Null]);
                }
                for item in items {
                    merge(&mut into[0], item);
                }
            }
            _ => {}
        }
    }
    let mut shape = serde_json::Value::Null;
    for set in ["first", "alternate", "empty"] {
        merge(&mut shape, &serde_json::Value::Object(samples(set)));
    }
    shape
}

/// Makes every struct table in a sample payload raise on a key its `shape` lacks, naming the
/// nearest, so a config reading a removed or misspelled field fails the check rather than reading
/// `nil`. Arrays and maps (a lone `sample` key) take any key, their items checked; a running
/// shell's payloads stay plain.
fn strict(lua: &mlua::Lua, value: &mlua::Value, shape: &serde_json::Value, path: &str) -> mlua::Result<()> {
    let mlua::Value::Table(table) = value else { return Ok(()) };
    match shape {
        serde_json::Value::Array(item) => {
            for (at, element) in table.clone().sequence_values::<mlua::Value>().enumerate() {
                strict(lua, &element?, &item[0], &format!("{path}[{}]", at + 1))?;
            }
        }
        // A map: any key reads, and each value is checked against the one sample value.
        serde_json::Value::Object(fields) if fields.keys().eq(["sample"]) => {
            for pair in table.clone().pairs::<mlua::Value, mlua::Value>() {
                let (key, inner) = pair?;
                let key = key.to_string().unwrap_or_default();
                strict(lua, &inner, &fields["sample"], &format!("{path}[\"{key}\"]"))?;
            }
        }
        serde_json::Value::Object(fields) if !fields.is_empty() => {
            for (field, inner) in fields {
                strict(lua, &table.raw_get(field.as_str())?, inner, &format!("{path}.{field}"))?;
            }
            let (keys, path): (Vec<String>, String) = (fields.keys().cloned().collect(), path.to_owned());
            let index = lua.create_function(move |lua, (_, key): (mlua::Value, mlua::Value)| {
                let mlua::Value::String(key) = key else { return Ok(mlua::Value::Nil) };
                let (key, keys) = (key.to_string_lossy(), keys.iter().map(String::as_str).collect::<Vec<_>>());
                // The other tagged variant's field, absent from this one.
                if keys.contains(&key.as_str()) {
                    return Ok(mlua::Value::Nil);
                }
                let hint = crate::lua::hint::hint(&key, &keys);
                Err(crate::lua::location::located(lua, mlua::Error::runtime(format!("{path} has no `{key}`: {hint}"))))
            })?;
            let meta = lua.create_table()?;
            meta.set("__index", index)?;
            table.set_metatable(Some(meta))?;
        }
        _ => {}
    }
    Ok(())
}

/// `run`'s evaluation, returning the `Loader` last: the node tables in `LoadOutput` live in its
/// Lua state, so it has to outlive them.
fn evaluate(config_dir: &Path) -> Result<(LoadOutput, Vec<SurfaceSpec>, Namespace, Loader), String> {
    let shell_lua = config_dir.join("shell.lua");
    if !shell_lua.is_file() {
        return Err(format!(
            "{}: no shell.lua. `mantle init -c {}` writes one.",
            shell_lua.display(),
            config_dir.display()
        ));
    }

    let dirty = DirtyFlag::new();
    let loader = Loader::new(dirty.clone(), config_dir).map_err(|err| format!("{}: {err}", config_dir.display()))?;

    // Register `mantle`, `process.run` and `palette.quantize`: configs reach for all three during
    // evaluation, and a bare `Loader` dies on the first `mantle.` access. Capabilities read `nil`,
    // as at real boot before the first snapshot.
    //
    // Frames go into an undrained channel: without a Supervisor, `process.run` and every action have
    // nowhere to run. Correct for a checker that evaluates, but does not start, a config. Nothing
    // polls `palette.quantize` here either. The VM holds the receiver, so an action from a handler
    // the sample passes run still queues instead of logging that the writer is gone.
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel::<shared::RendererFrame>();
    loader.lua().set_app_data(outbound_rx);
    let commands = CommandSender::new(0, outbound_tx);
    crate::lua::process::register(loader.lua(), ProcessRegistry::new(commands.clone()))
        .map_err(|err| err.to_string())?;
    crate::lua::palette::register(loader.lua(), PaletteRegistry::new(None)).map_err(|err| err.to_string())?;
    crate::lua::file::register(loader.lua(), FileRegistry::checking()).map_err(|err| err.to_string())?;
    let namespace = namespace::build(&loader, &dirty, &commands, &shell_lua).map_err(|err| err.to_string())?;

    let (output, specs) =
        evaluate_and_specs(&loader, &shell_lua).map_err(|err| format!("{}: {err}", config_dir.display()))?;
    Ok((output, specs, namespace, loader))
}

#[cfg(test)]
mod tests {
    /// A config split across `require`d files, so `mantle check` resolves modules as a real boot does.
    #[test]
    fn a_config_that_evaluates_reports_each_surface_it_declares() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("modules")).unwrap();
        std::fs::write(dir.path().join("modules/bar.lua"), "return panel { id = \"bar\", layer = \"top\" }\n").unwrap();
        std::fs::write(dir.path().join("shell.lua"), "local bar = require(\"modules.bar\")\nreturn { bar }\n").unwrap();
        let report = super::run(dir.path()).expect("the config must evaluate");
        assert!(report.contains("ok, 1 surface(s)"), "{report}");
        assert!(report.contains("panel   bar"), "the bar must be in the report:\n{report}");
    }

    /// `mantle check` on a panel `lens` (`props`) holding a pill `pill` with `shadow`.
    fn shadow_report(props: &str, shadow: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let source = format!(
            "return panel {{ id = \"lens\", layer = \"top\", {props}, child = rect {{ id = \"pill\", width = \"fill\", \
             height = \"fill\", background = \"#ffffff\", radius = 60, shadows = {{ {shadow} }} }} }}\n"
        );
        std::fs::write(dir.path().join("shell.lua"), source).unwrap();
        super::run(dir.path()).expect("a cut shadow is a warning, not a failure")
    }

    const DROP: &str = r#"{ blur = 48, offset = { y = 18 } }"#;

    #[test]
    fn a_shadow_cut_at_the_surface_edge_warns_once_and_one_with_room_does_not() {
        let report = shadow_report("width = 360, height = 140", DROP);
        let line = r#"warning: panel "lens": rect "pill"'s shadow is cut: needs 54 px above, 72 right, 90 below, 72 left (the surface has 0 on each); grow the surface and pad the content"#;
        assert!(report.contains(line), "{report}");
        assert_eq!(report.matches("warning:").count(), 1, "four passes and four sides, one line: {report}");
        assert!(!shadow_report("width = 360, height = 140", "{ spread = 1 }").contains("warning"), "a hairline");
        assert!(!shadow_report("width = 360, height = 140", r#"{ blur = 48, inset = true }"#).contains("warning"));
        assert!(
            !shadow_report("width = 560, height = 340, padding = 90", DROP).contains("warning"),
            "a surface with room for the shadow"
        );
    }

    #[test]
    fn a_side_on_the_output_edge_is_left_to_the_screen() {
        let words = ["above", "right", "below", "left"];
        for (side, word) in ["top", "right", "bottom", "left"].into_iter().zip(words) {
            let anchored = format!("width = 360, height = 140, anchor = {{ {side} = true }}");
            let report = shadow_report(&anchored, DROP);
            assert!(words.iter().all(|w| report.contains(w) != (*w == word)), "{side}: {report}");
            let lifted = shadow_report(&format!("{anchored}, margin = {{ {side} = 8 }}"), DROP);
            assert!(lifted.contains(word), "a margin leaves room past the {side} edge: {lifted}");
        }
    }

    #[test]
    fn a_directory_with_no_shell_lua_says_so_and_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("no shell.lua"), "{err}");
        assert!(err.contains("mantle init"), "an error a new user hits should name the way out: {err}");
    }

    /// Every row is a config that fails `mantle check`; the substrings pin the parts an author reads.
    #[test]
    fn the_error_corpus_names_the_site_the_lua_type_and_the_fix() {
        type Row<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [&'a str]);
        let rows: &[Row] = &[
            (
                "syntax error in shell.lua",
                &[("shell.lua", "return panel {\n")],
                &["syntax error: shell.lua:2: unexpected symbol"],
            ),
            (
                "syntax error two modules deep",
                &[
                    ("shell.lua", "return require(\"a.b\")\n"),
                    ("a/b.lua", "return require(\"c.d\")\n"),
                    ("c/d.lua", "local x = 1\nlocal = \n"),
                ],
                &["c/d.lua:2:", "a/b.lua:1: in main chunk"],
            ),
            // quoted in docs/guide/cli.md
            (
                "runtime error in a module",
                &[
                    ("shell.lua", "local bar = require(\"widgets.bar\")\nlocal built = bar.build()\nreturn built\n"),
                    ("widgets/bar.lua", "local M = {}\n\nfunction M.build()\n  return nil + 1\nend\nreturn M\n"),
                ],
                &[
                    "widgets/bar.lua:4: attempt to perform arithmetic on a nil value",
                    "in function 'widgets.bar.build'",
                    "shell.lua:2: in main chunk",
                ],
            ),
            (
                "missing module",
                &[("shell.lua", "require(\"nope.missing\")\n")],
                &["shell.lua:1: module 'nope.missing' not found", "no file 'nope/missing.lua'"],
            ),
            (
                "error with a table",
                &[("shell.lua", "error({ code = 1 })\n")],
                &["error object is a table, not a string", "shell.lua:1: in main chunk"],
            ),
            (
                "error at level 2",
                &[
                    ("shell.lua", "local m = require(\"need\")\nm.need()\n"),
                    ("need.lua", "local M = {}\nfunction M.need() error(\"msg\", 2) end\nreturn M\n"),
                ],
                &["shell.lua:2: msg"],
            ),
            (
                "unknown root property",
                &[("shell.lua", "\nreturn panel { id = \"bar\", layer = \"top\", paddng = 1 }\n")],
                &["shell.lua:2: panel: no property `paddng`; did you mean `padding`?"],
            ),
            (
                "bad root value",
                &[("shell.lua", "\nreturn panel { id = \"bar\", layer = \"top\", height = \"tall\" }\n")],
                &["shell.lua:2: panel: invalid value for `height`", "got string \"tall\""],
            ),
            (
                "bad window value",
                &[("shell.lua", "\nreturn window { id = \"w\", width = \"wide\" }\n")],
                &["shell.lua:2: window: invalid value for `width`", "got string \"wide\"", "(on `w`)"],
            ),
            (
                "bad popup value",
                &[(
                    "shell.lua",
                    "\nreturn popup { id = \"p\", parent = \"bar\", anchor_rect = { x = 0, y = 0, width = 1, height = 1 }, width = \"wide\" }\n",
                )],
                &["shell.lua:2: popup: invalid value for `width`", "got string \"wide\""],
            ),
            (
                "root built in a module",
                &[
                    ("shell.lua", "return require(\"widgets.bar\")\n"),
                    ("widgets/bar.lua", "\nreturn panel { id = \"bar\", layer = \"top\", height = \"tall\" }\n"),
                ],
                &["widgets/bar.lua:2: panel: invalid value for `height`", "got string \"tall\""],
            ),
            // quoted in docs/guide/cli.md
            (
                "unknown child property",
                &[(
                    "shell.lua",
                    "local function label()\n  return text { contnet = \"hi\" }\nend\n\n-- bar\n\nreturn panel { id = \"bar\", layer = \"top\", child = row { children = { label() } } }\n",
                )],
                &[
                    "shell.lua:2: text[1]: no property `contnet`; did you mean `content`? (at panel (shell.lua:7) > row (shell.lua:7) on `bar@DP-1`)",
                ],
            ),
            (
                "bad value deep in the tree",
                &[(
                    "shell.lua",
                    "return panel { id = \"p\", layer = \"top\", child = column {\n  children = { row {\n    children = { rect { opacity = 2 } },\n  } },\n} }\n",
                )],
                &[
                    "shell.lua:3: rect[1]: invalid value for `opacity`",
                    "(at panel (shell.lua:1) > column (shell.lua:1) > row[1] (shell.lua:2) on `p@DP-1`)",
                    "must be within [0, 1], got integer 2",
                ],
            ),
            (
                "enum typo",
                &[(
                    "shell.lua",
                    "return panel { id = \"p\", layer = \"top\", child = row { align_h = \"cenetr\" } }\n",
                )],
                &["shell.lua:1: row:", "got string \"cenetr\"; did you mean `center`?"],
            ),
            (
                "enum far off",
                &[(
                    "shell.lua",
                    "return panel { id = \"p\", layer = \"top\", child = row { align_h = \"qqqqqqqq\" } }\n",
                )],
                &["shell.lua:1: row:", "got string \"qqqqqqqq\"; expected one of `start`, `center`, `end`, `stretch`"],
            ),
            (
                "nested key typo",
                &[("shell.lua", "return panel { id = \"p\", layer = \"top\", anchor = { lefft = true } }\n")],
                &["shell.lua:1: panel:", "unknown key `lefft`; did you mean `left`?"],
            ),
            (
                "nested key that is not a string",
                &[("shell.lua", "return panel { id = \"p\", layer = \"top\", anchor = { [1] = true } }\n")],
                &["unknown key `1`; expected one of `top`"],
            ),
            (
                "duplicate id",
                &[(
                    "shell.lua",
                    "local a = panel { id = \"bar\", layer = \"top\" }\nreturn { a,\n panel { id = \"bar\", layer = \"top\" } }\n",
                )],
                &["shell.lua:3: panel: invalid value for `id`", "two surfaces declare `bar` (first at shell.lua:1)"],
            ),
            (
                "return a number",
                &[("shell.lua", "return 5\n")],
                &["shell.lua's top-level return must be", "got integer 5"],
            ),
            (
                "text at the top level",
                &[("shell.lua", "return text { content = \"hi\" }\n")],
                &["shell.lua's top-level return must be", "got string \"text\""],
            ),
            // quoted in docs/guide/cli.md
            (
                "failing map",
                &[(
                    "shell.lua",
                    "-- clock\nlocal count = state(\"count\", 1)\nlocal label = count:map(function(n)\n  return n.missing\nend)\n\n-- bar\n\nreturn panel { id = \"bar\", layer = \"top\", child = text { content = label } }\n",
                )],
                &[
                    "shell.lua:9: text: invalid value for `content`: Signal getter on a `text` node failed: signal created at shell.lua:3: shell.lua:4: attempt to index a number value (local 'n') (at panel (shell.lua:9) on `bar@DP-1`)",
                    "shell.lua:4: in function <shell.lua:3>",
                ],
            ),
            (
                "wrong argument to a global",
                &[("shell.lua", "\nfonts(5)\nreturn {}\n")],
                &["shell.lua:2: fonts: bad argument #1 (chain): expected a table, got integer"],
            ),
            (
                "bad timer argument",
                &[("shell.lua", "\ntimer(\"x\", function() end)\nreturn {}\n")],
                &["shell.lua:2: timer: bad argument #1 (ms): expected a number, got string"],
            ),
            (
                "timer out of range",
                &[("shell.lua", "\ntimer(0, function() end)\nreturn {}\n")],
                &["shell.lua:2: timer: ms must be within [1, 86400000], got integer 0"],
            ),
            (
                "capability field read",
                &[("shell.lua", "\nlocal v = mantle.audio.sinks\nreturn {}\n")],
                &["shell.lua:2: mantle.audio has no `sinks`: did you mean `mantle.audio:get().sinks`?"],
            ),
            (
                "capability field far from every name",
                &[("shell.lua", "\nlocal v = mantle.audio.zzzzzzzz\nreturn {}\n")],
                &["shell.lua:2: mantle.audio has no `zzzzzzzz`: expected one of `", "`:get()` reads its state"],
            ),
            (
                "signal method argument",
                &[("shell.lua", "\nscroll(\"s\"):reveal(0)\nreturn {}\n")],
                &["shell.lua:2: signal:reveal(): index must be 1 or more, got integer 0"],
            ),
            (
                "signal method argument of the wrong type",
                &[("shell.lua", "\nscroll(\"s\"):reveal(\"x\")\nreturn {}\n")],
                &["shell.lua:2: method reveal: bad argument #1: expected an integer, got string"],
            ),
            (
                "idle method argument of the wrong type",
                &[(
                    "shell.lua",
                    "\nmantle.idle:register_threshold(\"x\", function() end, function() end)\nreturn {}\n",
                )],
                &["shell.lua:2: method register_threshold: bad argument #1: expected an integer, got string"],
            ),
            (
                "a caught method raise keeps the caller's line",
                &[(
                    "shell.lua",
                    "local sig = scroll(\"s\")\nlocal ok, msg = pcall(function()\n  sig:reveal(0)\nend)\nlocal ok2, msg2 = pcall(function()\n  sig:reveal(\"x\")\nend)\nlocal _, msg3 = pcall(function()\n  sig:reveal()\nend)\nif not tostring(msg):find(\"shell.lua:3: signal:reveal\", 1, true) or not tostring(msg2):find(\"shell.lua:6: method reveal\", 1, true) or not tostring(msg3):find(\"shell.lua:9: method reveal: bad argument #1: expected an integer, got nil\", 1, true) then error(\"unlocated: \" .. tostring(msg) .. tostring(msg2)) end\nerror(msg, 0)\n",
                )],
                &["shell.lua:3: signal:reveal(): index must be 1 or more, got integer 0"],
            ),
            (
                "a map argument of the wrong type",
                &[("shell.lua", "\nstate(\"s\", 1):map(5)\nreturn {}\n")],
                &["shell.lua:2: method map: bad argument #1: expected a function, got integer"],
            ),
            (
                "idle inhibit argument of the wrong type",
                &[("shell.lua", "\nmantle.idle:inhibit({})\nreturn {}\n")],
                &["shell.lua:2: method inhibit: bad argument #1: expected a string, got table"],
            ),
            (
                "idle cancel_threshold argument of the wrong type",
                &[("shell.lua", "\nmantle.idle:cancel_threshold(\"x\")\nreturn {}\n")],
                &["shell.lua:2: method cancel_threshold: bad argument #1: expected an integer, got string"],
            ),
            (
                "idle threshold out of range",
                &[("shell.lua", "\nmantle.idle:register_threshold(0, function() end, function() end)\nreturn {}\n")],
                &["shell.lua:2: mantle.idle:register_threshold: seconds"],
            ),
            (
                "bad action parameter",
                &[("shell.lua", "\nmantle.keyboard:switch_layout(\"x\")\nreturn {}\n")],
                &[
                    "shell.lua:2: mantle.keyboard:switch_layout: bad argument #1: expected a non-negative integer, got string \"x\"",
                ],
            ),
            (
                "action with a wrong argument count",
                &[("shell.lua", "\nmantle.audio:set_volume(1, 2, 3)\nreturn {}\n")],
                &["shell.lua:2: mantle.audio:set_volume: expects 2 arguments, got 3"],
            ),
            (
                "action value out of range",
                &[("shell.lua", "\nmantle.audio:set_default_sink(4294967296)\nreturn {}\n")],
                &[
                    "shell.lua:2: mantle.audio:set_default_sink: bad argument #1: expected a non-negative integer, got integer 4294967296 (out of range)",
                ],
            ),
            (
                "a caught raise keeps the caller's line",
                &[(
                    "shell.lua",
                    "local ok, msg = pcall(function()\n  timer(0, function() end)\nend)\nif not tostring(msg):find(\"shell.lua:2: timer\", 1, true) then error(\"unlocated: \" .. tostring(msg)) end\nerror(msg, 0)\n",
                )],
                &["shell.lua:2: timer: ms must be within [1, 86400000], got integer 0"],
            ),
            (
                "a caught bad argument keeps the caller's line",
                &[(
                    "shell.lua",
                    "local ok, msg = pcall(function()\n  fonts(1)\nend)\nif not tostring(msg):find(\"shell.lua:2: fonts\", 1, true) then error(\"unlocated: \" .. tostring(msg)) end\nerror(msg, 0)\n",
                )],
                &["shell.lua:2: fonts: bad argument #1 (chain): expected a table, got integer"],
            ),
            (
                "action argument NaN",
                &[("shell.lua", "\nmantle.audio:set_volume(59, 0 / 0)\nreturn {}\n")],
                &["shell.lua:2: mantle.audio:set_volume: bad argument #2: expected a finite number, got number nan"],
            ),
            (
                "action argument nil before another",
                &[("shell.lua", "\nmantle.folders:watch(nil, { \"a\" })\nreturn {}\n")],
                &["shell.lua:2: mantle.folders:watch: bad argument #1: expected a string, got nil"],
            ),
            (
                "action argument that quotes serde",
                &[("shell.lua", "\nmantle.keyboard:switch_layout(\"a, expected b\")\nreturn {}\n")],
                &["bad argument #1: expected a non-negative integer, got string \"a, expected b\""],
            ),
            (
                "a plain-table child leads with its parent's line",
                &[(
                    "shell.lua",
                    "\nreturn panel { id = \"p\", layer = \"top\",\n  child = { kind = \"text\", contnet = \"x\" } }\n",
                )],
                &["shell.lua:2: text: no property `contnet`; did you mean `content`? (on `p@DP-1`)"],
            ),
            (
                "a plain-table child without a kind",
                &[("shell.lua", "\nreturn panel { id = \"p\", layer = \"top\",\n  child = { content = \"x\" } }\n")],
                &["shell.lua:2: child: node table has no `kind` field (on `p@DP-1`)"],
            ),
            (
                "a plain-table child in children",
                &[(
                    "shell.lua",
                    "\nreturn panel { id = \"p\", layer = \"top\", child = column {\n  children = { text { content = \"a\" }, { content = \"x\" } } } }\n",
                )],
                &["shell.lua:2: children[2]: node table has no `kind` field (at panel (shell.lua:2) on `p@DP-1`)"],
            ),
            (
                "a typo in an each row",
                &[(
                    "shell.lua",
                    "\nreturn panel { id = \"p\", layer = \"top\", child = list {\n  items = { 1, 2, 3, 4 },\n  each = function(n) if n == 4 then return text { contnet = \"x\" } end return text { content = \"x\" } end } }\n",
                )],
                &[
                    "shell.lua:4: text: no property `contnet`; did you mean `content`? (at panel (shell.lua:2) > list (shell.lua:2) on `p@DP-1`)",
                ],
            ),
            (
                "delay of a plain value",
                &[("shell.lua", "\nlocal d = delay(5, 10)\nreturn {}\n")],
                &["shell.lua:2: delay: source must be a Signal or a `mantle` capability, got integer 5"],
            ),
            (
                "state declared twice with different seeds",
                &[("shell.lua", "state(\"a\", 1)\nstate(\"a\", 0 / 0)\nreturn {}\n")],
                &["shell.lua:2: state(\"a\", ...) is declared twice", "integer 1 then number nan"],
            ),
            (
                "action argument nested in a table",
                &[("shell.lua", "\nmantle.folders:watch(\"/tmp\", { \"a\", 5 })\nreturn {}\n")],
                &["shell.lua:2: mantle.folders:watch: bad argument #2: expected a string, got integer 5"],
            ),
            (
                "set_volume with a string",
                &[("shell.lua", "\nmantle.audio:set_volume(59, \"loud\")\nreturn {}\n")],
                &["shell.lua:2: mantle.audio:set_volume: bad argument #2: expected a number, got string \"loud\""],
            ),
            (
                "an integral float is a number",
                &[("shell.lua", "\nmantle.keyboard:switch_layout(1.0)\nreturn {}\n")],
                &["expected a non-negative integer, got number 1.0"],
            ),
            // quoted in docs/guide/agents.md
            (
                "typo in a counter",
                &[(
                    "shell.lua",
                    "local count = state(\"count\", 0)\n\n-- counter\n\n\n\n\n\n\n\n\n\n\n\nreturn panel { id = \"counter\", layer = \"top\",\n  child = text { contnet = \"x\" } }\n",
                )],
                &[
                    "shell.lua:16: text: no property `contnet`; did you mean `content`? (at panel (shell.lua:15) on `counter@DP-1`)",
                ],
            ),
        ];
        for (name, files, must) in rows {
            let dir = tempfile::tempdir().unwrap();
            for (path, source) in *files {
                let path = dir.path().join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, source).unwrap();
            }
            let err = super::run(dir.path()).unwrap_err();
            let dir_name = dir.path().display().to_string();
            for want in *must {
                assert!(err.contains(want), "{name}: missing {want:?} in:\n{err}");
            }
            assert_eq!(err.matches(&dir_name).count(), 1, "{name}: the config dir is named once:\n{err}");
            assert!(err.starts_with(&format!("{dir_name}: ")), "{name}: the config dir heads the error:\n{err}");
            assert!(!err.contains("[C]: in metamethod"), "{name}: the error handler's own frame is noise:\n{err}");
            if !must.iter().any(|want| want.contains("top-level return")) {
                assert!(!err.contains("top-level return"), "{name}: not a top-level return error:\n{err}");
            }
            // The random tempdir name can spell "u32" or "f64" by chance.
            let body = err.replace(&dir_name, "");
            for rust_form in
                ["String(", "Integer(", "usize", "f64", "i64", "i32", "u32", "u64", "converting", "C modules", "..="]
            {
                assert!(!body.contains(rust_form), "{name}: Rust form {rust_form:?} leaked:\n{err}");
            }
        }
    }

    /// Each `fixtures/errors/*.lua` is a broken `shell.lua` whose first line names a substring of the
    /// error `mantle check` must print.
    #[test]
    fn every_error_fixture_fails_with_the_message_it_names() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/errors");
        let mut seen = 0;
        for entry in std::fs::read_dir(&fixtures).unwrap() {
            let path = entry.unwrap().path();
            let source = std::fs::read_to_string(&path).unwrap();
            let want = source.lines().next().and_then(|line| line.strip_prefix("-- expect: ")).expect("first line");
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("shell.lua"), &source).unwrap();
            let err = super::run(dir.path()).expect_err(&format!("{} must fail", path.display()));
            assert!(err.contains(want), "{}: missing {want:?} in:\n{err}", path.display());
            if path.ends_with("misspelled_property.lua") {
                assert!(err.contains("\n    | return panel {"), "the source line follows the error:\n{err}");
            }
            seen += 1;
        }
        assert!(seen > 0, "no fixtures found");
    }

    /// The first set's text span and the alternate's image span both name their fields, and a
    /// map takes any key.
    #[test]
    fn a_sample_reads_either_tagged_variants_fields_and_any_map_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            "local s = mantle.notifications:map(function(n) local b = n.feed[1] and n.feed[1].body[1] \
             return tostring(b and (b.image_path or b.text)) end, '')\n\
             local a = mantle.applications:map(function(a) return tostring(a.by_app_id.firefox) end, '')\n\
             return panel { id = 'bar', layer = 'top', child = row { children = { text { content = s }, text { content = a } } } }\n",
        )
        .unwrap();
        super::run(dir.path()).expect("both variants' fields and a map key read without raising");
    }

    /// The handler list is copied before any runs, as a running shell does.
    #[test]
    fn an_on_change_that_registers_another_passes_the_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            "mantle.audio:on_change(function() mantle.audio:on_change(function() end) end)\n\
             return panel { id = 'bar', layer = 'top', child = text { content = 'x' } }\n",
        )
        .unwrap();
        super::run(dir.path()).expect("a handler may register another");
    }

    /// An action from a handler the sample passes run queues into the check VM's own channel, on
    /// a failing check too, rather than logging that the control-socket writer is gone.
    #[test]
    fn an_action_from_a_handler_queues_silently_beside_a_failing_layout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            "mantle.updates:on_change(function() mantle.updates:check() end)\n\
             return panel { id = 'bar', layer = 'top', child = rect { hittable = 'yes' } }\n",
        )
        .unwrap();
        let (output, specs, namespace, loader) = super::evaluate(dir.path()).unwrap();
        let size = super::LogicalSize { width: 1920.0, height: 1080.0 };
        let shaping = super::ShapingHandle::spawn();
        let failures = super::lay_out_passes(&output, &specs, &namespace, &loader, &shaping, size).unwrap_err();
        assert!(failures[0].contains("invalid value for `hittable`"), "{failures:?}");
        let mut frames = loader
            .lua()
            .remove_app_data::<tokio::sync::mpsc::UnboundedReceiver<shared::RendererFrame>>()
            .expect("the check VM holds its outbound receiver");
        let queued = std::iter::from_fn(|| frames.try_recv().ok());
        assert!(
            queued
                .filter(
                    |frame| matches!(frame, shared::RendererFrame::Command(command) if command.params.action == "check")
                )
                .count()
                > 1,
            "each sample pass's handler queued its `check`"
        );
    }

    /// An object with no known fields, such as a struct with none, takes any key.
    #[test]
    fn a_table_whose_shape_has_no_fields_reads_any_key() {
        let lua = mlua::Lua::new();
        let table = lua.create_table().unwrap();
        super::strict(&lua, &mlua::Value::Table(table.clone()), &serde_json::json!({}), "t").unwrap();
        assert_eq!(table.get::<mlua::Value>("anything").unwrap(), mlua::Value::Nil);
    }

    #[test]
    fn an_error_with_a_line_prints_that_source_line_under_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shell.lua"), "local x = 1\n  return panel { idd = \"bar\" }\n").unwrap();
        let err = super::run(dir.path()).unwrap_err();
        let (head, excerpt) = err.split_once('\n').unwrap();
        assert!(head.contains("shell.lua:2:") && excerpt == "    | return panel { idd = \"bar\" }", "{err}");
    }

    /// mlua's userdata dispatch adds `[C]: in upvalue '__index'` and `__mlua_index:31` frames above
    /// the config's; only the config's own line is left.
    #[test]
    fn a_traceback_keeps_only_the_config_frames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shell.lua"), "local volume = mantle.audio.volume\nreturn {}\n").unwrap();

        let err = super::run(dir.path()).unwrap_err();

        assert!(err.ends_with("\nstack traceback:\n\tshell.lua:1: in main chunk"), "{err}");
    }

    /// A panel pinned to a named monitor still lays out, so its errors are caught too.
    #[test]
    fn a_layout_error_on_a_monitor_pinned_panel_fails_the_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            "return { panel { id = \"bar\", layer = \"top\", output = \"HDMI-A-1\", child = rect { width = \"Wide\", height = 10 } } }\n",
        )
        .unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.starts_with(&format!("{}: shell.lua:1: rect: invalid value", dir.path().display())), "{err}");
        assert!(
            err.lines().next().unwrap().ends_with("on `bar@HDMI-A-1`)"),
            "a static error fails every pass and names none: {err}"
        );
    }

    /// An `each` runs only once a `list` has items, and every capability reads `nil` until
    /// its first push, so only the sample pass reaches this typo.
    #[test]
    fn a_typo_inside_an_each_fails_the_check_with_sample_capability_data() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            r#"return { panel { id = "p", layer = "top", anchor = { top = true }, width = "fill", height = 30,
    child = list {
        items = mantle.workspaces:map(function(s) return s and s.outputs or {} end),
        each = function(o) return text { contnet = o.name } end,
    } } }
"#,
        )
        .unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("with sample capability data"), "the failing pass must be named: {err}");
        assert!(err.contains("contnet"), "{err}");
    }

    #[test]
    fn a_launcher_filtering_no_display_still_checks_its_each() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            r#"return panel { id = "launcher", layer = "top", child = list {
    items = mantle.applications:map(function(s)
        local entries = {}
        for _, app in ipairs(s and s.entries or {}) do
            if not app.no_display then entries[#entries + 1] = app end
        end
        return entries
    end),
    each = function(app) return text { contnet = app.name } end,
} }
"#,
        )
        .unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("with alternate sample data"), "{err}");
        assert!(err.contains("contnet"), "{err}");
    }

    /// The first set reads `muted = true` and battery `"unknown"`, and lists one entry long, so
    /// each row below shows only under a later set.
    #[test]
    fn false_last_variant_and_empty_list_branches_fail_their_own_pass() {
        for (source, pass) in [
            (
                "mantle.audio:map(function(s) return (s and s.sinks[1] and not s.sinks[1].muted) and { 1 } or {} end)",
                "with alternate sample data",
            ),
            (
                "mantle.battery:map(function(b) return (b and b.state == 'pending_discharge') and { 1 } or {} end)",
                "with alternate sample data",
            ),
            (
                "mantle.applications:map(function(s) return (s and #s.entries == 0) and { 1 } or {} end)",
                "with empty sample lists",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("shell.lua"),
                format!(
                    "return panel {{ id = \"p\", layer = \"top\", child = list {{\n    items = {source},\n    each = function() return text {{ contnet = \"row\" }} end,\n}} }}\n"
                ),
            )
            .unwrap();
            let err = super::run(dir.path()).unwrap_err();
            assert!(err.starts_with(&format!("{}: shell.lua:", dir.path().display())), "{source}: {err}");
            assert!(err.ends_with(&format!("\n({pass})")), "{source}: {err}");
            assert_eq!(
                err.matches("contnet").count(),
                2,
                "once in the error, once in its source line: {source}: {err}"
            );
        }
    }
}

/// Every ```` ```lua ```` block under `docs/` evaluates and lays out as `mantle check` does, and every
/// `lua,shot` block matches its committed screenshot. The fence tags are documented in
/// `docs/development/documenting.md`.
#[cfg(test)]
mod doc_examples;
