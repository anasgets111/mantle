# FAQ

Symptoms whose cause lives on another page: find the symptom, then follow the link. A trap that
stays within one page is in that page's Gotchas table.

## First steps when something is wrong

| Step | Command | Tells you |
| :--- | :--- | :--- |
| 1 | `mantle check` | Syntax and top-level errors, with file and line. Node and layout errors as laid out with every capability `nil` ([what check covers](cli.md#what-check-covers)) |
| 2 | `mantle log` | Evaluation errors, layout errors, errors raised in callbacks, failed `mantle call`s and refused `mantle set`/`toggle` writes ([output and logging](runtime.md#output-and-logging)) |
| 3 | `MANTLE_DUMP_LAYOUT=<id>@<output> mantle -vvv` | Every visible node's kind and rect on that surface after each pass ([how do I](cli.md#how-do-i)) |

## Nothing shows

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| No surface appears at all after starting | The startup evaluation raised, so there is no scene. The error is in the log | [Evaluation, reload and generations](runtime.md#evaluation-reload-and-generations) |
| `mantle check` passes, but a surface is empty or lays out wrong | `check` lays out once with every capability `nil` on a 1920x1080 output, so a branch that needs capability data, or a smaller output, went unchecked. The running shell reports those in `mantle log` | [What check covers](cli.md#what-check-covers), then the [layout dump](cli.md#how-do-i) and the [layout model](../nodes/index.md) |
| A node shows before its data arrives | The map returns `nil` for `visible`, which counts as absent, and `visible` defaults to `true` | [signals gotchas](signals.md#gotchas) |
| Two bars on screen | Two shells are running, one per `mantle` start | [cli gotchas](cli.md#gotchas) |
| The shell vanishes and comes back only after 30 s | The Renderer died three times within 60 s, so the next respawn waits | Fix the error in `mantle log` ([limits](runtime.md#limits-and-budgets)) |

## A save or a click does nothing

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| Saving a file leaves the old UI on screen, and `mantle.rescue.is_rescue` is `true` | The reload failed: an evaluation or apply error keeps the previous scene, sets `mantle.rescue` and logs the error. The next reload that applies clears it | [Find out why a reload did nothing](runtime.md#find-out-why-a-reload-did-nothing), [error banner](runtime.md#evaluation-reload-and-generations) |
| After a broken save, `mantle call` says the action does not exist, timers stop and `on_change` goes quiet | A failed reload drops every action, timer, handler and idle threshold the last evaluation registered | [Evaluation, reload and generations](runtime.md#evaluation-reload-and-generations) |
| An `on_change`, `timer`, `process.run`, `palette` or idle callback does nothing | Its error, a blown CPU budget included, is a warning. Read `mantle log` | [Output and logging](runtime.md#output-and-logging) |
| `mantle.<cap>:invoke(...)` returns `nil` and nothing changes | `invoke` is fire and forget; a wrong argument type or count is dropped with a log line | [actions](../capabilities/index.md#actions) |
| A keybind running `mantle set` or `mantle toggle` does nothing | It was refused (an undeclared name, a bare `toggle` on a non-boolean). The compositor discards the error; `mantle log` keeps it | [cli gotchas](cli.md#gotchas) |
| Saving a `.json` or an image beside `shell.lua` does not reload | Only `.lua` and `.frag` changes reload; a byte-identical save and an unreadable directory (`changes inside it will not reload`) do not either | [Evaluation, reload and generations](runtime.md#evaluation-reload-and-generations) |
| An edit to `fonts { ... }` does nothing | The font chain is read when the Renderer starts | Restart the shell ([fonts](scripting.md#fonts)) |
| A `textfield` shows no caret and takes no keys | The panel does not take keyboard focus | [text fields](input.md#text-fields), [panel](../surfaces/panel.md) |

## Values are wrong or stale

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| A map raises `attempt to index a nil value` at startup, or a capability reads `nil` | Every capability reads `nil` until its first push, for the whole of `mantle check`, and for good when its backend is missing | [The one rule](signals.md#the-one-rule), [capabilities](../capabilities/index.md) |
| A text never updates | It holds a `:get()` snapshot, not the signal | [The one rule](signals.md#the-one-rule) |
| A setting is lost after the shell restarts | Named state lives in the Renderer and dies with it | [persistent_table](scripting.md#persistent_table) |
| A switched view keeps old state, or snaps in without its animation | `visible = false` freezes the subtree in place; two id-less views of the same kind are reused | [Switching views](signals.md#switching-views), [nodes](../nodes/index.md) |

## Errors in the log

| Message | Cause | Fix |
| :--- | :--- | :--- |
| `surface 2 is a string, not a node` | `require` returned the module and its path into the surface list | [Modules and require](runtime.md#modules-and-require) |
| `exceeded the 5ms CPU budget for one evaluation` | A map, `computed`, handler or timer did too much work | [Limits and budgets](runtime.md#limits-and-budgets) |
| `signal nesting exceeded its maximum depth of 32 levels` | A derived chain reads itself or nests too deep | [Errors](signals.md#errors) |
| `a Signal resolved to another Signal` | A map returned a signal | [Errors](signals.md#errors) |
| `` `margin.left` is a Signal handle `` | A signal nested in a property table does not resolve | [signals gotchas](signals.md#gotchas) |
| `` `mantle` asked to write state ... and was refused `` | `mantle set`/`toggle` named an undeclared state, or wrote a value it refuses | [Values and arguments](cli.md#values-and-arguments) |

## Running processes

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| `process.run` prints nothing and `exit_cb` gets `nil` | The spawn failed, usually a command not on `PATH`. `mantle log` has the reason | [process.run](processes.md#processrun) |
| A program runs twice after a save | A top-level `process.detach` launches again on every reload | [session_process](processes.md#session_process), [What survives a reload](runtime.md#what-survives-a-reload) |

## Capabilities

A capability [starts](../glossary.md#capabilities) on the config's first `mantle.<name>` read, so a
config that never reads it runs no server, watcher or agent. The quoted log lines need `-v` or `-vv`.

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| A capability reads `nil` or stays inert | Its backend is missing or started after the shell ([requirements](installation.md#requirements)); `sysinfo` and `updates` wait for `configure`; or the config reads `mantle.<name>` only inside a callback that never ran | Start the backend, then restart the shell: a reload does not retry. Call `configure` where the page says so. Read `mantle.<name>` at config top level |
| `battery.present` is `false` | UPower's display device is not a present battery (desktop, or battery not detected), or UPower is not running (`state` `"Unknown"`) | Expected on desktops; otherwise start `upower.service`, then restart the shell; `upower -d` should list `DisplayDevice` |
| No tray icons | Config never reads `mantle.tray`; another host (waybar, snixembed) owns `org.kde.StatusNotifierWatcher`; the app is XEmbed-only; or it registered at an unlisted path before the shell started | Read `mantle.tray`; stop the other host; restart the app so it registers again |
| Notifications not showing | Another daemon (mako, dunst, swaync) owns `org.freedesktop.Notifications` (`another notification daemon already owns this name`), or the config never reads `mantle.notifications` | Stop and disable the other daemon, then restart the shell. `busctl --user status org.freedesktop.Notifications` names the owner |
| No notification sound | DND or quiet on (critical still plays); the app is muted; the file is not Ogg Vorbis or 16-bit WAV, is over 4 MiB or 30 s, or lies outside the sound roots; `sound-name` with no tier sound registered | [notifications backend](../capabilities/notifications.md#backend) |
| Polkit prompts not appearing | Config never reads `mantle.polkit`, so the agent never registers; another agent (polkit-gnome, hyprpolkitagent) registered first; `$XDG_SESSION_ID` unset | Read `mantle.polkit`; stop the other agent and restart the shell; start the session through logind |
| Polkit authentication always fails | `/run/polkit/agent-helper.socket` is missing, so the helper cannot run | Check that the installed polkit provides that socket |
| Idle never fires | A block-mode idle inhibitor is held (`systemd-inhibit --list`), a browser or player holds ScreenSaver, a Wayland surface inhibitor is up, or the config's own `inhibit` is still held; or the compositor lacks `ext_idle_notifier_v1` | `mantle.idle.inhibited` and `inhibitors` name the holder (empty `who` is the compositor) |
| Unlock refuses the right password | PAM stack `login` in use and `pam_nologin` or `pam_shells` refusing | Install the `mantle` PAM stack ([install](installation.md#install)) |
| Locked session with no lock screen | The Renderer died and its replacement could not retake the lock (`could not take the session lock over`) | Switch VT and unlock through the compositor's own mechanism |
| Keyboard layout switch does nothing | Not niri or Hyprland; only one layout configured (`layout_count` 1); on Hyprland it sends `switchxkblayout main <i>` to the keyboard marked `main`, which likely fails on Hyprland 0.56+, whose socket parses Lua | Configure several layouts in the compositor; on Hyprland 0.56+, switch through a compositor keybind |
| Caps/Num Lock always `false` | No readable `/dev/input` keyboard with LEDs and no sysfs LED | Give the user read access to the input device |
| `brightness` reads `nil` | No `/sys/class/backlight` device; external monitors are not covered | None; `brightness` is backlight-only |
| Brightness writes ignored | Session not active (another VT), so logind refuses `SetBrightness` | Switch back to the session |
| Pairing prompt never shows | The adapter is not visible and this shell did not start the pairing, or the device asks for a PIN or passkey entry (rejected) | Make the adapter visible or pair from the shell |

See also: [runtime](runtime.md), [cli](cli.md), [signals](signals.md), [processes](processes.md),
[capabilities](../capabilities/index.md), [glossary](../glossary.md) (rescue, hydration, generation).

Source: [reload and rescue](../../renderer/src/socket/client/mod.rs),
[apply](../../renderer/src/socket/client/resolve.rs), [check](../../renderer/src/check.rs),
[callback logging](../../renderer/src/lua/capability.rs), [log subsystems](../../shared/src/log.rs).
