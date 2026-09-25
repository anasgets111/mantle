//! The dnf check: two `repoquery` runs as the user, whose metadata goes to their own cache (dnf5's
//! `~/.cache/libdnf5`) under no system lock.

use std::collections::HashMap;
use std::process::Command;

use super::super::backend::{UpdateCandidate, nonzero, run};

/// Space-separated because no field holds a space. dnf4 adds its own newline, leaving blank lines.
const UPGRADE_FORMAT: &str = "%{name} %{arch} %{evr} %{repoid} %{downloadsize} %{installsize}\n";
const INSTALLED_FORMAT: &str = "%{name} %{arch} %{evr}\n";

/// Upgrades with their installed versions. `--refresh` matches pacman's sync on every check.
pub fn outdated() -> Result<Vec<UpdateCandidate>, String> {
    let upgrades = parse_upgrades(&repoquery(&["--refresh", "--upgrades", "--qf", UPGRADE_FORMAT])?)?;
    if upgrades.is_empty() {
        return Ok(Vec::new());
    }
    let mut arguments = vec!["--installed", "--qf", INSTALLED_FORMAT];
    arguments.extend(upgrades.iter().map(|(_, candidate)| candidate.name.as_str()));
    let installed = repoquery(&arguments)?;
    Ok(with_installed(upgrades, &installed))
}

/// Fedora's `skip_if_unavailable=True` plus `--quiet` would turn an offline refresh into an empty,
/// successful answer; a repo that fails to refresh fails the check, as in pacman's.
fn repoquery(arguments: &[&str]) -> Result<String, String> {
    run(
        Command::new("dnf")
            .args(["repoquery", "--quiet", "--latest-limit=1", "--setopt=skip_if_unavailable=False"])
            .args(arguments),
        nonzero,
    )
}

/// [`UPGRADE_FORMAT`] lines as `(arch, candidate)`, `old_version` still empty.
fn parse_upgrades(stdout: &str) -> Result<Vec<(String, UpdateCandidate)>, String> {
    let parse = |line: &str| {
        let [name, arch, evr, repo, download, installed] = fields(line)?;
        Some((
            arch.to_string(),
            UpdateCandidate {
                name: name.to_string(),
                old_version: String::new(),
                new_version: evr.to_string(),
                download_size: download.parse().ok()?,
                installed_size: installed.parse().ok()?,
                repository: repo.to_string(),
            },
        ))
    };
    lines(stdout).map(|line| parse(line).ok_or_else(|| format!("unreadable dnf line: {line}"))).collect()
}

/// Fills `old_version` from [`INSTALLED_FORMAT`] lines, matched on name and arch.
fn with_installed(upgrades: Vec<(String, UpdateCandidate)>, installed: &str) -> Vec<UpdateCandidate> {
    let installed: HashMap<(&str, &str), &str> =
        lines(installed).filter_map(fields).map(|[name, arch, evr]| ((name, arch), evr)).collect();
    upgrades
        .into_iter()
        .map(|(arch, mut candidate)| {
            if let Some(evr) = installed.get(&(candidate.name.as_str(), arch.as_str())) {
                candidate.old_version = (*evr).to_string();
            }
            candidate
        })
        .collect()
}

fn lines(stdout: &str) -> impl Iterator<Item = &str> {
    stdout.lines().map(str::trim).filter(|line| !line.is_empty())
}

fn fields<const N: usize>(line: &str) -> Option<[&str; N]> {
    line.split(' ').collect::<Vec<_>>().try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from dnf5 5.4.5 on Fedora 44, run as a normal user.
    const UPGRADES: &str = "\
curl x86_64 8.18.0-10.fc44 updates 245961 491563
vim-minimal x86_64 2:9.2.1119-1.fc44 updates 917712 1884323
";
    const INSTALLED: &str = "\
curl x86_64 8.18.0-4.fc44
vim-minimal x86_64 2:9.2.240-1.fc44
";

    #[test]
    fn a_check_joins_each_upgrade_to_its_installed_version() {
        let packages = with_installed(parse_upgrades(UPGRADES).unwrap(), INSTALLED);

        assert_eq!(
            packages[1],
            UpdateCandidate {
                name: "vim-minimal".to_string(),
                old_version: "2:9.2.240-1.fc44".to_string(),
                new_version: "2:9.2.1119-1.fc44".to_string(),
                download_size: 917712,
                installed_size: 1884323,
                repository: "updates".to_string(),
            }
        );
        assert_eq!(packages[0].old_version, "8.18.0-4.fc44");
    }

    #[test]
    fn dnf4s_blank_lines_are_skipped() {
        // dnf4 (CentOS Stream 9) ends each line twice.
        let upgrades = parse_upgrades("tzdata noarch 2026c-1.el9 baseos 926398 1917852\n\n").unwrap();
        assert_eq!(upgrades.len(), 1);
    }

    #[test]
    fn an_installed_package_of_another_arch_is_not_the_old_version() {
        let packages = with_installed(parse_upgrades(UPGRADES).unwrap(), "curl i686 8.0-1.fc44\n");
        assert_eq!(packages[0].old_version, "");
    }

    #[test]
    fn a_line_in_another_format_is_an_error_not_a_skipped_package() {
        assert!(parse_upgrades("curl x86_64 8.18.0-10.fc44 updates %{downloadsize} 491563\n").is_err());
        assert!(parse_upgrades("Updating and loading repositories:\n").is_err());
    }
}
