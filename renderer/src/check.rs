//! `mantle check`: evaluate config, lay it out, report declared surfaces, and exit through the
//! Renderer's Lua loader. The Supervisor has no `mlua` runtime, so it re-execs this binary with
//! `shared::CHECK_ENV` and forwards the exit code.
//!
//! No Wayland, surfaces, or GPU: layout runs through the production `Scene` apply on stand-in
//! outputs. Matches pre-first-`StateSnapshot` evaluation: every capability signal reads `nil`
//! (ADR-0044).

use std::path::Path;

use crate::layout::instance::{OutputGeometry, expand_instances};
use crate::layout::node::SurfaceSpec;
use crate::layout::scene::{LogicalSize, Scene};
use crate::lua::LoadOutput;
use crate::lua::capability::CommandSender;
use crate::lua::palette::PaletteRegistry;
use crate::lua::process::ProcessRegistry;
use crate::lua::signal::DirtyFlag;
use crate::lua::{Loader, namespace, surfaces::evaluate_and_specs};
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
pub fn run(config_dir: &Path) -> Result<String, String> {
    let shell_lua = config_dir.join("shell.lua");
    let (output, specs, loader) = evaluate(config_dir)?;
    lay_out(&output, &specs, &loader, &ShapingHandle::spawn())
        .map_err(|err| format!("{}: {err}", shell_lua.display()))?;
    let mut report = format!("{}: ok, {} surface(s)\n", shell_lua.display(), specs.len());
    for spec in &specs {
        report.push_str(&format!("  {:<7} {}\n", role_of(spec), spec.declared_id()));
    }
    Ok(report)
}

/// Lays the evaluated scene out through the production `Scene::apply_locked` on one 1920x1080
/// output, plus one per distinct `monitor` a panel names, so a monitor-pinned panel is laid out too.
fn lay_out(output: &LoadOutput, specs: &[SurfaceSpec], loader: &Loader, shaping: &ShapingHandle) -> Result<(), String> {
    let size = LogicalSize { width: 1920.0, height: 1080.0 };
    let mut outputs = vec![OutputGeometry { name: "CHECK".into(), size }];
    for spec in specs {
        if let SurfaceSpec::Panel(panel) = spec
            && !matches!(panel.topology.monitor.as_str(), "All" | "Active")
            && outputs.iter().all(|output| output.name != panel.topology.monitor)
        {
            outputs.push(OutputGeometry { name: panel.topology.monitor.clone(), size });
        }
    }
    let instances = expand_instances(specs, &outputs);
    Scene::new()
        .apply_locked(&output.surfaces, &instances, shaping, loader.lua(), false)
        .map_err(|err| format!("layout: {err}"))
}

/// `run`'s evaluation, returning the `Loader` last: the node tables in `LoadOutput` live in its
/// Lua state, so it has to outlive them.
fn evaluate(config_dir: &Path) -> Result<(LoadOutput, Vec<SurfaceSpec>, Loader), String> {
    let shell_lua = config_dir.join("shell.lua");
    if !shell_lua.is_file() {
        return Err(format!(
            "{}: no shell.lua. `mantle init -c {}` writes one.",
            shell_lua.display(),
            config_dir.display()
        ));
    }

    let dirty = DirtyFlag::new();
    let loader = Loader::new(dirty.clone(), config_dir).map_err(|err| format!("{}: {err}", shell_lua.display()))?;

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
    namespace::build(&loader, &dirty, &commands, &shell_lua).map_err(|err| err.to_string())?;

    let (output, specs) =
        evaluate_and_specs(&loader, &shell_lua).map_err(|err| format!("{}: {err}", shell_lua.display()))?;
    Ok((output, specs, loader))
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

    /// A panel pinned to a named monitor still lays out, so its errors are caught too.
    #[test]
    fn a_layout_error_on_a_monitor_pinned_panel_fails_the_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shell.lua"),
            "return { panel { id = \"bar\", layer = \"Top\", monitor = \"DP-1\", child = rect { width = \"Wide\", height = 10 } } }\n",
        )
        .unwrap();
        let err = super::run(dir.path()).unwrap_err();
        assert!(err.contains("shell.lua: layout:"), "{err}");
    }
}

/// Every ```` ```lua ```` block under `docs/` evaluates and lays out as `mantle check` does. The fence
/// tags are documented in `docs/development/documenting.md`.
#[cfg(test)]
mod doc_examples {
    use std::path::{Path, PathBuf};

    use crate::text::shaping::ShapingHandle;

    fn pages(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pages(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "md") {
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

    /// `(1-based fence line, info string, body)` for each fence whose info string is `lua` or
    /// starts `lua,`. Other fences are tracked only so their bodies are not read as fences.
    fn lua_blocks(markdown: &str) -> Vec<(usize, String, String)> {
        let mut blocks = Vec::new();
        let mut open: Option<(usize, String, String)> = None;
        for (index, raw) in markdown.lines().enumerate() {
            let line = unquoted(raw);
            let fence = line.trim_start().strip_prefix("```").map(str::trim);
            match (&mut open, fence) {
                (Some(_), Some("")) => {
                    blocks.extend(open.take().filter(|(_, info, _)| info == "lua" || info.starts_with("lua,")))
                }
                (Some((_, _, body)), _) => body.extend([line, "\n"]),
                (None, Some(info)) => open = Some((index + 1, info.to_string(), String::new())),
                (None, None) => {}
            }
        }
        blocks
    }

    /// `block` as a `shell.lua`. One that returns a single non-surface node is mounted in a panel,
    /// so a widget example needs no surface around it. `load` tries the block as an expression
    /// first, as the loader's own `eval` does.
    fn shell(block: &str) -> String {
        let level = (0..).map(|count| "=".repeat(count)).find(|level| !block.contains(&format!("]{level}]"))).unwrap();
        format!(
            r#"local block = [{level}[
{block}]{level}]
local chunk = load("return " .. block, "=block") or assert(load(block, "=block"))
local root = chunk()
local surfaces = {{ panel = true, window = true, popup = true, lock = true }}
if type(root) == "table" and root.kind and not surfaces[root.kind] then
    return panel {{ id = "doc", layer = "Top", child = root }}
end
return root
"#
        )
    }

    fn lay_out(block: &str, shaping: &ShapingHandle) -> Result<(), String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shell.lua"), shell(block)).unwrap();
        let (output, specs, loader) = super::evaluate(dir.path())?;
        super::lay_out(&output, &specs, &loader, shaping)
    }

    #[test]
    fn every_lua_block_in_the_docs_evaluates_and_lays_out() {
        let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs");
        let mut paths = Vec::new();
        pages(&docs, &mut paths);
        paths.sort();
        let shaping = ShapingHandle::spawn();
        let mut failures = Vec::new();
        for path in &paths {
            for (line, info, source) in lua_blocks(&std::fs::read_to_string(path).unwrap()) {
                let outcome = match info.as_str() {
                    "lua" | "lua,must-fail" => lay_out(&source, &shaping),
                    "lua,fragment" => {
                        mlua::Lua::new().load(&source).into_function().map(drop).map_err(|err| err.to_string())
                    }
                    "lua,no-check" => continue,
                    _ => Err(format!("unknown fence tag `{info}`")),
                };
                let at = format!("docs/{}:{line}", path.strip_prefix(&docs).unwrap().display());
                match (outcome, info == "lua,must-fail") {
                    (Ok(()), true) => failures.push(format!("{at}: a `lua,must-fail` block passed")),
                    (Err(err), false) => failures.push(format!("{at}: {err}")),
                    _ => {}
                }
            }
        }
        assert!(failures.is_empty(), "{} doc block(s) failed:\n{}", failures.len(), failures.join("\n"));
    }
}
