//! AUR half of the pacman backend (ADR-0250): helper detection, the AUR web API check for foreign
//! packages, and the helper's install argv.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::process::{Command, Stdio};

use super::super::backend::{UpdateCandidate, program_is_in};

/// Detection order when several are installed.
const HELPERS: [&str; 2] = ["paru", "yay"];

pub const NO_HELPER: &str = "no AUR helper found (paru, yay)";

const INFO_URL: &str = "https://aur.archlinux.org/rpc/v5/info";

pub fn detect_helper() -> Option<&'static str> {
    std::env::var_os("PATH").and_then(|path| first_helper(&path))
}

fn first_helper(path: &OsStr) -> Option<&'static str> {
    HELPERS.into_iter().find(|helper| program_is_in(path, helper))
}

/// `--noconfirm` also skips PKGBUILD review, and `--sudo pkexec` sends the elevation prompt to
/// Mantle's polkit agent, since no terminal can take a `sudo` password. The sudo loop is forced off:
/// pkexec has no `-v`, and yay retries a failed `-v` forever.
pub fn install_arguments(helper: &str) -> Vec<String> {
    let no_sudo_loop = if helper == "yay" { "--sudoloop=false" } else { "--nosudoloop" };
    ["-Syu", "--noconfirm", "--sudo", "pkexec", no_sudo_loop].map(String::from).to_vec()
}

/// AUR upgrades for `foreign` `(name, installed version)` pairs, in one POST.
pub fn check(foreign: &[(String, String)]) -> Result<Vec<UpdateCandidate>, String> {
    if foreign.is_empty() {
        return Ok(Vec::new());
    }
    let mut command = Command::new("curl");
    command.args(["--silent", "--show-error", "--fail", "--max-time", "30", INFO_URL]);
    for (name, _) in foreign {
        command.arg("--data-urlencode").arg(format!("arg[]={name}"));
    }
    let output = command.stdin(Stdio::null()).output().map_err(|err| format!("failed to run curl: {err}"))?;
    if !output.status.success() {
        return Err(format!("AUR request failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(parse_info(&output.stdout, foreign)?
        .into_iter()
        .filter(|candidate| newer(&candidate.new_version, &candidate.old_version))
        .collect())
}

/// pacman's own `vercmp`, so an epoch or pkgrel orders exactly as `-Syu` will.
fn newer(offered: &str, installed: &str) -> bool {
    offered != installed
        && Command::new("vercmp")
            .args([offered, installed])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|output| output.stdout.trim_ascii() == b"1")
}

#[derive(serde::Deserialize)]
struct InfoResponse {
    #[serde(default)]
    results: Vec<Info>,
    error: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Info {
    name: String,
    version: String,
}

/// The answer is untrusted: only names in `foreign` survive, still unordered by version.
fn parse_info(body: &[u8], foreign: &[(String, String)]) -> Result<Vec<UpdateCandidate>, String> {
    let response: InfoResponse = serde_json::from_slice(body).map_err(|err| format!("unreadable AUR answer: {err}"))?;
    if let Some(error) = response.error {
        return Err(format!("AUR answered: {error}"));
    }
    let installed: HashMap<&str, &str> =
        foreign.iter().map(|(name, version)| (name.as_str(), version.as_str())).collect();
    Ok(response
        .results
        .into_iter()
        .filter_map(|info| {
            let old = *installed.get(info.name.as_str())?;
            Some(UpdateCandidate {
                old_version: old.to_string(),
                name: info.name,
                new_version: info.version,
                download_size: 0,
                installed_size: 0,
                repository: "aur".to_string(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paru_wins_over_yay_and_either_alone_is_found() {
        let both = tempfile::tempdir().unwrap();
        let yay_only = tempfile::tempdir().unwrap();
        for helper in HELPERS {
            std::fs::write(both.path().join(helper), "").unwrap();
        }
        std::fs::write(yay_only.path().join("yay"), "").unwrap();

        assert_eq!(first_helper(both.path().as_os_str()), Some("paru"));
        assert_eq!(first_helper(yay_only.path().as_os_str()), Some("yay"));
        assert_eq!(first_helper(OsStr::new("")), None);
    }

    fn foreign() -> Vec<(String, String)> {
        [("newer", "1.0-1"), ("same", "2.0-1"), ("epoch", "9.0-1"), ("older", "3.0-1")]
            .map(|(name, version)| (name.to_string(), version.to_string()))
            .to_vec()
    }

    #[test]
    fn only_requested_packages_survive() {
        let body = br#"{"type":"multiinfo","results":[
            {"Name":"newer","Version":"1.1-1"},
            {"Name":"same","Version":"2.0-1"},
            {"Name":"epoch","Version":"1:1.0-1"},
            {"Name":"older","Version":"2.9-1"},
            {"Name":"unrequested","Version":"5.0-1"}]}"#;

        let found = parse_info(body, &foreign()).unwrap();
        let names: Vec<_> = found.iter().map(|candidate| candidate.name.as_str()).collect();

        assert_eq!(names, ["newer", "same", "epoch", "older"], "nobody asked for `unrequested`");
        assert_eq!(found[0].old_version, "1.0-1");
        assert_eq!(found[0].new_version, "1.1-1");
        assert_eq!(found[0].repository, "aur");
    }

    #[test]
    fn an_error_answer_is_an_error_not_an_empty_list() {
        let body = br#"{"error":"No request type/data specified.","resultcount":0,"results":[],"type":"error"}"#;
        assert!(parse_info(body, &foreign()).is_err());
        assert!(parse_info(b"<html>", &foreign()).is_err());
    }
}
