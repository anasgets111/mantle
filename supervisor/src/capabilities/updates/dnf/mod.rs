//! Fedora's `dnf`, as an `mantle.updates` backend (ADR-0134). dnf5 and dnf4 take the same
//! `repoquery` flags; only their install progress lines differ.

pub mod check;
pub mod install;

use super::backend::{Backend, CheckReport, InstallCommand, InstallStep};

pub struct DnfBackend;

impl Backend for DnfBackend {
    fn name(&self) -> &'static str {
        "dnf"
    }

    fn check(&self) -> Result<CheckReport, String> {
        Ok(CheckReport { packages: check::outdated()?, aur_error: None })
    }

    /// `--refresh` so root's cache sees what the user's check saw. `pkexec` keeps the caller's
    /// locale, which would translate the progress lines.
    fn install_command(&self) -> InstallCommand {
        InstallCommand {
            program: "pkexec".to_string(),
            arguments: ["env", "LC_ALL=C", "dnf", "upgrade", "-y", "--refresh"].map(String::from).to_vec(),
        }
    }

    fn parse_install_step(&self, line: &str) -> Option<InstallStep> {
        install::parse_install_step(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_install_goes_through_polkit_and_refreshes_roots_cache() {
        let command = DnfBackend.install_command();
        assert_eq!(command.program, "pkexec");
        assert_eq!(command.arguments, ["env", "LC_ALL=C", "dnf", "upgrade", "-y", "--refresh"]);
    }
}
