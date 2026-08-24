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
  femtovg on EGL is the locked GPU target; CPU SHM is the bootstrap path.
- Keep product widgets in Lua when existing primitives and capabilities suffice.
  TextField/TextArea are the documented engine exception.
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
  after `prepare` succeeds. The trait's paint abstraction covers buffer
  submission only; backend choice per step 6.
- [ ] 6. Add the SCTK layer-shell backend with CPU SHM paint. Perform the
  null-buffer commit and configure handshake before `prepare` succeeds.
- [ ] 7. Add the femtovg/EGL paint backend behind the renderer trait: GL
  context per surface, glyph atlas via cosmic-text integration, damage-driven
  redraw. Same `prepare`/readiness contract as SHM. SHM stays as llvmpipe/CI
  fallback.
- [ ] 8. Add candidate timeout and process-group cleanup. Test a child that
  ignores `SIGTERM`.
- [ ] 9. Add a file watcher with 250 ms debounce. Test event coalescing and
  config-path watch-root selection.

## Phase 2: safe Lua configuration

- [ ] 10. Embed vendored Lua 5.4 in each renderer generation. Expose only the
  approved standard libraries.
- [ ] 11. Add the rooted `require` loader. Reject traversal, symlink escape,
  unsupported files, and source over the limit.
- [ ] 12. Record dependency paths and content hashes. Recheck them before the
  generation can commit.
- [ ] 13. Define typed constructors for a root panel, text, row, and column.
  Reject unknown properties at construction time. Every property accepts a
  value or a signal binding from day one; the diff machinery in phase 5
  consumes them.
- [ ] 14. Add a built-in Rust diagnostic scene for invalid configuration.
  Configuration errors must keep the active generation.
- [ ] 15. Add instruction, heap, source, node, binding, timer, and callback
  limits. Test syntax errors, missing modules, limits, and infinite loops.

## Phase 3: retained scene

- [ ] 16. Add explicit string signals and direct components. Refresh only dirty
  component subtrees. Keep builders side-effect-free during construction.
- [ ] 17. Add the persist registry: components declare persistent values by
  name, supervisor copies them old-VM to new-VM during staging before the new
  scene builds. Bounded serializable types only; drop-or-default on mismatch.
- [ ] 18. Add bounded keyed repeaters. Reorders retain matched nodes. Removed
  keys unmount child-first. Failed refreshes retain the previous subtree.
- [ ] 19. Add intrinsic row and column layout for bounded text and panel nodes.
  Reject oversized descriptor trees before retention or paint.
- [ ] 20. Add host-font Unicode shaping and path-only PNG/JPEG assets. Bound
  source bytes, dimensions, decoded pixels, and the generation cache.
- [ ] 21. Add root-level pointer and keyboard callbacks with bounded
  value-only events routed to the surface root. Gate keyboard input on active,
  authorized, unfrozen state. Per-node dispatch waits for step 33.
- [ ] 22. Add native one-shot timers and `process.run`. Bound child count,
  capture bytes, timeout, and cleanup.

## Phase 4: general renderer

- [ ] 23. Define bounded output topology and one owned surface entry per output.
  Reject duplicate output targets.
- [ ] 24. Drive buffer size from configure dimensions, integer scale, and
  transform. Recreate released SHM storage safely on resize.
- [ ] 25. Reconcile output hotplug. Gate a new output's input and redraw on its
  own configure and first frame.
- [ ] 26. Add popup surfaces as the second surface kind: xdg-popup children of
  a layer surface, with grab semantics, dismiss rules, placement strategies,
  and independent configure/input/cleanup/handoff rules. Overlay layer surfaces
  are a later slice if a fixture needs one.
- [ ] 27. Finish per-surface frame scheduling. Test removal while a frame
  callback is pending.
- [ ] 28. Run a real multi-output compositor check and record the supported
  protocol matrix.

## Phase 5: retained UI engine

- [ ] 29. Assign stable Rust-owned node IDs. Define their lifetime across a
  same-generation diff and a reload.
- [ ] 30. Replace positional refresh with transactional keyed reconciliation
  where keyed children require it. Preserve unchanged nodes and event IDs.
- [ ] 31. Add lifecycle cleanup for nodes, timers, subscriptions, animations,
  and capability leases. Test child-before-parent unmount.
- [ ] 32. Add explicit sizing, alignment, clipping, and bounded text measurement.
  Reconsider a layout crate only after a measured need.
- [ ] 33. Add generic per-node hit testing over retained node rects, focus
  leases, capture policy, callback-error ordering, and semantic event dispatch
  to Lua handlers. This refines step 21's root routing into node-targeted
  events.
- [ ] 34. Add bounded text editing, cursor, selection, and IME boundaries only
  after generic input routing has a focused test.
- [ ] 35. Add native animation scheduling: engine-clocked tweens with retarget,
  completion callback, and Behavior-style declarations. A diff must not
  duplicate a live animation or timer. Animated writes go through the same
  dirty-marking path as bindings; last writer wins per tick.
- [ ] 36. Add a second Lua shell fixture with a different topology and input
  structure. It must use only the public API.
- [ ] 37. Add one headless-Wayland boot smoke to CI: start under niri's
  headless mode (fallback: cage on wlroots headless), draw one frame via SHM,
  reload once, exit clean. Run with llvmpipe; do not require GPU in CI. Assert
  on protocol events and exit codes, not pixels.

## Phase 6: capability slices

- [ ] 38. Define one versioned capability envelope with bounded snapshots,
  revisions, availability state, validated commands, and feature detection
  (`capability:has("feature")`).
- [ ] 39. Use MPRIS as the first service fixture. Keep its backend owner off Lua.
  Test startup, disconnect, stale revision, and shutdown.
- [ ] 40. Add the notification snapshot bridge. Keep staged renderers feed-gated
  until visible-ID presentation and routing ownership are verified.
- [ ] 41. Add notification commands and public D-Bus ownership only after the
  authority boundary has a test.
- [ ] 42. Add the compositor adapter seam: Niri and Hyprland adapters behind
  capability signals (monitors, workspaces, keyboard layout), generic Wayland
  protocols as the floor, feature detection for compositor-specific extras
  like special workspaces. Session actions use logind directly, outside the
  seam.
- [ ] 43. Port power, network, Bluetooth, audio, workspaces, and clipboard one
  at a time. Each service needs a Rust owner, bounded state, command validation,
  unavailable state, Lua API, and focused test. Workspaces go through the step
  42 adapters.
- [ ] 44. Run a real-session check for each service. Record disconnect, reload,
  stale-revision, and backend-failure behavior.

## Phase 7: test fixture shell

- [ ] 45. Add the supervisor IPC socket: built-in verbs (reload, status,
  rollback, shutdown) plus generic `on_ipc` pass-through to the active
  generation. Socket at `$XDG_RUNTIME_DIR/oblisk.sock`, same-user only,
  bounded messages.
- [ ] 46. Add a Lua bar and launcher fixture using existing layout, input,
  process, and capability primitives. It must use only the public API.
- [ ] 47. Add notification center, OSD, control center, and widget components
  in Lua. Add Rust only for a missing reusable primitive or capability.
- [ ] 48. Exercise reload, hotplug, capability loss, persist carry-over, and
  cleanup with the fixture.
- [ ] 49. Add the tray capability: SNI watcher/host in the supervisor process
  (not a generation), icon decoding, menu data objects. Lua builds the drawer
  UI.
- [ ] 50. Add the supervisor error banner and `reload-rejected` events to
  `on_ipc` (step 45 provides the channel).

## Phase 8: security and durable facilities

- [ ] 51. Define the lock/auth process boundary and supported compositor matrix.
  Test with a disposable PAM account.
- [ ] 52. Add greetd and Polkit conversations without passing secrets to Lua.
- [ ] 53. Add detached jobs, public IPC, and broker extraction only when a
  lifetime or authority requirement exists. Test authentication and revocation.
- [ ] 54. Add theming: palette capability with fixed role vocabulary, wallpaper
  derivation, template stamping on the durable side. Port Noctalia's MIT app
  templates as reference.
- [ ] 55. Add icons, assets, canvas, shaders, accessibility, compositor adapters
  beyond Niri/Hyprland, packaging, and generated LuaLS definitions as separate
  slices.
- [ ] 56. Add supervised external capabilities only through the bounded IPC
  contract and explicit manifest.
- [ ] 57. Run reload-memory, compositor, service-fault, and lock recovery
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
