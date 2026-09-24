# Mantle engine

Engine vocabulary. The code is the source of truth. Terms a config author meets (Supervisor,
generation, surface, signal, capability, snapshot, rescue...) are defined once, in the
[glossary](docs/glossary.md); this file holds the engine-internal ones and the engine notes on
those. Contracts live in the [docs](docs/introduction.md); [decisions](DECISIONS.md) is history,
cited as ADR-NNNN for the why behind behavior the code confirms.

## Engine notes on glossary terms

| Term | Engine note |
| :--- | :--- |
| **Supervisor** | Respawns the Renderer after a crash (ADR-0058). |
| **Renderer** | Lua VM, retained scene, Wayland client and GL paint share its main thread (ADR-0039); socket I/O, text shaping and image decode run on worker threads. |
| **Generation** | Numbered by a generation ID. |
| **Instance directory** | ADR-0222, ADR-0227. |
| **Check mode** | The Supervisor runs the Renderer binary with `CHECK_ENV`. |
| **In-place reload** | ADR-0216. |
| **Rollback** | Also keeps the previous surface instances. |
| **Rescue** | After a startup evaluation failure no surface binds; after a startup apply failure surfaces bind but paint nothing until a reload or push applies. |
| **Surface** | Not "window" or "layer". |
| **Surface instance** | Keys the retained scene (ADR-0246). |
| **Lock surface** | ADR-0052. |
| **Node identity** | ADR-0045. |
| **Paint-only property** | `PAINT_ONLY` (ADR-0178, ADR-0261). |
| **Shader node** | No input textures (ADR-0253). |
| **Capture node** | Has its own texture cache (ADR-0248). |
| **Input signal** | `HoverRegistry`, `ScrollRegistry`, `GeometryRegistry` (ADR-0062, ADR-0069). |
| **Change handler** | ADR-0115. |
| **Idle threshold** | The Supervisor keeps each duration's `ext_idle_notify` listeners across reloads and fans events out per generation (ADR-0158, ADR-0232). |
| **Idle inhibit** | One logind inhibitor (ADR-0231). |
| **Session process** | ADR-0175. |
| **Persistent table** | Surfaced through the `storage` capability (ADR-0136). |
| **Tween**, **Spring**, **Keyframes** | ADR-0154 (spring), ADR-0152 (keyframes). |
| **Leaving node** | ADR-0150. |
| **Cross-dissolve**, **Transition shader** | The engine compiles and binds the shader (ADR-0181, ADR-0184, ADR-0186). |
| **Capability** | A Supervisor module. Not "service" or "backend". |
| **Capability start** | ADR-0070. |
| **Snapshot** | Carries a [revision](#capabilities). |
| **Secure submit** | ADR-0005, ADR-0027. |
| **Toplevel window** | ADR-0247. |

## Processes and ownership

| Term | Meaning |
| :--- | :--- |
| **Authoritative generation** | The generation the Supervisor sends to and accepts frames from. A replacement becomes authoritative when spawned and is hydrated when it connects. |
| **Lock authority** | The Supervisor's side of the session lock: deciding to lock, authorizing release after PAM, owning the unlock exit, relocking after a Renderer crash (ADR-0058, ADR-0190). |
| **Lock client** | The Renderer holding `ext_session_lock_v1` and painting its lock surfaces (ADR-0042, ADR-0052). |

## Reloads

| Term | Meaning |
| :--- | :--- |
| **Watcher** | The Supervisor's inotify observer of the config tree (`.lua`, `.frag`). A debounced save with changed content sends `Reevaluate` to the authoritative generation. |
| **Loader** | The Renderer's evaluation of `shell.lua` into a node tree and surface specs. It first drops config modules from `package.loaded` and forgets idle thresholds. |
| **Topology change** | An edit that adds, removes or re-fingerprints a surface. Applies in place like any reload, except one that renames or removes the lock surface while locked, which is refused (ADR-0216). |

## Scene

| Term | Meaning |
| :--- | :--- |
| **Retained scene** | A generation's persistent node tree per surface instance, reconciled across evaluations and passes. |
| **Retained-scene transaction** | One atomic reconcile and resolve of the retained scene. Unmatched children are dropped or become leaving nodes; a failure rolls back. |
| **Signal resolution** | Reading a signal's current value while resolving a node property. `:get()` is a snapshot, not a live property. |
| **Dirty scope** | `DirtyScope`, what a pass re-resolves: `Clean` (written cells nobody read), `Instances` (those that read them), or `All` (a scene-wide mark from reload, resize or rollback, or any write while the session lock is held). ADR-0244. |
| **Layout pass budget** | `LayoutPassBudget`, the 2 s CPU ceiling for one whole pass, beside the 5 ms per-callback budget. Exceeding it fails the pass. See [Runtime](docs/guide/runtime.md#limits-and-budgets). |
| **Layout style** | A node's layout properties after signal resolution and validation (`LayoutStyle`), which the solver translates to a taffy style (ADR-0077). |
| **Paint pass** | Drawing one surface instance from its resolved nodes without changing the scene. An unchanged `DisplayList` skips it (ADR-0063, ADR-0258). |
| **Image cache** | A generation's decoded and uploaded textures (`CacheKey`: path, target box, file version, tint, crop, blur). A new generation starts cold. |
| **Icon resolver** | `image::icons::resolve`: an icon theme name to an image file, memoized; an absolute path passes through (ADR-0054). |

## Capabilities

| Term | Meaning |
| :--- | :--- |
| **Capability roster** | The `shared::Capability` enum: every capability with snapshot state, `idle` included (ADR-0076). `process` is addressable but off-roster. |
| **Capabilities** | The Supervisor's `Capabilities` struct of controllers and channels. Not the `GenerationRegistry`, which tracks Renderer connections. |
| **Revision** | A capability's snapshot counter, stamped on commands as `expected_revision`. Nothing checks it (ADR-0004). |
| **IDL** | The typed engine contract in `lua-meta/`, generated from Rust: capability payloads and actions from their types; node and surface properties from the typed fields the parsers read, their Rust types and `///` docs; globals from their Rust signatures and `///` docs; a nested table shape's key names and field types from the struct its parser reads it as, its optional keys and Lua spellings marked by hand there. Still hand-written: the table aliases `Gradient`, `GradientStop`, `Mask`, `Easing`, `Animation`, `Animations` and `Exit`, and `Cursor`, whose crate lists no names (`Align`, `Percent`, `EasingName` and `PopupAnchor` are filled from Rust) and the `StateSignal`, `ScrollSignal`, `PersistentTable` and `SessionProcessHandle` classes, which LuaLS models with generics or dynamic keys. |

## Capability domains

| Term | Meaning |
| :--- | :--- |
| **Compositor probe** | Session-level detection of a supported compositor (`CompositorKind`: niri, Hyprland), shared by `keyboard`, `workspaces` and `windows`; `windows` falls back to wlr-foreign-toplevel elsewhere (ADR-0075). |
| **Compositor link** | The keyboard capability's per-compositor connection for layout state and switching (`CompositorLink`). |
| **Notification body span** | One allowlisted styled-text or validated-image run of a sanitized notification body (ADR-0033). |
| **Track identity** | MPRIS track ID, URL and title combined, telling a new track from a refresh of the same one (ADR-0036). |
