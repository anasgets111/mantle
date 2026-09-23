//! Every runtime diagnostic goes through `shared`'s level macros (ADR-0229), so the clock, the
//! subsystem and `MANTLE_LOG` all reach it. A bare `eprintln!` survives only where it runs before
//! `shared::log::init` and has no subsystem to name: argument errors, usage, `mantle log`'s own
//! note about which run it picked.
//!
//! Here rather than beside either binary because the rule is the workspace's, and `shared/tests` is
//! where a check that spans crates lives.

use std::path::Path;

/// A Lua call whose own name opens with its subsystem. `process: process.detach: ...` reads as the
/// action that failed, not as the subsystem said twice.
const ACTION_NAMES: [&str; 2] = ["process.detach", "process.run"];

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
            // Test scaffolding's `eprintln!` is std's again (both crate roots take `shared`'s only
            // `cfg(not(test))`) and prints to the harness rather than to the shell's log.
            let source = std::fs::read_to_string(&path).expect("readable source");
            for (offset, line) in runtime_lines(&source) {
                if line.contains("eprintln!(") || line.contains("eprint!(") {
                    offences.push(format!("{}:{}: {}", relative.display(), offset + 1, line.trim()));
                }
            }
        }
    }

    assert!(offences.is_empty(), "a runtime diagnostic takes a level (ADR-0229):\n{}", offences.join("\n"));
}

/// A message must not open with the subsystem the logger already prints in front of it, nor with
/// another subsystem's name, which `MANTLE_LOG` cannot filter by (ADR-0229).
///
/// Containment rather than equality, ignoring case and separators, because the spellings that drift
/// are the near ones: `pam worker` against `pam_worker`, `config watcher` against `watcher`.
#[test]
fn a_message_does_not_repeat_the_subsystem_the_logger_puts_in_front_of_it() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("shared sits inside the workspace");
    let flatten = |text: &str| text.to_lowercase().replace([' ', '-', '_'], "");
    let mut subsystems = std::collections::HashSet::new();
    let mut prefixes = Vec::new();

    for (krate, crate_src) in [("mantle_renderer", "renderer/src"), ("mantle", "supervisor/src")] {
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
            // `main` and `mod` are not module segments: `module_path!()` in either is the module
            // that contains them, and for a crate root's `main.rs` that is the crate itself.
            module.retain(|segment| segment != "mod" && segment != "main");
            let subsystem = shared::log::subsystem(&module.join("::")).to_string();
            subsystems.insert(flatten(&subsystem));

            let source = std::fs::read_to_string(&path).expect("readable source");
            let lines = runtime_lines(&source);
            for (index, &(offset, line)) in lines.iter().enumerate() {
                // A call rustfmt split puts its message on the next line.
                let joined = match lines.get(index + 1) {
                    Some((_, next)) if line.trim_end().ends_with("!(") => {
                        format!("{}{}", line.trim_end(), next.trim_start())
                    }
                    _ => line.to_string(),
                };
                let Some(prefix) = opening_prefix(&joined) else { continue };
                if ACTION_NAMES.contains(&prefix.as_str()) {
                    continue;
                }
                prefixes.push((format!("{}:{}", relative.display(), offset + 1), subsystem.clone(), prefix));
            }
        }
    }

    let offences: Vec<_> = prefixes
        .into_iter()
        .filter(|(_, subsystem, prefix)| {
            let (subsystem, prefix) = (flatten(subsystem), flatten(prefix));
            prefix.contains(&subsystem) || subsystem.contains(&prefix) || subsystems.contains(&prefix)
        })
        .map(|(at, subsystem, prefix)| format!("{at}: logged as {subsystem:?}, but the message opens {prefix:?}"))
        .collect();
    assert!(offences.is_empty(), "the logger prints the subsystem (ADR-0229):\n{}", offences.join("\n"));
}

/// `source`'s lines, numbered from zero, minus every `#[cfg(test)]` item.
///
/// ponytail: counts braces without lexing, so a lone `{` or `}` in a test's string literal ends the
/// skip early or late. Upgrade to `syn` if that ever misreads a file.
fn runtime_lines(source: &str) -> Vec<(usize, &str)> {
    let mut kept = Vec::new();
    let mut lines = source.lines().enumerate();
    while let Some((offset, line)) = lines.next() {
        if line.trim() != "#[cfg(test)]" {
            kept.push((offset, line));
            continue;
        }
        let mut depth = 0i32;
        for (_, item) in lines.by_ref() {
            depth += item.matches('{').count() as i32 - item.matches('}').count() as i32;
            if depth == 0 && (item.trim_end().ends_with(';') || item.trim_end().ends_with('}')) {
                break;
            }
        }
    }
    kept
}

/// The lowercase word a levelled message opens with, before its first `": "`. Bounded in length and
/// restricted to word characters, so a sentence that merely contains a colon is not read as a prefix.
fn opening_prefix(line: &str) -> Option<String> {
    let start = ["error!(\"", "warn!(\"", "notice!(\"", "info!(\"", "debug!(\"", "debug!(1; \"", "debug!(2; \""]
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
