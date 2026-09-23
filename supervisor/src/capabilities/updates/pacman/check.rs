//! Real `alpm` sync and outdated-package diff (ADR-0034), replacing `checkupdates`+`expac`
//! subprocesses with the Arch project's own `libalpm` binding.
//! Requires a real handle and mirror I/O, so it is verified live against a sync db root. `alpm`
//! wraps non-`Send` C pointers; callers run this inside one `spawn_blocking` closure.

use std::path::Path;

use super::super::backend::UpdateCandidate;
use super::conf::RepoServers;

/// Each foreign package's installed name and version: in no synced repo.
pub type Foreign = Vec<(String, String)>;

/// Registers `repos` against user-owned `db_path`, syncs them like `checkupdates`' `pacman -Sy`
/// (a db the mirror reports unchanged is not downloaded), then diffs `root`'s installed packages
/// with `alpm::sync_new_version`. Also returns the foreign packages.
pub fn check_for_updates(
    root: &Path,
    db_path: &Path,
    repos: &[RepoServers],
) -> Result<(Vec<UpdateCandidate>, Foreign), alpm::Error> {
    let mut handle = alpm::Alpm::new(root.to_string_lossy().into_owned(), db_path.to_string_lossy().into_owned())?;

    for repo in repos {
        let db = handle.register_syncdb_mut(repo.name.clone(), alpm::SigLevel::USE_DEFAULT)?;
        for server in &repo.servers {
            db.add_server(server.as_str())?;
        }
    }

    handle.syncdbs_mut().update(false)?;

    let syncdbs = handle.syncdbs();
    let mut candidates = Vec::new();
    let mut foreign = Vec::new();
    for installed in handle.localdb().pkgs() {
        if let Some(newer) = installed.sync_new_version(syncdbs) {
            candidates.push(UpdateCandidate {
                name: installed.name().to_string(),
                old_version: installed.version().to_string(),
                new_version: newer.version().to_string(),
                download_size: newer.download_size(),
                installed_size: newer.isize(),
                repository: newer.db().map(|db| db.name().to_string()).unwrap_or_default(),
            });
        } else if syncdbs.iter().all(|db| db.pkg(installed.name()).is_err()) {
            foreign.push((installed.name().to_string(), installed.version().to_string()));
        }
    }

    Ok((candidates, foreign))
}
