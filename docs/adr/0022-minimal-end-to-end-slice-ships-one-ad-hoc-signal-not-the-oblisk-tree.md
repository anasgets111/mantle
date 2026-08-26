# Minimal end-to-end slice ships one ad hoc signal, not the `oblisk.*` tree

Phase 11's title ("Minimal End-to-End Slice") and its build-steps.md text scope one thing: prove
Phase 9's socket and Phase 10's loader are wired correctly, using the audio mixer's already-real
data as the payload. Matching that scope, this phase does not build:

1. **The full `oblisk.*` signal tree / per-capability namespacing** (`oblisk-idl-api-specs.md`
   §2.1-2.13). `renderer/src/socket.rs` registers exactly one global, `audio`, holding whatever
   `StateSnapshot` the Supervisor most recently pushed. There is no `oblisk.audio.apps` path, no
   other capability's signal, and nothing preventing a second capability's push from landing on
   the same connection with no way to tell it apart from audio's -- there's only one message
   shape today, so nothing needs telling apart yet.

2. **`expected_revision`/staleness rejection** (ADR-0004). `supervisor/src/main.rs` increments
   `audio_revision` on every push and carries it in `StateSnapshot.revision`; the Renderer never
   reads it. Last-write-wins (`LiveSignalHandle::set` unconditionally overwrites) is correct only
   because the two sides talk over one ordered Unix-socket connection -- `read_json_frame` returns
   frames in the order `write_frame` sent them, so there's no reordering to detect. That stops
   being true the moment a second writer exists, or the transport itself can reorder or drop.

3. **Real generation-ID assignment.** `renderer/src/socket.rs` still resolves its identity from
   `OBLISK_GENERATION_ID`, defaulting to `0`; `supervisor/src/main.rs`'s new
   `RENDERER_GENERATION_ID` constant hardcodes the same `0` on the other end. Both sides agree by
   coincidence of shared defaults, not by any real handshake tying a generation ID to a spawned
   process. That's still a later phase, once Phase 7/8's spawn primitives connect to this
   transport.

4. **Reconnection.** If the Renderer's client socket drops, `renderer/src/socket.rs`'s connect
   loop does not retry -- it logs and the dedicated thread's `run()` returns, same as a failed
   first connect already did before this phase. Mirrors `supervisor/src/socket.rs`'s own
   already-documented "no accept-loop restart policy" ceiling (ADR-0020): neither side has a
   process-level supervision primitive to restart into yet.

5. **A real `shell.lua` file.** `PROOF_OF_WIRING_SHELL` in `renderer/src/socket.rs` is a hardcoded
   Rust string literal (`return surface { id = "bar", layer = "Top", audio_apps = audio:get() }`),
   re-evaluated on every push. No file is read from disk. The Watcher (`CONTEXT.md`, Watcher;
   Phase 13) owns the real file location and the trigger to (re-)read it.

6. **Anything downstream of the evaluated `LoadOutput`.** `run()`'s receive-loop callback logs
   each surface's `kind` and `properties` and stops there. Phase 12's retained-scene
   reconciliation is what will actually turn a `VirtualNode` tree into something drawn.

7. **Any capability besides `audio::mixer`.** BlueZ, NetworkManager, notifications, and the rest
   of `oblisk-supervisor-services-dbus.md`'s backends are untouched -- still nonexistent or, where
   they exist (polkit, per ADR-0015), still dead-ending at `eprintln!`.

8. **Write-command dispatch back through the socket** (Renderer -> Supervisor). This phase is
   push-only, Supervisor -> Renderer, matching build-steps.md's explicit direction ("The
   Supervisor is the listener, the Renderer connects as client... don't invert it"). Inbound
   `CommandEnvelope`s still just get logged, unchanged from Phase 9.

Decision: three small pieces connect Phase 9's socket to Phase 10's loader, none of them touching
either phase's existing shape.

- **`Signal::new_live`** (`renderer/src/lua/signal.rs`). `SignalKind` gains a third variant,
  `Live(Rc<RefCell<Value>>)`, alongside the existing `Direct`/`Computed`. `Direct` is frozen at
  construction; `Live` is the one kind Rust can overwrite afterward, through the paired
  `LiveSignalHandle::set`. `Rc<RefCell<_>>`, not `Arc<Mutex<_>>`: the `Loader` a live signal lives
  on stays confined to one dedicated OS thread (the socket-client thread), the same
  single-threaded-state convention `supervisor/src/audio/mixer.rs`'s `Rc<RefCell<MixerState>>`
  already uses -- no cross-thread `Send` bound needed because nothing ever moves this data across
  a thread boundary. `try_new_direct`'s marshalling checks (`marshal::check_number`, etc.) don't
  apply to `new_live`'s value: `try_new_direct` guards Lua-authored values crossing into Rust,
  while a live signal's value already passed through `serde_json`'s own serialization of a Rust
  struct (`AppStream`), which can't produce a NaN, an out-of-range integer, or an oversized string
  the way hand-authored Lua can.

- **`Loader::set_global`/`Loader::to_lua_value`** (`renderer/src/lua/mod.rs`). `Loader` already
  registers `computed` and the node constructors as globals internally at construction; these two
  methods expose that same mechanism to a caller outside the module, plus the JSON->Lua conversion
  (`mlua::LuaSerdeExt::to_value`, confirmed present under mlua 0.12.0's `"serde"` feature by
  reading `src/serde/mod.rs` directly) needed to turn a `StateSnapshot.payload`
  (`serde_json::Value`) into something `LiveSignalHandle::set` can store. Neither method changes
  what `evaluate` does or requires; they're additive.

- **`handle_snapshot`/`receive_loop`** (`renderer/src/socket.rs`). `run()` no longer holds the
  connection open and does nothing (`std::future::pending`); it builds a `Loader`, registers one
  live signal as the global `audio`, and loops reading `StateSnapshot` frames. Each frame goes
  through `handle_snapshot`: convert the payload to a Lua value, push it into the live signal, and
  re-evaluate `PROOF_OF_WIRING_SHELL` -- the real `Loader::evaluate` path, not a side path, so this
  proves the actual production API composes end to end rather than a parallel one built just for
  this test. `receive_loop` is generic over `AsyncRead`, matching `shared/src/framing.rs`'s own
  test style (`tokio::io::duplex` instead of a real `UnixListener`) so the frame-loop logic is
  testable without a filesystem socket. A frame that fails to decode ends the loop here, unlike
  `supervisor/src/socket.rs`'s inbound `CommandEnvelope` loop (which tolerates a bad frame and
  keeps reading): this connection has exactly one sender and one message shape, so a bad frame
  means the two sides desynced, not a stray malformed message worth shrugging off.

  On the Supervisor side, `main.rs`'s `audio_apps.recv()` arm builds a `StateSnapshot` from the
  same `Vec<AppStream>` snapshot it used to just log, and pushes it via
  `GenerationRegistry::send_to` -- the real destination Phase 6 deferred and ADR-0017 named as
  blocked on exactly this. `AppStream` needed a `Serialize` derive it didn't have; nothing else in
  `audio::mixer.rs` changed.

- **`renderer/src/main.rs`'s `mod lua;`** lost its blanket `#[allow(dead_code)]` -- the claim it
  carried ("this whole subtree has no production caller yet") is now false for most of the
  subtree. What's still genuinely uncalled outside tests got its own scoped allow instead, per
  `supervisor/src/socket.rs`'s `GenerationRegistry::send_to` precedent (fine-grained, not
  blanket): `marshal.rs`'s entire module (only `try_new_direct` calls into it, and that
  constructor itself has no caller yet either), plus `SignalKind::Direct` and
  `Signal::try_new_direct` specifically. `LoadOutput.surfaces` and `VirtualNode.properties`
  looked like they'd need the same treatment -- the derived `Debug` impl `run()`'s log line was
  going to lean on doesn't count as a "use" for dead-code analysis, per rustc's own note -- but
  the fix there was to make `run()`'s log line actually iterate `output.surfaces` and print each
  `.kind`/`.properties` directly instead of one opaque `{output:?}` dump. That's a real field read,
  not a suppression, and it produces a more useful log line besides.

Tested against (TDD, `/tdd` discipline, one seam per cycle): `Signal::new_live`/
`LiveSignalHandle::set` (a live signal reflects a value pushed after construction, not the value
frozen at `new_live`'s call time); `Loader::set_global` (a value registered before `evaluate` is
visible inside the evaluated script); `Loader::to_lua_value` (a JSON object's fields are readable
from Lua after conversion); `handle_snapshot` (a hand-built `StateSnapshot` produces a
`VirtualNode` whose `audio_apps` property matches the pushed payload); `receive_loop` (two frames
in sequence over a `tokio::io::duplex` pipe each update the live signal in turn, proving the second
frame isn't ignored); `AppStream`'s `Serialize` derive (field names on the wire match the struct's
own field names, unchanged).

Not automated: the literal acceptance test named in build-steps.md ("a real audio-mixer volume
change, made on the system, visibly reaches a `Signal:get()` call inside the Renderer process")
needs a running PipeWire daemon and a real playback stream, the same reason
`supervisor/src/audio/mixer.rs`'s own tests use recorded `pw-dump` output and this-process's own
pid instead of spawning real audio -- Phase 6's own precedent. `handle_snapshot`'s test fabricates
the `StateSnapshot` a real push would produce instead, at the same seam Phase 9's socket tests
fabricate a `CommandEnvelope` for.

Upgrade path, in order: (a) Phase 12's retained-scene transaction gives `LoadOutput` a real
consumer beyond a log line (item 6); (b) Phase 13's Watcher supplies a real `shell.lua` file
location and the reload trigger, replacing `PROOF_OF_WIRING_SHELL` (item 5) and is also the
natural place to build revision-based staleness rejection once more than one writer exists (item
2); (c) real generation-ID assignment (item 3) and reconnection (item 4) both wait on a later
phase's process-spawning wiring; (d) the `oblisk.*` tree (item 1) and other capabilities' pushes
(item 7) grow this same `set_global` mechanism outward, one capability at a time, once each has
something worth pushing; (e) write-command dispatch (item 8) is Phase 14+'s job per ADR-0021's own
upgrade path.

This does not contradict `docs/oblisk-idl-api-specs.md` §1-2, `build-steps.md`'s Phase 11 text,
ADR-0020, or ADR-0021: all describe or assume the target shape once the full signal tree,
staleness rejection, and retained-scene consumption exist, and none of that is built here. It also
does not invert Phase 9's listener/client roles -- the Supervisor is still the listener, the
Renderer still connects as client, matching Phase 11's own text.
