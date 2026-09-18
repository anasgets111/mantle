//! Every runtime diagnostic goes through `shared`'s level macros (ADR-0229), so the clock, the
//! subsystem and `OBELISK_LOG` all reach it. A bare `eprintln!` survives only where it runs before
//! `shared::log::init` and has no subsystem to name: argument errors, usage, `obelisk log`'s own
//! note about which run it picked.
//!
//! Here rather than beside either binary because the rule is the workspace's, and `shared/tests` is
//! where a check that spans crates lives.

use std::path::Path;

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
