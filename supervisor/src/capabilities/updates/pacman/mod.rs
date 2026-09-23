//! Arch's `pacman`, as an `mantle.updates` backend (ADR-0034, ADR-0134). The one implementation
//! of [`super::backend::Backend`] this Supervisor ships. Everything under here knows about
//! `libalpm`, `/etc/pacman.conf` and `pacman`'s own stdout; nothing above the trait does.

pub mod aur;
pub mod check;
pub mod conf;
pub mod install;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use shared::warn;

use super::backend::{Backend, CheckReport, InstallCommand, InstallStep};

/// Checks sync into their own db root; installs use the real one. Injected paths let tests use a
/// tempdir.
pub struct PacmanBackend {
    conf_path: PathBuf,
    db_root: PathBuf,
    aur_helper: Option<&'static str>,
    aur: AtomicBool,
}

impl PacmanBackend {
    pub fn new(conf_path: PathBuf, db_root: PathBuf, aur_helper: Option<&'static str>) -> Self {
        Self { conf_path, db_root, aur_helper, aur: AtomicBool::new(false) }
    }

    /// The helper to check and install through: detected and asked for.
    fn active_helper(&self) -> Option<&'static str> {
        self.aur_helper.filter(|_| self.aur.load(Ordering::Relaxed))
    }

    fn missing_helper(&self) -> Option<String> {
        (self.aur.load(Ordering::Relaxed) && self.aur_helper.is_none()).then(|| aur::NO_HELPER.to_string())
    }
}

impl Backend for PacmanBackend {
    fn name(&self) -> &'static str {
        "pacman"
    }

    fn check(&self) -> Result<CheckReport, String> {
        let mut report = check_in_a_child(&self.conf_path, &self.db_root, self.active_helper().is_some())?;
        report.aur_error = report.aur_error.or_else(|| self.missing_helper());
        Ok(report)
    }

    /// Root upgrade against real `/etc/pacman.conf` and `/var/lib/pacman`. `pkexec` triggers
    /// Mantle's registered polkit agent instead of requiring a terminal. An active AUR helper runs
    /// as the user instead and elevates itself (ADR-0250).
    fn install_command(&self) -> InstallCommand {
        match self.active_helper() {
            Some(helper) => InstallCommand { program: helper.to_string(), arguments: aur::install_arguments(helper) },
            None => InstallCommand {
                program: "pkexec".to_string(),
                arguments: vec!["pacman".to_string(), "-Syu".to_string(), "--noconfirm".to_string()],
            },
        }
    }

    fn parse_install_step(&self, line: &str) -> Option<InstallStep> {
        install::parse_install_step(line)
    }

    fn aur_helper(&self) -> Option<&'static str> {
        self.aur_helper
    }

    fn set_aur(&self, enabled: bool) -> Option<String> {
        self.aur.store(enabled, Ordering::Relaxed);
        let missing = self.missing_helper();
        if let Some(error) = &missing {
            warn!("aur requested: {error}");
        }
        missing
    }
}

/// Env names carrying the check into a re-exec of this binary.
const CHECK_WORKER: &str = "MANTLE_PACMAN_CHECK";
const CHECK_CONF: &str = "MANTLE_PACMAN_CONF";
const CHECK_DB_ROOT: &str = "MANTLE_PACMAN_DB_ROOT";
const CHECK_SYNC_ROOT: &str = "MANTLE_PACMAN_SYNC_ROOT";
const CHECK_AUR: &str = "MANTLE_PACMAN_AUR";

/// Runs the check in a child that then exits, because process exit is the only thing that returns
/// the memory. One sync costs ~55 MiB of glibc arena and `malloc_trim` gives back none of it: the
/// bytes are freed, but libalpm leaves at least one live chunk on every arena page, so nothing can
/// be unmapped. In-process the *first* check raised the Supervisor's floor for the rest of the
/// session.
///
/// `Backend::check` already runs inside `spawn_blocking`, so this waits on the child rather than
/// reaching for `tokio::process`.
fn check_in_a_child(conf_path: &Path, db_root: &Path, aur: bool) -> Result<CheckReport, String> {
    // Per login like `checkupdates`' `/tmp/checkup-db-$UID`, but in the 0700 runtime dir.
    let sync_root = shared::runtime_root().map_err(|err| format!("no directory to sync into: {err}"))?.join("pacman");
    let mut command = std::process::Command::new(crate::pam_worker::SELF_EXE);
    command
        .env(CHECK_WORKER, "1")
        .env(CHECK_CONF, conf_path)
        .env(CHECK_DB_ROOT, db_root)
        .env(CHECK_SYNC_ROOT, &sync_root);
    if aur {
        command.env(CHECK_AUR, "1");
    }
    let output = command.output().map_err(|err| format!("failed to spawn the update check: {err}"))?;

    if !output.status.success() {
        // A killed sync leaves libalpm's lock behind, and it would refuse every later check.
        if std::os::unix::process::ExitStatusExt::signal(&output.status).is_some() {
            let _ = std::fs::remove_file(sync_root.join("db.lck"));
        }
        // The worker prints its own diagnosis; without one, name the status so a crash is not a
        // silent "no updates".
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("the update check exited with {}", output.status)
        } else {
            detail.to_string()
        });
    }

    serde_json::from_slice::<Result<CheckReport, String>>(&output.stdout)
        .map_err(|err| format!("the update check returned no readable result: {err}"))?
}

/// Child branch of [`check_in_a_child`], entered from `main` before anything else starts. Writes
/// one JSON `Result` and returns; the exit that follows is what reclaims the arena.
pub(crate) fn run_check_worker() -> Result<(), Box<dyn std::error::Error>> {
    let conf_path = PathBuf::from(std::env::var_os(CHECK_CONF).ok_or("missing the pacman conf path")?);
    let db_root = PathBuf::from(std::env::var_os(CHECK_DB_ROOT).ok_or("missing the pacman db root")?);
    let sync_root = PathBuf::from(std::env::var_os(CHECK_SYNC_ROOT).ok_or("missing the pacman sync root")?);

    let result = check_against_the_sync_root(&conf_path, &db_root, &sync_root, std::env::var_os(CHECK_AUR).is_some());
    serde_json::to_writer(std::io::stdout().lock(), &result)?;
    Ok(())
}

/// Syncs and checks in `sync_root`, which holds one symlink to `db_root/local`, never in the real db
/// (ADR-0034, amended ADR-0113). Only `sync_root/sync/` is written, and it persists between checks
/// so an unchanged mirror db is not downloaded again.
///
/// Uses a symlink like `checkupdates` (`ln -s "${DBPath}/local" "$CHECKUPDATES_DB"`): `local/` is
/// read-only installed metadata, and a copy would walk ~1,500 package directories per check
/// (ADR-0034).
///
/// ponytail: unlike a copy, the symlink can observe a concurrent install mid-write,
/// causing a transient `check_error`. This matches `checkupdates` and self-heals next check.
fn check_against_the_sync_root(
    conf_path: &Path,
    db_root: &Path,
    sync_root: &Path,
    aur: bool,
) -> Result<CheckReport, String> {
    std::fs::create_dir_all(sync_root).map_err(|err| format!("failed to create {}: {err}", sync_root.display()))?;
    link_local_db(db_root, sync_root)?;

    let repos = conf::resolve_repo_servers(conf_path);
    if repos.is_empty() {
        return Err(format!("no repos resolved from {}", conf_path.display()));
    }

    let (mut packages, foreign) =
        check::check_for_updates(Path::new("/"), sync_root, &repos).map_err(|err| err.to_string())?;
    let aur_error = if aur { aur::check(&foreign).map(|found| packages.extend(found)).err() } else { None };
    Ok(CheckReport { packages, aur_error })
}

/// Links `db_root/local` in as `sync_root/local`, the one name `alpm` looks for when it reads
/// installed packages out of a db root. Split out from [`check_against_the_sync_root`] only so the
/// name and the read-through are testable without a mirror: everything else that function does
/// needs the network.
fn link_local_db(db_root: &Path, sync_root: &Path) -> Result<(), String> {
    let local_src = db_root.join("local");
    // Refuse a missing source: `symlink` permits a dangling `local/`, which `alpm` reads as no
    // installed packages and therefore every mirror package being an update.
    if !local_src.is_dir() {
        return Err(format!("{} is not a directory; cannot check updates against it", local_src.display()));
    }
    let link = sync_root.join("local");
    if std::fs::read_link(&link).is_ok_and(|target| target == local_src) {
        return Ok(());
    }
    // A link to another root is replaced; a real directory makes the `symlink` below fail.
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&local_src, &link)
        .map_err(|err| format!("failed to link {} into {}: {err}", local_src.display(), sync_root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_names_itself_after_the_command_a_config_would_recognize() {
        let backend = PacmanBackend::new(PathBuf::from("/etc/pacman.conf"), PathBuf::from("/var/lib/pacman"), None);
        assert_eq!(backend.name(), "pacman");

        let command = backend.install_command();
        assert_eq!(command.program, "pkexec", "elevation goes through polkit, so Mantle's own agent prompts");
        assert_eq!(command.arguments, vec!["pacman", "-Syu", "--noconfirm"]);
    }

    #[test]
    fn a_detected_helper_installs_only_once_aur_is_asked_for() {
        let backend = PacmanBackend::new(PathBuf::new(), PathBuf::new(), Some("paru"));
        assert_eq!(backend.install_command().program, "pkexec");

        assert_eq!(backend.set_aur(true), None);
        let command = backend.install_command();
        assert_eq!(command.program, "paru");
        assert_eq!(command.arguments, ["-Syu", "--noconfirm", "--sudo", "pkexec", "--nosudoloop"]);
    }

    #[test]
    fn aur_without_a_helper_is_an_error_and_pacman_still_installs() {
        let backend = PacmanBackend::new(PathBuf::new(), PathBuf::new(), None);

        assert_eq!(backend.set_aur(true).as_deref(), Some(aur::NO_HELPER));
        assert_eq!(backend.install_command().program, "pkexec");
        assert_eq!(backend.set_aur(false), None);
    }

    #[test]
    fn the_sync_root_reads_installed_packages_through_a_link_named_local() {
        // `alpm` looks specifically for `local/`; another link name makes every package look new.
        let real = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(real.path().join("local").join("bash-5.3-1")).unwrap();
        std::fs::write(real.path().join("local").join("bash-5.3-1").join("desc"), "%NAME%\nbash\n").unwrap();

        let sync_root = tempfile::tempdir().unwrap();
        link_local_db(real.path(), sync_root.path()).unwrap();

        let linked = sync_root.path().join("local");
        assert!(linked.symlink_metadata().unwrap().is_symlink(), "local must be a link, not a copied tree");
        assert_eq!(
            std::fs::read_to_string(linked.join("bash-5.3-1").join("desc")).unwrap(),
            "%NAME%\nbash\n",
            "the real package metadata must be readable through the link"
        );
    }

    #[test]
    fn a_later_check_reuses_the_link_and_repoints_one_to_another_root() {
        let real = tempfile::tempdir().unwrap();
        std::fs::create_dir(real.path().join("local")).unwrap();
        let sync_root = tempfile::tempdir().unwrap();

        link_local_db(real.path(), sync_root.path()).unwrap();
        link_local_db(real.path(), sync_root.path()).expect("the second check finds its own link");

        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join("local")).unwrap();
        link_local_db(other.path(), sync_root.path()).unwrap();
        assert_eq!(std::fs::read_link(sync_root.path().join("local")).unwrap(), other.path().join("local"));
    }

    #[test]
    fn a_real_local_directory_in_the_sync_root_is_an_error_not_a_silent_reuse() {
        // A copied `local/` would be stale package metadata.
        let real = tempfile::tempdir().unwrap();
        std::fs::create_dir(real.path().join("local")).unwrap();
        let sync_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(sync_root.path().join("local")).unwrap();

        assert!(link_local_db(real.path(), sync_root.path()).is_err());
    }

    #[test]
    fn a_db_root_with_no_local_directory_is_refused_rather_than_linked_to_nothing() {
        let missing = tempfile::tempdir().unwrap();
        let sync_root = tempfile::tempdir().unwrap();

        assert!(link_local_db(&missing.path().join("no-such-root"), sync_root.path()).is_err());
        assert!(!sync_root.path().join("local").exists());
    }
}
