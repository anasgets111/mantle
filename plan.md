# Oblisk shell framework

## Goal

Build a Wayland shell framework with Rust-owned platform code and Lua-owned
composition. Renderer generations run as separate processes so reload count does
not become a memory bound. The default shell uses the same API as user shells.

This is a clean-start design. It records intended contracts, not progress.
Noctalia and Quickshell provide comparison points. They do not define
the Rust or Lua object model.

## System boundaries

- **Engine.** Rust owns Wayland objects, retained nodes, layout, input, paint,
  animation, process cleanup, and capability transport.
- **Configuration.** One Lua VM describes a generation's scene. Lua receives
  bounded values and opaque handles. It never receives Wayland, D-Bus,
  PipeWire, authentication, or broker objects.
- **Capabilities.** Rust adapters publish bounded snapshots and accept
  validated commands. A failed optional service becomes an error state.
- **Default shell.** Lua components compose bars, launchers, notifications,
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
Do not design a plugin ABI, service registry, or UI toolkit before a concrete
caller needs one.

`ponytail:` Keep one active reload and one pending reload. This bounds
coordination to O(1). Add a queue only if measured editor behavior requires it.

`ponytail:` Start layout with intrinsic rows and columns. Add flex, grid, or a
layout crate only after a measured scene needs them.

## Supervisor

- Watch the configuration dependency graph with a 250 ms debounce.
- Assign generation IDs and own every renderer child handle.
- Stage one candidate while the active generation remains authoritative.
- Accept one pending reload. A newer request replaces it.
- Treat `READY` as successful scene and surface preparation, not presentation.
- Activate a candidate only after its configure barrier and scene checks pass.
- Keep input, exclusive zones, and public capability routing with the active
  generation only.
- On failure, terminate the candidate, wait, kill its process group if needed,
  and reap it.

## Renderer

- Own only one generation's Lua VM, scene, surfaces, timers, workers, and
  generation-local caches.
- Keep Wayland dispatch and proxy calls on the Calloop thread.
- Stage surfaces with null buffers, acknowledge configure, and wait for the
  supervisor's activation command before mapping them.
- Use a frame callback per surface as the redraw gate.
- Reconcile output hotplug through bounded surface specifications.
- Keep the renderer product-neutral. Product widgets belong in Lua.
- Exit after handoff. The process boundary reclaims renderer memory and maps.

## Lua configuration

`shell.lua` returns a declarative scene built with framework constructors.

### Runtime boundary

- Embed vendored PUC Lua 5.4 through `mlua`.
- Create one VM per renderer generation.
- Enable only the libraries required by the configuration language.
- Replace `require` with a rooted loader. Reject traversal, symlink escape,
  unsupported files, and source over the configured limit.
- Track every dependency path and content hash before committing a generation.
- Enforce limits for source bytes, VM heap, nodes, bindings, timers, and
  callback time.
- Run Lua on the renderer event-loop thread. Background work returns immutable
  bounded messages.
- Return validation failures as Lua errors. Never unwind a Rust panic through
  Lua.

These limits protect availability. They do not sandbox a user-owned process.

### Scene and components

- Constructors return typed descriptors. Reject unknown properties at
  construction time.
- The root declares an API version. Reject unsupported versions before creating
  surfaces.
- Construction has no external side effects. Defer timers, subscriptions,
  processes, and state-changing commands until activation.
- A component is a Lua function that returns constructor-built descriptors.
- Components may declare props, children, slots, events, and lifecycle hooks.
- Lua builds descriptors. Rust owns the retained scene and resource lifetimes.

### Reactivity and retained nodes

- `signal(value)` exposes `get` and `set`.
- `computed(dependencies, fn)` takes explicit dependencies. Do not infer reads
  through global tracking.
- A signal write marks only the named component instances dirty.
- Rust coalesces dirty work per event-loop tick, invokes affected builders, and
  diffs their descriptors against retained nodes.
- Stable node IDs survive same-generation diffs. A reload mounts a new
  generation and does not reuse old node IDs.
- Keyed repeaters match by bounded keys. Reorders preserve matched nodes. Removed
  keys unmount child-first. New keys mount only after validation.
- Lifecycle callbacks are protected calls. A failing callback loses its own
  lease and does not poison unrelated nodes.

`ponytail:` Use bounded string keys before adding `slotmap`. Upgrade when keys
need ownership independent of their immediate list.

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
- A failing callback disables and releases only its own resource.
- Renderer exit reclaims generation-local resources.
- A durable owner revokes generation leases when the private channel closes.

## Capabilities and durable state

Use this data flow:

```text
system backend -> Rust adapter -> bounded revisioned snapshot -> renderer -> Lua
Lua action -> validated command -> capability owner
```

A capability has a Rust owner, bounded snapshot, revision, availability state,
and validated command set. Lua receives IDs and values, never backend handles.

Start with MPRIS and notifications. Follow with power, network, Bluetooth,
audio, workspaces, and clipboard. Port one service through ownership, Lua API,
headless tests, and a real-session test before adding another.

The supervisor may own small stores and private bridges. Move a service to a
broker when it needs a public D-Bus name, durable authority, authenticated IPC,
or overlap-safe lifetime.

Services publish state. Lua decides how to display it. A widget built from
existing primitives must not require Rust.

## Lock and authentication

- A separate process owns the compositor session-lock protocol and PAM.
- Greetd and Polkit conversations stay outside Lua.
- Lua may style and position a data-only authentication scene. It never reads
  passwords or receives authentication callbacks.
- Passwords never enter snapshots, logs, IPC, or reload state. Use zeroizing
  buffers.
- Do not implement a cosmetic lock fallback. Define the supported compositor
  matrix and test with a disposable PAM account first.

## Reload transaction

1. Debounce and hash the dependency graph.
2. Start generation `N+1` without changing `N`.
3. Validate Lua, descriptors, dependencies, and limits.
4. Prepare every candidate surface with null buffers and configure handshakes.
5. Freeze `N`'s content commits and disable its input.
6. Send the nonce-bound activation command to `N+1`.
7. Wait for the candidate's first frame or presentation feedback.
8. Grant the candidate's generation lease and run the health window.
9. On failure, unmap `N+1` and restore `N`. On success, terminate and reap `N`.

Wayland does not provide an atomic cross-process surface handoff. Brief overlap
or a gap is allowed. Do not claim physical presentation from an activation ACK.

## Configuration failures

| Failure | Response |
| --- | --- |
| Empty or changing save | Debounce and retry from a stable snapshot. |
| Syntax, module, API, limit, or readiness error | Reject and reap the candidate. Keep the active generation. |
| No valid generation | Render a built-in Rust diagnostic. Do not execute user Lua. |
| Timer, binding, or event callback error | Disable that source and retain unrelated scene state. |
| Candidate crash during handoff | Roll back to the frozen generation. |
| Active renderer crash | Retry bounded times, then use the built-in safe shell. |
| Capability loss | Publish unavailable state. Keep unrelated UI running. |
| Lock-theme error | Use the last validated data-only lock scene. |

## Invariants

- One renderer accepts input and reserves exclusive space.
- One process owns each public D-Bus name, IPC socket, and durable job.
- Every frame, snapshot, and queue has a byte and count limit.
- No raw platform handle, secret, callback, or unbounded collection crosses
  into Lua.
- No old renderer PID survives a successful handoff.
- Capability failure does not block unrelated UI.
- The default shell uses only public framework APIs.
- A second shell must use a different topology without Rust changes.
- Every long-lived resource has a traceable owner and cleanup path.

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
| D-Bus | `zbus` | Add it with a concrete service owner and test. |
| Audio | `pipewire` | One owner thread for PipeWire objects. |
| Authentication | `pam-client` and `zeroize` | Keep secrets in the lock process. |
| Diagnostics | `tracing` | Use spans for build, diff, layout, paint, and handoff. |

Do not add `taffy`, `wgpu`, a broker, or an extension ABI to satisfy a list.
Add each when the next slice needs it.

## Delivery order

1. Supervisor, bounded protocol, one disposable renderer, and cleanup.
2. Restricted Lua VM, rooted loader, typed descriptors, and diagnostics.
3. Retained nodes, signals, keyed lists, intrinsic layout, text, input, timers,
   and direct processes.
4. Output topology, dynamic buffers, scale, transform, hotplug, and a second
   surface kind.
5. Stable node IDs, keyed cleanup, layout sizing, input routing, text editing,
   and animation scheduling.
6. MPRIS and notifications, then one service at a time.
7. A default Lua shell and a second Lua shell fixture.
8. Lock/auth, durable jobs, public IPC, and broker extraction when required.
9. Themes, assets, canvas, accessibility, compositor adapters, packaging, and
   long-run reload qualification.

Each step needs a focused test, an owner, bounded inputs and outputs, and a
manual compositor check when the platform is involved.

## Cleanup requirements

- Retain every child handle, process group, and exit waiter.
- Escalate termination from `SIGTERM` to `SIGKILL`, then reap.
- Bound source, frame, queue, snapshot, timer, process, and image memory.
- Keep caches generation-local unless a durable owner has an explicit limit.
- Run reload and fault-injection qualification after the implementation exists.
