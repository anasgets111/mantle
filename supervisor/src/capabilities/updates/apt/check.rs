//! Parsers for the check's `apt-get -s` and `apt-cache show` output, in the C locale.

use std::collections::HashMap;

use super::super::backend::UpdateCandidate;

/// Upgrades from the simulation's `Inst` lines, sizes `0` until [`fill_sizes`]. A package the
/// upgrade newly installs has no installed version and is not listed.
pub fn parse_simulation(stdout: &str) -> Vec<UpdateCandidate> {
    stdout.lines().filter_map(parse_inst).collect()
}

/// `Inst gzip [1.12-1ubuntu3] (1.12-1ubuntu3.2 Ubuntu:24.04/noble-updates, Ubuntu:24.04/noble-security [amd64])`
fn parse_inst(line: &str) -> Option<UpdateCandidate> {
    let (name, rest) = line.strip_prefix("Inst ")?.split_once(' ')?;
    let (old_version, rest) = rest.strip_prefix('[')?.split_once("] (")?;
    let (release, _arch) = rest.split_once(')')?.0.rsplit_once(" [")?;
    let (new_version, origins) = release.split_once(' ').unwrap_or((release, ""));
    // `Origin:Version/Suite`, comma-separated; an origin may hold spaces, a suite does not.
    let suites: Vec<&str> = origins
        .split(", ")
        .filter(|origin| !origin.is_empty())
        .map(|origin| origin.rsplit('/').next().unwrap_or(origin))
        .collect();
    Some(UpdateCandidate {
        name: name.to_string(),
        old_version: old_version.to_string(),
        new_version: new_version.to_string(),
        download_size: 0,
        installed_size: 0,
        repository: suites.join(","),
    })
}

/// Sets each package's sizes from the `apt-cache show name=version` stanza of its new version:
/// `Size` in bytes, whether or not the `.deb` is cached, and `Installed-Size` in KiB.
pub fn fill_sizes(packages: &mut [UpdateCandidate], stdout: &str) {
    let mut sizes = HashMap::new();
    for stanza in stdout.split("\n\n") {
        let field = |key: &str| stanza.lines().find_map(|line| line.strip_prefix(key)?.strip_prefix(": "));
        let (Some(name), Some(version)) = (field("Package"), field("Version")) else { continue };
        let number = |key| field(key).and_then(|value| value.trim().parse::<i64>().ok()).unwrap_or(0);
        sizes.entry((name, version)).or_insert((number("Size"), number("Installed-Size") * 1024));
    }
    for package in packages {
        // A foreign-architecture `name:i386` is plain `name` in its stanza.
        let name = package.name.split(':').next().unwrap_or(&package.name);
        if let Some(&(download, installed)) = sizes.get(&(name, package.new_version.as_str())) {
            package.download_size = download;
            package.installed_size = installed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `apt-get -s upgrade --with-new-pkgs` as a user on Ubuntu 24.04.
    const UBUNTU_SIMULATION: &str = "\
NOTE: This is only a simulation!
      apt-get needs root privileges for real execution.
      Keep also in mind that locking is deactivated,
      so don't depend on the relevance to the real current situation!
Reading package lists...
Building dependency tree...
Reading state information...
Calculating upgrade...
The following packages will be upgraded:
  gzip libaudit-common libaudit1 perl-base
4 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.
Inst gzip [1.12-1ubuntu3] (1.12-1ubuntu3.2 Ubuntu:24.04/noble-updates, Ubuntu:24.04/noble-security [amd64])
Conf gzip (1.12-1ubuntu3.2 Ubuntu:24.04/noble-updates, Ubuntu:24.04/noble-security [amd64])
Inst perl-base [5.38.2-3.2ubuntu0.4] (5.38.2-3.2ubuntu0.6 Ubuntu:24.04/noble-updates, Ubuntu:24.04/noble-security [amd64])
Conf perl-base (5.38.2-3.2ubuntu0.6 Ubuntu:24.04/noble-updates, Ubuntu:24.04/noble-security [amd64])
Inst libaudit-common [1:3.1.2-2.1build1.1] (1:3.1.2-2.1ubuntu0.1 Ubuntu:24.04/noble-updates [all])
Conf libaudit-common (1:3.1.2-2.1ubuntu0.1 Ubuntu:24.04/noble-updates [all])
Inst libaudit1 [1:3.1.2-2.1build1.1] (1:3.1.2-2.1ubuntu0.1 Ubuntu:24.04/noble-updates [amd64])
Conf libaudit1 (1:3.1.2-2.1ubuntu0.1 Ubuntu:24.04/noble-updates [amd64])
";

    fn candidate(name: &str, old: &str, new: &str, repository: &str) -> UpdateCandidate {
        UpdateCandidate {
            name: name.to_string(),
            old_version: old.to_string(),
            new_version: new.to_string(),
            download_size: 0,
            installed_size: 0,
            repository: repository.to_string(),
        }
    }

    #[test]
    fn each_inst_line_is_one_upgrade_with_every_suite_that_carries_it() {
        assert_eq!(
            parse_simulation(UBUNTU_SIMULATION),
            [
                candidate("gzip", "1.12-1ubuntu3", "1.12-1ubuntu3.2", "noble-updates,noble-security"),
                candidate("perl-base", "5.38.2-3.2ubuntu0.4", "5.38.2-3.2ubuntu0.6", "noble-updates,noble-security"),
                candidate("libaudit-common", "1:3.1.2-2.1build1.1", "1:3.1.2-2.1ubuntu0.1", "noble-updates"),
                candidate("libaudit1", "1:3.1.2-2.1build1.1", "1:3.1.2-2.1ubuntu0.1", "noble-updates"),
            ]
        );
    }

    #[test]
    fn a_debian_security_upgrade_names_both_suites() {
        // Debian 13, with the installed version rewritten older in the dpkg status.
        let line = "Inst libssl3t64 [3.5.7-1~deb13u2~old] (3.5.7-1~deb13u2 Debian:13.7/stable, Debian-Security:13/stable-security [amd64])";
        assert_eq!(
            parse_simulation(line),
            [candidate("libssl3t64", "3.5.7-1~deb13u2~old", "3.5.7-1~deb13u2", "stable,stable-security")]
        );
    }

    #[test]
    fn an_origin_with_a_space_and_a_trailing_hint_still_parse() {
        // Composed: a third-party origin, and the `[]` apt appends while ordering breaks a dependency.
        let line = "Inst google-chrome-stable [139.0-1] (140.0-1 Google LLC:1.0/stable [amd64]) []";
        assert_eq!(parse_simulation(line), [candidate("google-chrome-stable", "139.0-1", "140.0-1", "stable")]);
    }

    #[test]
    fn a_newly_installed_dependency_is_not_an_upgrade() {
        let line = "Inst linux-image-6.8.0-90-generic (6.8.0-90.91 Ubuntu:24.04/noble-updates [amd64])";
        assert!(parse_simulation(line).is_empty());
    }

    #[test]
    fn sizes_come_from_the_stanza_of_the_new_version() {
        // `apt-cache show gzip=1.12-1ubuntu3.2 perl-base=5.38.2-3.2ubuntu0.6`, trimmed.
        let show = "\
Package: gzip
Architecture: amd64
Version: 1.12-1ubuntu3.2
Installed-Size: 244
Depends: libc6 (>= 2.34)
Size: 99204
Description: GNU compression utilities
 This package provides the standard GNU file compression utilities.

Package: perl-base
Installed-Size: 7919
Version: 5.38.2-3.2ubuntu0.6
Size: 1825574
";
        let mut packages = vec![
            candidate("gzip", "1.12-1ubuntu3", "1.12-1ubuntu3.2", ""),
            candidate("perl-base", "5.38.2-3.2ubuntu0.4", "5.38.2-3.2ubuntu0.6", ""),
            candidate("sed", "4.9-2", "4.9-3", ""),
        ];
        fill_sizes(&mut packages, show);
        assert_eq!((packages[0].download_size, packages[0].installed_size), (99_204, 244 * 1024));
        assert_eq!((packages[1].download_size, packages[1].installed_size), (1_825_574, 7919 * 1024));
        assert_eq!((packages[2].download_size, packages[2].installed_size), (0, 0), "no stanza, no guess");
    }
}
