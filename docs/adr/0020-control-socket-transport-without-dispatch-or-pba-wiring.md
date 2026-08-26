# Control-socket transport ships without dispatch, PBA wiring, or process.run streaming

Phase 9's title ("IPC Control-Socket Transport & Wire Framing") and its build-steps.md text
scope a real Unix control-socket transport and connection-identity handshake, explicitly
deferring everything downstream of "a frame arrived" to later phases. Matching that scope, this
phase does not build:

1. **The command-dispatch routing table** (`oblisk-idl-api-specs.md` § 3.2's ~30 write
   commands: `audio:set_volume`, `network:connect`, `bluetooth:pair`, and so on). The
   Supervisor's connection handler (`supervisor/src/socket.rs::handle_connection`) decodes
   every inbound frame as a `shared::CommandEnvelope` and forwards it, tagged with its sender's
   `generation_id`, to a single aggregated channel drained by `main.rs`'s `eprintln!` loop --
   the same ceiling `dbus::polkit`'s challenge channel and `audio::mixer`'s app-stream channel
   already hit in Phase 5/6. No capability exists yet to route `capability`/`action` fields to.

2. **`CandidateLink`'s real implementation.** `supervisor/src/reload.rs`'s `CandidateLink`
   trait (Phase 8) still has no production implementer. This phase's framing
   (`shared::framing::write_frame`/`read_frame`/`write_json_frame`/`read_json_frame`) is the
   primitive a real implementation would use, but wiring `push_state_snapshot`/
   `recv_ready_signal`/`send_activate_draw`/`recv_presentation_evidence` onto this transport is
   Phase 14's job, per ADR-0019's upgrade path item (a). This phase's `GenerationRegistry`
   tracks connections by `generation_id` precisely so that wiring has something to build on,
   but nothing here calls into `reload.rs` or vice versa.

3. **`process.run`'s line-streaming.** Phase 9's own text calls this out: stdout/stderr
   streaming from a spawned process to Lua (Phase 15) is "a different transport concern" from
   generic frame delivery, even though both eventually cross the same socket.

4. **Any handshake deadline.** `handle_connection` awaits the first frame
   (`ConnectionHandshake`) with no timeout -- a client that connects and never sends one blocks
   that connection's task forever (harmless: one idle tokio task, not the accept loop, which
   keeps accepting new connections). PBA-specific deadlines belong to Phase 14's
   `CandidateLink`, not this generic transport.

5. **Real generation-ID assignment.** `renderer/src/socket.rs` reads `OBLISK_GENERATION_ID`
   from the environment, defaulting to `0`, because nothing yet spawns a Renderer process with
   a real generation ID to hand it -- that lands once Phase 7/8's spawn primitives are wired to
   this transport (a later phase, downstream of Phase 14).

6. **Reconnection, backoff, or auth.** Unnecessary for a local `AF_UNIX` socket restricted by
   filesystem permissions; not requested by any doc.

Decision: `shared::framing` ships generic, transport-agnostic framing only -- a 4-byte
big-endian length prefix ahead of a JSON payload, generic over `AsyncRead`/`AsyncWrite` so both
a real `UnixStream` and an in-memory `tokio::io::duplex` pair exercise the same code path in
tests. It enforces `MAX_FRAME_LEN` (16 MiB) against the length prefix before allocating a
payload buffer, closing the obvious DoS a `u32` length prefix invites on a socket ADR-0005
already slates to carry secure textfield submissions. `shared::ConnectionHandshake` is the one
new wire type this phase adds: `{ generation_id: u32 }`, sent as the first frame on every
connection, before any other traffic.

`supervisor/src/socket.rs` binds `$XDG_RUNTIME_DIR/oblisk-shell.sock` (never `/tmp` -- world-
writable and unsuitable per Phase 9's text), clearing a stale socket file left by an unclean
prior shutdown before binding (undocumented in the specs; without it, `bind` fails with
`AddrInUse` on every restart after a crash). It accepts unboundedly many simultaneous
connections -- required because Generation `N` and Candidate `N+1` are both connected during a
swap (`CONTEXT.md`'s Candidate and Authoritative generation entries) -- spawning one task per
connection and registering each by `generation_id` in a `GenerationRegistry` so a later caller
can address a specific generation's connection directly (`GenerationRegistry::send_to`) instead
of assuming exactly one peer.

`renderer/src/socket.rs` connects as the client, on its own OS thread with a dedicated
current-thread tokio runtime -- the main thread is occupied by `wayland::run()`'s blocking
dispatch loop, and `renderer/Cargo.toml`'s `rt`/`net`/`macros` tokio features (declared since
scaffolding, unused until now) were staged for exactly this one dedicated I/O task, matching
`text::shaping`'s own dedicated-worker-thread reasoning. After the handshake it holds the
connection open indefinitely (`std::future::pending`) -- reading a real payload back is Phase
11's job, once the Supervisor has a `StateSnapshot` worth pushing.

Tested against real I/O throughout (TDD, `/tdd` discipline, matching `reload.rs`'s and
`process::mod`'s own precedent of testing against real processes instead of fakes): framing
round-trips over `tokio::io::duplex` (no filesystem); the Supervisor's listener binds and
accepts over a real `UnixListener` in a `tempfile::tempdir()` path, with two simultaneous real
client connections registered by distinct `generation_id`s and independently addressable; the
Renderer's client connects to a real listener and its handshake decodes correctly on the other
end. Both `shared/Cargo.toml` and `supervisor/Cargo.toml`/`renderer/Cargo.toml` gained a
`tempfile` dev-dependency (already present transitively in `Cargo.lock`) for the filesystem-
backed listener tests; `shared`'s already-declared, previously-unused `thiserror` dependency
gets its first real job as `shared::framing::FramingError`.

Upgrade path, in order: (a) Phase 11 gives the Supervisor a real `StateSnapshot` to push
through `GenerationRegistry::send_to` and the Renderer's client something to read back,
proving both directions of this transport are wired correctly; (b) Phase 14 gives
`CandidateLink` a production implementation built on `shared::framing`, replacing `reload.rs`'s
fake test double; (c) a real capability layer (Phase 16 and beyond) gives the command-dispatch
routing table (item 1) something to route `CommandEnvelope`s to, replacing the current
decode-and-log ceiling; (d) Phase 15 gives `process.run` its own line-streaming transport
concern (item 3), independent of this one; (e) once Phase 7/8's spawn primitives are wired to
this transport, the Renderer's real generation ID replaces the `OBLISK_GENERATION_ID`
env-var stand-in (item 5).

This does not contradict `docs/oblisk-idl-api-specs.md` § 3.1/§ 7, `build-steps.md`'s Phase 9
text, or ADR-0019: all describe or assume the target shape once dispatch, `CandidateLink`, and
real generation-ID assignment exist, and none of that is built here. It also does not
contradict ADR-0018's `process.run` deferral: this phase's transport is a prerequisite for
`process.run`'s eventual line-streaming, not an implementation of it.
