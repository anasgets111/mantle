# Mantle engine

Project vocabulary. The code is the source of truth. Contracts live in [lua-api](docs/lua-api.md)
and [services](docs/services.md); [decisions](docs/decisions.md) is history, cited as ADR-NNNN for
the why behind behavior the code confirms.

## Processes and ownership

| Term | Meaning |
| :--- | :--- |
| **Supervisor** | The long-lived process. Owns capabilities, idle-notify, PAM, polkit, session processes, the watcher and the control socket; spawns the Renderer and respawns it after a crash (ADR-0058). |
| **Renderer** | The `mantle-renderer` process. Lua VM, retained scene, Wayland client and GL paint share its main thread (ADR-0039); socket I/O, text shaping and image decode run on worker threads. One per generation. |
| **Generation** | One Renderer process and its Lua VM, numbered by a generation ID. Only a Renderer replacement starts a new one; a reload does not. |
| **Authoritative generation** | The generation the Supervisor sends to and accepts frames from. A replacement becomes authoritative when spawned and is hydrated when it connects. |
| **Instance directory** | `$XDG_RUNTIME_DIR/mantle/<pid>-<start ms>/`, one per Supervisor: control socket, log, lock file, icon spools. `mantle list`, `log`, `set` and `call` pick one (ADR-0222, ADR-0227). |
| **Lock authority** | The Supervisor's side of the session lock: deciding to lock, authorizing release after PAM, owning the unlock exit, relocking after a Renderer crash (ADR-0058, ADR-0190). |
| **Lock client** | The Renderer holding `ext_session_lock_v1` and painting its lock surfaces (ADR-0042, ADR-0052). |
| **Check mode** | `mantle check`: the Supervisor runs the Renderer binary with `CHECK_ENV` to evaluate the config with no Wayland, subprocesses or state writes; every capability reads `nil`. See [CLI](docs/lua-api/cli.md). |

## Reloads

| Term | Meaning |
| :--- | :--- |
| **Watcher** | The Supervisor's inotify observer of the config tree (`.lua`, `.frag`). A debounced save with changed content sends `Reevaluate` to the authoritative generation. |
| **Loader** | The Renderer's evaluation of `shell.lua` into a node tree and surface specs. It first drops config modules from `package.loaded` and forgets idle thresholds. |
| **In-place reload** | Re-evaluation in the same generation and VM, then one apply: reconcile the scene, then destroy or create the surfaces whose fingerprint changed (ADR-0216). |
| **Surface fingerprint** | A declaration's creation-time fields (panel: id, layer, anchor, monitor, namespace; other roles: id). A change rebuilds that surface in place; other edits update live surfaces. |
| **Topology change** | An edit that adds, removes or re-fingerprints a surface. Applies in place like any reload, except one that renames or removes the lock surface while locked, which is refused (ADR-0216). |
| **Evaluation-scoped registration** | `action`, `on_change` and idle-threshold callbacks, cleared before each evaluation because they close over its locals; `timer`s the evaluation arms are staged and go live only when its output applies. For what survives, see [Runtime](docs/lua-api/runtime.md#what-survives-a-reload). |
| **Rollback** | A failed evaluation or apply keeps the previous scene, surfaces and instances. A failed evaluation drops the timers, actions and change handlers it registered; a failed apply drops only its staged timers. |
| **Rescue** | `mantle.rescue`, `{ is_rescue, error_log }`: set by a failed evaluation, a failed startup apply, or a refused or lost session lock; cleared by the next successful evaluation. A failed reload apply only logs. After a startup evaluation failure no surface binds; after a startup apply failure surfaces bind but paint nothing until a reload or push applies. |

## Surfaces

| Term | Meaning |
| :--- | :--- |
| **Surface** | A top-level declaration returned by `shell.lua`, with one role and one or more instances. Not "window" or "layer". |
| **Surface role** | `panel` (layer-shell), `window` (xdg_toplevel), `popup` (xdg_popup) or `lock` (session lock). See [Surfaces](docs/lua-api/surfaces.md). |
| **Structural property** | A property read once per evaluation to make a structural decision, so it refuses a signal: any node's `id`; a `panel`'s `layer`, `anchor`, `monitor`, `namespace`; a `popup`'s `parent`. Those in the surface fingerprint rebuild the surface on change. |
| **Surface instance** | One mapped copy of a surface. Per-output panels and locks use `{id}@{output}`; windows, popups and `monitor = "Active"` panels use the bare id (ADR-0246). Keys the retained scene. |
| **Lock surface** | The lock declaration's instance on one output, alive only while the Renderer holds the lock (ADR-0052). |

## Scene

| Term | Meaning |
| :--- | :--- |
| **Retained scene** | A generation's persistent node tree per surface instance, reconciled across evaluations and passes. |
| **Node identity** | How a node is matched across evaluations, scoped to its parent: sibling `id` or list `key` (which wins), else position among id-less siblings. An unmatched id makes a new node (ADR-0045). |
| **Retained-scene transaction** | One atomic reconcile and resolve of the retained scene. Unmatched children are dropped or become leaving nodes; a failure rolls back. |
| **Signal resolution** | Reading a signal's current value while resolving a node property. `:get()` is a snapshot, not a live property. |
| **Dirty scope** | `DirtyScope`, what a pass re-resolves: `Clean` (written cells nobody read), `Instances` (those that read them), or `All` (a scene-wide mark from reload, resize or rollback, or any write while the session lock is held). ADR-0244. |
| **Layout pass budget** | `LayoutPassBudget`, the 2 s CPU ceiling for one whole pass, beside the 5 ms per-callback budget. Exceeding it fails the pass. See [Runtime](docs/lua-api/runtime.md). |
| **Layout style** | A node's layout properties after signal resolution and validation (`LayoutStyle`), which the solver translates to a taffy style (ADR-0077). |
| **Paint pass** | Drawing one surface instance from its resolved nodes without changing the scene. An unchanged `DisplayList` skips it (ADR-0063, ADR-0258). |
| **Image cache** | A generation's decoded and uploaded textures (`CacheKey`: path, target box, file version, tint, crop, blur). A new generation starts cold. |
| **Icon resolver** | `image::icons::resolve`: an icon theme name to an image file, memoized; an absolute path passes through (ADR-0054). |
| **Shader node** | `shader { source, progress, params }`: a config `.frag` drawn as a node with no input textures; the Watcher reloads on its edit (ADR-0253). |
| **Capture node** | `capture { output, region, live, ... }`: a live output preview with its own texture cache (ADR-0248). |

## Signals and state

| Term | Meaning |
| :--- | :--- |
| **Named state** | A `state(name, initial)` signal, keyed by name in the VM. Survives reloads until a scalar `initial` changes; lost with the generation. `mantle set`/`toggle` write it. |
| **Input signal** | An engine-written, name-keyed signal (`hover`, `hover_rect`, `scroll`, `geometry`; `HoverRegistry`, `ScrollRegistry`, `GeometryRegistry`). Survives reloads like named state (ADR-0062, ADR-0069). |
| **Change handler** | An `on_change(fn)` callback run with the current and previous payload on each capability, `rescue` or `screens` push (ADR-0115). |
| **Idle threshold** | A registered inactivity duration with idle and resume callbacks, cancellable by its handle. The Supervisor keeps each duration's `ext_idle_notify` listeners across reloads and fans events out per generation (ADR-0158, ADR-0232). |
| **Idle inhibit** | A hold that stops idle actions, through one logind inhibitor shared by config and `org.freedesktop.ScreenSaver` clients (ADR-0231). |
| **Session process** | A `session_process` program the Supervisor owns. Survives reloads and Renderer replacement; stopped at shutdown with its declared signal (default SIGTERM), then SIGKILL after 5 s (ADR-0175). Unlike a `process.run` child, which is reaped with its generation. |
| **Persistent table** | `persistent_table`: a JSON file read as signals and written one key at a time, surfaced through the `storage` capability (ADR-0136). |

## Animation

Contract: [Animation](docs/lua-api/animation.md).

| Term | Meaning |
| :--- | :--- |
| **Tween** | A property moving from its displayed value to a newly resolved one, advanced per frame callback without Lua. |
| **Paint-only property** | A `PAINT_ONLY` property, whose change repaints without relayout (`opacity`, colours, `radius`, shadows, blurs, transforms, `progress`); its tweens tick without a pass (ADR-0178, ADR-0261). |
| **Spring** | A tween driven by stiffness and damping instead of duration and easing; it keeps its velocity when the target moves (ADR-0154). |
| **Keyframes** | A property walked through a list of values, once or looped, driven by elapsed time rather than a resolved target (ADR-0152). |
| **Leaving node** | A child the scene dropped, painted at its last rect with no layout, input or identity while its `animate.exit` runs (ADR-0150). |
| **Cross-dissolve** | An `image` blending from the picture it holds to a newly decoded one over its `transition` (ADR-0181, ADR-0186). |
| **Transition shader** | `transition.shader`: a config fragment shader that draws an image's cross-dissolve; the engine compiles and binds it (ADR-0184). |

## Capabilities

| Term | Meaning |
| :--- | :--- |
| **Capability** | A Supervisor module owning one slice of platform state and its actions, read in Lua as `mantle.<name>`. Not "service" or "backend". |
| **Capability roster** | The `shared::Capability` enum: every capability with snapshot state, `idle` included (ADR-0076). `process` is addressable but off-roster. |
| **Capabilities** | The Supervisor's `Capabilities` struct of controllers and channels. Not the `GenerationRegistry`, which tracks Renderer connections. |
| **Capability start** | The first `mantle.<name>` read or secure-submit target starts the backend for the Supervisor's lifetime. `mantle.idle`, set directly on the namespace, starts on its first method call. `lock` and the polkit agent are built at boot; polkit registers its agent on start (ADR-0070). |
| **Snapshot** | A capability's full state payload and revision, pushed to the authoritative generation on change. An equal payload is dropped, except `tray` and `notifications`, whose icon files are rewritten in place. |
| **Hydration** | The Supervisor replaying its last snapshots to a newly connected generation. Before its first snapshot a capability reads `nil`. |
| **Revision** | A capability's snapshot counter, stamped on commands as `expected_revision`. Nothing checks it (ADR-0004). |
| **Mantle namespace** | The `mantle` table: capabilities, the Renderer-sourced `screens` and `rescue`, `version` and `config_dir`. |
| **IDL** | The typed engine contract in `lua-meta/`: capability payloads and actions generated from Rust, node and surface properties hand-written. |
| **Secure submit** | A secret field sending its native buffer straight to a named capability action, never through Lua (ADR-0005, ADR-0027). |

## Capability domains

| Term | Meaning |
| :--- | :--- |
| **Compositor probe** | Session-level detection of a supported compositor (`CompositorKind`: niri, Hyprland), shared by `keyboard`, `workspaces` and `windows`; `windows` falls back to wlr-foreign-toplevel elsewhere (ADR-0075). |
| **Compositor link** | The keyboard capability's per-compositor connection for layout state and switching (`CompositorLink`). |
| **Toplevel window** | Another application's window, listed by the `windows` capability (ADR-0247). Not the `window` surface role. |
| **Do-not-disturb** | A `notifications` toggle that silences non-critical sounds. It does not filter the feed. |
| **Notification body span** | One allowlisted styled-text or validated-image run of a sanitized notification body (ADR-0033). |
| **Track identity** | MPRIS track ID, URL and title combined, telling a new track from a refresh of the same one (ADR-0036). |
