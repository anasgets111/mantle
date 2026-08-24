# Parity review: Quickshell and Noctalia

Reviewed: 2026-08-24. Scope is Wayland parity. X11 is out of scope.
Compositor-specific behavior belongs in optional adapters.

## Purpose

Use this file to compare the restart plan with Quickshell's module families and
Noctalia's feature set. It records required contracts and checks. It does not
record code state or test results.

## Reference behavior

| | Quickshell | Noctalia |
| --- | --- | --- |
| Model | Framework: QML types, users compose everything | Product: complete shell, users configure TOML, extend with Luau plugins |
| Reload | Rebuild QML tree in-process, state transfer by `reloadableId` | Patch live process through typed schema engine |
| Rendering | Qt scene graph | Hand-rolled scene graph on GLES2 |
| Text/IME | Qt (text-input-v3 inherited) | Pango/Cairo; no visible IME support |
| Tray | Engine service module, user builds UI | Full engine implementation incl. drawer |
| Theming | None; users hardcode | Wallpaper-to-palette via Material Color Utilities, template stamping into other apps' configs |
| Errors | On-screen overlay with error text and file:line | Not documented as a feature |
| IPC | None shipped; users build sockets | `noctalia msg` CLI plus D-Bus debug service |
| Animations | Qt animations, Behavior pattern, springs available | Engine animation manager, config-level speed policy |
| Compositors | Per-compositor modules (Hyprland, I3) | Niri/Hyprland/Sway adapters plus generic protocol floor |

Oblisk takes the framework model from Quickshell, the reload safety of its own
process-per-generation design, and theming scope from Noctalia.

## Parity map

| Area | Required contract | First check | Boundary |
| --- | --- | --- | --- |
| Retained scene | Explicit signals, direct components, bounded keyed repeaters, stable IDs, and child-first cleanup | Reorder, remove, replace, and failed-refresh tests | Rust owns nodes and leases. Lua returns descriptors. |
| Layout and paint | Intrinsic rows and columns, bounded Unicode text, panel fills, and path-only PNG/JPEG assets | Nested layout, overflow, text, and cache-limit tests | femtovg on EGL defines the renderer trait. CPU SHM is its test adapter (llvmpipe/CI), not a second maintained implementation. Add flex, SVG, or clipping only with a measured caller. |
| Input | Bounded pointer and keyboard values, focus authorization, freeze behavior, and callback ordering | Input FIFO, focus, freeze, and callback-error tests | Rust owns Wayland input. Lua sees values. |
| Processes | Generation-scoped direct commands, bounded capture, timeout escalation, and process-group cleanup | Child timeout, descendant, capture, and reap tests | Rust owns children. Lua owns opaque handles. |
| Outputs | Per-output surface entries, configure size, scale, transform, hotplug, and frame gating | Output add/remove, resize, scale, transform, and first-frame tests | SCTK owns Wayland proxies. |
| Surface kinds | Panel first, then popups (deferred to phase 7 where the launcher and notification center become their first callers); overlays wait for a fixture that needs one | Second surface topology under the same renderer trait | Keep product layout in Lua. |
| Notifications | Supervisor-owned bounded snapshot bridge, visible-ID gating, public authority only after routing ownership | Snapshot bounds, staged-feed, presentation, and command tests | No raw D-Bus objects enter Lua. |
| MPRIS | Rust-owned service feed with bounded rows, revisions, unavailable state, and validated commands | Startup, disconnect, stale revision, and shutdown tests | Renderer-local is acceptable until durable commands need another owner. |
| Desktop services | One adapter at a time for tray, audio, power, network, Bluetooth, workspaces, and clipboard; feature detection per capability | Headless contract plus real-session check per service | Services publish state. Lua chooses the view. |
| Theming | Palette capability with fixed role vocabulary, wallpaper derivation, durable-side template stamping | Palette signal drives widget bindings; template post-hook test | Stamping never runs inside a renderer generation. |
| Error surfacing | Supervisor-owned Rust banner plus `reload-rejected` events to `on_ipc` | Rejected-candidate and callback-failure tests | The banner survives when every generation is broken. |
| IPC | Supervisor socket verbs plus generic `on_ipc` pass-through | Malformed message and dead-generation tests | Built-in verbs work without a valid generation. |
| Lock and auth | Separate process owns lock protocol, PAM, greetd, and Polkit conversations | Disposable PAM account and compositor recovery check | Secrets never enter Lua or renderer IPC. |
| Compositor APIs | Optional capability adapters; Niri and Hyprland first-class, generic protocols as floor | Protocol-specific test per compositor | No compatibility layer before a caller exists. |

## Quickshell and Noctalia module comparison

| Reference family | Oblisk plan | Rust boundary |
| --- | --- | --- |
| Core authoring and reload (Q: QML tree, N: schema patch) | Lua descriptors, explicit dependencies, retained Rust nodes, process-based reload | Supervisor owns generations. |
| IO and processes (Q) | Direct bounded commands first. Streams, sockets, and detached jobs need separate slices. | Rust owns pipes and process groups. |
| Panels and widgets (both) | Lua composition over Rust surface, layout, input, and capability contracts; TextField/TextArea as engine exception | Rust owns Wayland objects. |
| Rendering and assets (Q: Qt, N: GLES scene graph) | femtovg paint, cosmic-text shaping, bounded PNG/JPEG assets | Lua supplies names and values. Rust opens and decodes assets. |
| Notifications and media (both) | Bounded notification bridge and MPRIS service feed | Rust owns service connections and revisions. |
| Lock and authentication (both) | Separate durable process with data-only Lua theming (N: session-lock + PAM proven at scale) | Authentication owner holds secrets. |
| Tray (N: full engine, Q: service module) | Engine watcher/host/menu data on the durable side; Lua builds the drawer | Watcher name outlives generations. |
| Theming (N only) | Palette capability, wallpaper derivation, template stamping with post-hooks | Stamping runs on the durable side. |
| IPC (N: `noctalia msg`, Q: none) | Supervisor verbs plus `on_ipc` pass-through | Built-in verbs survive a broken generation. |
| Compositor APIs (both) | Optional capability adapters | Each adapter owns its protocol objects. |

Quickshell's public module families are documented in its
[core](https://github.com/quickshell-mirror/quickshell/blob/master/src/core/module.md),
[IO](https://github.com/quickshell-mirror/quickshell/blob/master/src/io/module.md),
[Wayland](https://github.com/quickshell-mirror/quickshell/blob/master/src/wayland/module.md),
and [service](https://github.com/quickshell-mirror/quickshell/tree/master/src/services)
sources. Noctalia's stack, layout, and config reference live in its
[CONTRIBUTING.md](https://github.com/noctalia-dev/noctalia/blob/main/CONTRIBUTING.md)
and [example.toml](https://github.com/noctalia-dev/noctalia/blob/main/example.toml).

## Design constraints

- A candidate must remain unmapped until supervisor activation.
- Activation ACK grants commit permission. It does not prove presentation.
- A first frame or presentation feedback arms the health window, on a
  wall-clock deadline independent of frame callbacks.
- The active generation freezes only after the candidate's presentation
  evidence arrives; a stalled candidate is reaped while N stays fully live.
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
