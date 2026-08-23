# Build steps

This is the restart checklist for [plan.md](/mnt/Work/0Coding/1Rust/oblisk-shell/plan.md).
All boxes start unchecked. Check a step only after its code, focused test, and
required platform check exist.

## Rules

- Finish one step before starting the next.
- Keep each step small enough to compile or to have one explicit manual check.
- Keep Lua off the platform hot path.
- Keep Wayland and service ownership in Rust.
- Keep durable state outside renderer generations.
- Keep the renderer behind a small trait so CPU and GPU paths can diverge later.
- Keep product widgets in Lua when existing primitives and capabilities suffice.
- Add a capability only with a bounded snapshot, validated command path,
  cleanup owner, and focused runnable check.
- Delete abstractions that have no caller.

## Phase 1: disposable renderer

- [ ] 1. Create the Cargo workspace with supervisor, renderer, and control
  crates. Spawn one headless renderer, receive versioned readiness, terminate
  it, and reap it.
- [ ] 2. Define the length-bounded private control protocol. Test valid frames,
  malformed frames, wrong generations, and oversized frames.
- [ ] 3. Give the supervisor generation IDs and child records. Reap every child
  on every return path.
- [ ] 4. Add staging. Keep one active renderer while one candidate starts. New
  reload requests replace the pending request.
- [ ] 5. Define the renderer trait and a headless backend. Emit readiness only
  after `prepare` succeeds.
- [ ] 6. Add the SCTK layer-shell backend. Perform the null-buffer commit and
  configure handshake before `prepare` succeeds.
- [ ] 7. Add candidate timeout and process-group cleanup. Test a child that
  ignores `SIGTERM`.
- [ ] 8. Add a file watcher with 250 ms debounce. Test event coalescing and
  config-path watch-root selection.

## Phase 2: safe Lua configuration

- [ ] 9. Embed vendored Lua 5.4 in each renderer generation. Expose only the
  approved standard libraries.
- [ ] 10. Add the rooted `require` loader. Reject traversal, symlink escape,
  unsupported files, and source over the limit.
- [ ] 11. Record dependency paths and content hashes. Recheck them before the
  generation can commit.
- [ ] 12. Define typed constructors for a root panel, text, row, and column.
  Reject unknown properties at construction time.
- [ ] 13. Add a built-in Rust diagnostic scene for invalid configuration.
  Configuration errors must keep the active generation.
- [ ] 14. Add instruction, heap, source, node, binding, timer, and callback
  limits. Test syntax errors, missing modules, limits, and infinite loops.

## Phase 3: retained scene

- [ ] 15. Add explicit string signals and direct components. Refresh only dirty
  component subtrees. Keep builders side-effect-free during construction.
- [ ] 16. Add bounded keyed repeaters. Reorders retain matched nodes. Removed
  keys unmount child-first. Failed refreshes retain the previous subtree.
- [ ] 17. Add intrinsic row and column layout for bounded text and panel nodes.
  Reject oversized descriptor trees before retention or paint.
- [ ] 18. Add host-font Unicode shaping and path-only PNG/JPEG assets. Bound
  source bytes, dimensions, decoded pixels, and the generation cache.
- [ ] 19. Add root pointer and keyboard callbacks with bounded value-only
  events. Gate keyboard input on active, authorized, unfrozen state.
- [ ] 20. Add native one-shot timers and `process.run`. Bound child count,
  capture bytes, timeout, and cleanup.

## Phase 4: general renderer

- [ ] 21. Define bounded output topology and one owned surface entry per output.
  Reject duplicate output targets.
- [ ] 22. Drive buffer size from configure dimensions, integer scale, and
  transform. Recreate released SHM storage safely on resize.
- [ ] 23. Reconcile output hotplug. Gate a new output's input and redraw on its
  own configure and first frame.
- [ ] 24. Add a second surface kind, such as popup or overlay. Define its
  configure, input, cleanup, and handoff rules.
- [ ] 25. Finish per-surface frame scheduling. Test removal while a frame
  callback is pending.
- [ ] 26. Run a real multi-output compositor check and record the supported
  protocol matrix.

## Phase 5: retained UI engine

- [ ] 27. Assign stable Rust-owned node IDs. Define their lifetime across a
  same-generation diff and a reload.
- [ ] 28. Replace positional refresh with transactional keyed reconciliation
  where keyed children require it. Preserve unchanged nodes and event IDs.
- [ ] 29. Add lifecycle cleanup for nodes, timers, subscriptions, animations,
  and capability leases. Test child-before-parent unmount.
- [ ] 30. Add explicit sizing, alignment, clipping, and bounded text measurement.
  Reconsider a layout crate only after a measured need.
- [ ] 31. Add generic hit testing, focus, capture policy, and callback-error
  ordering for retained nodes.
- [ ] 32. Add bounded text editing, cursor, selection, and IME boundaries only
  after generic input routing has a focused test.
- [ ] 33. Add native animation scheduling. A diff must not duplicate a live
  animation or timer.
- [ ] 34. Add a second Lua shell fixture with a different topology and input
  structure. It must use only the public API.

## Phase 6: capability slices

- [ ] 35. Define one versioned capability envelope with bounded snapshots,
  revisions, availability state, and validated commands.
- [ ] 36. Use MPRIS as the first service fixture. Keep its backend owner off Lua.
  Test startup, disconnect, stale revision, and shutdown.
- [ ] 37. Add the notification snapshot bridge. Keep staged renderers feed-gated
  until visible-ID presentation and routing ownership are verified.
- [ ] 38. Add notification commands and public D-Bus ownership only after the
  authority boundary has a test.
- [ ] 39. Port power, network, Bluetooth, audio, workspaces, and clipboard one
  at a time. Each service needs a Rust owner, bounded state, command validation,
  unavailable state, Lua API, and focused test.
- [ ] 40. Run a real-session check for each service. Record disconnect, reload,
  stale-revision, and backend-failure behavior.

## Phase 7: default shell

- [ ] 41. Add a default Lua bar and launcher using existing layout, input,
  process, and capability primitives.
- [ ] 42. Add notification center, OSD, control center, and widget components in
  Lua. Add Rust only for a missing reusable primitive or capability.
- [ ] 43. Exercise reload, hotplug, capability loss, and cleanup with the
  default shell and the second-shell fixture.

## Phase 8: security and durable facilities

- [ ] 44. Define the lock/auth process boundary and supported compositor matrix.
  Test with a disposable PAM account.
- [ ] 45. Add greetd and Polkit conversations without passing secrets to Lua.
- [ ] 46. Add detached jobs, public IPC, and broker extraction only when a
  lifetime or authority requirement exists. Test authentication and revocation.

## Phase 9: productization

- [ ] 47. Add themes, icons, assets, canvas, shaders, accessibility, compositor
  adapters, packaging, and generated LuaLS definitions as separate slices.
- [ ] 48. Add supervised external capabilities only through the bounded IPC
  contract and explicit manifest.
- [ ] 49. Run reload-memory, compositor, service-fault, and lock recovery
  qualification. Remove unused dependencies and stale alternatives.

## Acceptance gate

A step needs:

- one owner for every resource and process;
- byte and count limits at every boundary;
- deterministic cleanup;
- one focused runnable test;
- a manual compositor or real-session check when required.

Do not mark a product widget as framework work. Do not add a dependency because
the plan mentions it. Add it when code needs it and a test can exercise it.
