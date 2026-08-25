# Oblisk Hardware Event Pipeline Specification
## Zero-Polling Event-Driven Hardware Adapters, Audio Streams, and Display Scaling

This specification defines the low-level event pipelines, protocol hooks, and kernel-level monitors managed inside the Oblisk Supervisor and Renderer processes. All hardware telemetry, network profiles, and audio stream routing run as true **zero-polling, event-driven monitors**, updating the Lua configuration instantly upon hardware or system state transitions.

---

## 1. Zero-Polling Kernel-Level Event Pipelines

Oblisk rejects high-latency CPU-polling loops. All hardware adapters bind to native event-driven APIs:

### 1.1 Sysfs Battery Monitor (Udev & Netlink)
*   Instead of periodic reads, the Supervisor registers a `udev` monitor or binds a netlink socket to kernel-level device events.
*   On battery state modifications (e.g., plugging a charger or a change in percentage), the kernel emits a netlink event.
*   The Supervisor captures this event, reads `/sys/class/power_supply/BAT0/capacity` and `/sys/class/power_supply/BAT0/status` exactly once, and broadcasts the updated state over IPC to the Renderer.

### 1.2 Backlight Brightness Monitor (Inotify)
*   The Supervisor registers an `inotify` watch on the backlight actual brightness file: `/sys/class/backlight/*/actual_brightness`.
*   Whenever the screen brightness changes (either via hardware buttons or software calls), the kernel triggers a write event. The Supervisor captures this instantly, converts the raw value to a logical percentage `[0, 100]`, and pushes it to Lua.

### 1.3 NetworkManager Event Broker (D-Bus Subscriptions)
*   The Supervisor subscribes to `org.freedesktop.NetworkManager` properties changed signals on the session bus.
*   SSID, signal strength, and connectivity states are updated instantly upon kernel-level link handshakes.
*   **Scanning States**: The Supervisor monitors scanning changes by binding to the wireless device's properties. `network.scanning` is updated reactively, avoiding any timed loop overheads.

### 1.4 Bluetooth ObjectManager Broker (BlueZ Subscriptions)
*   The Supervisor subscribes to the D-Bus `org.freedesktop.DBus.ObjectManager` interface under path `/` for BlueZ.
*   It intercepts `InterfacesAdded` and `InterfacesRemoved` on the `org.bluez.Device1` interface.
*   Discovered and connected device lists, MAC addresses, and names update instantly as devices associate or drop off without any periodic bluetoothctl polling.

---

## 2. PipeWire Native Audio Stream Event Loop

The Audio controller is strictly **PipeWire-only**, binding directly to the PipeWire API (`libpipewire` or native socket dispatching) inside a dedicated thread.

### 2.1 Event-Driven Mixer Updates
*   The Supervisor registers callbacks for the core Registry (`pw_registry`).
*   **Default Routes**: On active output (sink) or input (source) modifications (e.g., plugging headphones or a microphone), PipeWire emits default node updates. The Supervisor catches this, updates the `audio.sinks` and `audio.sources` lists, and highlights the active route.
*   **Volume & Mute**: Node volume changes from external sliders (like `pavucontrol` or keyboard keybinds) trigger properties changes. The Supervisor extracts the new dB volume levels, maps them to a linear float `[0.0, 1.0]`, and pushes the updates to the active `audio.volume` signal.
*   **Per-App stream tracking**: Playback nodes are monitored. When applications open or close audio streams, the PipeWire thread updates `audio.apps` in real-time.

---

## 3. MPRIS Zero-Polling Seek & Live Progress Sync

Track progress seekbars inside status bars are notorious for generating massive CPU usage if updated over IPC loops. Oblisk solves this mathematically, maintaining high-frequency smoothness with zero IPC overhead.

### 3.1 Continuous Interpolation Math
*   When a player begins playing, the Supervisor caches the track progress: `position` (microseconds), `position_updated_at` (steady monotonic clock timestamp in microseconds), and `play_state` (`"Playing"`).
*   The Renderer's Lua VM receives these signals once. Inside Lua, the current track position is computed dynamically on every screen draw tick using the formula:
    $$	ext{CurrentPosition} = 	ext{position} + (	ext{Instant::now()} - 	ext{position_updated_at})$$
    (This is only computed if `play_state == "Playing"`). This results in fluid, sub-millisecond progress updates on your bar without sending a single IPC message over the socket!
*   **External Seek Event Tracking**: If an external application triggers a seek action (e.g. clicking the track bar inside Spotify), the player emits a `Seeked(position)` signal. The Supervisor captures this D-Bus signal instantly, updates the cached position and monotonic timestamp, and pushes the updated signals down to Lua.

---

## 4. Wayland Displays, Output Scaling, and Monitor Hotplugging

The Renderer process handles all display geometries on its primary thread:

### 4.1 wl_output Event Pipeline
The Renderer binds the core `wl_output` and the `zxdg_output_v1` interfaces. When monitor hotplugs occur:
1.  `wl_output::geometry`: Logical position, orientation, and subpixel layouts.
2.  `wl_output::mode`: Physical width, height, and refresh rates.
3.  `zxdg_output_v1::logical_size`: Logical screen boundaries.
4.  The Renderer updates the `workspaces.outputs` layout matrix and triggers a layout resolution tick, instantly adjusting the surface geometry.

### 4.2 Fractional Scaling & Subpixel Snapping
To prevent blurry lines and text rendering on HiDPI displays under fractional scale factors (e.g., $1.25$, $1.5$):
*   All layouts are solved in standard floating-point logical pixels.
*   Before pushing damage rectangles to the compositor, coordinates are multiplied by the scale factor $S_f$ and rounded to physical integer pixels using ceiling offsets:
    $$X_{phys} = 	ext{round}(X_{logical} 	imes S_f)$$
    $$Y_{phys} = 	ext{round}(Y_{logical} 	imes S_f)$$
    $$W_{phys} = \lceil (X_{logical} + W_{logical}) 	imes S_f \rceil - X_{phys}$$
    $$H_{phys} = \lceil (Y_{logical} + H_{logical}) 	imes S_f \rceil - Y_{phys}$$
*   This prevents visual subpixel cracks or overlapping borders between adjacent elements.

---

## 5. Keyboard Lock Status Interception

*   The Renderer registers modifier changes directly through the active compositor's Wayland socket (e.g. Hyprland `.socket2.sock` or Niri's JSON-RPC event stream).
*   It updates the `keyboard.caps_lock` signal instantly when physical seat modifier modifications occur, bypassing any periodic terminal polls.

---

## 6. Compositor-Independent Active Window Event Streams

Rather than using compositor-specific event channels, the Renderer's primary event loop tracks windows natively:

### 6.1 Foreign Toplevel Manager Protocol Bindings
*   The Renderer binds to standard compositor-agnostic interfaces `ext-foreign-toplevel-list-v1` or `zwlr_foreign_toplevel_manager_v1`.
*   Whenever focused states, class modifications, or title changes occur at the compositor level, the protocol broadcasts events.
*   The Renderer catches these events on its main thread, serializes window properties, and updates the `workspaces.active_client` state dictionary dynamically without any delay or terminal polls.

---

## 7. Dynamic Telemetry Scheduler Ticks (`sysinfo`)

To minimize power drain, `sysinfo` sensor threads avoid raw, periodic poll streams:

### 7.1 Dynamic Task Scheduling
*   The Supervisor manages three asynchronous timer tasks (`tokio::time::interval`) for CPU, RAM, and hardware temperature metrics.
*   Upon receipt of `sysinfo:configure()`, the timer parameters are dynamically adjusted.
*   If an interval parameter is updated to `0`, the matching scheduler task is fully suspended and its thread execution loop remains completely dormant in memory, achieving 0% idle CPU overhead.
