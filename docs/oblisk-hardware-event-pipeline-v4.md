# Oblisk Hardware Event Pipeline (v5)
## Zero-Polling Reactive Hardware Adapters and FFT Visualization (v5)

This specification defines the low-level contracts for hardware state tracking, event processing, and audio visualization inside the Oblisk framework. To prevent high-frequency polling patterns and heavy script executions (common in systems like Quickshell), all metrics are natively monitored in Rust using kernel-level event APIs and lazily mapped to Lua as immutable reactive signals.

---

## 1. Lazy Capability Activation Protocol

To maintain a zero-idle resource footprint and uphold our zero-opinion pure framework boundaries, no background threads, udev monitors, PipeWire connections, or D-Bus system bus watchers are spawned on boot. They exist in a **Dormant** state until explicitly requested by the compiled Lua AST during the generation transition.

This guarantees that unused features do not run inotify watches, claim system D-Bus endpoints, or consume memory, keeping the shell completely silent, transparent, and non-intrusive to external applications and standard system daemons.

### 1.1 Activation Handshake Flow
1. **Compilation Phase**: The Candidate Renderer compiles `shell.lua` and traverses the visual node tree.
2. **Import Detection**: When the Lua script triggers `require("oblisk.audio")` or references a hardware signal (e.g., `brightness.percent`), the Renderer registers a dependency handle.
3. **Registration Frame**: Before the health window closes, the Renderer writes a `RegisterCapability` control packet over the private Unix socket to the Supervisor:
   ```json
   {"generation_id": 4, "capability": "audio"}
   ```
4. **On-Demand Thread Allocation**: The Supervisor's Capability Authority validates the request against the current generation epoch and spawns the dedicated native OS thread for that system interface.

```text
[Lua Engine require()] ──▶ [Renderer IPC Engine] ──(Unix Sock)──▶ [Supervisor Authority]
                                                                        │
                                                        (Spawns Thread) │ (Lazy Initializer)
                                                                        ▼
                                                             [Native Kernel Watcher]
```

---

## 2. Keyboard Lock Keys & Layouts (Modifiers & Switches)

To avoid running continuous background scripts, running raw layout switches, or checking `/sys` in high-frequency loops, Oblisk tracks keyboard states and active layouts through unified, event-driven compositor hooks.

### 2.1 Direct-Focus Wayland Pipeline
When the active Renderer holds keyboard focus, modifier states and layout indexes are intercepted directly from the Wayland compositor seat.
* **The Protocol**: The Renderer monitors `wl_keyboard::modifiers` events.
* **The Parser**: The event provides four modifier bitmasks: `mods_depressed`, `mods_latched`, `mods_locked`, and a layout index integer `group`. The Rust backend passes these values to the active `xkb_state` object using `xkb_state_update_mask`.
* **State Modifier Mapping**: Modifiers are queried via `xkb_state_mod_name_is_active`:
  * `Caps Lock` ──▶ `xkb_state_mod_name_is_active(state, XKB_MOD_NAME_CAPS, XKB_STATE_MODS_LOCKED)`
  * `Num Lock` ──▶ `xkb_state_mod_name_is_active(state, XKB_MOD_NAME_NUM, XKB_STATE_MODS_LOCKED)`
  * `Scroll Lock` ──▶ `xkb_state_mod_name_is_active(state, "ScrollLock", XKB_STATE_MODS_LOCKED)`
* **Layout Index Mapping**: The `group` value directly yields the active layout index `keyboard.active_layout_index`.

### 2.2 Global System-Wide Pipeline (Indirect Focus)
When the shell is unfocused (e.g., a full-screen game is running), the Supervisor monitors keyboard lock and layout change events at the system level.
* **LED Modifiers**: The Supervisor registers an `inotify` watcher on `/sys/class/leds/`. It monitors write events (`IN_MODIFY`) on capslock, numlock, and scrolllock directories. On event, it reads exactly 1 byte (`1` = active, `0` = inactive).
* **Dynamic Layout Tracking (Compositor-IPC Adapters)**:
  1. **Hyprland**: The Supervisor connects to Hyprland's `/tmp/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket2.sock` on startup. It monitors the `activelayout>>{keyboard_name},{layout_name}` socket event, parses the string, and instantly synchronizes the layout state.
  2. **Niri**: The Supervisor monitors Niri's Unix domain socket stream, subscribing to IPC state events for keyboard layout modifications.
  3. **Standard Wayland Fallback**: Utilizes `ext-workspace-v1` or generic layout notifications if available.

### 2.3 Command Layout Switching Execution
When Lua triggers `keyboard:set_layout(index)` or `keyboard:next_layout()`:
* **Hyprland**: Supervisor maps this to the command execution `hyprctl switchxkblayout current {index/next}`.
* **Niri**: Maps to `niri msg action switch-layout {index/next}`.
* **Detached Command Execution**: Executed via double-forking on the Supervisor to eliminate UI loop hangs or blocked threads.

---

## 3. Webcamera Privacy Monitoring

To ensure privacy and security, Oblisk monitors active video capture devices without scraping `/proc` or executing `lsof`.

### 3.1 PipeWire Node Watcher (Preferred)
In modern Linux systems, active video streams are routed through PipeWire.
* **Registry Hook**: The Supervisor's background PipeWire thread subscribes to registry global events using the `pw_registry_add_listener` interface.
* **Node Filtering**: It filters for global objects with the interface type `PW_TYPE_INTERFACE_Node`.
* **State Evaluation**:
  1. For each node, it parses the `media.class` property. If the property matches `"Video/Source"`, it is identified as a camera feed.
  2. The thread monitors the node's state variable (`pw_node_info::state`). 
  3. When the state transitions to `PW_NODE_STATE_RUNNING`, the Supervisor increments an internal `active_count` counter (not exposed to Lua) and extracts the calling application name from the parent client node (`application.name` or `node.name`), appending it to `webcam.active_clients`.
  4. When the state transitions to `PW_NODE_STATE_IDLE` or `PW_NODE_STATE_SUSPENDED`, the counter is decremented; `webcam.active` reflects `active_count > 0`.

### 3.2 V4L2 Device Monitor (Fallback)
If PipeWire is unavailable, the Supervisor monitors `/dev/video*` character devices directly.
* **Kernel Events**: The Supervisor registers a `udev` monitor matching the `video4linux` subsystem.
* **File Descriptor Epoll**: 
  1. The Supervisor opens `/dev/video*` devices in non-blocking read-only mode (`O_RDONLY | O_NONBLOCK`).
  2. These file descriptors are added to an OS `epoll` thread loop.
  3. While no capture stream is active, the epoll thread remains completely suspended by the kernel.
  4. When an application opens a device node, the kernel triggers an `EPOLLERR` or `EPOLLPRI` flag. The Supervisor interprets this state transition as "Camera Active" and notifies Lua. It closes the temporary file descriptors immediately to prevent resource exhaustion.

---

## 4. Arch Linux System Updates Engine

To check for system updates without waking up the CPU or wasting network resources, the update engine uses a network-aware, file-triggered timer pipeline.

### 4.1 Pacman Sync Inotify Watcher
Instead of polling the package database at a high frequency, the Supervisor watches the pacman synchronization database folder directly.
* **The Watch Path**: `/var/lib/pacman/db/sync/` (or the custom database directory specified in `pacman.conf`).
* **The Trigger**: An `inotify` watcher is registered on this directory for `IN_CLOSE_WRITE` and `IN_MOVED_TO` events. This ensures that the update check runs only when a database sync operation (e.g., `pacman -Sy` or a system upgrade) successfully completes.

### 4.2 Network-Aware Update Executor
When triggered by an `inotify` sync event, or when the user-configured timer interval expires, the Supervisor executes a sub-process wrapper:
1. **Network Status Verification**: The Supervisor queries its internal D-Bus NetworkManager bridge.
2. **Interval Gating**:
   * **Online Mode**: If the network is active, the Supervisor executes the command `checkupdates` on your configured `online_interval` (default: 15 minutes / 900 seconds).
   * **Offline Mode**: If the network is down, the Supervisor skips the network call and instead checks only local packages against local databases on your `offline_interval` (default: 5 minutes / 300 seconds).
3. **Execution Safety**: The command is executed using `std::process::Command` under a low-priority scheduling class (`nice -n 19`) with a hard timeout of 10 seconds. The output stdout lines are parsed to extract the package list.
4. **State Push**: Updates `updates.count` (integer) and `updates.list` (array of strings) in the active Lua VM.

---

## 5. Input Overlay Pipeline

The input overlay capability tracks physical keyboard and pointer events globally across all desktop sessions.

### 5.1 Non-Unprivileged evdev Input Monitoring
To capture keystrokes globally without running as root (`sudo`), Oblisk utilizes Linux's native `evdev` device interface.
* **Permissions Contract**: The Supervisor spawns an unprivileged helper thread that opens `/dev/input/event*` nodes. To allow access, the user account running the shell must belong to the OS `input` group:
  ```bash
  # Enforced during installation or build checks
  sudo usermod -aG input $USER
  ```
* **Device Selection**: The Supervisor queries the `/dev/input/event*` devices, filtering by event capabilities (`EVIOCGBIT`) to identify keyboards (supporting `EV_KEY`) and pointers (supporting `EV_REL` or `EV_ABS`).

### 5.2 Epoll-Driven Input Processing
* **Event Dispatch Loop**:
  1. The selected input devices are registered inside an `epoll` listener.
  2. When a physical key is pressed, the kernel writes an `input_event` struct (defined in `<linux/input.h>`) to the event stream:
     ```rust
     struct input_event {
         time: timeval,
         type_: u16,  // EV_KEY
         code: u16,   // Keycode (e.g., 30 for KEY_A)
         value: i32,  // 1 = press, 2 = repeat, 0 = release
     }
     ```
  3. The Rust helper thread translates the raw keycode to its standardized string symbol using its active `xkbcommon` keymap context.
  4. The keys are pushed to a **thread-safe, bounded ring-buffer** (max capacity: 10 entries) and synced to the active Lua VM. High-frequency repeating modifiers (like holding down Shift or Ctrl) are debounced inside Rust before reaching the Lua VM to prevent UI performance degradation.

---

## 6. PipeWire Audio & Cava FFT Visualization Pipeline

To support real-time audio visualization without causing visual stutter in the scripting runtime, Oblisk processes raw PCM audio signals entirely within a Rust background thread before pushing simplified visual snapshots to Lua.

### 6.1 PipeWire Audio Monitor
* **The Connection**: The Supervisor connects to PipeWire and monitors active audio nodes using a native `pw_thread_loop` execution thread.
* **Volume tracking**:
  * Watches default audio sink volume changes via D-Bus WirePlumber interfaces or raw PipeWire port metadata.
  * Pushes `audio.volume` (0.0 to 1.0) and `audio.muted` (boolean) to Lua on-change.
  * Writes to volume sliders are processed via generation-guarded commands (e.g., `audio:set_volume(0.3)`), executing on the PipeWire loop thread in less than 1ms.

### 6.2 The Native FFT Engine (Cava Feature)
When `oblisk.cava` is active, the Supervisor spawns a high-performance audio capture stream:

```text
[PipeWire Loopback Device] ──▶ [PCM Buffer (44.1kHz)] ──▶ [Hanning Window]
                                                                │
[Throttled Lua State (60Hz)] ◀── [Log Binning & Smoothing] ◀── [FFT (1024 points)]
```

1. **PCM Capture**: The Supervisor opens a PipeWire stream configured to record raw PCM data from the default system monitor source (typically the loopback monitor of your active audio output device).
   * **Sample Rate**: Configured to match the system default (typically 44,100Hz or 48,000Hz, 16-bit Mono).
   * **Buffer Size**: Fixed at 1024 samples to balance frequency resolution and temporal latency.
2. **Windowing**: The Rust engine applies a standard **Hanning Window** to the raw PCM buffer to eliminate spectral leakage along frame boundaries:
   $$\omega(n) = 0.5 \left(1 - \cos\left(\frac{2\pi n}{N-1}\right)\right)$$
3. **FFT Processing**: The windowed buffer is processed using a fast, native Cooley-Tukey FFT algorithm (`rustfft` crate).
4. **Logarithmic Binning**: The raw FFT output (consisting of 512 frequency bins) is downsampled and grouped into a configurable number of visual bar bins (default: 20 bins) using a logarithmic scaling formula to match human auditory perception:
   $$f_{\text{boundary}}(i) = f_{\text{min}} \cdot \left(\frac{f_{\text{max}}}{f_{\text{min}}}\right)^{\frac{i}{N}}$$
5. **Temporal Smoothing & Scaling**: To prevent erratic visual spikes, the engine applies a temporal gravity-smoothing filter:
   $$y_t(i) = \max\left(y_{t-1}(i) - \text{gravity}, x_t(i)\right)$$
   Where $x_t(i)$ is the raw amplitude of bin $i$ normalized to a range between `0.0` and `1.0`.
6. **Throttled Lua Push**: The array of normalized float values is updated and pushed to the Lua VM. To avoid overloading the UI layout engine, updates are throttled to match your monitor's physical refresh rate (e.g., 60Hz or 120Hz), updating the visual nodes with minimal overhead.

---

## 7. Hardware State Files & Signals Specification

The following schema cross-references the signals sourced by this document's
kernel- and D-Bus-event pipelines with their Rust and Lua types. It is not
the full `oblisk.*` schema: `oblisk-idl-api-specs.md` §2 is the authoritative,
complete signal list (weather, launcher, mpris, idle, system, power,
notifications, and tray signals are defined there, not here).

### 7.1 Unified Signals Schema

| Signal Path | Rust Engine Type | Lua VM Type | Update Trigger |
| :--- | :--- | :--- | :--- |
| `brightness.percent` | `u8` | `integer` | udev event on `/sys/class/backlight` |
| `keyboard.caps_lock` | `bool` | `boolean` | wl_keyboard::modifiers / inotify on sysfs |
| `keyboard.active_layout` | `String` | `string` | active compositor IPC socket event / wl_keyboard |
| `keyboard.active_layout_index`| `u32` | `integer` | wl_keyboard::modifiers group / active compositor event |
| `keyboard.layouts` | `Vec<String>` | `table` (array) | compositor capability startup query |
| `keyboard.num_lock` | `bool` | `boolean` | wl_keyboard::modifiers / inotify on sysfs |
| `keyboard.scroll_lock` | `bool` | `boolean` | wl_keyboard::modifiers / inotify on sysfs |
| `battery.percent` | `u8` | `integer` | sysfs power_supply poll / udev event |
| `battery.charging` | `bool` | `boolean` | sysfs power_supply poll / udev event |
| `audio.volume` | `f32` | `number` | PipeWire / WirePlumber volume change |
| `audio.muted` | `bool` | `boolean` | PipeWire / WirePlumber mute toggle |
| `updates.count` | `u32` | `integer` | checkupdates execution / inotify sync |
| `updates.list` | `Vec<String>` | `table (array)` | checkupdates execution / inotify sync |
| `webcam.active` | `bool` | `boolean` | PipeWire Video/Source state / udev event |
| `webcam.active_clients`| `Vec<String>` | `table (array)` | PipeWire Node client metadata |
| `input_overlay.keys` | `Vec<String>` | `table (array)` | evdev physical keypress event |
| `cava.bars` | `Vec<f32>` | `table (array)` | FFT thread output (60Hz throttled push) |
| `workspaces.outputs` | `Vec<Output>` | `table (array)` | Compositor IPC socket events |
| `rescue.is_rescue` | `bool` | `boolean` | Lua VM error panics or compilation crash |
| `rescue.error_log` | `String` | `string` | Backtrace parsed by Rust engine |

| `network.connected` | `bool` | `boolean` | D-Bus NetworkManager state change |
| `network.available_networks` | `Vec<AccessPoint>` | `table (array)` | D-Bus NetworkManager scan / `AccessPointAdded` |
| `bluetooth.enabled` | `bool` | `boolean` | D-Bus BlueZ adapter `Powered` state change |
| `bluetooth.discovered_devices` | `Vec<Device>` | `table (array)` | D-Bus BlueZ ObjectManager `InterfacesAdded`/`Removed` |
| `bluetooth.connected_devices` | `Vec<Device>` | `table (array)` | D-Bus BlueZ `Device1`/`Battery1` property changes |

---


## 8. Dynamic Display Output and Hotplug Pipeline

To ensure that display switches, hotplugs, and scaling changes complete with zero UI flicker, the Supervisor hooks into the low-level compositor seat monitor events.

### 8.1 Wayland Output Event Sequence
When an output state changes, the Wayland display manager triggers events on the `wl_output` interface:
1.  **`wl_output::geometry`**: Sends the active monitor's physical dimensions (width/height in mm), subpixel layout, transform (rotation), and make/model strings.
2.  **`wl_output::mode`**: Sends refresh rates and pixel densities.
3.  **`wl_output::scale`**: Sends the physical HiDPI scale factor (e.g. `2` for 200% scale).
4.  **`wl_output::done`**: Broadcasts the transaction end. The Supervisor calculates the logical scaling bounding box:
    $$\text{Width}_{\text{logical}} = \frac{\text{Width}_{\text{physical}}}{\text{Scale}}$$
    $$\text{Height}_{\text{logical}} = \frac{\text{Height}_{\text{physical}}}{\text{Scale}}$$
5.  **State Sync**: Serializes the coordinates to the Renderer. The GLES3 viewport boundaries are dynamically updated, and the layout engine triggers a full damage-tracking layout calculation pass on the active layer surface, re-anchoring the bar in under 1ms.
