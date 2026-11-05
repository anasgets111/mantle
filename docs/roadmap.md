# Roadmap

Ordering is intent, not a schedule. The [docs](introduction.md) hold what exists,
[`DECISIONS.md`](../DECISIONS.md) why. Rust owns platform connections, validation, secrets,
resource lifetimes, input and rendering; Lua owns composition, appearance and orchestration. A
feature one config lacks is not an engine gap.

## Next

Defects or missing pieces a config cannot work around.

| Item | Why / what's left | ADR |
| :--- | :--- | :--- |
| Lock screen fingerprint ([#14](https://github.com/anasgets111/mantle/issues/14)) | One stack that runs `pam_fprintd` before `pam_unix` blocks typing until the scan times out. Run a fingerprint-only PAM service beside the password one and kill the loser's worker; it needs real `account` checks and a deliberate faillock choice. Polkit cannot do this, since its helper runs one stack | 0028, 0381 |

## Later

Wanted, but each needs a consumer or a decision first.

| Item | Why / what's left | ADR |
| :--- | :--- | :--- |
| Greeter ([#8](https://github.com/anasgets111/mantle/issues/8)) | Mantle as a greetd login screen, inside whatever compositor greetd starts for it. Needs a session-launch command | — |
| Output actions ([#9](https://github.com/anasgets111/mantle/issues/9)) | `windows` has six actions; screens have none. Pick the actions, then settle niri/Hyprland differences and revert | 0119, 0247 |
| Text field editing ([#10](https://github.com/anasgets111/mantle/issues/10), [#15](https://github.com/anasgets111/mantle/issues/15)) | The caret has no stop inside a ligature, and IME needs the field's surface focused, so a parent-focused popup gets none. The secure field keeps end-only editing and no IME, so an input method never sees the draft | 0236, 0312 |
| Wayland and input extras ([#12](https://github.com/anasgets111/mantle/issues/12), [#17](https://github.com/anasgets111/mantle/issues/17), [#18](https://github.com/anasgets111/mantle/issues/18)) | No shortcut inhibition, touch gestures, or cross-app drag and drop | — |
| Window capture backends ([#13](https://github.com/anasgets111/mantle/issues/13), [#16](https://github.com/anasgets111/mantle/issues/16)) | `capture.window` accepts Hyprland `windows` IDs. Niri needs a toplevel capture source; wlr needs an exact bridge from its connection-local IDs | 0247, 0248 |

## Won't do

| Item | Instead | ADR |
| :--- | :--- | :--- |
| Native FFT or audio peak metering | Stream Cava output into state and draw it | 0316 |
| Weather, currency, geolocation, HTTP, sockets or watched file contents | `process.run` streaming `curl`, `socat` or `tail -f`, then `json.decode`; `file.read` reads a file once; `folders` watches folders | — |
| MPRIS remote artwork | `curl` the player's `art_url` into a cache file through `process.run` and draw that path | 0036 |
| Generic IPC state read, subscription or per-panel commands | An `action` that returns the value, read with `mantle call`; `mantle set` and `toggle` write state | 0197 |
| Child stdin, cwd or env | `process.run("env", { "-C", dir, "K=V", cmd })`; `sh -c` for one-shot stdin | 0175, 0188 |
| Per-surface idle inhibition | `idle:inhibit` while the surface shows, `release_inhibit` when it hides | — |
| Clipboard capability | `process.detach("wl-copy", { text })`: a selection needs a process that stays alive to serve it | 0188 |
| Video encoding or global input capture | A recorder or input backend under `session_process`, streamed in | 0175 |
| Wallpaper capability | A `"background"`-layer panel, an `image` with `async`/`retain`/`transition`, `folders` for the folder, `persistent_table` for the choice | 0055 |
| Rust widgets (sliders, calendars, launchers, settings) | Lua components over existing nodes | — |
| Framework settings schema | `persistent_table` with config-declared files | — |
| Deferred surface loader | Wayland objects are created when shown; the [Lua CPU budget](guide/runtime.md#limits-and-budgets) guards one signal resolve, not a whole evaluation | 0157 |
| Shaders over the desktop behind a surface | The compositor owns those pixels. `effect.shader` reads what this surface painted, a node's subtree; the input-less `shader` node and `image.transition` keep their contracts | 0184, 0253, 0336 |
| Display manager (PAM as root, sessions, seats) | greetd; see Greeter | — |
| X11 or i3 | The target is a Wayland session shell | — |
