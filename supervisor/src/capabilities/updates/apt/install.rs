//! Install progress from `apt-get`'s C-locale output. apt numbers nothing, so the step count comes
//! from its summary line and each `Setting up` line after it.

use super::super::backend::InstallStep;

/// Reads `line` against `progress`, the run's `(done, total)`. The summary sets the total and
/// restarts the count; each package dpkg configures is one step.
pub fn parse_install_step(progress: &mut (u32, u32), line: &str) -> Option<InstallStep> {
    if let Some(total) = transaction_size(line) {
        *progress = (0, total);
        return None;
    }
    // `Setting up gzip (1.12-1ubuntu3.2) ...`
    let package = line.strip_prefix("Setting up ")?.split(' ').next()?;
    let (done, total) = progress;
    // A package left unconfigured by an earlier run is set up too, uncounted in the summary.
    *done = (*done + 1).min(*total);
    Some(InstallStep { current: *done, total: *total, package: package.to_string() })
}

/// `4 upgraded, 1 newly installed, 0 to remove and 2 not upgraded.` as `5`.
fn transaction_size(line: &str) -> Option<u32> {
    let (upgraded, rest) = line.split_once(" upgraded, ")?;
    let (installed, _) = rest.split_once(" newly installed, ")?;
    Some(upgraded.parse::<u32>().ok()? + installed.parse::<u32>().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `apt-get -y upgrade --with-new-pkgs` as root on Ubuntu 24.04, trimmed.
    const UBUNTU_INSTALL: &str = "\
Reading package lists...
The following packages will be upgraded:
  gzip libaudit-common libaudit1 perl-base
4 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.
Need to get 1978 kB of archives.
Get:1 http://archive.ubuntu.com/ubuntu noble-updates/main amd64 gzip amd64 1.12-1ubuntu3.2 [99.2 kB]
Fetched 1978 kB in 1s (1697 kB/s)
Preparing to unpack .../gzip_1.12-1ubuntu3.2_amd64.deb ...
Unpacking gzip (1.12-1ubuntu3.2) over (1.12-1ubuntu3) ...
Setting up gzip (1.12-1ubuntu3.2) ...
Unpacking perl-base (5.38.2-3.2ubuntu0.6) over (5.38.2-3.2ubuntu0.4) ...
Setting up perl-base (5.38.2-3.2ubuntu0.6) ...
Setting up libaudit-common (1:3.1.2-2.1ubuntu0.1) ...
Unpacking libaudit1:amd64 (1:3.1.2-2.1ubuntu0.1) over (1:3.1.2-2.1build1.1) ...
Setting up libaudit1:amd64 (1:3.1.2-2.1ubuntu0.1) ...
Processing triggers for libc-bin (2.39-0ubuntu8.9) ...
";

    #[test]
    fn each_setting_up_line_is_the_next_of_the_summary_count() {
        let mut progress = (0, 0);
        let steps: Vec<_> = UBUNTU_INSTALL.lines().filter_map(|line| parse_install_step(&mut progress, line)).collect();
        let step = |current, package: &str| InstallStep { current, total: 4, package: package.to_string() };
        assert_eq!(
            steps,
            [step(1, "gzip"), step(2, "perl-base"), step(3, "libaudit-common"), step(4, "libaudit1:amd64")]
        );
    }

    #[test]
    fn the_summary_restarts_a_count_left_by_the_last_run() {
        let mut progress = (7, 9);
        assert_eq!(
            parse_install_step(&mut progress, "1 upgraded, 2 newly installed, 0 to remove and 0 not upgraded."),
            None
        );
        assert_eq!(progress, (0, 3));
    }

    #[test]
    fn a_step_beyond_the_summary_stays_at_the_total() {
        let mut progress = (1, 1);
        let step = parse_install_step(&mut progress, "Setting up libc-bin (2.39-0ubuntu8.9) ...").unwrap();
        assert_eq!((step.current, step.total), (1, 1));
    }
}
