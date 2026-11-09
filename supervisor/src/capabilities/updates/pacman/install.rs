//! Progress from pacman's C-locale package operations and transaction summaries.
//! A missed line affects only UI progress; success comes from the exit status.

use super::super::backend::InstallStep;

#[derive(Default)]
pub struct Progress {
    current: u32,
    total: u32,
    package: String,
}

/// Summary totals include removals as well as additions. Numbered lines supply their own counts.
pub fn parse_install_step(progress: &mut Progress, line: &str) -> Option<InstallStep> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("Packages (") {
        *progress = Progress::default();
        let (total, rest) = rest.split_once(')')?;
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return None;
        }
        progress.total = total.parse().ok()?;
        return None;
    }
    let (counts, rest) = if let Some(rest) = line.strip_prefix('(') {
        let (counts, rest) = rest.split_once(')')?;
        let (current, total) = counts.split_once('/')?;
        let current: u32 = current.trim().parse().ok()?;
        let total: u32 = total.trim().parse().ok()?;
        if current == 0 || current > total {
            return None;
        }
        (Some((current, total)), rest.trim())
    } else {
        (None, line)
    };
    let rest = rest
        .strip_prefix("installing ")
        .or_else(|| rest.strip_prefix("upgrading "))
        .or_else(|| rest.strip_prefix("reinstalling "))
        .or_else(|| rest.strip_prefix("downgrading "))
        .or_else(|| rest.strip_prefix("removing "))?;
    let package = if counts.is_some() {
        rest.split_whitespace().next()?.trim_end_matches("...")
    } else {
        rest.strip_suffix("...")?
    };
    if package.is_empty() || !package.bytes().all(|c| c.is_ascii_alphanumeric() || b"@._+-".contains(&c)) {
        return None;
    }
    if let Some((current, total)) = counts {
        progress.current = current;
        progress.total = total;
    } else if package != progress.package {
        progress.current = progress.current.saturating_add(1).min(progress.total);
    }
    progress.package = package.to_string();
    Some(InstallStep { current: progress.current, total: progress.total, package: progress.package.clone() })
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
            ("(1/1) downgrading linux", 1, 1, "linux"),
            ("(1/1) removing old", 1, 1, "old"),
        ] {
            let step = parse_install_step(&mut Progress::default(), line).expect(line);
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
            "(0/1) installing nss",
            "(2/1) installing nss",
            "(1/1) upgrading   ",
            "upgrading bad/name...",
            "Packages (1)garbage",
            "Packages (4294967296)",
            "Packages (broken",
            "upgrading ...",
            "upgrading ",
            "upgrading nss",
            "upgrading nss ...",
            "(1/1) upgrading ...",
        ] {
            assert!(parse_install_step(&mut Progress::default(), line).is_none(), "{line:?}");
        }
    }

    #[test]
    fn summaries_and_numbered_steps_keep_one_transaction_count() {
        let mut progress = Progress::default();
        for (line, expected) in [
            ("Packages (3) a-1 b-1 c-1", None),
            ("upgrading a...", Some((1, 3, "a"))),
            ("(1/3) upgrading a", Some((1, 3, "a"))),
            ("upgrading a...", Some((1, 3, "a"))),
            ("reinstalling b...", Some((2, 3, "b"))),
            ("downgrading c...", Some((3, 3, "c"))),
            ("installing extra...", Some((3, 3, "extra"))),
            ("Packages (4294967295) huge", None),
            ("(4294967295/4294967295) upgrading last", Some((u32::MAX, u32::MAX, "last"))),
            ("upgrading overflow...", Some((u32::MAX, u32::MAX, "overflow"))),
            ("Packages (invalid)", None),
            ("upgrading unknown...", Some((0, 0, "unknown"))),
            ("Packages (1)garbage", None),
            ("installing bad-summary...", Some((0, 0, "bad-summary"))),
            ("Packages (1)", None),
            ("installing only...", Some((1, 1, "only"))),
            ("Packages (0)", None),
            (":: nothing to do", None),
        ] {
            let step = parse_install_step(&mut progress, line);
            assert_eq!(step.as_ref().map(|s| (s.current, s.total, s.package.as_str())), expected, "{line}");
        }
    }
}
