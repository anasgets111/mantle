# Good and Bad Tests

## Good Tests

**Integration-style**: Test through real module interfaces, not mocks of internal parts.

```rust
// GOOD: Tests observable reload behavior
#[test]
fn atomic_reload_keeps_prior_scene_when_any_surface_fails() {
    let mut scene = Scene::default();
    scene.apply_locked(&valid_surfaces(), &instances, &shaping, &lua, false, true).unwrap();

    let err = scene.apply_locked(&broken_surfaces(), &instances, &shaping, &lua, false, true);
    assert!(err.is_err());
    assert_eq!(scene.surface_ids(), ["bar@eDP-1"]);
}
```

Characteristics:

- Tests behavior callers care about
- Uses the public interface only
- Survives internal refactors
- Describes WHAT, not HOW
- One logical assertion per test

## Bad Tests

**Implementation-detail tests**: Coupled to internal structure.

```rust
// BAD: Tests an internal call instead of the reload contract
#[test]
fn reevaluate_calls_loader_drop_package_loaded() {
    let mock_loader = mock_loader();
    reevaluate(&mock_loader);
    assert!(mock_loader.dropped_package_loaded());
}
```

Red flags:

- Mocking internal collaborators
- Testing private methods
- Asserting on call counts or call sequence
- Test breaks when refactoring without behavior change
- Test name describes HOW not WHAT
- Verifying through external means instead of interface

```rust
// BAD: Bypasses the dependency-snapshot interface to inspect storage
#[test]
fn snapshot_opens_shell_lua() {
    capture_snapshot("shell.lua");
    let internal_files = snapshot_store.opened_files();
    assert!(internal_files.iter().any(|file| file.path() == "shell.lua"));
}

// GOOD: Verifies the public snapshot result
#[test]
fn snapshot_returns_opened_file_identity() {
    let snapshot = capture_snapshot("shell.lua");
    assert_eq!(snapshot.files()[0].path(), "shell.lua");
}
```

**Tautological tests**: Expected value restates the implementation, so the test passes by construction.

```rust
// BAD: Expected events are rebuilt from the implementation's own output
#[test]
fn reload_emits_expected_events() {
    let events = run_reload_trace();
    let expected = events.clone();
    assert_eq!(events, expected);
}

// GOOD: Expected order comes from the ADR-0216 reload contract
#[test]
fn topology_reload_applies_scene_before_updating_protocol_objects() {
    assert_eq!(
        run_reload_trace(),
        ["reconcile_instances", "apply_scene", "destroy_removed", "create_added"]
    );
}
```
