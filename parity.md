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
| Tray | Engine module, user builds UI | Full engine implementation incl. drawer |
| Theming | None; users hardcode | Wallpaper-to-palette via Material Color Utilities, template stamping into other apps' configs |
| Errors | On-screen overlay with error text and file:line | Not documented as a feature |
| IPC | None shipped; users build sockets | `noctalia msg` CLI plus D-Bus debug endpoint |
| Animations | Qt animations, Behavior pattern, springs available | Engine animation manager, config-level speed policy |
| Compositors | Per-compositor modules (Hyprland, I3) | Niri/Hyprland/Sway adapters plus generic protocol floor |

Oblisk takes the framework model from Quickshell, the reload safety of its own
process-per-generation design, and theming scope from Noctalia.

## Parity map

| Area | Required contract | First check | Rust seam |
| --- | --- | --- | --- |
| Reload transaction | One supervisor state machine owns activation, presentation evidence, freeze ordering, rollback, deadlines, and reaping | Wrong epoch, stalled presentation, crash, rollback, and pending-request tests | Renderer milestones, presentation feedback, and process cleanup are adapters. |
| Dependency snapshot | One rooted graph captures opened-file inode and hash, bounded source, and watch roots | Traversal, symlink, rename, missing-parent, and changed-inode tests | Loader and watcher consume the same snapshot module. |
| Retained scene | One transaction owns signals, builders, bounded keyed repeaters, stable IDs, writes, leases, and child-first cleanup | Reorder, remove, replace, failed-refresh, and callback-error tests | Rust owns the scene transaction. Lua returns descriptors. |
| Layout and paint | Intrinsic rows and columns, bounded Unicode text, panel fills, and path-only PNG/JPEG assets | Nested layout, overflow, text, and cache-limit tests | femtovg on EGL is the production adapter. CPU SHM is the headless test adapter (llvmpipe/CI). No third paint adapter. |
| Input | Bounded pointer and keyboard values, focus authorization, freeze behavior, and callback ordering | Input FIFO, focus, freeze, and callback-error tests | Rust owns Wayland input. Lua sees values. |
| Processes | Generation-scoped direct commands, bounded capture, timeout escalation, and process-group cleanup | Child timeout, descendant, capture, and reap tests | Rust owns children. Lua owns opaque handles. |
| Outputs | Per-output surface entries, configure size, scale, transform, hotplug, and frame gating | Output add/remove, resize, scale, transform, and first-frame tests | SCTK owns Wayland proxies. |
| Surface kinds | Panel first, then popups (deferred to phase 7 where the launcher and notification center become their first callers); overlays wait for a fixture that needs one | Second surface topology under the same renderer trait | Keep product layout in Lua. |
| Notifications | Supervisor-owned bounded snapshot bridge, visible-ID gating, public authority only after routing ownership | Snapshot bounds, staged-feed, presentation, and command tests | No raw D-Bus objects enter Lua. |
| Capability authority | Bounded snapshots, revisions, availability, generation authorization, stale-command rejection, and disconnect revocation. State-dependent commands carry the expected snapshot revision. | Stale generation, stale revision, disconnect, unavailable state, and shutdown tests | One authority module. Backend adapters validate backend-specific commands. |
| MPRIS | Renderer-local Rust capability feed for the first slice, with bounded rows, revisions, unavailable state, and validated commands | Startup, disconnect, stale generation, stale revision, and shutdown tests | Move to a durable owner only for continuity, public ownership, or overlap-safe lifetime. |
| Desktop capabilities | One adapter at a time for tray, audio, power, network, Bluetooth, workspaces, and clipboard; feature detection per capability | Headless contract plus real-session check per capability | Capabilities publish state. Lua chooses the view. |
| Theming | Palette capability with fixed role vocabulary and wallpaper derivation. Template stamping is deferred until a real consumer exists. | Palette signal drives widget bindings; template post-hook test when stamping has a caller | Stamping runs on the durable side and never inside a renderer generation. |
| Error surfacing | Supervisor-owned Rust banner plus `reload-rejected` events to `on_ipc` | Rejected-candidate and callback-failure tests | The banner survives when every generation is broken. |
| IPC | Supervisor socket verbs plus generic `on_ipc` pass-through | Malformed message and dead-generation tests | Built-in verbs work without a valid generation. |
| Lock and auth | Separate process owns the session-lock protocol and PAM. Greetd and Polkit conversations remain deferred until a caller exists. | Disposable PAM account and compositor recovery check | Secrets never enter Lua or renderer IPC. |
| Compositor interfaces | Optional capability adapters; Niri and Hyprland first-class, generic protocols as floor | Protocol-specific test per compositor | No compatibility layer before a caller exists. |

## Quickshell and Noctalia module comparison

| Reference family | Oblisk plan | Rust seam |
| --- | --- | --- |
| Core authoring and reload (Q: QML tree, N: schema patch) | Lua descriptors, dependency snapshots, retained-scene commits, and process-based reload | Supervisor owns the reload transaction and generations. |
| IO and processes (Q) | Direct bounded commands first. Streams, sockets, and detached jobs need separate slices. | Rust owns pipes and process groups. |
| Panels and widgets (both) | Lua composition over Rust surface, layout, input, and capability contracts; TextField/TextArea as engine exception | Rust owns Wayland objects. |
| Rendering and assets (Q: Qt, N: GLES scene graph) | femtovg paint, cosmic-text shaping, bounded PNG/JPEG assets | Lua supplies names and values. Rust opens and decodes assets. |
| Notifications and media (both) | Bounded notification bridge and MPRIS capability feed | Rust owns capability connections and revisions. |
| Lock and authentication (both) | Separate durable process with data-only Lua theming (N: session-lock + PAM proven at scale) | Authentication owner holds secrets. |
| Tray (N: full engine, Q: engine module) | Engine watcher/host/menu data on the durable side; Lua builds the drawer | Watcher name outlives generations. |
| Theming (N only) | Palette capability and wallpaper derivation. Template stamping waits for a real consumer. | Stamping runs on the durable side when implemented. |
| IPC (N: `noctalia msg`, Q: none) | Supervisor verbs plus `on_ipc` pass-through | Built-in verbs survive a broken generation. |
| Compositor interfaces (both) | Optional capability adapters | Each adapter owns its protocol objects. |

Quickshell's public module families are documented in its
[core](https://github.com/quickshell-mirror/quickshell/blob/master/src/core/module.md),
[IO](https://github.com/quickshell-mirror/quickshell/blob/master/src/io/module.md),
[Wayland](https://github.com/quickshell-mirror/quickshell/blob/master/src/wayland/module.md),
and [module families](https://github.com/quickshell-mirror/quickshell/tree/master/src/services)
sources. Noctalia documents its stack, layout, and config in
[CONTRIBUTING.md](https://github.com/noctalia-dev/noctalia/blob/main/CONTRIBUTING.md)
and [example.toml](https://github.com/noctalia-dev/noctalia/blob/main/example.toml).

## Acceptance checks

Before calling a slice ready, record:

1. The owner of each proxy, thread, child, cache, lease, and public endpoint.
2. Byte and count limits at each input and queue seam.
3. Cleanup behavior during reload, freeze, callback failure, disconnect, and
   process exit.
4. One focused headless test.
5. A real compositor or capability check where protocol behavior matters.

Do not add a crate without an immediate caller and runnable check. Prefer SCTK,
Calloop, direct protocol bindings, standard library facilities, and focused
crates in that order.

## Update rules

- Update this file when a contract or parity decision changes.
- Keep implementation detail in `plan.md` and the checklist in `build-steps.md`.
- Do not add speculative features to this ledger.
- Record a check only when its owner and test path are named.
