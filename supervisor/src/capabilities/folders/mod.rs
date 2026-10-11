//! `mantle.folders` keeps a config-requested folder listing current through inotify (ADR-0120).
//! Plain filesystem reads and one kernel watch per folder, with no D-Bus proxy or hardware thread.
//!
//! The config VM has no `io` (ADR-0048); `process.run("ls")` would parse lines for a table the
//! Supervisor can provide. The first caller was a wallpaper picker, but screenshot trays, download
//! shelves, and file browsers need the same folder watcher.

pub mod controller;

pub use controller::FoldersController;
use shared::action::FoldersAction;

/// `mantle.folders`'s action dispatch (ADR-0037). Synchronous: `watch` spawns the listing and the
/// inotify loop as a task and returns, and `unwatch` aborts that task.
pub fn dispatch(controller: &FoldersController, envelope: &shared::CommandEnvelope) {
    let Some(action) = crate::parse_action::<FoldersAction>(&envelope.params) else { return };
    match action {
        FoldersAction::Watch { path, extensions } => controller
            .watch(&path, extensions.iter().map(|ext| ext.trim_start_matches('.').to_ascii_lowercase()).collect()),
        FoldersAction::Unwatch { path } => controller.unwatch(&path),
    }
}
