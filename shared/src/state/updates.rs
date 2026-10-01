//! `mantle.updates` snapshot payload.

use crate::action::UpdateCandidate;
use serde::Serialize;

/// `mantle.updates`'s payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpdatesState {
    /// Package manager, e.g. `"pacman"`, from the first push, which comes at start; `nil` when none is
    /// supported, and then every action is ignored (ADR-0134).
    pub package_manager: Option<String>,
    /// AUR helper found at start, `"paru"` or `"yay"`, or `nil`; used only once `configure` sets
    /// `aur` (ADR-0250).
    pub aur_helper: Option<String>,
    /// Always `#packages`.
    pub count: u32,
    /// Pending upgrades. A failed check keeps the last good list.
    pub packages: Vec<UpdateCandidate>,
    /// Unix seconds of the last successful check (or the `checked_at` seed), else `nil`.
    pub last_successful_check: Option<i64>,
    /// Why the last check failed, or `nil` after a success. A check never modifies the system.
    pub check_error: Option<String>,
    /// Why AUR packages are missing: the last check's AUR query failed, or `aur` is on with no
    /// `aur_helper`; `nil` otherwise. `packages` still holds the repos' answer.
    pub aur_error: Option<String>,
    /// A check is running.
    pub checking: bool,
    /// Check failures in a row; a success resets it to `0`.
    pub consecutive_check_failures: u32,
    /// An install is running; the `install_*` fields describe the latest run.
    pub installing: bool,
    /// 1-based number of the package being installed, e.g. pacman's `(2/5)`; `0` before the first.
    pub install_current_step: u32,
    /// Packages in the transaction; `0` until the first step line, so draw progress as indeterminate.
    pub install_total_steps: u32,
    /// Package being installed; empty before the first step line.
    pub install_current_package: String,
    /// Package manager's exit code for the last install (`0` success); `nil` while running, before
    /// one, or when a signal killed it.
    pub install_exit_code: Option<i32>,
    /// Unix seconds when the last install's process ended, whatever its status; `nil` while
    /// running, before one, or when it failed to spawn.
    pub install_finished_at: Option<i64>,
    /// The last 200 lines of install output, stdout and stderr interleaved, newest last; cleared
    /// when an install starts.
    pub install_log: Vec<String>,
    /// Why the package manager could not be run or waited on, or `nil`. Its own failures are
    /// `install_exit_code`.
    pub install_error: Option<String>,
    /// `/run/mantle-reboot-required` exists, watched live. Mantle never writes it; anything you set up
    /// may, a pacman hook for example, and `/run` empties on reboot.
    pub reboot_required: bool,
}
