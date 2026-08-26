# PBA orchestrator wired with atomic per-candidate promotion, not true per-output streaming

Phase 14's title ("Wire the PBA Orchestrator, Per-Output Evidence & Renderer Presentation
Feedback") and build-steps.md's own text describe giving `reload::run_pba` and its
`CandidateLink` trait (Phase 8, ADR-0019) their first real transport, per-output evidence
collection, and a real Renderer-side null-buffer/`ActivateDraw`/`wp_presentation_feedback`
handshake. Matching that scope, this phase does not build:

1. **Scene-to-GPU rendering.** The retained `Scene` (Phase 12) is still not wired to any real
   `wl_surface` anywhere in the codebase -- `renderer/src/layout/mod.rs`'s own doc comment
   already says its input-region computation is "pure but not yet wired to a live
   `wl_surface::set_input_region` call," and the only real `wl_surface` objects are the three
   static ones `wayland/mod.rs` creates directly at startup. `App::activate_draw` therefore draws
   the same Phase 3/4 static proof-of-pipeline content (`bind_and_clear`'s existing clear +
   "Oblisk" proof string on `main_bar`) that non-candidate mode already drew on first configure --
   not Lua-authored `Scene` content. There is no later numbered build-steps.md phase for this gap
   either; it's a real, disclosed ceiling, not an oversight.
2. **A topology-driven, arbitrary per-output surface set.** ADR-0003's "output" is realized here
   as the fixed set of `TrackedSurface`s `wayland::App` already creates -- `main_bar` and
   `overlay_canvas` (one surface each, `output: None`, compositor picks) plus one
   `wallpaper_layer` per connected output -- not a monitor in the abstract, and not anything
   `Scene`/Lua topology drives (since, per item 1, nothing wires `Scene` to real surfaces yet).
   Every wire message and `CandidateLink` method therefore names this `surface_id: String`, not
   `output`: it's honestly a surface identifier for today's fixed set, not yet ADR-0003's full
   per-monitor model for arbitrary Lua-authored surfaces. `surface_id` is `role.label()` for
   `main_bar`/`overlay_canvas` and `"wallpaper_layer@{name}"` (falling back to
   `"wallpaper_layer@output-{index}"` when the compositor doesn't report a name) per wallpaper
   instance, resolved once in `create_wallpaper_layers` and stored on `TrackedSurface`, not
   recomputed later.
3. **True per-output independent promotion timing, or partial-candidate abort.** ADR-0003
   describes each output transferring "the moment its own presentation evidence lands, with no
   barrier on its siblings," including waiting indefinitely for a sleeping output. Building that
   for real requires a primitive to *partially* abort a candidate -- reap it for only the
   surface_ids that never arrived, while the ones that did already transferred -- or to keep a
   superseded generation alive-but-partially-authoritative indefinitely. Neither exists, and
   building either is a much bigger change than this phase's scope. Streaming promotion per-output
   while the whole candidate is still abortable on a global `evidence_timeout` is actively unsafe
   regardless: if 2 of 3 surfaces promote and the 3rd times out, aborting (reaping) the candidate
   now blacks out the 2 surfaces that already transferred to it -- exactly the failure PBA exists
   to prevent. So `reload::drive_handshake` collects evidence *per surface_id* (the wire-level
   granularity ADR-0019 item 5 asked for) but still gates the Swap
   (`DeselectInput`/`PromoteGeneration`) on *all* expected surface_ids reporting within one shared
   `evidence_timeout` -- the same all-or-nothing safety property as before this phase, built out of
   N evidence messages instead of one. `PbaOutcome::promoted_surfaces` is always either every
   expected surface_id (full success) or the operation fails entirely (`PbaFailure`); there is no
   partial-success return shape.
4. **Real per-surface input-region/focus effects for `DeselectInput`/`PromoteGeneration`.** Both
   are real wire messages now (`shared::DeselectInput`/`PromoteGeneration`, sent by `main.rs` for
   every promoted surface_id, received by `renderer/src/socket.rs`'s `dispatch_loop`) but have no
   real effect on the Renderer side -- there's no per-surface input-region/focus machinery to
   attach them to yet (that's downstream of item 1: it would need real `wl_surface`s tied to real
   Lua-authored topology). `dispatch_loop` logs and drops both.
5. **Concurrent PBA handshakes.** `main.rs`'s main `tokio::select!` loop blocks synchronously for
   the duration of one in-flight swap handshake (bounded by `PBA_TIMINGS`'s `ready_timeout +
   evidence_timeout`, seconds not minutes) rather than running `run_pba` concurrently with the
   rest of the loop via `tokio::spawn`. This is safe and deliberate: swaps are rare and bounded,
   nothing else is capability-routed over this socket yet anyway (ADR-0020), and `run_pba` itself
   only drives one candidate at a time regardless -- a router/spawned-task/oneshot design to demux
   `inbound_frames` between the ordinary loop and an in-flight handshake would be real complexity
   spent on a case that can't happen today.
6. **A distinct fast-fail signal for `wp_presentation_feedback`'s `discarded` event.** § 15.3
   only asks for `presented`; `discarded` (the content update was never displayed, e.g. superseded
   or the surface was destroyed) is logged only in `App::discarded` and does **not** send anything
   into `presented_tx`. The existing `evidence_timeout` is what catches a surface that never
   presents, discarded or otherwise -- inventing a second, faster failure path for `discarded`
   specifically would duplicate that gating logic for a case `evidence_timeout` already handles
   correctly (and a `discarded` frame followed by a later real `presented` on a retried commit,
   while unlikely for this phase's static content, isn't ruled out by the protocol either).
7. **A packaging/install-path story for the Candidate binary.** `renderer_binary_path` in
   `main.rs` resolves the Renderer binary via `std::env::current_exe()?.with_file_name("renderer")`
   -- the standard same-workspace cargo layout (`target/{profile}/renderer` next to
   `target/{profile}/supervisor`). No install-path configuration exists yet, and none needs to for
   this phase; a real packaging phase will need to replace this assumption.
8. **NetworkManager/BlueZ state-hydration content.** `run_pba`'s `push_state_snapshot` still
   reuses whatever `shared::StateSnapshot` was last pushed to the authoritative generation (the
   PipeWire audio-apps push, Phase 11) or a fresh revision-0 empty one if none has been pushed
   yet. ADR-0019 item 4's gap (only PipeWire content exists) is unchanged by this phase -- still
   open, not touched here.
9. **Everything below this line**, disclosed the same way, that came up while implementing rather
   than being anticipated above.

## Decision

**Real wire types** (`shared/src/lib.rs`): `ActivateDraw { nonce }`, `ReadySignal { surfaces }`,
`PresentationEvidence { nonce, surface_id }`, `DeselectInput { surface_id }`,
`PromoteGeneration { surface_id }`, added to `SupervisorFrame`/`RendererFrame` with the same
adjacently-tagged (`kind`/`data`) convention ADR-0024 established. `ActivateDraw` still isn't
`CommandEnvelope` (ADR-0019's reasoning: wrong direction/shape, a Supervisor-issued one-off nonce
vs. a Lua-initiated write action) -- unchanged by this phase, just finally sent for real.

**`CandidateLink`'s real implementation** (`supervisor/src/reload_link.rs`, new file):
`SocketCandidateLink` borrows the Supervisor's shared `inbound_frames` channel and a cloned
`GenerationRegistry` for the duration of one in-flight handshake (see item 5 above).
`push_state_snapshot`/`send_activate_draw` go through `GenerationRegistry::send_frame` (new
method, consolidating the free `push_frame` function `main.rs` used to have -- its second real
caller, per the spec, is exactly the reason to consolidate). `recv_ready_signal`/
`recv_presentation_evidence` loop-and-filter `inbound_frames`: a frame from the wrong
`generation_id`, or a frame type/mismatched nonce this link doesn't care about (a `Command` or
`ReevaluateReport` from the still-authoritative generation is entirely plausible mid-handshake),
is logged and dropped, not routed anywhere else. A closed channel is `SocketLinkError::
ConnectionClosed`, distinct from a send failure (`SocketLinkError::Send`, wrapping
`socket::SendFrameError`). Both `SocketLinkError` and `SendFrameError` get real `Display`/`Error`
impls (not just `Debug`) -- partly for better log messages, partly because clippy's `dead_code`
lint doesn't credit a derived `Debug`-only access as "reading" an enum variant's field, and this
phase gives both types their first real (non-test) callers, so their fields needed a real,
non-test read path to stop being flagged. `recv_matching`'s rejected-frame `Err` payload is boxed
(`Box<RendererFrame>`, not `RendererFrame`) -- `RendererFrame` is large enough (its `Command`
variant embeds a whole `CommandEnvelope`) that clippy's `result_large_err` lint flagged the
unboxed closure return type.

**`reload.rs`'s restructuring**: `CandidateLink::recv_ready_signal` now returns `Vec<String>` (the
staged surface_ids) and `recv_presentation_evidence` returns which surface_id one piece of
evidence is for. `drive_handshake` collects evidence in a loop wrapped in one
`timeout(evidence_timeout, ...)` for the whole loop, comparing each returned surface_id against
`expected` and a `HashSet` of already-collected ones -- an unannounced or duplicate surface_id
fails with the new `PbaFailure::UnexpectedEvidence { stage, surface_id }` variant, immediately (no
waiting for the timeout). `run_pba` drops the `superseded: &mut Child` parameter entirely and
returns `PbaOutcome { candidate, promoted_surfaces }` instead of also reaping `superseded` --
§ 15.4's ordering is Input Deselection -> Candidate Promotion -> Reap, and the Swap messages go to
two different connections (`superseded`'s and the candidate's) while `CandidateLink` is
deliberately scoped to only the candidate's. Reaping inside `run_pba` would force sending the Swap
messages either too early (before evidence is verified) or too late (after `superseded`'s
connection is already dead) -- so `run_pba` stops right after evidence verification, and the
caller (`main.rs`) sends the Swap messages, then reaps `superseded` directly via
`process::reap_process_group` (now a real public API, `#[allow(dead_code)]` removed from both it
and `spawn_group_leader`, which also gained an `envs: &[(String, String)]` parameter for
`OBLISK_GENERATION_ID`/`OBLISK_PBA_CANDIDATE`).

**`main.rs`'s real wiring**: `RENDERER_GENERATION_ID` is gone, replaced by a real `Authoritative {
generation_id, child }` struct reassigned wholesale on a successful swap, starting from a real
boot-spawned Generation 0 (`process::spawn_group_leader`, fatal on failure -- there is no shell
without a Generation 0). `next_generation_id` starts at 1 and increments per candidate.
`ReevaluateReport::TopologyChanged` is now `run_pba`'s real caller: it builds `candidate_envs`,
constructs a `SocketCandidateLink` borrowing `inbound_frames` for the handshake's duration, and on
success sends `DeselectInput`/`PromoteGeneration` for every promoted surface_id before reaping the
old `authoritative.child` and replacing `authoritative` with the new generation. The `&mut
inbound_frames` borrow needed for `SocketCandidateLink` inside the same `select!` arm that
received `inbound_frames.recv()`'s value compiled without any restructuring -- `tokio::select!`
drops each branch's polling future once it resolves and its value is extracted, so the borrow is
no longer live by the time the match arm's body runs; the spec's suggested fallback (a standalone
helper `async fn`) wasn't needed. `PBA_TIMINGS` picks `ready_timeout: 2s`, `evidence_timeout: 3s`
(generous enough that a healthy Candidate binding real Wayland/EGL never trips them, tight enough
that a wedged one doesn't hang a config edit indefinitely) and reuses `process::
DEFAULT_REAP_GRACE` (100ms) for `reap_grace` -- this constant's, and the swap's own reap call's,
first real callers.

**Renderer-side candidate mode** (`renderer/src/wayland/mod.rs`): `App::is_pba_candidate` is read
once from `OBLISK_PBA_CANDIDATE` in `run()`. `bind_and_clear`'s first-configure branch, in
candidate mode, commits a null buffer (`wl_surface::attach(None, 0, 0)` + `commit()`) instead of
binding EGL, and records the configure's size on a new `TrackedSurface::configured_size` field --
needed because candidate mode never captures a size from `configure` any other way, and
`App::activate_draw` (called later, from the poll loop, not from a `configure` callback) needs a
real size to bind its EGL window surface to. `App::maybe_send_ready_signal` sends the full
surface_id list via `ready_tx` once every tracked surface's `null_buffered` is `true`, exactly
once (`ready_signal_sent`). `App::activate_draw`/`activate_draw_one` mirror the existing
non-candidate first-configure EGL-bind-and-clear path (indexing into `self.surfaces` by position
rather than holding a `&mut TrackedSurface`, for the same borrow-conflict reason
`draw_main_bar_proof_text` already documents), requesting `wp_presentation_feedback` immediately
before `swap_buffers` so the request associates with the commit `swap_buffers` performs. The tail
loop in `run()` replaces `event_queue.blocking_dispatch(&mut app)?` with a bounded-latency loop
(`dispatch_pending` + a 15ms `nix::poll` on the connection fd + checking `activate_rx`) -- a real
Wayland event might not arrive for a long time after `ActivateDraw` is sent, since nothing else
happens on these mostly-static surfaces once staged. This changes the *non-candidate* path's
blocking behavior too (no longer blocks indefinitely between dispatches), but doesn't change its
first-configure draw behavior, which still runs synchronously inside the `configure` handler that
`dispatch_pending` still calls -- and there were no existing tests directly driving this loop to
update.

**`wp_presentation`/`wp_presentation_feedback` binding -- a deliberate deviation from this
phase's own spec text.** The spec's draft assumed writing manual `Dispatch<WpPresentation,
GlobalData>`/`Dispatch<WpPresentationFeedback, PresentationFeedbackData>` impls by hand, with a
custom `PresentationFeedbackData { nonce, surface_id }` struct passed as the `feedback()`
request's user data. Verifying against the vendored `smithay-client-toolkit-0.21.1` source (as
the spec itself instructed) found this crate already ships a complete, tested
`smithay_client_toolkit::presentation_time` module: `PresentationTimeState::bind`/`::feedback`
and a `PresentationTimeHandler` trait (`presented`/`discarded` callbacks), wired through SCTK's
own `Dispatch2`/`delegate_dispatch2!` machinery this file already uses for every other protocol
(`CompositorState`, `LayerShell`, `OutputState`). Its `feedback()` method doesn't accept custom
user data -- it builds its own opaque, `#[doc(hidden)]` `PresentationTimeData` internally, which
already tracks the target `wl_surface` and hands it back on `presented`/`discarded`. So `App`
implements `PresentationTimeHandler` directly (no manual `Dispatch` impls, no custom user-data
struct) and correlates evidence itself: `nonce` comes from a single `App::active_nonce` field (one
handshake in flight at a time, per item 5 above), and `surface_id` is recovered by matching the
callback's `&wl_surface::WlSurface` against `self.surfaces`. This is a strictly better fit than
what the spec sketched -- it reuses tested infrastructure instead of hand-rolling protocol
boilerplate -- and is exactly the kind of divergence the spec's "verify against vendored source"
instruction anticipated. Relatedly: the spec's text about "destroy the feedback object's proxy
after" turned out to describe something that doesn't exist to call -- `wp_presentation_feedback`'s
protocol XML has no `destroy` request at all; both `presented` and `discarded` are marked
`type="destructor"`, so the object is automatically invalidated client-side once either event is
processed. There is no explicit destroy call to make, and SCTK's own generated bindings don't
expose one.

## Tested against

TDD, one seam at a time, real transports/processes wherever this codebase already established
that pattern:

- `shared/src/lib.rs`: one adjacent-tagging round-trip test per new type (`ActivateDraw`,
  `DeselectInput`, `PromoteGeneration`, `ReadySignal`, `PresentationEvidence`), matching Phase 13's
  own test style exactly (a raw `serde_json::json!` comparison plus a round trip).
- `supervisor/src/reload.rs`: real short-lived `sh -c` child processes for the Candidate (matching
  this file's existing `process::mod`-style technique), a `FakeCandidateLink` extended with a
  `ready_surfaces: Vec<String>` field and a per-call `EvidenceOutcome` queue (`Return`/`Fail`/
  `Hang`) so a test can script multiple surfaces succeeding, one hanging, one failing, or the same
  one twice. Covers: full success with 1 and with 3 expected surfaces (the 3-surface case proves
  `promoted_surfaces`' order comes from `ReadySignal`, not evidence-arrival order, by scripting
  evidence to arrive in a shuffled order); a hang on each of the four handshake steps aborting the
  candidate with the right `Stage`; 2 of 3 expected surfaces reporting while the 3rd hangs still
  times out and the candidate is still reaped (proven via `/proc`, not just inferred, matching this
  file's established pattern); an unannounced surface_id and a duplicate surface_id both producing
  `PbaFailure::UnexpectedEvidence`; a link error during evidence collection specifically (a gap
  the pre-Phase-14 test suite didn't cover, since evidence collection used to be a single call);
  the pre-existing link-error/spawn-failure coverage, updated for the new signatures.
- `supervisor/src/reload_link.rs`: `SocketCandidateLink` tested with a real `GenerationRegistry`
  (a fake connection registered directly, `GenerationRegistry::register` widened to `pub(crate)`
  for this) and a hand-constructed `mpsc::UnboundedReceiver<InboundFrame>` fed `InboundFrame`
  values directly -- no real `UnixListener` needed to exercise the filtering/matching logic, per
  the spec's own suggested shortcut. Covers the happy path for all four `CandidateLink` methods, an
  irrelevant frame (wrong generation, then right-generation-wrong-type) arriving before the
  relevant one being skipped not returned, a mismatched-nonce evidence frame being skipped then a
  matching one accepted, and the channel closing while waiting reporting `ConnectionClosed`.
- `renderer/src/socket.rs`: `dispatch_loop`'s new `tokio::select!` branches, each with its own
  test -- `ActivateDraw` forwarding its nonce to a `std::sync::mpsc` receiver; `DeselectInput`/
  `PromoteGeneration` logging and not derailing the loop (proven by a third, recognized frame
  still getting a real response afterward); the bridged `ready`/`presented` channels each writing
  their corresponding `RendererFrame` variant back over the wire, raced against a timeout since
  `dispatch_loop` doesn't return on its own in those tests. The pre-existing `dispatch_loop` test
  updated for the widened signature (channel ends it doesn't exercise are simply never sent into).
- `renderer/src/wayland/mod.rs`: no test harness invented, matching this file's existing
  precedent (Wayland-protocol-integration code, no headless harness in this repo) -- the one piece
  of pure logic factored out, `wallpaper_surface_id(name: Option<&str>, index: usize) -> String`,
  gets direct unit tests for both the named and the fallback case.

Not automated, matching this codebase's established fixture-style ceiling for anything short of a
full two-process integration harness (`docs/adr/0024`'s own "Not automated" paragraph already
states this precedent): a real Supervisor and a real Renderer process actually swapping over a
real `$XDG_RUNTIME_DIR` socket, driving a real compositor. **Not manually smoke-tested either**,
unlike this spec's Testing section asked for: this implementation session ran against the
developer's own live Wayland session (a real `niri` compositor, not a nested/headless test
instance) -- `main_bar`'s layer-shell surface reserves a 32px exclusive zone at the top anchor,
so actually running either binary here would visibly reserve screen space (and, non-candidate
mode, draw a real bar) on someone's live desktop as a side effect of an automated build task.
Deliberately not run for that reason. What backs this phase's correctness instead: every
non-obvious API surface (`nix::poll`'s `PollFd`/`PollTimeout`, `wl_surface::attach`/`commit`'s
exact signatures, `wayland-protocols`' `presentation_time` module path and its stable-not-staging
feature gating, `smithay-client-toolkit`'s own `presentation_time` module and `Dispatch2`/
`delegate_dispatch2!` mechanics, `GlobalList::bind`'s signature) was verified against the vendored
crate source under `~/.local/share/cargo/registry/src/`, not guessed; the candidate-mode null-
buffer/EGL-bind code paths mirror this file's existing, previously-real-compositor-tested
non-candidate first-configure path almost line for line (same EGL calls, same GL clear, same
proof-text draw); and the whole workspace builds clean (`cargo check --workspace`) and passes
`cargo clippy --workspace --all-targets --all-features -- -D warnings` with zero warnings. A real
smoke test (`WAYLAND_DEBUG=1`, confirming `feedback`'s request precedes the corresponding `commit`
on the wire, and a real end-to-end swap against a disposable nested compositor such as `Xwayland`/
a headless `wlroots` instance rather than a live session) is still owed before this phase should be
considered production-verified, not just compile-verified.

## Upgrade path, in order

(a) Scene-to-GPU rendering (item 1) gives `ActivateDraw` real Lua-authored content to draw, once
some later phase wires the retained `Scene` to real `wl_surface`s at all; (b) that same wiring
would let `surface_id` (item 2) generalize from today's fixed `TrackedSurface` set to a real
topology-driven set derived from `Scene`; (c) a partial-candidate-abort primitive (item 3) would
let per-output promotion stream independently, matching ADR-0003's full model, instead of gating
on one shared `evidence_timeout`; (d) real per-surface input-region/focus machinery (item 4) would
give `DeselectInput`/`PromoteGeneration` an actual effect to attach to, instead of a log line; (e)
once capability routing exists over this socket (ADR-0020's own ceiling), running `run_pba`
concurrently (item 5) via `tokio::spawn` becomes worth the router/demux complexity it currently
isn't; (f) real NetworkManager/BlueZ controllers (item 8, ADR-0019 item 4) give state hydration's
payload real content beyond PipeWire's; (g) a packaging/install-path story (item 7) replaces
`current_exe()`'s sibling-directory assumption once one exists.

This does not contradict `docs/oblisk-supervisor-services-dbus.md` § 15, build-steps.md's Phase 14
text, ADR-0003, or ADR-0019: all describe the target shape once Scene-to-GPU rendering, real
per-output surface tracking, and a partial-abort primitive exist, none of which this phase builds
or claims to. It closes ADR-0019 items 1, 3, and 6 (the control-socket transport, the Renderer-side
null-buffer/`wp_presentation_feedback` wiring, and the Swap messages) and item 7 (wiring `reload.rs`
into `main.rs`'s runtime) in full; item 5 (true multi-output fan-out) only partially, per item 3
above. Item 2 (Lua AST evaluation) was already closed by earlier phases (Phase 11/13's real Lua
loader), not this one. Item 4 (NetworkManager/BlueZ hydration) is unrelated to this phase's scope
and remains exactly as ADR-0019 left it open.
