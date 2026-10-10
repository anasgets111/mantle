//! `pacman` half of `updates:install()` (ADR-0034): install-progress syntax. The
//! command is `env LC_ALL=C pkexec pacman -Syu --noconfirm`, run by `controller.rs` through
//! `process::spawn_group_leader_piped`; `pkexec` talks to polkit and triggers the Mantle polkit
//! agent's interactive prompt rather than a manual `CheckAuthorization` call. It modifies the real
//! `/etc/pacman.conf`/`/var/lib/pacman` as root, unlike `check.rs`.
//!
//! Progress parsing is best-effort against pacman's real format
//! (`"(2/5) installing nss (3.127-1 -> 3.128-1)"`), not end-to-end verified because a privileged
//! run needs the user's polkit prompt. A missed line affects only UI progress; success comes from
//! process exit status.

use super::super::backend::InstallStep;

/// Parses `(current/total) installing|upgrading|reinstalling <package> ...` install lines. `None`
/// for sync messages, download bars, and blanks; the previous progress remains.
pub fn parse_install_step(line: &str) -> Option<InstallStep> {
    let rest = line.trim().strip_prefix('(')?;
    let (counts, rest) = rest.split_once(')')?;
    let (current, total) = counts.split_once('/')?;
    let current: u32 = current.trim().parse().ok()?;
    let total: u32 = total.trim().parse().ok()?;
    let rest = rest.trim();
    let rest = rest
        .strip_prefix("installing ")
        .or_else(|| rest.strip_prefix("upgrading "))
        .or_else(|| rest.strip_prefix("reinstalling "))?;
    let package = rest.split_whitespace().next()?.to_string();
    Some(InstallStep { current, total, package })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_install_step_reads_installing_upgrading_and_reinstalling_lines() {
        for (line, current, total, package) in [
            ("(2/5) installing nss (3.127-1 -> 3.128-1)", 2, 5, "nss"),
            ("(1/3) upgrading ca-certificates-mozilla", 1, 3, "ca-certificates-mozilla"),
            ("(1/1) reinstalling linux", 1, 1, "linux"),
        ] {
            let step = parse_install_step(line).expect(line);
            assert_eq!(step, InstallStep { current, total, package: package.to_string() }, "{line}");
        }
    }

    #[test]
    fn parse_install_step_is_none_for_lines_that_are_not_a_step() {
        for line in [
            ":: Synchronizing package databases...",
            "nss-3.128-1-x86_64  1811951 KiB  4.05 MiB/s 00:00:03 [#####] 100%",
            "",
            "(a/b) installing nss",
        ] {
            assert!(parse_install_step(line).is_none(), "{line:?}");
        }
    }
}
