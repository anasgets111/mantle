# Good and Bad Tests

## Good Tests

**Integration-style**: Test through real module interfaces, not mocks of internal parts.

```rust
// GOOD: Tests observable reload behavior
#[test]
fn candidate_needs_presentation_evidence_from_every_targeted_output() {
    let mut transaction = ReloadTransaction::new(configured_outputs());
    transaction.accept(candidate_ready());
    transaction.presented(output_a());
    assert_eq!(transaction.authority(), Authority::Authoritative(old_generation()));

    transaction.presented(output_b());
    assert_eq!(transaction.authority(), Authority::Authoritative(candidate_generation()));
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
// BAD: Tests an internal call instead of the capability contract
#[test]
fn reload_calls_supervisor_activate_candidate() {
    let mock_supervisor = mock_supervisor();
    reload_candidate(mock_supervisor);
    assert!(mock_supervisor.activate_candidate_called());
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

// GOOD: Expected trace comes from the reload contract
#[test]
fn reload_keeps_old_authority_until_presentation() {
    assert_eq!(run_reload_trace(), ["stage", "present", "activate", "freeze"]);
}
```
