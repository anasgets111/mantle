//! `reset_on_close` (ADR-0289): once per turn, after surfaces are shown and hidden, each declared
//! surface that stopped being shown writes back what it lists.

use std::collections::HashMap;

use super::RendererClient;
use crate::lua;
use crate::lua::nodes::properties::closable;

impl RendererClient {
    /// `shown` is every instance with a role object this turn. A declared surface shown last call
    /// and not now resets its list; one instance left on any output keeps it shown, and a surface
    /// destroyed and rebuilt within the turn never left the set.
    pub fn reset_closed_surfaces(&mut self, shown: &[&str]) {
        let mut now: HashMap<String, Vec<lua::signal::Signal>> = HashMap::new();
        for instance in self.instances.iter().filter(|instance| shown.contains(&instance.instance_id.as_str())) {
            let list = match self.scene.surface(&instance.instance_id) {
                Some(tree) => closable::reset_on_close.read(&tree.properties).unwrap_or_default(),
                // No tree after a failed apply: keep what the evaluation that showed it listed.
                None => self.shown_resets.get(&instance.declared_id).cloned().unwrap_or_default(),
            };
            now.entry(instance.declared_id.clone()).or_insert(list);
        }
        let closed = std::mem::replace(&mut self.shown_resets, now);
        for (declared_id, list) in closed {
            if self.shown_resets.contains_key(&declared_id) {
                continue;
            }
            for signal in &list {
                lua::signal::reset(self.loader.lua(), signal);
                self.scene.stop_scroll(signal);
                self.owes_pass = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{rescue_state, run_startup, test_client, write_shell_lua};
    use super::super::*;
    use crate::layout::instance::{OutputGeometry, expand_instances};

    const LAUNCHER: &str = r#"
        query = state("query", "")
        results = scroll("results")
        seen = ""
        query:on_change(function(now, before) seen = before .. "->" .. now end)
        return {
            panel { id = "bar", layer = "top", child = text { content = "bar" } },
            panel {
                id = "launcher", layer = "overlay", visible = state("open", true),
                reset_on_close = { query, results }, child = text { content = query },
            },
            popup {
                id = "menu", parent = "bar", anchor_rect = { x = 0, y = 0, width = 1, height = 1 },
                reset_on_close = { query }, child = text { content = "menu" },
            },
        }
    "#;

    fn eval(client: &RendererClient, expr: &str) -> String {
        client.loader.lua().load(format!("return tostring({expr})")).eval().unwrap()
    }

    fn scroll_to(client: &RendererClient, offset: f64) {
        let ud: mlua::AnyUserData = client.loader.lua().globals().get("results").unwrap();
        let handle = lua::signal::from_userdata(&ud).unwrap().scroll_handle().unwrap();
        handle.set(mlua::Value::Number(offset));
    }

    /// A client on `source` with every surface shown once, the query typed and the list scrolled.
    fn started(source: &str) -> (tempfile::TempDir, RendererClient) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), source);
        let (mut client, _rx) = test_client(&path);
        assert!(run_startup(&mut client));
        client.reset_closed_surfaces(&["bar@TEST", "launcher@TEST", "menu"]);
        client.loader.lua().load(r#"query:set("fire")"#).exec().unwrap();
        scroll_to(&client, 40.0);
        lua::signal::run_state_handlers(client.loader.lua());
        (dir, client)
    }

    #[test]
    fn a_surface_that_stops_being_shown_resets_what_it_lists_as_an_ordinary_write() {
        let (_dir, mut client) = started(LAUNCHER);
        assert_eq!(eval(&client, "seen"), "->fire");

        client.reset_closed_surfaces(&["bar@TEST", "launcher@TEST", "menu"]);
        assert_eq!(eval(&client, "query:get()"), "fire", "still shown: nothing resets");

        client.reset_closed_surfaces(&["bar@TEST", "menu"]);
        assert_eq!(eval(&client, "query:get()"), "", "back to the declared initial");
        assert_eq!(eval(&client, "results:get()"), "0.0", "a scroll goes back to the top");
        assert!(client.next_wake_deadline().is_some(), "the reset owes a pass before the loop sleeps");
        assert!(client.re_resolve_if_dirty());
        assert_eq!(eval(&client, "seen"), "fire->", "the reset ran the state's on_change");
        assert!(client.next_wake_deadline().is_none());
    }

    #[test]
    fn a_scroll_request_pending_at_close_does_not_outlive_the_surface() {
        let (_dir, mut client) = started(LAUNCHER);
        client.loader.lua().load("results:scroll_by(30)").exec().unwrap();
        client.reset_closed_surfaces(&["bar@TEST", "menu"]);
        let ud: mlua::AnyUserData = client.loader.lua().globals().get("results").unwrap();
        assert!(lua::signal::from_userdata(&ud).unwrap().pending_scroll().is_none());
    }

    #[test]
    fn a_popup_closing_with_its_parent_resets_what_it_lists() {
        let (_dir, mut client) = started(LAUNCHER);
        client.reset_closed_surfaces(&["launcher@TEST"]);
        assert_eq!(eval(&client, "query:get()"), "");
    }

    #[test]
    fn a_surface_never_shown_resets_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_shell_lua(dir.path(), LAUNCHER);
        let (mut client, _rx) = test_client(&path);
        assert!(run_startup(&mut client));
        client.reset_closed_surfaces(&["bar@TEST"]);
        client.loader.lua().load(r#"query:set("fire")"#).exec().unwrap();
        client.reset_closed_surfaces(&["bar@TEST"]);
        assert_eq!(eval(&client, "query:get()"), "fire");
    }

    /// Reloads `path` with `source` the way `App::apply_pending` does, keeping the one output.
    fn reload(client: &mut RendererClient, path: &std::path::Path, source: &str) -> bool {
        std::fs::write(path, source).unwrap();
        if !client.reevaluate() {
            return false;
        }
        let (specs, _) = client.pending_surfaces().unwrap();
        let outputs =
            [OutputGeometry { name: "TEST".into(), size: layout::LogicalSize { width: 1920.0, height: 1080.0 } }];
        client.set_instances(expand_instances(&specs, &outputs));
        client.handle_apply_pending()
    }

    #[test]
    fn a_reload_that_removes_a_surface_resets_to_the_initial_the_screen_declares() {
        let (dir, mut client) = started(LAUNCHER);
        let path = dir.path().join("shell.lua");
        assert!(!reload(&mut client, &path, "error('half a config')"));
        client.reset_closed_surfaces(&["bar@TEST", "launcher@TEST", "menu"]);
        assert_eq!(eval(&client, "query:get()"), "fire", "a failed reload closes nothing");

        // A changed `layer` rebuilds the surface under the same id: still shown.
        assert!(reload(&mut client, &path, &LAUNCHER.replace(r#""overlay""#, r#""top""#)));
        client.reset_closed_surfaces(&["bar@TEST", "launcher@TEST", "menu"]);
        assert_eq!(eval(&client, "query:get()"), "fire", "a recreated surface never stopped being shown");

        let without = r#"
            query = state("query", "none")
            results = scroll("results")
            return panel { id = "bar", layer = "top", child = text { content = "bar" } }
        "#;
        assert!(reload(&mut client, &path, without));
        client.loader.lua().load(r#"query:set("fire")"#).exec().unwrap();
        client.reset_closed_surfaces(&["bar@TEST"]);
        assert_eq!(eval(&client, "query:get()"), "none", "the removed surface's list, the new evaluation's initial");
    }

    #[test]
    fn a_per_output_surface_resets_when_its_last_instance_closes() {
        let (_dir, mut client) =
            started(&LAUNCHER.replace(r#"layer = "overlay","#, r#"layer = "overlay", output = "all","#));
        let outputs = ["A", "B"].map(|name| OutputGeometry {
            name: name.into(),
            size: layout::LogicalSize { width: 1920.0, height: 1080.0 },
        });
        let specs = client.applied_surface_specs();
        client.set_instances(expand_instances(&specs, &outputs));
        assert!(client.apply_instances());
        client.reset_closed_surfaces(&["bar@A", "bar@B", "launcher@A", "launcher@B", "menu"]);

        client.reset_closed_surfaces(&["bar@A", "bar@B", "launcher@B", "menu"]);
        assert_eq!(eval(&client, "query:get()"), "fire", "one output left still shows it");
        client.reset_closed_surfaces(&["bar@A", "bar@B", "menu"]);
        assert_eq!(eval(&client, "query:get()"), "");
    }

    #[test]
    fn an_entry_that_is_not_a_state_or_scroll_handle_fails_the_evaluation_naming_the_surface() {
        for entry in ["query:map(function(q) return q end)", "\"query\"", "hover(\"h\")"] {
            let dir = tempfile::tempdir().unwrap();
            let source = format!(
                r#"local query = state("query", "")
                return panel {{ id = "launcher", layer = "top", reset_on_close = {{ query, {entry} }} }}"#
            );
            let path = write_shell_lua(dir.path(), &source);
            let (mut client, _rx) = test_client(&path);
            assert!(!run_startup(&mut client), "{entry}");
            let (_, log) = rescue_state(&client.loader);
            assert!(
                log.contains("`launcher`") && log.contains("reset_on_close") && log.contains("entry 2"),
                "{entry}: {log}"
            );
        }
    }
}
