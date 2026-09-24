//! `mantle.applications`: installed `.desktop` entries (ADR-0061). A top-level capability using
//! plain filesystem reads, with no D-Bus proxy or hardware thread.
//!
//! ADR-0054 decision 5 called for this when a window needed an icon it did not report. The
//! launcher needs every name/icon/command, a focused window has `app_id` but no icon, and a tray
//! item may have neither `IconName` nor `IconPixmap`. Enumerate instead of the synchronous
//! `system:find_icon(app_id, ...)`: the control socket has no reply shape for it, and launchers
//! need the whole list.

pub mod controller;
pub mod entry;
pub mod scan;
mod watch;

use shared::warn;

pub use controller::{ApplicationsController, ApplicationsSignal, LaunchError, OpenUrlError};
pub use scan::application_dirs;

#[derive(Debug, serde::Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ApplicationsAction {
    /// Rescans installed desktop entries. The directories are watched, so only a failed watch
    /// (logged) needs this.
    Refresh,
    /// Launches `entries[].id`, detached; `Terminal=true` entries run in `$TERMINAL`.
    Launch { id: String },
    /// Opens an `http`, `https` or `mailto` URL with `xdg-open` (ADR-0103). One over 2048 bytes or
    /// holding whitespace or a control character is refused.
    OpenUrl { url: String },
}

/// `mantle.applications` action dispatch (ADR-0037). `refresh` calls `spawn_blocking`; `launch`
/// and `open_url` spawn detached children without waiting.
pub fn dispatch(controller: &ApplicationsController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<ApplicationsAction>(&envelope.params) else { return };
    match action {
        ApplicationsAction::Refresh => controller.refresh(),
        ApplicationsAction::Launch { id } => {
            if let Err(err) = controller.launch(&id) {
                let reason = match err {
                    LaunchError::Unknown => format!("no application entry with id {id:?}"),
                    LaunchError::NoTerminal => {
                        format!(
                            "{id:?} declares Terminal=true and $TERMINAL is unset, so there is no emulator to run it in"
                        )
                    }
                    LaunchError::Spawn(message) => format!("spawning {id:?} failed: {message}"),
                };
                warn!("launch: {reason}");
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
