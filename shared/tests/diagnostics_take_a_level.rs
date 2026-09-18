//! Every runtime diagnostic goes through `shared`'s level macros (ADR-0229), so the clock, the
//! subsystem and `OBELISK_LOG` all reach it. A bare `eprintln!` survives only where it runs before
//! `shared::log::init` and has no subsystem to name: argument errors, usage, `obelisk log`'s own
//! note about which run it picked.
//!
//! Here rather than beside either binary because the rule is the workspace's, and `shared/tests` is
//! where a check that spans crates lives.

use std::path::Path;

/// A Lua call whose own name opens with its subsystem. `process: process.detach: ...` reads as the
/// action that failed, not as the subsystem said twice.
const ACTION_NAMES: [&str; 1] = ["process.detach"];

/// Files whose `eprintln!` is a command talking to whoever typed it, not the shell reporting on
/// itself.
const CLI_ONLY: [&str; 4] =
    ["renderer/src/main.rs", "supervisor/src/main.rs", "supervisor/src/cli.rs", "supervisor/src/log.rs"];

#[test]
fn a_runtime_diagnostic_takes_a_level_rather_than_going_straight_to_stderr() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("shared sits inside the workspace");
    let mut offences = Vec::new();

    for crate_src in ["renderer/src", "supervisor/src"] {
        let mut paths = vec![workspace.join(crate_src)];
        while let Some(path) = paths.pop() {
            if path.is_dir() {
                let entries = std::fs::read_dir(&path).expect("readable source directory");
                paths.extend(entries.map(|entry| entry.expect("readable source entry").path()));
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let relative = path.strip_prefix(workspace).expect("under the workspace");
            if CLI_ONLY.contains(&relative.to_string_lossy().as_ref()) {
                continue;
            }
            // Everything from the first `#[cfg(test)]` on is scaffolding, where `eprintln!` is std's
            // again (both crate roots take `shared`'s only `cfg(not(test))`) and prints to the
            // harness rather than to the shell's log.
            let source = std::fs::read_to_string(&path).expect("readable source");
            let runtime = source.split("#[cfg(test)]").next().unwrap_or(&source);
            for (offset, line) in runtime.lines().enumerate() {
                if line.contains("eprintln!(") || line.contains("eprint!(") {
                    offences.push(format!("{}:{}: {}", relative.display(), offset + 1, line.trim()));
                }
            }
        }
    }

    assert!(offences.is_empty(), "a runtime diagnostic takes a level (ADR-0229):\n{}", offences.join("\n"));
}

/// A message must not open with the subsystem the logger already prints in front of it (ADR-0229).
///
/// Containment rather than equality, ignoring case and separators, because the spellings that drift
/// are the near ones: `pam worker` against `pam_worker`, `config watcher` against `watcher`.
#[test]
fn a_message_does_not_repeat_the_subsystem_the_logger_puts_in_front_of_it() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("shared sits inside the workspace");
    let mut offences = Vec::new();

    for (krate, crate_src) in [("obelisk_renderer", "renderer/src"), ("obelisk", "supervisor/src")] {
        let mut paths = vec![workspace.join(crate_src)];
        while let Some(path) = paths.pop() {
            if path.is_dir() {
                let entries = std::fs::read_dir(&path).expect("readable source directory");
                paths.extend(entries.map(|entry| entry.expect("readable source entry").path()));
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let relative = path.strip_prefix(workspace.join(crate_src)).expect("under the crate");
            let mut module = vec![krate.to_string()];
            module
                .extend(relative.with_extension("").components().map(|c| c.as_os_str().to_string_lossy().into_owned()));
            module.retain(|segment| segment != "mod");
            let subsystem = shared::log::subsystem(&module.join("::")).to_string();

            let source = std::fs::read_to_string(&path).expect("readable source");
            for (offset, line) in source.split("#[cfg(test)]").next().unwrap_or("").lines().enumerate() {
                let Some(prefix) = opening_prefix(line) else { continue };
                if ACTION_NAMES.contains(&prefix.as_str()) {
                    continue;
                }
                let flatten = |text: &str| text.to_lowercase().replace([' ', '-', '_'], "");
                if flatten(&prefix).contains(&flatten(&subsystem)) || flatten(&subsystem).contains(&flatten(&prefix)) {
                    offences.push(format!(
                        "{}:{}: {subsystem:?} is already printed, but the message opens {prefix:?}",
                        relative.display(),
                        offset + 1
                    ));
                }
            }
        }
    }

    assert!(offences.is_empty(), "the logger prints the subsystem (ADR-0229):\n{}", offences.join("\n"));
}

/// The lowercase word a levelled message opens with, before its first `": "`. Bounded in length and
/// restricted to word characters, so a sentence that merely contains a colon is not read as a prefix.
fn opening_prefix(line: &str) -> Option<String> {
    let start = ["error!(\"", "warn!(\"", "info!(\"", "debug!(\""]
        .iter()
        .find_map(|open| line.find(open).map(|at| at + open.len()))?;
    let rest = &line[start..];
    let prefix = rest.split_once(": ")?.0;
    let usable = prefix.len() <= 24
        && !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_lowercase() || " -_.".contains(c))
        && prefix.starts_with(|c: char| c.is_ascii_lowercase());
    usable.then(|| prefix.to_string())
}
