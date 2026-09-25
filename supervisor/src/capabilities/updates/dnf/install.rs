//! Install progress from `dnf upgrade`'s C-locale output, dnf5's or dnf4's.

use super::super::backend::InstallStep;

/// dnf5's `[ 3/12] Upgrading vim-data-2:9.2.1119-1 100% | ...` or dnf4's
/// `  Upgrading  : curl-minimal-7.76.1-43.el9.x86_64   2/6`. The total counts removals of the
/// old versions, and dnf5's verify and prepare steps. `None` for downloads, scriptlets and
/// dnf4's verify pass, which restarts at `1/`.
pub fn parse_install_step(line: &str) -> Option<InstallStep> {
    let line = line.trim();
    let (counts, verb, package) = match line.strip_prefix('[') {
        Some(rest) => {
            let (counts, rest) = rest.split_once(']')?;
            let mut words = rest.split_whitespace();
            (counts, words.next()?, dnf5_name(words.next()?))
        }
        None => {
            let (verb, rest) = line.split_once(':')?;
            let mut words = rest.split_whitespace();
            let nevra = words.next()?;
            (words.next_back()?, verb.trim(), dnf4_name(nevra)?)
        }
    };
    if !matches!(verb, "Installing" | "Upgrading" | "Downgrading" | "Reinstalling" | "Removing" | "Cleanup" | "Erasing")
    {
        return None;
    }
    let (current, total) = counts.split_once('/')?;
    Some(InstallStep {
        current: current.trim().parse().ok()?,
        total: total.trim().parse().ok()?,
        package: package.to_string(),
    })
}

/// dnf5 always prints the epoch, so the name ends at `-<epoch>:`.
/// ponytail: dnf5 cuts the nevra to a fixed column; a name longer than it is reported cut.
fn dnf5_name(nevra: &str) -> &str {
    nevra
        .split_once(':')
        .map_or(nevra, |(name_epoch, _)| name_epoch.rsplit_once('-').map_or(name_epoch, |(name, _)| name))
}

/// dnf4 prints `name-[epoch:]version-release.arch` whole.
fn dnf4_name(nevra: &str) -> Option<&str> {
    nevra.rsplit_once('.')?.0.rsplitn(3, '-').nth(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dnf5_transaction_lines_are_steps() {
        let step =
            parse_install_step("[ 4/12] Upgrading vim-minimal-2:9.2.111 100% |  75.0 MiB/s |   1.8 MiB |  00m00s");
        assert_eq!(step, Some(InstallStep { current: 4, total: 12, package: "vim-minimal".to_string() }));

        let step =
            parse_install_step("[12/12] Removing curl-0:8.18.0-4.fc44.x 100% | 236.0   B/s |  18.0   B |  00m00s");
        assert_eq!(step.unwrap().package, "curl");
    }

    #[test]
    fn dnf5_downloads_and_setup_steps_are_not() {
        assert_eq!(
            parse_install_step("[1/5] curl-0:8.18.0-10.fc44.x86_64      100% | 416.3 KiB/s | 240.2 KiB |  00m01s"),
            None
        );
        assert_eq!(
            parse_install_step("[ 1/12] Verify package files            100% | 416.0   B/s |   5.0   B |  00m00s"),
            None
        );
        assert_eq!(parse_install_step("Upgrading:"), None);
    }

    #[test]
    fn dnf4_transaction_lines_are_steps_and_its_verify_pass_is_not() {
        let step =
            parse_install_step("  Upgrading        : libcurl-minimal-7.76.1-43.el9.x86_64                   1/6 ");
        assert_eq!(step, Some(InstallStep { current: 1, total: 6, package: "libcurl-minimal".to_string() }));
        let step =
            parse_install_step("  Cleanup          : vim-minimal-2:9.2.240-1.fc40.x86_64                    5/6 ");
        assert_eq!(step.unwrap().package, "vim-minimal");

        assert_eq!(
            parse_install_step("  Verifying        : curl-minimal-7.76.1-43.el9.x86_64                      1/6 "),
            None
        );
        assert_eq!(
            parse_install_step("  Running scriptlet: libcurl-minimal-7.76.1-41.el9.x86_64                   6/6 "),
            None
        );
        assert_eq!(
            parse_install_step("  Preparing        :                                                        1/1 "),
            None
        );
    }
}
