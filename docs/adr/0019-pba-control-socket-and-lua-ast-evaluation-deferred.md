# PBA control-socket transport and Lua AST evaluation are deferred, not built

Phase 8's title ("Hot-Reload Presentation Before Authority (PBA) Flow") and its cited prose
(`docs/oblisk-supervisor-services-dbus.md` § 15.1-15.4) describe a full end-to-end handoff: a
Candidate renderer process that evaluates a Lua AST for `shell.lua`, binds real Wayland
layer-shell surfaces and stages null-buffers, exchanges messages with the Supervisor over a
private Unix control socket, attaches `wp_presentation_feedback` to a real GLES3 frame commit,
and gets promoted while Generation `N` is reaped via Phase 7's `process::spawn_group_leader`/
`process::reap_process_group`. None of the surrounding system exists yet, same situation
ADR-0015, ADR-0017, and ADR-0018 hit for polkit's PAM conversation, `audio.apps`'s Lua push, and
`process.run`:

1. **The real Unix control-socket wire transport.** No listener exists in `supervisor`, no
   client exists in `renderer`. `§ 15.2`'s "private control socket" and its framing, connection
   lifecycle, and message encoding are all unresearched here -- inventing them now would mean
   guessing at a wire format nothing else in the codebase defines or consumes.
2. **Lua AST evaluation.** `§ 15.2` point 1 has the Candidate "compile and evaluate `shell.lua`"
   -- `mlua` has been a `renderer` dependency since scaffolding (`renderer/Cargo.toml`, commit
   `9fd6231`), but nothing in `renderer/src/` instantiates or calls into it: there is no working
   Lua VM anywhere in this codebase, matching the gap ADR-0015 and ADR-0017 already hit for
   polkit's textfield and `audio.apps`.
3. **Renderer-side null-buffer Wayland commit and `wp_presentation_feedback` wiring.** `§ 15.2`
   point 2 and `§ 15.3` describe the Candidate acknowledging `zwlr_layer_surface_v1`'s `configure`
   with a null-buffer commit, then later attaching `wp_presentation_feedback` to a real GLES3
   frame. Grepped `renderer/src/wayland/` -- neither exists there yet; the Renderer doesn't
   participate in this protocol at all today.
4. **NetworkManager/BlueZ state-hydration payload construction.** `§ 15.2` point 1 has state
   hydration cover "NetworkManager, BlueZ, and PipeWire state snapshots." Only the PipeWire
   mixer exists (`supervisor/src/audio/mixer.rs`, Phase 6) -- there is nothing to build a real
   NetworkManager/BlueZ payload out of yet.
5. **True multi-output "verified across all outputs" fan-out.** `§ 15.4` gates the atomic swap
   on presentation evidence "across all connected displays." ADR-0003 already establishes the
   target shape for this -- authority transfers per-`(generation, output)` pair, not as one
   whole-process flag, specifically so one sleeping or slow-to-wake output can't stall every
   other output's promotion. This phase's `CandidateLink::recv_presentation_evidence` collapses
   that to one verified-evidence signal, structurally: there's no per-output Wayland surface
   tracking on either side of the (nonexistent) control socket yet to fan that signal out
   against.
6. **`§ 15.4`'s "Swap" messaging: input deselection on `N` and promotion signaling to `N+1`.**
   `§ 15.4` point 1 has the Supervisor tell Generation `N` to clear its input region, and point 2
   has it tell `N+1` to claim pointer/keyboard focus -- two more control-socket operations,
   distinct from `CandidateLink`'s four `§ 15.2`/`§ 15.3` methods. Nothing in `reload.rs` sends
   either message: `run_pba`'s final step only reaps `N`'s process group once evidence is
   verified, which is the "Reap" half of `§ 15.4`, not the "Swap" half. This rides on the same
   unbuilt control-socket transport as item 1, so it's deferred for the same reason: no wire
   format exists yet to carry either message.
7. **Wiring `reload.rs` into `main.rs`'s runtime.** Even with the trait/state-machine seam built,
   there's no real `CandidateLink` implementation to drive it with, and spawning a real
   Renderer binary as a "candidate" would hang forever: today's Renderer binary has no code that
   sends a ready signal or presentation evidence over anything.
8. **Wiring up `inotify`.** `inotify = "0.11.5"` has been an unused dependency in
   `supervisor/Cargo.toml` since it was scaffolded, clearly staged for this phase's
   config-watch trigger (`§ 15.1`: "a configuration file edit is detected via the Supervisor's
   `inotify` watch"). It stays unused: with no real caller for `reload::run_pba` yet (item 7),
   a real inotify watcher would have nowhere to feed its events.

Decision: `supervisor/src/reload.rs` ships the PBA orchestration's ordering and gating contract
only -- the state machine build-steps.md's numbered list and § 15.1-15.4 describe, implemented
against two real primitives and one seam:

- **Real process lifecycle.** `run_pba` calls `process::spawn_group_leader` for step 1
  (Overlapping Spawn) and `process::reap_process_group` for both the success path (step 6,
  reaping Generation `N` after evidence verification) and every failure path (aborting the
  Candidate). This is that pair's first real caller, per ADR-0018's upgrade-path item (c).
- **`CandidateLink`, the IPC-boundary trait.** Its four methods are each one operation
  § 15.2-15.3 names crossing the control socket, named to trace back directly:
  `push_state_snapshot` (§ 15.2 point 1, "State Hydration"), `recv_ready_signal` (§ 15.2 points
  2-3, "Null-Buffer Staging"), `send_activate_draw` (§ 15.2 point 3, "Activate Draw"), and
  `recv_presentation_evidence` (§ 15.3, "Evidence Verification"). `shared::StateSnapshot` is
  reused as-is for hydration's payload -- § 15.2 names exactly this ("a state snapshot pushed by
  the Supervisor"). `shared::CommandEnvelope` is deliberately *not* reused for `ActivateDraw`:
  `docs/oblisk-idl-api-specs.md` § 7.2 defines that envelope as a generation-guarded wrapper
  around a Lua-initiated write action traveling Renderer -> Supervisor (capability, action,
  arguments, expected_revision) -- the opposite direction and a different shape from a
  Supervisor-issued one-off activation nonce. Forcing it on would invent a mismatched payload
  rather than reuse a real fit; `ActivateDraw` carries a plain `u64` nonce instead.
- **Failure semantics** (not spelled out by § 15's happy-path prose; chosen as the reading
  consistent with PBA's whole point -- never a black frame, never an unverified swap): any
  failure before presentation evidence is verified -- a `CandidateLink` error, or the ready- or
  evidence-deadline expiring -- aborts the Candidate (reaps its process group) and leaves
  Generation `N` untouched and still authoritative. Generation `N` is reaped only after evidence
  verification succeeds, never before.

Tested against the real seam this phase actually builds (TDD, `/tdd` discipline): a fake
`CandidateLink` drives the handshake's timing deterministically (immediate success, delayed
success, hung/never-resolving calls, and a simulated link error), combined with real
short-lived `sh -c` child processes for both Generation `N` and the Candidate -- the same
technique as `process::mod`'s own tests -- so `spawn_group_leader`/`reap_process_group` are
exercised for real, not mocked. Ten tests in `supervisor/src/reload.rs` cover: full-success
promotion and reap; that Generation `N` is observably still alive *while* evidence verification
is in flight (a concurrent `tokio::select!` polls `/proc` mid-handshake, not just after
`run_pba` returns); a hang on each of the four handshake steps (state hydration, ready signal,
activate draw, evidence) each abort the Candidate and leave `N` untouched, tagged with the
correct `Stage`; a `CandidateLink` error aborts the Candidate and leaves `N` untouched; the
aborted Candidate's own process group is confirmed reaped via `/proc`, not just inferred; and a
spawn failure (nonexistent binary) surfaces cleanly without touching `N` or attempting an abort
reap on a Candidate that never existed.

Also kept minimal, matching ADR-0001/ADR-0006's split: `reload.rs` only orchestrates the
generation-swap path (candidate spawn through promote-and-reap). It has no opinion on, and
doesn't touch, the in-place (value-change) reload path ADR-0001 describes -- that path never
spawns a Candidate or touches this module at all.

Upgrade path, in order: (a) the real control-socket transport (item 1) gives `CandidateLink` a
production implementation, replacing the fake tests drive today; (b) the Lua VM and `shell.lua`
compilation (item 2) give the Candidate something real to do between staging and drawing; (c)
the Renderer's null-buffer commit and `wp_presentation_feedback` wiring (item 3) give the
Candidate side of the handshake real Wayland behavior to report through `CandidateLink`; (d)
real NetworkManager/BlueZ controllers (item 4) give state hydration's payload real content
beyond PipeWire's; (e) per-output evidence fan-out (item 5) replaces
`recv_presentation_evidence`'s single signal with the per-`(generation, output)` model ADR-0003
already specifies, once the Renderer tracks its own per-output surfaces to report against; (f)
the input-deselection and promotion messages (item 6) ride on (a)'s same transport and give
`§ 15.4`'s "Swap" half an actual wire signal, rather than leaving it inferred from which `Child`
`PbaOutcome` returns; (g) once (a)-(c) exist, `main.rs` gets a real `mod reload;` caller -- an
inotify watch on `~/.config/oblisk/` (item 8) feeding config-edit events into `run_pba`, per
§ 15.1.

This does not contradict `docs/oblisk-supervisor-services-dbus.md` § 15, `build-steps.md`'s
Phase 8 text, or ADR-0003: all three describe the target shape once the control-socket
transport, the Lua VM, the Renderer-side Wayland wiring, and per-output evidence tracking exist,
and none of that is built or wired against here. It also does not contradict ADR-0001 or
ADR-0006: this module is exactly the generation-swap half those ADRs already carved out as
distinct from in-place reload, built to the ordering contract they assume without altering
either decision.
