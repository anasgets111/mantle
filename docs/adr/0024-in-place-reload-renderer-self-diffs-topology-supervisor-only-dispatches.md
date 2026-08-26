# In-place reload: the Renderer self-diffs topology, the Supervisor only dispatches

Phase 13's title ("Watcher & In-Place Reload") and its build-steps.md text scope a Supervisor-side
`inotify` watcher on `~/.config/oblisk/` that, on a debounced edit, asks the authoritative
generation's loader to re-evaluate and report its new surface topology, then either applies the
result in place (unchanged topology) or hands off to a generation swap (changed topology).
Matching that scope, this phase does not build:

1. **A generation swap on `TopologyChanged`.** The Supervisor logs it and stops -- `reload::run_pba`
   (Phase 8) is fully built and tested against a fake `CandidateLink`, but its runtime caller is
   still Phase 14's job (docs/adr/0019 already deferred this; nothing new here). `main.rs`'s
   `RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { .. })` arm is the real
   detection/dispatch point a future `run_pba` call slots into.

2. **`reset_registrations` (ADR-0006) doing anything, and its exact ordering relative to
   evaluation.** `reset_registrations(generation_id)` is a real, called, currently-empty function,
   not the real IPC message ADR-0006's own text literally describes ("in-place reload sends one
   explicit IPC message... capability: `'renderer'`, action: `'reset_registrations'`") -- there's
   no capability yet to register anything against a `generation_id` (`oblisk-supervisor-services-
   dbus.md`'s idle/tray/MPRIS/etc. sections, Phase 16, don't exist), so there is nothing for a real
   message to reach. Matches `socket::GenerationRegistry` itself sitting real-but-unwired through
   Phase 9-10 (docs/adr/0020).

   A second, sharper gap a Spec review caught: ADR-0006 requires the reset to happen *before* "the
   fresh top-level run repopulates" registrations, so a re-issued `idle:register_threshold` reads
   as a replacement, not a duplicate leak. This phase's round trip can't honor that ordering as
   written -- `handle_reevaluate` runs the evaluation (the "top-level run") *before* the Supervisor
   even decides whether to reset anything, because that decision depends on the evaluation's own
   verdict. Calling `reset_registrations` any earlier would be actively wrong: it would also fire
   for a `TopologyChanged` or `Failed` verdict, where generation `N`'s real, currently-applied
   registrations must *not* be cleared (`N` keeps running its old, unedited config in both cases).
   Correctly solving this needs the same pending/apply staging this phase already gave the retained
   `Scene` -- a real registration call made during a *speculative* evaluation would need to stage
   itself, not take effect, until that evaluation is confirmed `Unchanged` and applied. There's no
   real registration capability yet to build or test that staging against, so it isn't built now;
   see the upgrade path. A narrower, already-fixed correctness bug in the same area (a `reset_
   registrations` call firing for an `Unchanged` report that a newer, in-flight `Reevaluate` has
   already superseded) is closed by `main.rs`'s `is_current_reload` guard -- see the Decision
   section -- but that guard does not touch the ordering-relative-to-evaluation gap described here.

3. **The full `oblisk.*` namespaced signal tree.** Only a second ad-hoc global, `rescue`, is
   registered (`{ is_rescue, error_log }`, a plain Lua table, not per-field `Signal`s), mirroring
   the ad-hoc `audio` global ADR-0022 already chose over building the tree. `docs/oblisk-idl-api-
   specs.md` § 2.10's `oblisk.rescue.is_rescue`/`oblisk.rescue.error_log` naming is honored in
   spirit (same two fields, same meaning), not in the literal namespaced form.

4. **A startup evaluation failure keeping any prior scene.** `run_startup_evaluation` has no prior
   applied scene to roll back to -- there's no round trip, no candidate, nothing to fall back on if
   the very first `shell.lua` read fails. The shell is blank until the user fixes the file and its
   next save fires a `Reevaluate`. `CONTEXT.md`'s Rollback entry ("never leave the shell blank") is
   about a *re*-evaluation keeping its prior scene; it doesn't speak to first boot, and this phase
   doesn't invent a boot-time fallback scene to satisfy it anyway.

   A Correctness review caught this claim not actually holding: `applied_topology` originally
   stayed `Vec::new()` (an *empty topology*, not "nothing applied") after a startup failure, which
   is structurally indistinguishable from a real generation with zero surfaces. Every later
   `Reevaluate` -- even against a file the user had correctly fixed -- diffed its non-empty
   topology against that empty one, always read as `TopologyChanged`, and `TopologyChanged` is
   never applied (item 1). The shell stayed blank permanently, not just until the next save, which
   is exactly what this item claimed wouldn't happen. Fixed by making `applied_topology: Option<Vec
   <SurfaceTopology>>`: `None` means "nothing applied yet, safe to apply the next evaluation",
   distinct from `Some(vec![])` (a real zero-surface generation). `handle_reevaluate` now treats
   `None` as not-changed. This is the one correctness fix in this phase that isn't just closing a
   race or an inert stub -- it's the difference between rescue mode being recoverable at all and
   not.

5. **Real multi-generation Watcher/topology bookkeeping.** `main.rs`'s `next_sequence` counter and
   every `push_frame` call are hardcoded to `RENDERER_GENERATION_ID` (`0`), matching every other
   `main.rs` push already in the file (the audio `StateSnapshot` arm predates this phase). Real
   generation-ID assignment tied to process spawning is still ADR-0020's already-declared ceiling.

6. **A configurable debounce window.** `RELOAD_DEBOUNCE` is a fixed `200ms` constant in `main.rs`.

7. **Watching anything but `shell.lua` itself.** `watcher::spawn_watcher` filters every inotify
   event to `event.name == Some("shell.lua")`; a future per-file import (a `modules/` directory, for
   instance) triggers nothing today.

8. **Resolving `layer`/`anchor`/`monitor` against real Wayland outputs.** `layout::node::
   SurfaceTopology`'s fields are compared as raw parsed values purely for topology-diff equality.
   Surfaces still aren't dynamically bound to any real `zwlr_layer_surface_v1` anywhere in this
   codebase (the three static surfaces from Phase 3 are still hardcoded) -- unrelated, still-unbuilt
   work this phase doesn't touch.

9. **A JSON-RPC-shaped wire protocol for the new messages.** `shared::SupervisorFrame`/
   `RendererFrame` are a hand-rolled adjacently-tagged enum (`{"kind": ..., "data": ...}`), not
   `shared::CommandEnvelope`'s shape. `CommandEnvelope`'s own doc comment, and `reload.rs`'s doc
   comment for the analogous `ActivateDraw` case, both already establish why: it's a guarded
   envelope around a *Lua-initiated write action* traveling Renderer -> Supervisor, not a shape for
   engine-internal control messages traveling either direction. Forcing it on here would be a
   mismatched reuse, the same call `reload.rs` already made for `ActivateDraw`.

Decision: the Renderer classifies its own re-evaluation, the Supervisor only dispatches on the
verdict.

- **Wire protocol** (`shared/src/lib.rs`): `ReevaluateRequest { sequence: u64 }` (Supervisor ->
  Renderer), `ReevaluateReport::{Unchanged, TopologyChanged, Failed} { sequence, .. }` and
  `ApplyPendingReload { sequence: u64 }` complete a three-message round trip per debounced edit,
  correlated by `sequence` -- the same role `reload::run_pba`'s `nonce: u64` already plays for
  `ActivateDraw`/evidence correlation, reused here because the debounced watcher can legitimately
  fire a second `Reevaluate` before the first round trip finishes, and a stale `ApplyPendingReload`
  must not clobber a newer pending evaluation. `SupervisorFrame`/`RendererFrame` multiplex these
  alongside the pre-existing `StateSnapshot`/`CommandEnvelope` frames per direction, adjacently
  tagged (`content = "data"`, not internally-tagged) because `ReevaluateReport` is itself an enum
  and can't merge into an internally-tagged wrapper's flat object. The correlation guard runs on
  *both* ends, not just the Renderer's `pending` check: a Correctness review found the Supervisor
  side had no equivalent guard, so an `Unchanged` report already superseded by a newer, in-flight
  `Reevaluate` still fired `reset_registrations` and sent an `ApplyPendingReload` nobody asked for
  any more (harmless today only because `reset_registrations` is a no-op). `main.rs`'s `is_current_
  reload(report_sequence, next_sequence)` closes this: an `Unchanged` report is only acted on if its
  `sequence` still matches the most recently sent `Reevaluate`.

- **Why the Renderer classifies, not the Supervisor.** The Renderer already holds both the old
  (currently-applied) and new (freshly-evaluated) topology in one process -- shipping a
  `SurfaceTopology` DTO across the wire just for the Supervisor to redo a comparison it can't do any
  more cheaply would be pure duplication. `CONTEXT.md`'s Watcher entry ("owns the swap-vs-in-place
  decision, not the reload's execution") is about *dispatch*: only the Supervisor can execute either
  branch (send the go-ahead, or -- Phase 14 -- spawn a candidate process), regardless of which side
  computed the classification.

- **`renderer/src/lua/mod.rs`**: `Loader::evaluate_file` reads `shared::shell_lua_path()`
  (`~/.config/oblisk/shell.lua`, resolved the same way on both sides via `$XDG_CONFIG_HOME`/`$HOME`)
  and evaluates it exactly like `Loader::evaluate` -- the real entry point `PROOF_OF_WIRING_SHELL`
  (Phase 11) stood in for.

- **`renderer/src/socket.rs`**: a `shared::StateSnapshot` push only hydrates the live `audio`
  signal now; it no longer triggers any Lua evaluation (Phase 11's proof-of-wiring behavior).
  Evaluation runs only on `Reevaluate` (plus once, directly, at startup with no round trip -- there
  being no prior scene to protect). A `Reevaluate` evaluates and diffs but does **not** apply to the
  `Scene` -- the evaluated `LoadOutput` is stashed as `pending`, applied only by a later
  `ApplyPendingReload` whose `sequence` still matches (a stale one is logged and ignored, the
  current `pending` left untouched). A `TopologyChanged` verdict never stashes `pending` at all: a
  topology-changed generation must not have its own scene mutated -- that's a different
  generation's job (a swap), not this one's in-place path.

- **`layout/node.rs` topology fields**: `layer` required (every existing fixture already sets it,
  same precedent as `id`); `anchor` defaults all-`false` (matches `EdgeInsets`'s existing
  default-zero shape, booleans instead of floats); `monitor` defaults `"All"` (§ 6.1 documents
  `"All"` as a real, meaningful value -- an unqualified surface already means "every monitor").

- **`supervisor/src/socket.rs`**: `InboundCommand`/`CommandEnvelope`-only decoding widens to
  `InboundFrame`/`RendererFrame`, since a Renderer connection now sends two message shapes
  (`Command`, unchanged since Phase 9; `ReevaluateReport`, new) in one direction.

- **`supervisor/src/watcher.rs`** (new): `spawn_watcher` watches the config *directory* (not
  `shell.lua`'s own inode -- an atomic-save editor unlinks/recreates rather than writing in place,
  which would silently break a watch on the old inode), filters to `shell.lua` by name, and
  debounces a burst of events (a typical save fires `CREATE`+`MODIFY`+`CLOSE_WRITE`, or `MOVED_TO`
  for an atomic-save editor) into exactly one trigger per settled edit. `inotify`'s default features
  (including `stream`, pulling in `futures-util`) have been declared, unused, on `supervisor` since
  scaffolding -- this is their first real caller. The debounce timer is an absolute deadline
  (`Option<Instant>`), not a relative `sleep(debounce)` reconstructed fresh every loop iteration --
  a Correctness review found the original relative version let *any* directory event (including one
  that isn't `shell.lua` and is otherwise ignored) push the deadline back, since looping at all
  rebuilt the sleep from "now". Only an actual `shell.lua` event now advances the deadline.

- **`renderer/src/socket.rs` grouping**: a Standards review found four functions
  (`dispatch_loop`/`run_startup_evaluation`/`handle_reevaluate`/`handle_apply_pending`) threading
  the same 6-7 pieces (`loader`, `shell_lua_path`, `scene`, `shaping`, `audio_handle`,
  `rescue_handle`, `state`) through their parameter lists separately, with `dispatch_loop` needing
  `#[allow(clippy::too_many_arguments)]` to compile. Grouped into one `RendererClient` struct with
  those four as methods; the `allow` is gone.

- **`surfaces_topology`'s error type**: a Correctness review found a topology-field error (e.g.
  `anchor.top` not a boolean) was folded into `LoaderError::InvalidTopLevelReturn`, whose fixed
  message ("must be a `surface` node or an array of them") is wrong for a field-level error inside
  an otherwise well-shaped surface -- that message ends up in `rescue.error_log`, shown to whoever's
  debugging their `shell.lua`. Given its own `LoaderError::InvalidTopology(String)` variant instead.

Tested against (TDD, one seam per cycle): `SupervisorFrame`/`RendererFrame`'s adjacent tagging for
every variant (`shared/src/lib.rs`); `parse_layer`/`parse_anchor`/`parse_monitor`/`surface_topology`
including the required-vs-defaulted-vs-`Signal`-rejected cases (`layout/node.rs`); `evaluate_file`
against a real temp file and a missing path (`lua/mod.rs`); `apply_state_snapshot` no longer
evaluating anything; `run_startup_evaluation` applying a valid file and clearing rescue, or setting
rescue and leaving `applied_topology` at `None` on a missing file; `handle_reevaluate` reporting
`Unchanged` (and stashing `pending`), `TopologyChanged` (and *not* stashing `pending`), `Failed` (and
setting rescue), and a topology-field error reporting a topology-specific message rather than the
top-level-return one, all against real evaluated files; a dedicated regression test proving a
successful evaluation *after* a startup failure is classified `Unchanged` (recoverable), not stuck
at `TopologyChanged` forever; `handle_apply_pending` reconciling a matching-sequence pending
evaluation into the `Scene`, and ignoring (not clobbering) a mismatched one; one end-to-end
`dispatch_loop` test proving the wire-level decode/dispatch/encode path really answers a
`Reevaluate` frame with a `ReevaluateReport` frame (`renderer/src/socket.rs`); `supervisor/src/
socket.rs`'s `RendererFrame` decoding for both `Command` and `ReevaluateReport`; `is_current_reload`
directly (`supervisor/src/main.rs`); `watcher.rs`'s debounce contract directly -- one trigger for a
single write, exactly one (not two) for a rapid burst, none for an unrelated file in the same
directory, and (the regression case) an unrelated event during an armed debounce window not
delaying the trigger, all against a real `tempfile` directory and real `inotify` events (no fake).

Not automated: nothing here exercises the Supervisor and Renderer as two real, separately-spawned
OS processes talking over a real `$XDG_RUNTIME_DIR` socket at once -- every test above drives one
side's real code against a fake peer (a `tokio::io::duplex` pair, or a real `UnixListener` on one
side only). Matches this codebase's established fixture style for anything short of a full
two-process integration harness (`renderer/src/socket.rs`'s and `supervisor/src/socket.rs`'s
existing tests already work this way).

Upgrade path, in order: (a) Phase 14 gives `TopologyChanged` (item 1) a real `run_pba` caller and
`reset_registrations` (item 2) its first real registration source once a D-Bus/hardware capability
exists to register against a `generation_id` -- at which point that capability's registration calls
need the same pending/apply staging the retained `Scene` already has, so a speculative evaluation's
side effects don't take effect until its `ApplyPendingReload` lands, closing the ordering gap item 2
describes rather than just filling in an empty function body; (b) the full `oblisk.*` tree (item 3) subsumes the
ad-hoc `rescue`/`audio` globals together whenever that's built, not before; (c) a boot-time fallback
scene (item 4) is a product decision, not an architectural gap -- add it if a blank first boot on a
broken config ever needs to look better than blank; (d) real generation-ID assignment (item 5)
generalizes every hardcoded `RENDERER_GENERATION_ID` site at once, this one included; (e) the
debounce window (item 6) and the single-file watch scope (item 7) are one-line changes whenever a
concrete need shows up; (f) real Wayland output binding (item 8) is Phase 14/16 territory, unrelated
to topology diffing itself.

This does not contradict `docs/oblisk-supervisor-services-dbus.md` § 15.1, `docs/oblisk-idl-api-
specs.md` § 2.10/§ 6.1, `CONTEXT.md`'s Watcher/Rollback/In-place reload/Generation swap/Topology
change entries, ADR-0006, or build-steps.md's Phase 13 text: all describe the target behavior this
phase delivers (debounced detection, re-evaluation, unchanged-vs-changed dispatch, rollback-safe
failure handling) without pinning down which process performs the topology comparison or what the
wire messages look like -- both are this phase's own necessary inventions, not contradictions of
anything stated.
