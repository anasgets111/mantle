//! Debian and Ubuntu's `apt`, as an `mantle.updates` backend (ADR-0277). Runs `apt-get` and
//! `apt-cache`, never `apt`, whose output is not a stable interface.

pub mod check;
pub mod install;

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use super::backend::{Backend, CheckReport, InstallCommand, InstallStep, nonzero, run};

/// The full upgrade both the check simulates and the install runs: new dependencies, such as a
/// kernel ABI package, come in, and nothing is removed.
const UPGRADE: [&str; 2] = ["upgrade", "--with-new-pkgs"];

/// Drops the `update` hooks, which run as the caller: a check is no `apt-get update` to announce to
/// PackageKit, command-not-found or the motd.
const NO_HOOKS: &str =
    "#clear APT::Update::Pre-Invoke;\n#clear APT::Update::Post-Invoke;\n#clear APT::Update::Post-Invoke-Success;\n";

#[derive(Default)]
pub struct AptBackend {
    /// `(done, total)` of the running install, from its summary and `Setting up` lines.
    install: Mutex<(u32, u32)>,
}

impl Backend for AptBackend {
    fn name(&self) -> &'static str {
        "apt"
    }

    /// Like `checkupdates`: lists refresh into `$XDG_RUNTIME_DIR/mantle/apt` as the user, and the
    /// upgrade is simulated against them and the real dpkg status.
    fn check(&self) -> Result<CheckReport, String> {
        let root = shared::runtime_root().map_err(|err| format!("no directory to sync into: {err}"))?.join("apt");
        let lists = root.join("lists");
        let cache = root.join("cache");
        let conf = root.join("apt.conf");
        for dir in [lists.join("partial"), cache.clone()] {
            std::fs::create_dir_all(&dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
        }
        std::fs::write(&conf, NO_HOOKS).map_err(|err| format!("failed to write {}: {err}", conf.display()))?;
        let private = [
            "-c".into(),
            conf.into_os_string(),
            "-o".into(),
            prefixed("Dir::State::Lists=", &lists),
            "-o".into(),
            prefixed("Dir::Cache=", &cache),
        ];

        // A repo that fails to refresh fails the check, as in pacman's, not old lists read as current.
        run_private("apt-get", &private, &["-q", "update", "--error-on=any"])?;
        let mut packages =
            check::parse_simulation(&run_private("apt-get", &private, &[&["-s"], &UPGRADE[..]].concat())?);
        if !packages.is_empty() {
            let wanted: Vec<String> =
                packages.iter().map(|package| format!("{}={}", package.name, package.new_version)).collect();
            let wanted: Vec<&str> = wanted.iter().map(String::as_str).collect();
            check::fill_sizes(&mut packages, &run_private("apt-cache", &private, &[&["show"], &wanted[..]].concat())?);
        }
        Ok(CheckReport { packages, aur_error: None })
    }

    /// `pkexec` elevates `env`, since it drops the caller's environment; `update` first, so the
    /// real lists match the ones the check read. Changed conffiles keep the local copy.
    fn install_command(&self) -> InstallCommand {
        let upgrade = format!(
            "apt-get update && apt-get -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold {}",
            UPGRADE.join(" ")
        );
        InstallCommand {
            program: "pkexec".to_string(),
            arguments: ["env", "LC_ALL=C", "DEBIAN_FRONTEND=noninteractive", "sh", "-c", &upgrade]
                .map(String::from)
                .to_vec(),
        }
    }

    fn parse_install_step(&self, line: &str) -> Option<InstallStep> {
        install::parse_install_step(&mut self.install.lock().expect("mutex poisoned"), line)
    }
}

fn prefixed(option: &str, path: &Path) -> std::ffi::OsString {
    let mut value = std::ffi::OsString::from(option);
    value.push(path);
    value
}

/// `program` with the private options, then `arguments`.
fn run_private(program: &str, private: &[std::ffi::OsString], arguments: &[&str]) -> Result<String, String> {
    run(Command::new(program).args(private).args(arguments), nonzero)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_install_updates_the_real_lists_then_runs_the_upgrade_the_check_simulated() {
        let command = AptBackend::default().install_command();
        assert_eq!(command.program, "pkexec");
        assert_eq!(command.arguments[..5], ["env", "LC_ALL=C", "DEBIAN_FRONTEND=noninteractive", "sh", "-c"]);
        assert!(command.arguments[5].starts_with("apt-get update && apt-get -y "));
        assert!(command.arguments[5].ends_with(" upgrade --with-new-pkgs"));
    }
}
