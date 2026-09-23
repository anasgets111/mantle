# Roadmap

Ordering is intent, not a schedule. Nothing here is dated or promised.
[API](lua-api.md) and [services](services.md) hold what exists, [decisions](decisions.md) why.

Rust owns platform connections, validation, secret handling, resource lifetimes, input and
rendering. Lua owns composition, appearance, preferences and orchestration. A feature one config
lacks is not an engine gap.

## Next

Correctness before features. Each of these is a defect or a missing piece a config cannot work
around.

- **`expected_revision` is unchecked.** The socket drops a frame naming another generation, but
  nothing reads the revision it claims, so it is not an authorization guarantee (services.md § 13).
  Settle stale-revision semantics before anything relies on them.
- **Keyboard focus and accessibility.** Only `textfield` can hold keyboard focus, and Tab reaches
  the config as `on_navigate("tab")` rather than moving it. Needs focusable controls, keyboard
  activation and an accessibility tree.

## Later

Wanted, but each needs a consumer or a decision first.

| Area | Open question |
| :--- | :--- |
| Multi-prompt PAM | The worker relays every masked prompt (ADR-0241), but `LockState` and `secure_submit` carry one password, so the Supervisor answers each prompt with it. Fingerprint, 2FA and an expired password fail on the lock screen until a second prompt reaches the config. Echo-on prompts stay refused by design |
| Greeter | greetd keeps PAM and root, so Mantle would be a client on its JSON socket, launched under cage or sway. Needs the multi-prompt contract above and a session-launch command |
| Drawing | No config-facing paths, gradients or shadows, and clipping is limited to a box's own corner shape. Add the smallest set a real component needs; SVG already covers static artwork |
| Large lists | Every item is constructed: 0.25 ms at 12 rows, 10.9 ms at 500 (ADR-0219). Virtualization would have to *require* `key`, which a config can be told but not made to supply (ADR-0191) |
| Output actions | `windows` focuses, closes, fullscreens, minimizes and maximizes (ADR-0247), but screens are read-only (ADR-0119). Pick the actions, then settle niri/Hyprland differences and revert behaviour |
| Service depth | MPRIS lacks stop, shuffle, repeat, rate and volume; PipeWire exposes volume and balance, not per-channel levels, and has no peak metering; UPower reads only `DisplayDevice`. Extend for concrete controls, not upstream parity |
| External IPC | `set` and `toggle` are one-way; `call` answers, but only what the config chose to return (ADR-0197). No generic state read and no subscription. Does an integration need either? |
| Process control | `run`, `detach` (ADR-0188) and `session_process` (ADR-0175) cover start, stream and signal. Does anything need to write a child's stdin, or set its cwd and env? |
| Move transitions | A sibling closing a gap does not animate. Needs the solver's old and new rects for every sibling, so add it against a demonstrated consumer |
| Text field editing | A plain field has a caret, grapheme-wise motion and deletion, click-to-position and drag or Shift selection (ADR-0064, ADR-0092, ADR-0102, ADR-0236). No undo, no paste and no IME composition, and the secure path is still append and backspace. Paste needs a Wayland selection read, which nothing has asked for |
| Fonts and localization | `text.font` is per-node over the global chain (ADR-0144). No translation API, and `Name`/`GenericName`/`Keywords` are read unlocalized (ADR-0112) |
| Wayland and input extras | No shortcut inhibition, per-surface idle inhibition, touch gestures or cross-app drag and drop. Pick the protocol and a consumer; logind and screensaver inhibition already work |
| Window capture | `capture` covers an output (ADR-0248). A window source would take `windows` ids (ADR-0247), and needs a consumer first |
| Native I/O | No HTTP, sockets or arbitrary watched file contents; JSON storage and folder watching exist. Subprocess helpers first, native only for a measured latency or volume need |
| KDE Connect | No device or plugin model. A Supervisor capability or a helper streaming state, but not unrestricted D-Bus for parity |
| Dynamic topology | A reload rebuilds only what changed (ADR-0216). Keep the current rules unless dynamic windows need a different lifetime model |

## Won't do

| Feature | Instead |
| :--- | :--- |
| Weather, currency or geolocation capabilities | `process.run` with an HTTP CLI, then `json.decode` |
| A native FFT service | Stream Cava output into Lua state and draw it |
| A clipboard capability | `process.detach("wl-copy", { text })`. A Wayland selection belongs to a process that stays alive to serve it, which is what `detach` is |
| Video encoding in the shell | Declare the recorder with `session_process` so it survives a reload, and drive it from config |
| Global input capture in the renderer | Stream an external input backend |
| A wallpaper service or fixed wallpaper surfaces | A background `panel`, `image` with `async`/`retain`/`transition`, watched folders and persisted preferences |
| Rust sliders, calendars, launchers or settings panels | Lua components over existing nodes |
| A framework-owned settings schema | `persistent_table` with config-declared files |
| Dedicated IPC commands per panel | `mantle set` and `mantle toggle` |
| A loader to defer surface creation | Wayland objects are created when shown. The 5 ms cap guards one outermost signal resolve, not a whole config evaluation (ADR-0157) |
| Shaders over an arbitrary subtree, or as a persistent filter | `image.transition` between two endpoints (ADR-0184), the engine's own cross-dissolve being one of those shaders (ADR-0186). Two endpoints and a progress number keep a stable contract; an arbitrary subtree does not |
| Being the display manager: PAM as root, session opening, seat management | greetd already does it, and a PAM stack with no root can only run the `auth` chain anyway. See Greeter above |
| X11 or i3 | The target is a Wayland session shell |
