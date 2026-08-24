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
  femtovg on EGL is the locked GPU target and defines the trait. CPU SHM is its
  test adapter (llvmpipe/CI), not a second maintained implementation.
- Keep product widgets in Lua when existing primitives and capabilities suffice.
  TextField/TextArea are the documented engine exception.
- Add a capability only with a bounded snapshot, validated command path,
  cleanup owner, and focused runnable check.
- Delete abstractions that have no caller.

## Phase 1: disposable renderer

- [ ] 1. Create the Cargo workspace with supervisor and renderer crates (the
  private protocol lives in the supervisor). Spawn one headless renderer,
  receive versioned readiness, terminate it, and reap it.
- [ ] 2. Define the length-bounded private control protocol. Test valid frames,
  malformed frames, wrong generations, and oversized frames.
- [ ] 3. Give the supervisor generation IDs and child records. Reap every child
  on every return path.
- [ ] 4. Add staging. Keep one active renderer while one candidate starts. New
  reload requests replace the pending request.
- [ ] 5. Define the renderer trait and a headless backend. Emit readiness only
  after `prepare` succeeds. The trait's paint abstraction covers buffer
  submission only; backend choice per step 6.
- [ ] 6. Add the SCTK layer-shell backend with CPU SHM paint as the renderer
  trait's test adapter. Perform the null-buffer commit and configure handshake
  before `prepare` succeeds.
- [ ] 7. Add the femtovg/EGL paint backend as the definition of the renderer
  trait: GL context per surface, glyph atlas via cosmic-text integration,
  damage-driven redraw. Same `prepare`/readiness contract as SHM. The SHM path
  is the trait's test adapter, exercised in CI via llvmpipe.
- [ ] 8. Add candidate timeout and process-group cleanup. Test a child that
  ignores `SIGTERM`.
- [ ] 9. Add a file watcher with 250 ms debounce. Test event coalescing and
  config-path watch-root selection. Open each file before hashing: check the
  opened fd's inode (`fstat` + hash), never the path twice. Treat rename-based
  saves as fresh triggers regardless of debounce coalescing.

## Phase 2: safe Lua configuration

- [ ] 10. Embed vendored Lua 5.4 in each renderer generation. Expose only the
  approved standard libraries.
- [ ] 11. Add the rooted `require` loader. Reject traversal, symlink escape,
  unsupported files, and source over the limit.
- [ ] 12. Record dependency paths and content hashes. Recheck them before the
  generation can commit.
- [ ] 13. Define the descriptor core: `node(kind, props)` with `children` as a
  prop, `bind(signal)` sentinel, and `list(keyfn, sig, itemfn)` keyed repeater.
  Reject unknown properties at construction time. Generate sugar constructors
  (panel, row, column, text, icon, button) from the Rust schema; one source of
  truth. Reserve the `raw` kind name. The scene root declares an API version;
  reject unsupported versions before creating surfaces.
- [ ] 14. Add a built-in Rust diagnostic scene for invalid configuration.
  Configuration errors must keep the active generation. This diagnostic is also
  the safe shell: on active-renderer crash after bounded retries, the
  supervisor runs it directly without executing user Lua.
- [ ] 15. Add instruction, heap, source, node, binding, timer, and callback
  limits. Test syntax errors, missing modules, limits, and infinite loops.
  Scale node and binding caps by detected output count; count repeater children
  against a separate budget from static nodes; name the exceeded limit and its
  current value in the error.

## Phase 3: retained scene

- [ ] 16. Add explicit string signals, direct components, and
  `computed(dependencies, fn)` with explicit dependencies. Refresh only dirty
  component subtrees. Keep builders side-effect-free during construction.
  Resolve computed chains to fixed point per tick with a depth cap of 8;
  exceeding it marks the chain errored and drops the write, it does not disable
  the source signal.
- [ ] 17. Add the persist registry: components declare persistent values by
  name, supervisor copies them old-VM to new-VM during staging before the new
  scene builds. Bounded serializable types only; drop-or-default on mismatch.
  Copy semantics are last-committed-tick: the supervisor snapshots at quiesce,
  not mid-callback; document that a callback racing reload may lose its write.
- [ ] 18. Add bounded keyed repeaters. Reorders retain matched nodes. Removed
  keys unmount child-first. Failed refreshes retain the previous subtree.
- [ ] 19. Add intrinsic row and column layout for bounded text and panel nodes.
  Reject oversized descriptor trees before retention or paint.
- [ ] 20. Add host-font Unicode shaping and path-only PNG/JPEG assets. Bound
  source bytes, dimensions, decoded pixels, and the generation cache.
- [ ] 21. Add root-level pointer and keyboard callbacks with bounded
  value-only events routed to the surface root through one bounded FIFO. Focus
  drops on freeze or seat removal. Gate keyboard input on active, authorized,
  unfrozen state. Per-node dispatch waits for step 33.
- [ ] 22. Add native one-shot timers and `process.run`. Bound child count,
  capture bytes, timeout, and cleanup.

## Phase 4: general renderer

- [ ] 23. Define bounded output topology and one owned surface entry per output.
  Reject duplicate output targets.
- [ ] 24. Drive buffer size from configure dimensions, integer scale, and
  transform. Recreate released SHM storage safely on resize.
- [ ] 25. Reconcile output hotplug. Gate a new output's input and redraw on its
  own configure and first frame.
- [ ] 26. Add popup surfaces as the second surface kind, deferred from phase 4
  to here because phase 7's launcher and notification center are their first
  callers: xdg-popup children of a layer surface, with grab semantics, dismiss
  rules, placement strategies, and independent configure/input/cleanup/handoff
  rules. Overlay layer surfaces are a later slice if a fixture needs one.
- [ ] 27. Finish per-surface frame scheduling. Test removal while a frame
  callback is pending. Add presentation-feedback handling: activation ACK does
  not prove presentation; the first frame or presentation feedback arms the
  health window.
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
- [ ] 34. Add bounded text editing, cursor, selection, clipboard via
  `data-control`, and IME boundaries only after generic input routing has a
  focused test.
- [ ] 35. Add native animation scheduling: engine-clocked tweens with retarget
  (hover/unhover mid-flight happens on day one) and completion callback.
  Behavior-style declarations are deferred until a fixture demands them. A
  diff must not duplicate a live animation or timer. Animated writes go through
  the same dirty-marking path as bindings; last writer wins per tick.
- [ ] 36. Add a second Lua shell fixture with a different topology and input
  structure. It must use only the public API.
- [ ] 37. Add one headless-Wayland boot smoke to CI: start under niri's
  headless mode (fallback: cage on wlroots headless), draw one frame via SHM,
  reload once, exit clean. Run with llvmpipe; do not require GPU in CI. Assert
  on protocol events and exit codes, not pixels.

## Phase 6: capability slices

- [ ] 38. Define one versioned capability envelope with bounded snapshots,
  revisions, availability state, and validated commands. Every validated
  command carries the sender's generation ID; the owner rejects any ID that is
  not the active generation, before unmapping the dying generation's surfaces.
  Feature detection (`capability:has("feature")`) lands with the first feature-
  specific consumer (step 42).
- [ ] 39. Use MPRIS as the first service fixture. Keep its backend owner off Lua.
  Test startup, disconnect, stale revision, shutdown, and command rejection for
  a stale generation ID.
- [ ] 40. Add the notification snapshot bridge. Keep staged renderers feed-gated
  until visible-ID presentation and routing ownership are verified.
- [ ] 41. Add notification commands and public D-Bus ownership only after the
  authority boundary has a test.
- [ ] 42. Add the compositor adapter seam: Niri and Hyprland adapters behind
  capability signals (monitors, workspaces, keyboard layout), generic Wayland
  protocols as the floor. Two adapters now is what makes the seam real; defer
  only `has()` until a config consumes a compositor-specific feature like
  special workspaces. Session actions use logind directly, outside the seam.
- [ ] 43. Port services one at a time through the full pipeline: power,
  network, Bluetooth, audio, workspaces, clipboard. Each is its own step-sized
  slice: Rust owner, bounded state, command validation, unavailable state, Lua
  API, focused test. Workspaces go through the step 42 adapters.
- [ ] 44. Run a real-session check for each service. Record disconnect, reload,
  stale-revision, and backend-failure behavior.

## Phase 7: test fixture shell

- [ ] 45. Add the supervisor IPC socket: built-in verbs (reload, status,
  reload-last-good, shutdown) plus generic `on_ipc` pass-through to the active
  generation. Socket at `$XDG_RUNTIME_DIR/oblisk.sock`, same-user only.
  Attach the peer PID via SO_PEERCRED; pass-through verbs are default-deny and
  configs opt in per verb. Message shape: verb is a non-empty string up to 64
  bytes, args a flat table up to 16 entries with string/number/bool values;
  supervisor validates before delivery.
- [ ] 46. Add a Lua bar and launcher fixture using existing layout, input,
  process, and capability primitives. It must use only the public API. Treat
  fixture friction as API bugs, not fixture bugs.
- [ ] 47. Add notification center, OSD, control center, and widget components
  in Lua. Add Rust only for a missing reusable primitive or capability.
- [ ] 48. Exercise reload, hotplug, capability loss, persist carry-over, and
  cleanup with the fixture.
- [ ] 49. Add the tray capability: SNI watcher/host in the supervisor process
  (not a generation), icon decoding, menu data objects. Menu trees cross to Lua
  as revisioned snapshots through the capability envelope. Icon decode applies
  the same byte/dimension/pixel bounds as config assets, runs off-thread with a
  hard deadline, and drops to unavailable state on violation; a decompression
  bomb must never take down the watcher. Lua builds the drawer UI.
- [ ] 50. Add the supervisor error banner surface itself (Rust-rendered,
  supervisor-owned lifetime) and wire all three sources: rejected candidates,
  rate-limited callback failures, capability hard-failures. Auto-clear on clean
  activation. Route `reload-rejected` events to `on_ipc` (step 45 provides the
  channel).

## Phase 8: security and durable facilities

- [ ] 51. Define the lock/auth process boundary and supported compositor matrix.
  Test with a disposable PAM account. Session-lock + PAM only; greetd and
  Polkit conversations are deferred behind an actual greeter or polkit-agent
  deliverable.
- [ ] 52. Add detached jobs, public IPC, and broker extraction only when a
  lifetime or authority requirement exists. Test authentication and revocation.
- [ ] 53. Add theming when the first themed widget exists: palette capability
  with fixed role vocabulary, then wallpaper derivation. Template stamping into
  other apps' configs is deferred until a real consumer asks; Noctalia's MIT
  templates remain reference material.
- [ ] 54. Add icons, assets, canvas, shaders, accessibility, compositor adapters
  beyond Niri/Hyprland, packaging, and generated LuaLS definitions as separate
  slices, each with an explicit trigger (e.g. accessibility: first AT-SPI
  consumer).
- [ ] 55. Run reload-memory, compositor, service-fault, and lock recovery
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
