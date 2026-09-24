# Capabilities

`mantle.<name>` is how a config reads the system and asks it to act: audio, network, battery,
workspaces, notifications and the rest. Each one is a read-only signal over one backend's state,
plus `:invoke` to send it an action. Reach for this page whenever a widget shows system state or a
click changes it. Each capability's page ends its reference with a Backend section: which D-Bus
service or files it uses, and how it behaves when they are missing.

```lua
local battery_text = text {
    content = mantle.battery:map(function(battery)
        if battery == nil or not battery.present then
            return "" -- nil until the first push; present = false on a desktop
        end
        return string.format("%d%%", battery.percent)
    end),
}
```

## Reading and acting

A capability works like any [signal](../guide/signals.md): pass it, or a `:map` of it, to a property and the
property stays live. The difference is where the value comes from: the backend pushes a whole new
snapshot whenever its state changes, and the config never writes it.

| Member | Contract |
| :--- | :--- |
| `:get()` | Snapshot of the last push; `nil` before the first |
| `:map(fn)` | Derived signal; `fn` must handle `nil`. Capabilities also work as `computed` dependencies |
| `:on_change(fn)` | `fn(current, previous)` once per pushed snapshot, after the push; `previous` is `nil` on the first. Runs under the 5 ms callback budget and may `:invoke`, `process.run` or write state. A raise is logged as a warning and the next handler still runs. Cleared before every evaluation |
| `:invoke(action, ...)` | Queues one command and returns nothing. Observe state for the outcome |

There is no `:set`: capability state is read-only.

**Lifecycle.** Nothing runs until the config asks for it.

| Stage | What happens |
| :--- | :--- |
| First read of `mantle.<name>` | The name moves onto the table and the Supervisor starts that backend once. `mantle.idle` starts on its first method call instead |
| Before the first push | Every read is `nil`. A missing backend (no compositor, no backlight) may keep it `nil` for good |
| Running | A started backend runs for the Supervisor's lifetime. State survives reloads and Renderer replacement; a new generation gets every last snapshot replayed ([hydration](../glossary.md#capabilities)) |
| Shared readers | `audio` and `privacy` share one PipeWire thread; `workspaces` and `windows` share one niri/Hyprland reader. Whichever is read first starts it |
| Buses | One shared system-bus connection. `tray`, `notifications`, `mpris` and `idle` each open their own session bus. Every method call times out after 25 s |
| Updates | Event-driven where the backend signals; `system`, `sysinfo`, `updates` and the brightness fallback run timers |
| Generation leaves | Its Bluetooth discovery stops, a pending Wi-Fi prompt is cancelled, and its `files` watches, idle thresholds and inhibits are dropped. Its `process.run` children are reaped ([processes](../guide/processes.md#processrun)) |
| Missing backend | Logged; the capability goes inert or stays `nil` ([requirements](../guide/installation.md#requirements)). Only `network` retries: a failed NetworkManager build is rebuilt on the next start, which a Renderer sends once per generation |
| Boot exception | `lock` is built at boot so the session can relock. The `polkit` controller also exists at boot, but its agent registers only on first read (or a `secure_submit` naming it) |
| Unknown name | `mantle.audioo` is plain `nil`, so the next `:get()` raises on that line |

**Invoke.** Arguments are positional and must be JSON-shaped (numbers, strings, booleans, tables).
A function or userdata argument raises at the call, naming its slot, and so does an action name the
capability does not have, listing the ones it does. Arguments are checked by the Supervisor: a
wrong type or a wrong argument count is logged (`mantle log`) and dropped. A trailing `nil` counts
as omitted.

| Convention | Rule |
| :--- | :--- |
| Targets | Pass the ID from the snapshot: `sinks[].id`, `feed[].id`, `players[].id`, `windows[].id`. IDs are opaque; compare them, never build them |
| Volume | `1.0` is 100%. Master output `set_volume` clamps to `[0, 1.5]`; input, app streams to `[0, 1]` |
| Percentages | Integers `0` to `100` (`brightness` `set`, `keyboard` `set_backlight`) |
| Indices | Zero-based (`switch_layout`, codec `index`) |
| Integers | An `integer` argument refuses a float: `5.0` is dropped, `5` works. Round with `math.floor(x + 0.5)`, which returns an integer |
| No arguments | Pass none; `invoke("scan", 1)` is refused |

**Finding fields and actions.** [`lua-meta/mantle.lua`](../../lua-meta/mantle.lua) is generated from
the Rust payload and action types. With it on your LuaLS library path, `mantle.audio:get().` completes
fields, and typing `mantle.audio:invoke("` offers every action with its argument names and doc.

<a id="actions"></a>

## Capability list

Each page holds what the capability is for, an example, its state (the table `:get()` returns,
with every nested record type), every action, then recipes and gotchas. Actions are called as
`mantle.<name>:invoke("action", args...)`, positionally in the order shown. An unknown action name
raises; a no-op or refused call is logged, never returned.

| Name | What it gives you | Some actions (all on its page) | Backend | Notes |
| :--- | :--- | :--- | :--- | :--- |
| [`audio`](audio.md) | Output/input volume and mute, devices, per-app streams, Bluetooth codecs | `set_volume`, `toggle_mute`, `set_default_sink` | PipeWire native API | Shares a PipeWire thread with `privacy` |
| [`network`](network.md) | Connectivity, Wi-Fi scan, join progress, password requests | `scan`, `connect`, `set_wifi_enabled` | NetworkManager (system bus) | Secured joins take the key via `secure_submit` |
| [`bluetooth`](bluetooth.md) | Adapter power, connected, paired and discovered devices, pairing prompts | `start_discovery`, `connect`, `answer_pairing` | BlueZ `org.bluez` and an `Agent1` (system bus) | |
| [`notifications`](notifications.md) | Newest 20 notifications with spans and actions, DND | `dismiss`, `invoke_action`, `set_dnd` | Hosts `org.freedesktop.Notifications` (session bus) | |
| [`tray`](tray.md) | Tray items, artwork, menus | `activate`, `activate_menu_item`, `scroll` | Hosts `org.kde.StatusNotifierWatcher`; SNI items, DBusMenu (session bus) | |
| [`mpris`](mpris.md) | Players, metadata, position | `control`, `seek`, `seek_relative` | `org.mpris.MediaPlayer2.*` (session bus) | |
| [`workspaces`](workspaces.md) | Compositor name, per-output workspaces, specials, overview, focused window | `focus`, `toggle_special` | niri IPC or Hyprland sockets | niri or Hyprland only; `nil` otherwise |
| [`windows`](windows.md) | Every toplevel: title, app ID, workspace, output, state flags | `focus`, `close`, `set_fullscreen` | niri/Hyprland events, else `zwlr_foreign_toplevel_management_v1` | Falls back to wlr foreign-toplevel |
| [`keyboard`](keyboard.md) | Active layout, lock keys, keyboard backlight | `switch_layout`, `set_backlight` | evdev, logind, the compositor | |
| [`brightness`](brightness.md) | Screen backlight percentage | `set` | sysfs backlight, logind | `nil` without a backlight |
| [`power`](power.md) | Power profiles, on battery, power draw | `set_profile` | power-profiles-daemon, UPower | |
| [`battery`](battery.md) | Charge, state, time estimates | none | UPower | Read-only; check `present` first |
| [`privacy`](privacy.md) | Apps using camera, microphone, screen capture | none | `/proc/*/fd` scan for `/dev/video*`, PipeWire streams | Read-only |
| [`system`](system.md) | Wall clock and monotonic seconds, pushed once a second | none | Supervisor clock | Read-only |
| [`sysinfo`](sysinfo.md) | CPU, memory, swap, temperatures | `configure` | `/proc/stat`, `/proc/meminfo`, `/sys/class/hwmon/` | Stays `nil` until `configure` sets an interval |
| [`updates`](updates.md) | Pending packages, install progress, reboot needed | `configure`, `check`, `install` | libalpm, AUR RPC, `pkexec` | No schedule until `configure`; `check` works regardless |
| [`applications`](applications.md) | Desktop entries, window `app_id` index | `launch`, `refresh`, `open_url` | XDG `.desktop` files, inotify | |
| [`files`](files.md) | Listings of watched folders | `watch`, `unwatch` | inotify | Absolute paths only |
| [`lock`](lock.md) | Lock held, authentication progress, last failure | `lock`, `set_unlock_animation` | logind, PAM worker; the Renderer holds `ext_session_lock_v1` | No `unlock`: only a correct password unlocks |
| [`polkit`](polkit.md) | The pending authentication request | `cancel` | `org.freedesktop.PolicyKit1` authentication agent (system bus), polkit's agent helper | Password goes through `secure_submit` |
| [`processes`](processes.md) | Programs declared with `session_process` | `declare`, `start`, `stop` | Supervisor-owned child processes | Use `session_process` rather than invoking directly |
| [`storage`](storage.md) | Each `persistent_table` file | `open`, `set` | JSON files the config declares | Use `persistent_table` rather than invoking directly |
| [`idle`](idle.md) | Whether anything holds the session awake, and who | none; [methods](idle.md#methods) instead | `ext_idle_notifier_v1`, logind inhibit, hosts `org.freedesktop.ScreenSaver` (session bus) | |

`battery`, `privacy` and `system` have no actions: an `invoke` on them raises.

### Renderer members

Four members come from the Renderer, not a backend, so they are never `nil` and start nothing.

| Member | Kind | Contract |
| :--- | :--- | :--- |
| `mantle.screens` | Signal | Connected outputs (`name`, `x`, `y`, `width`, `height`, `scale`, ...). Seeded `{}`, so a loop runs zero times before the first output arrives |
| `mantle.rescue` | Signal | `{ is_rescue, error_log }`. `is_rescue` turns `true` when an evaluation (startup or reload) raises, its scene fails to apply, a live update fails, or the session lock is refused or ends; `error_log` holds the drawable reason. The next reload that applies clears it, and so does a later pass after a failed startup apply or live update |
| `mantle.version` | Plain table | `{ major, minor, patch }` integers, for guarding newer API |
| `mantle.config_dir` | Plain string | Directory `shell.lua` was loaded from, for naming files shipped beside it |

Each `mantle.screens` entry:

#### `Screen`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `name` | `string` | Connector name, e.g. `"eDP-1"`, as a surface's `monitor` takes it; `"output-N"` below `wl_output` v4. |
| `x` | `integer` | Left edge in compositor space: `xdg_output`'s logical position, else `wl_output`'s. |
| `y` | `integer` | Top edge, on the same terms as `x`. |
| `width` | `integer` | Logical pixels (already divided by scale); the mode's pixels when no logical size is known. |
| `height` | `integer` | Logical pixels, on the same terms as `width`. |
| `scale` | `integer` | Integer scale factor, e.g. `2` on HiDPI. `width` and `height` are already logical. |
| `fractional_scale` | `number` | Real scale, e.g. `1.5`: mode width over logical width; `scale` without both. |
| `refresh` | `number` | Refresh rate in Hz; `0` without a current mode, e.g. a virtual output. |
| `orientation` | `Orientation` | The `wl_output` transform. |
| `model` | `string` | Monitor model, e.g. `"DELL U2720Q"`; stable across connector renames. Empty when unadvertised. |
| `description?` | `string` | The compositor's human label; format varies (Hyprland's has the serial). Absent below `wl_output` v4. |

#### `Orientation`

| Value | Meaning |
| :--- | :--- |
| `"normal"` | No transform. |
| `"90"` | Rotated 90 degrees counter-clockwise. |
| `"180"` | Rotated 180 degrees. |
| `"270"` | Rotated 270 degrees counter-clockwise. |
| `"flipped"` | Mirrored around a vertical axis, no rotation. |
| `"flipped_90"` | Mirrored, then rotated 90 degrees counter-clockwise. |
| `"flipped_180"` | Mirrored, then rotated 180 degrees. |
| `"flipped_270"` | Mirrored, then rotated 270 degrees counter-clockwise. |

## Idle

`mantle.idle` has no `:invoke`; it takes callbacks instead. Its methods, `register_threshold`,
`cancel_threshold`, `inhibit` and `release_inhibit`, are on [its page](idle.md#methods).

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a value that may not have arrived yet | Guard `nil` in the map: [battery label](battery.md#how-do-i) |
| Change volume or brightness with the scroll wheel | `on_wheel` plus `:get()` and `:invoke`: [volume](audio.md#how-do-i), [brightness](brightness.md) |
| Show an OSD when volume changes | `:on_change` writing state: [OSD example](audio.md#how-do-i) |
| Show a clock | `os.date` over `mantle.system.time`: [system](system.md) |
| Give each monitor its own bar and workspaces | A function `child` gets the connector name; match it in `workspaces.outputs`: [workspaces](workspaces.md) |
| Show CPU and memory use | `configure` once at top level, then map: [sysinfo](sysinfo.md) |
| Name or iconify the focused app | `workspaces.active_client.class` through `applications.by_app_id`: [applications](applications.md) |
| Play or pause whatever is playing | `control` on `players[1].id`: [mpris](mpris.md) |
| Show a microphone or camera indicator | [privacy](privacy.md) |
| Keep the screen awake (caffeine) | [idle](idle.md#how-do-i) |
| Know whether an action worked | Watch the state it changes: [a failed Wi-Fi join](network.md#how-do-i) |

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `attempt to index a nil value` in a `:map` at startup | Every capability is `nil` until its first push, and some stay `nil` (no backend). Guard the whole payload first |
| Optional field missing | A JSON `null` arrives as an absent key. Fields marked `?` in `lua-meta/mantle.lua` need their own guard (`audio.volume` is `nil` with no default sink) |
| `local ok = mantle.audio:invoke(...)` is always `nil` | `invoke` is fire-and-forget. Bind the state it changes; read `mantle log` for dropped commands |
| `unknown action; it takes ...` | A typo in the action name. Pick one from the list the error prints |
| An action silently does nothing | Wrong argument type or count, often a float where an `integer` goes (`brightness:invoke("set", 50.0)`). Round with `math.floor`, check `mantle log` |
| `on_change` fires at startup with `previous == nil` | That push is learned state, not a change; return early. A replacement Renderer gets every snapshot replayed the same way. An in-place reload keeps the last value, so its next push has a real `previous` |
| `on_change` fires with nothing visibly changed | It runs per push, and a push carries the whole snapshot. Compare the fields you care about |

See also: [signals](../guide/signals.md) for `:map`, `computed` and named state; [input](../guide/input.md) for click and wheel handlers; [scripting](../guide/scripting.md) for `persistent_table` and `timer`; [processes](../guide/processes.md) for `session_process`; [installation](../guide/installation.md#requirements) for what each backend needs.

Source: [namespace](../../renderer/src/lua/namespace.rs), [capability](../../renderer/src/lua/capability.rs),
[idle](../../renderer/src/lua/idle.rs), [lazy start and dispatch](../../supervisor/src/capabilities/lifecycle.rs),
[argument decoding](../../supervisor/src/action.rs), [idle holds](../../supervisor/src/capabilities/idle/controller.rs),
payload and action types under [`supervisor/src/capabilities/`](../../supervisor/src/capabilities/).
