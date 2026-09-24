# Capabilities

`mantle.<name>` is how a config reads the system and asks it to act: audio, network, battery,
workspaces, notifications and the rest. Each one is a read-only signal over one backend's state,
plus `:invoke` to send it an action. Reach for this page whenever a widget shows system state or a
click changes it. Backend details (which D-Bus service, which files, update rates) live in
[services](../services.md).

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

A capability works like any [signal](signals.md): pass it, or a `:map` of it, to a property and the
property stays live. The difference is where the value comes from: the backend pushes a whole new
snapshot whenever its state changes, and the config never writes it.

| Member | Contract |
| :--- | :--- |
| `:get()` | Snapshot of the last push; `nil` before the first |
| `:map(fn)` | Derived signal; `fn` must handle `nil`. Capabilities also work as `computed` dependencies |
| `:on_change(fn)` | `fn(current, previous)` once per pushed snapshot, after the push; `previous` is `nil` on the first. Runs under the 5 ms callback budget and may `:invoke`, `process.run` or write state. A raise is logged at debug (`-vv`) and the next handler still runs. Cleared before every evaluation |
| `:invoke(action, ...)` | Queues one command and returns nothing. Observe state for the outcome |

There is no `:set`: capability state is read-only.

**Lifecycle.** Nothing runs until the config asks for it.

| Stage | What happens |
| :--- | :--- |
| First read of `mantle.<name>` | The name moves onto the table and the Supervisor starts that backend once. `mantle.idle` starts on its first method call instead |
| Before the first push | Every read is `nil`. A missing backend (no compositor, no backlight) may keep it `nil` for good |
| Running | A started backend runs for the Supervisor's lifetime. State survives reloads and Renderer respawns; a new generation gets every last snapshot replayed |
| Boot exception | `lock` is built at boot so the session can relock. The `polkit` controller also exists at boot, but its agent registers only on first read (or a `secure_submit` naming it) |
| Unknown name | `mantle.audioo` is plain `nil`, so the next `:get()` raises on that line |

**Invoke.** Arguments are positional and must be JSON-shaped (numbers, strings, booleans, tables).
A function or userdata argument raises at the call, naming its slot. Everything else is checked by
the Supervisor: an unknown action, a wrong type or a wrong argument count is logged (`mantle log`)
and dropped. A trailing `nil` counts as omitted.

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

## Capability list

| Name | What it gives you | Some actions ([all](#capability-reference)) | Notes |
| :--- | :--- | :--- | :--- |
| `audio` | Output/input volume and mute, devices, per-app streams, Bluetooth codecs | `set_volume`, `toggle_mute`, `set_default_sink` | Shares a PipeWire thread with `privacy`. [§6](../services.md#6-pipewire-and-privacy) |
| `network` | Connectivity, Wi-Fi scan, join progress, password requests | `scan`, `connect`, `set_wifi_enabled` | Secured joins take the key via `secure_submit`. [§4](../services.md#4-networkmanager) |
| `bluetooth` | Adapter power, connected, paired and discovered devices, pairing prompts | `start_discovery`, `connect`, `answer_pairing` | [§5](../services.md#5-bluez) |
| `notifications` | Newest 20 notifications with spans and actions, DND | `dismiss`, `invoke_action`, `set_dnd` | [§1](../services.md#1-notifications) |
| `tray` | Tray items, artwork, menus | `activate`, `activate_menu_item`, `scroll` | [§2](../services.md#2-system-tray) |
| `mpris` | Players, metadata, position | `control`, `seek`, `seek_relative` | [§3](../services.md#3-mpris) |
| `workspaces` | Compositor name, per-output workspaces, specials, overview, focused window | `focus`, `toggle_special` | niri or Hyprland only; `nil` otherwise. [§8](../services.md#8-workspaces-and-windows) |
| `windows` | Every toplevel: title, app ID, workspace, output, state flags | `focus`, `close`, `set_fullscreen` | Falls back to wlr foreign-toplevel. [§8](../services.md#8-workspaces-and-windows) |
| `keyboard` | Active layout, lock keys, keyboard backlight | `switch_layout`, `set_backlight` | [§11](../services.md#11-other-capabilities) |
| `brightness` | Screen backlight percentage | `set` | `nil` without a backlight. [§11](../services.md#11-other-capabilities) |
| `power` | Power profiles, on battery, power draw | `set_profile` | [§11](../services.md#11-other-capabilities) |
| `battery` | Charge, state, time estimates | none | Read-only; check `present` first. [§11](../services.md#11-other-capabilities) |
| `privacy` | Apps using camera, microphone, screen capture | none | Read-only. [§6](../services.md#6-pipewire-and-privacy) |
| `system` | Wall clock and monotonic seconds, pushed once a second | none | Read-only. [§9](../services.md#9-telemetry-and-clock) |
| `sysinfo` | CPU, memory, swap, temperatures | `configure` | Stays `nil` until `configure` sets an interval. [§9](../services.md#9-telemetry-and-clock) |
| `updates` | Pending packages, install progress, reboot needed | `configure`, `check`, `install` | No schedule until `configure`; `check` works regardless. [§11](../services.md#11-other-capabilities) |
| `applications` | Desktop entries, window `app_id` index | `launch`, `refresh`, `open_url` | Not watched; `refresh` rescans. [§11](../services.md#11-other-capabilities) |
| `files` | Listings of watched folders | `watch`, `unwatch` | Absolute paths only. [§11](../services.md#11-other-capabilities) |
| `lock` | Lock held, authentication progress, last failure | `lock`, `set_unlock_animation` | No `unlock`: only a correct password unlocks. [§7](../services.md#lock) |
| `polkit` | The pending authentication request | `cancel` | Password goes through `secure_submit`. [§7](../services.md#polkit) |
| `processes` | Programs declared with `session_process` | `declare`, `start`, `stop` | Use `session_process` rather than invoking directly. [§10](../services.md#10-processes) |
| `storage` | Each `persistent_table` file | `open`, `set` | Use `persistent_table` rather than invoking directly. [§12](../services.md#12-paths-and-persistence) |
| `idle` | Whether anything holds the session awake, and who | none; see [Idle](#idle) | [§7](../services.md#idle) |

`battery`, `privacy` and `system` have no actions: an `invoke` on them is logged and dropped.

### Renderer members

Four members come from the Renderer, not a backend, so they are never `nil` and start nothing.

| Member | Kind | Contract |
| :--- | :--- | :--- |
| `mantle.screens` | Signal | Connected outputs (`name`, `x`, `y`, `width`, `height`, `scale`, ...). Seeded `{}`, so a loop runs zero times before the first output arrives |
| `mantle.rescue` | Signal | `{ is_rescue, error_log }`. `is_rescue` turns `true` when an evaluation (startup or reload) raises, the startup scene fails to apply, or the session lock is refused or ends; `error_log` holds the drawable reason. The next successful evaluation clears it. A reload that evaluates but fails to apply only logs and leaves it `false` |
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

<a id="actions"></a>

## Capability reference

One sub-section per capability: what it is, a small example, its state (the table `:get()` returns,
with every nested record type), then every action. A field marked `?` may be absent (`nil`);
guard it. Actions are called as `mantle.<name>:invoke("action", args...)`, positionally in the
order shown, with `?` marking an argument you may omit. A no-op or refused call is logged, never
returned. `idle`'s state and methods are under [Idle](#idle).

### audio

PipeWire: output and input volume and mute, device lists, per-app streams and Bluetooth codecs. Backend: [services §6](../services.md#6-pipewire-and-privacy).

```lua
list {
    source = mantle.audio:map(function(audio)
        return audio and audio.sinks or {}
    end),
    key = function(sink) return tostring(sink.id) end,
    itemfn = function(sink)
        return button {
            padding = 6,
            background = sink.active and "#45475A" or "#1E1E2E",
            on_click = function() mantle.audio:invoke("set_default_sink", sink.id) end,
            children = { text { content = sink.name } },
        }
    end,
}
```

#### `AudioState`

`mantle.audio`'s payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `apps` | `AppStream[]` | Apps playing or recording audio, excluding pid-less streams, notification sounds, meters and monitor captures. |
| `balance?` | `number` | Default output balance, `-1.0` (left) to `1.0` (right); `nil` for mono or an unknown channel map. |
| `bluetooth` | `BluetoothCodecs[]` | BlueZ audio devices PipeWire knows, with their codecs, ordered by `device`. |
| `muted` | `boolean` | Default output mute; `false` with no default sink. |
| `sinks` | `AudioDevice[]` | Every output device. |
| `source_muted` | `boolean` | Default input (microphone) mute; `false` with no default source. |
| `source_volume?` | `number` | Default input volume, `1.0` is 100%; `set_source_volume` caps at `1.0`, another client may not. `nil` with no source or before its first volume report. |
| `sources` | `AudioDevice[]` | Every input device. |
| `volume?` | `number` | Default output volume, `0.0` to `1.5` (`1.0` is 100%), loudest channel; louder writes by other clients are pulled back to `1.5`. `nil` with no sink or before its first volume report. |

#### `AppStream`

One app's playback or recording stream. Streams without a pid are left out.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `binary?` | `string` | `application.process.binary`, e.g. `"firefox"`. |
| `icon?` | `string` | XDG icon name from `application.icon-name`, else `media.icon-name`, e.g. `"firefox"`. |
| `id` | `integer` | PipeWire node id, the first argument of `set_app_volume` and `set_app_muted`. |
| `muted` | `boolean` | Stream mute; `false` until `volume` is known. |
| `name?` | `string` | `application.name`, if the client set one. |
| `pid` | `integer` | Owning process id, from `application.process.id`. |
| `process_name?` | `string` | `/proc/<pid>/comm`, or `nil` if it was unreadable when the stream's properties were read. |
| `recording` | `boolean` | A capture stream, such as a call's microphone, rather than playback. |
| `volume?` | `number` | Stream volume, `1.0` is 100%; `nil` until PipeWire reports the stream's `Props`. |

#### `BluetoothCodecs`

One BlueZ audio device's codec choices, joined to `mantle.bluetooth` by MAC.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `active?` | `integer` | `index` of the active profile; `nil` before PipeWire reports it or when it is not in `codecs`. |
| `codecs` | `CodecProfile[]` | Available profiles that name a codec, ordered by `index`. |
| `device` | `integer` | PipeWire device id, the first argument of `set_bluetooth_profile`. |
| `mac` | `string` | MAC address from the `bluez_card.*` name, `_` turned to `:`. |

#### `CodecProfile`

One entry of `BluetoothCodecs::codecs`.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `codec` | `string` | Codec from the profile name, else its English description, e.g. `"AAC"`, `"LDAC"`, `"mSBC"`. |
| `description` | `string` | PipeWire's description, e.g. `"High Fidelity Playback (A2DP Sink, codec AAC)"`. |
| `index` | `integer` | Profile index, the second argument of `set_bluetooth_profile`. |

#### `AudioDevice`

One `sinks` or `sources` entry.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `active` | `boolean` | This is the default output or input; with no default known, the lowest `id` is. |
| `bus?` | `string` | `device.bus`, e.g. `"pci"`, `"usb"`, `"bluetooth"`. |
| `form_factor?` | `string` | `device.form-factor`, e.g. `"headset"`. |
| `icon?` | `string` | `device.icon-name` theme name, e.g. `"audio-card-analog"`. |
| `id` | `integer` | PipeWire node id, the argument of `set_default_sink`/`set_default_source`; not reboot-stable. |
| `name` | `string` | `node.description`, e.g. `"Built-in Audio Analog Stereo"`, else `node.nick`, else `node.name`. |
| `port?` | `string` | The active card route's `port.type`, e.g. `"headphones"`, `"hdmi"`, `"mic"`. |

Actions on `mantle.audio`:

| Action | Does |
| :--- | :--- |
| `set_volume(volume: number)` | Sets master output volume, clamped to `[0.0, 1.5]`. |
| `set_muted(muted: boolean)` | Sets master output mute. |
| `toggle_mute()` | Toggles master output mute. |
| `set_balance(balance: number)` | Sets default output balance, `-1.0` (left) to `1.0` (right), clamped; the louder side keeps its level. |
| `set_default_sink(id: integer)` | Makes this `sinks[].id` the default output. |
| `set_default_source(id: integer)` | Makes this `sources[].id` the default input. |
| `set_source_volume(volume: number)` | Sets default input volume, clamped to `[0.0, 1.0]`. |
| `set_source_muted(muted: boolean)` | Sets default input mute. |
| `toggle_source_mute()` | Toggles default input mute. |
| `set_app_volume(id: integer, volume: number)` | Sets an `apps[].id` stream's volume, clamped to `[0.0, 1.0]`. |
| `set_app_muted(id: integer, muted: boolean)` | Sets an `apps[].id` stream's mute. |
| `set_bluetooth_profile(device: integer, index: integer)` | Switches a `bluetooth[].device` to one of its `codecs[].index`. |

### network

NetworkManager: connectivity, Wi-Fi and wired state, scanned access points and join progress. Backend: [services §4](../services.md#4-networkmanager).

```lua
text {
    content = mantle.network:map(function(network)
        if network == nil then
            return ""
        elseif not network.connected then
            return "offline"
        elseif network.ssid == "Ethernet" then
            return "wired"
        end
        return string.format("%s %d%%", network.ssid or "", network.strength)
    end),
}
```

#### `NetworkState`

`mantle.network`'s payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `available_networks` | `AccessPointInfo[]` | NetworkManager's visible networks, re-read on every change: one per SSID, at most 20, ordered associated, then saved, then strongest. `{}` without Wi-Fi hardware. |
| `connect_error?` | `JoinError` | The last failed `connect`, or `nil` before any or after a success. Kept until the next `connect`, `cancel_connect` or `abort_connect`; check its `ssid` before showing it. |
| `connected` | `boolean` | A connection carries the default route; `false` means offline. |
| `connecting_ssid?` | `string` | The SSID `connect` is joining, or `nil`; clears on a verdict or `abort_connect`. |
| `ethernet_enabled` | `boolean` | A wired device is activated; `set_ethernet_enabled`'s read-back, unlike carrier. |
| `ethernet_ip?` | `string` | The first activated wired device's IPv4 address without prefix, or `nil`. |
| `ethernet_present` | `boolean` | At least one wired device exists, cable or not. |
| `ethernet_speed` | `integer` | That wired device's link speed in Mb/s; `0` when unknown or none is activated. |
| `networking_enabled` | `boolean` | NetworkManager networking is on (`NetworkingEnabled`). |
| `password_ssid?` | `string` | The SSID whose `connect` waits for a password from a `network`/`connect` secure field, or `nil`. Also set after a rejected key; cleared when a join starts or by `cancel_connect`. |
| `scanning` | `boolean` | A scan is in flight, from the moment `scan` is accepted. |
| `ssid?` | `string` | `"Ethernet"` when the default route is wired, else the associated SSID, else `nil`. An association still getting an address has an `ssid` while `connected` is `false`. |
| `strength` | `integer` | The associated network's `strength`, `0` to `100`; `0` without a Wi-Fi association. |
| `wifi_enabled` | `boolean` | Wi-Fi radio power (`WirelessEnabled`); can be `true` with no Wi-Fi hardware, see `wifi_present`. |
| `wifi_ip?` | `string` | The Wi-Fi device's IPv4 address without prefix, or `nil`. |
| `wifi_present` | `boolean` | A Wi-Fi device exists. |

#### `AccessPointInfo`

One scanned network in `available_networks`.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `active` | `boolean` | The Wi-Fi device is associated with this SSID. |
| `band` | `string` | `"2.4 GHz"`, `"5 GHz"`, `"6 GHz"`, or empty for a frequency outside those bands. |
| `saved` | `boolean` | A saved NetworkManager profile names this SSID, so `connect` asks for no password. |
| `secure` | `boolean` | Needs a key: WEP, WPA or RSN. |
| `ssid` | `string` | Network name; one entry per SSID, from its strongest access point. |
| `strength` | `integer` | Signal strength, `0` to `100`. |

#### `JoinError`

A failed join, as `connect_error`.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `message` | `string` | Display text, such as `"wrong password"` or `"network not found"`. |
| `ssid` | `string` | The network the join was for. |

Actions on `mantle.network`:

| Action | Does |
| :--- | :--- |
| `set_networking_enabled(enabled: boolean)` | Turns NetworkManager networking on or off. |
| `set_wifi_enabled(enabled: boolean)` | Powers the Wi-Fi radio. |
| `set_ethernet_enabled(enabled: boolean)` | `false` disconnects every wired device; `true` activates each one's autoconnect profile, and a device without one stays down. |
| `scan()` | Requests a Wi-Fi scan; a no-op without Wi-Fi hardware. |
| `connect(ssid: string, hidden: boolean)` | Joins a network. Without a saved profile, a secured, `hidden` or out-of-range one sets `password_ssid` and waits for a key. |
| `cancel_connect()` | Drops the password request `password_ssid` names; a join already running continues. |
| `abort_connect()` | Stops the join `connecting_ssid` names, deleting a profile the join created. |
| `forget(ssid: string)` | Deletes every saved profile for this SSID. |
| `disconnect_wifi()` | Disconnects Wi-Fi; NetworkManager does not autoconnect it again until the next join. |

### bluetooth

BlueZ: adapter power, discovery, connected, paired and discovered devices, and pairing prompts. Backend: [services §5](../services.md#5-bluez).

```lua
list {
    source = mantle.bluetooth:map(function(bluetooth)
        return bluetooth and bluetooth.connected_devices or {}
    end),
    key = function(device) return device.mac end,
    itemfn = function(device)
        local battery = device.battery >= 0 and string.format(" %d%%", device.battery) or ""
        return button {
            on_click = function() mantle.bluetooth:invoke("disconnect", device.mac) end,
            children = { text { content = device.name .. battery } },
        }
    end,
}
```

#### `BluetoothState`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `available` | `boolean` | BlueZ has an adapter; `false` without one or without `bluetoothd`. |
| `connected_devices` | `ConnectedDevice[]` | Paired, connected devices. Unordered and may reshuffle on any push: sort before drawing. |
| `discoverable` | `boolean` | Other devices can find this adapter. BlueZ turns it off after `DiscoverableTimeout` (180 s by default). |
| `discovered_devices` | `DiscoveredDevice[]` | Unpaired devices BlueZ knows, unordered. Kept after `stop_discovery`; BlueZ expires unseen temporary ones after `TemporaryTimeout` (30 s by default). |
| `discovering` | `boolean` | The adapter is scanning, whichever client started it. |
| `enabled` | `boolean` | The adapter is powered. |
| `paired_devices` | `PairedDevice[]` | Paired devices that are not connected. Unordered like `connected_devices`. |
| `pairing_request?` | `PairingRequest` | The pairing question to show, or `nil`. Answer with `answer_pairing`. |

#### `ConnectedDevice`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `battery` | `integer` | Battery percentage, or `-1` when the device reports none. |
| `busy?` | `"pairing"\|"connecting"\|"disconnecting"` | Same as `DiscoveredDevice::busy`. |
| `category` | `string` | From the class of device: `"keyboard"`, `"mouse"`, `"headphones"`, `"headset"`, `"phone"`, `"computer"` or `"generic"`. |
| `mac` | `string` | MAC address, e.g. `"00:1A:7D:DA:71:11"`; every `bluetooth` action takes it. |
| `name` | `string` | The device's advertised name, or empty. |

#### `DiscoveredDevice`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `blocked` | `boolean` | BlueZ refuses to pair with or connect to the device until it is unblocked. |
| `busy?` | `"pairing"\|"connecting"\|"disconnecting"` | The action this shell is running on the device, or `nil`; another client's never shows. |
| `mac` | `string` | MAC address, the argument of `pair`. |
| `name` | `string` | Advertised name, often empty when the device broadcasts only an address. |
| `paired` | `boolean` | Always `false`. |

#### `PairedDevice`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `blocked` | `boolean` | BlueZ refuses every connection to or from the device until it is unblocked. |
| `busy?` | `"pairing"\|"connecting"\|"disconnecting"` | Same as `DiscoveredDevice::busy`. |
| `category` | `string` | Same set as `ConnectedDevice.category`. |
| `mac` | `string` | MAC address, the argument of `connect` and `forget`. |
| `name` | `string` | The device's advertised name, or empty. |

#### `PairingRequest`

What the pairing agent is asking the user.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `code?` | `string` | Six-digit passkey for `"confirm"`, passkey or PIN for `"display"`, else `nil`. |
| `kind` | `"confirm"\|"authorize"\|"service"\|"display"` | `"confirm"`: does the device show `code`? `"authorize"`: may it pair? `"service"`: may a paired, untrusted device connect? `"display"`: type `code` on the device; nothing to answer. |
| `mac` | `string` | The device's MAC address. |
| `name` | `string` | The device's advertised name, or empty. |

Actions on `mantle.bluetooth`:

| Action | Does |
| :--- | :--- |
| `set_enabled(enabled: boolean)` | Powers the adapter on or off. |
| `set_discoverable(discoverable: boolean)` | Makes the adapter findable by other devices, or not. |
| `start_discovery()` | Clears `discovered_devices` and scans. The request holds, so a scan starts once the adapter powers on and pauses while a `pair` runs. |
| `stop_discovery()` | Stops discovery; `discovered_devices` stays. |
| `pair(mac: string)` | Pairs a discovered device, then trusts and connects it. |
| `connect(mac: string)` | Trusts and connects a paired device. |
| `disconnect(mac: string)` | Disconnects a connected device. |
| `forget(mac: string)` | Removes a device from BlueZ, unpairing it. |
| `answer_pairing(mac: string, accept: boolean)` | Accepts or rejects the `pairing_request` for `mac`; a yes within 750 ms of it appearing is ignored. |

### notifications

The notification server: the newest 20 notifications and do-not-disturb. Backend: [services §1](../services.md#1-notifications).

```lua
button {
    on_click = function()
        local notifications = mantle.notifications:get()
        if notifications then
            mantle.notifications:invoke("set_dnd", not notifications.dnd)
        end
    end,
    children = {
        text {
            content = mantle.notifications:map(function(notifications)
                return (notifications and notifications.dnd) and "DND on" or "DND off"
            end),
        },
    },
}
```

#### `NotificationsState`

`mantle.notifications`'s payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `dnd` | `boolean` | Do-not-disturb: mutes non-critical sounds only. Hiding popups is the config's call. |
| `feed` | `Notification[]` | The newest 20 of up to 100 queued notifications, newest first, expired ones included; a replacement keeps its place. An entry past 20 stays dismissable by id. |

#### `Notification`

One `notifications.feed` entry.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `actions` | `NotificationAction[]` | Buttons in sender order, at most 8, excluding `default` and `inline-reply`. |
| `app_icon?` | `string` | Application icon for `icon { name = ... }`: a theme name such as `"firefox"` or an absolute path, or `nil`. |
| `app_name` | `string` | Sending application, truncated to 64 bytes. |
| `body` | `NotificationSpan[]` | Parsed body markup; the raw body is truncated to 512 bytes first. |
| `desktop_entry?` | `string` | Sender's desktop id, e.g. `"org.telegram.desktop"`, for `mantle.applications.by_app_id`; `nil` when absent or containing `/`. |
| `expired` | `boolean` | The timeout ran out: drop it from popups, keep it in history until dismissed. Never true for critical or `expire_timeout = 0`; a replacement resets it. |
| `has_default_action` | `boolean` | Clicking the card may `:invoke("invoke_action", id, "default")`. |
| `has_reply` | `boolean` | The sender accepts `:invoke("reply", id, text)`. |
| `id` | `integer` | Server id, from `1`; a replacement keeps the id it replaces. |
| `image_path?` | `string` | Attached picture (album art, avatar) as an existing absolute path, or `nil`. Never a theme name. |
| `reply_placeholder?` | `string` | Placeholder for an empty reply field, e.g. `"Reply to Alice"`, capped at 64 bytes; `nil` when unset. |
| `summary` | `string` | Title as sent, truncated to 128 bytes. Not markup-parsed: the spec makes it plain text. |
| `timestamp` | `integer` | Arrival time, Unix seconds; age is `mantle.system.time - timestamp`. A replacement restamps it. |
| `transient` | `boolean` | Popup-only: removed on expiry instead of retired to history. |
| `urgency` | `"low"\|"normal"\|"critical"` | `"normal"` when the sender set none. `"critical"` never expires and plays sound through DND. |

#### `NotificationAction`

One action button.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `icon_name?` | `string` | Theme icon name (the key) when the sender set `action-icons`, else `nil`. Never a path. |
| `key` | `string` | Opaque key for `:invoke("invoke_action", id, key)`. |
| `label` | `string` | Button label, capped at 64 bytes. An empty label falls back to the key unless `icon_name` is set. |

#### `NotificationSpan`

One body-markup run: styled text or an image.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `bold?` | `boolean` | Whether the run was inside `<b>`. |
| `href?` | `string` | `<a href>` target, or `nil` when not a link. |
| `image_path?` | `string` | Existing absolute path under an icon root; images elsewhere are dropped. |
| `italic?` | `boolean` | Whether the run was inside `<i>`. |
| `kind` | `"text"\|"image"` |  |
| `text?` | `string` | Unescaped text; empty runs are omitted. |
| `underline?` | `boolean` | Whether the run was inside `<u>`. |

Actions on `mantle.notifications`:

| Action | Does |
| :--- | :--- |
| `dismiss(id: integer)` | Removes a queued notification. |
| `invoke_action(id: integer, key: string)` | Invokes an `actions[].key`, or `"default"`; removes the notification unless it is resident. |
| `reply(id: integer, text: string)` | Sends reply text to a notification with `has_reply`; removes it unless it is resident. |
| `set_sound(urgency: "low"\|"normal"\|"critical", path: string)` | Sets an urgency tier's sound: an existing file under `/usr/share`, `/usr/local/share`, `/opt` or `$XDG_DATA_HOME`, else ignored. Only Ogg Vorbis and 16-bit PCM WAV play. |
| `set_dnd(enabled: boolean)` | Gates non-critical notification sounds. |
| `set_quiet(enabled: boolean)` | Mutes non-critical sounds like `set_dnd`, without changing `dnd`. |
| `set_app_muted(app: string, muted: boolean)` | Silences every sound from an app, critical included, matched exactly on `app_name` or `desktop_entry`. |
| `hold_expiry(seconds: integer)` | Pauses every expiry countdown for `seconds`, capped at 300; `0` releases the hold. |

### tray

StatusNotifierItem: registered tray items with artwork, status and menus. Backend: [services §2](../services.md#2-system-tray).

```lua
list {
    direction = "Horizontal",
    spacing = 4,
    source = mantle.tray:map(function(tray)
        return tray and tray.items or {}
    end),
    key = function(item) return item.id end,
    itemfn = function(item)
        return button {
            on_click = function(_, which)
                if which == "left" and not item.item_is_menu then
                    mantle.tray:invoke("activate", item.id, 0, 0) -- screen x, y; most apps ignore them
                end
            end,
            children = { icon { name = item.icon_name or item.icon_path or "", size = 16 } },
        }
    end,
}
```

#### `TrayState`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `items` | `TrayItem[]` | Registered items in registration order, oldest first; updates never reorder them. |

#### `TrayItem`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `attention_icon_name?` | `string` | Artwork to draw while `status == "NeedsAttention"`, paired with `attention_icon_path` like the base icon; both `nil` when unset. |
| `attention_icon_path?` | `string` | File half of the attention artwork. |
| `icon_name?` | `string` | Theme icon name for `icon { name = ... }`. At most one of it and `icon_path` is set. |
| `icon_path?` | `string` | Icon file for `image { source = ... }`: one from the item's `IconThemePath`, or its pixmap spooled to a PNG. |
| `id` | `string` | Item identity for every `tray` action, e.g. `"1.234/StatusNotifierItem"`. Opaque. |
| `item_is_menu` | `boolean` | Left click should open `menu` instead of `activate`. |
| `menu?` | `MenuItem[]` | Top-level menu entries, or `nil` when the item exports no DBusMenu or its first fetch failed. |
| `name` | `string` | SNI `Title`, or its `Id` when the title is empty. |
| `overlay_icon_name?` | `string` | Badge to draw over the icon's corner, paired with `overlay_icon_path`; both `nil` when unset. |
| `overlay_icon_path?` | `string` | File half of the badge. |
| `status` | `string` | `"Active"`, `"Passive"` (the item asks to be hidden) or `"NeedsAttention"`, as the item sent it. |
| `tooltip?` | `string` | Tooltip title and text joined by a newline, or `nil` when both are empty. |

#### `MenuItem`

One `tray.items[].menu` entry.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `children` | `MenuItem[]` | Submenu entries, empty for a leaf. An app that fills submenus lazily sends them only after `menu_will_show`. |
| `enabled` | `boolean` | `false` for a greyed-out entry; draw it, but clicking does nothing. |
| `icon_name?` | `string` | Theme icon name, or `nil`. Icon pixmaps are not carried. |
| `id` | `integer` | DBusMenu id, the second argument of `activate_menu_item` and `menu_will_show`. |
| `label?` | `string` | Entry text as sent, or `nil`. `_` mnemonic markers remain (`"_Quit"`); strip them to draw. |
| `menu_type` | `string` | `"standard"` (the default) or `"separator"`, as the application sent it. |
| `toggle_state?` | `integer` | `0` off, `1` on, `-1` indeterminate or unreported; `nil` exactly when `toggle_type` is. |
| `toggle_type?` | `string` | `"checkmark"`, `"radio"`, or `nil` for an entry that is not a toggle. |

Actions on `mantle.tray`:

| Action | Does |
| :--- | :--- |
| `activate(id: string, x: integer, y: integer)` | Left-click activation at screen coordinates `x`, `y`; a no-op when `item_is_menu`. |
| `secondary_activate(id: string, x: integer, y: integer)` | Middle-click activation at screen coordinates `x`, `y`. |
| `scroll(id: string, delta: integer, orientation: string)` | Scrolls the icon by `delta`; `orientation` is `"vertical"` or `"horizontal"`, passed verbatim. |
| `activate_menu_item(id: string, menu_item_id: integer)` | Clicks the item's `MenuItem.id`. |
| `menu_will_show(id: string, submenu_id: integer)` | Tells the application submenu `submenu_id` is opening, then refetches the menu unless it answers that nothing changed. |

### mpris

MPRIS: media players with track metadata, playback state and position. Backend: [services §3](../services.md#3-mpris).

```lua
local player = mantle.mpris:map(function(mpris)
    return mpris and mpris.players[1]
end)

button {
    on_click = function()
        local current = player:get()
        if current then
            mantle.mpris:invoke("control", current.id, "play_pause")
        end
    end,
    children = {
        text {
            max_width = 240,
            elide = "End",
            content = player:map(function(current)
                return current and (current.play_state .. ": " .. current.title) or ""
            end),
        },
    },
}
```

#### `MprisState`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `players` | `PlayerState[]` | Every controllable MPRIS player except `playerctld`, longest-running first, so `players[1]` stays put; empty when none runs. |

#### `PlayerState`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `album_art_path` | `string` | Cover art as an existing absolute path, or empty; a remote `artUrl` is not fetched. |
| `artist` | `string` | Artists joined with `", "`; empty when unset. |
| `desktop_entry` | `string` | The player's `.desktop` basename, e.g. `"firefox"`, for app matching; empty when unset. |
| `id` | `string` | Bus-name suffix after `org.mpris.MediaPlayer2.`, e.g. `"spotify"`; every action takes it. |
| `identity` | `string` | Display name, e.g. `"Spotify"`; empty if unanswered. |
| `length` | `integer` | Track length in microseconds, or `-1` when unknown, as for a live stream. |
| `play_state` | `string` | `"Playing"`, `"Paused"` or `"Stopped"`; keeps the last value when a read fails, empty if none. |
| `position` | `integer` | Playback offset in microseconds as of `position_updated_at`, not polled while playing: add elapsed time. `-1` when unknown. |
| `position_updated_at` | `integer` | `CLOCK_MONOTONIC` microseconds when `position` was read. No Lua clock shares this epoch (not `mantle.system.monotonic`); only compare it with itself. |
| `title` | `string` | Track title; empty when unset, normal between tracks. |
| `url` | `string` | `xesam:url` as sent, e.g. a `file://` path or an `https://` page; empty when unset. |

Actions on `mantle.mpris`:

| Action | Does |
| :--- | :--- |
| `control(id: string, cmd: "play"\|"pause"\|"play_pause"\|"next"\|"previous")` | Sends a playback command to `players[].id`. |
| `seek(id: string, position_us: integer)` | Seeks to an absolute position in microseconds, clamped to `[0, length]` (only `>= 0` when `length` is `-1`). |
| `seek_relative(id: string, offset_us: integer)` | Seeks by a signed offset in microseconds, unclamped; past the end may skip to the next track. |

### workspaces

Workspaces per output, special workspaces and the focused window. Backend: [services §8](../services.md#8-workspaces-and-windows).

```lua
panel {
    id = "bar",
    layer = "Top",
    anchor = { top = true, left = true, right = true },
    height = 28,
    child = function(output) -- one instance per monitor, named by connector
        return text {
            content = mantle.workspaces:map(function(workspaces)
                for _, entry in ipairs(workspaces and workspaces.outputs or {}) do
                    if entry.name == output then
                        for _, workspace in ipairs(entry.workspaces) do
                            if workspace.id == entry.active_workspace then
                                return "workspace " .. workspace.idx
                            end
                        end
                    end
                end
                return ""
            end),
        }
    end,
}
```

#### `WorkspacesState`

`mantle.workspaces` payload; `nil` without niri or Hyprland.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `active_client?` | `ActiveClient` | The focused window, or `nil` when none has focus. One per session, not per output. |
| `compositor` | `string` | `"niri"` or `"hyprland"`. |
| `outputs` | `OutputWorkspaces[]` | One entry per output, sorted by connector name. |
| `overview_open?` | `boolean` | Whether niri's overview is open; `nil` on Hyprland, which has none. |
| `special?` | `SpecialWorkspace[]` | Hyprland special workspaces, sorted by name. `nil` on niri; empty means none exist. |

#### `ActiveClient`

The focused window.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `class` | `string` | Wayland `app_id`, e.g. `"firefox"`; the key of `applications.by_app_id`. Empty when unset. |
| `is_floating` | `boolean` | Whether the window floats rather than tiles. |
| `is_fullscreen?` | `boolean` | Whether the window is fullscreen (maximized is `false`); `nil` on niri, which does not report it. |
| `title` | `string` | Window title; empty when unset. |

#### `OutputWorkspaces`

One output's workspaces.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `active_workspace` | `integer` | `WorkspaceEntry::id` shown on this output. |
| `focused_workspace?` | `integer` | `WorkspaceEntry::id` with focus, present only on the focused output. |
| `name` | `string` | Connector name, e.g. `"eDP-1"`, as in `mantle.screens` and a surface's `monitor`. |
| `workspaces` | `WorkspaceEntry[]` | Workspaces on this output, sorted by `WorkspaceEntry::idx`. |

#### `WorkspaceEntry`

One workspace. Draw `idx`, send `id`.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `app_id?` | `string` | `app_id` of a window here: Hyprland's most recently focused one with an `app_id`; on niri the focused one, else the lowest id, `nil` if that one has no `app_id`. `nil` when empty. |
| `id` | `integer` | Stable id, the argument of `"focus"`. Hyprland's workspace number; opaque on niri. |
| `idx` | `integer` | Label number: niri's 1-based position on the output, renumbered on reorder; Hyprland's workspace number, equal to `id` up to `255`, where it saturates. |
| `name?` | `string` | Workspace name; `nil` when unnamed, or on Hyprland when the name is just the number. |
| `populated` | `boolean` | Whether a window sits here. |

#### `SpecialWorkspace`

One Hyprland special workspace.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `app_id?` | `string` | `app_id` of its representative window, chosen as `WorkspaceEntry::app_id` is. |
| `name` | `string` | Full name, `"special:scratch"` or `"special"`; the argument of `"toggle_special"`. |
| `populated` | `boolean` | Whether at least one window sits on it. |
| `shown_on?` | `string` | Connector showing it, or `nil` while hidden. |

Actions on `mantle.workspaces`:

| Action | Does |
| :--- | :--- |
| `focus(id: integer)` | Focuses a `WorkspaceEntry.id`. Hyprland creates a missing number; niri ignores it. |
| `toggle_special(name: string)` | Shows or hides a `special[].name` on Hyprland, creating an unknown one; no-op on niri. |

### windows

Open toplevel windows with title, app ID, workspace, output and state flags. Backend: [services §8](../services.md#8-workspaces-and-windows).

```lua
list {
    source = mantle.windows:map(function(windows)
        return windows and windows.windows or {}
    end),
    key = function(window) return window.id end,
    itemfn = function(window)
        return button {
            on_click = function() mantle.windows:invoke("focus", window.id) end,
            children = {
                text { content = window.title, foreground = window.focused and "#89B4FA" or "#CDD6F4" },
            },
        }
    end,
}
```

#### `WindowsState`

`mantle.windows` payload; `nil` with no niri, Hyprland or wlr-foreign-toplevel backend.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `source` | `string` | `"niri"`, `"hyprland"`, or `"wlr_foreign_toplevel"`. |
| `windows` | `WindowEntry[]` | Sorted by `workspace_id`, then backend order; windows without one last. |

#### `WindowEntry`

One toplevel window. `nil` optional fields are ones the backend does not report.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `app_id` | `string` | Wayland `app_id` (Hyprland's `class`); empty when unset. |
| `floating?` | `boolean` | `nil` on wlr. |
| `focused` | `boolean` | Whether the window has keyboard focus. |
| `fullscreen?` | `boolean` | `nil` on niri. |
| `id` | `string` | Opaque, backend-shaped id for `:invoke`; compare it, never parse it. |
| `maximized?` | `boolean` | `nil` on niri. |
| `minimized?` | `boolean` | `nil` except on wlr. |
| `output?` | `string` | Connector name; `nil` when unknown. On wlr, the first output the window entered. |
| `title` | `string` | Window title; empty when unset. |
| `workspace_id?` | `integer` | `WorkspaceEntry.id`; `nil` on wlr and on Hyprland special workspaces. |

Actions on `mantle.windows`:

| Action | Does |
| :--- | :--- |
| `focus(id: string)` | Focuses a window. |
| `close(id: string)` | Asks the compositor to close the window. |
| `set_fullscreen(id: string, fullscreen: boolean)` | Sets fullscreen on or off; no-op on niri. |
| `set_minimized(id: string, minimized: boolean)` | Sets minimized on or off; wlr only. |
| `set_maximized(id: string, maximized: boolean)` | Sets maximized on or off; no-op on niri. |

### keyboard

Lock keys, the active layout and the keyboard backlight. Backend: [services §11](../services.md#11-other-capabilities).

```lua
button {
    on_click = function()
        local keyboard = mantle.keyboard:get()
        if keyboard and keyboard.layout_count > 1 then
            mantle.keyboard:invoke("switch_layout", (keyboard.active_layout_index + 1) % keyboard.layout_count)
        end
    end,
    children = {
        text {
            content = mantle.keyboard:map(function(keyboard)
                if keyboard == nil then
                    return ""
                end
                return keyboard.active_layout .. (keyboard.caps_lock and " ⇪" or "")
            end),
        },
    },
}
```

#### `KeyboardState`

`mantle.keyboard`'s payload. Lock keys read `false` when no source resolves.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `active_layout` | `string` | Layout display name, e.g. `"English (US)"`; empty before the compositor answers or without one. |
| `active_layout_index` | `integer` | 0-based position of the active layout, as `switch_layout` takes it. |
| `backlight_pct` | `integer` | Keyboard backlight, `0` to `100`, or `-1` without a backlight device or readable level. Refreshes on hardware hotkeys and `set_backlight` only, not on other software writes. |
| `caps_lock` | `boolean` | Caps Lock is on. |
| `layout_count` | `integer` | Configured layout count; below `2` there is nothing to switch. |
| `num_lock` | `boolean` | Num Lock is on. |
| `scroll_lock` | `boolean` | Scroll Lock is on. |

Actions on `mantle.keyboard`:

| Action | Does |
| :--- | :--- |
| `set_backlight(percent: integer)` | Sets the keyboard backlight, `0` to `100`; higher clamps to `100`. |
| `switch_layout(index: integer)` | Switches to the 0-based configured layout `index`. |

### brightness

The screen backlight percentage; `nil` without a backlight. Backend: [services §11](../services.md#11-other-capabilities).

```lua
button {
    on_wheel = function(_, steps)
        local brightness = mantle.brightness:get()
        if brightness then
            local percent = brightness.percent + math.floor(steps * 5) -- math.floor returns an integer
            mantle.brightness:invoke("set", math.max(1, math.min(100, percent)))
        end
    end,
    children = {
        text {
            content = mantle.brightness:map(function(brightness)
                return brightness and string.format("☀ %d%%", brightness.percent) or ""
            end),
        },
    },
}
```

#### `BrightnessState`

`mantle.brightness`'s payload; the capability stays `nil` on a machine with no backlight.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `percent` | `integer` | Screen backlight, `0` to `100`: the last requested level (sysfs `brightness`), not the mid-fade one. |

Actions on `mantle.brightness`:

| Action | Does |
| :--- | :--- |
| `set(percent: integer)` | Sets the screen backlight, `0` to `100`; higher clamps to `100`. |

### power

Power profiles, mains or battery, and battery power draw. Backend: [services §11](../services.md#11-other-capabilities).

```lua
list {
    direction = "Horizontal",
    source = mantle.power:map(function(power)
        return power and power.profiles or {}
    end),
    itemfn = function(name)
        local power = mantle.power:get()
        return button {
            padding = 6,
            background = (power and power.active_profile == name) and "#89B4FA" or "#313244",
            on_click = function() mantle.power:invoke("set_profile", name) end,
            children = { text { content = name } },
        }
    end,
}
```

#### `PowerState`

`mantle.power`'s payload. Profile fields are `nil` without power-profiles-daemon, the rest without UPower; a failed read is also `nil`. With neither service the capability stays `nil`.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `active_profile?` | `string` | Active platform profile, e.g. `"balanced"`. |
| `energy_rate?` | `number` | UPower's display-device `EnergyRate` in watts; direction is `mantle.battery.state`. |
| `on_battery?` | `boolean` | UPower's `OnBattery`: running on battery rather than mains. |
| `profiles?` | `string[]` | Available profiles in daemon order, e.g. `{"power-saver", "balanced", "performance"}`. |

Actions on `mantle.power`:

| Action | Does |
| :--- | :--- |
| `set_profile(name: string)` | Switches to one of `profiles`. Not validated here; a rejected name is logged and `active_profile` stays. |

### battery

UPower's display device: charge, state and time estimates. Backend: [services §11](../services.md#11-other-capabilities).

```lua
text {
    content = mantle.battery:map(function(battery)
        local seconds = battery and battery.present and battery.time_to_empty
        if not seconds then
            return ""
        end
        return string.format("%d:%02d left", seconds // 3600, seconds % 3600 // 60)
    end),
}
```

#### `BatteryState`

`mantle.battery`'s payload. No battery, or no UPower, reads `present = false` and defaults.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `percent` | `integer` | UPower's `Percentage`, rounded to `0` to `100`; a spurious `0` while not draining keeps the last value. |
| `present` | `boolean` | UPower's display device is a present battery. Check it before drawing the other fields. |
| `state` | `BatteryStatus` | What the battery is doing; see `BatteryStatus`. |
| `time_to_empty?` | `integer` | Seconds until flat, or `nil` while UPower has no estimate. |
| `time_to_full?` | `integer` | Seconds until full, or `nil` while UPower has no estimate. |

#### `BatteryStatus`

`battery.state`: UPower's `Device.State` by name, e.g. `b.state == "PendingCharge"`.

| Value | Meaning |
| :--- | :--- |
| `"Unknown"` | No answer: UPower unreachable, an unknown state number, or a display device that is not a battery. |
| `"Charging"` | Taking current from an adapter. |
| `"Discharging"` | Draining. |
| `"Empty"` | Flat. |
| `"FullyCharged"` | Charged and holding. |
| `"PendingCharge"` | On mains, neither draining nor taking current: a charge limit, weak charger or thermal pause. |
| `"PendingDischarge"` | Waiting to discharge. |

Read-only: no actions.

### privacy

Apps using the camera, microphone or screen capture right now. Backend: [services §6](../services.md#6-pipewire-and-privacy).

```lua
rect {
    width = 8,
    height = 8,
    radius = 4,
    background = "#F38BA8",
    visible = mantle.privacy:map(function(privacy)
        return privacy ~= nil and (#privacy.microphone_users > 0 or #privacy.camera_users > 0)
    end),
}
```

#### `PrivacyState`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `camera_users` | `PrivacyUser[]` | One entry per process holding a `/dev/videoN` open; empty when none is. Only devices present when `privacy` started are watched. |
| `microphone_users` | `PrivacyUser[]` | Apps with a running PipeWire audio capture, one per name. Idle streams and sink-monitor captures are absent; a muted microphone still counts. |
| `screencast_users` | `PrivacyUser[]` | Apps with a running PipeWire screen-capture stream, one per name. wlr-screencopy tools such as `wf-recorder` and `grim` never appear. |

#### `PrivacyUser`

One app using a camera, microphone or screen capture.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `app_name` | `string` | PipeWire `application.name`, else `/proc/<pid>/comm`, else `"pid 1234"` (or `"node 56"`); never empty. |

Read-only: no actions.

### system

Wall and monotonic clocks, pushed once a second. Backend: [services §9](../services.md#9-telemetry-and-clock).

```lua
text {
    content = mantle.system:map(function(system)
        return system and os.date("%a %H:%M", system.time) or ""
    end),
}
```

#### `SystemState`

`mantle.system`'s payload, pushed once a second.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `monotonic` | `integer` | Seconds since `system` was first used; excludes suspend. Take durations from it, since NTP moves `time`. |
| `time` | `integer` | Unix epoch seconds, as `os.date` takes them. |

Read-only: no actions.

### sysinfo

CPU, memory and swap use, CPU and GPU temperatures. `nil` until `configure` sets intervals. Backend: [services §9](../services.md#9-telemetry-and-clock).

```lua
mantle.sysinfo:invoke("configure", { cpu_interval = 2, ram_interval = 5 })

text {
    content = mantle.sysinfo:map(function(sysinfo)
        if sysinfo == nil then
            return ""
        end
        return string.format("CPU %d%%  RAM %d%%", sysinfo.cpu_percent, sysinfo.ram_percent)
    end),
}
```

#### `SysinfoState`

`mantle.sysinfo`'s payload; `nil` until `configure` sets an interval and a reading lands.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `cpu_percent` | `integer` | CPU utilization across all cores, `0` to `100`, rounded down; `0` until two samples form a delta. |
| `ram_percent` | `integer` | Physical memory in use (`MemTotal - MemAvailable`), `0` to `100`, rounded down. |
| `swap_percent` | `integer` | Swap in use, `0` to `100`, rounded down; also `0` without swap. |
| `temp_cores` | `integer[]` | CPU temperatures in whole Celsius: per core (`coretemp`) or per CCD (`k10temp`), else one package or `acpitz` reading; empty without a sensor. An unreadable sensor is skipped. |
| `temp_gpu` | `integer` | `amdgpu`, `nouveau` or `nvidia` hwmon temperature in whole Celsius, or `-1` without a readable one. |

Actions on `mantle.sysinfo`:

| Action | Does |
| :--- | :--- |
| `configure(intervals: SysinfoConfigure)` | Sets poll intervals; every one starts at `0`, so nothing is read until this. The first reading lands one interval later (CPU: two). |

#### `SysinfoConfigure`

`sysinfo:configure`'s table. Absent keys keep their interval; one wrong-typed key drops the call.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `cpu_interval?` | `integer` | Seconds between CPU reads; `0` (the default) stops them. |
| `ram_interval?` | `integer` | Seconds between memory and swap reads; `0` (the default) stops them. |
| `temp_interval?` | `integer` | Seconds between temperature reads; `0` (the default) stops them. |

### updates

Pending package upgrades (pacman, optionally AUR), install progress and whether a reboot is due. Backend: [services §11](../services.md#11-other-capabilities).

```lua
mantle.updates:invoke("configure", { interval = 3600 })

button {
    on_click = function() mantle.updates:invoke("check") end,
    children = {
        text {
            content = mantle.updates:map(function(updates)
                if updates == nil or updates.checking then
                    return "…"
                end
                return updates.count > 0 and (updates.count .. " updates") or "up to date"
            end),
        },
    },
}
```

#### `UpdatesState`

`mantle.updates`'s payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `aur_error?` | `string` | Why AUR packages are missing: the last check's AUR query failed, or `aur` is on with no `aur_helper`; `nil` otherwise. `packages` still holds the repos' answer. |
| `aur_helper?` | `string` | AUR helper found at start, `"paru"` or `"yay"`, or `nil`; used only once `configure` sets `aur`. |
| `check_error?` | `string` | Why the last check failed, or `nil` after a success. A check never modifies the system. |
| `checking` | `boolean` | A check is running. |
| `consecutive_check_failures` | `integer` | Check failures in a row; a success resets it to `0`. |
| `count` | `integer` | Always `#packages`. |
| `install_current_package` | `string` | Package being installed; empty before the first step line. |
| `install_current_step` | `integer` | 1-based number of the package being installed, from the manager's `(2/5)`; `0` before the first. |
| `install_error?` | `string` | Why the package manager could not be run or waited on, or `nil`. Its own failures are `install_exit_code`. |
| `install_exit_code?` | `integer` | Package manager's exit code for the last install (`0` success); `nil` while running, before one, or when a signal killed it. |
| `install_finished_at?` | `integer` | Unix seconds when the last install's process ended, whatever its status; `nil` while running, before one, or when it failed to spawn. |
| `install_log` | `string[]` | The last 200 lines of install output, stdout and stderr interleaved, newest last; cleared when an install starts. |
| `install_total_steps` | `integer` | Packages in the transaction; `0` until the first step line, so draw progress as indeterminate. |
| `installing` | `boolean` | An install is running; the `install_*` fields describe the latest run. |
| `last_successful_check?` | `integer` | Unix seconds of the last successful check (or the `checked_at` seed), else `nil`. |
| `package_manager?` | `string` | Package manager, e.g. `"pacman"`, or `nil` when unsupported. Set from the first push. |
| `packages` | `UpdateCandidate[]` | Pending upgrades. A failed check keeps the last good list. |
| `reboot_required` | `boolean` | `/run/mantle-reboot-required` exists, watched live. Mantle never writes it: a user-installed pacman hook must, and `/run` empties on reboot. |

#### `UpdateCandidate`

One installed package with a newer version.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `download_size` | `integer` | Bytes to fetch; `0` when already cached. |
| `installed_size` | `integer` | Bytes the new version occupies installed; not a delta. |
| `name` | `string` | Package name. |
| `new_version` | `string` | Version on offer. |
| `old_version` | `string` | Installed version. |
| `repository?` | `string` | Source repository, e.g. `"extra"` or `"aur"`; empty in a seeded list that lacks it. |

Actions on `mantle.updates`:

| Action | Does |
| :--- | :--- |
| `check()` | Checks for upgrades now, even when dormant; ignored while `checking`. |
| `configure(config: UpdatesConfigure)` | Sets the check schedule and AUR use, and seeds a remembered check. |
| `install()` | Runs a full upgrade, `pkexec pacman -Syu --noconfirm` or `aur_helper` when `aur` is on; ignored while `installing`. |

#### `UpdatesConfigure`

`configure`'s table. One wrong-typed key drops the whole call.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `aur?` | `boolean` | Also check the AUR and install through `aur_helper`. Sends every foreign package name to aur.archlinux.org and builds without PKGBUILD review. |
| `checked_at?` | `integer` | Persisted Unix seconds of the last successful check. Seeds `last_successful_check` only while that is `nil`, so a restart need not recheck at once. |
| `interval` | `integer` | Seconds between scheduled checks, the first at once unless `last_successful_check` is younger; `0` checks only on `check`. |
| `packages?` | `UpdateCandidate[]` | Persisted `packages` from that check, seeded on the same terms; ignored without `checked_at`. |

### applications

Installed desktop entries, indexed by window `app_id`. Backend: [services §11](../services.md#11-other-capabilities).

```lua
text {
    content = computed({ mantle.workspaces, mantle.applications }, function(workspaces, applications)
        local client = workspaces and workspaces.active_client
        if client == nil or applications == nil then
            return ""
        end
        local index = applications.by_app_id[client.class]
        return index and applications.entries[index].name or client.class
    end),
}
```

#### `ApplicationsState`

`mantle.applications` payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `by_app_id` | `table<string, integer>` | Window `app_id` to its 1-based index: `entries[by_app_id[app_id]]`. Keys are exact `StartupWMClass` and desktop ids, then lowercased and last-dot-segment guesses. |
| `entries` | `AppSummary[]` | Installed entries, sorted by `name` (byte order). Not watched: `"refresh"` rescans. |

#### `AppSummary`

One visible `Type=Application` desktop entry; display data only, argv stays private.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `comment?` | `string` | `Comment=`, unlocalized, e.g. `"Web Browser"`; `nil` without the key. |
| `generic_name?` | `string` | `GenericName=`, unlocalized, e.g. `"Text Editor"`; `nil` without the key. |
| `icon?` | `string` | `Icon=` as written, a theme name or absolute path, both accepted by `icon { name }`; `nil` without the key. |
| `id` | `string` | Desktop file id, e.g. `"org.telegram.desktop"`; the argument of `"launch"`. |
| `keywords` | `string[]` | `Keywords=` split on `;`, for search; empty without the key. |
| `name` | `string` | `Name=`, unlocalized: `Name[xx]` is not read. |

Actions on `mantle.applications`:

| Action | Does |
| :--- | :--- |
| `refresh()` | Rescans installed desktop entries. |
| `launch(id: string)` | Launches `entries[].id`, detached; `Terminal=true` entries run in `$TERMINAL`. |
| `open_url(url: string)` | Opens an `http`, `https` or `mailto` URL (at most 2048 bytes) with `xdg-open`. |

### files

Live file listings of watched folders. Backend: [services §11](../services.md#11-other-capabilities).

```lua
local folder = (os.getenv("HOME") or "") .. "/Pictures/Wallpapers"
mantle.files:invoke("watch", folder, { "jpg", "png" })

list {
    source = mantle.files:map(function(files)
        local listing = files and files.folders[folder]
        return listing and listing.entries or {}
    end),
    key = function(entry) return entry.path end,
    itemfn = function(entry)
        return text { content = entry.name }
    end,
}
```

#### `FilesState`

`mantle.files` payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `folders` | `table<string, Folder>` | One entry per `"watch"`, keyed by its `path` minus trailing slashes; `nil` until watched. |

#### `Folder`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `entries` | `FileEntry[]` | Files (and symlinks to files) directly inside, minus dotfiles, filtered by extension and sorted case-insensitively by name. Relisted 200 ms after the last change. |
| `error?` | `string` | Why listing failed, e.g. `"No such file or directory (os error 2)"`; `nil` on success. A missing or deleted folder is not watched for reappearing. |
| `ready` | `boolean` | `false` until the first listing lands, then `true` even when empty or failed. |

#### `FileEntry`

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `modified` | `integer` | Modification time in Unix seconds; `0` when unavailable. |
| `name` | `string` | File name, e.g. `"sunrise.jpg"`. |
| `path` | `string` | Absolute path. |

Actions on `mantle.files`:

| Action | Does |
| :--- | :--- |
| `watch(path: string, extensions?: string[])` | Keeps `folders[path]` listing an absolute folder. `extensions` match case-insensitively, dot optional; omitted means every file. |
| `unwatch(path: string)` | Stops watching `path` and removes it from `folders`. |

### lock

The session lock: whether it is held, authentication progress and the last failure. Backend: [services §7](../services.md#lock).

The lock screen itself is a [`lock` surface](surfaces.md).

```lua
button {
    on_click = function() mantle.lock:invoke("lock") end,
    children = { text { content = "Lock" } },
}
```

#### `LockState`

`mantle.lock`'s payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `active` | `boolean` | The Renderer confirmed the session locked; a requested lock stays `false` until then. |
| `attempts` | `integer` | Rejected passwords since this lock was confirmed; reset by the next lock. |
| `authenticating` | `boolean` | A password is with PAM. A second submit is refused while true. |
| `error` | `string` | Last failure to draw: a rejected password (`"authentication failed"`) or a refused lock's reason. Cleared by a correct password, `lock`, a confirmed lock, and unlock. |
| `unlocking` | `boolean` | PAM said yes and the lock is still up: the window for an out-animation. |

Actions on `mantle.lock`:

| Action | Does |
| :--- | :--- |
| `lock()` | Locks the session; a no-op while `active`. |
| `set_unlock_animation(ms?: integer)` | Keeps the lock up `ms` after a correct password for an out-animation. Clamped to 600; omitted is `0`. |

### polkit

The pending polkit authentication request, its progress and the last failure. Backend: [services §7](../services.md#polkit).

The password goes through a `secure_submit` [text field](input.md), never through Lua.

```lua
text {
    visible = mantle.polkit:map(function(polkit)
        return polkit ~= nil and polkit.active
    end),
    content = mantle.polkit:map(function(polkit)
        return polkit and polkit.message or ""
    end),
}
```

#### `PolkitState`

`mantle.polkit`'s payload. Every other field is empty while `active` is false.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `action_id` | `string` | Action being authorized, e.g. `org.freedesktop.systemd1.manage-units`. |
| `active` | `boolean` | polkitd is waiting for the user to authenticate. |
| `authenticating` | `boolean` | A password is with PAM. A second submit is refused while true. |
| `error` | `string` | Drawable reason for the last failure, e.g. `"authentication failed"`. The prompt stays open to retry. |
| `icon_name` | `string` | Themed icon name, or empty when the caller set none. |
| `message` | `string` | Translated prompt text, e.g. `"Authentication is required to ..."`. |

Actions on `mantle.polkit`:

| Action | Does |
| :--- | :--- |
| `cancel()` | Dismisses the prompt; the requesting program sees the request cancelled. |

### processes

Programs declared with `session_process`: running state, start time and last exit. Backend: [services §10](../services.md#10-processes).

Declare programs with [`session_process`](scripting.md), which sends these actions for you.

```lua
text {
    content = mantle.processes:map(function(processes)
        local recorder = processes and processes.sessions.recorder
        return (recorder and recorder.running) and "● REC" or ""
    end),
}
```

#### `ProcessesState`

`mantle.processes` payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `sessions` | `table<string, SessionProcess>` | One entry per `session_process` name; an undeclared name is `nil`. |

#### `SessionProcess`

One declared program: its current run, or what is left of its last one.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `exit_code?` | `integer` | Exit status of the last run; `nil` while running, before any run, or when a signal killed it. |
| `pid?` | `integer` | Process id, also its process group id; kept after exit, `nil` before a spawn or after a failed `start`. |
| `running` | `boolean` | Whether it is up now. Otherwise the fields below describe the last run. |
| `start_error` | `string` | Why the last `start` failed to spawn, e.g. a `cmd` not on `PATH`; empty when it spawned. |
| `started_at?` | `integer` | Unix seconds when the run began; `nil` before a spawn or after a failed `start`. |

Actions on `mantle.processes`:

| Action | Does |
| :--- | :--- |
| `declare(name: string, stop_signal?: "TERM"\|"INT"\|"HUP"\|"QUIT"\|"USR1"\|"USR2"\|"KILL"\|"STOP"\|"CONT")` | Registers `name` (required before `start`) and sets its stop signal, default `TERM`. Redeclaring updates the signal without touching a running program. |
| `start(name: string, cmd: string, args?: string[])` | Runs `cmd` with `args` (no shell) as its own process group. No-op while `running` or when `name` is undeclared. |
| `signal(name: string, signal: "TERM"\|"INT"\|"HUP"\|"QUIT"\|"USR1"\|"USR2"\|"KILL"\|"STOP"\|"CONT")` | Sends `signal` to the program's process (not its group); no-op when not running. |
| `stop(name: string)` | Sends the declared stop signal to the process group, then `KILL` if it is still up 5 s later; no-op when not running. |

### storage

Each `persistent_table` JSON file, keyed by absolute path. Backend: [services §12](../services.md#12-paths-and-persistence).

Use [`persistent_table`](scripting.md), which sends these actions for you.

#### `StorageState`

`mantle.storage` payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `files` | `table<string, any>` | Each declared `persistent_table`'s contents, keyed by its absolute file path; `nil` until declared. Another writer's change to the file replaces it, unsaved writes included. |

Actions on `mantle.storage`:

| Action | Does |
| :--- | :--- |
| `open(path: string, defaults?: table<string, any>)` | Loads an absolute JSON file into `files[path]`, filling missing top-level keys from `defaults`. `persistent_table` sends this; stored values win over defaults. |
| `set(path: string, key: string, value?: any)` | Sets `key` in a declared file, `nil` deleting it; saved 1 s after the last write. |

## Idle

`mantle.idle` reads like the others (`:get`, `:map`, `:on_change`; state is `{ inhibited,
inhibitors }`) but has no `:invoke`. Instead it takes callbacks, which cannot cross the wire.

#### `IdleState`

`mantle.idle`'s payload.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `inhibited` | `boolean` | Something holds the session awake: a logind inhibitor (this shell's included), a `ScreenSaver` client or a Wayland inhibitor. No threshold fires while true. |
| `inhibitors` | `IdleInhibitor[]` | Holders other than this shell, `ScreenSaver` clients included. The compositor's hold has an empty `who`; draw `why` then. |

#### `IdleInhibitor`

One holder blocking idle.

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `who` | `string` | Free-text holder name, e.g. `"mpv"`; draw it, never match it. |
| `why` | `string` | Free-text reason, e.g. `"Playing video"`; often empty. |

| Method | Contract |
| :--- | :--- |
| `:register_threshold(seconds, on_idle, on_resume)` | `on_idle` after `seconds` without input, `on_resume` when input returns. Returns an integer handle. No threshold fires while `inhibited` is `true`. If another registration at the same `seconds` already saw this idle period, `on_idle` runs at once |
| `:cancel_threshold(handle)` | Drops one registration; an unknown or already-cancelled handle is a no-op |
| `:inhibit(reason)` | Takes one logind idle inhibit hold. Counted: two calls need two releases |
| `:release_inhibit()` | Releases one hold; with none held, a no-op |

| Across a reload | Across a Renderer respawn |
| :--- | :--- |
| Thresholds are dropped before the config re-evaluates; register again at top level. Re-registering the same `seconds` does not re-run `on_idle` for the current idle period | Thresholds and holds of the old generation are dropped |
| Inhibit holds survive, so a `state` that records "I hold one" stays true | |

```lua
local dimmed = state("dimmed", false)

mantle.idle:register_threshold(300, function()
    dimmed:set(true)
end, function()
    dimmed:set(false)
end)
```

## Examples

Battery label with every `nil` state handled:

```lua
text {
    foreground = mantle.battery:map(function(battery)
        return (battery and battery.present and battery.percent <= 15) and "#F38BA8" or "#CDD6F4"
    end),
    content = mantle.battery:map(function(battery)
        if battery == nil then
            return "…"
        elseif not battery.present then
            return ""
        end
        return string.format("%d%%%s", battery.percent, battery.state == "Charging" and " +" or "")
    end),
}
```

Volume on the wheel, mute on middle click. `:get()` inside a handler is the right read: it wants the
value now, not a binding. Input handlers are covered in [input](input.md).

```lua
button {
    on_wheel = function(_, steps)
        local audio = mantle.audio:get()
        if audio == nil or audio.volume == nil then
            return
        end
        mantle.audio:invoke("set_volume", math.max(0, math.min(1, audio.volume + steps * 0.05)))
    end,
    on_click = function(_, which)
        if which == "middle" then
            mantle.audio:invoke("toggle_mute")
        end
    end,
    children = {
        text {
            content = mantle.audio:map(function(audio)
                if audio == nil or audio.volume == nil then
                    return "--"
                end
                return audio.muted and "muted" or string.format("%d%%", math.floor(audio.volume * 100 + 0.5))
            end),
        },
    },
}
```

Workspace buttons. Draw `idx`, send `id` ([`list`](nodes.md) builds one button per entry):

```lua
list {
    direction = "Horizontal",
    spacing = 4,
    source = mantle.workspaces:map(function(workspaces)
        local output = workspaces and workspaces.outputs[1]
        return output and output.workspaces or {}
    end),
    key = function(workspace) return tostring(workspace.id) end,
    itemfn = function(workspace)
        local active = mantle.workspaces:map(function(workspaces)
            local output = workspaces and workspaces.outputs[1]
            return output ~= nil and output.active_workspace == workspace.id
        end)
        return button {
            padding = { left = 8, right = 8 },
            radius = 6,
            background = active:map(function(is_active) return is_active and "#89B4FA" or "#313244" end),
            on_click = function() mantle.workspaces:invoke("focus", workspace.id) end,
            children = { text { content = tostring(workspace.idx) } },
        }
    end,
}
```

Notification list; a click dismisses by the snapshot's `id`:

```lua
list {
    spacing = 6,
    source = mantle.notifications:map(function(notifications)
        return notifications and notifications.feed or {}
    end),
    key = function(item) return tostring(item.id) end,
    itemfn = function(item)
        return button {
            width = 320,
            padding = 8,
            radius = 8,
            background = "#1E1E2E",
            on_click = function() mantle.notifications:invoke("dismiss", item.id) end,
            children = {
                column {
                    children = {
                        text { content = item.summary, font_size = 13, elide = "End", width = "Fill" },
                        text { content = item.app_name, font_size = 11, foreground = "#A6ADC8" },
                    },
                },
            },
        }
    end,
}
```

An OSD on volume change. `on_change` reacts to a push rather than drawing it, so it writes
[named state](signals.md#named-state) that a panel binds; [`timer`](scripting.md) hides it again:

```lua
local osd_text = state("osd_text", "")
local osd_visible = state("osd_visible", false)
local hide_timer

mantle.audio:on_change(function(audio, previous)
    if previous == nil or audio.volume == nil then
        return -- the first push is learned state, not a change
    end
    if audio.volume == previous.volume and audio.muted == previous.muted then
        return
    end
    osd_text:set(audio.muted and "Muted" or string.format("Volume %d%%", math.floor(audio.volume * 100 + 0.5)))
    osd_visible:set(true)
    if hide_timer then
        hide_timer:cancel()
    end
    hide_timer = timer(2000, function() osd_visible:set(false) end)
end)

local osd = panel {
    id = "osd",
    layer = "Overlay",
    anchor = { bottom = true },
    margin = { bottom = 80 },
    visible = osd_visible,
    padding = 12,
    radius = 12,
    background = "#1E1E2ECC",
    child = text { content = osd_text, font_size = 16 },
}
```

## How do I…

| Task | Answer |
| :--- | :--- |
| Show a value that may not have arrived yet | Guard `nil` in the map: [battery label](#examples) |
| Change volume or brightness with the scroll wheel | `on_wheel` plus `:get()` and `:invoke`: [volume](#examples), [brightness](#brightness) |
| Show an OSD when volume changes | `:on_change` writing state: [OSD example](#examples) |
| Show a clock | `os.date` over `mantle.system.time`: [system](#system) |
| Give each monitor its own bar and workspaces | A function `child` gets the connector name; match it in `workspaces.outputs`: [workspaces](#workspaces) |
| Show CPU and memory use | `configure` once at top level, then map: [sysinfo](#sysinfo) |
| Name or iconify the focused app | `workspaces.active_client.class` through `applications.by_app_id`: [applications](#applications) |
| Play or pause whatever is playing | `control` on `players[1].id`: [mpris](#mpris) |
| Show a microphone or camera indicator | [privacy](#privacy) |
| Keep the screen awake (caffeine) | Below |
| Know whether an action worked | Watch the state it changes. Below: a failed Wi-Fi join |

A caffeine toggle. The hold survives reloads, so the flag records it in [named state](signals.md#named-state):

```lua
local caffeine = state("caffeine", false)

button {
    on_click = function()
        if caffeine:get() then
            mantle.idle:release_inhibit()
        else
            mantle.idle:inhibit("caffeine")
        end
        caffeine:set(not caffeine:get())
    end,
    children = {
        text {
            content = computed({ caffeine, mantle.idle }, function(held, idle)
                if held then
                    return "awake (held)"
                elseif idle and idle.inhibited then
                    return "awake (another app)"
                end
                return "idle allowed"
            end),
        },
    },
}
```

`invoke` returns nothing, so a failed `connect` shows up in state instead:

```lua
text {
    foreground = "#F38BA8",
    content = mantle.network:map(function(network)
        local failure = network and network.connect_error
        return failure and (failure.ssid .. ": " .. failure.message) or ""
    end),
}
```

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `attempt to index a nil value` in a `:map` at startup | Every capability is `nil` until its first push, and some stay `nil` (no backend). Guard the whole payload first |
| Optional field missing | A JSON `null` arrives as an absent key. Fields marked `?` in `lua-meta/mantle.lua` need their own guard (`audio.volume` is `nil` with no default sink) |
| `local ok = mantle.audio:invoke(...)` is always `nil` | `invoke` is fire-and-forget. Bind the state it changes; read `mantle log` for dropped commands |
| An action silently does nothing | Wrong argument type or count, often a float where an `integer` goes (`brightness:invoke("set", 50.0)`). Round with `math.floor`, check `mantle log` |
| `on_change` fires at startup with `previous == nil` | That push is learned state, not a change; return early. A respawned Renderer replays every snapshot the same way. An in-place reload keeps the last value, so its next push has a real `previous` |
| `on_change` fires with nothing visibly changed | It runs per push, and a push carries the whole snapshot. Compare the fields you care about |
| `register_threshold` inside `on_change`, a timer or a click handler | Each call adds another registration until the next reload. Register once at top level, or keep the handle and `cancel_threshold` it |
| An `inhibit` hold that never ends | Holds survive reloads and are counted. Record the hold in a `state` and release exactly once per `inhibit` |
| Workspace labels show large or odd numbers on niri | `id` is opaque on niri; draw `idx` (niri: 1-based position per output; Hyprland: the workspace number, equal to `id`) |
| Workspace strip differs between compositors | Hyprland lists no empty workspaces and `focus` on a missing number creates it; niri ignores unknown ids. `special` is `nil` on niri, `overview_open` `nil` on Hyprland. Branch on `workspaces.compositor` |
| Window flags are `nil` | `fullscreen` and `maximized` are `nil` on niri, `minimized` except on wlr; `set_fullscreen` and `set_maximized` are no-ops on niri |
| `sysinfo` stays `nil` | It reads nothing until `mantle.sysinfo:invoke("configure", { cpu_interval = 2 })` |

Source: [namespace](../../renderer/src/lua/namespace.rs), [capability](../../renderer/src/lua/capability.rs),
[idle](../../renderer/src/lua/idle.rs), [lazy start and dispatch](../../supervisor/src/capabilities/lifecycle.rs),
[argument decoding](../../supervisor/src/action.rs), [idle holds](../../supervisor/src/capabilities/idle/controller.rs),
payload and action types under [`supervisor/src/capabilities/`](../../supervisor/src/capabilities/).

See also: [signals](signals.md) for `:map`, `computed` and named state; [input](input.md) for click and wheel handlers; [scripting](scripting.md) for `session_process`, `persistent_table` and `timer`; [services](../services.md) for backends.
