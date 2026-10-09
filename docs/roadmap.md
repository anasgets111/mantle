# Roadmap

Ordering is intent, not a schedule. The [docs](introduction.md) hold what exists,
[`DECISIONS.md`](../DECISIONS.md) why. Rust owns platform connections, validation, secrets,
resource lifetimes, input and rendering; Lua owns composition, appearance and orchestration. A
feature one config lacks is not an engine gap.

## Next

Defects or missing pieces a config cannot work around.

| Item | Why / what's left | ADR |
| :--- | :--- | :--- |
| Multi-prompt PAM | The worker relays every masked prompt, but `LockState` and `secure_submit` carry one password, answered to every prompt. Fingerprint, 2FA and expired passwords fail. Echo-on prompts stay refused. Left: relay each prompt, info and error line to Lua for both lock and polkit (`polkit-agent-helper-1` already emits them); answer secret prompts through `secure_submit`, visible ones as typed text. Fingerprint on the lock screen: run a fingerprint-only PAM service beside the password one and kill the loser's worker, since one stack runs `pam_fprintd` before `pam_unix` and blocks typing until the scan times out; polkit cannot do this and shows the scan prompt first | 0028, 0241 |
| Polkit identity choice | `first_unix_user_uid` answers for the first identity; with several admin accounts the user should pick one, restarting the helper for it | 0028 |

## Later

Wanted, but each needs a consumer or a decision first.

| Item | Why / what's left | ADR |
| :--- | :--- | :--- |
| Greeter | Mantle as a greetd login screen, inside whatever compositor greetd starts for it. Needs multi-prompt PAM and a session-launch command | — |
| Output actions | `windows` has six actions; screens have none. Pick the actions, then settle niri/Hyprland differences and revert | 0119, 0247 |
| Text field editing | The caret has no stop inside a ligature, and IME needs the field's surface focused, so a parent-focused popup gets none. The secure field keeps end-only editing and no IME, so an input method never sees the draft | 0236, 0312 |
| Animated WebP and APNG | Only GIF animates; the others draw their first frame. `AnimationDecoder` covers both | 0233 |
| Wayland and input extras | No shortcut inhibition, touch gestures, cross-app drag and drop, or pointer buttons past left, right and middle | — |
| Window capture backends | `capture.window` accepts Hyprland `windows` IDs. Niri needs a toplevel capture source; wlr needs an exact bridge from its connection-local IDs | 0247, 0248 |

## Won't do

| Item | Instead | ADR |
| :--- | :--- | :--- |
| Native FFT or audio peak metering | Stream Cava output into state and draw it | 0316 |
| Weather, currency, geolocation, HTTP, sockets or watched file contents | `process.run` streaming `curl`, `socat` or `tail -f`, then `json.decode`; `files` watches folders | — |
| KDE Connect | `kdeconnect-cli` or `dbus-monitor` streamed through `process.run` | — |
| MPRIS remote artwork | `curl` the player's `art_url` into a cache file through `process.run` and draw that path | 0036 |
| Generic IPC state read, subscription or per-panel commands | An `action` that returns the value, read with `mantle call`; `mantle set` and `toggle` write state | 0197 |
| Child stdin, cwd or env | `process.run("env", { "-C", dir, "K=V", cmd })`; `sh -c` for one-shot stdin | 0175, 0188 |
| Per-surface idle inhibition | `idle:inhibit` while the surface shows, `release_inhibit` when it hides | — |
| List virtualization | `limit`, or page the source in Lua. A full rebuild costs about 25 µs a row; revisit for a real list past ~200 rows | 0191, 0219 |
| Clipboard capability | `process.detach("wl-copy", { text })`: a selection needs a process that stays alive to serve it | 0188 |
| Video encoding or global input capture | A recorder or input backend under `session_process`, streamed in | 0175 |
| Wallpaper capability | A `"background"`-layer panel, an `image` with `async`/`retain`/`transition`, `files` for the folder, `persistent_table` for the choice | 0055 |
| Rust widgets (sliders, calendars, launchers, settings) | Lua components over existing nodes | — |
| Framework settings schema | `persistent_table` with config-declared files | — |
| Deferred surface loader | Wayland objects are created when shown; the [Lua CPU budget](guide/runtime.md#limits-and-budgets) guards one signal resolve, not a whole evaluation | 0157 |
| Shaders over the desktop behind a surface | The compositor owns those pixels. `effect.shader` reads what this surface painted, a node's subtree; the input-less `shader` node and `image.transition` keep their contracts | 0184, 0253, 0336 |
| Display manager (PAM as root, sessions, seats) | greetd; see Greeter | — |
| X11 or i3 | The target is a Wayland session shell | — |
