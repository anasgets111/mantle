//! `mantle.applications`: installed `.desktop` entries (ADR-0061) and Unity `LauncherEntry`
//! badges. Entry scanning uses plain filesystem reads; live unread counts, progress and urgency
//! arrive over `com.canonical.Unity.LauncherEntry` on the session bus.
//!
//! ADR-0054 decision 5 called for this when a window needed an icon it did not report. The
//! launcher needs every name/icon/command, a focused window has `app_id` but no icon, and a tray
//! item may have neither `IconName` nor `IconPixmap`. Enumerate instead of the synchronous
//! `system:find_icon(app_id, ...)`: the control socket has no reply shape for it, and launchers
//! need the whole list.

mod badges;
pub mod controller;
pub mod entry;
pub mod scan;
mod watch;

use shared::warn;

pub use controller::{ApplicationsController, LaunchError, OpenUrlError};
pub use entry::current_locale;
pub use scan::application_dirs;
use shared::action::ApplicationsAction;

fn log_launch_error(verb: &str, target: &str, err: LaunchError) {
    let reason = match err {
        LaunchError::Unknown => format!("no application target {target}"),
        LaunchError::NoTerminal => {
            format!("{target} declares Terminal=true and $TERMINAL is unset, so there is no emulator to run it in")
        }
        LaunchError::Spawn(message) => format!("spawning {target} failed: {message}"),
    };
    warn!("{verb}: {reason}");
}

/// `mantle.applications` action dispatch (ADR-0037). `refresh` calls `spawn_blocking`; `launch`,
/// `launch_action` and `open_url` spawn detached children without waiting.
pub fn dispatch(controller: &ApplicationsController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<ApplicationsAction>(&envelope.params) else { return };
    match action {
        ApplicationsAction::Refresh => controller.refresh(),
        ApplicationsAction::Launch { id, opts } => {
            if let Err(err) = controller.launch(&id, &opts.env) {
                log_launch_error("launch", &format!("{id:?}"), err);
            }
        }
        ApplicationsAction::LaunchAction { id, action, opts } => {
            if let Err(err) = controller.launch_action(&id, &action, &opts.env) {
                log_launch_error("launch_action", &format!("{id:?} action {action:?}"), err);
            }
        }
        ApplicationsAction::OpenUrl { url } => {
            if let Err(err) = controller.open_url(&url) {
                let reason = match err {
                    OpenUrlError::Refused(why) => format!("refused {url:?}: {why}"),
                    OpenUrlError::Spawn(message) => format!("spawning xdg-open for {url:?} failed: {message}"),
                };
                warn!("open_url: {reason}");
            }
        }
    }
}
