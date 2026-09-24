# Glossary

The terms these pages use, each defined once. Engine-internal vocabulary (retained scene, dirty
scope, capability roster) lives in [`CONTEXT.md`](../CONTEXT.md).

## Processes

| Term | Meaning |
| :--- | :--- |
| **Supervisor** | The long-lived `mantle` process. Owns capabilities, idle-notify, PAM, polkit, session processes, the file watcher and the control socket; spawns the Renderer and respawns it after a crash. See [CLI](guide/cli.md#binaries). |
| **Renderer** | The `mantle-renderer` process: the Lua VM, the scene, the Wayland client and painting. One per generation. See [runtime](guide/runtime.md#the-vm). |
| **Generation** | One Renderer process and its Lua VM. Only a Renderer replacement (after a crash) starts a new one; a reload does not. |
| **Instance directory** | `$XDG_RUNTIME_DIR/mantle/<pid>-<start ms>/`, one per running `mantle`: control socket, log, lock file, icon spools. `mantle list`, `log`, `set`, `toggle` and `call` pick one. See [which shell](guide/cli.md#which-config-and-which-shell). |
| **Check mode** | `mantle check`: evaluates and lays out the config with no Wayland, no subprocesses and no state writes; every capability reads `nil`. See [what check covers](guide/cli.md#what-check-covers). |
| **Session process** | A [`session_process`](guide/processes.md#session_process) program the Supervisor owns. Survives reloads and Renderer replacement; stopped at shutdown with its declared signal, then SIGKILL after 5 s. A `process.run` child, by contrast, dies with its generation. |

## Reloads

| Term | Meaning |
| :--- | :--- |
| **Evaluation** | One run of `shell.lua` and the modules it `require`s, producing the surface list. |
| **In-place reload** | A re-evaluation in the same generation and VM after a saved `.lua` or `.frag` file or an output change, then one apply: the scene is reconciled, and surfaces whose fingerprint changed are destroyed or created. See [runtime](guide/runtime.md#evaluation-reload-and-generations). |
| **Surface fingerprint** | A declaration's creation-time fields (panel: `id`, `layer`, `anchor`, `monitor`, `namespace`; other roles: `id`). A change rebuilds that surface; other edits update it live. |
| **Evaluation-scoped registration** | `action`, `on_change` and idle-threshold callbacks, cleared before each evaluation because they close over its locals. The `timer`s an evaluation arms go live only when its result applies. See [what survives a reload](guide/runtime.md#what-survives-a-reload). |
| **Rollback** | A failed evaluation or apply keeps the previous scene and surfaces. A failed evaluation drops the timers, actions and change handlers it registered; a failed apply drops only its timers. |
| **Rescue** | [`mantle.rescue`](capabilities/index.md#renderer-members), `{ is_rescue, error_log }`: set by a failed evaluation, apply or live update, or a refused or lost session lock; cleared by the next reload that applies. A failed startup apply or live update also clears when a later pass applies. |

## Surfaces

| Term | Meaning |
| :--- | :--- |
| **Surface** | A top-level declaration returned by `shell.lua`, with one role and one or more instances. See [surfaces](surfaces/index.md). |
| **Surface role** | [`panel`](surfaces/panel.md) (layer-shell), [`window`](surfaces/window.md) (xdg_toplevel), [`popup`](surfaces/popup.md) (xdg_popup) or [`lock`](surfaces/lock.md) (session lock). |
| **Surface instance** | One mapped copy of a surface. Per-output panels and locks are keyed `{id}@{output}`; windows, popups and `monitor = "Active"` panels use the bare id. |
| **Structural property** | A property read once per evaluation to make a structural decision, so it refuses a signal: any node's `id`; a `panel`'s `layer`, `anchor`, `monitor`, `namespace`; a `popup`'s `parent`. |
| **Lock surface** | The `lock` declaration's instance on one output, alive only while the session is locked. |

## Nodes and paint

| Term | Meaning |
| :--- | :--- |
| **Node** | An element in a surface's tree: `row`, `text`, `button`, `list` and the other [kinds](nodes/index.md). |
| **Node identity** | How a node is matched across evaluations, scoped to its parent: sibling `id` or list `key` (which wins), else position among id-less siblings. An unmatched node is new and starts fresh. |
| **Paint-only property** | A property whose change repaints without relayout (`opacity`, colours, `radius`, shadows, blurs, transforms, `progress`). Its tweens tick without a layout pass. |
| **Shader node** | [`shader`](nodes/shader.md): a config `.frag` drawn as a node. Editing the file reloads. |
| **Capture node** | [`capture`](nodes/capture.md): a live preview of an output. |

## Signals and state

| Term | Meaning |
| :--- | :--- |
| **Signal** | A reactive value. Pass it to a property to keep that property live; `:get()` is a snapshot. See [signals](guide/signals.md). |
| **Derived signal** | A signal computed from others: `:map`, `computed`, `delay`, `pulse`. See [derived signals](guide/signals.md#derived-signals). |
| **Named state** | A [`state(name, initial)`](guide/signals.md#named-state) signal, keyed by name. Survives reloads until a scalar `initial` changes; lost with the generation. `mantle set` and `toggle` write it. |
| **Input signal** | An engine-written, name-keyed signal: `hover`, `hover_rect`, `scroll`, `geometry`. Survives reloads like named state. |
| **Change handler** | An [`on_change(fn)`](capabilities/index.md#reading-and-acting) callback, run with the current and previous payload on each capability, `rescue` or `screens` push. |
| **Persistent table** | [`persistent_table`](guide/scripting.md#persistent_table): a JSON file read as signals and written one key at a time. |
| **Idle threshold** | An inactivity duration with idle and resume callbacks, registered on [`mantle.idle`](capabilities/idle.md#methods) and cancellable by its handle. |
| **Idle inhibit** | A hold that stops idle actions, shared by the config and `org.freedesktop.ScreenSaver` clients. |

## Animation

| Term | Meaning |
| :--- | :--- |
| **Tween** | A property moving from its displayed value to a newly resolved one, advanced per frame without Lua. See [animation](guide/animation.md). |
| **Spring** | A tween driven by stiffness and damping instead of duration and easing; it keeps its velocity when the target moves. |
| **Keyframes** | A property walked through a list of values, once or looped, driven by elapsed time rather than a resolved target. |
| **Leaving node** | A removed child, painted at its last rect with no layout or input while its `animate.exit` runs. |
| **Cross-dissolve** | An `image` blending from the picture it holds to a newly decoded one over its `transition`. |
| **Transition shader** | `transition.shader`: a config fragment shader that draws an image's cross-dissolve. |

## Capabilities

| Term | Meaning |
| :--- | :--- |
| **Capability** | A backend owning one slice of platform state and its actions, read in Lua as `mantle.<name>`. See [capabilities](capabilities/index.md). |
| **Capability start** | The first `mantle.<name>` read (or `secure_submit` naming it) starts the backend for the Supervisor's lifetime. `mantle.idle` starts on its first method call; `lock` and the polkit controller exist from boot. |
| **Snapshot** | A capability's full state, pushed on change. An equal payload is not pushed again, except for `tray` and `notifications`, whose icon files change in place. |
| **Push** | A capability sending a new snapshot. Every property reading that capability re-resolves. |
| **Hydration** | The Supervisor replaying its last snapshots to a new generation. Before its first snapshot a capability reads `nil`. |
| **Action** | `mantle.<cap>:invoke(name, ...)`, fire and forget; or `action(name, fn)`, which exposes Lua to [`mantle call`](guide/scripting.md#action). |
| **Mantle namespace** | The `mantle` table: capabilities, plus the Renderer's `screens`, `rescue`, `version` and `config_dir`. |
| **Secure submit** | A secret text field sending its buffer straight to a capability action, never through Lua. See [secure fields](guide/input.md#secure-fields). |
| **Toplevel window** | Another application's window, listed by the [`windows`](capabilities/windows.md) capability. Not the `window` surface role. |
| **Do-not-disturb** | A `notifications` toggle that silences non-critical sounds. It does not filter the feed. |
