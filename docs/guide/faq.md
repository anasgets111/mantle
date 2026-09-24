# FAQ

Symptoms a newcomer hits whose cause lives on another page. Find the symptom, read the cause, and
follow the link for the fix. Traps that stay within one topic are in that page's Gotchas table.

## First steps when something is wrong

| Step | Command | Tells you |
| :--- | :--- | :--- |
| 1 | `mantle check` | Syntax and top-level errors, with file and line. It never lays out nodes ([what check covers](cli.md#what-check-covers)) |
| 2 | `mantle log` | Evaluation errors, layout errors and refused `mantle set`/`toggle` writes ([output and logging](runtime.md#output-and-logging)) |
| 3 | Restart with `mantle -vv`, then `mantle log -f` | Errors raised inside callbacks, which print only at debug level ([log levels](cli.md#flags)) |
| 4 | `MANTLE_DUMP_LAYOUT=<id>@<output> mantle -vvv` | Every visible node's kind and rect on that surface after each pass ([how do I](cli.md#how-do-i)) |

## Nothing shows

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| No surface appears at all after starting | The startup evaluation raised, so there is no scene. The error is in the log | [Evaluation, reload and generations](runtime.md#evaluation-reload-and-generations) |
| `mantle check` passes, but a surface is empty or lays out wrong | `check` evaluates `shell.lua` and checks each surface's own fields. It never builds the node tree, so node property errors, map errors and sizes are not checked. The running shell reports them in `mantle log` | [What check covers](cli.md#what-check-covers), then the [layout dump](cli.md#how-do-i) and the [layout model](../nodes/index.md) |
| An `image`, `capture`, `shader` or `textfield` is invisible | They have no intrinsic size | [nodes](../nodes/index.md) |
| A `"Fill"` child or a `"50%"` size is 0 | The parent is content-sized on that axis | [nodes](../nodes/index.md) |
| A bar anchored to both edges paints its background only behind its content | The surface spans the edges, but its root node is content-sized | [panel](../surfaces/panel.md) |
| A node shows before its data arrives | The map returns `nil` for `visible`, which counts as absent, and `visible` defaults to `true` | [signals gotchas](signals.md#gotchas) |
| A `blur = true` panel shows no blur | The compositor lacks the blur protocol. Nothing is raised | [Blurs](paint.md#blurs) |
| A shader draws nothing | `check` does not compile GLSL. The compile error is in `mantle log` | [shader](../nodes/shader.md) |
| Two bars on screen | Two shells are running, one per `mantle` start | [cli gotchas](cli.md#gotchas) |
| The shell vanishes and comes back only after 30 s | The Renderer died three times within 60 s, so the next respawn waits | Fix the error in `mantle log` ([limits](runtime.md#limits-and-budgets)) |

## A save or a click does nothing

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| Saving a file leaves the old UI on screen | The reload failed. An evaluation error keeps the previous scene and sets `mantle.rescue`; a layout error keeps it and only logs a warning | [Find out why a reload did nothing](runtime.md#how-do-i) |
| The bar shows an old version and `mantle.rescue.is_rescue` is `true` (rescue mode) | Set by a failed evaluation, a failed startup apply, or a refused or lost session lock. The scene on screen is the last one that applied. The next successful evaluation clears it | [Evaluation, reload and generations](runtime.md#evaluation-reload-and-generations) (error banner), [renderer members](../capabilities/index.md) |
| After a broken save, `mantle call` says the action does not exist, timers stop and `on_change` goes quiet | A failed reload drops every action, timer, handler and idle threshold the last evaluation registered | [Evaluation, reload and generations](runtime.md#evaluation-reload-and-generations) |
| An `on_change`, `timer`, `process.run`, `palette` or idle callback does nothing and nothing is logged | Its error, a blown CPU budget included, is logged at debug level only. Restart with `mantle -vv` or `MANTLE_LOG=lua=debug`, then read `mantle log`. Errors in `on_click` and other input handlers are warnings, visible by default | [Output and logging](runtime.md#output-and-logging) |
| `process.run` prints nothing and `exit_cb` gets `nil` | The spawn failed, usually a command not on `PATH`. The reason is logged at debug level only | [process.run](processes.md#processrun) |
| `mantle.<cap>:invoke(...)` returns `nil` and nothing changes | `invoke` is fire and forget; a wrong argument type or count is dropped with a log line | [actions](../capabilities/index.md#actions) |
| A keybind running `mantle set` or `mantle toggle` does nothing | Refusals (an undeclared name, a bare `toggle` on a non-boolean) go to `mantle log`, not the exit code | [cli gotchas](cli.md#gotchas) |
| Saving a `.json` or an image beside `shell.lua` does not reload | Only `.lua` and `.frag` changes reload; a byte-identical save and an unreadable directory (`changes inside it will not reload`) do not either | [Evaluation, reload and generations](runtime.md#evaluation-reload-and-generations) |
| An edit to `fonts { ... }` does nothing | The font chain is read when the Renderer starts | [fonts](scripting.md#fonts) |
| A `textfield` shows no caret and takes no keys | The panel does not take keyboard focus | [text fields](input.md#text-fields), [panel](../surfaces/panel.md) |
| An exit animation never plays | The node is hidden instead of removed, or the surface unmaps first | [Exit](animation.md#exit) |

## Values are wrong or stale

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| A map raises `attempt to index a nil value` at startup, or a capability reads `nil` | Every capability reads `nil` until its first push, for the whole of `mantle check`, and for good when its backend is missing | [The one rule](signals.md#the-one-rule), [capabilities](../capabilities/index.md) |
| `mantle.sysinfo` stays `nil` | It reads nothing until configured | [sysinfo](../capabilities/sysinfo.md) |
| A field inside a capability payload is `nil` | A JSON `null` arrives as an absent key. Optional fields need their own guard | [capabilities](../capabilities/index.md) |
| A text never updates | It holds a `:get()` snapshot, not the signal | [The one rule](signals.md#the-one-rule) |
| `` `margin.left` is a Signal handle `` | Signals nested in a property table do not resolve | [signals gotchas](signals.md#gotchas) |
| A named state resets on every reload | Its scalar seed changed between evaluations | [Named state](signals.md#named-state) |
| A setting is lost after the shell restarts | Named state lives in the Renderer and dies with it | [persistent_table](scripting.md#persistent_table) |
| A persisted key reads the old value right after `:set` | The signal updates on the next push | [persistent_table](scripting.md#persistent_table) |
| A switched view keeps old state, or snaps in without its animation | `visible = false` freezes the subtree in place; two id-less views of the same kind are reused | [Switching views](signals.md#switching-views), [nodes](../nodes/index.md) |
| A `pulse` of a capability fires on every push, or a `delay` of one never settles | Each push is a fresh table, so it never compares equal | [signals gotchas](signals.md#gotchas) |

## Errors in the log

| Message | Cause | Fix |
| :--- | :--- | :--- |
| `surface 2 is a string, not a node` | `require` returned the module and its path into the surface list | [Modules and require](runtime.md#modules-and-require) |
| `exceeded the 5ms CPU budget for one evaluation` | A map, `computed`, handler or timer did too much work | [Limits and budgets](runtime.md#limits-and-budgets) |
| `signal nesting exceeded its maximum depth of 32 levels` | A derived chain reads itself or nests too deep | [Errors](signals.md#errors) |
| `a Signal resolved to another Signal` | A map returned a signal | [Errors](signals.md#errors) |
| `` `mantle` asked to write state ... and was refused `` | `mantle set`/`toggle` named an undeclared state, or wrote a value it refuses | [Values and arguments](cli.md#values-and-arguments) |

## Running processes

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| A program runs twice after a save | A top-level `process.run` starts again on every reload beside the old child | [session_process](processes.md#session_process), [What survives a reload](runtime.md#what-survives-a-reload) |
| `~`, globs or pipes in a command do nothing | `process.run` runs no shell | [process.run](processes.md#processrun) |
| A poll loop speeds up after a few saves | The timer was re-armed from `exit_cb` of a child that outlived the reload | [Poll a command every N seconds](processes.md#poll-a-command-every-n-seconds) |

## Capabilities

A capability starts only when the config reads `mantle.<name>`, so a config that never touches it
has no server, watcher or agent at all. Run the shell with `-v` (or `-vv`) to see the log lines
quoted here.

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| A capability always reads `nil` | Its backend is missing ([requirements](installation.md#requirements)), or the config reads it only inside a callback that never ran | Check the log for its start line; read `mantle.<name>` at config top level |
| `battery.present` is `false` | UPower's display device is not a present battery (desktop, or battery not detected), or UPower is not running (`state` `"Unknown"`) | Expected on desktops; otherwise start `upower.service`, then restart the shell; `upower -d` should list `DisplayDevice` |
| No tray icons | Config never reads `mantle.tray`; another host (waybar, snixembed) owns `org.kde.StatusNotifierWatcher`; the app is XEmbed-only; or it registered at an unlisted path before the shell started | Read `mantle.tray`; stop the other host; restart the app so it registers again |
| Notifications not showing | Another daemon (mako, dunst, swaync) owns `org.freedesktop.Notifications` (`another notification daemon already owns this name`), or the config never reads `mantle.notifications` | Stop and disable the other daemon, then restart the shell. `busctl --user status org.freedesktop.Notifications` names the owner |
| No notification sound | DND or quiet on (critical still plays); the app is muted; the file is not Ogg Vorbis or 16-bit WAV, is over 4 MiB or 30 s, or lies outside the sound roots; `sound-name` with no tier sound registered | [notifications backend](../capabilities/notifications.md#backend) |
| Polkit prompts not appearing | Config never reads `mantle.polkit`, so the agent never registers; another agent (polkit-gnome, hyprpolkitagent) registered first; `$XDG_SESSION_ID` unset | Read `mantle.polkit`; stop the other agent and restart the shell; start the session through logind |
| Polkit authentication always fails | `/run/polkit/agent-helper.socket` is missing, so the helper cannot run | Check that the installed polkit provides that socket |
| Idle never fires | A block-mode idle inhibitor is held (`systemd-inhibit --list`), a browser or player holds ScreenSaver, a Wayland surface inhibitor is up, or the config's own `inhibit` is still held; or the compositor lacks `ext_idle_notifier_v1` | `mantle.idle.inhibited` and `inhibitors` name the holder (empty `who` is the compositor). Set `IdleAction=ignore` in `logind.conf` so logind does not act too |
| Unlock refuses the right password | PAM stack `login` in use and `pam_nologin` or `pam_shells` refusing | Install the `mantle` PAM stack ([install](installation.md#install)) |
| Locked session with no lock screen | The Renderer died and its replacement could not retake the lock (`could not take the session lock over`) | Switch VT and unlock through the compositor's own mechanism |
| Keyboard layout switch does nothing | Not niri or Hyprland; only one layout configured (`layout_count` 1); on Hyprland it sends `switchxkblayout main <i>` to the keyboard marked `main`, which likely fails on Hyprland 0.56+, whose socket parses Lua | Configure several layouts in the compositor; on Hyprland 0.56+, switch through a compositor keybind |
| Caps/Num Lock always `false` | No readable `/dev/input` keyboard with LEDs and no sysfs LED | Give the user read access to the input device |
| `brightness` reads `nil` | No `/sys/class/backlight` device; external monitors are not covered | None; `brightness` is backlight-only |
| Brightness writes ignored | Session not active (another VT), so logind refuses `SetBrightness` | Switch back to the session |
| Wi-Fi or `network` stays `nil` | NetworkManager not running | Start it, then restart the shell; a reload does not retry |
| Bluetooth inert | `bluetoothd` started after the shell | Restart the shell |
| Pairing prompt never shows | The adapter is not visible and this shell did not start the pairing, or the device asks for a PIN or passkey entry (rejected) | Make the adapter visible or pair from the shell |
| Volume or app list missing | PipeWire unreachable when `audio` started; no reconnect | Restart the shell after PipeWire is up |
| Workspaces `nil` | Neither `$NIRI_SOCKET` nor `$HYPRLAND_INSTANCE_SIGNATURE` in the Supervisor's environment | Launch `mantle` from the compositor session |
| Updates never check | `configure` not called (dormant), or no `pacman` on `PATH` (`package_manager` `nil`) | Call `configure` with an interval |

See also: [runtime](runtime.md), [cli](cli.md), [signals](signals.md), [processes](processes.md),
[capabilities](../capabilities/index.md), [glossary](../glossary.md) (rescue, hydration, generation).

Source: [reload and rescue](../../renderer/src/socket/client/mod.rs),
[apply](../../renderer/src/socket/client/resolve.rs), [check](../../renderer/src/check.rs),
[callback logging](../../renderer/src/lua/capability.rs), [log subsystems](../../shared/src/log.rs).
