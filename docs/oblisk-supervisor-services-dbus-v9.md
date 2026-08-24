# Oblisk Supervisor Services and D-Bus Integration Specification (v9)
## Durable Daemon Services and Multi-Process Security Contracts

This specification defines the persistent system services owned and executed by the long-lived **Oblisk Supervisor** process. These services survive hot-reloads of the ephemeral Renderer process, ensuring that desktop state, notifications, system trays, and security handshakes remain fully persistent without memory leaks or state corruption.

---

## 1. Durable D-Bus Notifications Server (`org.freedesktop.Notifications`)

The Supervisor claims and maintains ownership of the `org.freedesktop.Notifications` D-Bus interface. It processes, validates, and queues incoming notifications, presenting a sanitized, read-only snapshot to the active Lua VM.

### 1.1 DBus Registration and Lifecycle
*   **Bus**: Session Bus.
*   **Object Path**: `/org/freedesktop/Notifications`.
*   **Interface**: `org.freedesktop.Notifications`.
*   **Persistence**: Handled entirely inside the Supervisor’s main async loop. The registration is made at boot time and is never released, even if all Renderer processes crash.

### 1.2 The 100-Entry Memory-Bounded Queue
To prevent memory exhaustion attacks, incoming notifications must pass through strict size limits and a FIFO sliding queue:
*   **Queue Cap**: Max 100 active notifications. When notification 101 arrives, notification 1 (the oldest) is automatically removed and a `NotificationClosed(1, Reason: Expired)` signal is emitted to the D-Bus.
*   **String Truncation**: 
    *   `app_name` is truncated to 64 bytes.
    *   `summary` is truncated to 128 bytes.
    *   `body` is truncated to 512 bytes.
*   **Asset Sanitation**: Raw image byte-arrays passed via the `image-data` or `icon_data` D-Bus hint are **categorically rejected** (to prevent multi-megabyte allocations in the Supervisor). Instead, Oblisk only parses file-path URIs (`file://`) and standard icon names. If a raw byte array is supplied, the Supervisor writes the payload off-thread to a temporary, sandboxed file `/dev/shm/oblisk-notifications/notif-{id}.png` and substitutes the raw data with that path.

### 1.3.1 D-Bus Interactive Inline Replies and Actions
When a client application broadcasts a notification containing an inline reply requirement (detected via the `x-kde-reply` D-Bus hint or a corresponding action key), the Supervisor parses and tracks the interaction. It serializes `has_reply = true` and populates `reply_action_key` (typically set to `"inline-reply"`) in the synced snapshot envelope.
When the user submits text inside the Lua visual `textfield`, the Renderer issues a secure `notifications:reply(id, text)` command. The Supervisor captures this payload and emits the corresponding `ActionInvoked(id, action_key, text)` signal on the `org.freedesktop.Notifications` interface, returning the typed response safely to the calling D-Bus client.

### 1.3.2 Cosmic-Text Safe HTML & Markdown Preprocessing
To support formatted bodies containing links or bold accents without exposing the UI layout engine to security vulnerabilities:
1. **Tokenizer Filtering**: Before pushing the notifications snapshot, the Supervisor pre-filters incoming `body` strings using a non-backtracking, regex-safe HTML tokenizer. All executable blocks, script tags, style overrides, and images are stripped out.
2. **Safe Formatting Runs**: The parser extracts only a secure subset of HTML tags: `<b>` (bold weight), `<i>` (italic style), and `<a href="...">` (anchor links).
3. **Structured Snapshot**: It builds a localized snapshot array containing raw text and character offsets mapping formatting runs (e.g., character index `12` to `24` matches a link to `https://google.com`). This metadata is passed in the `html_formatted_body` field.
4. **Hit-Testing & Disowned Launching**: The Renderer's text-shaping engine (`cosmic-text`) reads these runs to apply appropriate fonts and registers bounding coordinates for clickable text regions. When a user clicks a link region, the Renderer dispatches a `launcher:launch_url(url)` call, executing a disowned detached `xdg-open` process on the Supervisor.

### 1.4 The Reactive Lua State Synchronization
The Supervisor exposes the notification queue to the Renderer as a read-only, revisioned snapshot.

```json
{
  "revision": 42,
  "notifications": [
    {
      "id": 1024,
      "app_name": "Spotify",
      "summary": "Now Playing",
      "body": "The Pretender - Foo Fighters",
      "icon_path": "/dev/shm/oblisk-notifications/notif-1024.png",
      "actions": ["default", "Open Player", "dismiss", "Dismiss"],
      "urgency": 1,
      "timeout": 5000
    }
  ]
}
```

*   **IPC Push**: Whenever the queue changes, the Supervisor pushes the updated snapshot over the control socket to the active Renderer.
*   **Dismissal Guard**: When Lua triggers `notifications:dismiss(id)` or `notifications:action(id, action_key)`, the command envelope must include the expected snapshot revision number. If the revision on the Supervisor has changed, the command is rejected, preventing race conditions where a user clicks a button on an item that was already auto-dismissed.

---

## 2. System Tray Host (`StatusNotifierWatcher` and `StatusNotifierItem`)

To achieve seamless tray handling without exposing Lua to raw D-Bus marshaling, the Supervisor registers the `org.kde.StatusNotifierWatcher` service and handles tray client lifecycles off-thread.

### 2.1 Watcher Interfaces
The Supervisor registers:
*   `org.kde.StatusNotifierWatcher` at `/StatusNotifierWatcher`.
*   It listens for registering clients calling `RegisterStatusNotifierItem(service_or_path)`.

### 2.2 Off-Thread Icon Decoding Security
StatusNotifierItems typically send icons as raw ARGB pixel byte streams. Loading these directly into GLES/Vulkan memory from unvalidated sources is a major security vulnerability.
1.  **Buffer Bounds Checks**: The Supervisor receives the raw ARGB array. It verifies that width and height are equal, and that `width * height * 4` matches the payload byte count.
2.  **Size Limits**: Icons are strictly capped at 128x128 pixels. Any icon exceeding this is rejected.
3.  **SHM Spooling**: The Supervisor decodes the validated ARGB data and writes it as a static PNG to `/dev/shm/oblisk-tray/{service_name}.png`.
4.  **Signal Propagation**: The Renderer receives only the secure path `/dev/shm/oblisk-tray/{service_name}.png` inside the tray state snapshot. Lua draws the icon using a standard, unprivileged image node.

### 2.3 Tray State Snapshot Table
The active tray items are serialized and synchronized to Lua as a flat, index-keyed list:

```lua
-- Synced to Lua state as 'tray.items'
{
  {
    id = "org.freedesktop.NetworkManager",
    icon = "/dev/shm/oblisk-tray/nm-applet.png",
    tooltip = "Connected to Wi-Fi",
    menu_path = "/MenuBar", -- DBus Menu path
  }
}
```

*   **Menu Interaction**: When a user clicks a tray icon in Lua, the Renderer does *not* talk to D-Bus. It sends a click command to the Supervisor: `tray:activate(id, x, y)`. The Supervisor converts this to a native `Activate(x, y)` call on the corresponding D-Bus item.

---

## 3. Persistent Media Controls (Supervisor-Owned MPRIS)

Unlike the temporary model defined in ADR-0004, the production-grade MPRIS capability is managed by the persistent **Supervisor**. This eliminates D-Bus connection drops and UI stutter during configuration reloads.

### 3.1 D-Bus Player Discovery
The Supervisor listens to `org.freedesktop.DBus` name changes, automatically discovering any service prefix matching `org.mpris.MediaPlayer2.*` (e.g., Spotify, Audacious, MPV, Firefox).

### 3.2 Metadata Caching and Normalization
The Supervisor subscribes to `org.freedesktop.DBus.Properties.PropertiesChanged` for the interface `org.mpris.MediaPlayer2.Player`. It caches and normalizes the following variables:
*   `Metadata["mpris:trackid"]` -> Unique Track ID.
*   `Metadata["xesam:title"]` -> Title String (truncated to 128 bytes).
*   `Metadata["xesam:artist"]` -> Joined Artist String.
*   `Metadata["mpris:artUrl"]` -> Album art URL (sandboxed or local file path).
*   `PlaybackStatus` -> `"Playing"`, `"Paused"`, or `"Stopped"`.
*   `Volume` -> Float (0.0 to 1.0).

### 3.3 Handoff and Command Guards
*   **Instant Sync**: During a hot-reload, the moment the new Renderer registers its `RegisterCapability(MPRIS)` command, the Supervisor instantly pushes the cached player states. The first frame of the new shell contains correct media information.
*   **Command Verification**: Media player control requests (e.g. `play_pause`, `next`, `previous`) are wrapped in a generation-guarded container. If the active epoch has shifted, the Supervisor drops the command, ensuring a user's multi-click queue doesn't carry over into a newly reloaded desktop state.

---

## 4. Secure Polkit Authorization Agent

The user's Polkit configuration must draw a custom visual window while keeping system secrets completely isolated from the Lua configuration heap.

### 4.1 Security Boundaries and Handoff Mechanics
1.  **DBus Registration**: The Supervisor registers on the system D-Bus authority as a PolicyKit agent (`org.freedesktop.PolicyKit1.Authority.RegisterAgent`).
2.  **Challenge Capture**: When a privileged operation triggers (e.g. installing a package), the Polkit daemon sends an authentication request to the Supervisor.
3.  **Payload Deserialization**: The Supervisor extracts only the essential, non-sensitive metadata:
    *   `action_id` (e.g., `org.archlinux.pkexec.gparted`).
    *   `message` (The display text explaining what application wants root).
    *   `user` (The target system user list).
    *   `cookie` (The transient Polkit transaction cookie).
4.  **Lua VM Event**: The Supervisor serializes this metadata and pushes a `PolkitChallenge` event to the Renderer.

### 4.2 Lua Visual Customization and Rust-Input Gating
Lua describes the popup dialog UI using a dedicated declarative structure. 
*   **The Text Input Exception**: To prevent password keys from leaking into the Lua VM memory space or being read by a malicious Lua script, the user password entry field is rendered as a native Rust `TextField` node.
*   **Rust Memory Sanitization**: Raw keyboard events on this text field bypass Lua entirely. Rust processes the characters directly into an internal `seckey` or zeroized memory allocation.
*   **PAM Validation**: When the user clicks "Authorize", the Renderer sends the secure memory handle back to the Supervisor, which runs the local PAM conversation. The moment validation is complete, the memory buffer in both processes is zeroized via `zeroize::Zeroize`.

```lua
-- polkit_agent.lua
on_event("polkit:challenge", function(challenge)
    return panel {
        align_h = "Center",
        align_v = "Center",
        width = 400,
        height = 250,
        background = "#1E1E2E",
        children = {
            text { content = "Authentication Required" },
            text { content = challenge.message }, -- Ex: "Run GParted as Superuser"
            -- Native Rust-owned password widget (no keystrokes touch Lua)
            textfield {
                id = "password_entry",
                placeholder = "Password...",
                secure = true, -- Masks input, runs zeroization in Rust backend
            },
            button {
                content = "Authenticate",
                on_click = function()
                    -- Passes validation execution to Rust
                    polkit:authenticate(challenge.cookie, "password_entry")
                end
            }
        }
    }
end)
```

---


----

## 5. Native Application Launcher Indexer & Fuzzy Engine (`oblisk.launcher`)

To eliminate the need for external launchers (like rofi/wofi) or heavy, performance-degrading filesystem walks inside Lua, the Supervisor natively manages application indexing and searching.

### 5.1 XDG Desktop File Directory Watcher
*   **Monitored Roots**: The Supervisor registers `inotify` watches on standard application paths:
    *   `/usr/share/applications/`
    *   `/usr/local/share/applications/`
    *   `~/.local/share/applications/`
*   **Dynamic Re-indexing**: On write, delete, or modify filesystem events inside these directories, the Supervisor schedules a low-priority background thread to rebuild the application cache—consuming exactly zero resources when static.

### 5.2 Native Desktop Entry Parser
The background worker parses `.desktop` files using a fast, non-blocking INI parser:
*   **Filtering**: Files containing `NoDisplay=true`, `Hidden=true`, or `OnlyShowIn` (if not matching our Wayland session) are ignored.
*   **Extraction**:
    *   `Name` -> Unified display string.
    *   `Exec` -> Cleaned execution path (stripping XDG field codes like `%f`, `%u`, `%F`, `%U`).
    *   `Icon` -> Icon identifier string or absolute image filepath.
    *   `Comment` -> Mapped to application description.

### 5.3 High-Performance Fuzzy Search Engine
Passing hundreds of application entries into the Lua VM would cause massive garbage collection pauses and UI rendering stutter.
*   **Fuzzy Algorithm**: The Supervisor performs fuzzy matching natively in Rust (using a fast Jaro-Winkler or Smith-Waterman distance scan across names and descriptions).
*   **Bounded Serialization**: The Supervisor sorts and caps the results at **exactly the top 20 matches**.
*   **Reactivity**: When Lua invokes `launcher:set_query("term")`, the Supervisor fuzzy-filters the index in less than 0.1ms and pushes the 20-item results list as a reactive signal snapshot to the Renderer.

### 5.4 Native Mathematical Expression Parser (`calc`)
To prevent the overhead of invoking system shell interpreters or loading Lua-side parsers during search typing:
*   **Regex Interception**: The Supervisor's search handler intercepts the query using a fast regex scanner identifying standard mathematical characters `[0-9\+\-\*\/\^\(\)\.]`.
*   **Isolated Evaluation**: Matches are parsed and evaluated inside a sandboxed math expression engine (`evalexpr`) executing entirely in Rust.
*   **Priority Injection**: Successful calculations instantly compile a results card with `kind: "calc"`, injecting it as the absolute first entry of the synced results table, bypassing XDG lists.

### 5.5 Offline-Cached Currency Conversion (`currency`)
*   **Daily Async Syncing**: The Supervisor manages a lazy daily cron-style update. If the network connectivity signal is `true`, it issues an async HTTP fetch of exchange tables and caches them locally at `/tmp/oblisk-exchange-rates.json`. 
*   **Zero-Network Keystroke Processing**: No network calls are executed when typing currency strings. Calculations are made entirely locally using the cached exchange rates file.
*   **Expression Scopes**: Queries matching patterns like `{amount} {currency_from} to {currency_to}` or `{amount} {currency_from} in {currency_to}` are validated, converted, and injected as a top-priority `currency` kind entry.

### 5.6 Debounced Asynchronous Web Suggestions (`web_suggestion`)
Autocomplete APIs (e.g. DuckDuckGo or Google suggestions) must never introduce latency bottlenecks in the UI textfield event loop:
*   **Online Guardrail**: If `network.connected` is false, the web suggestion pipeline is immediately short-circuited.
*   **High-Resolution Debouncer**: The Supervisor runs a non-blocking thread debouncer. Network request dispatches are delayed by **150ms** after the last keystroke. 
*   **Asynchronous Processing**: Requests are dispatched off the main thread. While waiting, `launcher.results_loading` is marked true.
*   **Response Slicing**: Upon completion, suggestions are parsed, mapped to `web_suggestion` kinds, and unified with local desktop results inside a single atomic IPC transaction, minimizing Lua state transitions.

---


## 6. Wayland Clipboard Persistence (`wl-clip-persist`)

To ensure clipboard contents survive when parent application windows are closed, Oblisk integrates native clipboard persistence directly into the core engine.

### 6.1 Protocol Binding
The Renderer process binds the Wayland `zwlr_data_control_device_v1` protocol. It monitors active clipboard selections across the system.

### 6.2 The Persistence Handshake
1.  **Monitoring**: When a client application sets a clipboard selection, the Renderer receives a `data_offer` event and caches the available MIME types (`text/plain`, `image/png`, etc.).
2.  **Caching**: If the offering application is closed (which would normally empty the clipboard), the Renderer intercepts the exit. It requests the clipboard data from the source window, buffers the bytes directly in Rust memory (capped at 5MB to prevent memory exhaustion), and advertises itself to the compositor as the new active clipboard owner.
3.  **Lua Isolation**: The Lua VM has no hand in this background negotiation. It is exposed strictly to a read-only string signal `clipboard.text` if it needs to draw a dashboard clipboard history widget.

---

## 7. Durable Idle Capability (`ext-idle-notifier-v1`)

To prevent continuous timers and high-latency polling patterns, idle detection is entirely event-driven.

### 7.1 Supervisor Idle Registration
The Supervisor binds to the Wayland compositor’s `ext_idle_notifier_v1` protocol.
*   **No Active Loops**: The Supervisor does not execute timers. It registers discrete threshold intervals requested by Lua directly with the compositor.
*   **Lifecycle Persistence**: Because the Supervisor owns these bindings, idle registrations remain active during Renderer hot-reloads. The countdown is never disrupted.

### 7.2 IPC Coordination
When inactivity thresholds are breached or user input is resumed, the Supervisor pushes simple, lightweight event packets to the active Renderer, executing the corresponding Lua handlers instantly:

```json
{"type": "idled", "threshold": 300}
{"type": "resumed", "threshold": 300}
```


---

## 8. High-Performance Wallpaper Engine & MD3 Palette Generator

To match the animated wallpaper features of Quickshell layouts, Oblisk integrates a native, GPU-accelerated background layer-shell renderer with custom shader-driven transition effects.

### 8.1 Wayland Surface and Caching
*   **Protocol Binding**: The Renderer process maps a dedicated `zwlr_layer_surface_v1` on the `"Background"` layer, anchored to all four monitor edges.
*   **Asset Guards**: Wallpaper files are verified off-thread against a 32MB file size cap and a maximum resolution of 8192x8192 pixels.
*   **GPU Caching**: Validated images are decoded and cached directly as static GPU textures to prevent CPU bottlenecks during visual updates.

### 8.2 Scaling and Fit Algorithms (GPU-Side Box Model)
To handle differing physical monitor aspect ratios without image distortion, the engine performs one-pass texture scaling inside GLES3 fragment shaders:
*   **Cover** (Default): Scales the texture proportionally to completely cover the output surface. Cropping occurs on the excess dimension.
*   **Contain**: Scales the texture proportionally to fit within the screen boundaries, letterboxing or pillarboxing the borders with a customizable background color.
*   **Stretch**: Scales the texture non-proportionally to match the logical monitor coordinates exactly.
*   **Tile**: Renders the texture at its integer-scaled physical size, repeating the image in a grid across the screen.

### 8.3 Shader-Driven Transition Animations
To achieve smooth visual continuity during wallpaper changes (like your `animatedwallpaper.qml` implementation), the engine executes double-buffered transition shaders:
*   **The Transition Sequence**:
    1.  On wallpaper change, the active texture is kept in slot `u_tex_old` and the new texture is loaded into `u_tex_new`.
    2.  An internal timer is registered with Wayland `wl_surface::frame` callbacks, interpolating a transition progress factor `u_progress` from `0.0` to `1.0` over your specified duration.
    3.  The transition shader blends the textures on the GPU in real-time. Once `u_progress == 1.0`, the old texture is released from memory.
*   **Supported Animation Types**:
    *   `Crossfade`: Blends pixel values linearly via `mix(color_old, color_new, u_progress)`.
    *   `Slide`: Translates the textures horizontally or vertically using logical viewport coordinates.
    *   `Sweep`: Applies a directional gradient wipe transition where pixels are revealed along a moving linear boundary.
    *   `Zoom`: Interpolates texture coordinates scaling factor from `0.5` to `1.0` during blend.

### 8.4 Material Design 3 (MD3) Color Palette Extraction
*   **K-Means Background Worker**: Upon a wallpaper transition start, the Supervisor spawns a low-priority thread that downsamples the target image to a 128x128 grid.
*   **Quantization**: Runs a native K-Means color quantizer to extract the five most dominant colors.
*   **State Push**: Computes standard Material Design 3 light and dark mode color roles (`primary`, `on_primary`, `background`, `surface`, `accent`) and publishes them to Lua as a static table `oblisk.palette` so user widgets bind to them reactively.


---


## 9. Interactive Network Manager D-Bus Controller (`oblisk.network`)

The Supervisor registers as an event observer and controller on the system D-Bus for `org.freedesktop.NetworkManager`. All network state monitoring and mutation requests are processed asynchronously in a dedicated Rust thread using non-blocking IPC.

### 9.1 Master Switch and Radio Controls
*   **Global Networking Switch**: Sets the property `NetworkingEnabled` (boolean) on interface `org.freedesktop.NetworkManager` at path `/org/freedesktop/NetworkManager` to enable/disable networking globally (airplane mode).
*   **Wi-Fi Radio Switch**: Sets the property `WirelessEnabled` (boolean) on the main NetworkManager interface to toggle the physical wireless transmitter radio on/off.
*   **Ethernet Link Control**: Finds physical ethernet adapters (device type `1`). It calls `Disconnect` on the matching device path `/org/freedesktop/NetworkManager/Devices/{id}` under interface `org.freedesktop.NetworkManager.Device` or toggles link states natively.

### 9.2 Wi-Fi Scanning and Results Normalization
*   **Scanning Execution**: When Lua calls `network:scan()`, the Supervisor fetches the primary Wi-Fi device path and invokes the `RequestScan(options: Dict)` method under interface `org.freedesktop.NetworkManager.Device.Wireless`.
*   **Asynchronous Gathering**: The Supervisor listens to properties changes on the device (monitoring `LastScanTime`) or listens to the D-Bus `AccessPointAdded` signal.
*   **Access Point Normalization**:
    1.  The Supervisor invokes `GetAllAccessPoints` on the wireless device.
    2.  For each AP object path, it reads properties on interface `org.freedesktop.NetworkManager.AccessPoint`:
        *   `Ssid`: Byte array, converted to a clean UTF-8 string.
        *   `Strength`: Unsigned byte (`0` to `100` percent).
        *   `WpaFlags` and `RsnFlags`: Examined to set `secure = true` if security flags are present, or `false` for open networks.
    3.  **Deduplication**: Merges access points sharing identical SSIDs, preserving the highest signal strength, and serializes them into a 20-entry dense array pushed to the Renderer as `network.available_networks`.

### 9.3 Open, Secure, and Hidden Network Association
*   **Connection profile settings**: When connecting to a target SSID, the Supervisor queries NetworkManager's `/org/freedesktop/NetworkManager/Settings` interface for existing profiles matching the SSID.
*   **Open Networks**: If no profile exists, the Supervisor invokes `AddAndActivateConnection2` on `org.freedesktop.NetworkManager.Settings`, passing a minimal configuration dictionary omitting the `802-11-wireless-security` settings block.
*   **Secure Networks**: If a password is supplied, the settings dictionary includes `802-11-wireless-security` with key-mgmt set to `"wpa-psk"`, and the PSK string set directly.
*   **Hidden Networks**: For hidden networks, the settings dictionary explicitly includes `hidden = true` (and `scan-ssid = true` inside the `802-11-wireless` parameter list) to force NetworkManager to broadcast active probes for the target SSID before attempting association.
*   **Lifecycle Monitoring**: The Supervisor tracks the activation progress. If association fails (e.g., incorrect credentials), it emits a `NetworkConnectionFailed` event. On success, it synchronizes the new active connection properties.

### 9.4 IPv4 Configuration Resolution
To update the Lua interface connection details, the Supervisor queries active network interfaces under `/org/freedesktop/NetworkManager/ActiveConnection/{id}`:
1.  Reads properties on interface `org.freedesktop.NetworkManager.Connection.Active` to fetch the `Ip4Config` path (e.g., `/org/freedesktop/NetworkManager/IP4Config/1`).
2.  Reads `Addresses` (IP, Subnet, Gateway) and `Nameservers` (DNS array) on `org.freedesktop.NetworkManager.IP4Config`.
3.  Pushes a populated association table `network.connection_details` containing the IP address, subnet mask prefix, default gateway, DNS server IPs, and physical interface name.

---


## 10. Bluetooth and Codec Controller (`oblisk.bluetooth`)

The Supervisor interfaces with BlueZ (`org.bluez` on System D-Bus) and PipeWire (`libpipewire`/WirePlumber) to coordinate device scanning, pairing, battery telemetry, and audio codec selection.

### 10.1 Global State and Discovery Loop
*   **Adapter State Control**: Sets the property `Powered` (boolean) on interface `org.bluez.Adapter1` at `/org/bluez/hci0` to enable or disable Bluetooth globally.
*   **Device Discovery**: Invokes `StartDiscovery` and `StopDiscovery` on `org.bluez.Adapter1`. Listens to D-Bus ObjectManager (`org.freedesktop.DBus.ObjectManager`) signals for `InterfacesAdded` and `InterfacesRemoved` on interface `org.bluez.Device1` to dynamically construct and update `bluetooth.discovered_devices` in Lua.

### 10.2 Pairing, Connection and Telemetry
*   **Pairing**: Invokes `Pair` on `/org/bluez/hci0/dev_XX_XX_XX_XX_XX_XX` under interface `org.bluez.Device1`. It hosts a local system Agent to securely process pin codes or authorization prompts.
*   **Connection**: Invokes `Connect` or `Disconnect` on the device interface.
*   **Battery Telemetry**: Monitors `org.bluez.Battery1` interface properties on connected devices. On changes, the Supervisor extracts the `Percentage` property and maps it to the matching device's MAC address in the `bluetooth.connected_devices` table.

### 10.3 PipeWire / WirePlumber Bluetooth Audio Codec Control
Under modern Linux setups, audio compression codecs are negotiated via PipeWire's BlueZ SPA plugins. The Supervisor bridges BlueZ's connected audio nodes with PipeWire:
*   **Node Querying**: The Supervisor's background PipeWire thread queries active audio device nodes where property `device.api` matches `"bluez5"`.
*   **Active Codec Resolution**:
    *   Reads the node's active parameter properties (e.g. `bluez.codec` or `api.bluez5.codec`) to determine the current codec.
    *   Queries WirePlumber's route manager or queries the node's `EnumFormat` parameter values to fetch the list of supported codecs (e.g., `"ldac"`, `"aac"`, `"sbc"`, `"aptx_hd"`). This list is pushed to Lua as `available_codecs`.
*   **Codec Switching**: When Lua calls `bluetooth:set_audio_codec(mac, codec)`, the Supervisor writes a `SetParam` command containing parameter ID `SPA_PARAM_Route` directly to the PipeWire node, specifying the target profile codec. PipeWire instantly tear-down and re-negotiates the Bluetooth link, completing the switch with zero stutter or audio drops.

---


## 11. Event Sound and Low-Latency Audio Feedback Engine (`oblisk.audio`)

The Supervisor embeds a lightweight native audio mixer thread to handle event sounds with minimal latency, bypassing external command spawns (like `aplay` or `pw-play`) and avoiding filesystem IO overhead during triggers.

### 11.1 Native Low-Latency Audio Mixer
*   **Direct PipeWire Connection**: The Supervisor hosts an internal audio playback queue using `rodio` or a direct `pw_stream` connected to the default PipeWire output sink.
*   **In-Memory Sound Caching**: Common sound effect files (WAV or FLAC) are pre-loaded from standard sound themes (e.g., `/usr/share/sounds/freedesktop/stereo/`) and cached as raw PCM buffers in RAM. This reduces play trigger latency to less than 2ms.

### 11.2 Automated Supervisor-Side Event Triggers
To guarantee latency-free feedback even under heavy CPU loads, audio playbacks are linked *directly* to internal Supervisor state machine transitions inside the Rust binary, bypassing the Lua VM:
1.  **Notification Received Event**: A new notification entry is added to the notifications queue $\rightarrow$ triggers the `"notification-message"` sound.
2.  **Low Battery Event**: Battery percent falls below 15% and state transitions to discharging $\rightarrow$ triggers the `"battery-caution"` sound.
3.  **Secure Lock State Event**: Session idle threshold is breached and screen lock is summoned $\rightarrow$ triggers the `"lock-session"` sound.

Lua configurations can globally enable or disable these sounds using `audio:set_event_sounds_enabled(bool)` or trigger specific playbacks manually on-demand using `audio:play_sound(sound_name_or_path)`.

---

## 12. Durable Keyboard Layout Controller (`oblisk.keyboard`)

The Supervisor integrates directly with the compositor's control sockets (Hyprland IPC and Niri IPC) to monitor and alter physical keyboard layout groups with zero polling.

### 12.1 Discovery and State Mapping
*   **Startup Inquiry**: On startup, the Supervisor queries the active compositor for all configured layout engines (e.g., reading `hyprctl devices` or executing a Niri state dump). It populates a static list of layout names `keyboard.layouts` and synchronizes it with the Renderer.
*   **Compositor Event Hooking**:
    *   **Hyprland**: The Supervisor listens to the `activelayout>>{keyboard_name},{layout_name}` event broadcasted over the Hyprland `/tmp/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock` stream. It parses the payload, matches the user-friendly layout name, updates the reactive signals `keyboard.active_layout` and `keyboard.active_layout_index`, and pushes them down to Lua.
    *   **Niri**: The Supervisor monitors Niri's IPC stream, matching keyboard state events.

### 12.2 Layout Mutation Commands
When a layout change is triggered (via `keyboard:set_layout(index)` or `keyboard:next_layout()`):
*   **Zero-Block Execution**: Commands serialize to JSON and traverse the private control socket. The Supervisor executes the target compositor command using its double-fork disown pipeline.
*   **Compositor Bridges**:
    *   **Hyprland**: Executes `hyprctl switchxkblayout current {index}` (for explicit sets) or `hyprctl switchxkblayout current next` (for toggles).
    *   **Niri**: Executes `niri msg action switch-layout {index}` or `niri msg action switch-layout next`.
*   **Transaction Lock**: If a reload transaction is active, the layout switch is held in a FIFO queue until the new Renderer is declared authoritative, preventing layout switching race conditions during hot-reloads.


----

## 13. Power and Session Management Controller (`oblisk.power`)

The Supervisor manages system power transitions, session terminations, and DPMS screen standbys. It integrates directly with systemd-logind over D-Bus for standard system state transitions, and executes custom compositor-specific commands via disowned, detached subprocesses.

### 12.1 systemd-logind D-Bus Integration
For system-level power actions (shutdown, reboot, suspend), the Supervisor completely bypasses shell process execution. It interacts directly with the D-Bus system bus interface `org.freedesktop.login1.Manager` at path `/org/freedesktop/login1`:
*   **Shutdown**: Invokes the `PowerOff(interactive: boolean)` method on the interface.
*   **Reboot**: Invokes the `Reboot(interactive: boolean)` method on the interface.
*   **Suspend**: Invokes the `Suspend(interactive: boolean)` method on the interface.

This direct integration ensures proper evaluation of systemd inhibitor locks (e.g., preventing accidental suspension while writing file streams), cleanly unmounts filesystems, and manages multi-user desktop session teardowns gracefully without privilege escalation.

### 12.2 Compositor-Specific Commands and Session Detachment
Because commands for logging out and controlling display power management signals (DPMS) vary drastically between Wayland compositors (e.g. Hyprland, Sway, Niri), Lua registers custom commands at startup.
*   **Registration**: The configuration registers custom command lines (e.g., `hyprctl dispatch exit`, `swaymsg exit`, or `niri msg action quit`) over IPC via `power:configure(cfg)`. These are cached in the Supervisor's long-lived memory state.
*   **Double-Fork Execution**: When `logout` is executed, the Supervisor runs the registered shell command using a standard double-fork disown pattern. This detaches the executing command from the Renderer and Supervisor process groups. This is a critical safety guarantee: even if the compositor instantly closes the Wayland connection (which tears down the shell and kills the Renderer process), the logout command is already fully detached and executes to completion under `init` (PID 1).
*   **Display Power Management Standby (DPMS)**:
    *   **Standby Entry**: When Lua triggers `power:dpms(false)`, the Supervisor executes the registered `dpms_off_cmd` (e.g., `hyprctl dispatch dpms off`), marks `power.dpms_active` as `true`, and publishes the state update over IPC.
    *   **Automated Wakeup Handshake**: When physical input activity occurs on the seat, the Supervisor's Wayland `ext-idle-notifier-v1` thread detects a resume event. If `power.dpms_active` is `true`, the Supervisor automatically executes the cached `dpms_on_cmd` (e.g., `hyprctl dispatch dpms on`) *before* forwarding the resume trigger to the Lua VM. This ensures that monitor backlights resume instantly with sub-millisecond physical response latencies.

---

## 14. XDG Base Directory Storage & Atomic State Manager

To ensure filesystem safety, maintain compatibility with read-only root filesystems, and protect the user's SSD hardware from write-exhaustion, Oblisk implements a split storage model conforming to the XDG Base Directory Specification.

### 14.1 The Core Storage Directory Zones

The Supervisor and Renderer partition all system data, caches, and layouts across four distinct physical directory locations:

| Path | XDG Variable Baseline | Write Permission | Primary Technical Content |
| :--- | :--- | :--- | :--- |
| `~/.config/oblisk/` | `$XDG_CONFIG_HOME` | **Read-Only** (to engine) | User layout files (`shell.lua`, `theme.lua`). Never written by Oblisk. |
| `~/.local/state/oblisk/`| `$XDG_STATE_HOME` | **Read-Write** | Persistent user choices (`state.json`), including wallpaper selections and dark mode switches. |
| `~/.cache/oblisk/` | `$XDG_CACHE_HOME` | **Read-Write** | Recreatable caches (`currency.json`, `weather.json`, MPRIS album-art covers). |
| `/dev/shm/oblisk-$UID/`| Shared Memory Disk | **Read-Write** (RAM Only)| Decoded raw PNG structures (tray icons, notifications) for fast streaming to GLES3. |

### 14.2 Atomic Safe Writes for Persistent User Choices (`state.json`)

To prevent persistent state file corruption during abrupt system crashes, power losses, or core reloads, the Supervisor manages writes to `$XDG_STATE_HOME/oblisk/state.json` using a double-buffered filesystem handshake:

1. **Memory Synchronization**: When Lua triggers `system:write_state(key, val)`, the Renderer issues a serial command to the Supervisor. The Supervisor updates its in-memory state representation.
2. **Double-Buffered Temp Write**: The Supervisor creates a transient temporary file in the same directory: `$XDG_STATE_HOME/oblisk/state.json.tmp`.
3. **Flushing**: It serializes the updated state dictionary into raw JSON, writes it to the temporary file, and executes a filesystem sync (`std::fs::File::sync_all`) to force physical disk commits.
4. **Atomic Swap**: The Supervisor calls a native atomic rename function (`std::fs::rename`). The operating system swaps the temporary file over the old `state.json` in a single transaction, ensuring that a crash never leaves the state file half-written or corrupted.
5. **State Broadcast**: The Supervisor broadcasts the updated state over the control socket as a reactive signal snapshot to the active Lua VM.

----

## 14. Multi-Display Workspaces Adapter (`oblisk.workspaces`)

The Supervisor hosts a dedicated off-thread IPC client matching the system's active Wayland compositor. It processes state transitions asynchronously to insulate the Lua Renderer from socket blockage.

### 14.1 Compositor Driver Subscriptions
*   **Hyprland**: Spawns a non-blocking Unix socket client binding to `/tmp/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock`. It listens for broadcast events:
    *   `workspace>>{name}`: Emitted when layout focus shifts.
    *   `focusedmon>>{mon_name},{ws_id}`: Active workspace changed on output.
    *   `openwindow>>` / `closewindow>>`: Re-triggers workspace state evaluation to update the `is_empty` flag.
    *   `createworkspace>>` / `destroyworkspace>>`: Dynamic workspace creations.
*   **Niri**: Binds to Niri's JSON-RPC control socket, issuing `subscribe` events and processing stream deltas.

### 14.2 The 3-Tier Multi-Display Workspace Model
To prevent visual layout bugs across multi-monitor configurations, the Supervisor resolves and exposes three distinct boolean flags for every workspace:
1.  **`is_visible`**: Is the workspace currently rendering on *any* physical monitor output? Visible workspaces are drawn in active background layouts.
2.  **`is_active`**: Is the workspace currently selected as the active display slot on its assigned monitor? Both monitors hold an active workspace, even if only one has focus.
3.  **`is_focused`**: Is this the active workspace on the physical monitor that currently holds the system's keyboard/pointer focus seat?
4.  **`is_special`**: Explicitly flags scratchpads (e.g., Hyprland's special workspaces). This allows the configuration to exclude scratchpads from the standard system bar layout and display them in a dedicated drawer or layout overlay.


---


## 15. Dynamic Monitor Hotplugging and Display Routing (`wl_output`)

To maintain stability across display state transitions (monitor connect/disconnect, screen rotation, DPI scaling adjustments), Oblisk integrates native, event-driven Wayland output management.

### 15.1 Wayland Protocol Bindings
*   **Interfaces**: The Renderer process binds the core `wl_output` interface and the Wayland-protocols extension `zxdg_output_v1`.
*   **Atomic Event Loop**: The Supervisor tracks outputs using physical connector EDIDs (e.g., `"HDMI-A-1"`, `"eDP-1"`). When a compositor event triggers:
    1.  `wl_output::geometry`: Resolves physical position coordinates, screen orientation, and subpixel layouts.
    2.  `wl_output::mode`: Resolves resolution refresh rates (V-Sync coordinates) and scale multipliers.
    3.  `zxdg_output_v1::logical_size`: Updates logical screen boundaries.
    4.  The Supervisor builds a reactive table `workspaces.outputs` matching the active outputs and serializes it over IPC.

### 15.2 The Handoff and Surface Teardown Contract
*   **Graceful Teardown**: When an output is disconnected (emitting `wl_registry::global_remove`), the Renderer intercepts the removal. It tears down the corresponding `zwlr_layer_surface_v1` container and frees associated GLES3 framebuffers in under 5ms, avoiding Wayland protocol validation crashes.
*   **Dynamic Re-Spawning**: Upon a connection event, the Renderer invokes the Lua root configuration (`shell.lua`). Lua's declarative layout engine evaluates the updated outputs signal, dynamically spawning a status bar container mapped to the new display grid.

---


## 16. Visual Error Boundaries and Rescue Mode UI

If the user's Lua configuration contains syntax errors, runtime exceptions (e.g. indexing `nil`), or gets caught in an infinite loop, the shell must never crash or display a blank screen.

### 16.1 Lua VM Execution Guards and Sandboxing
*   **Infinite Loop Interception**: The Renderer sets a hard instruction execution threshold using the Lua C-API `lua_sethook`. If a single layout transition executes more than **1,000,000 instructions**, the hook throws a memory execution panic, halting the infinite loop.
*   **Error Catching Boundaries**: Every execution of user layout files is wrapped in a pcall (`pcall` or C-equivalent `lua_pcall`). If an error is caught, the Renderer drops the configuration reload, preserves the last-known-good generation active, and flags `rescue.is_rescue` as `true`.

### 16.2 GLES2 Fallback Panel (Rescue Mode)
*   **Bypassing User Layouts**: If the initial config fails validation during cold start, the Renderer bypasses the user's Lua script completely.
*   **Internal Compiled Panel**: It initializes a minimalist, hardcoded fallback visual panel using FemtoVG compiled directly into the Rust binary.
*   **Stack-Trace Visualizer**: This panel reads the Supervisor's error log and renders the exact Lua syntax error or backtrace directly on the GPU.
*   **Interactive Recovery**: Features an interactive "Reload Config" button that watches your workspace directory. When you fix the syntax bug and click the button, the Supervisor validates the changes and reloads the Renderer, instantly exiting Rescue Mode.

---


## 17. Font, SVG, and Icon Asset Cache Management

To maintain a fluid 120Hz interface rendering cycle, the Renderer separates visual ticks from blocking filesystem reads.

### 17.1 Size-Bounded Least-Recently-Used (LRU) Caching
*   **Asset Buffers**: The Rust Renderer maintains direct size-bounded memory tables for parsed vector graphics (SVGs) and shaped text blocks:
    *   **SVG Texture Cache**: Capped at 128 items. Decoded vector shapes are scaled and rendered as static GLES3 texture maps on-demand.
    *   **Glyph Atlas Cache**: Glyphs are packed into a 2048x2048 physical GPU texture atlas managed by `cosmic-text`.
*   **Purging policy**: When the table caps are breached, the least-recently-used assets are evicted, preventing RAM accumulation.

### 17.2 Off-Thread Text Pre-Shaping
*   **Asynchronous Text Shaping**: When dynamic text (such as clock epochs or active media tracks) is updated, the layout metrics are passed to an off-thread worker.
*   **The Handshake**: The worker calculates font-families, font fallback trees, and shapes characters into static glyph clusters asynchronously. The results are queued and passed to the primary thread before the next swapchain tick, keeping frame render bounds below 1.5ms.

---


## 18. Single Shell Entry-Point and Functional Module Scoping

To avoid the namespace pollution typical of interpreted engines, Oblisk enforces strict lexical encapsulation and a single entry point file configuration.

### 18.1 The Entry Gating Contract
*   **Root Gating**: On boot or reload, the Renderer's Lua VM is hardcoded to evaluate exactly one file: **`~/.config/oblisk/shell.lua`**.
*   **Return Contract**: The root file must return a single declarative visual primitive node (e.g. `panel` or `row`) or a flat associative table mapping monitor EDIDs to visual containers.

### 18.2 Reusable Subcomponents (The Functional Module Contract)
To write modular custom widgets without registering them globally (which pollutes memory and compromises reload safety):
1.  **Lexical Return**: Custom widgets are written as isolated files returning a standard Lua builder function.
2.  **Scoped Imports**: Subcomponents are imported inside `shell.lua` using lexical `require` statements.
3.  **Encapsulation**: Properties are passed into the builder functions as localized parameter tables, keeping components isolated and secure.
