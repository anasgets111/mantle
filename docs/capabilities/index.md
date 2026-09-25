# Capabilities

`mantle.<name>` reads one slice of the system (audio, network, battery, workspaces and the rest) as
a read-only signal, and its action methods ask its backend to act. This page holds the rules every
capability shares; each capability's page holds its state, actions and backend.

```lua
button {
    on_click = function() mantle.audio:toggle_mute() end,
    on_wheel = function(_, steps)
        local audio = mantle.audio:get()
        if audio and audio.volume then
            mantle.audio:set_volume(audio.volume + steps * 0.05) -- clamped to [0, 1.5]
        end
    end,
    children = {
        text {
            content = mantle.audio:map(function(audio)
                if audio == nil or audio.volume == nil then
                    return "--" -- nil before the first push; no volume without a default sink
                end
                return audio.muted and "muted" or string.format("%d%%", math.floor(audio.volume * 100 + 0.5))
            end),
        },
    },
}
```

## Reading and acting

A capability is a [signal](../guide/signals.md) the backend writes: pass it, or a `:map` of it, to
a property and the property stays live. Each push replaces the whole snapshot.

| Member | Contract |
| :--- | :--- |
| `:get()` | The last pushed snapshot; `nil` before the first |
| `:map(fn)` | Derived signal; `fn` must handle `nil`. A capability also works as a `computed` dependency |
| `:on_change(fn)` | `fn(current, previous)` once per push, after it lands; `previous` is `nil` on the first. Runs under the 5 ms callback budget and may call actions, `process.run` or write state. A raise logs a warning and the next handler still runs. Every evaluation clears them before `shell.lua` registers its own |
| `:<action>(...)` | One method per action on the capability's page, e.g. `mantle.audio:set_volume(0.5)`. Queues one command and returns nothing. Read the state it changes for the outcome. Call it with `:`; a `.` call raises |

There is no `:set` on the state; `mantle.brightness:set` and `mantle.storage:set` are actions. `mantle.idle` has no actions; it takes [methods](idle.md#methods) instead.

### Lifecycle

| Stage | What happens |
| :--- | :--- |
| First read of `mantle.<name>` | The Supervisor starts that backend, once. `mantle.idle` starts on its first method call, `:get`, `:map` and `:on_change` included |
| Before the first push | Every read is `nil`. A missing backend may keep it `nil` for good |
| Running | A started backend runs for the Supervisor's lifetime. Its state survives reloads and Renderer replacement: a new generation gets every last snapshot replayed ([hydration](../glossary.md#capabilities)) |
| Shared readers | `audio` and `privacy` share one PipeWire thread; `workspaces` and `windows` share one niri/Hyprland reader. Whichever is read first starts it |
| Buses | One system-bus connection for all. `tray`, `notifications`, `mpris` and `idle` each open their own session bus. Every D-Bus method call times out after 25 s |
| Pushes | On backend events. `system`, `sysinfo`, `updates`, notification expiry, mpris's position recheck and the `brightness` fallback also run timers |
| Renderer replaced | The departed generation's Bluetooth discovery stops, its pending Wi-Fi prompt is cancelled, its `files` watches, idle thresholds and inhibits are dropped, and its `process.run` children are reaped ([processes](../guide/processes.md#processrun)). An in-place reload keeps the generation |
| Missing backend | Logged; the capability goes inert or stays `nil`. Each page's Backend section says which. Only `network` retries: a failed NetworkManager connection is rebuilt on the next start, which each new generation sends |
| Built at boot | `lock`, so the session can relock after a Renderer dies. The `polkit` controller also exists at boot, but its agent registers on the first read of `mantle.polkit` or a `secure_submit` naming it |
| Unknown name | `mantle.audioo` is plain `nil`, so the `:get()` after it raises on that line |

### Actions

Arguments are positional, in the order each page's Actions table lists them, and JSON-shaped:
numbers, strings, booleans and tables. The Renderer checks the action name and marshalling; the
Supervisor checks types and count.

| Mistake | Result |
| :--- | :--- |
| A state field read off the capability (`mantle.audio.volume`) | Raises at the read: `did you mean mantle.audio:get().volume?` |
| A misspelled action (`mantle.audio:set_volum(1)`) | Raises at the read: `did you mean mantle.audio:set_volume(...)?` |
| Any other unknown name | Raises at the read, listing the actions the capability takes |
| Any other method but `get`, `map` and `on_change` on `battery`, `privacy` or `system` | Raises: they have no actions |
| A function or userdata argument | Raises at the call, naming its slot |
| Wrong type or argument count | Logged (`mantle log`) and dropped |
| A float where an `integer` goes | Dropped: `5.0` is refused, `5` works. `math.floor(x + 0.5)` returns an integer |
| Arguments to an action that takes none | Dropped: `mantle.network:scan(1)` is refused |

A trailing `nil` counts as omitted, so an optional last argument can be passed as `nil`.

| Convention | Rule |
| :--- | :--- |
| Targets | Pass the ID from the snapshot (`sinks[].id`, `feed[].id`, `players[].id`, `windows[].id`). IDs are opaque: compare them, never build them |
| Volume | `1.0` is 100% |
| Percentages | Integers `0` to `100` |
| Indices | Zero-based |

[`lua-meta/mantle.lua`](../../lua-meta/mantle.lua) is generated from the same Rust types as the
pages. On the LuaLS library path, `mantle.audio:get().` completes fields and
`mantle.audio:` offers every action, and a wrong argument type is a warning.

## Capability list

| Name | What it gives you | Note |
| :--- | :--- | :--- |
| [`applications`](applications.md) | Desktop entries, window `app_id` index, launching | |
| [`audio`](audio.md) | Output and input volume and mute, devices, per-app streams, Bluetooth codecs | |
| [`battery`](battery.md) | Charge, state, time estimates | Check `present` first |
| [`bluetooth`](bluetooth.md) | Adapter power, devices, discovery, pairing prompts | |
| [`brightness`](brightness.md) | Screen backlight percentage | `nil` without a backlight |
| [`files`](files.md) | Listings of watched folders | Lists nothing until `watch` |
| [`idle`](idle.md) | Who holds the session awake; idle thresholds and inhibits | Methods, not actions |
| [`keyboard`](keyboard.md) | Active layout, lock keys, keyboard backlight | `nil` until the compositor reports a layout or a lock key changes |
| [`lock`](lock.md) | Lock held, authentication progress, last failure | No `unlock`: only a correct password unlocks |
| [`mpris`](mpris.md) | Media players, metadata, position | |
| [`network`](network.md) | Connectivity, Wi-Fi scan, join progress | Secured joins take the key through `secure_submit` |
| [`notifications`](notifications.md) | The newest 20 notifications, do-not-disturb | Mantle is the notification daemon |
| [`polkit`](polkit.md) | The pending authentication request | Mantle is the polkit agent; the password goes through `secure_submit` |
| [`power`](power.md) | Power profiles, on battery, power draw | |
| [`privacy`](privacy.md) | Apps using the camera, microphone or screen capture | |
| [`processes`](processes.md) | Programs declared with `session_process` | Use [`session_process`](../guide/processes.md#session_process), not its actions |
| [`storage`](storage.md) | Each `persistent_table` file | Use [`persistent_table`](../guide/scripting.md#persistent_table), not its actions |
| [`sysinfo`](sysinfo.md) | CPU, memory, swap, temperatures | `nil` until `configure` |
| [`system`](system.md) | Wall and monotonic clocks, once a second | |
| [`tray`](tray.md) | Tray items, artwork, menus | Mantle hosts the StatusNotifierWatcher |
| [`updates`](updates.md) | Pending packages, install progress, reboot needed | No schedule until `configure` |
| [`windows`](windows.md) | Every toplevel: title, app ID, workspace, output, state | |
| [`workspaces`](workspaces.md) | Per-output workspaces, specials, focused window | niri or Hyprland only |

### Renderer members

Four members come from the Renderer, not a backend, so they are never `nil` and start nothing.

| Member | Kind | Contract |
| :--- | :--- | :--- |
| `mantle.screens` | Signal | Connected outputs, one [`Screen`](#screen) each. Starts as `{}`, so a loop runs zero times before the first output arrives. An output with no known size is left out |
| `mantle.rescue` | Signal | `{ is_rescue, error_log }`. `is_rescue` turns `true` when an evaluation raises, a scene fails to apply, a live update fails, a reload renames the lock surface while locked, or the session lock is refused or torn down; `error_log` holds the reason, ready to draw. The next reload that applies clears it, and so does a later pass after a failed startup apply or live update |
| `mantle.version` | Plain table | `{ major, minor, patch }` integers, for guarding newer API |
| `mantle.config_dir` | Plain string | Directory `shell.lua` was loaded from, for naming files shipped beside it |

#### `Screen`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `name` | `string` | Connector name, e.g. `"eDP-1"`, as a surface's `monitor` takes it; `"output-N"` below `wl_output` v4 |
| `x` | `integer` | Left edge in compositor space: `xdg_output`'s logical position, else `wl_output`'s |
| `y` | `integer` | Top edge, on the same terms as `x` |
| `width` | `integer` | Logical pixels, already divided by scale; the mode's pixels when no logical size is known |
| `height` | `integer` | Logical pixels, on the same terms as `width` |
| `scale` | `integer` | Integer scale factor, e.g. `2` on HiDPI |
| `fractional_scale` | `number` | Real scale, e.g. `1.5`: mode width over logical width; `scale` without both |
| `refresh` | `number` | Refresh rate in Hz; `0` without a current mode, e.g. a virtual output |
| `orientation` | `Orientation` | The `wl_output` transform |
| `model` | `string` | Monitor model, e.g. `"DELL U2720Q"`; stable across connector renames. Empty when unadvertised |
| `description?` | `string` | The compositor's human label; format varies (Hyprland's has the serial). Absent below `wl_output` v4 |

#### `Orientation`

| Value | Meaning |
| :--- | :--- |
| `"normal"` | No transform |
| `"90"`, `"180"`, `"270"` | Rotated that many degrees counter-clockwise |
| `"flipped"` | Mirrored around a vertical axis, no rotation |
| `"flipped_90"`, `"flipped_180"`, `"flipped_270"` | Mirrored, then rotated that many degrees counter-clockwise |

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a value that may not have arrived yet | Guard `nil` in the map: [battery label](battery.md#how-do-i) |
| Change volume or brightness with the wheel | `on_wheel` plus `:get()` and an action, as in the example above; [brightness](brightness.md) |
| Show an OSD when volume changes | `:on_change` writing state: [Volume OSD](../cookbook/volume-osd.md) |
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
| `attempt to index a nil value` in a `:map` at startup | Guard the whole payload before its fields |
| An optional field is `nil` | A JSON `null` arrives as an absent key. Fields marked `?` need their own guard (`audio.volume` with no default sink) |
| `local ok = mantle.audio:set_volume(...)` is always `nil` | Bind the state the action changes; read `mantle log` for dropped commands |
| An action silently does nothing | Wrong argument type or count, often a float where an `integer` goes (`brightness:set(50.0)`). Check `mantle log` |
| `on_change` fires at startup with `previous == nil` | That push is learned state, not a change; return early. A replacement Renderer gets every snapshot replayed the same way. An in-place reload keeps the last value, so its next push has a real `previous` |
| `on_change` fires with nothing visibly changed | Every push carries the whole snapshot. Compare the fields you care about |

See also: [signals](../guide/signals.md) for `:map`, `computed` and named state;
[input](../guide/input.md) for click and wheel handlers; [installation](../guide/installation.md#requirements)
for what each backend needs.

Source: [namespace](../../renderer/src/lua/namespace.rs), [capability](../../renderer/src/lua/capability.rs),
[idle](../../renderer/src/lua/idle.rs), [screens](../../renderer/src/wayland/output.rs),
[lazy start and dispatch](../../supervisor/src/capabilities/lifecycle.rs),
[argument decoding](../../supervisor/src/action.rs), payload and action types under
[`supervisor/src/capabilities/`](../../supervisor/src/capabilities/).
