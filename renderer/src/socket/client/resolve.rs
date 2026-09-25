use std::borrow::Cow;

use shared::{debug, error, notice, warn};

use super::RendererClient;
use crate::layout::Scene;
use crate::layout::instance::SurfaceInstance;
use crate::layout::node::SurfaceSpec;
use crate::lua;
use crate::lua::capability::CommandSender;

impl RendererClient {
    /// Applies the last evaluation to current instances, setting rescue on failure. Returns success
    /// so `crate::wayland::run` can log a startup that applied nothing. Instances come only from
    /// [`Self::set_instances`].
    pub fn apply_instances(&mut self) -> bool {
        let Some(output) = self.state.applied_output.as_ref() else {
            return false;
        };
        let applied = self.scene.apply_locked(
            &output.surfaces,
            &self.instances,
            &self.shaping,
            self.loader.lua(),
            self.holds_session_lock,
        );
        match applied {
            Ok(()) => {
                log_applied_surfaces(&self.scene, &self.instances);
                start_secure_submit_capabilities(&self.scene, &self.instances, &self.commands);
                self.rescue_applied_output(None);
                // Consume `set_screens`'s pre-evaluation seed (ADR-0041 decision 2) only after
                // success; a failed apply leaves it for the next one.
                self.dirty.take();
                self.last_resolved = None;
                self.settle_geometry();
                true
            }
            Err(err) => {
                error!("startup shell.lua evaluated but failed to apply to the scene: {err}");
                self.rescue_applied_output(Some(&err.to_string()));
                false
            }
        }
    }

    /// Applies `state.pending`. Returns whether the scene took it.
    pub fn handle_apply_pending(&mut self) -> bool {
        let Some((output, specs)) = self.state.pending.take() else {
            return false;
        };
        match self.scene.apply_locked(
            &output.surfaces,
            &self.instances,
            &self.shaping,
            self.loader.lua(),
            self.holds_session_lock,
        ) {
            Ok(()) => {
                notice!("shell reloaded");
                log_applied_surfaces(&self.scene, &self.instances);
                start_secure_submit_capabilities(&self.scene, &self.instances, &self.commands);
                self.set_rescue_state(false, "");
                self.state.applied_specs = specs;
                // ADR-0044 decision 2 re-resolve target.
                self.state.applied_output = Some(output);
                // The poll loop repaints on `re_resolve_if_dirty`.
                crate::lua::signal::reset_read_tracker(self.loader.lua());
                self.dirty.mark();
                self.last_resolved = None;
                self.settle_geometry();
                lua::timer::promote(self.loader.lua());
                lua::signal::promote_states(self.loader.lua());
                true
            }
            Err(err) => {
                lua::timer::discard(self.loader.lua());
                error!("the re-evaluated config failed to apply, keeping the prior scene: {err}");
                // Not `rescue_applied_output`: this failure is the pending evaluation's, which a
                // re-resolve of the prior scene cannot clear.
                self.set_rescue_state(true, &err.to_string());
                false
            }
        }
    }

    /// The pending evaluation's specs, and the declared ids whose fingerprint the applied set lacks:
    /// their protocol objects cannot be reused.
    pub fn pending_surfaces(&self) -> Option<(Vec<SurfaceSpec>, Vec<String>)> {
        let (_, specs) = self.state.pending.as_ref()?;
        let applied = &self.state.applied_specs;
        let rebuilt = specs
            .iter()
            .filter(|spec| !applied.iter().any(|old| old.fingerprint() == spec.fingerprint()))
            .map(|spec| spec.declared_id().to_string())
            .collect();
        Some((specs.clone(), rebuilt))
    }

    /// Re-runs `Scene::apply` against `state.applied_output` after a live signal marks the scene
    /// dirty (ADR-0044 decision 2). Never touches `shell.lua`: the retained tree holds its signals,
    /// readable through this client's field ordering and decision 1's resolve-at-layout-time rule.
    /// Called once per poll turn after inbound frames; `DirtyFlag::take` coalesces pushes. Returns
    /// whether it re-resolved; `false` means clean or failed.
    pub fn re_resolve_if_dirty(&mut self) -> bool {
        // Check before taking the flag: a failed first apply must not swallow pushes and stay blank
        // until an inotify edit forces reevaluation.
        let Some(output) = self.state.applied_output.as_ref() else {
            return false;
        };
        let scope = if self.holds_session_lock {
            if !self.dirty.take() {
                return false;
            }
            crate::lua::signal::DirtyScope::All
        } else {
            self.dirty.take_scope(self.loader.lua())
        };
        let (resolved_scope, instances) = match scope {
            // Also a follow-up whose moved rects nobody reads, which ends the chain.
            crate::lua::signal::DirtyScope::Clean => {
                self.geometry_follow_up = false;
                return false;
            }
            crate::lua::signal::DirtyScope::All => (None, Cow::Borrowed(self.instances.as_slice())),
            crate::lua::signal::DirtyScope::Instances(ids) => {
                let filtered: Vec<SurfaceInstance> = self
                    .instances
                    .iter()
                    .filter(|inst| ids.iter().any(|id| id == &inst.instance_id))
                    .cloned()
                    .collect();
                if filtered.is_empty() {
                    self.dirty.mark();
                    return false;
                }
                (Some(ids), Cow::Owned(filtered))
            }
        };
        let applied = self.scene.apply_locked(
            &output.surfaces,
            &instances,
            &self.shaping,
            self.loader.lua(),
            self.holds_session_lock,
        );
        if let Err(err) = applied {
            // Rollback keeps the prior scene; the rescue lasts until a pass applies.
            warn!("dirty-scene re-resolve failed, keeping the prior scene: {err}");
            self.rescue_applied_output(Some(&err.to_string()));
            self.dirty.mark();
            crate::lua::signal::reset_read_tracker(self.loader.lua());
            return false;
        }
        self.last_resolved = resolved_scope;
        start_secure_submit_capabilities(&self.scene, &instances, &self.commands);
        self.rescue_applied_output(None);
        self.settle_geometry();
        dump_layout_if_asked(&self.scene);
        true
    }

    /// Sets rescue for a failure to apply `applied_output`, or clears one when it applies. Any other
    /// rescue describes a file or lock the prior scene's success says nothing about.
    fn rescue_applied_output(&mut self, failed: Option<&str>) {
        match failed {
            Some(err) => {
                self.set_rescue_state(true, err);
                self.rescue_is_applied_output = true;
            }
            None if self.rescue_is_applied_output => self.set_rescue_state(false, ""),
            None => {}
        }
    }

    /// One follow-up pass over the readers of each `geometry(name)` rect a pass moved, so a
    /// property bound to the measurement lays out from it before anything else happens; never two
    /// in a row.
    fn settle_geometry(&mut self) {
        let moved = crate::lua::signal::take_geometry_moved(self.loader.lua());
        self.geometry_follow_up = !moved.is_empty() && !self.geometry_follow_up;
        if self.geometry_follow_up {
            for id in moved {
                self.dirty.mark_cell(id);
            }
        }
    }

    /// One animation frame (ADR-0145): advances every tween to `now` and relays out the instances
    /// that carry one, without reading `shell.lua` or any signal. Called from the poll loop when a
    /// compositor frame callback lands. Returns the instance ids it advanced, so the caller
    /// repaints those surfaces and no others.
    pub fn tick_animations(&mut self, now: std::time::Instant) -> Vec<String> {
        self.scene.tick(&self.instances, &self.shaping, self.loader.lua(), now)
    }
}

/// `MANTLE_DUMP_LAYOUT=<instance id>` (e.g. `bar@eDP-1`) prints each visible node's kind,
/// rect, and text after every pass. Off unless asked. It answers which node has the wrong geometry
/// in a live session, including layouts the test harness did not build (a card at a live output's
/// scale with the Supervisor's current feed).
fn dump_layout_if_asked(scene: &Scene) {
    let Ok(wanted) = std::env::var("MANTLE_DUMP_LAYOUT") else { return };
    let Some(surface) = scene.surface(&wanted) else { return };
    fn walk(node: &crate::layout::ResolvedNode, depth: usize, out: &mut String) {
        if !node.visible {
            return;
        }
        let text = node
            .properties
            .get("content")
            .map(|value| format!(" {}", crate::layout::node::preview_for_error(value)))
            .unwrap_or_default();
        out.push_str(&format!("{}{} {:?}{text}\n", "  ".repeat(depth), node.kind, node.rect));
        for child in &node.children {
            walk(child, depth + 1, out);
        }
    }
    let mut out = format!("layout dump: {wanted}\n");
    walk(surface, 0, &mut out);
    debug!(2; "{out}");
}

/// Starts capabilities named by applied `textfield` `secure_submit`s (ADR-0070 decision 5), so a
/// password prompt registers its agent even if nothing reads the member. The sender deduplicates.
///
/// Called from every successful apply, `re_resolve_if_dirty` included, so a `textfield` a pushed
/// value reveals registers its agent on the re-resolve that reveals it rather than waiting for the
/// next reevaluation.
///
/// ponytail: one tree walk per instance per re-resolve, at capability-push cadence.
/// `CommandSender::start_capability` dedupes, so a repeat costs one set lookup and no frame.
/// Upgrade: an accumulated roster, if the walk itself ever shows up.
fn start_secure_submit_capabilities(scene: &Scene, instances: &[SurfaceInstance], commands: &CommandSender) {
    for instance in instances {
        let Some(tree) = scene.surface(&instance.instance_id) else { continue };
        for target in crate::layout::secure_submit::secure_submit_targets(tree) {
            commands.start_capability(&target.capability);
        }
    }
}

/// Diagnostic geometry after `scene.apply`. Iterates *instances*, not declarations: one declaration
/// can produce several sizes.
fn log_applied_surfaces(scene: &Scene, instances: &[SurfaceInstance]) {
    dump_layout_if_asked(scene);
    for instance in instances {
        match scene.surface(&instance.instance_id) {
            Some(r) => debug!(
                2; "layout resolved: surface {:?} on {:?} kind={} rect={:?} visible={} children={} properties={}",
                instance.instance_id,
                instance.output,
                r.kind,
                r.rect,
                r.visible,
                r.children.len(),
                r.properties.len()
            ),
            None => {
                warn!("layout resolved but surface {:?} is absent from the applied scene", instance.instance_id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        instances_for, push_workspace, queued_starts, rescue_state, run_startup, test_client, write_shell_lua,
    };
    use super::super::*;

    /// ADR-0070 decision 5: polkit has no roster entry or `mantle.polkit`, so only a
    /// `secure_submit`
    /// naming it can request the authentication agent.
    #[test]
    fn a_secure_submit_target_starts_the_capability_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "prompt", layer = "Top", child = textfield {
                   secure_submit = { capability = "polkit", action = "authenticate" } } }"#,
        );
        let (mut client, mut outbound_rx) = test_client(&path);

        client.run_startup_evaluation().unwrap();
        client.set_instances(instances_for(&["prompt"]));
        assert!(client.apply_instances());

        assert!(queued_starts(&mut outbound_rx).contains(&"polkit".to_string()));
    }

    /// The other half of ADR-0070 decision 5: a field a pushed value reveals must register its
    /// agent on the re-resolve that reveals it. `re_resolve_if_dirty` never reads `shell.lua`, so
    /// waiting for the next reevaluation left a revealed prompt with no agent behind it.
    #[test]
    fn a_secure_submit_revealed_by_a_pushed_value_starts_its_capability_on_that_re_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            return panel { id = "prompt", layer = "Top", child = row { children = computed({mantle.network}, function(ssid)
                if ssid then
                    return { textfield { secure_submit = { capability = "polkit", action = "authenticate" } } }
                end
                return {}
            end) } }
            "#,
        );
        let (mut client, mut outbound_rx) = test_client(&path);
        run_startup(&mut client);
        assert!(
            !queued_starts(&mut outbound_rx).contains(&"polkit".to_string()),
            "nothing declares the field yet, so nothing has named polkit"
        );

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "network".to_string(),
                revision: 1,
                payload: serde_json::json!("home"),
            })
            .unwrap();
        assert!(client.re_resolve_if_dirty(), "the push must have re-resolved");

        assert!(
            client.scene.surface("prompt@TEST").unwrap().children[0].children.len() == 1,
            "the re-resolve must have revealed the field"
        );
        assert!(
            queued_starts(&mut outbound_rx).contains(&"polkit".to_string()),
            "the re-resolve that revealed the field must start the capability it names"
        );
    }

    /// A boot apply failure is `applied_output`'s own, so the first pass that applies clears it.
    #[test]
    fn a_boot_apply_failure_clears_when_a_re_resolve_applies() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = mantle.workspace }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        push_workspace(&mut client, 1, serde_json::json!("not a boolean"));
        assert!(!run_startup(&mut client));
        assert!(rescue_state(&client.loader).0);

        push_workspace(&mut client, 2, serde_json::json!(true));
        assert!(client.re_resolve_if_dirty());
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    /// A reload that evaluates but fails to apply is in rescue until a reload applies; a pass over
    /// the prior scene says nothing about the new file.
    #[test]
    fn a_reload_apply_failure_stays_in_rescue_until_a_reload_applies() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        push_workspace(&mut client, 1, serde_json::json!("not a boolean"));
        assert!(run_startup(&mut client));

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = mantle.workspace }"#);
        assert!(client.reevaluate());
        assert!(!client.handle_apply_pending());
        assert!(rescue_state(&client.loader).0, "an evaluation that fails to apply must reach rescue");

        client.dirty.mark();
        assert!(client.re_resolve_if_dirty());
        assert!(rescue_state(&client.loader).0, "a pass over the prior scene must not clear it");

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        assert!(client.reevaluate() && client.handle_apply_pending());
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    /// Evaluation success alone is not the truth: the banner clears only when the scene takes it.
    #[test]
    fn an_evaluation_rescue_survives_re_resolves_and_clears_on_an_applied_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"w = state("w", 1)
            return panel { id = "bar", layer = "Top", child = rect { width = w, height = 1 } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));

        std::fs::write(&path, "this is not lua").unwrap();
        assert!(!client.reevaluate());
        client.loader.lua().load("w:set(2)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert!(rescue_state(&client.loader).0, "the file is still broken");

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        assert!(client.reevaluate());
        assert!(rescue_state(&client.loader).0, "not cleared before the apply");
        assert!(client.handle_apply_pending());
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    #[test]
    fn handle_apply_pending_reconciles_the_pending_evaluation_into_the_scene() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        let (output, specs) = evaluate_and_specs(&client.loader, &path).unwrap();
        client.set_instances(instances_for(&["bar"]));
        client.state.pending = Some((output, specs));

        client.handle_apply_pending();

        assert!(client.scene.surface("bar@TEST").is_some());
        assert!(client.state.pending.is_none());
        assert_eq!(client.state.applied_specs.len(), 1);
        // The poll loop repaints only when `re_resolve_if_dirty` reports change.
        assert!(client.dirty.take(), "an applied in-place reload must mark the scene dirty");
    }

    // ADR-0044 decision 2: a `StateSnapshot` dirties the scene; dirty re-resolve uses the last
    // applied evaluation without `shell.lua`. `workspace` is outside `shared::Capability::ALL`, so
    // push it before `run_startup_evaluation` for a bare (not `:get()`) reference to evaluate.

    #[test]
    fn apply_state_snapshot_marks_the_scene_dirty() {
        let missing = std::path::PathBuf::from("/no/such/shell.lua");
        let (client, _outbound_rx) = test_client(&missing);
        assert!(!client.dirty.take(), "a fresh client must not start dirty");

        let snapshot = StateSnapshot {
            capability: "audio".to_string(),
            revision: 1,
            payload: serde_json::json!({ "app_name": "Zen" }),
        };
        client.apply_state_snapshot(snapshot).unwrap();

        assert!(client.dirty.take(), "LiveSignalHandle::set must mark the shared scene-dirty flag");
    }

    #[test]
    fn re_resolve_if_dirty_applies_a_pushed_value_without_reading_shell_lua_again() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = mantle.workspace }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 1,
                payload: serde_json::json!(true),
            })
            .unwrap();
        run_startup(&mut client);
        assert!(
            client.scene.surface("bar@TEST").unwrap().visible,
            "startup must have applied the pushed initial value"
        );

        // Break the file: re-resolve must read the retained tree's live signal, never disk.
        std::fs::write(&path, "this is not lua").unwrap();

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 2,
                payload: serde_json::json!(false),
            })
            .unwrap();
        client.re_resolve_if_dirty();

        assert!(!client.scene.surface("bar@TEST").unwrap().visible, "the re-resolve must reflect the pushed value");
        assert_eq!(
            rescue_state(&client.loader),
            (false, String::new()),
            "no evaluation error occurred -- shell.lua was never re-read, so the broken file on disk is never seen"
        );
    }

    #[test]
    fn re_resolve_if_dirty_clears_the_flag_and_a_second_call_does_no_work() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = mantle.workspace }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 1,
                payload: serde_json::json!(true),
            })
            .unwrap();
        run_startup(&mut client);
        client
            .apply_state_snapshot(StateSnapshot {
                capability: "workspace".to_string(),
                revision: 2,
                payload: serde_json::json!(false),
            })
            .unwrap();

        client.re_resolve_if_dirty();
        assert!(!client.scene.surface("bar@TEST").unwrap().visible, "the first re-resolve must apply the pushed value");
        assert!(!client.dirty.take(), "re_resolve_if_dirty must clear the flag it consumed");

        // Replace `applied_output` directly, bypassing the dirtying push path, with
        // `visible = true`.
        // A true no-op leaves the scene as the first resolve left it.
        let poisoned_path =
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "Top", visible = true }"#);
        let (poisoned_output, _) = evaluate_and_specs(&client.loader, &poisoned_path).unwrap();
        client.state.applied_output = Some(poisoned_output);

        client.re_resolve_if_dirty();
        assert!(
            !client.scene.surface("bar@TEST").unwrap().visible,
            "with nothing pushed since, a second re-resolve must do no work at all, even though a different applied_output is now in place"
        );
    }

    #[test]
    fn re_resolve_if_dirty_narrows_to_targeted_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            q = state("q", false)
            return {
                panel { id = "bar", layer = "Top" },
                panel { id = "modal", layer = "Top", visible = q },
            }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        run_startup(&mut client);
        assert_eq!(client.take_last_resolved(), None);

        client.loader.lua().load("q:set(true)").exec().unwrap();

        assert!(client.re_resolve_if_dirty());
        assert_eq!(client.take_last_resolved(), Some(vec!["modal@TEST".to_string()]));
    }

    /// ADR-0147 amendment: the follow-up a moved rect earns re-resolves the instances that read
    /// it, not the scene, and a follow-up nobody reads leaves the next move its own.
    #[test]
    fn a_moved_geometry_re_resolves_only_its_readers() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            g = geometry("card")
            w = state("w", 30)
            return {
                panel { id = "bar", layer = "Top", child = rect { width = w, height = 20, geometry = g } },
                panel { id = "reader", layer = "Top", child = rect { width = g:map(function(r) return r.width end), height = 1 } },
                panel { id = "other", layer = "Top" },
            }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));

        assert!(client.re_resolve_if_dirty(), "the first measurement earns a follow-up");
        assert_eq!(client.take_last_resolved(), Some(vec!["reader@TEST".to_string()]));
        assert!(!client.re_resolve_if_dirty(), "and only one");

        client.loader.lua().load("w:set(40)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(client.take_last_resolved(), Some(vec!["bar@TEST".to_string()]));
        assert!(client.re_resolve_if_dirty(), "a later move earns its own follow-up");
        assert_eq!(client.take_last_resolved(), Some(vec!["reader@TEST".to_string()]));
        let reader = client.scene.surface("reader@TEST").unwrap();
        assert_eq!(reader.children[0].rect.width, 40.0, "the reader laid out from the moved rect");
    }

    #[test]
    fn failed_narrowed_pass_does_not_freeze_retained_readers() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            q = state("q", true)
            armed = state("armed", false)
            local modal_opacity = computed({armed}, function(a)
                if a then return 2.0 else return 1.0 end
            end)
            return {
                panel { id = "bar", layer = "Top", visible = q },
                panel { id = "modal", layer = "Top", child = text { content = "m", opacity = modal_opacity } },
            }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        run_startup(&mut client);
        assert!(client.scene.surface("bar@TEST").unwrap().visible);

        // Fail modal resolution.
        client.loader.lua().load("armed:set(true)").exec().unwrap();
        assert!(!client.re_resolve_if_dirty(), "pass with invalid property must fail");
        assert!(client.scene.surface("bar@TEST").unwrap().visible, "retained tree preserved");

        // Disarm failure.
        client.loader.lua().load("armed:set(false)").exec().unwrap();
        assert!(client.re_resolve_if_dirty(), "recovery pass must succeed");

        // Mutate q: bar must still be tracked and resolve to false.
        client.loader.lua().load("q:set(false)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(client.take_last_resolved(), Some(vec!["bar@TEST".to_string()]));
        assert!(!client.scene.surface("bar@TEST").unwrap().visible);
    }
}
