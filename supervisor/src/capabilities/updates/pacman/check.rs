//! Repo half of the pacman check (ADR-0276). `curl` refreshes each repo db into a user-owned db
//! root, since `pacman -Sy` refuses a non-root user; `pacman` then reads that root. `pacman`,
//! `pacman-conf` and `curl` all ship with pacman, so the check adds no runtime dependency.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use shared::warn;

use super::super::backend::UpdateCandidate;

/// Each foreign package's installed name and version: in no synced repo.
pub type Foreign = Vec<(String, String)>;

/// Downloads every repo `conf` names into `db_path/sync/`, like `checkupdates`' `pacman -Sy`: a db
/// the mirror reports unchanged is not downloaded again.
///
/// ponytail: no `.db.sig` is fetched, so a repo set to `DatabaseRequired` fails the check; fetch
/// the signature when such a repo turns up.
pub fn sync(conf: &Path, db_path: &Path) -> Result<(), String> {
    let sync_dir = db_path.join("sync");
    std::fs::create_dir_all(&sync_dir).map_err(|err| format!("failed to create {}: {err}", sync_dir.display()))?;
    let repos = run(Command::new("pacman-conf").arg("--config").arg(conf).arg("--repo-list"))?;
    if repos.trim().is_empty() {
        return Err(format!("no repos configured in {}", conf.display()));
    }
    for repo in repos.lines() {
        // `pacman-conf` has already expanded `Include`, `$repo` and `$arch`.
        let servers = run(Command::new("pacman-conf").arg("--config").arg(conf).args(["--repo", repo, "Server"]))?;
        fetch_db(&sync_dir, repo, servers.lines())?;
    }
    Ok(())
}

/// Tries each mirror in order until one answers, as pacman does. Timeouts match libalpm's.
fn fetch_db<'a>(sync_dir: &Path, repo: &str, servers: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let db = sync_dir.join(format!("{repo}.db"));
    // A killed download leaves only the part file; the db pacman reads is swapped in whole.
    let part = sync_dir.join(format!("{repo}.db.part"));
    let mut failure = "no Server configured".to_string();
    for server in servers {
        let mut command = Command::new("curl");
        command.args(["--silent", "--show-error", "--fail", "--location", "--remote-time"]);
        command.args(["--connect-timeout", "10", "--speed-limit", "1", "--speed-time", "10"]);
        command.arg("--output").arg(&part);
        let _ = std::fs::remove_file(&part);
        if db.exists() {
            command.arg("--time-cond").arg(&db);
        }
        let output = command
            .arg(format!("{server}/{repo}.db"))
            .stdin(Stdio::null())
            .output()
            .map_err(|err| format!("failed to run curl: {err}"))?;
        if !output.status.success() {
            failure = String::from_utf8_lossy(&output.stderr).trim().to_string();
            continue;
        }
        // Unchanged writes nothing: a 304, or an unmet time condition curl reports as success
        // (`file://`, a server that ignores `If-Modified-Since`).
        if !part.exists() {
            return Ok(());
        }
        return std::fs::rename(&part, &db).map_err(|err| format!("failed to store {}: {err}", db.display()));
    }
    Err(format!("failed to sync {repo}: {failure}"))
}

/// Diffs the installed packages with the synced repos under `db_path`.
pub fn outdated(conf: &Path, db_path: &Path) -> Result<Vec<UpdateCandidate>, String> {
    let upgrades = parse_upgrades(&pacman(conf, db_path, &["-Qu"])?);
    if upgrades.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<&str> = upgrades.iter().map(|(name, _, _)| name.as_str()).collect();
    // `-Sp` sizes the download as pacman would, `0` once cached; only `-Si` has the installed size.
    let downloads =
        parse_downloads(&pacman(conf, db_path, &[&["-Sddp", "--print-format", "%n %s"], &names[..]].concat())?);
    let info = parse_info(&pacman(conf, db_path, &[&["-Si"], &names[..]].concat())?);
    Ok(upgrades
        .into_iter()
        .map(|(name, old_version, new_version)| {
            let (repository, installed_size) = info.get(&name).cloned().unwrap_or_default();
            UpdateCandidate {
                download_size: downloads.get(&name).copied().unwrap_or_default(),
                installed_size,
                repository,
                name,
                old_version,
                new_version,
            }
        })
        .collect())
}

/// The installed packages no repo under `db_path` carries.
pub fn foreign(conf: &Path, db_path: &Path) -> Result<Foreign, String> {
    Ok(pacman(conf, db_path, &["-Qm"])?
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, version)| (name.to_string(), version.to_string()))
        .collect())
}

fn pacman(conf: &Path, db_path: &Path, args: &[&str]) -> Result<String, String> {
    run(Command::new("pacman")
        .arg("--config")
        .arg(conf)
        .arg("--dbpath")
        .arg(db_path)
        .args(["--color", "never"])
        .args(args))
}

/// Stdout of a command in the C locale. Warnings are logged, not fatal.
fn run(command: &mut Command) -> Result<String, String> {
    let output = command
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("failed to run {}: {err}", command.get_program().to_string_lossy()))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if failed(output.status.code(), &stderr) {
        return Err(format!("{} failed: {}", command.get_program().to_string_lossy(), stderr.trim()));
    }
    for line in stderr.lines().filter(|line| !line.trim().is_empty()) {
        warn!("{line}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A query that matches nothing exits 1, so exit 1 fails only on a stderr line that is not a
/// `warning:`, such as a `pacman.conf` that fails to parse.
fn failed(code: Option<i32>, stderr: &str) -> bool {
    match code {
        Some(0) => false,
        Some(1) => stderr.lines().any(|line| !line.trim().is_empty() && !line.starts_with("warning:")),
        _ => true,
    }
}

/// `-Qu`'s `name old -> new` lines. An `[ignored]` package is left out: `-Syu` will not take it.
fn parse_upgrades(text: &str) -> Vec<(String, String, String)> {
    text.lines()
        .filter(|line| !line.ends_with("[ignored]"))
        .filter_map(|line| match line.split_whitespace().collect::<Vec<_>>()[..] {
            [name, old, "->", new] => Some((name.to_string(), old.to_string(), new.to_string())),
            _ => None,
        })
        .collect()
}

/// `--print-format '%n %s'` lines: name and download bytes.
fn parse_downloads(text: &str) -> HashMap<String, i64> {
    text.lines()
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(name, size)| Some((name.to_string(), size.parse().ok()?)))
        .collect()
}

/// `-Si` blocks: each name's repository and installed bytes. A name in several repos keeps the
/// first, the one `-S` installs.
fn parse_info(text: &str) -> HashMap<String, (String, i64)> {
    let mut info = HashMap::new();
    for block in text.split("\n\n") {
        let field = |key: &str| {
            block.lines().find_map(|line| {
                let (name, value) = line.split_once(" : ")?;
                (name.trim_end() == key).then(|| value.trim())
            })
        };
        let (Some(repository), Some(name)) = (field("Repository"), field("Name")) else { continue };
        let size = field("Installed Size").and_then(parse_size).unwrap_or_default();
        info.entry(name.to_string()).or_insert((repository.to_string(), size));
    }
    info
}

/// pacman's `9820.16 KiB` or `50.66 MiB`: two decimals, so up to ~5 KiB off at MiB.
fn parse_size(text: &str) -> Option<i64> {
    let (value, unit) = text.split_once(' ')?;
    let power = ["B", "KiB", "MiB", "GiB", "TiB"].iter().position(|known| *known == unit)?;
    Some((value.parse::<f64>().ok()? * 1024_f64.powi(power as i32)).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_1_is_an_empty_answer_unless_stderr_has_more_than_warnings() {
        assert!(!failed(Some(1), ""));
        assert!(!failed(Some(1), "warning: config file /etc/pacman.conf, line 9: directive 'Foo' not recognized.\n"));
        assert!(failed(Some(1), "error: config file /etc/pacman.conf could not be read: No such file or directory\n"));
        assert!(failed(Some(2), ""));
        assert!(failed(None, ""));
    }

    #[test]
    fn upgrades_are_read_and_an_ignored_one_is_left_out() {
        let text = "linux 6.9.1-1 -> 6.9.2-1\nmesa 1:24.1.0-1 -> 1:24.1.1-1\nnvidia 550-1 -> 555-1 [ignored]\n";
        assert_eq!(
            parse_upgrades(text),
            [("linux", "6.9.1-1", "6.9.2-1"), ("mesa", "1:24.1.0-1", "1:24.1.1-1")].map(|(name, old, new)| (
                name.to_string(),
                old.to_string(),
                new.to_string()
            ))
        );
    }

    #[test]
    fn download_sizes_are_bytes_and_a_cached_package_is_zero() {
        let sizes = parse_downloads("linux 148230012\nmesa 0\n");
        assert_eq!(sizes["linux"], 148_230_012);
        assert_eq!(sizes["mesa"], 0);
    }

    #[test]
    fn info_keeps_the_first_repo_that_carries_a_name() {
        let text = "Repository      : core\nName            : bash\nVersion         : 5.3.20-1\n\
                    Download Size   : 1954.93 KiB\nInstalled Size  : 9820.16 KiB\n\n\
                    Repository      : chaotic-aur\nName            : bash\nInstalled Size  : 1.00 KiB\n\n\
                    Repository      : extra\nName            : mesa\nInstalled Size  : 120.50 MiB\n";
        let info = parse_info(text);
        assert_eq!(info["bash"], ("core".to_string(), 10_055_844));
        assert_eq!(info["mesa"], ("extra".to_string(), 126_353_408));
    }

    #[test]
    fn sizes_read_every_unit_pacman_prints() {
        assert_eq!(parse_size("512.00 B"), Some(512));
        assert_eq!(parse_size("1.50 GiB"), Some(1_610_612_736));
        assert_eq!(parse_size("12 parsecs"), None);
    }
}
