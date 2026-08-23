# Quickshell parity review

Reviewed: 2026-08-23. Scope is Wayland parity. X11 is out of scope.
Compositor-specific behavior belongs in optional adapters.

## Purpose

Use this file to compare the restart plan with Quickshell's module families.
It records required contracts and checks. It does not record code state or test
results.

## Parity map

| Area | Required contract | First check | Boundary |
| --- | --- | --- | --- |
| Retained scene | Explicit signals, direct components, bounded keyed repeaters, stable IDs, and child-first cleanup | Reorder, remove, replace, and failed-refresh tests | Rust owns nodes and leases. Lua returns descriptors. |
| Layout and paint | Intrinsic rows and columns, bounded Unicode text, panel fills, and path-only PNG/JPEG assets | Nested layout, overflow, text, and cache-limit tests | CPU SHM paint first. Add flex, SVG, themes, clipping, or GPU only with a measured caller. |
| Input | Bounded pointer and keyboard values, focus authorization, freeze behavior, and callback ordering | Input FIFO, focus, freeze, and callback-error tests | Rust owns Wayland input. Lua sees values. |
| Processes | Generation-scoped direct commands, bounded capture, timeout escalation, and process-group cleanup | Child timeout, descendant, capture, and reap tests | Rust owns children. Lua owns opaque handles. |
| Outputs | Per-output surface entries, configure size, scale, transform, hotplug, and frame gating | Output add/remove, resize, scale, transform, and first-frame tests | SCTK owns Wayland proxies. |
| Surface kinds | Panel first, then popup or overlay with independent configure, input, frame, and cleanup | Second surface topology under the same renderer trait | Keep product layout in Lua. |
| Notifications | Supervisor-owned bounded snapshot bridge, visible-ID gating, public authority only after routing ownership | Snapshot bounds, staged-feed, presentation, and command tests | No raw D-Bus objects enter Lua. |
| MPRIS | Rust-owned service feed with bounded rows, revisions, unavailable state, and validated commands | Startup, disconnect, stale revision, and shutdown tests | Renderer-local is acceptable until durable commands need another owner. |
| Desktop services | One adapter at a time for tray, audio, power, network, Bluetooth, workspaces, and clipboard | Headless contract plus real-session check per service | Services publish state. Lua chooses the view. |
| Lock and auth | Separate process owns lock protocol, PAM, greetd, and Polkit conversations | Disposable PAM account and compositor recovery check | Secrets never enter Lua or renderer IPC. |
| Wayland extras | Screen copy, idle, workspaces, and compositor APIs stay adapters | Protocol-specific test per compositor | No compatibility layer before a caller exists. |

## Quickshell module comparison

| Quickshell family | Oblisk plan | Rust boundary |
| --- | --- | --- |
| Core authoring and reload | Lua descriptors, explicit dependencies, retained Rust nodes, process-based reload | Supervisor owns generations. |
| IO and processes | Direct bounded commands first. Streams, sockets, and detached jobs need separate slices. | Rust owns pipes and process groups. |
| Panels and widgets | Lua composition over Rust surface, layout, input, and capability contracts | Rust owns Wayland objects. |
| Rendering and assets | CPU SHM paint, host-font text, bounded PNG/JPEG assets | Lua supplies names and values. Rust opens and decodes assets. |
| Notifications and media | Bounded notification bridge and MPRIS service feed | Rust owns service connections and revisions. |
| Lock and authentication | Separate durable process with data-only Lua theming | Authentication owner holds secrets. |
| Compositor APIs | Optional capability adapters | Each adapter owns its protocol objects. |

Quickshell's public module families are documented in its
[core](https://github.com/quickshell-mirror/quickshell/blob/master/src/core/module.md),
[IO](https://github.com/quickshell-mirror/quickshell/blob/master/src/io/module.md),
[Wayland](https://github.com/quickshell-mirror/quickshell/blob/master/src/wayland/module.md),
and [service](https://github.com/quickshell-mirror/quickshell/tree/master/src/services)
sources.

## Design constraints

- A candidate must remain unmapped until supervisor activation.
- Activation ACK grants commit permission. It does not prove presentation.
- A first frame or presentation feedback arms the health window.
- Only one control command may wait in the renderer's post-activation slot.
- Input stays in one bounded FIFO and loses focus during freeze or seat removal.
- Notification feeds are private and generation-scoped before durable routing
  exists.
- Output and surface state drives allocation. Never use a hard-coded diagnostic size
  after the dynamic surface slice begins.
- The watcher follows the successful dependency graph, including trusted include
  roots and parent directories for missing modules.
- Every optional service publishes an unavailable state instead of blocking the
  renderer.

## Acceptance checks

Before calling a slice ready, record:

1. The owner of each proxy, thread, child, cache, lease, and public endpoint.
2. Byte and count limits at each input and queue boundary.
3. Cleanup behavior during reload, freeze, callback failure, disconnect, and
   process exit.
4. One focused headless test.
5. A real compositor or service check where protocol behavior matters.

Do not add a crate without an immediate caller and runnable check. Prefer SCTK,
Calloop, direct protocol bindings, standard library facilities, and focused
crates in that order.

## Update rules

- Update this file when a contract or parity decision changes.
- Keep implementation detail in `plan.md` and the checklist in `build-steps.md`.
- Do not add speculative features to this ledger.
- Use acceptance labels only when the corresponding test and ownership path are
  recorded.
