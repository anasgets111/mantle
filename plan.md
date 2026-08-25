# Oblisk shell framework

## Goal

Build a Wayland shell framework with Rust-owned platform code and Lua-owned
composition. Renderer generations run as separate processes so reload count does
not become a memory bound. There is no default shell product; a test fixture in
this repo exercises the public interface, and users write their own `shell.lua`.

This is a clean-start design. It records intended contracts, not progress.
Noctalia and Quickshell provide comparison points. They do not define
the Rust or Lua object model.

## Detailed specifications

This file and [parity.md](parity.md) record decisions and required contracts
at the architecture level. Concrete, compiler-checkable detail lives in the
specs below and is authoritative wherever it is more specific than this file:
Rust/Lua type marshalling, the `oblisk.*` signal schema, capability command
JSON envelopes, D-Bus interfaces and object paths, wire formats, and
scene-node property tables.

- [oblisk-idl-api-specs-v9.md](docs/oblisk-idl-api-specs-v9.md): Rust-Lua
  marshalling, the `oblisk.*` signal tree, the command envelope schema, the
  lazy activation handshake, and the AST node property tables.
- [oblisk-hardware-event-pipeline-v4.md](docs/oblisk-hardware-event-pipeline-v4.md):
  zero-polling hardware adapters (keyboard, webcam, updates, input overlay,
  cava FFT, output hotplug) and their kernel/D-Bus event sources.
- [oblisk-supervisor-services-dbus-v9.md](docs/oblisk-supervisor-services-dbus-v9.md):
  durable Supervisor-owned D-Bus services (notifications, tray, MPRIS, Polkit,
  launcher indexer, clipboard persistence, idle, wallpaper, network,
  Bluetooth, power, workspaces, rescue mode).
- [oblisk-layout-engine-geometry.md](docs/oblisk-layout-engine-geometry.md):
  the one-pass constraint/size/position layout algorithm, keyed
  reconciliation, fractional scaling, and damage tracking.
- [oblisk-reference-fixtures-v8.md](docs/oblisk-reference-fixtures-v8.md):
  worked example `shell.lua` configurations exercising the public interface.

## Locked decisions

| Topic | Decision |
| --- | --- |
| Rendering | femtovg on EGL/GLES3. Blur comes from the compositor via transparent surfaces, not from us. The headless renderer is a process fixture, not a paint adapter. CPU SHM is the renderer trait's test adapter (llvmpipe/CI); EGL is the production adapter. |
| Reload authority | [ADR 0001](docs/adr/0001-presentation-before-authority.md). Per-output presentation evidence controls the handoff. |
| Dependency watching | [ADR 0002](docs/adr/0002-shared-dependency-snapshot.md). One snapshot module serves loading and watching. |
| Capability command guards | [ADR 0003](docs/adr/0003-generation-and-revision-command-guards.md). State-dependent commands carry generation ID and snapshot revision. |
| Capability activation | [ADR 0005](docs/adr/0005-lazy-capability-claiming.md). Singleton system resources (D-Bus names, PolicyKit agent, background surfaces, indexer threads) claim lazily on first `require()`, never at Supervisor boot. |
| MPRIS placement | [ADR 0004](docs/adr/0004-mpris-durable-supervisor-ownership.md). Durable Supervisor ownership from the first implementation, so reload does not drop D-Bus player subscriptions. |
| Scene ownership | A private Rust retained-scene module owns the descriptor-to-commit transaction, parent tree, node IDs, leases, and child-first cleanup. Lua holds weak proxies. Retainable locks keep nodes alive for exit animations. |
| Reactivity | `bind(signal)` supplies properties and callbacks supply behavior. Signals mark dirty builders; the retained-scene module resolves writes transactionally. A diff-time check catches imperative writes to bound properties; log once and drop. |
| Input | Engine hit-tests retained node rects and delivers semantic events (`on_click`, `on_scroll`, `on_key`). Focus and pointer grabs are engine-owned leases. Popup placement strategies are engine work. |
| Animation | Engine-clocked tweens: retarget mid-flight and completion callback. Behavior-style declarations deferred until a fixture demands them. Springs deferred until a widget needs them. Animated writes go through the same dirty-marking path as bindings. |
| Text | TextField and TextArea are engine types with IME (`wp-text-input-v3`) and clipboard (`data-control`). Documented exception to the product-widgets-in-Lua rule. |
| Compositors | Adapter seam behind capability signals. Niri and Hyprland first-class, generic Wayland protocols as the floor. Feature detection via `capability:has("feature")`; missing features are unavailable state, not nil. Session actions use logind directly, outside the compositor seam. |
| Product shape | Quickshell model: no default shell product. A test fixture Lua file lives in this repo and uses only the public interface. All capabilities ship eventually, one at a time through the full pipeline. |
| IPC | Supervisor owns built-in verbs over a Unix socket in `$XDG_RUNTIME_DIR`: reload, status, reload-last-good, shutdown. Renderer exposes one generic `on_ipc(msg)` handler for user-defined actions. `msg` is a bounded table: verb is a non-empty string up to 64 bytes, args is a flat table up to 16 entries with string/number/bool values. The supervisor validates shape before delivery. Pass-through verbs are default-deny; configs opt in per verb, with the peer PID attached via SO_PEERCRED. D-Bus names deferred to the broker stage. |
| Tray | Engine owns StatusNotifierItem watcher, host, icon decoding, and menu data. The watcher D-Bus name lives on the durable side so reloads do not drop tray items. Menu trees cross to Lua as revisioned bounded snapshots through the same capability envelope as every other capability, never as live handles. Lua receives item objects (icon handle, tooltip, title) and builds its own drawer UI. |
| Theming | Palette capability with a fixed M3-style role vocabulary and wallpaper-derived palettes. Template stamping into other apps' configs is deferred until a real consumer exists; when implemented, it runs on the durable side with post-hooks. Noctalia's MIT-licensed templates are reference material. Community fetch is deferred. |
| Errors | Supervisor-owned Rust-rendered error banner fed by three sources: rejected candidates, rate-limited callback failures, and capability hard-failures. It clears on clean activation. Rejected-reload events also reach the authoritative generation's `on_ipc` so healthy configurations can display them. |
| Testing | Pure-logic units (reload transitions, dependency snapshots, retained-scene commits, leases, persist registry), fixture-driven integration tests for the Lua VM and loader, fake renderer processes for supervisor protocol tests, one headless-Wayland boot smoke in CI. No golden-frame screenshot diffs; assert on protocol events and exit codes. |

### State carry-over across generations

- Builders declare persistent values by name: `persist("panel.open", signal)`.
- Values are bounded serializable types only: numbers, strings, bools, flat tables. No handles, functions, or nodes.
- The supervisor copies declared values from the old VM to the new VM during staging, before the new scene builds.
- Name mismatches resolve by drop-or-default, never error. Renaming a key is a documented break.

`ponytail:` No schema or migration system. Names are the contract. Add one only when a real config rename forces it.

## System ownership

- **Engine.** Rust owns Wayland objects, retained nodes, layout, input, paint,
  animation, process cleanup, and capability transport.
- **Configuration.** One Lua VM describes a generation's scene. Lua receives
  bounded values and opaque handles. It never receives Wayland, D-Bus,
  PipeWire, authentication, or broker objects.
- **Capabilities.** Rust adapters publish bounded snapshots and accept
  validated commands. An unavailable optional capability does not block
  unrelated UI.
- **Default shell.** Lua builders compose bars, launchers, notifications,
  OSDs, control centers, and widgets. Rust does not encode those product
  layouts.

The process model is:

```text
file watcher -> supervisor -> renderer generation
                         \-> durable capability owner, when required
lock request ----------------> lock/auth process
```

The supervisor owns small durable stores first. Add a broker process only when
overlapping generations, public authority, or lifetime requirements demand it.
Do not design a plugin ABI, capability registry, or UI toolkit before a concrete
caller needs one.

`ponytail:` Keep one active reload and one pending reload. This bounds
coordination to O(1). Add a queue only if measured editor behavior requires it.

`ponytail:` Start layout with intrinsic rows and columns. Add flex, grid, or a
layout crate only after a measured scene needs them.

## Deep module seams

These are private Rust modules with small interfaces. Do not expose their
internal state machines or adapters to Lua.

- **Reload transaction.** The supervisor feeds reload requests, renderer
  milestones, presentation evidence, process exits, and deadlines into one
  state machine. It emits effects for staging, activation, freeze, rollback,
  and cleanup. The authoritative generation stays authoritative until
  presentation evidence arrives.
- **Dependency snapshot.** The loader and watcher use one filesystem module to
  resolve the rooted graph, open each file once, record inode and hash from that
  descriptor, and return bounded watch roots. Before the first successful
  snapshot, the supervisor uses cold-start roots. Afterwards, it watches only
  the last successful snapshot.
- **Retained scene.** One transaction owns descriptor validation, dirty refresh,
  keyed identity, write ordering, node leases, event IDs, and child-first
  cleanup. Signals, input, timers, and animations submit writes through it.
- **Capability authority.** One module owns snapshot bounds, revisions,
  availability, generation authorization, stale-command rejection, and lease
  revocation. Backend adapters translate state and validate backend commands.
- **Renderer paint.** The renderer trait has two paint adapters, SHM for
  headless Wayland tests and EGL for production. The phase-one headless process
  fixture has no paint implementation.

The deletion test is explicit. Removing the first four modules would scatter
their invariants across callers. Removing a separate headless paint module
removes code without removing a requirement, so it does not exist.

## Supervisor

- Feed the configured cold-start roots, or the last successful dependency
  snapshot once one exists, to a 250 ms debounce watcher.
- Assign generation IDs and own every renderer child handle.
- Stage one candidate without changing authority.
- Accept one pending reload. A newer request replaces it.
- Treat `READY` as successful scene and surface preparation, not presentation.
- Activate a candidate only after its configure barrier and scene checks pass.
- Keep input, exclusive zones, and public capability routing with the
  authoritative generation only.
- Allow only one control command to wait in the renderer's post-activation slot.
- Run the private reload transaction. It owns activation state, presentation
  evidence, freeze ordering, rollback, deadlines, and renderer cleanup.

## Renderer

- Own only one generation's Lua VM, scene, surfaces, timers, workers, and
  generation-local caches.
- Keep Wayland dispatch and proxy calls on the Calloop thread.
- Stage surfaces with null buffers, acknowledge configure, and wait for the
  supervisor's activation command before mapping them.
- Use a frame callback per surface as the redraw gate.
- Reconcile output hotplug through bounded surface specifications.
- Route pointer and keyboard events through one bounded FIFO. Drop focus on
  freeze or seat removal. Accept keyboard input only for the authoritative,
  authorized, unfrozen generation.
- Drive allocation from output and surface state. Do not use a hard-coded
  diagnostic size once dynamic surfaces exist.
- Use SHM for the headless Wayland test adapter and EGL for production paint.
- The phase-one headless process fixture exercises control and cleanup only; it
  does not create a second paint adapter.
- Keep the renderer product-neutral. Product widgets belong in Lua.
- Exit after handoff. Process exit reclaims generation-local memory and mappings.

## Lua configuration

`shell.lua` returns a declarative scene built with framework constructors.

### Runtime seam

- Embed vendored PUC Lua 5.4 through `mlua`.
- Create one VM per renderer generation.
- Enable only the libraries required by the configuration language.
- Replace `require` with the rooted dependency snapshot module. Reject
  traversal, symlink escape, unsupported files, and source over the configured
  limit.
- Each capture opens every file once, records its inode and hash from that
  descriptor, and returns the bounded dependency graph. Verification re-captures
  expected files through the same module before commit. Before a successful
  snapshot exists, the watcher uses the configured cold-start roots; afterwards
  it consumes watch roots from the last successful snapshot.
- Enforce limits for source bytes, VM heap, nodes, bindings, timers, and
  callback time.
- Run Lua on the renderer event-loop thread. Background work returns immutable
  bounded messages.
- Return validation failures as Lua errors. Never unwind a Rust panic through
  Lua.

These limits protect availability. They do not sandbox a user-owned process.

### Scene and builders

- Constructors return typed descriptors. Reject unknown properties at
  construction time.
- The root declares an interface version. Reject unsupported versions before creating
  surfaces.
- Construction has no external side effects. Defer timers, subscriptions,
  processes, and state-changing commands until activation.
- A builder is a plain Lua function that returns constructor-built descriptors.
  No builder protocol or inheritance machinery; composition is
  function composition. Add memoization or slots only when a real config needs
  them.
- Lua builds descriptors. Rust owns the retained scene and resource lifetimes.

#### Constructor interface

One core, thin sugar, generated schemas:

```lua
node(kind, props)          -- core: children is a prop; pure data out
bind(signal)               -- sentinel any prop accepts; Rust subscribes
list(keyfn, sig, itemfn)   -- keyed repeater as a reserved kind
panel(props), row(props), column(props), text(str, props),
icon(name, props), button(fn, props)  -- sugar over node(), defaults included
```

- Sugar constructors are generated from the Rust schema. One source of truth;
  no hand-written validation to drift. Positional args capped at one.
- Descriptors are plain tables (`kind`, `props`): dumpable, snapshot-testable,
  fuzzable against the differ.
- The `raw` kind name is reserved for a future imperative escape hatch
  (animations, focus). Do not implement it until a caller needs it.
- Rejected alternative: a builder protocol with props/slots/inheritance and
  plugin node registration. Machinery for configs nobody has written yet; one
  implementation is not a seam.
- Property writes have one path: sources (bindings, callbacks, animations)
enqueue writes, the engine resolves them last-writer-wins per tick. An active
animation suspends the binding on that property while it runs; the binding
resumes after. Document once, enforce at diff time.

### Reactivity and retained nodes

- `signal(value)` exposes `get` and `set`.
- `computed(dependencies, fn)` takes explicit dependencies. Do not infer reads
  through global tracking.
- A signal write marks only the named builder instances dirty.
- The retained-scene module coalesces dirty work per event-loop tick, invokes
  affected builders, validates their descriptors, and commits one scene diff.
- Stable node IDs survive same-generation diffs. A reload mounts a new
  generation and does not reuse old node IDs.
- Keyed repeaters match by bounded keys inside the same transaction. Reorders
  preserve matched nodes. Removed keys unmount child-first. New keys mount only
  after validation.
- Failed refreshes retain the previous subtree. Binding, callback, and
  animated writes enter one queue and resolve last-writer-wins per tick.
- Lifecycle callbacks are protected calls. A failing callback loses its own
  lease and does not affect unrelated nodes.

`ponytail:` Use a bounded linear scan for string keys before adding an index.
This is O(n²) per keyed refresh in the worst case. Upgrade when measured list
size or key ownership requires it.

### Timers and processes

- `timer.after` and `timer.every` return cancellable native handles.
- Timer callbacks run on the renderer thread and have a callback deadline.
- `process.run` starts a generation-scoped direct child in its own process
  group. Capture bytes and process count are bounded.
- Timeout and cancellation send `SIGTERM`, wait, then send `SIGKILL` and reap.
- Detached jobs are separate supervisor requests. They require an explicit
  manifest for durable ownership and never inherit Lua handles.
- `PR_SET_PDEATHSIG` protects direct children. It does not clean daemonized
  descendants. Use a supervisor-owned process group or cgroup when hard cleanup
  is required.

### Ownership and cleanup

Every acquirable resource uses a Rust-owned lease identified by generation and
owner. Lua sees only an opaque handle.

- Removing a node unmounts children before the parent and releases its leases.
- Reload releases the old generation after the health window or on rollback.
  Every validated command carries its sender's generation ID; owners reject
  non-authoritative IDs before a dying generation's surfaces are unmapped.
- A failing callback disables and releases only its own resource.
- Renderer exit reclaims generation-local resources.
- A durable owner revokes generation leases when the private channel closes.

## Capabilities and durable state

Use this data flow:

```text
system backend -> Rust adapter -> bounded revisioned snapshot -> renderer -> Lua
Lua action -> validated command -> capability owner
```

A capability authority has a Rust owner, bounded snapshot, revision,
availability state, generation authorization, and validated command envelope.
Every state-dependent command carries the sender's generation ID and expected
snapshot revision. The authority rejects stale generation IDs or revisions
before backend adapters validate command meaning. State-independent commands
may omit the revision when their contract does not depend on a snapshot.
Lua receives IDs and values, never backend handles.

Start with MPRIS and notifications. MPRIS is durable Supervisor-owned from the
first implementation ([ADR 0004](docs/adr/0004-mpris-durable-supervisor-ownership.md)).
The Supervisor discovers `org.mpris.MediaPlayer2.*` names and caches
normalized player state independent of any renderer generation, so reload
reconnects nothing and the new generation's first frame carries correct media
info. Then port power, network, Bluetooth, audio, workspaces, and clipboard
one capability at a time through ownership, Lua interface, headless tests, and
a real-session test.

The supervisor may own small stores and private bridges. Move a capability to a
broker when it needs a public D-Bus name, durable authority, authenticated IPC,
or overlap-safe lifetime. Do not make the broker part of the capability
interface.

Notification and tray D-Bus ownership follow
[ADR 0005](docs/adr/0005-lazy-capability-claiming.md): claimed only on first
use, never at boot.

Durable-side owners are the StatusNotifierItem watcher name, the MPRIS player
cache, the error banner surface, and public D-Bus names claimed on first use.
Theme stamping joins this list when a real consumer requires it. None of
these belong to a generation; reloads must not drop them.

Capabilities publish state. Lua decides how to display it. A widget built from
existing engine types must not require Rust.

Each capability supports feature detection: `capability:has("feature")` returns
whether the active adapter provides it (for example Hyprland special
workspaces). Missing features are unavailable state for widgets to degrade on,
not nil fields. `has()` is static per adapter; availability is runtime.
Widgets check `has()` once at build time and availability on every snapshot.

## Lock and authentication

- A separate process owns the compositor session-lock protocol, greetd, and
  PAM. Lua may style and position a data-only lock scene. It never reads
  passwords or receives authentication callbacks.
- Polkit is not part of that process
  ([ADR 0006](docs/adr/0006-polkit-in-supervisor.md)): the Supervisor runs the
  PolicyKit agent as an ordinary capability. Lua receives challenge metadata
  (`action_id`, `message`, `cookie`) and renders its own dialog; only the
  password field is a Rust-native `TextField` that keystrokes never reach Lua
  through.
- Passwords never enter snapshots, logs, IPC, or reload state, in either
  surface. Use zeroizing buffers.
- Do not implement a cosmetic lock fallback. Define the supported compositor
  matrix and test with a disposable PAM account first.

## Input overlay

[ADR 0007](docs/adr/0007-input-overlay-evdev-boundary.md). A global
key-visualizer for streaming needs input from whatever window has focus, not
just Oblisk surfaces, so it reads raw `evdev` gated by `input`-group
membership instead of the engine's normal per-surface input pipeline. It
claims lazily on `require("oblisk.input_overlay")`, per
[ADR 0005](docs/adr/0005-lazy-capability-claiming.md), and never opens a
device node otherwise. Symbols are ephemeral: a bounded, debounced ring
buffer, never logged or persisted. Treat `input_overlay.keys` as sensitive by
default.

## Reload transaction

1. Ask the dependency snapshot module for a stable graph and watch roots.
   Before the first successful snapshot, watch the configured entry file,
   trusted include roots, and parent directories needed for missing modules.
   After success, watch only the last successful snapshot.
2. Start generation `N+1` without changing `N`.
3. Validate Lua, descriptors, dependencies, and limits.
4. Prepare every candidate surface with null buffers and configure handshakes.
5. Send the nonce-bound activation command to `N+1`. The nonce includes a
   topology epoch; a renderer ACKs with the epoch it actually mapped. A
   mismatch restarts staging with the new topology instead of activating stale
   state.
6. Wait for every targeted output's first frame or presentation feedback. An
   activation ACK does not prove presentation. Untargeted outputs do not block
   the handoff.
7. Freeze `N`'s content commits and disable its input only after every targeted
   output has presentation evidence. Never freeze `N` while the candidate is
   still unproven; if a targeted output stalls, reap the candidate and keep `N`
   fully live.
8. Grant the candidate's generation lease and run the health window, armed by a
   wall-clock deadline independent of frame callbacks (an occluded output must
   not stall it).
9. On failure, unmap `N+1` and restore `N`. On success, terminate and reap `N`.

The supervisor's private reload transaction is the only module that decides
which generation is authoritative. Its input is bounded events. Its output is
bounded effects. Renderer protocol, presentation feedback, and process-group
cleanup are adapters behind that seam.

Wayland does not provide an atomic cross-process surface handoff. Brief overlap
or a gap is allowed. Do not claim physical presentation from an activation ACK.

## Configuration failures

| Failure | Response |
| --- | --- |
| Empty or changing save | Debounce and retry from a stable snapshot. |
| Syntax, module, interface, limit, or readiness error | Reject and reap the candidate. Keep the authoritative generation. |
| No valid generation | Render a built-in Rust diagnostic. Do not execute user Lua. |
| Timer, binding, or event callback error | Disable that source and retain unrelated scene state. |
| Candidate crash during handoff | Roll back to the frozen generation. |
| Authoritative generation crash | Retry bounded times, then use the built-in safe shell. |
| Capability loss | Publish unavailable state. Keep unrelated UI running. |
| Lock-theme error | Use the last validated data-only lock scene. |
| Rejected reload | Supervisor shows the error banner and sends a `reload-rejected` event to the authoritative generation's `on_ipc`. |
| Computed cycle | Depth cap of 8 per tick. Exceeding it marks the chain errored and drops the write; the source signal stays live. |

## Invariants

- One renderer accepts input and reserves exclusive space.
- One process owns each public D-Bus name, IPC socket, and durable job.
- Every frame, snapshot, and queue has a byte and count limit.
- No raw platform handle, secret, callback, or unbounded collection crosses
  into Lua.
- No old renderer PID survives a successful handoff.
- Capability failure does not block unrelated UI.
- The capability authority rejects stale generation IDs and expected snapshot
  revisions before backend command validation.
- The default shell uses only the public framework interface.
- A second shell must use a different topology without Rust changes.
- Every long-lived resource has a traceable owner and cleanup path.
- Build the test fixture early against the constructor interface. Treat fixture
  friction as an interface bug. Do not add Rust helpers for fixture widgets.
- Periodically write a small shell from scratch without reading the fixture. If
  it requires Rust-specific helpers, fix the interface.

## Dependencies

Prefer the standard library, existing workspace dependencies, direct protocol
bindings, then focused crates. Add a dependency only when it has an immediate
caller, an ownership decision, and a runnable check.

| Need | Candidate | Rule |
| --- | --- | --- |
| Wayland and layer shell | SCTK and `wayland-client` | Keep proxy ownership in Rust. |
| Event loop | `calloop` | Do not add Tokio to the renderer without a measured need. |
| Lua | `mlua` with vendored Lua 5.4 | One bounded VM per generation. |
| Text and images | `cosmic-text` and `image` | Bound source bytes, dimensions, and decoded pixels. |
| D-Bus | `zbus` | Add it with a concrete capability owner and test. |
| Audio | `pipewire` | One owner thread for PipeWire objects. |
| Authentication | `pam-client` and `zeroize` | Keep secrets in the lock process. |
| Diagnostics | `tracing` | Use spans for build, diff, layout, paint, and handoff. |
| Rendering | `femtovg` on EGL/GLES3 | Reconsider the renderer only after a caller exposes a measured need. |
| Headless CI smoke | wlroots headless or niri headless mode | One boot, one frame, one reload, clean exit. Assert on protocol events, not pixels. |

Do not add `taffy`, `wgpu`, a broker, or an extension ABI to satisfy a list.
Add each when the next slice needs it.

## IPC protocol

- Unix socket at `$XDG_RUNTIME_DIR/oblisk.sock`, same-user only, bounded messages.
- Supervisor verbs: `reload`, `status`, `reload-last-good`, `shutdown`. These
  work even when the authoritative generation is broken.
  `reload-last-good` re-runs the most recent config snapshot that passed
  validation.
- `oblisk msg <verb>` is the CLI. A generic pass-through forwards everything
  else to the authoritative generation's `on_ipc(msg)` handler.
- Public D-Bus names wait for the broker stage.

## Delivery order

Implementation order, step contracts, and acceptance criteria live in
[build-steps.md](build-steps.md).

## Cleanup requirements

- Retain every child handle, process group, and exit waiter.
- Escalate termination from `SIGTERM` to `SIGKILL`, then reap.
- Bound source, frame, queue, snapshot, timer, process, and image memory.
- Keep caches generation-local unless a durable owner has an explicit limit.
- Keep reload transitions in the supervisor transaction and backend-specific
  logic in adapters.
- Run reload and fault-injection qualification after the implementation exists.
