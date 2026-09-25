//! Package manager abstraction for `mantle.updates` (ADR-0134).
//!
//! Separates `controller.rs` scheduling/state from package-manager execution, parsing, and reboot
//! requirements.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use shared::warn;

/// One installed package with a newer version.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct UpdateCandidate {
    /// Package name.
    pub name: String,
    /// Installed version.
    pub old_version: String,
    /// Version on offer.
    pub new_version: String,
    /// Bytes to fetch; `0` when already cached.
    pub download_size: i64,
    /// Bytes the new version occupies installed; not a delta.
    pub installed_size: i64,
    /// Source repository, e.g. `"extra"` or `"aur"`; empty in a seeded list that lacks it.
    #[serde(default)]
    pub repository: String,
}

/// One successful check. `aur_error` means the AUR half failed; `packages` still holds the repos'.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub packages: Vec<UpdateCandidate>,
    pub aur_error: Option<String>,
}

/// Parsed install progress: which package of how many. `None` for other lines; progress stays put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallStep {
    pub current: u32,
    pub total: u32,
    pub package: String,
}

/// Privileged upgrade command. Backends use `pkexec`, routing the prompt to Mantle's polkit agent
/// (`crate::polkit`) instead of a terminal.
pub struct InstallCommand {
    pub program: String,
    pub arguments: Vec<String>,
}

/// Package-manager backend. `Send + Sync + 'static` is required because the controller shares it
/// with a spawned task.
pub trait Backend: Send + Sync + 'static {
    /// Manager name in `UpdatesState::package_manager`, e.g. lowercase command name `"pacman"`.
    fn name(&self) -> &'static str;

    /// Blocking outdated-package check; it syncs repo databases over the network. The caller runs
    /// it in `tokio::task::spawn_blocking`; no async runtime is assumed here.
    ///
    /// Must not modify the real system: a wrong answer is a badge error; side effects can leave a
    /// half-upgraded machine.
    fn check(&self) -> Result<CheckReport, String>;

    /// The command that performs the real upgrade.
    fn install_command(&self) -> InstallCommand;

    /// Parses one [`Backend::install_command`] output line as progress, if applicable.
    fn parse_install_step(&self, line: &str) -> Option<InstallStep>;

    /// The AUR helper this backend would install through, if it has one (ADR-0250).
    fn aur_helper(&self) -> Option<&'static str> {
        None
    }

    /// Applies `configure`'s `aur`, returning the `aur_error` it leaves.
    fn set_aur(&self, _enabled: bool) -> Option<String> {
        None
    }
}

/// Package manager that owns `/`, or `None`. Fedora and Debian package `pacman`, Fedora `apt` and
/// Debian `dnf` for building chroots, so pacman and apt count only with packages in their own db.
pub fn detect() -> Option<Box<dyn Backend>> {
    let pacman_db = PathBuf::from("/var/lib/pacman");
    if on_path("pacman") && std::fs::read_dir(pacman_db.join("local")).is_ok_and(|mut dir| dir.next().is_some()) {
        return Some(Box::new(super::pacman::PacmanBackend::new(
            PathBuf::from("/etc/pacman.conf"),
            pacman_db,
            super::pacman::aur::detect_helper(),
        )));
    }
    if on_path("apt-get") && std::fs::metadata("/var/lib/dpkg/status").is_ok_and(|status| status.len() > 0) {
        return Some(Box::<super::apt::AptBackend>::default());
    }
    if on_path("dnf") {
        return Some(Box::new(super::dnf::DnfBackend));
    }
    None
}

/// Stdout of `command` in the C locale with no stdin. `failed` judges the exit code against
/// stderr; the stderr of a success is logged as warnings.
pub(super) fn run(command: &mut Command, failed: fn(Option<i32>, &str) -> bool) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("failed to run {program}: {err}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if failed(output.status.code(), &stderr) {
        let detail = stderr.trim();
        return Err(if detail.is_empty() {
            format!("{program} exited with {}", output.status)
        } else {
            format!("{program} failed: {detail}")
        });
    }
    for line in stderr.lines().filter(|line| !line.trim().is_empty()) {
        warn!("{line}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// [`run`]'s `failed` for a manager whose every non-zero exit is a failure.
pub(super) fn nonzero(code: Option<i32>, _stderr: &str) -> bool {
    code != Some(0)
}

/// Whether `program` is a file in this process's `PATH`; avoids spawning a shell during capability
/// startup.
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| program_is_in(&path, program))
}

/// [`on_path`] against an explicit `PATH`, so tests avoid mutating the process environment.
pub(super) fn program_is_in(path: &std::ffi::OsStr, program: &str) -> bool {
    std::env::split_paths(path).any(|directory| directory.join(program).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_found_in_a_directory_that_path_names() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("fake-manager"), "").unwrap();

        assert!(program_is_in(directory.path().as_os_str(), "fake-manager"));
        assert!(!program_is_in(directory.path().as_os_str(), "some-other-manager"));
    }

    #[test]
    fn a_directory_of_the_right_name_is_not_a_program() {
        // `PATH` entries must be files, not a directory named `pacman`.
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("pacman")).unwrap();

        assert!(!program_is_in(directory.path().as_os_str(), "pacman"));
    }

    #[test]
    fn a_program_is_found_in_the_second_of_several_path_entries() {
        let empty = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        std::fs::write(real.path().join("pacman"), "").unwrap();
        let path = std::env::join_paths([empty.path(), real.path()]).unwrap();

        assert!(program_is_in(&path, "pacman"));
    }

    #[test]
    fn an_empty_path_finds_nothing() {
        assert!(!program_is_in(std::ffi::OsStr::new(""), "pacman"));
    }
}
