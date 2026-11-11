//! `SupervisorFrame` protocol dispatch and state snapshot hydration.

use shared::{
    CallOutcome, CallResult, Declared, IdleEvent, Inject, ProcessExited, ProcessOutputLine, RendererFrame,
    SetSessionLock, StateSnapshot, SupervisorFrame, error, warn,
};

use super::RendererClient;
use crate::lua;

/// What an inbound frame still owes Wayland after [`RendererClient::handle_frame`]: `ApplyPending`
/// needs `crate::wayland::App`'s surfaces, `SetSessionLock` its SCTK lock state (ADR-0042).
#[derive(Debug, Clone, PartialEq)]
pub enum FrameOutcome {
    /// Fully serviced by [`RendererClient::handle_frame`].
    Handled,
    /// ADR-0042/ADR-0052's `SetSessionLock`: match the session lock to this flag
    /// (`crate::wayland::App::set_session_lock`).
    SetSessionLock(bool),
    /// A control client's `mantle input`: only `crate::wayland::App` has the surfaces to deliver it
    /// to, and it answers the call.
    Inject { id: u64, inject: Inject },
    /// A successful `Reevaluate`, which may change the surface set (`crate::wayland::App::apply_pending`).
    ApplyPending,
    /// Icons now resolve to other files under unchanged display lists.
    IconThemeChanged,
}

impl RendererClient {
    /// Hydrates only `snapshot.capability`; no Lua evaluation (`socket` module docs). `&self` is needed for
    /// the lazy `capabilities` registration.
    pub(super) fn apply_state_snapshot(&self, snapshot: StateSnapshot) -> mlua::Result<()> {
        let value = crate::lua::json::to_lua(self.loader.lua(), &snapshot.payload)?;
        let handle = self.capability_handle(&snapshot.capability)?;
        let previous = handle.hydrate(value);
        // Run `on_change` handlers (ADR-0115) after hydration and before layout. This is the only
        // Lua a push runs, not a `shell.lua` evaluation.
        handle.notify_change(self.loader.lua(), previous);
        Ok(())
    }

    /// Handles one [`pump`](super::super::pump)-decoded frame. Returns [`FrameOutcome`]: `Handled`, or work handed to
    /// `crate::wayland::App` for its surfaces or SCTK
    /// `SessionLockState`/lock surfaces (ADR-0042).
    #[must_use]
    pub fn handle_frame(&mut self, frame: SupervisorFrame) -> FrameOutcome {
        match frame {
            SupervisorFrame::StateSnapshot(snapshot) => {
                if let Err(err) = self.apply_state_snapshot(snapshot) {
                    warn!("failed to convert a pushed StateSnapshot to a Lua value: {err}");
                }
            }
            SupervisorFrame::Reevaluate => {
                if self.reevaluate() {
                    return FrameOutcome::ApplyPending;
                }
            }
            SupervisorFrame::ProcessOutput(ProcessOutputLine { id, stream, line }) => {
                self.process_registry.dispatch_output(id, stream, line);
            }
            SupervisorFrame::ProcessExited(ProcessExited { id, code }) => {
                self.process_registry.dispatch_exit(id, code);
            }
            // `ext_session_lock_v1` is a Wayland object; `crate::wayland::App` services it
            // (ADR-0042, ADR-0052 decision 1).
            SupervisorFrame::SetSessionLock(SetSessionLock { locked }) => return FrameOutcome::SetSessionLock(locked),
            // The Supervisor addresses this process via its own socket (ADR-0032).
            SupervisorFrame::IdleEvent(IdleEvent { generation_id: _, threshold_sec, state }) => {
                self.idle_registry.dispatch_event(threshold_sec, state);
            }
            // ADR-0112: `mantle set`/`mantle toggle`. The refusal goes to the log and back to the
            // waiting client, which exits non-zero with it; the write dirties the scene.
            SupervisorFrame::SetState { id, set } => {
                let outcome = match lua::signal::write_state(self.lua(), &set) {
                    Ok(()) => CallOutcome::Returned(serde_json::Value::Null),
                    Err(why) => {
                        warn!("`mantle` asked to write state {:?} and was refused: {why}", set.name);
                        CallOutcome::Failed(why)
                    }
                };
                if let Err(err) = self.commands.frames().send(RendererFrame::CallResult(CallResult { id, outcome })) {
                    error!("failed to answer the write to state {:?}: {err}", set.name);
                }
            }
            // ADR-0197: `mantle call`. Runs outside layout, like an `on_change` handler, and
            // always answers -- a caller is holding its socket open for this.
            SupervisorFrame::Call(call) => {
                let outcome = lua::action::dispatch(self.lua(), &call.name, &call.arguments);
                // A keybind's caller discards stderr, so the log is where a failure stays visible.
                if let CallOutcome::Failed(why) = &outcome {
                    warn!("`mantle call {}` failed: {why}", call.name);
                }
                let result = CallResult { id: call.id, outcome };
                if let Err(err) = self.commands.frames().send(RendererFrame::CallResult(result)) {
                    error!("failed to answer `mantle call {}`: {err}", call.name);
                }
            }
            // A bare `mantle call`/`set`/`toggle`: what this generation answers for.
            SupervisorFrame::ListDeclared { id, declared } => {
                let outcome = match declared {
                    Declared::Actions => CallOutcome::Returned(lua::action::names(self.lua())),
                    Declared::States => {
                        CallOutcome::Returned(serde_json::json!(lua::signal::declared_states(self.lua())))
                    }
                };
                if let Err(err) = self.commands.frames().send(RendererFrame::CallResult(CallResult { id, outcome })) {
                    error!("failed to answer a `mantle` listing: {err}");
                }
            }
            SupervisorFrame::Inject { id, inject } => return FrameOutcome::Inject { id, inject },
            SupervisorFrame::IconTheme(theme) => {
                if crate::image::icons::follow_portal_theme(theme) {
                    return FrameOutcome::IconThemeChanged;
                }
            }
            // The Supervisor routes these to control clients; one arriving here is a wire fault.
            SupervisorFrame::CallResult(result) => {
                error!("ignoring a CallResult for id {}; nothing here calls", result.id);
            }
        }
        FrameOutcome::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{test_client, write_shell_lua};
    use super::*;

    #[test]
    fn handle_frame_hands_a_successful_reevaluate_to_the_wayland_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        assert_eq!(
            client.handle_frame(SupervisorFrame::Reevaluate),
            FrameOutcome::ApplyPending,
            "every apply goes to the Wayland loop, which owns the protocol objects"
        );
        write_shell_lua(dir.path(), "this is not lua");
        assert_eq!(client.handle_frame(SupervisorFrame::Reevaluate), FrameOutcome::Handled);
    }

    #[test]
    fn handle_frame_hands_a_set_session_lock_back_to_the_wayland_loop_in_both_directions() {
        // Both directions matter: `locked = true` reaches Wayland for refusal without a `lock`
        // surface (ADR-0052 decision 3); `locked = false` is the only unlock path (ADR-0042).
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);

        assert_eq!(
            client.handle_frame(SupervisorFrame::SetSessionLock(SetSessionLock { locked: true })),
            FrameOutcome::SetSessionLock(true)
        );
        assert_eq!(
            client.handle_frame(SupervisorFrame::SetSessionLock(SetSessionLock { locked: false })),
            FrameOutcome::SetSessionLock(false)
        );
    }

    #[test]
    fn an_input_frame_is_left_for_the_surfaces_that_can_deliver_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        let inject = shared::Inject { surface: "bar".into(), steps: vec![shared::InputStep::Text("a".into())] };

        assert_eq!(
            client.handle_frame(SupervisorFrame::Inject { id: 7, inject: inject.clone() }),
            FrameOutcome::Inject { id: 7, inject }
        );
    }

    #[test]
    fn handle_frame_routes_process_output_and_exit_frames_to_the_registered_lua_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        // Register real callbacks through `process.run`; fresh registry id 0 is what inbound frames
        // address.
        client
            .loader
            .evaluate(
                r#"
                process.run("cmd", {}, function(line, stream) probe_line = line; probe_stream = stream end, function(code) probe_code = code end)
                return panel { id = "bar", layer = "top" }
                "#,
            )
            .unwrap();

        let output_frame = SupervisorFrame::ProcessOutput(ProcessOutputLine {
            id: 0,
            stream: shared::ProcessStream::Stdout,
            line: "hello".to_string(),
        });
        assert_eq!(client.handle_frame(output_frame), FrameOutcome::Handled);
        assert_eq!(
            client.handle_frame(SupervisorFrame::ProcessExited(ProcessExited { id: 0, code: Some(3) })),
            FrameOutcome::Handled
        );

        let lua = client.loader.lua().globals();
        assert_eq!(lua.get::<String>("probe_line").unwrap(), "hello");
        assert_eq!(lua.get::<String>("probe_stream").unwrap(), "stdout");
        assert_eq!(lua.get::<i64>("probe_code").unwrap(), 3);
    }
}
