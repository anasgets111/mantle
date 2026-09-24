# Roadmap

Ordering is intent, not a schedule. The [docs](introduction.md) hold what exists,
[`DECISIONS.md`](../DECISIONS.md) why. Rust owns platform connections, validation, secrets,
resource lifetimes, input and rendering; Lua owns composition, appearance and orchestration. A
feature one config lacks is not an engine gap.

## Next

Defects or missing pieces a config cannot work around.

| Item | Why / what's left | ADR |
| :--- | :--- | :--- |
| `expected_revision` is unchecked | The socket drops a frame from another generation, but nothing reads the revision it claims, so it is no authorization guarantee. Settle stale-revision semantics before anything relies on it | — |
| Keyboard focus and accessibility | Only `textfield` holds focus; Tab reaches the config as `on_navigate("tab")`. Needs focusable controls, keyboard activation and an accessibility tree | — |
| Blocking `dofile` / `loadfile` | The base library keeps both, and they read files on the Wayland thread outside the CPU budget, against ADR-0048's intent. Remove them or route them through `require`'s resolver | 0048 |
| HiDPI | Paint scale is fixed at `1.0`, so every surface on a scaled output is upscaled and soft. Needs `set_buffer_scale` (or fractional-scale plus viewporter) with a matching EGL resize and glyph raster scale | — |
| `mantle check` misses the node tree | `renderer/src/check.rs` validates each surface's own properties only; unknown node properties, bad values and stray `require` results under `child` pass and fail at apply. Run the node parser over each tree | — |
| Silent failures | Errors in `on_change`, `timer`, `process.run` and idle callbacks log at debug; `warn()` prints nothing; a failed reload apply leaves `mantle.rescue` false; an invalid `stop_signal` or unknown `secure_submit` target is dropped with a log line; duplicate surface ids and `mantle set` on a removed `state` pass. Each should reach the author | — |
| Hyprland layout switch | `keyboard/layout.rs` sends `switchxkblayout`, which Hyprland 0.56's Lua socket likely rejects; the other Hyprland writes already use `hl.dsp.*` | — |
| Multi-prompt PAM | The worker relays every masked prompt, but `LockState` and `secure_submit` carry one password, answered to every prompt. Fingerprint, 2FA and expired passwords fail. Echo-on prompts stay refused | 0241 |

## Later

Wanted, but each needs a consumer or a decision first.

| Item | Why / what's left | ADR |
| :--- | :--- | :--- |
| Greeter | Mantle as a greetd client under cage or sway. Needs multi-prompt PAM and a session-launch command | — |
| Drawing | Gradients, `mask`, shadows and blurs exist; no config-facing paths, and no node masks another. Add the smallest set a real component needs; SVG covers static artwork, but its `<text>` draws nothing | 0254–0256 |
| Large lists | Every item up to `limit` is built on every pass, visible or not. Virtualization would need `key` to be mandatory, which cannot be enforced | 0191, 0219 |
| Output actions | `windows` has five actions; screens are read-only. Pick the actions, then settle niri/Hyprland differences and revert | 0119, 0247 |
| Service depth | MPRIS lacks stop, shuffle, repeat, rate and volume; audio has no per-channel levels or peak metering; UPower reads only `DisplayDevice`; `network` tracks only the first Wi-Fi device; Bluetooth pairing refuses PIN and passkey entry. Extend for concrete controls | — |
| External IPC | `set`/`toggle` are one-way; `call` returns only what the action returns. No generic state read or subscription | 0197 |
| Process control | Start, stream and signal exist. No child stdin, cwd or env | 0175, 0188 |
| Panel root sizing | A panel spanning an axis sizes the surface but not its root node, while window and lock roots fill theirs (`forced_root_size`). Decide whether panel roots fill too | — |
| Move transitions | A sibling closing a gap snaps. Needs the solver's old and new rects per sibling | — |
| Text field editing | No undo, paste or IME; the secure field edits only at its end. On RTL or mixed lines a click lands one cluster off and the caret does not move inside a ligature | 0236 |
| Animated WebP and APNG | Only GIF animates; the others draw their first frame. `AnimationDecoder` covers both | 0233 |
| Localization | No translation API; desktop entry `Name`, `GenericName` and `Keywords` are read unlocalized | 0112 |
| Wayland and input extras | No shortcut inhibition, per-surface idle inhibition, touch gestures, cross-app drag and drop, pointer buttons past left, right and middle, or a click position inside a button. logind and ScreenSaver inhibition work | — |
| Window capture | `capture` takes an output. A window source would take `windows` ids | 0247, 0248 |
| Native I/O | No HTTP, sockets, watched file contents or `json.encode`; JSON storage and folder watching exist. Native only for a measured latency or volume need | — |
| KDE Connect | No device or plugin model. A capability or a streaming helper, not unrestricted D-Bus | — |
| Dynamic topology | A reload rebuilds only what changed. Revisit only if dynamic windows need a different lifetime | 0216 |

## Won't do

| Item | Instead | ADR |
| :--- | :--- | :--- |
| Weather, currency or geolocation capabilities | `process.run` with an HTTP CLI, then `json.decode` | — |
| Native FFT | Stream Cava output into state and draw it | — |
| Clipboard capability | `process.detach("wl-copy", { text })`: a selection needs a process that stays alive to serve it | 0188 |
| Video encoding | A recorder under `session_process`, driven from config | 0175 |
| Global input capture | An external input backend, streamed in | — |
| Wallpaper capability | A `Background` panel, an `image` with `async`/`retain`/`transition`, `files` for the folder, `persistent_table` for the choice | 0055 |
| Rust widgets (sliders, calendars, launchers, settings) | Lua components over existing nodes | — |
| Framework settings schema | `persistent_table` with config-declared files | — |
| Per-panel IPC commands | `mantle set`, `toggle` and `call` | — |
| Deferred surface loader | Wayland objects are created when shown; the 5 ms cap guards one signal resolve, not a whole evaluation | 0157 |
| Shaders over a subtree or as a persistent filter | `image.transition` and the input-less `shader` node keep a stable contract. Fixed blur or shadow is `content_blur` and `shadow_*` | 0184, 0253, 0254 |
| Display manager (PAM as root, sessions, seats) | greetd; see Greeter | — |
| X11 or i3 | The target is a Wayland session shell | — |
