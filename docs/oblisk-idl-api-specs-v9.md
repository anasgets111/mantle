# Oblisk IDL and API Specification (v9)
## Strict Rust-Lua Boundary and Interface Contract (v9)

This document defines the strict binary and type boundaries between the Rust platform layers (Supervisor and Renderer) and the Lua configuration environment. To prevent LLM code generation from hallucinating interfaces, this specification serves as the absolute, compiler-validated contract for type marshalling, reactive state signals, command schemas, and lazy registration handshakes.

---

## 1. Rust-Lua Marshalling and Type Mapping

All data passing across the Rust-Lua boundary (driven by `mlua` hosting PUC Lua 5.4) is mapped according to the following strict, non-coercive rules. Any mismatch must fail immediately at construction/execution time rather than degrading silently or raising unhandled panic errors.

### 1.1 Fundamental Type Mapping Table

| Rust Type | Lua Type | Boundary Mapping Rules & Constraints |
| :--- | :--- | :--- |
| `f64` | `number` | Double-precision float. NaN and Inf are rejected; mapped to Lua `nil` with a warning. |
| `i64` / `u64` | `integer` / `number` | Mapped to Lua integer if within `[-2^53 + 1, 2^53 - 1]`. Out-of-bounds integers are rejected. |
| `String` | `string` | UTF-8 encoded, byte-length limited string. Null-bytes are rejected. |
| `bool` | `boolean` | Clean mapping. No type coercion (e.g., non-zero integer is not converted to `true`). |
| `Option<T>` | `T` or `nil` | Maps to its inner type `T` on `Some(T)`, or to Lua `nil` on `None`. |
| `Vec<T>` | `table` (array) | 1-indexed dense Lua table. Sparse or mixed-type tables are rejected. |
| `HashMap<String, T>`| `table` (dictionary) | Key-value associative table. Numeric keys in dictionaries are rejected. |
| `Box<Signal<T>>` | `userdata` (`Signal`) | Opaque C-userdata reference containing a stable pointer to the Rust-owned signal. |
| `OpaqueHandle` | `userdata` (`Handle`) | Stable generation-scoped handle with lifetime tracked by Rust-owned leases. |

### 1.2 The Reactive Signal Sentinel (`Signal`)

Signals are exposed to Lua as read-only or read-write userdata primitives. 

*   **Read-Only Signal Methods**:
    *   `signal:get()`: Returns the current unwrapped primitive value.
    *   `signal:map(fn)`: Returns a new `Computed` signal computed by applying the Lua function `fn` to the parent value.
*   **Computed Signal Rules**:
    *   `computed(dependencies, fn)`: Exposes a multi-dependency computed signal. The `dependencies` argument must be an array of `Signal` or `Computed` handles.
    *   The evaluation function `fn` must be entirely side-effect-free. CPU runtime is capped at 5ms per evaluation.
*   **Dirty Propagation**:
    *   When Rust updates a backing hardware value, it writes to the Rust signal and marks its associated layout/UI nodes dirty.
    *   The engine evaluates computed trees down to fixed points once per tick (depth limit of 8) before drawing.

---

## 2. Core Engine Signals (Read-Only State Schema)

The active Renderer process populates the global `oblisk` state tree with the following schema. No other properties exist in the global space. All fields below return a `Signal` wrapping the indicated inner type.

### 2.1 Keyboard Modifier & Layout State (`oblisk.keyboard`)

Exposes keyboard modifier lock states and active layout properties mapped from Wayland seat events and compositor-specific hooks.

*   `keyboard.caps_lock`: `boolean` (Active = `true`, Inactive = `false`)
*   `keyboard.num_lock`: `boolean` (Active = `true`, Inactive = `false`)
*   `keyboard.scroll_lock`: `boolean` (Active = `true`, Inactive = `false`)
*   `keyboard.active_layout`: `string` (The user-friendly active layout name, e.g., `"English (US)"` or `"Turkish"`)
*   `keyboard.active_layout_index`: `integer` (The active layout index in the configuration array, 0-indexed)
*   `keyboard.layouts`: `table` (Array of strings representing all configured layout names)

### 2.2 Battery Status (`oblisk.battery`)

Polled by the Supervisor via sysfs `/sys/class/power_supply/` at 5-second intervals and pushed to the Renderer.

*   `battery.present`: `boolean` (True if physical battery detected)
*   `battery.percent`: `integer` (`0` to `100`)
*   `battery.charging`: `boolean` (True if status is "Charging" or "Full")
*   `battery.time_remaining`: `integer` (Estimated minutes remaining; `-1` if unknown/calculating)

### 2.3 Brightness State (`oblisk.brightness`)

Monitored via the Supervisor's udev backlight event loop in `/sys/class/backlight/`.

*   `brightness.percent`: `integer` (`0` to `100`)

### 2.4 Audio State (`oblisk.audio`)

Monitored via PipeWire/WirePlumber client event loop on a dedicated Supervisor thread.

*   `audio.volume`: `number` (`0.0` to `1.0` linear scale)
*   `audio.muted`: `boolean`
*   `audio.default_sink_name`: `string` (UTF-8 name of the active audio output)
*   `audio.event_sounds_enabled`: `boolean` (Global master switch for event sound feedback)

### 2.5 Network State (`oblisk.network`)

Monitored via System D-Bus hooks on `org.freedesktop.NetworkManager`.

*   `network.connected`: `boolean` (True if any active network connection is established)
*   `network.type`: `string` (`"wifi"`, `"ethernet"`, `"none"`)
*   `network.wifi_enabled`: `boolean` (True if physical Wi-Fi adapter radio is enabled/powered)
*   `network.ethernet_enabled`: `boolean` (True if physical Ethernet interface link state is active)
*   `network.networking_enabled`: `boolean` (True if NetworkManager global networking master switch is enabled)
*   `network.available_networks`: `table` (Array of scanned Wi-Fi access points available in range)
    *   `access_point` structure:
        *   `ssid`: `string` (SSID string name)
        *   `strength`: `integer` (`0` to `100` signal strength percent)
        *   `secure`: `boolean` (True if connection requires WPA/WPA2/WPA3 password)
        *   `hidden`: `boolean` (True if network does not broadcast its SSID)
        *   `active`: `boolean` (True if currently connected to this access point)
*   `network.connection_details`: `table` (Detailed properties dictionary of the current active default connection)
    *   `details` structure:
        *   `type`: `string` (`"wifi"`, `"ethernet"`)
        *   `ip_address`: `string` (Active IPv4 address, e.g., `"192.168.1.124"`)
        *   `subnet`: `string` (Subnet mask prefix, e.g., `"24"` for `255.255.255.0`)
        *   `gateway`: `string` (Default gateway IP, e.g., `"192.168.1.1"`)
        *   `dns`: `table` (Array of Active DNS resolver IP strings)
        *   `interface`: `string` (The physical adapter interface name, e.g., `"wlan0"`, `"eth0"`)

### 2.6 Bluetooth State (`oblisk.bluetooth`)

Monitored via System D-Bus hooks on `org.bluez` and WirePlumber/PipeWire audio parameters.

*   `bluetooth.enabled`: `boolean` (True if the local BlueZ adapter is powered)
*   `bluetooth.discovering`: `boolean` (True if the adapter is actively running an RF discovery scan)
*   `bluetooth.discovered_devices`: `table` (Array of uncoupled bluetooth devices scanned in range)
    *   `discovered_device` structure:
        *   `name`: `string` (User-friendly device name)
        *   `mac`: `string` (Standardized MAC address, e.g., `"00:1A:7D:DA:71:11"`)
        *   `rssi`: `integer` (Received Signal Strength Indicator in dBm)
        *   `connected`: `boolean`
        *   `paired`: `boolean`
*   `bluetooth.connected_devices`: `table` (Array of currently active paired and connected devices with audio metrics)
    *   `connected_device` structure:
        *   `name`: `string`
        *   `mac`: `string`
        *   `battery`: `integer` (Estimated device battery charge percent `0` to `100`; `-1` if not reported)
        *   `codec`: `string` (Active negotiated audio compression codec, e.g., `"LDAC"`, `"AAC"`, `"SBC"`, `"aptX-HD"`; empty if non-audio device)
        *   `available_codecs`: `table` (Array of supported audio codecs reported by the device for negotiation)

### 2.7 Notifications State (`oblisk.notifications`)

Durable 100-entry queue managed by the Supervisor's D-Bus `org.freedesktop.Notifications` daemon.

*   `notifications.feed`: `table` (Dense array of notification structures)
    *   `notification` structure:
        *   `id`: `integer` (Unique notification identifier)
        *   `app_name`: `string`
        *   `summary`: `string` (Strictly truncated to 128 bytes)
        *   `body`: `string` (Strictly truncated to 512 bytes)
        *   `html_formatted_body`: `string` (The safe HTML sub-string containing parsed `<b>`, `<i>`, and `<a href="...">` markup elements parsed off-thread)
        *   `icon_path`: `string` (Sanitized file:// URI path to decoded icon in `/dev/shm/` or default fallback)
        *   `urgency`: `integer` (`0` = Low, `1` = Normal, `2` = Critical)
        *   `has_reply`: `boolean` (True if the notification supports direct inline text-input replies)
        *   `reply_action_key`: `string` (The specific action key for submitting an inline reply, e.g., `"inline-reply"`)

### 2.8 System Updates State (`oblisk.updates`)

Calculated by the Supervisor based on pacman db sync directory inotify events and offline-safe package checking.

*   `updates.count`: `integer` (Number of pending updates)
*   `updates.list`: `table` (Array of strings representing package names; capped at 50 entries)

### 2.9 Weather State (`oblisk.weather`)

Off-thread HTTP fetching managed by Rust. Requires initial configuration in Lua before activation.

*   `weather.available`: `boolean`
*   `weather.temp`: `number` (Temperature in configured unit)
*   `weather.condition`: `string` (e.g. `"Clear"`, `"Rain"`, `"Clouds"`)
*   `weather.icon`: `string` (Icon identifier string)
*   `weather.humidity`: `integer` (Percent `0` to `100`)

### 2.10 Live Audio Visualization (`oblisk.cava`)

A PipeWire capture stream executing fast FFT analysis directly inside a Rust-native thread.

*   `cava.bars`: `table` (Array of exactly 20 elements containing `number` values normalized to `[0.0, 1.0]`)

### 2.11 Media Players (`oblisk.mpris`)

Durable MPRIS controller monitored by the Supervisor session D-Bus interface.

*   `mpris.players`: `table` (Array of player state structures)
    *   `player` structure:
        *   `id`: `string` (Bus name suffix, e.g., `"spotify"`)
        *   `identity`: `string` (User-friendly name)
        *   `play_state`: `string` (`"Playing"`, `"Paused"`, `"Stopped"`)
        *   `title`: `string`
        *   `artist`: `string`
        *   `album_art_path`: `string` (Sanitized local filepath URI)
        *   `position`: `integer` (Current playback offset in microseconds)
        *   `length`: `integer` (Total track length in microseconds)

### 2.12 Idle Timeout State (`oblisk.idle`)

Binds to Wayland `ext_idle_notifier_v1` on the Supervisor.

*   `idle.is_idle`: `boolean` (True if any active registered idle threshold has been breached)



### 2.14 Application Launcher (`oblisk.launcher`)

Monitors XDG desktop applications and processes fuzzy-matching natively in Rust.

*   `launcher.search_query`: `string` (The active search input filter)
*   `launcher.results`: `table` (Array of filtered unified search structures, strictly capped at the top 20 matches)
    *   `launcher.results_loading`: `boolean` (True if background asynchronous web suggestion network calls are active)
    *   Unified structures can be one of the following based on the `kind` field:
        *   **`desktop`** (XDG Desktop Entry):
            *   `kind`: `string` (`"desktop"`)
            *   `name`: `string` (Application name, e.g., `"Firefox"`)
            *   `exec`: `string` (The command execution line, stripped of `%F/%U` specifiers)
            *   `icon`: `string` (Icon name or absolute path)
            *   `description`: `string` (Tooltip or sub-label parsed from Comment)
        *   **`calc`** (Local Mathematical Calculation):
            *   `kind`: `string` (`"calc"`)
            *   `expression`: `string` (The input math expression, e.g., `"120 * 1.25"`)
            *   `result`: `string` (The calculated result, e.g., `"150"`)
            *   `icon`: `string` (e.g., `"accessories-calculator"`)
        *   **`currency`** (Local Offline-Cached Currency Conversion):
            *   `kind`: `string` (`"currency"`)
            *   `expression`: `string` (The query expression, e.g., `"100 USD to EUR"`)
            *   `result`: `string` (The converted value with symbol, e.g., `"92.50 EUR"`)
            *   `icon`: `string` (e.g., `"currency-exchange"`)
        *   **`web_suggestion`** (Debounced Online Autocomplete Suggestion):
            *   `kind`: `string` (`"web_suggestion"`)
            *   `title`: `string` (The suggestion title, e.g., `"rust fn pointer syntax"`)
            *   `url`: `string` (The target web destination search URL)
            *   `icon`: `string` (e.g., `"system-search"`)

### 2.16 Persistent User State and Storage Paths (`oblisk.system`)

Exposes directories conforming to the XDG Base Directory Specification and manages state changes.

*   `system.state`: `table` (A reactive, read-only dictionary of persistent interactive states loaded from `$XDG_STATE_HOME/oblisk/state.json`).
*   `system.config_path`: `string` (The user configuration directory, e.g., `/home/user/.config/oblisk/`).
*   `system.state_path`: `string` (The user state directory, e.g., `/home/user/.local/state/oblisk/`).
*   `system.cache_path`: `string` (The user cache directory, e.g., `/home/user/.cache/oblisk/`).
*   `system.shm_path`: `string` (The user fast memory-mapped folder, e.g., `/dev/shm/oblisk-1000/`).


### 2.17 Workspaces & Output State (`oblisk.workspaces`)

Exposes nested, multi-display workspaces and scratchpads dynamically parsed from the active compositor's control IPC.

*   `workspaces.outputs`: `table` (Array of active physical outputs)
    *   `output` structure:
        *   `name`: `string` (The output name, e.g., `"eDP-1"` or `"DP-2"`)
        *   `is_focused`: `boolean` (True if this output currently holds the active keyboard seat)
        *   `scale`: `number` (The physical scaling factor applied to this monitor)
        *   `workspaces`: `table` (Dense array of workspace structures bound to this monitor)
            *   `workspace` structure:
                *   `id`: `integer` (Compositor-specific workspace identifier)
                *   `name`: `string` (The user-friendly display name or workspace tag)
                *   `is_visible`: `boolean` (True if currently visible on any display)
                *   `is_active`: `boolean` (True if selected as active on its parent output)
                *   `is_focused`: `boolean` (True if active on the output holding keyboard seat)
                *   `is_empty`: `boolean` (True if no user application client windows are running)
                *   `is_special`: `boolean` (True if this is an overlay scratchpad workspace)
*   `workspaces.active_workspace_id`: `integer` (The ID of the globally focused workspace)
*   `workspaces.special_active`: `boolean` (True if any special scratchpad workspace is currently displayed)

### 2.18 Rescue Mode & Recovery State (`oblisk.rescue`)

Exposes state flags from the Renderer's internal compilation and sandboxing modules during configuration failures.

*   `rescue.is_rescue`: `boolean` (True if running in fallback mode due to compilation or execution crashes)
*   `rescue.error_log`: `string` (The raw Lua compiler error or stack trace parsed by the Rust engine)
*   `rescue.config_valid`: `boolean` (True if `shell.lua` validates successfully)
*   `rescue.reload_count`: `integer` (The number of reloads executed in the current session)

### 2.13 Webcam Usage (`oblisk.webcam`)

Monitors PipeWire video nodes and `/dev/video*` state events.

*   `webcam.active`: `boolean` (True if any video recording node is in `PW_NODE_STATE_RUNNING`)
*   `webcam.active_clients`: `table` (Array of strings containing names of applications utilizing the camera)

### 2.15 Power & Session Management (`oblisk.power`)

Provides state indicators and configurations for system power states (shutdown, reboot, suspend) and session actions (logout, DPMS standby).

*   `power.configured`: `boolean` (True if compositor-specific session commands have been registered)
*   `power.dpms_active`: `boolean` (True if screens are currently put into DPMS-off standby)

---

## 3. Command Execution Protocol (Write Path)

All state mutations and system actions (volume changes, locking, input, dismissals) must traverse the private IPC command channel back to the Supervisor's `CapabilityAuthority`. Lua configurations invoke these through method calls on imported modules.

### 3.1 Serialization Format
All commands are serialized in the Renderer and written to the control socket using a length-prefixed, 24-byte header followed by a JSON payload.

```text
+-----------------------+-----------------------+----------------------------------+
| Magic (4 Bytes: OBCO) | Gen ID (8 Bytes: u64) | Payload Len (12 Bytes: Base10)  |
+-----------------------+-----------------------+----------------------------------+
|                                                                                  |
|                        JSON String Payload (Payload Len Bytes)                   |
|                                                                                  |
+----------------------------------------------------------------------------------+
```

### 3.2 JSON Command Envelope Schema

The payload must strictly validate against the following structural schema:

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "properties": {
    "generation_id": { "type": "integer", "minimum": 1 },
    "expected_revision": { "type": "integer", "minimum": 0 },
    "capability": { "type": "string", "enum": ["brightness", "audio", "notifications", "mpris", "idle", "polkit", "launcher", "network", "bluetooth", "power", "system"] },
    "action": { "type": "string" },
    "arguments": {
      "type": "array",
      "items": {
        "anyOf": [
          { "type": "string" },
          { "type": "number" },
          { "type": "boolean" }
        ]
      },
      "maxItems": 16
    }
  },
  "required": ["generation_id", "expected_revision", "capability", "action", "arguments"]
}
```

### 3.3 Target Command Protocols & Validations

| Module Method | IPC Command JSON Payload Details |
| :--- | :--- |
| `system:write_state(key, val)` | `capability: "system", action: "write_state", arguments: [key, val]`<br>**Validation**: `key` must be alphanumeric string (max 64 bytes). `val` must be a string, number, or boolean. Writes atomically to `state.json`. |
| `notifications:reply(id, text)`| `capability: "notifications", action: "reply", arguments: [id, text]`<br>**Validation**: `id` must be an active notification ID. `text` must be a string (max 512 bytes). Dispatches text back to D-Bus. |
| `brightness:set(pct)` | `capability: "brightness", action: "set", arguments: [pct]`<br>**Validation**: `pct` must be an integer in range `[0, 100]`. |
| `audio:set_volume(vol)` | `capability: "audio", action: "set_volume", arguments: [vol]`<br>**Validation**: `vol` must be a float in range `[0.0, 1.0]`. |
| `audio:toggle_mute()` | `capability: "audio", action: "toggle_mute", arguments: []` |
| `notifications:dismiss(id)` | `capability: "notifications", action: "dismiss", arguments: [id]`<br>**Validation**: `id` must match a valid active notification ID. |
| `mpris:send_command(p_id, cmd)`| `capability: "mpris", action: "control", arguments: [p_id, cmd]`<br>**Validation**: `cmd` must be one of `"play"`, `"pause"`, `"next"`, `"previous"`. |
| `idle:lock()` | `capability: "idle", action: "lock_session", arguments: []` |
| `launcher:set_query(query)` | `capability: "launcher", action: "set_query", arguments: [query]`<br>**Validation**: `query` must be a string (max 256 bytes). Updates results snapshot. |
| `launcher:launch(exec_str)` | `capability: "launcher", action: "launch", arguments: [exec_str]`<br>**Validation**: Launches a disowned detached application process on the Supervisor. |
| `launcher:launch_url(url)` | `capability: "launcher", action: "launch_url", arguments: [url]`<br>**Validation**: Opens URL using standard `xdg-open` within a detached disowned process. |
| `clipboard:set_text(text)` | `capability: "clipboard", action: "set_text", arguments: [text]`<br>**Validation**: Writes text directly to the system clipboard and ownership cache. |
| `keyboard:set_layout(index)` | `capability: "keyboard", action: "set_layout", arguments: [index]`<br>**Validation**: `index` must be an integer within configured layouts. |
| `keyboard:next_layout()` | `capability: "keyboard", action: "next_layout", arguments: []` |
| `polkit:authenticate(cookie, pwd)`| `capability: "polkit", action: "auth_response", arguments: [cookie]`<br>**Security Boundary**: The password parameter `pwd` is written directly to the secure zeroizing native buffer. It is *never* serialized in this JSON payload. The JSON only forwards the action challenge cookie. |
| `audio:play_sound(sound)` | `capability: "audio", action: "play_sound", arguments: [sound]`<br>**Validation**: `sound` must be a string matching an absolute filepath or icon theme name. |
| `audio:set_event_sound(evt, snd)`| `capability: "audio", action: "set_event_sound", arguments: [evt, snd]`<br>**Validation**: `evt` must match a supported system event. `snd` is a sound asset path. |
| `audio:set_event_sounds_enabled(en)`| `capability: "audio", action: "set_event_sounds_enabled", arguments: [en]`<br>**Validation**: `en` must be a boolean. |
| `network:set_wifi_enabled(en)` | `capability: "network", action: "set_wifi_enabled", arguments: [en]`<br>**Validation**: `en` must be a boolean. |
| `network:set_ethernet_enabled(en)`| `capability: "network", action: "set_ethernet_enabled", arguments: [en]`<br>**Validation**: `en` must be a boolean. |
| `network:set_networking_enabled(en)`| `capability: "network", action: "set_networking_enabled", arguments: [en]`<br>**Validation**: `en` must be a boolean. |
| `network:scan()` | `capability: "network", action: "scan", arguments: []`<br>**Validation**: Triggers asynchronous Wi-Fi scanning. |
| `network:connect(ssid, pwd, hid)`| `capability: "network", action: "connect", arguments: [ssid, pwd, hid]`<br>**Validation**: `ssid` is a string (max 32 chars). `pwd` is string (or nil for open). `hid` is boolean (true for hidden). |
| `network:disconnect()` | `capability: "network", action: "disconnect", arguments: []` |
| `bluetooth:set_enabled(en)` | `capability: "bluetooth", action: "set_enabled", arguments: [en]`<br>**Validation**: `en` must be a boolean. |
| `bluetooth:start_discovery()` | `capability: "bluetooth", action: "start_discovery", arguments: []` |
| `bluetooth:stop_discovery()` | `capability: "bluetooth", action: "stop_discovery", arguments: []` |
| `bluetooth:pair(mac)` | `capability: "bluetooth", action: "pair", arguments: [mac]`<br>**Validation**: `mac` must be a valid 17-character MAC address. |
| `bluetooth:connect(mac)` | `capability: "bluetooth", action: "connect", arguments: [mac]`<br>**Validation**: `mac` must be a valid 17-character MAC address. |
| `bluetooth:disconnect(mac)` | `capability: "bluetooth", action: "disconnect", arguments: [mac]`<br>**Validation**: `mac` must be a valid 17-character MAC address. |
| `bluetooth:set_audio_codec(mac, c)`| `capability: "bluetooth", action: "set_audio_codec", arguments: [mac, c]`<br>**Validation**: `mac` must be valid. `c` must be a string matching one of the supported codecs. |
| `workspaces:focus(id)` | `capability: "workspaces", action: "focus", arguments: [id]`<br>**Validation**: `id` must be an integer (or string for scratchpads). Focuses the target workspace on its assigned monitor. |
| `workspaces:move_window_to(id)`| `capability: "workspaces", action: "move_window_to", arguments: [id]`<br>**Validation**: Moves active window client to target workspace. |
| `workspaces:toggle_special(name)`| `capability: "workspaces", action: "toggle_special", arguments: [name]`<br>**Validation**: Toggles display of special scratchpad workspace by name. |
| `rescue:reload_config()` | `capability: "rescue", action: "reload_config", arguments: []`<br>**Validation**: Clears error cache, runs compiler pass on `shell.lua`, reloads Renderer if valid. |

| `power:configure(cfg)` | `capability: "power", action: "configure", arguments: [cfg]`<br>**Validation**: `cfg` is a dictionary table containing custom string keys (`shutdown_cmd`, `reboot_cmd`, `suspend_cmd`, `logout_cmd`, `dpms_on_cmd`, `dpms_off_cmd`). |
| `power:shutdown()` | `capability: "power", action: "shutdown", arguments: []` |
| `power:reboot()` | `capability: "power", action: "reboot", arguments: []` |
| `power:logout()` | `capability: "power", action: "logout", arguments: []` |
| `power:suspend()` | `capability: "power", action: "suspend", arguments: []` |
| `power:dpms(state)` | `capability: "power", action: "dpms", arguments: [state]`<br>**Validation**: `state` must be a boolean. |

---

## 4. The Lazy Capability Activation Handshake

To enforce the **Zero CPU, Zero Memory** resource constraint for unused hardware interfaces, background threads and listeners are initialized purely on-demand.

### 4.1 Sequence of Handshake

```text
Renderer (Lua VM)                      Renderer (Rust Engine)                Supervisor (Durable Daemon)
       │                                        │                                       │
       │─── require("oblisk.audio") ───────────▶│                                       │
       │                                        │─── IPC: RegisterCapability(audio) ───▶│
       │                                        │                                       │─── [Spawns PipeWire Thread]
       │                                        │◀── ACK IPC (Initial State) ───────────│
       │◀── Returns module proxy with signals ──│                                       │
```

1.  **Lua Evaluation**: The user's configuration runs. It evaluates `require("oblisk.audio")`.
2.  **Registration Trigger**: The Renderer's rooted Lua loader intercepts this import. Before returning the proxy object to Lua, the Renderer executes a synchronous `RegisterCapability` handshake to the Supervisor over the private control socket.
    *   **Payload**: `{"generation_id": N, "register": "audio"}`
3.  **Supervisor Activation**: The Supervisor receives the registration. It validates the request against the epoch, checks if the internal PipeWire thread is already active, and starts the capability monitoring thread if it is currently dormant.
4.  **Initial Snapshot Sync**: The Supervisor immediately pushes the initial state snapshot of the active sink volume and muted status down to the Renderer.
5.  **Binding Mount**: The Renderer mounts the static signals to the Lua proxy state table and completes the `require` execution.

---

## 5. Declarative UI Node Primitives (The AST Contract)

The Rust scene-graph engine parses layout trees built from sugar constructors. To keep the engine completely product-neutral, **no visual compound components** are written in Rust. Only the following geometric nodes are defined.

### 5.1 The Abstract Node Base Class Table

Every node schema contains the following base layout properties:

| Property Name | Type | Valid Range / Options | Layout Engine Interpretation |
| :--- | :--- | :--- | :--- |
| `width` | `integer` / `string` | `[0, 8192]` or `"Fill"` | Explicit width or fill maximum available parent container space. |
| `height` | `integer` / `string` | `[0, 8192]` or `"Fill"` | Explicit height or fill maximum available parent container space. |
| `margin` | `table` | `{ top, right, bottom, left }` | Outer spacing boundaries. Omitted fields default to `0`. |
| `padding` | `table` | `{ top, right, bottom, left }` | Inner spacing boundaries. Omitted fields default to `0`. |
| `align_h` | `string` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` | Horizontal alignment distribution inside parent. |
| `align_v` | `string` | `"Start"`, `"Center"`, `"End"`, `"Stretch"` | Vertical alignment distribution inside parent. |
| `visible` | `boolean` / `Signal` | `true`, `false`, or binary signal handle | Determines if the node enters the constraint pass and rendering. |

### 5.2 Specific Geometric Node Schemas

#### 1. `panel`
A basic containment bounding box.
*   `background`: `string` (Hex-color code `#RRGGBB` or `#RRGGBBAA`)
*   `children`: `table` (Dense array of child node structures)

#### 2. `row`
Arranges children horizontally.
*   `spacing`: `integer` (Pixels of space between siblings)
*   `children`: `table`

#### 3. `column`
Arranges children vertically.
*   `spacing`: `integer`
*   `children`: `table`

#### 4. `text`
Draws shaped unicode glyph text via `cosmic-text`.
*   `content`: `string` / `Signal` (The string text to display)
*   `font_size`: `integer` (Defaults to `14`)
*   `foreground`: `string` (Hex-color code)

#### 5. `icon`
Draws a system SVG/PNG icon.
*   `name`: `string` (The theme name, e.g., `"audio-volume-high"`)
*   `size`: `integer` (Bounding box diameter)

#### 6. `rect`
A basic custom drawing element for styling visual containers or graphs.
*   `background`: `string` (Hex-color code)
*   `radius`: `integer` (Corner rounding radius)

#### 7. `button`
Receives input focus and pointer events.
*   `children`: `table` (Content elements nested inside the button boundary)
*   `on_click`: `function` (Lua callback executed on mouse click or pointer tap)

#### 8. `list`
A fast-reconciling virtual repeater element.
*   `source`: `Signal` (Must wrap a flat array table)
*   `itemfn`: `function` (A Lua builder function that is executed for every index, returning child nodes. Updates are keyed to prevent teardowns)

#### 9. `textfield` (The Engine Security Exception)
An IME-aware native input field mapped directly to Rust-owned `wp-text-input-v3`.
*   `placeholder`: `string`
*   `mask_character`: `string` (Capped at 1 byte; if specified, hides typed input)
*   `on_change`: `function` (Lua callback executed on change. Key events are swallowed inside Rust's memory blocks during sensitive lock states)


#### 10. `wallpaper`
A background layer-shell surface designed for per-monitor wallpapers.
*   `source`: `string` / `Signal` (The file path or URI of the target image asset. Max size: 32MB, max resolution: 8192x8192)
*   `fit`: `string` (The fitting algorithm: `"Cover"`, `"Contain"`, `"Stretch"`, `"Tile"`. Defaults to `"Cover"`)
*   `animation_type`: `string` (The transition animation: `"None"`, `"Crossfade"`, `"Slide"`, `"Sweep"`, `"Zoom"`. Defaults to `"Crossfade"`)
*   `animation_duration`: `integer` (Duration in milliseconds. Defaults to `500`)
*   `animation_easing`: `string` (`"Linear"`, `"EaseIn"`, `"EaseOut"`, `"EaseInOut"`. Defaults to `"EaseInOut"`)
