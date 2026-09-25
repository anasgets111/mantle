//! `mantle check`: evaluate config, lay it out, report declared surfaces, and exit through the
//! Renderer's Lua loader. The Supervisor has no `mlua` runtime, so it re-execs this binary with
//! `shared::CHECK_ENV` and forwards the exit code.
//!
//! No Wayland, surfaces, or GPU: layout runs through the production `Scene` apply on stand-in
//! outputs. Evaluation matches a boot's, before the first `StateSnapshot`: every capability signal
//! reads `nil` (ADR-0044). Layout then runs again on sample pushes (ADR-0267).

use std::path::Path;

use crate::layout::instance::{OutputGeometry, SurfaceInstance, expand_instances};
use crate::layout::node::SurfaceSpec;
use crate::layout::scene::{LogicalSize, Scene};
use crate::lua::LoadOutput;
use crate::lua::capability::CommandSender;
use crate::lua::namespace::{self, Namespace};
use crate::lua::palette::PaletteRegistry;
use crate::lua::process::ProcessRegistry;
use crate::lua::signal::DirtyFlag;
use crate::lua::{Loader, surfaces::evaluate_and_specs};
use crate::text::shaping::ShapingHandle;

fn role_of(spec: &SurfaceSpec) -> &'static str {
    match spec {
        SurfaceSpec::Panel(_) => "panel",
        SurfaceSpec::Window(_) => "window",
        SurfaceSpec::Popup(_) => "popup",
        SurfaceSpec::Lock(_) => "lock",
    }
}

/// Evaluates and lays out `shell.lua` under `config_dir` and returns the report, or the error a
/// config author needs to read.
///
/// Lays out twice, as a boot does: once with every capability `nil`, then again after one sample
/// push per capability (ADR-0267), so an `itemfn` runs on a row. Each failing pass is reported.
pub fn run(config_dir: &Path) -> Result<String, String> {
    let shell_lua = config_dir.join("shell.lua");
    let (output, specs, namespace, loader) = evaluate(config_dir)?;
    let size = LogicalSize { width: 1920.0, height: 1080.0 };
    lay_out_both_passes(&output, &specs, &namespace, &loader, &ShapingHandle::spawn(), size).map_err(|failures| {
        failures.iter().map(|failure| format!("{}: {failure}", config_dir.display())).collect::<Vec<_>>().join("\n")
    })?;
    let mut report = format!("{}: ok, {} surface(s)\n", shell_lua.display(), specs.len());
    for spec in &specs {
        report.push_str(&format!("  {:<7} {}\n", role_of(spec), spec.declared_id()));
    }
    Ok(report)
}

/// Lays out with every capability `nil`, then again after [`push_samples`]; each failing pass is
/// one `<pass>: <error>` entry, which may span lines.
fn lay_out_both_passes(
    output: &LoadOutput,
    specs: &[SurfaceSpec],
    namespace: &Namespace,
    loader: &Loader,
    shaping: &ShapingHandle,
    size: LogicalSize,
) -> Result<(), Vec<String>> {
    let before = lay_out(output, specs, loader, shaping, size).err();
    push_samples(namespace, loader).map_err(|err| vec![format!("sample capability data: {err}")])?;
    // A static layout error fails both passes alike; name it once.
    let after = lay_out(output, specs, loader, shaping, size).err().filter(|after| Some(after) != before.as_ref());
    let failures: Vec<String> = [("before capability data", before), ("with sample capability data", after)]
        .into_iter()
        .filter_map(|(pass, err)| Some(format!("{pass}: {}", err?)))
        .collect();
    if failures.is_empty() { Ok(()) } else { Err(failures) }
}

/// Lays the evaluated scene out through the production `Scene::apply_locked` on one `size` output
/// named `DP-1`, the name a screenshot shows, plus one per other `monitor` a panel names, so a
/// monitor-pinned panel is laid out too.
fn lay_out(
    output: &LoadOutput,
    specs: &[SurfaceSpec],
    loader: &Loader,
    shaping: &ShapingHandle,
    size: LogicalSize,
) -> Result<(Scene, Vec<SurfaceInstance>), String> {
    let mut outputs = vec![OutputGeometry { name: "DP-1".into(), size }];
    for spec in specs {
        if let SurfaceSpec::Panel(panel) = spec
            && !matches!(panel.topology.monitor.as_str(), "All" | "Active")
            && outputs.iter().all(|output| output.name != panel.topology.monitor)
        {
            outputs.push(OutputGeometry { name: panel.topology.monitor.clone(), size });
        }
    }
    let instances = expand_instances(specs, &outputs);
    let mut scene = Scene::new();
    scene
        .apply_locked(&output.surfaces, &instances, shaping, loader.lua(), false)
        .map_err(|err| format!("layout: {err}"))?;
    Ok((scene, instances))
}

/// One sample `StateSnapshot` payload per capability, keyed by name: the file
/// `the_generated_stub_matches_what_is_checked_in` writes from the Supervisor's `*State` schemas.
pub(crate) fn samples() -> serde_json::Map<String, serde_json::Value> {
    serde_json::from_str(include_str!("check_samples.json")).expect("the generated samples are a JSON object")
}

/// One `StateSnapshot`-shaped push per capability from [`samples`].
fn push_samples(namespace: &Namespace, loader: &Loader) -> mlua::Result<()> {
    for (capability, payload) in &samples() {
        let Some(handle) = namespace.capabilities.get(capability) else { continue };
        let previous = handle.hydrate(loader.to_lua_value(payload)?, 1);
        handle.notify_change(loader.lua(), previous);
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
    // Frames go into an undrained channel: without a Supervisor, `process.run` has nowhere to run.
    // Correct for a checker that evaluates, but does not start, a config. Nothing polls
    // `palette.quantize` here either.
    let (outbound_tx, _outbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let commands = CommandSender::new(0, outbound_tx);
    loader.register_process(ProcessRegistry::new(commands.clone())).map_err(|err| err.to_string())?;
    loader.register_palette(PaletteRegistry::new(None)).map_err(|err| err.to_string())?;
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
        std::fs::write(dir.path().join("modules/bar.lua"), "return panel { id = \"bar\", layer = \"Top\" }\n").unwrap();
        std::fs::write(dir.path().join("shell.lua"), "local bar = require(\"modules.bar\")\nreturn { bar }\n").unwrap();
        let report = super::run(dir.path()).expect("the config must evaluate");
        assert!(report.contains("ok, 1 surface(s)"), "{report}");
        assert!(report.contains("panel   bar"), "the bar must be in the report:\n{report}");
    }

    #[test]
    fn a_directory_with_no_shell_lua_says_so_and_names_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("no shell.lua"), "{err}");
        assert!(err.contains("mantle init"), "an error a new user hits should name the way out: {err}");
    }

    #[test]
    fn a_config_that_does_not_evaluate_reports_the_lua_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shell.lua"), "return { panel { id = 1 } }\n").unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("shell.lua"), "the error must name the file: {err}");
    }

    /// The config directory once, then Lua's own `file:line`, named relative to it.
    #[test]
    fn an_error_in_a_required_module_names_the_directory_once_and_the_module_by_its_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("widgets")).unwrap();
        std::fs::write(
            dir.path().join("widgets/bar.lua"),
            "local M = {}\nfunction M.build() return nil + 1 end\nreturn M\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("shell.lua"), "local built = require(\"widgets.bar\").build()\nreturn built\n")
            .unwrap();

        let err = super::run(dir.path()).unwrap_err();

        let head =
            format!("{}: widgets/bar.lua:2: attempt to perform arithmetic on a nil value\n", dir.path().display());
        assert!(err.starts_with(&head), "{err}");
        assert_eq!(err.matches(&dir.path().display().to_string()).count(), 1, "{err}");
    }

    /// Each step of the path to a refused node names the line that built it, including a node a
    /// helper function returned.
    #[test]
    fn a_node_error_names_the_line_that_built_each_node_on_its_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            "local function label()\n\
             \x20 return text { contnet = \"hi\" }\n\
             end\n\
             return panel { id = \"p\", layer = \"Top\", child = row {\n\
             \x20 children = { row {\n\
             \x20   children = { label() },\n\
             \x20 } },\n\
             } }\n",
        )
        .unwrap();

        let err = super::run(dir.path()).unwrap_err();

        assert!(
            err.contains("row[0] (shell.lua:4) > row[0] (shell.lua:5) > children[0]: shell.lua:2: `text` has no property `contnet`"),
            "{err}"
        );
    }

    /// A getter's failure names the line that made the signal, not only the line inside its function.
    #[test]
    fn a_failing_map_names_where_the_signal_was_created() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            "local count = state(\"count\", 1)\n\
             local label = count:map(function(n)\n\
             \x20 return n.missing\n\
             end)\n\
             return panel { id = \"p\", layer = \"Top\", child = text { content = label } }\n",
        )
        .unwrap();

        let err = super::run(dir.path()).unwrap_err();

        assert!(
            err.contains(
                "text[0] (shell.lua:5) > Signal getter on a `text` node failed: signal created at shell.lua:2: \
                 shell.lua:3: attempt to index a number value (local 'n')\nstack traceback:\n\tshell.lua:3: in function <shell.lua:2>"
            ),
            "{err}"
        );
        assert!(!err.contains("[C]: in metamethod"), "the error handler's own frame is noise: {err}");
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
            "return { panel { id = \"bar\", layer = \"Top\", monitor = \"HDMI-A-1\", child = rect { width = \"Wide\", height = 10 } } }\n",
        )
        .unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains(&format!("{}: before capability data: layout:", dir.path().display())), "{err}");
    }

    /// An `itemfn` runs only once a `list` source has rows, and every capability reads `nil` until
    /// its first push, so only the sample pass reaches this typo.
    #[test]
    fn a_typo_inside_an_itemfn_fails_the_check_with_sample_capability_data() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            r#"return { panel { id = "p", layer = "Top", anchor = { top = true }, width = "Fill", height = 30,
    child = list {
        source = mantle.workspaces:map(function(s) return s and s.outputs or {} end),
        itemfn = function(o) return text { contnet = o.name } end,
    } } }
"#,
        )
        .unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("with sample capability data"), "the failing pass must be named: {err}");
        assert!(err.contains("contnet"), "{err}");
    }
}

/// Every ```` ```lua ```` block under `docs/` evaluates and lays out as `mantle check` does, and every
/// `lua,shot` block matches its committed screenshot. The fence tags are documented in
/// `docs/development/documenting.md`.
#[cfg(test)]
mod doc_examples {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use crate::image::ImageCache;
    use crate::image::capture::CaptureCache;
    use crate::layout::hit::LogicalPoint;
    use crate::layout::hover::hover_writes_at;
    use crate::layout::image_shader::ShaderStage;
    use crate::layout::instance::SurfaceInstance;
    use crate::layout::node::{PopupAnchor, PopupSpec, SurfaceSpec, popup_spec};
    use crate::layout::paint::{Shaders, build, execute, init_headless_egl, test_gl, text_painter};
    use crate::layout::scene::{LogicalSize, ResolvedNode, Scene};
    use crate::text::atlas::TextPainter;
    use crate::text::shaping::ShapingHandle;
    use crate::text::snap::PhysicalRect;
    use crate::wayland::apply_hover_write;

    /// Narrow enough that a full-width bar and its [`MARGIN`]s fit the book's 750 px column unscaled.
    const OUTPUT: LogicalSize = LogicalSize { width: 704.0, height: 396.0 };
    /// [`BACKDROP`] around what a shot paints, px.
    const MARGIN: usize = 16;
    /// The largest surface a shot paints, and the pbuffer it paints into.
    const MAX_EDGE: u32 = 2048;
    /// Per channel. NVIDIA and llvmpipe differ by at most 2; 1 px of padding, spacing or radius, or
    /// any colour change, moves some pixel further.
    const TOLERANCE: u8 = 2;
    /// Transparent pixels show as Catppuccin Mocha crust, darker than the book's base, so a surface
    /// painted in base keeps its edge.
    const BACKDROP: [u8; 3] = [0x11, 0x11, 0x1b];
    /// 2026-09-24 12:45:00 UTC: what `os.time()` returns in a shot.
    const EPOCH: u64 = 1_790_253_900;

    /// Every file under `dir` with extension `extension`.
    fn files(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path, extension, out);
            } else if path.extension().is_some_and(|ext| ext == extension) {
                out.push(path);
            }
        }
    }

    /// `line` without its blockquote markers, indentation kept.
    fn unquoted(mut line: &str) -> &str {
        while let Some(rest) = line.trim_start().strip_prefix('>') {
            line = rest.strip_prefix(' ').unwrap_or(rest);
        }
        line
    }

    /// `(1-based fence line, info string, body, the line above the fence)` for each fence.
    fn blocks(markdown: &str) -> Vec<(usize, String, String, String)> {
        let mut blocks = Vec::new();
        let mut open: Option<(usize, String, String, String)> = None;
        let mut above = "";
        for (index, raw) in markdown.lines().enumerate() {
            let line = unquoted(raw);
            let fence = line.trim_start().strip_prefix("```").map(str::trim);
            match (&mut open, fence) {
                (Some(_), Some("")) => blocks.extend(open.take()),
                (Some((_, _, body, _)), _) => body.extend([line, "\n"]),
                (None, Some(info)) => open = Some((index + 1, info.to_string(), String::new(), above.to_string())),
                (None, None) => {}
            }
            above = line;
        }
        blocks
    }

    /// `block` as a `shell.lua`. One that returns a single non-surface node is mounted in a panel,
    /// padded so a shadow shows, so a widget example needs no surface around it. `load` tries the
    /// block as an expression first, as the loader's own `eval` does.
    fn shell(block: &str) -> String {
        let level = (0..).map(|count| "=".repeat(count)).find(|level| !block.contains(&format!("]{level}]"))).unwrap();
        format!(
            r#"local block = [{level}[
{block}]{level}]
local chunk = load("return " .. block, "=block") or assert(load(block, "=block"))
local root = chunk()
local surfaces = {{ panel = true, window = true, popup = true, lock = true }}
if type(root) == "table" and root.kind and not surfaces[root.kind] then
    return panel {{ id = "doc", layer = "Top", child = column {{ padding = 24, children = {{ root }} }} }}
end
return root
"#
        )
    }

    /// What a shot prepends to its block: a clock fixed at [`EPOCH`] in UTC, a fixed `$USER` and
    /// `$HOME`, then the page's fakes.
    fn pinned(fakes: &str) -> String {
        format!(
            r#"local date, time = os.date, os.time
os.time = function(t) return t and time(t) or {EPOCH} end
os.date = function(format, at) return date("!" .. (format or "%c"):gsub("^!", ""), at or {EPOCH}) end
os.getenv = function(name) return ({{ USER = "user", HOME = "/home/user" }})[name] end
{fakes}
"#
        )
    }

    /// `lua` with every quoted absolute path whose file name is in `images` pointed at that file.
    fn with_fixture_images(mut lua: String, images: &Path) -> String {
        for entry in std::fs::read_dir(images).unwrap() {
            let fixture = entry.unwrap().path();
            let tail = format!("/{}\"", fixture.file_name().unwrap().to_string_lossy());
            let mut from = 0;
            while let Some(found) = lua[from..].find(&tail).map(|at| at + from) {
                let start = lua[..found].rfind('"').unwrap() + 1;
                if lua[start..].starts_with('/') {
                    lua.replace_range(start..found + tail.len() - 1, &fixture.display().to_string());
                }
                from = found + 1;
            }
        }
        lua
    }

    /// The `frames=` of a `<!-- shot: frames=0..400/20 -->` or `frames=0,50,100` comment, in ms, or
    /// empty for a still.
    fn frames(above: &str) -> Result<Vec<u64>, String> {
        let Some(spec) = above.trim().strip_prefix("<!-- shot:") else { return Ok(Vec::new()) };
        let bad =
            || format!("unreadable `{}`: want `<!-- shot: frames=0..400/20 -->` or `frames=0,50,100`", above.trim());
        let spec = spec.strip_suffix("-->").and_then(|spec| spec.trim().strip_prefix("frames=")).ok_or_else(bad)?;
        let number = |text: &str| text.trim().parse::<u64>().map_err(|_| bad());
        let times: Vec<u64> = match spec.split_once('/') {
            Some((range, step)) => {
                let (first, last) = range.split_once("..").ok_or_else(bad)?;
                (number(first)?..=number(last)?).step_by(number(step)?.max(1) as usize).collect()
            }
            None => spec.split(',').map(number).collect::<Result<_, _>>()?,
        };
        if times.len() < 2 || times.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(bad());
        }
        Ok(times)
    }

    /// One image: `(width, height)` and a buffer per frame, premultiplied RGBA as rendered and RGB
    /// as read back.
    type Shot = ((u32, u32), Vec<Vec<u8>>);

    /// One painted surface in a frame: where its top-left sits, its size, and its RGBA.
    type Part = ((i32, i32), (u32, u32), Vec<u8>);

    /// A headless GL context with everything a paint needs, current on the test thread.
    struct Gpu {
        painter: TextPainter,
        gl: glow::Context,
        stage: ShaderStage,
        images: ImageCache,
        captures: CaptureCache,
        paints: usize,
    }

    impl Gpu {
        fn new(shaping: &ShapingHandle) -> Self {
            // Fails rather than skips: a skip would pass with every screenshot unchecked.
            let instance = init_headless_egl(MAX_EDGE as i32, MAX_EDGE as i32)
                .expect("the docs screenshots need headless EGL (Mesa or NVIDIA)");
            let painter = text_painter(&instance, shaping, MAX_EDGE, MAX_EDGE).expect("no FemtoVG context");
            let gl = test_gl(&instance);
            let (stage, images, captures) = (ShaderStage::default(), ImageCache::inline(), CaptureCache::default());
            Gpu { painter, gl, stage, images, captures, paints: 0 }
        }

        /// `root` painted at scale 1, its laid-out box, as `((w, h), rgba)`. Only 1x: production
        /// paints at 1.0 (docs/roadmap.md, HiDPI).
        fn paint(&mut self, root: &ResolvedNode) -> Result<((u32, u32), Vec<u8>), String> {
            let (width, height) = (root.rect.width.ceil() as u32, root.rect.height.ceil() as u32);
            if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
                return Err(format!("a surface is {width}x{height}; a shot takes 1 to {MAX_EDGE} px a side"));
            }
            // As production does before each paint: a fallback face loaded by the last layout draws.
            self.painter.sync();
            self.painter.resize(width, height);
            let list = build(root, 1.0, None);
            // A key per paint: a kept layer from another paint would be reused.
            self.paints += 1;
            let shaders = Some(Shaders { gl: &self.gl, stage: &mut self.stage });
            let whole = [PhysicalRect { x0: 0, y0: 0, x1: width as i32, y1: height as i32 }];
            let size = (width as f32, height as f32);
            let key = format!("shot-{}", self.paints);
            execute(&key, &mut self.painter, &mut self.images, &mut self.captures, &list, 1.0, size, &whole, shaders);
            let image = self.painter.canvas_mut().screenshot().map_err(|err| format!("readback: {err}"))?;
            let (pixels, ..) = image.as_ref().to_contiguous_buf();
            Ok(((width, height), pixels.iter().flat_map(|px| [px.r, px.g, px.b, px.a]).collect()))
        }
    }

    /// The latest time any tween in `node` started: the instant a shot's frames count from.
    fn last_start(node: &ResolvedNode) -> Option<Instant> {
        node.tweens.iter().map(|tween| tween.started).chain(node.children.iter().filter_map(last_start)).max()
    }

    /// Every visible surface `source` declares, stacked top to bottom 8 px apart with each popup
    /// placed on its parent, at each of `frames` (ms after the last tween started), or once with
    /// every tween finished.
    ///
    /// A `fakes` global table the page's fakes set, `{ battery = {...} }`, is pushed into those
    /// capabilities before layout. After the first layout a `__pointer` table rests the pointer
    /// (see [`rest_pointer`]), a `__after` function runs, and the scene lays out again: that is how
    /// a shot shows a change, like an OSD appearing.
    fn shoot(
        source: &str,
        files: &[(&str, &str)],
        shaping: &ShapingHandle,
        gpu: &mut Gpu,
        frames: &[u64],
    ) -> Result<Shot, String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shell.lua"), source).unwrap();
        for (name, body) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let (output, specs, namespace, loader) = super::evaluate(dir.path())?;
        let lua = loader.lua();
        // The first push, as the Supervisor's would arrive: `on_change` runs too, once every fake
        // holds its value, in name order, so a handler reading another capability sees it.
        if let Ok(fakes) = lua.globals().get::<mlua::Table>("fakes") {
            let mut pushed = Vec::new();
            for pair in fakes.pairs::<String, mlua::Value>() {
                let (name, value) = pair.map_err(|err| format!("fakes: {err}"))?;
                let handle = namespace.capabilities.get(&name).ok_or(format!("fakes: no capability `{name}`"))?;
                pushed.push((name, handle, handle.hydrate(value, 1)));
            }
            pushed.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, handle, previous) in pushed {
                handle.notify_change(lua, previous);
            }
        }
        let (mut scene, instances) = super::lay_out(&output, &specs, &loader, shaping, OUTPUT)?;
        let pointer = lua.globals().get::<mlua::Table>("__pointer").ok();
        let after = lua.globals().get::<mlua::Function>("__after").ok();
        if pointer.is_some() || after.is_some() {
            // Settle the first layout's tweens, so the frames count from these changes alone rather
            // than from however long the first pass took.
            scene.tick(&instances, shaping, lua, Instant::now() + Duration::from_secs(10));
            if let Some(pointer) = pointer {
                rest_pointer(&pointer, &scene, &instances, lua)?;
            }
            if let Some(after) = after {
                after.call::<()>(()).map_err(|err| format!("__after: {err}"))?;
            }
            scene
                .apply_locked(&output.surfaces, &instances, shaping, lua, false)
                .map_err(|err| format!("layout: {err}"))?;
        }
        let start =
            instances.iter().filter_map(|instance| scene.surface(&instance.instance_id)).filter_map(last_start).max();
        let times: Vec<u64> = if frames.is_empty() { vec![10_000] } else { frames.to_vec() };
        let mut placed = Vec::new();
        for ms in times {
            if let Some(start) = start {
                scene.tick(&instances, shaping, lua, start + Duration::from_millis(ms));
            }
            // Surfaces first, stacked; then each popup on its parent, where the compositor puts it.
            let mut parts: Vec<(String, Part)> = Vec::new();
            let mut popups = Vec::new();
            let mut top = 0;
            for instance in &instances {
                let Some(root) = scene.surface(&instance.instance_id).filter(|root| root.visible) else { continue };
                let (size, rgba) = gpu.paint(root)?;
                match specs.iter().find(|spec| spec.declared_id() == instance.declared_id) {
                    Some(SurfaceSpec::Popup(_)) => {
                        popups.push((popup_spec(&root.properties).map_err(|err| format!("popup: {err}"))?, size, rgba))
                    }
                    _ => {
                        parts.push((instance.declared_id.clone(), ((0, top), size, rgba)));
                        top += size.1 as i32 + 8;
                    }
                }
            }
            for (spec, size, rgba) in popups {
                let (parent_at, parent_size) = parts
                    .iter()
                    .find(|part| part.0 == spec.parent)
                    .map(|(_, (at, size, _))| (*at, *size))
                    .ok_or(format!("popup `{}`: its parent `{}` is not shown", spec.id, spec.parent))?;
                let (x, y) = popup_origin(&spec, size, parent_size.0);
                parts.push((spec.id, ((parent_at.0 + x, parent_at.1 + y), size, rgba)));
            }
            if parts.is_empty() {
                return Err("no visible surface to show".into());
            }
            placed.push(parts.into_iter().map(|(_, part)| part).collect::<Vec<_>>());
        }
        crop(&placed)
    }

    /// `__pointer = { surface = "bar", x = 40, y = 12 }`: the pointer resting at that point of the
    /// surface, in its logical px, through the same hover writes a Wayland motion makes, so every
    /// `hover` on the path, its rect and its `on_hover` answer. `surface` defaults to the first.
    fn rest_pointer(
        pointer: &mlua::Table,
        scene: &Scene,
        instances: &[SurfaceInstance],
        lua: &mlua::Lua,
    ) -> Result<(), String> {
        let field = |err: mlua::Error| format!("__pointer: {err}");
        let surface: Option<String> = pointer.get("surface").map_err(field)?;
        let point = LogicalPoint { x: pointer.get("x").map_err(field)?, y: pointer.get("y").map_err(field)? };
        let (instance, tree) = instances
            .iter()
            .filter(|instance| surface.as_ref().is_none_or(|surface| *surface == instance.declared_id))
            .find_map(|instance| Some((instance, scene.surface(&instance.instance_id)?)))
            .ok_or(format!("__pointer: no surface `{}`", surface.unwrap_or_default()))?;
        for write in hover_writes_at(tree, Some(point)) {
            apply_hover_write(lua, write, true, &instance.instance_id);
        }
        Ok(())
    }

    /// Where an `xdg_positioner` puts a `size` popup in its parent: the `anchor` point of
    /// `anchor_rect`, the popup hung from it towards `gravity`, then `offset`. `SlideX` keeps it
    /// within the parent's width; no other adjustment is modelled.
    fn popup_origin(spec: &PopupSpec, size: (u32, u32), parent_width: u32) -> (i32, i32) {
        // 0 at the left or top edge, 1 at the right or bottom, 0.5 between.
        let edges = |side: PopupAnchor| {
            use PopupAnchor::*;
            let x = match side {
                Left | TopLeft | BottomLeft => 0.0,
                Right | TopRight | BottomRight => 1.0,
                _ => 0.5,
            };
            let y = match side {
                Top | TopLeft | TopRight => 0.0,
                Bottom | BottomLeft | BottomRight => 1.0,
                _ => 0.5,
            };
            (x, y)
        };
        let (rect, (anchor_x, anchor_y), (gravity_x, gravity_y)) =
            (spec.anchor_rect, edges(spec.anchor), edges(spec.gravity));
        let (width, height) = (size.0 as f32, size.1 as f32);
        let mut x = rect.x + rect.width * anchor_x - width * (1.0 - gravity_x) + spec.offset.x;
        let y = rect.y + rect.height * anchor_y - height * (1.0 - gravity_y) + spec.offset.y;
        if spec.constraint_adjustment.slide_x {
            x = x.min(parent_width as f32 - width).max(0.0);
        }
        (x.round() as i32, y.round() as i32)
    }

    /// Each frame's `(position, size, rgba)` parts drawn over each other in order, then every frame
    /// cropped to the box any of them paints, plus [`MARGIN`]. Fails when nothing is painted.
    fn crop(frames: &[Vec<Part>]) -> Result<Shot, String> {
        let parts = || frames.iter().flatten();
        let left = parts().map(|(at, ..)| at.0).min().unwrap();
        let top = parts().map(|(at, ..)| at.1).min().unwrap();
        let width = (parts().map(|(at, size, _)| at.0 + size.0 as i32).max().unwrap() - left) as usize;
        let height = (parts().map(|(at, size, _)| at.1 + size.1 as i32).max().unwrap() - top) as usize;
        let mut canvases = Vec::new();
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        for frame in frames {
            let mut canvas = vec![0u8; width * height * 4];
            for ((x, y), (part_width, part_height), rgba) in frame {
                let (x, y) = ((x - left) as usize, (y - top) as usize);
                for row in 0..*part_height as usize {
                    for column in 0..*part_width as usize {
                        let from = &rgba[(row * *part_width as usize + column) * 4..][..4];
                        let to = &mut canvas[((y + row) * width + x + column) * 4..][..4];
                        let under = 255 - from[3] as u32;
                        for (to, &from) in to.iter_mut().zip(from) {
                            *to = (from as u32 + (*to as u32 * under + 127) / 255).min(255) as u8;
                        }
                    }
                }
            }
            for (index, px) in canvas.as_chunks::<4>().0.iter().enumerate() {
                if px[3] > 0 {
                    let (x, y) = (index % width, index / width);
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
                }
            }
            canvases.push(canvas);
        }
        if x1 == 0 {
            return Err("every frame is empty".into());
        }
        let (out_width, out_height) = (x1 - x0 + 2 * MARGIN, y1 - y0 + 2 * MARGIN);
        let frames = canvases
            .iter()
            .map(|canvas| {
                let mut out = vec![0u8; out_width * out_height * 4];
                for row in y0..y1 {
                    let to = ((row - y0 + MARGIN) * out_width + MARGIN) * 4;
                    out[to..to + (x1 - x0) * 4]
                        .copy_from_slice(&canvas[(row * width + x0) * 4..(row * width + x1) * 4]);
                }
                out
            })
            .collect();
        Ok(((out_width as u32, out_height as u32), frames))
    }

    /// Premultiplied RGBA over [`BACKDROP`], as opaque RGB.
    fn over_backdrop(rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
        for px in rgba.as_chunks::<4>().0 {
            let under = 255 - px[3] as u32;
            out.extend(
                px[..3]
                    .iter()
                    .zip(BACKDROP)
                    .map(|(&channel, ground)| (channel as u32 + (ground as u32 * under + 127) / 255).min(255) as u8),
            );
        }
        out
    }

    /// `frames` as a PNG, or an APNG that holds each frame until the next time in `times` and the
    /// last for a second. An APNG's default image, what a viewer without APNG shows, is its last
    /// frame: the first of an entry animation is empty.
    fn write_png(path: &Path, ((width, height), frames): &Shot, times: &[u64]) {
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut encoder = png::Encoder::new(file, *width, *height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_compression(png::Compression::High);
        if frames.len() > 1 {
            encoder.set_animated(frames.len() as u32, 0).unwrap();
            encoder.set_sep_def_img(true).unwrap();
        }
        let mut writer = encoder.write_header().unwrap();
        if frames.len() > 1 {
            writer.write_image_data(&over_backdrop(frames.last().unwrap())).unwrap();
        }
        for (index, frame) in frames.iter().enumerate() {
            if frames.len() > 1 {
                let hold = times.get(index + 1).map_or(1000, |next| next - times[index]);
                writer.set_frame_delay(hold as u16, 1000).unwrap();
            }
            writer.write_image_data(&over_backdrop(frame)).unwrap();
        }
        writer.finish().unwrap();
    }

    /// Every frame of the PNG at `path` as RGB, and its size. An APNG's separate default image
    /// is left out.
    fn read_png(path: &Path) -> Option<Shot> {
        let mut decoder =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?)).read_info().ok()?;
        let count = decoder.info().animation_control().map_or(1, |control| control.num_frames);
        let separate = count > 1 && decoder.info().frame_control().is_none();
        let mut frames = Vec::new();
        for _ in 0..count + separate as u32 {
            let mut buffer = vec![0; decoder.output_buffer_size()?];
            let info = decoder.next_frame(&mut buffer).ok()?;
            buffer.truncate(info.buffer_size());
            frames.push(buffer);
        }
        frames.drain(..separate as usize);
        Some(((decoder.info().width, decoder.info().height), frames))
    }

    /// The largest channel difference between `shot` and the PNG at `path`, or `None` when it is
    /// missing or differs in size or frame count.
    fn delta(shot: &Shot, path: &Path) -> Option<u8> {
        let (size, frames) = read_png(path)?;
        if size != shot.0 || frames.len() != shot.1.len() {
            return None;
        }
        let rendered = shot.1.iter().map(|frame| over_backdrop(frame));
        Some(
            rendered
                .zip(&frames)
                .flat_map(|(ours, theirs)| ours.into_iter().zip(theirs.iter()).map(|(a, &b)| a.abs_diff(b)).max())
                .max()
                .unwrap_or(0),
        )
    }

    #[test]
    fn every_lua_block_in_the_docs_evaluates_and_lays_out() {
        let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs");
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/shots");
        let update = std::env::var_os("UPDATE_SHOTS").is_some();
        let mut paths = Vec::new();
        files(&docs, "md", &mut paths);
        paths.sort();
        let shaping = ShapingHandle::spawn_with(Some(fixtures.join("fonts/fonts.conf")));
        crate::image::icons::FIXTURE_ICONS.set(Some(fixtures.join("icons")));
        let mut gpu = None;
        let mut failures = Vec::new();
        let mut images = HashSet::new();
        for path in &paths {
            let page = path.strip_prefix(&docs).unwrap().with_extension("");
            let image_dir = docs.join("images").join(page.parent().unwrap());
            let fakes_path = image_dir.join(format!("{}.fakes.lua", page.file_name().unwrap().to_string_lossy()));
            let fakes = std::fs::read_to_string(&fakes_path).unwrap_or_default();
            let mut shots = 0;
            let blocks = blocks(&std::fs::read_to_string(path).unwrap());
            // A block under `<!-- file: shaders/glow.frag -->` is that file in every shot's config
            // directory, so a page's example files are the ones its shots load.
            let page_files: Vec<(&str, &str)> = blocks
                .iter()
                .filter_map(|(.., body, above)| {
                    Some((above.trim().strip_prefix("<!-- file:")?.strip_suffix("-->")?.trim(), body.as_str()))
                })
                .collect();
            for (line, info, source, above) in &blocks {
                if info != "lua" && !info.starts_with("lua,") {
                    continue;
                }
                let at = format!("docs/{}:{line}", page.with_extension("md").display());
                let outcome = match info.as_str() {
                    "lua" | "lua,must-fail" => lay_out(source, &shaping).map(drop),
                    "lua,shot" => {
                        shots += 1;
                        let image =
                            image_dir.join(format!("{}-{shots}.png", page.file_name().unwrap().to_string_lossy()));
                        images.insert(image.clone());
                        let gpu = gpu.get_or_insert_with(|| Gpu::new(&shaping));
                        frames(above).and_then(|times| {
                            let lua = with_fixture_images(pinned(&fakes) + &shell(source), &fixtures.join("images"));
                            let shot = shoot(&lua, &page_files, &shaping, gpu, &times)?;
                            compare(&shot, &times, &image, update)
                        })
                    }
                    "lua,fragment" => {
                        mlua::Lua::new().load(source).into_function().map(drop).map_err(|err| err.to_string())
                    }
                    "lua,no-check" => continue,
                    _ => Err(format!("unknown fence tag `{info}`")),
                };
                match (outcome, info == "lua,must-fail") {
                    (Ok(()), true) => failures.push(format!("{at}: a `lua,must-fail` block passed")),
                    (Err(err), false) => failures.push(format!("{at}: {err}")),
                    _ => {}
                }
            }
            if shots == 0 && fakes_path.exists() {
                failures.push(format!("{}: fakes for a page with no `lua,shot` block", fakes_path.display()));
            }
        }
        let mut committed = Vec::new();
        files(&docs.join("images"), "png", &mut committed);
        for orphan in
            committed.iter().filter(|path| !path.to_string_lossy().ends_with(".new.png") && !images.contains(*path))
        {
            match update {
                true => std::fs::remove_file(orphan).unwrap(),
                false => {
                    failures.push(format!("{}: no `lua,shot` block draws this image; delete it", orphan.display()))
                }
            }
        }
        assert!(failures.is_empty(), "{} doc block(s) failed:\n{}", failures.len(), failures.join("\n"));
    }

    #[test]
    fn a_resting_pointer_hovers_the_node_under_it_and_not_its_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
under, beside = hover("under"), hover("beside")
return panel { id = "bar", layer = "Top", height = 20, child = row { children = {
    rect { width = 40, height = 20, hover = under },
    rect { width = 40, height = 20, hover = beside },
} } }
"#;
        std::fs::write(dir.path().join("shell.lua"), source).unwrap();
        let (output, specs, _, loader) = super::evaluate(dir.path()).unwrap();
        let shaping = ShapingHandle::spawn_with(None);
        let (scene, instances) = super::lay_out(&output, &specs, &loader, &shaping, OUTPUT).unwrap();
        let lua = loader.lua();
        let pointer: mlua::Table = lua.load("{ surface = 'bar', x = 10, y = 10 }").eval().unwrap();

        rest_pointer(&pointer, &scene, &instances, lua).unwrap();

        let hovered = |name: &str| lua.load(format!("return {name}:get()")).eval::<bool>().unwrap();
        assert!(hovered("under"));
        assert!(!hovered("beside"));
    }

    fn lay_out(block: &str, shaping: &ShapingHandle) -> Result<(), String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shell.lua"), shell(block)).unwrap();
        let (output, specs, namespace, loader) = super::evaluate(dir.path())?;
        super::lay_out_both_passes(&output, &specs, &namespace, &loader, shaping, OUTPUT)
            .map_err(|failures| failures.join("\n"))
    }

    /// Passes when `shot` is within [`TOLERANCE`] of `image`. Otherwise writes it to `image`
    /// under `update`, else beside it as `.new.png` for review. Within tolerance, the committed
    /// file stays as it is, so driver noise never shows up in git.
    fn compare(shot: &Shot, times: &[u64], image: &Path, update: bool) -> Result<(), String> {
        let review = image.with_extension("new.png");
        let _ = std::fs::remove_file(&review);
        let delta = delta(shot, image);
        if delta.is_some_and(|delta| delta <= TOLERANCE) {
            return Ok(());
        }
        let name = image.file_name().unwrap().to_string_lossy();
        if update {
            std::fs::create_dir_all(image.parent().unwrap()).unwrap();
            write_png(image, shot, times);
            eprintln!("wrote {name}");
            return Ok(());
        }
        write_png(&review, shot, times);
        Err(match delta {
            Some(delta) => format!("{name} differs by up to {delta} per channel; compare {}", review.display()),
            None => format!("{name} is missing or a different size; the render is at {}", review.display()),
        })
    }
}
