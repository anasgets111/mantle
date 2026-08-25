# Build steps

This is the restart checklist for [plan.md](/mnt/Work/0Coding/1Rust/oblisk-shell/plan.md).
All boxes start unchecked. Check a step only after its code, focused test, and
required platform check exist.

## Rules

- Finish one step before starting the next.
- Keep each step small enough to compile or to have one explicit manual check.
- Keep Lua off the platform hot path.
- Keep Wayland and capability ownership in Rust.
- Keep durable state outside renderer generations.
- Keep one renderer paint trait. SHM is its headless test adapter and femtovg on
  EGL is its production adapter. The headless process fixture has no paint
  implementation.
- Keep product widgets in Lua when existing engine types and capabilities suffice.
  TextField/TextArea are the documented engine exception.
- Add a capability only with a bounded snapshot, validated command path,
  cleanup owner, and focused runnable check.
- Claim singleton system resources (D-Bus names, PolicyKit agent
  registration, background layer surfaces, indexer threads) lazily, on a
  generation's first `require()`, never at Supervisor boot.
- Delete abstractions that have no caller.

## Phase 1: disposable renderer

- [ ] 1. Create the Cargo workspace with supervisor and renderer crates (the
  private protocol lives in the supervisor). Spawn one headless renderer
  process fixture, receive versioned readiness, terminate it, and reap it.
- [ ] 2. Define the length-bounded private control protocol. Test valid frames,
  malformed frames, wrong generations, and oversized frames.
- [ ] 3. Give the supervisor generation IDs and child records. Reap every child
  on every return path.
- [ ] 4. Add the private reload transaction. Feed it reload requests, renderer
  milestones, presentation evidence, process exits, and deadlines. It emits
  staging, activation, freeze, rollback, and cleanup effects. Keep one
  authoritative generation while one candidate starts; a newer request replaces
  the pending request. Test event traces, wrong epochs, stalled presentation,
  rollback, and candidate reaping.
- [ ] 5. Define the renderer prepare/readiness contract without a paint
  backend. The process fixture reports readiness only after `prepare` succeeds.
- [ ] 6. Add the SCTK layer-shell backend with CPU SHM paint as the renderer
  trait's test adapter. Perform the null-buffer commit and configure handshake
  before `prepare` succeeds.
- [ ] 7. Add the femtovg/EGL paint backend as the definition of the renderer
  trait: GL context per surface, glyph atlas via cosmic-text integration,
  damage-driven redraw. Same `prepare`/readiness contract as SHM. The SHM path
  is the headless test adapter, exercised in CI via llvmpipe.
- [ ] 8. Add candidate timeout and process-group cleanup. Test a child that
  ignores `SIGTERM`.
- [ ] 9. Add the shared dependency snapshot module and its 250 ms watcher.
  Resolve the rooted graph, open each file once, check the opened fd's inode
  (`fstat`) and hash, and return bounded watch roots. Before the first
  successful snapshot, watch the configured entry file, trusted include roots,
  and parent directories needed for missing modules. After success, follow only
  the last successful snapshot. Test event coalescing, config-path selection,
  symlink escape, missing-module parent roots, and rename-based saves.

## Phase 2: safe Lua configuration

- [ ] 10. Embed vendored Lua 5.4 in each renderer generation. Expose only the
  approved standard libraries.
- [ ] 11. Connect the rooted Lua `require` loader to the dependency snapshot
  module. Reject traversal, symlink escape, unsupported files, and source over
  the limit.
- [ ] 12. Send the renderer's bounded dependency snapshot to the supervisor.
  Re-capture each expected file through the same module before the generation
  can commit. Compare inode and hash from opened descriptors, never a path-only
  hash. Replace the watch set only after successful activation.
- [ ] 13. Define the descriptor core: `node(kind, props)` with `children` as a
  prop, `bind(signal)` sentinel, and `list(keyfn, sig, itemfn)` keyed repeater.
  Reject unknown properties at construction time. Generate sugar constructors
  (panel, row, column, text, icon, button) from the Rust schema; one source of
  truth. Reserve the `raw` kind name. The scene root declares an interface
  version. Reject unsupported versions before creating surfaces.
- [ ] 14. Add a built-in Rust diagnostic scene for invalid configuration: a
  hardcoded panel, compiled into the renderer binary and independent of the
  Lua VM, that renders the supervisor's error log (syntax error or backtrace)
  and an interactive reload-config action. Configuration errors must keep the
  authoritative generation. This diagnostic is also the safe shell: after
  bounded retries for an authoritative-generation crash, the supervisor runs
  it directly without executing user Lua.
- [ ] 15. Add instruction, heap, source, node, binding, timer, and callback
  limits. Cap Lua execution via `lua_sethook` at 1,000,000 instructions per
  layout transition; wrap every user-layout execution in a protected call so a
  caught error drops the reload and flags `rescue.is_rescue` instead of
  crashing. Test syntax errors, missing modules, limits, and infinite loops.
  Scale node and binding caps by detected output count; count repeater children
  against a separate budget from static nodes; name the exceeded limit and its
  current value in the error.

## Phase 3: retained scene

- [ ] 16. Add the retained-scene transaction with explicit string signals,
  builders, and `computed(dependencies, fn)`. Refresh only dirty builder
  subtrees. Keep builders side-effect-free during construction.
  Resolve computed chains to fixed point per tick with a depth cap of 8;
  exceeding it marks the chain errored and drops the write, it does not disable
  the source signal.
- [ ] 17. Add the persist registry: builders declare persistent values by
  name, supervisor copies them from the old VM to the new VM during staging before the new
  scene builds. Bounded serializable types only; drop-or-default on mismatch.
  Copy semantics are last-committed-tick: the supervisor snapshots at quiesce,
  not mid-callback; document that a callback racing reload may lose its write.
- [ ] 18. Add bounded keyed reconciliation inside the retained-scene
  transaction. Reorders retain matched nodes. Removed keys unmount child-first.
  Failed refreshes retain the previous subtree. Use a bounded linear key scan;
  add an index only after measured list size requires it.
- [ ] 19. Add intrinsic row and column layout for bounded text and panel nodes.
  Reject oversized descriptor trees before retention or paint.
- [ ] 20. Add host-font Unicode shaping and path-only PNG/JPEG assets. Bound
  source bytes, dimensions, decoded pixels, and the generation cache.
- [ ] 21. Add root-level pointer and keyboard callbacks with bounded
  value-only events routed to the surface root through one bounded FIFO. Focus
  drops on freeze or seat removal. Gate keyboard input on the authoritative,
  authorized, unfrozen generation. Per-node dispatch waits for step 33.
- [ ] 22. Add native one-shot timers and `process.run`. Bound child count,
  capture bytes, timeout, and cleanup.

## Phase 4: general renderer

- [ ] 23. Define bounded output topology and one owned surface entry per output.
  Reject duplicate output targets.
- [ ] 24. Drive buffer size from configure dimensions, integer scale, and
  transform. Recreate released SHM storage safely on resize.
- [ ] 25. Reconcile output hotplug. Gate a new output's input and redraw on its
  own configure and first frame.
- [ ] 26. Define and test the popup-surface interface for the second surface
  kind. Defer implementation until phase 7, alongside the launcher and
  notification center callers. Cover xdg-popup children of a layer surface, grab
  semantics, dismiss rules, placement, and independent configure, input,
  cleanup, and handoff rules. Overlay layer surfaces remain deferred until a
  fixture needs one.
- [ ] 27. Finish per-surface frame scheduling. Test removal while a frame
  callback is pending. Add presentation-feedback handling: activation ACK does
  not prove presentation; every targeted output must provide a first frame or
  presentation feedback before the old generation freezes. Untargeted outputs
  do not block handoff, and the health window uses a wall-clock deadline
  independent of frame callbacks.
- [ ] 28. Run a real multi-output compositor check and record the supported
  protocol matrix.

## Phase 5: retained UI engine

- [ ] 29. Finish stable Rust-owned node IDs inside the retained-scene
  transaction. Define their lifetime across a same-generation diff and a
  reload.
- [ ] 30. Replace positional refresh with the transaction's keyed reconciliation.
  Preserve unchanged nodes and event IDs. A failed commit leaves the previous
  scene intact.
- [ ] 31. Route node, timer, subscription, animation, and node-owned capability
  lease cleanup through the retained-scene transaction. Test child-before-parent
  unmount and callback failure.
- [ ] 32. Add explicit sizing, alignment, clipping, and bounded text measurement.
  Reconsider a layout crate only after a measured need.
- [ ] 33. Add retained-scene hit testing over node rects, focus leases, capture
  policy, callback-error ordering, and semantic event dispatch to Lua handlers.
  Input remains an event producer; node identity and cleanup stay in the scene
  transaction. This refines step 21's root routing into node-targeted events.
- [ ] 34. Add bounded text editing, cursor, selection, clipboard via
  `data-control`, and IME handoff only after generic input routing has a
  focused test.
- [ ] 35. Add native animation scheduling: engine-clocked tweens with retarget
  when hover changes mid-flight, and a completion callback.
  Behavior-style declarations are deferred until a fixture demands them. A
  diff must not duplicate a live animation or timer. Animated writes go through
  the same dirty-marking path as bindings; last writer wins per tick.
- [ ] 36. Add a second Lua shell fixture with a different topology and input
  structure. It must use only the public interface.
- [ ] 37. Add one headless-Wayland boot smoke to CI: start under niri's
  headless mode (fallback: cage on wlroots headless), draw one frame via SHM,
  reload once, exit clean. Run with llvmpipe; do not require GPU in CI. Assert
  on protocol events and exit codes, not pixels.

## Phase 6: capability slices

- [ ] 38. Define one capability authority module with bounded snapshots,
  revisions, availability state, generation authorization, stale-command
  rejection, and disconnect revocation. State-dependent commands carry the
  sender's generation ID and expected snapshot revision; state-independent
  commands may omit the revision. Backend adapters validate only their own
  command meaning. Feature detection (`capability:has("feature")`) lands with
  the first feature-specific consumer (step 42).
- [ ] 39. Use MPRIS as the first capability fixture. Its backend adapter is
  Supervisor-owned and durable from the first implementation
  ([ADR 0004](docs/adr/0004-mpris-durable-supervisor-ownership.md)). The
  Supervisor discovers `org.mpris.MediaPlayer2.*` names and caches normalized
  player state outside any generation, pushing the cached snapshot immediately
  on `RegisterCapability`. Test startup, disconnect, stale revision, shutdown,
  reload-continuity (no reconnect, correct state on first frame), and command
  rejection for stale generation IDs and revisions through the authority
  module.
- [ ] 40. Add the notification snapshot bridge. Keep staged renderers feed-gated
  until visible-ID presentation and routing ownership are verified.
- [ ] 41. Add notification commands and public D-Bus ownership. Claim
  `org.freedesktop.Notifications` on the generation's first
  `require("oblisk.notifications")`, per the lazy-claim rule above, so a shell
  that skips it leaves dunst/mako running. Land it only after the capability
  authority seam has a test. Do not add a broker to the interface.
- [ ] 42. Add the compositor adapter seam with Niri and Hyprland adapters behind
  capability signals for monitors, workspaces, and keyboard layout. Generic
  Wayland protocols are the floor. Keep `has()` until a config consumes a
  compositor-specific feature such as special workspaces. Session actions use
  logind directly, outside the seam.
- [ ] 43. Port power, network, Bluetooth, audio, workspaces, and clipboard one
  capability at a time. Each slice needs a Rust owner, bounded state, command
  validation, unavailable state, Lua interface, and focused test. Workspaces go
  through the step 42 adapters.
- [ ] 44. Run a real-session check for each capability. Record disconnect, reload,
  stale-revision, and backend-failure behavior.

## Phase 7: test fixture shell

- [ ] 45. Add the supervisor IPC socket: built-in verbs (reload, status,
  reload-last-good, shutdown) plus generic `on_ipc` pass-through to the
  authoritative generation. Socket at `$XDG_RUNTIME_DIR/oblisk.sock`,
  same-user only.
  Attach the peer PID via SO_PEERCRED; pass-through verbs are default-deny and
  configs opt in per verb. Message shape: verb is a non-empty string up to 64
  bytes, args a flat table up to 16 entries with string/number/bool values;
  supervisor validates before delivery.
- [ ] 46. Implement popup surfaces, then add a Lua bar and launcher fixture
  using existing layout, input, process, and capability interfaces. It must use
  only the public interface.
  Treat fixture friction as interface bugs, not fixture bugs.
- [ ] 47. Add notification center, OSD, control center, and widget builders
  in Lua. Add Rust only for a missing reusable engine type or capability.
- [ ] 48. Exercise reload, hotplug, capability loss, persist carry-over, and
  cleanup with the fixture.
- [ ] 49. Add the tray capability: SNI watcher/host in the supervisor process
  (not a generation), icon decoding, menu data objects. Claim
  `org.kde.StatusNotifierWatcher` on the first `require("oblisk.tray")`, per
  the lazy-claim rule above. Menu trees cross to Lua
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

- [ ] 51. Define the lock/auth process seam and supported compositor matrix.
  Test with a disposable PAM account. Session-lock + PAM only, in the
  separate lock process; greetd is deferred behind an actual greeter
  deliverable. Polkit is not part of this seam
  ([ADR 0006](docs/adr/0006-polkit-in-supervisor.md)): it is a
  Supervisor-owned capability like any other, deferred until a caller lands.
  When it does, register the agent only on `polkit:enable_agent()`, never at
  boot, so a default agent (polkit-gnome, lxqt-policykit) keeps running until
  a config opts in.
- [ ] 52. Add detached jobs, public IPC, or broker extraction only when a
  lifetime or authority requirement exists. Test the ownership and revocation
  rules for each facility.
- [ ] 53. Add theming when the first themed widget exists: palette capability
  with fixed role vocabulary, then wallpaper derivation. Template stamping into
  other apps' configs is deferred until a real consumer asks; Noctalia's MIT
  templates remain reference material.
- [ ] 54. Add icons, assets, canvas, shaders, accessibility, compositor adapters
  beyond Niri/Hyprland, packaging, and generated LuaLS definitions as separate
  slices, each with an explicit trigger (e.g. accessibility: first AT-SPI
  consumer).
- [ ] 55. Run reload-memory, compositor, capability-fault, and lock recovery
  qualification. Remove unused dependencies and stale alternatives.

## Acceptance gate

A step needs:

- one owner for every resource and process;
- byte and count limits at every input and queue seam;
- deterministic cleanup;
- one focused runnable test;
- a manual compositor or real-session check when required.

Do not mark a product widget as framework work. Do not add a dependency because
the plan mentions it. Add it when code needs it and a test can exercise it.
