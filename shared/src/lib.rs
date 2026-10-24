pub mod action;
mod atomic;
mod capability;
mod frame;
pub mod framing;
pub mod log;
mod malloc;
mod paths;
#[cfg(feature = "schema")]
pub mod schema;
mod secure_buffer;
pub mod state;
pub use atomic::write_atomic;
pub use capability::Capability;
pub use frame::{
    CONTROL_CLIENT_GENERATION, Call, CallOutcome, CallResult, CommandEnvelope, CommandParams, ConnectionHandshake,
    Declared, IdleEvent, IdleState, Inject, InputButton, InputStep, LockOutcome, LockReport, PamMessage, PamOutcome,
    ProcessExited, ProcessOutputLine, ProcessStream, RendererFrame, ResumeCause, SecureSubmit, SetSessionLock,
    SetState, StateSnapshot, StateWrite, SupervisorFrame, valid_secret_name,
};
pub use malloc::Malloc;
pub use paths::{
    CHECK_ENV, CONFIG_DIR_ENV, EXIT_COMPOSITOR_GONE, GENERATION_ID_ENV, INSTANCE_DIR_ENV, PROFILE_ENV, VERBOSE_ENV,
    config_dir, control_socket_path, expand_home, expand_tilde, instance_dir, profile_interval, runtime_root,
    session_locked_flag_path, shell_lua_path, xdg_data_dirs, xdg_dir,
};
pub use secure_buffer::SecureBuffer;
pub use zeroize::{Zeroize, Zeroizing};

/// std's `eprintln!`/`eprint!` panic on a failed write, and under `panic = "abort"` a full log tmpfs
/// (ADR-0199) would kill lock authority. Both binaries import these over std's with `#[macro_use]`.
///
/// ponytail: a dependency's own write or panic still aborts; containing that needs a separate
/// lock/PAM process.
#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        let _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!("{}\n", format_args!($($arg)*)));
    }};
}

/// See [`eprintln!`].
#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {{
        let _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!($($arg)*));
    }};
}

/// One diagnostic at that level, named by the module it was written in (ADR-0199, amended).
///
/// Imported by path (`use shared::warn;`) rather than through `#[macro_use]`, which both crate roots
/// apply only `cfg(not(test))`, leaving library code under test unable to name these.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Error, module_path!(), format_args!($($arg)*)) };
}

/// See [`error!`].
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Warn, module_path!(), format_args!($($arg)*)) };
}

/// See [`error!`]. The shell's lifecycle: start, reload, respawn, stop (ADR-0251).
#[macro_export]
macro_rules! notice {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Notice, module_path!(), format_args!($($arg)*)) };
}

/// See [`error!`].
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Info, module_path!(), format_args!($($arg)*)) };
}

/// See [`error!`]. `debug!(2; "...")` writes at verbosity 2 (`-vvv`); bare
/// `debug!(...)` is verbosity 1, and any other verbosity fails to compile. The `;` (not `,`) keeps a plain format string from ever parsing as
/// a verbosity: `debug!("a, b")`'s first token is the whole string literal, not a bare integer.
#[macro_export]
macro_rules! debug {
    (2; $($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Debug(2), module_path!(), format_args!($($arg)*))
    };
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Debug(1), module_path!(), format_args!($($arg)*))
    };
}

/// The golden-file check behind every `the_generated_*` test, over `(path from the repo root,
/// rendered)`: `UPDATE_STUBS=1` rewrites the stale files, otherwise a stale one fails naming `just stubs`.
///
/// A test, not a build step: the renderers read types and registrations `build.rs` runs before, and
/// writing source during a build would break read-only checkouts. The output is checked in because
/// the language server reads the working tree, so a fresh clone has stubs before it compiles.
#[doc(hidden)]
pub fn check_generated(files: &[(String, String)]) {
    let path = |file: &str| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(file);
    let stale: Vec<&(String, String)> = files
        .iter()
        .filter(|(file, rendered)| std::fs::read_to_string(path(file)).ok().as_ref() != Some(rendered))
        .collect();
    if std::env::var_os("UPDATE_STUBS").is_some() {
        for (file, rendered) in stale {
            std::fs::write(path(file), rendered).expect("the generated file is writable");
        }
        return;
    }
    let stale: Vec<&str> = stale.iter().map(|(file, _)| file.as_str()).collect();
    assert!(stale.is_empty(), "{stale:?} are stale. Run `just stubs` and commit the result.");
}
