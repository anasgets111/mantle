use std::borrow::Cow;

use shared::{debug, error, notice, warn};

use super::RendererClient;
use crate::layout::Scene;
use crate::layout::instance::SurfaceInstance;
use crate::layout::node::SurfaceSpec;
use crate::lua;
use crate::lua::capability::CommandSender;

impl RendererClient {
    /// Applies the last evaluation to current instances, setting rescue on failure. Returns whether
    /// any surface applied, so `crate::wayland::run` can log a startup that applied nothing.
    /// Instances come only from [`Self::set_instances`].
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
            false,
        );
        match applied {
            Ok(failed) => {
                log_applied_surfaces(&self.scene, &self.instances);
                start_secure_submit_capabilities(&self.scene, &self.instances, &self.commands);
                self.note_pass(failed.map(|err| err.to_string()));
                // Consume `set_screens`'s pre-evaluation seed (ADR-0041 decision 2) only after
                // success; a failed apply leaves it for the next one.
                self.dirty.take();
                self.last_resolved = None;
                self.settle_layout();
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
            true,
        ) {
            Ok(_) => {
                self.drop_orphan_scrolls();
                notice!("shell reloaded");
                log_applied_surfaces(&self.scene, &self.instances);
                start_secure_submit_capabilities(&self.scene, &self.instances, &self.commands);
                self.set_rescue_state(false, "");
                log_re_resolve(&mut self.re_resolve_failure, None);
                self.state.applied_specs = specs;
                // ADR-0044 decision 2 re-resolve target.
                self.state.applied_output = Some(output);
                // The poll loop repaints on `re_resolve_if_dirty`.
                crate::lua::signal::reset_read_tracker(self.loader.lua());
                self.dirty.mark();
                self.last_resolved = None;
                self.settle_layout();
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
    /// A scroll offset the pass clamped or revealed under a getter re-resolves its readers once more
    /// this turn: the clamp depends on sizes that depend on the offset.
    // ponytail: one follow-up, a second clamp waits for the next write; upgrade: a capped fixed point.
    pub fn re_resolve_if_dirty(&mut self) -> bool {
        // Wheel clamps applied in place: no Lua reader by construction.
        self.dirty.take_quiet();
        if !self.pass_if_dirty() {
            return false;
        }
        let settled = self.scene.read_by_lua(self.dirty.take_quiet());
        if !settled.is_empty() {
            for cell in settled {
                self.dirty.mark_cell(cell);
            }
            let first = self.last_resolved.take();
            self.last_resolved = if self.pass_if_dirty() {
                first.zip(self.last_resolved.take()).map(|(mut ids, more)| {
                    ids.extend(more);
                    ids.sort();
                    ids.dedup();
                    ids
                })
            } else {
                first
            };
        }
        true
    }

    fn pass_if_dirty(&mut self) -> bool {
        // Every writer of this turn has returned, so no handler runs inside one (ADR-0288).
        lua::signal::run_state_handlers(self.loader.lua());
        self.owes_pass = false;
        // Check before taking the flag: a failed first apply must not swallow pushes and stay blank
        // until an inotify edit forces reevaluation.
        let Some(output) = self.state.applied_output.as_ref() else {
            return false;
        };
        // Keeps the memo table alive across `take_scope` and the subsequent `apply_locked`,
        // handing over evaluated computeds so they evaluate exactly once per dirty push.
        // Dropped on clean, empty-instance, or failed exits, clearing the memo table.
        let _memo = crate::lua::signal::EvaluationMemo::enter(self.loader.lua());
        // After a failure the read tracker is reset, so any mark retries the whole scene.
        let scope = if self.holds_session_lock || self.re_resolve_failure.is_some() {
            if !self.dirty.take() {
                drop(_memo);
                return false;
            }
            crate::lua::signal::DirtyScope::All
        } else {
            self.dirty.take_scope(self.loader.lua())
        };
        let (resolved_scope, instances) = match scope {
            // Also a follow-up whose moved rects nobody reads, which ends the chain.
            crate::lua::signal::DirtyScope::Clean => {
                drop(_memo);
                // A request on a signal no container reads re-lays out no instance.
                self.drop_orphan_scrolls();
                self.layout_follow_up = false;
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
                    drop(_memo);
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
            false,
        );
        drop(_memo);
        // No re-mark on failure: only a change can fix it, and the wakes between changes cannot.
        let failed = match applied {
            Ok(failed) => failed,
            Err(err) => {
                self.note_pass(Some(err.to_string()));
                return false;
            }
        };
        self.drop_orphan_scrolls();
        start_secure_submit_capabilities(&self.scene, &instances, &self.commands);
        self.note_pass(failed.map(|err| err.to_string()));
        self.last_resolved = resolved_scope;
        self.settle_layout();
        dump_layout_if_asked(&self.scene);
        true
    }

    /// Decided after a pass, not at the call, so a request made with the area it scrolls is consumed
    /// first; a request no surface's tree holds a container for is dropped.
    fn drop_orphan_scrolls(&self) {
        self.dirty.drop_orphan_requests(|cell| self.scene.holds_scroll(cell));
    }

    /// Logs a pass's failure once per run and holds rescue, or clears both when every surface
    /// applied. Failed instances keep their prior trees; the tracker reset makes every mark retry
    /// the whole scene until one applies.
    fn note_pass(&mut self, failure: Option<String>) {
        log_re_resolve(&mut self.re_resolve_failure, failure.clone());
        self.rescue_applied_output(failure.as_deref());
        if failure.is_some() {
            crate::lua::signal::reset_read_tracker(self.loader.lua());
        }
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

    /// One follow-up pass over the readers of each layout measurement a pass changed, so a
    /// property bound to the measurement lays out from it before anything else happens; never two
    /// in a row.
    fn settle_layout(&mut self) {
        let moved = crate::lua::signal::take_layout_changed(self.loader.lua());
        self.layout_follow_up = !moved.is_empty() && !self.layout_follow_up;
        if self.layout_follow_up {
            for id in moved {
                self.dirty.mark_cell(id);
            }
        }
    }

    /// One animation frame (ADR-0145) for the instances in `due`, whose compositor frame callbacks
    /// landed: advances their tweens to `now` and relays them out, without reading `shell.lua` or
    /// any signal. Returns the instance ids it advanced, so the caller repaints those surfaces and
    /// no others. A tree that settles owes its geometry readers one pass. Changed text truncation
    /// updates its readers even while a layout tween is running.
    pub fn tick_animations(&mut self, due: &[String], now: std::time::Instant) -> Vec<String> {
        let instances = self.instances.iter().filter(|instance| due.contains(&instance.instance_id));
        let ticked = self.scene.tick(instances, &self.shaping, self.loader.lua(), now);
        for id in crate::lua::signal::take_layout_changed(self.loader.lua()) {
            self.dirty.mark_cell(id);
        }
        ticked
    }
}

/// Folds a pass's outcome (`None` for any pass that applied) into `run`, logging what
/// [`fold_failure`] owes.
fn log_re_resolve(run: &mut Option<(String, u32)>, failure: Option<String>) {
    for line in fold_failure(run, failure) {
        error!("layout pass failed, keeping each failed surface's last applied tree: {line}");
    }
}

/// Folds one pass's outcome (`None` for success) into `run`, the last logged failure and its
/// unlogged repeats. Returns the lines owed to the log: the ended run's repeat count, then a new
/// failure.
fn fold_failure(run: &mut Option<(String, u32)>, failure: Option<String>) -> Vec<String> {
    if let (Some((last, repeats)), Some(failure)) = (run.as_mut(), failure.as_ref())
        && last == failure
    {
        *repeats += 1;
        return Vec::new();
    }
    let ended = std::mem::replace(run, failure.clone().map(|failure| (failure, 0)));
    let ended = ended.filter(|(_, repeats)| *repeats > 0).map(|(last, repeats)| format!("{last} (repeats: {repeats})"));
    ended.into_iter().chain(failure).collect()
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
        instances_for, push_workspace, queued_starts, rescue_state, run_startup, test_client, test_outputs,
        write_shell_lua,
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
            r#"return panel { id = "prompt", layer = "top", child = textfield {
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
            return panel { id = "prompt", layer = "top", child = row { children = computed({mantle.network}, function(ssid)
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
        let path = write_shell_lua(
            dir.path(),
            r#"return panel { id = "bar", layer = "top", child = column { children = mantle.workspace } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        push_workspace(&mut client, 1, serde_json::json!("not a list"));
        assert!(!run_startup(&mut client));
        assert!(rescue_state(&client.loader).0);

        push_workspace(&mut client, 2, serde_json::json!([]));
        assert!(client.re_resolve_if_dirty());
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    /// A reload that evaluates but fails to apply is in rescue until a reload applies; a pass over
    /// the prior scene says nothing about the new file.
    #[test]
    fn a_reload_apply_failure_stays_in_rescue_until_a_reload_applies() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
        let (mut client, _outbound_rx) = test_client(&path);
        push_workspace(&mut client, 1, serde_json::json!("not a boolean"));
        assert!(run_startup(&mut client));

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top", visible = mantle.workspace }"#);
        assert!(client.reevaluate());
        assert!(!client.handle_apply_pending());
        assert!(rescue_state(&client.loader).0, "an evaluation that fails to apply must reach rescue");

        client.dirty.mark();
        assert!(client.re_resolve_if_dirty());
        assert!(rescue_state(&client.loader).0, "a pass over the prior scene must not clear it");

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
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
            return panel { id = "bar", layer = "top", child = rect { width = w, height = 1 } }"#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));

        std::fs::write(&path, "this is not lua").unwrap();
        assert!(!client.reevaluate());
        client.loader.lua().load("w:set(2)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert!(rescue_state(&client.loader).0, "the file is still broken");

        write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
        assert!(client.reevaluate());
        assert!(rescue_state(&client.loader).0, "not cleared before the apply");
        assert!(client.handle_apply_pending());
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    #[test]
    fn handle_apply_pending_reconciles_the_pending_evaluation_into_the_scene() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top" }"#);
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
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top", visible = mantle.workspace }"#);
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
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top", visible = mantle.workspace }"#);
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
            write_shell_lua(dir.path(), r#"return panel { id = "bar", layer = "top", visible = true }"#);
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
                panel { id = "bar", layer = "top" },
                panel { id = "modal", layer = "top", visible = q },
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
                panel { id = "bar", layer = "top", child = rect { width = w, height = 20, geometry = g } },
                panel { id = "reader", layer = "top", child = rect { width = g:map(function(r) return r.width end), height = 1 } },
                panel { id = "other", layer = "top" },
            }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));

        assert!(!client.re_resolve_if_dirty(), "the reader, laid out after the card, mapped its first measurement");

        client.loader.lua().load("w:set(40)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(client.take_last_resolved(), Some(vec!["bar@TEST".to_string()]));
        assert!(client.re_resolve_if_dirty(), "a later move earns its own follow-up");
        assert_eq!(client.take_last_resolved(), Some(vec!["reader@TEST".to_string()]));
        let reader = client.scene.surface("reader@TEST").unwrap();
        assert_eq!(reader.children[0].rect.width, 40.0, "the reader laid out from the moved rect");
    }

    #[test]
    fn changed_elision_re_resolves_its_readers_without_re_resolving_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"cut = elided("body")
            w = state("w", 40)
            return {
                panel { id = "writer", layer = "top", child = text { width = w,
                    content = "A label too long for its box", elide = "end", elided = cut } },
                panel { id = "reader", layer = "top", visible = cut },
                panel { id = "other", layer = "top" },
            }"#,
        );
        let (mut client, _) = test_client(&path);
        assert!(run_startup(&mut client));
        assert!(client.re_resolve_if_dirty());
        assert_eq!(client.take_last_resolved(), Some(vec!["reader@TEST".to_string()]));
        assert!(client.scene.surface("reader@TEST").unwrap().visible);
        assert!(!client.re_resolve_if_dirty(), "an unchanged measurement schedules no work");
        client.loader.lua().load("w:set(500)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(client.take_last_resolved(), Some(vec!["writer@TEST".to_string()]));
        assert!(client.re_resolve_if_dirty());
        assert_eq!(client.take_last_resolved(), Some(vec!["reader@TEST".to_string()]));
        assert!(!client.scene.surface("reader@TEST").unwrap().visible);
        assert!(!client.re_resolve_if_dirty());
    }

    #[test]
    fn elision_that_changes_its_own_width_gets_only_one_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"local cut = elided("body")
            return panel { id = "bar", layer = "top", child = text {
                width = cut:map(function(v) return v and 500 or 40 end),
                content = "A label too long for its box", elide = "end", elided = cut
            } }"#,
        );
        let (mut client, _) = test_client(&path);
        assert!(run_startup(&mut client));
        assert!(client.re_resolve_if_dirty());
        assert_eq!(client.scene.surface("bar@TEST").unwrap().children[0].rect.width, 500.0);
        assert!(!client.re_resolve_if_dirty(), "the measurement cannot start a second follow-up");
    }

    #[test]
    fn elision_changes_during_a_width_tween_update_readers_and_clear_on_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"cut = elided("body")
            w = state("w", 40)
            return {
                panel { id = "writer", layer = "top", child = text { width = w,
                    animate = { width = { duration = 100, easing = "linear" } },
                    content = "A label too long for its box", elide = "end", elided = cut } },
                panel { id = "reader", layer = "top", visible = cut },
            }"#,
        );
        let (mut client, _) = test_client(&path);
        assert!(run_startup(&mut client));
        assert!(client.re_resolve_if_dirty());
        client.loader.lua().load("w:set(500)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        let started = client.scene.surface("writer@TEST").unwrap().children[0].tweens[0].started;
        client.tick_animations(&["writer@TEST".into()], started + std::time::Duration::from_millis(90));
        assert!(client.re_resolve_if_dirty(), "readers update before the animation settles");
        assert!(!client.scene.surface("reader@TEST").unwrap().visible);
        client.loader.lua().load("w:set(40)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        client.tick_animations(&["writer@TEST".into()], started + std::time::Duration::from_secs(1));
        assert!(client.re_resolve_if_dirty());
        assert!(client.scene.surface("reader@TEST").unwrap().visible);
        client.forget_surface("writer@TEST");
        assert!(client.re_resolve_if_dirty(), "removing the last writer clears the signal");
        assert!(!client.scene.surface("reader@TEST").unwrap().visible);
    }

    /// The loop's own path for a hover-held pill: a `delay` under two computeds lets go on the turn
    /// it comes due.
    #[test]
    fn a_lingering_width_collapses_on_the_turn_its_delay_comes_due() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            h = state("h", false)
            local linger = computed({ h, delay(h, 20) }, function(now, was) return now == true or was == true end)
            local expanded = computed({ linger, state("held", false) }, function(o, held) return o or held end)
            local probe = computed({ h, linger, expanded }, function(_, _, e) return e end)
            local function cell(shown)
                return row { width = computed({ expanded, shown }, function(o, k) return (o or k) and 34 or 0 end),
                    height = 20, opacity = computed({ expanded, shown }, function(o, k) return (o or k) and 1 or 0 end),
                    animate = { width = 100, opacity = 100 } }
            end
            return { panel { id = "bar", layer = "top", child = row { children = {
                row { spacing = probe:map(function(o) return o and 7 or 0 end), animate = { spacing = 100 },
                    children = { cell(state("s1", false)), cell(state("s2", true)) } } } } } }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        let widths = |client: &RendererClient| -> Vec<f32> {
            client.scene.surface("bar@TEST").unwrap().children[0].children[0]
                .children
                .iter()
                .map(|c| c.rect.width)
                .collect()
        };
        let turn = |client: &mut RendererClient| {
            client.wake_due_signals();
            client.re_resolve_if_dirty();
            client.tick_animations(
                &["bar@TEST".to_string()],
                std::time::Instant::now() + std::time::Duration::from_secs(1),
            );
            client.re_resolve_if_dirty();
        };
        client.loader.lua().load("h:set(true)").exec().unwrap();
        turn(&mut client);
        std::thread::sleep(std::time::Duration::from_millis(40));
        turn(&mut client);
        assert_eq!(widths(&client), [34.0, 34.0]);
        client.loader.lua().load("h:set(false)").exec().unwrap();
        turn(&mut client);
        assert_eq!(widths(&client), [34.0, 34.0], "the delay holds it open");
        std::thread::sleep(std::time::Duration::from_millis(40));
        turn(&mut client);
        assert_eq!(widths(&client), [0.0, 34.0], "and lets go on the turn it comes due");
    }

    /// A computed first run after a write its own pass made changes as of that write, so a
    /// surface resolved earlier in the pass, which read the old value, still resolves again.
    #[test]
    fn a_computed_changed_by_a_mid_pass_write_reaches_the_surfaces_resolved_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            wide = state("wide", false)
            local g = geometry("g")
            local x = computed({ g }, function(r) return r and r.width or 0 end)
            return {
                panel { id = "early", layer = "top", child = row { children = { row { width = x, height = 5 } } } },
                panel { id = "src", layer = "top", child = row { children = {
                    row { geometry = g, width = wide:map(function(w) return w and 50 or 10 end), height = 5 } } } },
                panel { id = "late", layer = "top", child = row { children = {
                    row { width = computed({ g, x, wide }, function(_, v, w) return w and v + 1 or v end), height = 5 } } } },
            }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        let width = |client: &RendererClient, id: &str| {
            client.scene.surface(&format!("{id}@TEST")).unwrap().children[0].children[0].rect.width
        };
        for _ in 0..4 {
            client.re_resolve_if_dirty();
        }
        assert_eq!((width(&client, "early"), width(&client, "late")), (10.0, 10.0));

        client.loader.lua().load("wide:set(true)").exec().unwrap();
        for _ in 0..4 {
            client.re_resolve_if_dirty();
        }
        assert_eq!((width(&client, "early"), width(&client, "late")), (50.0, 51.0));
    }

    /// A failure logs once per run of the same text; the run's repeat count lands when it
    /// changes or clears, so a failure retried on every push cannot flood the log.
    #[test]
    fn a_repeated_failure_logs_once_and_reports_its_repeats_when_it_ends() {
        let mut run = None;
        let mut fold = |failure: Option<&str>| super::fold_failure(&mut run, failure.map(str::to_string));

        assert_eq!(fold(Some("a")), ["a"]);
        assert!(fold(Some("a")).is_empty());
        assert!(fold(Some("a")).is_empty());
        assert_eq!(fold(Some("b")), ["a (repeats: 2)", "b"]);
        assert!(fold(None).is_empty(), "a failure that never repeated owes no count");
        assert_eq!(fold(Some("b")), ["b"], "a success ends the run, so the same text logs again");
        assert!(fold(Some("b")).is_empty());
        assert_eq!(fold(None), ["b (repeats: 1)"]);
    }

    /// A failed pass owes no retry on its own: the loop also wakes for pointer motion and frame
    /// callbacks, and none of them can change what failed. The next change retries.
    #[test]
    fn a_failed_pass_waits_for_a_change_before_it_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            armed = state("armed", false)
            return panel { id = "bar", layer = "top", child = text { content = "m",
                opacity = computed({armed}, function(a) if a then error("broken") else return 1.0 end end) } }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));

        client.loader.lua().load("armed:set(true)").exec().unwrap();
        assert!(!client.re_resolve_if_dirty(), "a raising getter must fail the pass");
        // The rescue write the failure made is a change of its own; it earns one retry.
        client.re_resolve_if_dirty();
        assert!(!client.dirty.take(), "a failed pass must not re-dirty the scene");

        client.loader.lua().load("armed:set(false)").exec().unwrap();
        assert!(client.re_resolve_if_dirty(), "the next change must retry the whole scene");
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    /// Any applied pass ends the run, so the same failure after it logs again rather than
    /// counting as a repeat of one the scene has since recovered from.
    #[test]
    fn an_applied_startup_pass_ends_a_re_resolve_failure_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            armed = state("armed", false)
            return panel { id = "bar", layer = "top", child = text { content = "m",
                opacity = computed({armed}, function(a) if a then error("broken") else return 1.0 end end) } }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        client.loader.lua().load("armed:set(true)").exec().unwrap();
        assert!(!client.re_resolve_if_dirty());
        assert!(client.re_resolve_failure.is_some());

        client.loader.lua().load("armed:set(false)").exec().unwrap();
        assert!(client.apply_instances());

        assert!(client.re_resolve_failure.is_none());
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
                if a then error("broken") else return 1.0 end
            end)
            return {
                panel { id = "bar", layer = "top", visible = q },
                panel { id = "modal", layer = "top", child = text { content = "m", opacity = modal_opacity } },
            }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        run_startup(&mut client);
        assert!(client.scene.surface("bar@TEST").unwrap().visible);

        // Fail modal resolution.
        client.loader.lua().load("armed:set(true)").exec().unwrap();
        assert!(!client.re_resolve_if_dirty(), "pass with a raising getter must fail");
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

    /// One broken getter froze every surface, the rescue banner included, until it was fixed.
    #[test]
    fn a_broken_surface_keeps_its_prior_tree_while_the_others_and_the_banner_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            q = state("q", true)
            armed = state("armed", false)
            return {
                panel { id = "bar", layer = "top", visible = q },
                panel { id = "menu", layer = "top", child = column { children = {
                    rect { height = 1, width = q:map(function(v) return v and 10 or 20 end) },
                    text { content = "m", opacity = armed:map(function(a) return a and error("broken") or 1.0 end) },
                } } },
                panel { id = "banner", layer = "top",
                    visible = mantle.rescue:map(function(r) return r ~= nil and r.is_rescue end) },
            }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        assert!(!client.scene.surface("banner@TEST").unwrap().visible);

        client.loader.lua().load("armed:set(true)").exec().unwrap();
        assert!(!client.re_resolve_if_dirty(), "the narrowed pass held only the broken menu");
        assert!(client.re_resolve_if_dirty(), "the rescue write's pass applies the other surfaces");
        assert!(client.scene.surface("banner@TEST").unwrap().visible, "the banner shows the failure");

        client.loader.lua().load("q:set(false)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert!(!client.scene.surface("bar@TEST").unwrap().visible, "an unbroken surface keeps updating");
        let menu_rect =
            |client: &RendererClient| client.scene.surface("menu@TEST").unwrap().children[0].children[0].rect;
        assert_eq!(menu_rect(&client).width, 10.0, "the broken one keeps its prior tree");
        assert!(rescue_state(&client.loader).0);

        client.loader.lua().load("armed:set(false)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(menu_rect(&client).width, 20.0, "fixed, it catches up");
        assert_eq!(rescue_state(&client.loader), (false, String::new()));
    }

    /// A bad value drops for its default and the rest of its surface applies: it froze the whole
    /// surface, a sibling's text and handlers included.
    #[test]
    fn a_bad_value_applies_its_default_while_its_siblings_update_and_rescue_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            op = state("op", 0.5)
            label = state("label", "a")
            clicked = state("clicked", "")
            return {
                panel { id = "menu", layer = "top", child = column { children = {
                    text { content = "m", opacity = op },
                    text { content = label, on_click = label:map(function(l)
                        return function() clicked:set(l) end
                    end) },
                } } },
                panel { id = "banner", layer = "top",
                    visible = mantle.rescue:map(function(r) return r ~= nil and r.is_rescue end) },
            }
            "#,
        );
        let (mut client, _outbound_rx) = test_client(&path);
        assert!(run_startup(&mut client));
        let menu = |client: &RendererClient| client.scene.surface("menu@TEST").unwrap().children[0].clone();
        let lua = client.loader.lua().clone();

        lua.load(r#"op:set(2) label:set("b")"#).exec().unwrap();
        assert!(client.re_resolve_if_dirty(), "the pass applies");
        let column = menu(&client);
        assert_eq!(column.children[0].opacity, 1.0, "the bad node draws at the default");
        let on_click = crate::layout::node::fields::pointer::on_click.read(&column.children[1].properties);
        on_click.unwrap().expect("the sibling's handler").call::<()>(()).unwrap();
        assert_eq!(lua.load("return clicked:get()").eval::<String>().unwrap(), "b", "and it is the new one");
        let (is_rescue, error_log) = rescue_state(&client.loader);
        assert!(is_rescue && error_log.contains("`opacity`") && error_log.contains("got 2"), "{error_log}");
        assert!(client.re_resolve_if_dirty(), "the rescue write's pass");
        assert!(client.scene.surface("banner@TEST").unwrap().visible, "the banner shows it");

        lua.load(r#"label:set("c")"#).exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert!(rescue_state(&client.loader).0, "an unrelated pass still reports the standing value");

        lua.load("op:set(0.25)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(menu(&client).children[0].opacity, 0.25);
        assert_eq!(rescue_state(&client.loader), (false, String::new()), "fixed, rescue clears");
    }

    #[test]
    fn a_dirty_capability_push_runs_a_computed_body_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            runs = 0
            local sig = computed({mantle.network}, function(net)
                runs = runs + 1
                return net and net.connected and "online" or "offline"
            end)
            return panel { id = "status", layer = "top", child = text { content = sig } }
            "#,
        );
        let (mut client, _) = test_client(&path);
        run_startup(&mut client);

        let initial_runs: i64 = client.loader.lua().globals().get("runs").unwrap();
        assert_eq!(initial_runs, 1, "startup must evaluate the computed once");

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "network".to_string(),
                revision: 1,
                payload: serde_json::json!({ "connected": true }),
            })
            .unwrap();

        assert!(client.re_resolve_if_dirty(), "the dirty push must re-resolve");

        let after_runs: i64 = client.loader.lua().globals().get("runs").unwrap();
        assert_eq!(
            after_runs, 2,
            "the computed body must run exactly once across take_scope and the layout pass (was {after_runs})"
        );

        let table = client.loader.lua().app_data_ref::<crate::lua::signal::MemoTable>().unwrap();
        assert!(table.map.is_empty(), "memo map must be empty after re-resolve");
        assert_eq!(table.depth, 0, "memo depth must be 0 after re-resolve");
    }

    #[test]
    fn a_clean_re_resolve_drops_the_memo_without_leaking() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            runs = 0
            local sig = computed({mantle.network}, function(net)
                runs = runs + 1
                return net and net.connected and "online" or "offline"
            end)
            return panel { id = "status", layer = "top", child = text { content = sig } }
            "#,
        );
        let (mut client, _) = test_client(&path);
        run_startup(&mut client);

        client
            .apply_state_snapshot(StateSnapshot {
                capability: "network".to_string(),
                revision: 1,
                payload: serde_json::Value::Null,
            })
            .unwrap();

        assert!(!client.re_resolve_if_dirty(), "unmodified state should not re-resolve");

        let table = client.loader.lua().app_data_ref::<crate::lua::signal::MemoTable>().unwrap();
        assert!(table.map.is_empty(), "memo map must be empty when scope is clean");
        assert_eq!(table.depth, 0, "memo depth must be 0 when scope is clean");
    }

    /// A wheel past either end: the pass clamps the offset after the getter read the wheel's value,
    /// so the getter re-resolves in the same call and lays out from the offset on screen.
    #[test]
    fn a_getter_reads_the_clamped_scroll_offset_in_the_same_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            r#"
            s = scroll("s")
            runs = 0
            local tiles = {}
            for i = 1, 6 do tiles[i] = rect { width = 50, height = 20 } end
            return panel { id = "bar", layer = "top", child = column { children = {
                row { width = 100, height = 20, scroll = s, children = tiles },
                rect { height = 1, width = s:map(function(o) runs = runs + 1 return o + 1 end) },
                rect { height = 1, width = s:map(function(o) return o + 1 end):map(function(w) return w * 2 end) },
            } } }
            "#,
        );
        let (mut client, _rx) = test_client(&path);
        assert!(run_startup(&mut client));
        let signal = crate::lua::signal::from_userdata(&client.loader.lua().globals().get("s").unwrap()).unwrap();
        let readout =
            |client: &RendererClient| client.scene.surface("bar@TEST").unwrap().children[0].children[1].rect.width;
        let runs = |client: &RendererClient| client.loader.lua().globals().get::<i64>("runs").unwrap();

        // Two wheel frames in one turn: one pass, one evaluation.
        let before = runs(&client);
        for asked in [30.0, 60.0] {
            assert_eq!(client.scroll_in_place(&signal, asked), None, "a getter reads the offset");
            signal.scroll_handle().unwrap().set_changed(mlua::Value::Number(asked.into()));
        }
        assert!(client.re_resolve_if_dirty());
        assert_eq!((runs(&client) - before, readout(&client)), (1, 61.0));

        for (asked, used) in [(900.0, 200.0), (-40.0, 0.0)] {
            signal.scroll_handle().unwrap().set_changed(mlua::Value::Number(asked));
            assert!(client.re_resolve_if_dirty());
            assert_eq!(signal.scroll_offset(), Some(used as f32), "300 px of tiles in a 100 px row");
            let chained = client.scene.surface("bar@TEST").unwrap().children[0].children[2].rect.width;
            assert_eq!(chained, (used as f32 + 1.0) * 2.0, "a map of the map follows too");
            assert_eq!(readout(&client), used as f32 + 1.0, "the getter saw {used}, not {asked}");
            assert!(!client.re_resolve_if_dirty(), "nothing is left for the next turn");
        }
    }

    /// Six 50 px tiles in a 100 px row (200 px of room) scrolled by `s`, and a getter of `s` beside it
    /// counting its runs. `animate` is the row's.
    fn scrolled_row(animate: &str) -> (RendererClient, crate::lua::signal::Signal, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(
            dir.path(),
            &format!(
                r#"
                s = scroll("s")
                n = state("n", 6)
                runs = 0
                return panel {{ id = "bar", layer = "top", reset_on_close = {{ s }}, child = column {{ children = {{
                    row {{ width = 100, height = 20, scroll = s, animate = {animate},
                        children = n:map(function(n)
                            local tiles = {{}}
                            for i = 1, n do tiles[i] = rect {{ width = 50, height = 20 }} end
                            return tiles
                        end) }},
                    rect {{ height = 1, width = s:map(function(o) runs = runs + 1 return o + 1 end) }},
                }} }} }}
                "#
            ),
        );
        let (mut client, _) = test_client(&path);
        assert!(run_startup(&mut client));
        let signal = crate::lua::signal::from_userdata(&client.loader.lua().globals().get("s").unwrap()).unwrap();
        (client, signal, dir)
    }

    fn runs(client: &RendererClient) -> i64 {
        client.loader.lua().globals().get("runs").unwrap()
    }

    /// The getter's width less one, and the first tile's x: the offset Lua saw and the one drawn.
    fn shown(client: &RendererClient) -> (f32, f32) {
        let column = &client.scene.surface("bar@TEST").unwrap().children[0];
        (column.children[1].rect.width - 1.0, -column.children[0].children[0].rect.x)
    }

    /// Smooth wheel scrolling: a notch eases the offset over frames, a second notch mid-run retargets
    /// from what is on screen, the target stops at an end, and a reader sees each frame's offset in
    /// that frame with one evaluation.
    #[test]
    fn a_notch_eases_the_offset_frame_by_frame_and_its_readers_follow() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        assert_eq!(client.wheel(&signal, 40.0, true, t0), ["bar@TEST"]);
        assert!(!client.re_resolve_if_dirty(), "the notch writes nothing until a frame");
        let step = |client: &mut RendererClient, at| {
            let before = runs(client);
            client.advance_scrolls(&["bar@TEST".to_string()], at);
            client.re_resolve_if_dirty();
            (runs(client) - before, shown(client))
        };
        assert_eq!(step(&mut client, ms(50)), (1, (20.0, 20.0)), "halfway, read and drawn in one frame");
        // A second notch at 20 heads for 80 from 20: no jump.
        client.wheel(&signal, 40.0, true, ms(50));
        // A pass that resolves the row again keeps its run.
        client.loader.lua().load("n:set(7)").exec().unwrap();
        assert_eq!(step(&mut client, ms(100)), (1, (50.0, 50.0)));
        assert_eq!(step(&mut client, ms(150)), (1, (80.0, 80.0)), "settled on the target");
        assert_eq!(step(&mut client, ms(200)), (0, (80.0, 80.0)), "a settled run asks nothing more");
        assert!(!client.scene.surface("bar@TEST").unwrap().animating());

        client.wheel(&signal, 900.0, true, ms(200));
        assert_eq!(step(&mut client, ms(250)), (1, (165.0, 165.0)), "halfway to the end, not to 980");
        assert_eq!(step(&mut client, ms(300)), (1, (250.0, 250.0)), "the target stops at the end of seven tiles");
        assert_eq!(signal.scroll_offset(), Some(250.0));

        // A touchpad follows the finger: it stops the run and moves from what is on screen.
        client.wheel(&signal, -100.0, true, ms(300));
        assert_eq!(step(&mut client, ms(350)), (1, (200.0, 200.0)));
        client.wheel(&signal, 5.0, false, ms(350));
        assert!(client.re_resolve_if_dirty());
        assert_eq!(shown(&client), (205.0, 205.0));
        assert_eq!(step(&mut client, ms(400)), (0, (205.0, 205.0)), "the run is gone");
    }

    /// Under `animate.scroll` a `:reveal` eases the run to the child like a notch would.
    #[test]
    fn a_reveal_eases_under_animate_scroll() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        client.loader.lua().load("s:reveal(5)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        // After the pass that started the run, so its clock cannot be ahead of this one.
        let t0 = std::time::Instant::now();
        assert_eq!(shown(&client), (0.0, 0.0), "the pass starts the run and moves nothing yet");
        client.advance_scrolls(&["bar@TEST".to_string()], t0 + std::time::Duration::from_millis(50));
        client.re_resolve_if_dirty();
        let (read, drawn) = shown(&client);
        assert!(read > 0.0 && read < 150.0 && read == drawn, "{read} on its way to 150, read as drawn");
        client.advance_scrolls(&["bar@TEST".to_string()], t0 + std::time::Duration::from_secs(1));
        client.re_resolve_if_dirty();
        assert_eq!(shown(&client), (150.0, 150.0), "tile 5 ends flush with the right edge");
        assert_eq!(signal.scroll_offset(), Some(150.0));
    }

    /// One frame of the smooth scrolls on `bar@TEST`, then the pass, as the main loop orders them:
    /// the reader's evaluations that frame and the offsets read and drawn.
    fn frame(client: &mut RendererClient, at: std::time::Instant) -> (i64, (f32, f32)) {
        let before = runs(client);
        client.advance_scrolls(&["bar@TEST".to_string()], at);
        client.re_resolve_if_dirty();
        (runs(client) - before, shown(client))
    }

    /// `reset_on_close` writes the top while a run is in flight: the run stops instead of dragging
    /// the offset back on the next frame.
    #[test]
    fn a_reset_on_close_stops_a_run() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        client.reset_closed_surfaces(&["bar@TEST"]);
        client.wheel(&signal, 40.0, true, t0);
        assert_eq!(frame(&mut client, ms(50)).1, (20.0, 20.0));
        client.reset_closed_surfaces(&[]);
        assert!(client.re_resolve_if_dirty());
        assert_eq!(frame(&mut client, ms(80)).1, (0.0, 0.0));
        assert!(!client.scene.surface("bar@TEST").unwrap().animating());
    }

    /// An easing that overshoots its target at the end never reports an offset past the room: one
    /// evaluation a frame, no follow-up pass to pull a reader back.
    #[test]
    fn an_overshooting_run_at_the_end_never_reads_past_the_room() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "out_back" } }"#);
        client.wheel(&signal, 160.0, false, std::time::Instant::now());
        assert!(client.re_resolve_if_dirty());
        let t0 = std::time::Instant::now();
        client.wheel(&signal, 120.0, true, t0);
        for at in [40, 60, 80, 100] {
            let (runs, (read, drawn)) = frame(&mut client, t0 + std::time::Duration::from_millis(at));
            assert!(runs <= 1 && read <= 200.0 && read == drawn, "{at} ms: {runs} runs, read {read}, drawn {drawn}");
        }
        assert_eq!(shown(&client), (200.0, 200.0));
    }

    /// Content that shrinks under a run: frames stop at the new end, and a notch back mid-run moves
    /// from that end rather than from the target the content no longer reaches.
    #[test]
    fn a_run_whose_content_shrinks_stops_at_the_new_end() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        client.wheel(&signal, 900.0, true, t0);
        assert_eq!(frame(&mut client, ms(50)).1, (100.0, 100.0), "halfway to 200");
        client.loader.lua().load("n:set(3)").exec().unwrap();
        assert_eq!(frame(&mut client, ms(60)).1, (50.0, 50.0), "three tiles leave 50 px");
        assert_eq!(frame(&mut client, ms(70)), (0, (50.0, 50.0)), "held at the end, nothing to re-read");
        client.wheel(&signal, -40.0, true, ms(70));
        assert_eq!(frame(&mut client, ms(170)).1, (10.0, 10.0), "back one notch from the end");
    }

    /// A spring in flight hands its speed to the run a second notch starts, so the motion bends
    /// instead of starting again from still.
    #[test]
    fn a_notch_mid_spring_keeps_its_speed() {
        let spring = r#"{ scroll = { spring = { stiffness = 200, damping = 20 } } }"#;
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        let (mut moving, signal, _dir) = scrolled_row(spring);
        moving.wheel(&signal, 40.0, true, t0);
        let (_, (from, _)) = frame(&mut moving, ms(30));
        moving.wheel(&signal, 40.0, true, ms(30));
        let carried = frame(&mut moving, ms(40)).1.0 - from;

        let (mut still, signal, _dir) = scrolled_row(spring);
        still.wheel(&signal, from, false, t0);
        assert!(still.re_resolve_if_dirty());
        still.wheel(&signal, 80.0 - from, true, ms(30));
        let rested = frame(&mut still, ms(40)).1.0 - from;
        assert!(carried > rested, "from {from}: moving {carried} px in 10 ms, from still {rested}");
    }

    /// Without `animate.scroll` a notch writes its offset at once, as before.
    #[test]
    fn a_notch_without_animate_scroll_moves_at_once() {
        let (mut client, signal, _dir) = scrolled_row("{}");
        assert!(client.wheel(&signal, 40.0, true, std::time::Instant::now()).is_empty());
        assert!(client.re_resolve_if_dirty());
        assert_eq!(shown(&client), (40.0, 40.0));
    }

    /// `:scroll_to` under `animate.scroll` eases like a notch, its reader evaluated once a frame,
    /// and settles on the target clamped to the room.
    #[test]
    fn a_scroll_to_eases_under_animate_scroll_and_stops_at_the_ends() {
        let (mut client, signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        client.loader.lua().load("s:scroll_to(900)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        assert_eq!(shown(&client), (0.0, 0.0), "the pass starts the run and moves nothing yet");
        let (evaluations, (read, drawn)) = frame(&mut client, ms(50));
        assert!(evaluations == 1 && (100.0..200.0).contains(&read) && read == drawn, "{evaluations}: {read}, {drawn}");
        assert_eq!(frame(&mut client, ms(1000)), (1, (200.0, 200.0)), "six tiles leave 200 px");
        assert_eq!(signal.scroll_offset(), Some(200.0));
        client.loader.lua().load("s:scroll_to(-50)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(frame(&mut client, ms(2000)).1, (0.0, 0.0));
    }

    /// Without `animate.scroll` both requests land in the pass, clamped; bad arguments raise.
    #[test]
    fn a_scroll_request_without_animate_scroll_moves_at_once() {
        let (mut client, _signal, _dir) = scrolled_row("{}");
        for (src, used) in [
            ("s:scroll_to(120)", 120.0),
            ("s:scroll_by(30)", 150.0),
            ("s:scroll_by(900)", 200.0),
            ("s:scroll_to(-5)", 0.0),
        ] {
            client.loader.lua().load(src).exec().unwrap();
            assert!(client.re_resolve_if_dirty());
            assert_eq!(shown(&client), (used, used), "{src}");
        }
        for src in ["s:scroll_to(0/0)", "s:scroll_by(math.huge)", "n:scroll_to(1)"] {
            let err = client.loader.lua().load(src).exec().unwrap_err().to_string();
            assert!(err.contains("finite number") || err.contains("only valid on a scroll"), "{src}: {err}");
        }
        assert!(!client.re_resolve_if_dirty(), "a refused request asks for nothing");
    }

    /// A page of `bar` holding a 200 px column (100 px of room) scrolled by `s`; the other page holds
    /// none. `side` holds a hidden column scrolled by `s` when `extra` is set.
    fn paged(extra: bool) -> (RendererClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let side = if extra {
            r#", panel { id = "side", layer = "top", child = column { width = 100, height = 100, scroll = s,
                visible = shown, children = tiles(10) } }"#
        } else {
            ""
        };
        let path = write_shell_lua(
            dir.path(),
            &format!(
                r#"
                s = scroll("s")
                page = state("page", 1)
                shown = state("shown", false)
                function tiles(n)
                    local out = {{}}
                    for i = 1, n do out[i] = rect {{ width = 10, height = 20 }} end
                    return out
                end
                return {{ panel {{ id = "bar", layer = "top", child = column {{ children = page:map(function(p)
                    if p == 1 then return {{ column {{ width = 100, height = 100, scroll = s, children = tiles(10) }} }} end
                    return {{ rect {{ width = 5, height = 5 }} }}
                end) }} }} {side} }}
                "#
            ),
        );
        let (mut client, _) = test_client(&path);
        assert!(run_startup(&mut client));
        (client, dir)
    }

    fn reload(client: &mut RendererClient, path: &std::path::Path, source: &str) {
        std::fs::write(path, source).unwrap();
        assert!(client.reevaluate());
        let (specs, _) = client.pending_surfaces().unwrap();
        client.set_instances(crate::layout::instance::expand_instances(&specs, &test_outputs()));
        assert!(client.handle_apply_pending());
    }

    /// Runs `src`, then passes until nothing is dirty; the offset `s` reports.
    fn then_offset(client: &mut RendererClient, src: &str) -> f32 {
        client.loader.lua().load(src).exec().unwrap();
        while client.re_resolve_if_dirty() {}
        let signal: mlua::AnyUserData = client.loader.lua().globals().get("s").unwrap();
        crate::lua::signal::from_userdata(&signal).unwrap().scroll_offset().unwrap()
    }

    /// A request made while no container holds the signal is dropped by the next pass, so the area
    /// that comes back starts at the top.
    #[test]
    fn a_scroll_request_with_no_holder_is_dropped_by_the_next_pass() {
        let (mut client, _dir) = paged(false);
        then_offset(&mut client, "page:set(2)");
        then_offset(&mut client, "s:scroll_by(50)");
        assert_eq!(then_offset(&mut client, "page:set(1)"), 0.0);
    }

    /// Adding the area and scrolling it in one handler still lands: the pass that builds it
    /// consumes the request before the check.
    #[test]
    fn a_request_made_with_the_area_it_scrolls_lands() {
        let (mut client, _dir) = paged(false);
        then_offset(&mut client, "page:set(2)");
        assert_eq!(then_offset(&mut client, "page:set(1) s:scroll_to(900)"), 100.0, "clamped to its room");
    }

    /// A container hidden in another surface still holds the signal: the request waits for its show.
    #[test]
    fn a_request_held_by_another_surface_waits_for_it() {
        let (mut client, _dir) = paged(true);
        then_offset(&mut client, "page:set(2)");
        then_offset(&mut client, "s:scroll_to(60)");
        assert_eq!(then_offset(&mut client, "shown:set(true)"), 60.0);
    }

    /// A named scroll survives a reload, and so does its request: the reload's own pass decides it.
    #[test]
    fn a_pending_request_survives_a_reload_that_keeps_its_area() {
        let (mut client, dir) = paged(false);
        client.loader.lua().load("s:scroll_to(60)").exec().unwrap();
        let path = dir.path().join("shell.lua");
        reload(&mut client, &path, &std::fs::read_to_string(&path).unwrap());
        let signal: mlua::AnyUserData = client.loader.lua().globals().get("s").unwrap();
        let signal = crate::lua::signal::from_userdata(&signal).unwrap();
        while client.re_resolve_if_dirty() {}
        assert_eq!(signal.scroll_offset(), Some(60.0));
    }

    /// The same reload without the area: the first pass drops the request.
    #[test]
    fn a_pending_request_is_dropped_by_the_reload_that_removes_its_area() {
        let (mut client, dir) = paged(false);
        client.loader.lua().load("s:scroll_to(60)").exec().unwrap();
        let path = dir.path().join("shell.lua");
        reload(&mut client, &path, r#"s = scroll("s") return panel { id = "bar", layer = "top", child = rect {} }"#);
        while client.re_resolve_if_dirty() {}
        let signal: mlua::AnyUserData = client.loader.lua().globals().get("s").unwrap();
        assert!(crate::lua::signal::from_userdata(&signal).unwrap().pending_scroll().is_none());
    }

    /// An arrow's `:scroll_by` adds to the run's target, not to the drawn offset, so clicks mid-run
    /// add up, and two in one turn both count.
    #[test]
    fn repeated_scroll_by_mid_run_adds_to_the_target() {
        let (mut client, _signal, _dir) = scrolled_row(r#"{ scroll = { duration = 100, easing = "linear" } }"#);
        client.loader.lua().load("s:scroll_by(50)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        let t0 = std::time::Instant::now();
        let ms = |n| t0 + std::time::Duration::from_millis(n);
        assert!(frame(&mut client, ms(50)).1.0 < 50.0, "mid-run");
        client.loader.lua().load("s:scroll_by(50)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(frame(&mut client, ms(1000)).1, (100.0, 100.0));
        client.loader.lua().load("s:scroll_by(30) s:scroll_by(30)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!(frame(&mut client, ms(2000)).1, (160.0, 160.0));
    }

    /// The wheel clamps against the room the last layout measured, so a getter reads the used offset
    /// in one evaluation; the follow-up pass is left for content that resized in the same turn.
    #[test]
    fn a_wheel_past_an_end_evaluates_its_reader_once() {
        let (mut client, signal, _dir) = scrolled_row("{}");
        let before = runs(&client);
        client.wheel(&signal, 900.0, false, std::time::Instant::now());
        assert!(client.re_resolve_if_dirty());
        assert_eq!((runs(&client) - before, shown(&client)), (1, (200.0, 200.0)));

        let before = runs(&client);
        client.wheel(&signal, -50.0, false, std::time::Instant::now());
        client.loader.lua().load("n:set(4)").exec().unwrap();
        assert!(client.re_resolve_if_dirty());
        assert_eq!((runs(&client) - before, shown(&client)), (2, (100.0, 100.0)), "four tiles leave 100 px");
    }
}
