# Supervisor services

What each Supervisor capability talks to, how long its state lives, and how Renderer and
Supervisor talk. Config-side calls: [capabilities](lua-api/capabilities.md),
[scripting](lua-api/scripting.md). Fields and actions: [`lua-meta/mantle.lua`](../lua-meta/mantle.lua).
Rationale: [decisions](decisions.md). Open work: [roadmap](roadmap.md).

## Capability map

| Capability | Backend | Section |
| :--- | :--- | :--- |
| `notifications` | Hosts `org.freedesktop.Notifications` (session bus) | [1](#1-notifications) |
| `tray` | Hosts `org.kde.StatusNotifierWatcher`; SNI items, DBusMenu (session bus) | [2](#2-system-tray) |
| `mpris` | `org.mpris.MediaPlayer2.*` (session bus) | [3](#3-mpris) |
| `network` | NetworkManager (system bus) | [4](#4-networkmanager) |
| `bluetooth` | BlueZ `org.bluez` and an `Agent1` (system bus) | [5](#5-bluez) |
| `audio` | PipeWire native API | [6](#6-pipewire-and-privacy) |
| `privacy` | `/proc/*/fd` scan for `/dev/video*`, PipeWire streams | [6](#6-pipewire-and-privacy) |
| `idle` | `ext_idle_notifier_v1`, logind inhibit, hosts `org.freedesktop.ScreenSaver` (session bus) | [7](#7-idle-lock-and-polkit) |
| `lock` | logind, PAM worker; the Renderer holds `ext_session_lock_v1` | [7](#7-idle-lock-and-polkit) |
| `polkit` | `org.freedesktop.PolicyKit1` authentication agent (system bus), polkit's agent helper | [7](#7-idle-lock-and-polkit) |
| `workspaces` | niri IPC or Hyprland sockets | [8](#8-workspaces-and-windows) |
| `windows` | niri/Hyprland events, else `zwlr_foreign_toplevel_management_v1` | [8](#8-workspaces-and-windows) |
| `sysinfo` | `/proc/stat`, `/proc/meminfo`, `/sys/class/hwmon/` | [9](#9-telemetry-and-clock) |
| `system` | Supervisor clock | [9](#9-telemetry-and-clock) |
| `processes` | Supervisor-owned child processes | [10](#10-processes) |
| `battery`, `power`, `brightness`, `keyboard`, `applications`, `updates`, `files` | UPower, power-profiles-daemon, sysfs, evdev, XDG dirs, libalpm, inotify | [11](#11-other-capabilities) |
| `storage` | JSON files the config declares | [12](#12-paths-and-persistence) |

`mantle.screens`, `rescue`, `version` and `config_dir` are Renderer members, not capabilities.

## Lifetimes

| Rule | Detail |
| :--- | :--- |
| Start | A `StartCapability` frame starts a controller when a generation first reads `mantle.<name>` or a `secure_submit` names it (ADR-0070); repeats are no-ops. It then runs for the Supervisor's lifetime |
| Boot-built | `lock` (ADR-0060) and the `polkit` controller; the polkit agent registers on its first start (ADR-0114) |
| Shared readers | `audio` and `privacy` share one PipeWire thread; `workspaces` and `windows` share one niri/Hyprland reader. Whichever is read first starts it |
| Buses | One shared system-bus connection. `tray`, `notifications`, `mpris` and `idle` each open their own session bus. Every method call times out after 25 s |
| State | Supervisor-global, not per generation. It survives reloads and Renderer respawns; a new generation is replayed every last snapshot |
| Departed generation | Its Bluetooth discovery stops, a pending Wi-Fi prompt is cancelled, and its `files` watches, idle thresholds and inhibits are dropped. Its `process.run` children are reaped ([§10](#10-processes)) |
| Updates | Event-driven where the backend signals; `system`, `sysinfo`, `updates` and the brightness fallback run timers |
| Missing backend | Logged; the capability goes inert or stays `nil` (per section). Only `network` retries: a failed NetworkManager build is rebuilt on the next start, which a Renderer sends once per generation |

Every capability reads `nil` in Lua until its first snapshot arrives
([capabilities](lua-api/capabilities.md)). A controller that never starts, or never pushes, stays
`nil` for good.

## Requirements

What must be installed or running for each capability, and what the config sees without it. The
log lines quoted are the ones to search for in `mantle log`.

| Capability | Needs | Without it |
| :--- | :--- | :--- |
| `notifications` | A session bus; no other notification daemon holding the name | Another owner: warns `another notification daemon already owns this name`, the D-Bus server is off for the run, `feed` stays empty. No session bus: inert |
| `tray` | A session bus; apps that speak StatusNotifierItem (XEmbed-only icons never appear) | Another watcher owns the name: Mantle queues and gets no items until it leaves. No session bus: inert, `items` empty |
| `mpris` | A session bus; players exporting `org.mpris.MediaPlayer2.*` | `players` empty |
| `network` | NetworkManager on the system bus | Logs `NetworkManager is unreachable` at debug (`-vv`); stays `nil` until a new generation starts it again |
| `bluetooth` | `bluetoothd` running before the Supervisor starts, and an adapter | Inert: `enabled` and `discovering` `false`, lists empty, writes no-op. A later `bluetoothd` needs a shell restart |
| `audio` | PipeWire (`pipewire-pulse` for the notification-role filter on Pulse clients) | Logs `pipewire registry listener stopped`; `audio` stays `nil`, `privacy` loses microphone and screencast. No reconnect |
| `privacy` | Readable `/proc/*/fd` of the user's processes; `/dev/videoN` nodes present at start | Camera use by another user's process, or a camera plugged in later, is not seen |
| `idle` | `ext_idle_notifier_v1` from the compositor; logind; a session bus for ScreenSaver | No protocol or setup over 5 s: thresholds never fire (logged once). No session bus: ScreenSaver holds unserved |
| `lock` | `ext_session_lock_v1`; logind session; PAM stack `mantle` (ships as `packaging/pam.d/mantle`) | No lock protocol: the Renderer reports `Refused`. No `mantle` stack: authenticates against `login` |
| `polkit` | polkitd on the system bus, `$XDG_SESSION_ID`, the socket-activated helper at `/run/polkit/agent-helper.socket`, and no other agent registered | No session id or another agent: logged, agent off for the run; prompts go to the other agent or nowhere |
| `workspaces`, `windows` | niri (`$NIRI_SOCKET`) or Hyprland (`$HYPRLAND_INSTANCE_SIGNATURE`); else `zwlr_foreign_toplevel_management_v1` for `windows` | `workspaces` `nil`; `windows` `nil` without the wlr protocol |
| `sysinfo` | `/proc`; hwmon chips `k10temp` or `coretemp` (CPU) and `amdgpu`, `nouveau` or `nvidia` (GPU) | Missing chip: that temperature is absent |
| `system` | Nothing | Always ticks |
| `battery` | UPower | No UPower or no battery: `present = false`, `state` `"Unknown"` without UPower |
| `power` | UPower and/or power-profiles-daemon | Missing half: its fields absent; `set_profile` without the daemon is ignored |
| `brightness` | A `/sys/class/backlight` device; logind; an active session for writes | No device: `nil` for good. Inactive session: `SetBrightness` fails, logged |
| `keyboard` | read access to a `/dev/input` keyboard with LEDs; `*::kbd_backlight` for backlight; niri or Hyprland for layout | No evdev: lock state read once from sysfs, else `false`. No backlight LED: `-1`. Other compositor: layout empty, count `0` |
| `applications` | `.desktop` files; `$TERMINAL` for `Terminal=true`; `xdg-open` for `open_url` | Terminal entries refuse to launch without `$TERMINAL` |
| `updates` | `pacman` on `PATH`; `pkexec`, answered by the `polkit` agent; paru or yay for AUR | No pacman: `package_manager` `nil`, every action refused. `packaging/polkit-1/rules.d/50-mantle-pacman.rules` keeps one approval per run for wheel users |
| `files`, `storage` | Absolute paths from the config | Relative paths refused |
| `processes` | The programs the config names on `PATH` | A failed `session_process` start is logged and set as its `start_error` |

## 1. Notifications

| Contract | Behavior |
| :--- | :--- |
| Name | Requested with `DoNotQueue`; if another daemon owns it, the D-Bus server stays off for the run |
| Retention | 100-entry FIFO; `feed` shows the newest 20. Eviction emits `NotificationClosed` reason 4 |
| Text bounds | App name 64 bytes, summary 128, body 512, 8 actions with 64-byte labels; UTF-8-safe truncation |
| Body markup | Allowlist: `<b>`, `<i>`, `<u>`, `<a href>`, `<img src>`. Other tags are stripped and their text kept; `script`/`style` content is dropped |
| Images | `image-path`, action icons and `<img>` must be regular files under `/usr/share/icons`, `/usr/share/pixmaps`, `$XDG_DATA_HOME/icons` or `~/.icons`. Raw `image-data` (8-bit, ≤ 128 px) is validated and spooled as `notifications/notif-<id>.png` |
| Expiry | A negative timeout means 5 s. A retained entry is marked expired, a `transient` one removed. Critical and `0` never expire. `hold_expiry` pauses countdowns, capped at 300 s |
| Removal | `invoke_action` and `reply` remove the entry unless `resident`; `dismiss` always does. Close reasons: 1 expired, 2 `dismiss` or `reply`, 3 `CloseNotification` or `invoke_action`, 4 evicted |
| Reply | Emits `NotificationReplied(id, text)` (ADR-0033) |
| DND, quiet | Gate sound only; critical plays through both |
| Sound | Per-urgency Ogg Vorbis or 16-bit WAV (≤ 4 MiB, ≤ 30 s). `suppress-sound` or a muted app (by app name or desktop entry) silences, critical included; else the client's `sound-file`; else `sound-name` from the freedesktop theme, only where a tier sound is registered; else the tier sound. Roots: `/usr/share`, `/usr/local/share`, `/opt`, `$XDG_DATA_HOME` |

Code: [types and limits](../supervisor/src/capabilities/notifications/mod.rs),
[controller](../supervisor/src/capabilities/notifications/controller.rs),
[markup](../supervisor/src/capabilities/notifications/markup.rs),
[sound](../supervisor/src/capabilities/notifications/sound.rs).

## 2. System tray

Hosts the watcher at `/StatusNotifierWatcher` and registers itself as a host. The name is requested
without `DoNotQueue`: if another host owns it, Mantle queues for it (ADR-0031). At start it adopts
items already on the bus at `/StatusNotifierItem`, `/StatusNotifierItem/1` and
`/org/chromium/StatusNotifierItem/1` (ADR-0073). Items are dropped, and their spooled PNGs deleted,
on `NameOwnerChanged`.

| Contract | Behavior |
| :--- | :--- |
| Icon | `IconName` found in the item's `IconThemePath`, then `IconName` as a theme name, then the largest valid pixmap |
| Pixmap | Square, 1–128 px, exactly `w × h × 4` ARGB bytes; spooled as PNG under `tray/` |
| Bounds | Strings 256 bytes; menus 1024 nodes, depth 32 |
| Activation | `ItemIsMenu` items get no `Activate`; secondary activate and scroll are unrestricted |
| Menus | `com.canonical.dbusmenu` layout; `menu_will_show` sends `AboutToShow`, `activate_menu_item` sends `Event("clicked")` |

Code: [tray](../supervisor/src/capabilities/tray/mod.rs), [controller](../supervisor/src/capabilities/tray/controller.rs),
[icons](../supervisor/src/capabilities/tray/icon.rs).

## 3. MPRIS

- Discovery: `ListNames` once, then `NameOwnerChanged` for `org.mpris.MediaPlayer2.*`, excluding
  `playerctld` (ADR-0036).
- `PlaybackStatus` and `Metadata` changes and `Seeked` refresh the cache; a status change also
  re-reads `Position` once 100 ms later. Position is sampled with a `CLOCK_MONOTONIC` timestamp for
  the config to extrapolate; nothing polls.
- Controls: play, pause, play/pause, next, previous. Absolute seek is `SetPosition` with the cached
  track id, clamped to the track length, else a relative `Seek` from the last known position.
  Relative seek is unclamped.

Code: [controller](../supervisor/src/capabilities/mpris/controller.rs),
[player](../supervisor/src/capabilities/mpris/player.rs).

## 4. NetworkManager

| Contract | Behavior |
| :--- | :--- |
| Events | Manager, device list, device state, wireless APs, active AP and saved-profile changes rebuild state (ADR-0082). A hotplugged device rescans the set |
| Devices | Only the first Wi-Fi device is tracked |
| Toggles | Networking via `Enable`; Wi-Fi via `WirelessEnabled`. Ethernet off disconnects wired devices; on activates each one's first autoconnect profile |
| Scan | `RequestScan`; duplicate SSIDs merge. Keeps the connected AP, then saved networks, then the strongest, capped at 20. Band from frequency |
| Connect | Saved profiles reactivate. A secured join takes its key through `secure_submit`, never Lua (ADR-0029). An aborted join deletes the profile it created. 45 s backstop on activation |
| Disconnect | `Device.Disconnect`; NetworkManager then skips autoconnect until the user joins again |

Code: [network](../supervisor/src/capabilities/network/mod.rs),
[connect](../supervisor/src/capabilities/network/connect.rs).

## 5. BlueZ

- State: `ObjectManager` plus property changes. An adapter added later is picked up; a
  `bluetoothd` started after the Supervisor is not. Without BlueZ the controller is inert (ADR-0030).
- Agent: the default `DisplayYesNo` agent at `/org/mantle/Bluez/Agent1`. A confirmation,
  authorization or displayed code becomes `pairing_request`, only while the adapter is visible or
  this shell is pairing that device. PIN and passkey entry are rejected.
- Discovery: starting clears the discovered list; stopping keeps it. BlueZ expires only devices
  still marked temporary, after `TemporaryTimeout` (30 s default).
- `Battery1` gives accessory charge; the `Class` major/minor bits give the category.
- Codecs come from PipeWire: `audio.bluetooth` lists each device's profiles and
  `set_bluetooth_profile` switches one ([§6](#6-pipewire-and-privacy)).

Code: [bluetooth](../supervisor/src/capabilities/bluetooth/mod.rs),
[agent](../supervisor/src/capabilities/bluetooth/agent.rs).

## 6. PipeWire and privacy

| Source | Feeds |
| :--- | :--- |
| Sinks, sources, default routing | `audio` devices, master and input volume/mute, balance |
| `Stream/Output/Audio`, `Stream/Input/Audio` with an `application.process.id` | `audio.apps` per-app volume/mute by node ID; excludes pid-less (portal) streams, notification sounds, peak meters and monitor captures |
| `Stream/Input/Audio`, minus monitor captures | `privacy` microphone users |
| `Stream/Output/Video` | `privacy` screencast users; wlr-screencopy clients are invisible |
| `/proc/*/fd` holders of `/dev/videoN`, rescanned on inotify open/close | `privacy` camera users; `Video/Source` nodes only enrich names (ADR-0034). Devices are enumerated once |

An unreachable PipeWire is logged and the thread exits; nothing reconnects.

Code: [audio](../supervisor/src/capabilities/audio/mod.rs),
[registry](../supervisor/src/capabilities/audio/mixer/registry.rs),
[streams](../supervisor/src/capabilities/audio/mixer/streams.rs),
[privacy](../supervisor/src/capabilities/privacy/mod.rs).

## 7. Idle, lock and Polkit

### Idle

| Contract | Behavior |
| :--- | :--- |
| Notifier | A dedicated Wayland connection to `ext_idle_notifier_v1`. Inert if the protocol is missing or setup exceeds 5 s |
| Listeners | Two per distinct duration, shared by every generation: `get_idle_notification` and its input-only twin; comparing them reveals a Wayland surface inhibitor (ADR-0160). Each generation is one fan-out entry per duration |
| Registrations | An in-place reload drops the generation's entries, then re-registers; a duration still asked for keeps its listener and timer. Cancelling the last callback at a duration destroys its listeners (ADR-0232). Inhibit holds survive the reload |
| Inhibit | `inhibit`/`release_inhibit` refcount one logind `Inhibit("idle", "block")` fd |
| ScreenSaver | Hosts `org.freedesktop.ScreenSaver` at `/org/freedesktop/ScreenSaver` and `/ScreenSaver`, requested with `DoNotQueue`; its clients share the same fd, and a client that leaves the bus loses its holds. A browser's video hold lands here, directly or via xdg-desktop-portal (ADR-0231) |
| Gate | While `BlockInhibited` names `idle`, threshold events stop and idled thresholds get `resumed`; on release, still-idle ones get `idled` again. Mantle is the idle daemon; pair it with `IdleAction=ignore` |
| State | `inhibited` covers logind, ScreenSaver and compositor surface holds. `inhibitors` lists every holder but this shell: block-mode logind holders, ScreenSaver clients, and the compositor's hold with an empty `who` |

### Lock

| Contract | Behavior |
| :--- | :--- |
| Ownership | The Supervisor decides lock and unlock; the Renderer holds and paints `ext_session_lock_v1` (ADR-0042) |
| Triggers | Config `lock`, and logind's `Lock` signal (`loginctl lock-session`). logind's `Unlock` is ignored (ADR-0138). Sets `LockedHint` |
| Unlock | Only a successful PAM conversation, in a re-exec'd worker using `/etc/pam.d/mantle` or `/usr/lib/pam.d/mantle`, else `login` (ADR-0241). A worker silent for 30 s fails the attempt |
| Unlock animation | `set_unlock_animation` delays the release, clamped to 600 ms; kept across reloads |
| Crash | A dead Renderer never unlocks; the replacement retakes the lock (ADR-0058). `$XDG_RUNTIME_DIR/mantle/session-locked` carries the fact across a Supervisor restart (ADR-0060) |
| Reload | An edit that would recreate a lock surface is refused while locked ([§14](#14-reload-lifecycle)) |

### Polkit

Registers as the authentication agent for `$XDG_SESSION_ID`'s session at
`/org/mantle/PolicyKit1/AuthenticationAgent`; if another agent already answers, it stays off for
the run. Challenge state and cancel belong to the Supervisor. The password goes from the native
secure field to polkit's root helper at `/run/polkit/agent-helper.socket`, never through Lua;
polkitd accepts the response only from uid 0 (ADR-0114).

Code: [idle](../supervisor/src/capabilities/idle/mod.rs),
[lock](../supervisor/src/capabilities/lock/mod.rs), [polkit state](../supervisor/src/capabilities/polkit.rs),
[polkit agent](../supervisor/src/polkit.rs), [PAM worker](../supervisor/src/pam_worker.rs).

## 8. Workspaces and windows

The compositor is probed once, from `$HYPRLAND_INSTANCE_SIGNATURE` then `$NIRI_SOCKET`. Anything
else leaves `workspaces` `nil`.

| Capability | niri | Hyprland | Other |
| :--- | :--- | :--- | :--- |
| `workspaces` | IPC event stream (ADR-0056); `overview_open` | Event (`.socket2.sock`) and command (`.socket.sock`) sockets (ADR-0118); special workspaces, `is_fullscreen` | `nil` |
| `windows` | Same event stream | Same sockets | `zwlr_foreign_toplevel_management_v1` on its own connection (5 s setup limit); outputs bound once |

Each workspace action opens a fresh compositor socket. A window action the backend lacks is
logged and dropped (ADR-0247).

Code: [workspaces](../supervisor/src/capabilities/workspaces/mod.rs),
[windows](../supervisor/src/capabilities/windows/mod.rs), [probe](../supervisor/src/compositor.rs).

## 9. Telemetry and clock

- `sysinfo`: CPU from `/proc/stat`, RAM and swap from `/proc/meminfo`, temperatures from
  `/sys/class/hwmon/` chips chosen by name preference (ADR-0035). Each sample has its own
  interval; all are `0` (off) until `configure`. The first reading lands one interval later (CPU: two).
- `system`: ticks once a second, aligned to the wall-clock second at start only; an NTP step or
  resume is not re-aligned. `time` is epoch seconds; `monotonic` counts seconds from the
  capability's start and excludes suspend (ADR-0202).

Code: [sysinfo](../supervisor/src/capabilities/sysinfo/mod.rs),
[system](../supervisor/src/capabilities/system/controller.rs).

## 10. Processes

| Kind | Spawn | Ends |
| :--- | :--- | :--- |
| `process.run` | Own process group, stdin `/dev/null`; stdout/stderr piped by line, 64 KiB per line (ADR-0026) | `kill()`, or the generation's departure or Supervisor shutdown: SIGTERM to the group, SIGKILL after 100 ms. In-place reloads keep it |
| `process.detach` | `setsid` and a double fork, stdio to `/dev/null`; init reaps it | Never signalled; outlives the shell |
| `session_process` | Own process group, stdio inherited (`shell.log`), held by `mantle.processes` (ADR-0175) | `stop()` or Supervisor shutdown: the declared signal to the group, SIGKILL after 5 s. Survives reloads and respawns |

One task per session process owns its `Child` and is the only place that signals it, so a
recycled pid is never hit. Application launches use the detached spawn.

Code: [registry](../supervisor/src/process/registry.rs), [spawn](../supervisor/src/process/mod.rs),
[session processes](../supervisor/src/capabilities/processes/controller.rs).

## 11. Other capabilities

| Capability | Backend | Notes |
| :--- | :--- | :--- |
| `battery` | UPower `DisplayDevice` | Read-only (ADR-0080). No UPower reads `present = false` |
| `power` | power-profiles-daemon (`org.freedesktop.UPower.PowerProfiles`, else `net.hadess.PowerProfiles`); UPower `OnBattery`, `EnergyRate` | Either half may be missing; its fields stay absent |
| `brightness` | sysfs backlight chosen once (firmware, then platform, then raw) via a udev watch (30 s poll fallback); writes via logind `Session.SetBrightness` | Reads the requested level, not `actual_brightness`. Stays `nil` without a backlight |
| `keyboard` | Lock LEDs from the first evdev device with `LED_CAPSL` (sysfs read once as fallback); `*::kbd_backlight` via logind; layout via the compositor | Layout is empty without niri or Hyprland (ADR-0034) |
| `applications` | `.desktop` files under `$XDG_DATA_HOME` and `$XDG_DATA_DIRS` `applications/`, first wins (ADR-0061) | Not watched; `refresh` rescans. `launch` spawns detached (`Terminal=true` needs `$TERMINAL`); `open_url` hands `http`, `https` and `mailto` URLs (≤ 2048 bytes) to `xdg-open` |
| `updates` | libalpm sync into a user-owned db; AUR RPC for foreign packages when `aur = true` (ADR-0250) | Installs with `pkexec pacman -Syu --noconfirm`, or paru/yay with `--sudo pkexec`; progress parsed from output. `/run/mantle-reboot-required` drives `reboot_required` (ADR-0134) |
| `files` | inotify per watched absolute folder (ADR-0120) | Relists 200 ms after the last event; a deleted folder is not re-watched |

Code: [capability wiring](../supervisor/src/capabilities/lifecycle.rs).

## 12. Paths and persistence

| Data | Location |
| :--- | :--- |
| Config | `-c`, else `$MANTLE_CONFIG_DIR`, `$XDG_CONFIG_HOME/mantle`, `~/.config/mantle`; entry `shell.lua` |
| Instance directory | `$XDG_RUNTIME_DIR/mantle/<supervisor pid>-<start ms>/`, mode `0700` (ADR-0222, ADR-0227). Stopped runs' directories stay until logout |
| In it | `control.sock`, `shell.log`, `instance.lock` (held while the Supervisor lives), `config`, spooled `notifications/` and `tray/` PNGs |
| Session-lock marker | `$XDG_RUNTIME_DIR/mantle/session-locked` |
| `persistent_table` files | The absolute path the config names; a relative one is refused (ADR-0136) |

`storage`:

- A write pushes at once and saves 1 s after the file's last write, via a temporary file and rename.
- Another writer's change, another shell's included, replaces memory whole, refills missing
  defaults and pushes (ADR-0223).
- An unparseable file is logged once and never saved over until it parses (ADR-0225).
- A save pending at shutdown is lost.

Code: [paths](../shared/src/paths.rs), [instance](../supervisor/src/instance.rs),
[storage](../supervisor/src/capabilities/storage/controller.rs).

## 13. Control socket and wire format

Renderers and control clients (`mantle set`, `toggle`, `call`) connect to `control.sock`. Each
frame is a 4-byte big-endian length (≤ 16 MiB) followed by JSON. The first frame is the handshake
`{"generation_id": N}`, due within 10 s; the Supervisor checks the peer's `SO_PEERCRED` pid against
the pid it spawned for that generation. Control clients use `u32::MAX` and may send only `SetState`
and `Call`.

Later frames are tagged `{"kind", "data"}`. A command:

```json
{
  "kind": "Command",
  "data": {
    "params": {
      "generation_id": 4,
      "capability": "audio",
      "action": "set_volume",
      "arguments": [0.5],
      "expected_revision": 42
    },
    "id": 105
  }
}
```

| Direction | Frames |
| :--- | :--- |
| Renderer → Supervisor | `Command`, `SecureSubmit`, `LockReport`, `StartCapability`, `CallResult`; control clients `SetState`, `Call` |
| Supervisor → Renderer | `StateSnapshot` (capability, per-capability `revision`, payload), `Reevaluate`, `ProcessOutput`, `ProcessExited`, `IdleEvent`, `SetSessionLock`, `SetState`, `Call`, `CallResult` |

| Limit | Value |
| :--- | :--- |
| Connections | 64 at once; more are closed on accept |
| Outbound queue | 1024 frames per peer; a Renderer that falls that far behind is hung up on and respawned |
| Pending `mantle call`s | 64; the CLI waits 5 s for an answer |

- `StateSnapshot` revisions start at 1. A payload equal to the last is not resent, except `tray`
  and `notifications`, whose icon files change in place.
- A frame naming a generation other than its connection's is dropped.
- Frames from a non-authoritative generation are dropped, except `CallResult`: after a respawn,
  the generation a call went to may still answer it. Only that generation's answer is accepted.
- `process` commands go to the process registry; other names resolve through the roster.
- Nothing checks `expected_revision`, so it is not an authorization guarantee.

Code: [wire types](../shared/src/lib.rs), [framing](../shared/src/framing.rs),
[socket](../supervisor/src/socket/mod.rs), [calls](../supervisor/src/socket/calls.rs),
[generation filter](../supervisor/src/main.rs).

## 14. Reload lifecycle

| Event | Behavior |
| :--- | :--- |
| Config edit | The watcher covers the config tree's `.lua` and `.frag` files, skips byte-identical saves and sends `Reevaluate` 200 ms after the last change (ADR-0047) |
| Successful evaluation | Applies in place in the same generation: the scene reconciles, then surfaces whose declaration was removed, added or changed a creation-time field are destroyed or created (ADR-0216) |
| Failed evaluation or apply | The active scene and surfaces stay. A failed evaluation sets `mantle.rescue`; a failed reload apply only logs a warning |
| Locked | An edit that would recreate a lock surface is refused; save again after unlock |
| Renderer exit | A new generation spawns and is replayed every last snapshot and any owed lock (ADR-0058). At most three respawns per 60 s; the next waits 30 s. Exit code 71 (compositor gone) stops the shell |

Code: [watcher](../supervisor/src/watcher.rs), [apply](../renderer/src/wayland/output.rs),
[respawn](../supervisor/src/supervisor.rs), [brake](../supervisor/src/generation.rs).

## Troubleshooting

Start with `mantle log -f`; run the shell with `-v` (or `-vv` for debug) to see the lines quoted
below. A capability starts only when the config reads `mantle.<name>`, so a config that never
touches it has no server, watcher or agent at all.

| Symptom | Cause | Fix |
| :--- | :--- | :--- |
| A capability always reads `nil` | Its backend is missing ([Requirements](#requirements)), or the config reads it only inside a callback that never ran | Check the log for its start line; read `mantle.<name>` at config top level |
| `battery.present` is `false` | UPower's display device is not a present battery (desktop, or battery not detected), or UPower is not running (`state` `"Unknown"`) | Expected on desktops; otherwise start `upower.service`, then restart the shell; `upower -d` should list `DisplayDevice` |
| No tray icons | Config never reads `mantle.tray`; another host (waybar, snixembed) owns `org.kde.StatusNotifierWatcher`; the app is XEmbed-only; or it registered at an unlisted path before the shell started | Read `mantle.tray`; stop the other host; restart the app so it registers again |
| Notifications not showing | Another daemon (mako, dunst, swaync) owns `org.freedesktop.Notifications` (`another notification daemon already owns this name`), or the config never reads `mantle.notifications` | Stop and disable the other daemon, then restart the shell. `busctl --user status org.freedesktop.Notifications` names the owner |
| No notification sound | DND or quiet on (critical still plays); the app is muted; the file is not Ogg Vorbis or 16-bit WAV, is over 4 MiB or 30 s, or lies outside the sound roots; `sound-name` with no tier sound registered | See [§1](#1-notifications) |
| Polkit prompts not appearing | Config never reads `mantle.polkit`, so the agent never registers; another agent (polkit-gnome, hyprpolkitagent) registered first; `$XDG_SESSION_ID` unset | Read `mantle.polkit`; stop the other agent and restart the shell; start the session through logind |
| Polkit authentication always fails | `/run/polkit/agent-helper.socket` is missing, so the helper cannot run | Check that the installed polkit provides that socket |
| Idle never fires | A block-mode idle inhibitor is held (`systemd-inhibit --list`), a browser or player holds ScreenSaver, a Wayland surface inhibitor is up, or the config's own `inhibit` is still held; or the compositor lacks `ext_idle_notifier_v1` | `mantle.idle.inhibited` and `inhibitors` name the holder (empty `who` is the compositor). Set `IdleAction=ignore` in `logind.conf` so logind does not act too |
| Unlock refuses the right password | PAM stack `login` in use and `pam_nologin` or `pam_shells` refusing | Install `packaging/pam.d/mantle` to `/etc/pam.d/mantle` |
| Locked session with no lock screen | The Renderer died and its replacement could not retake the lock (`could not take the session lock over`) | Switch VT and unlock through the compositor's own mechanism |
| Keyboard layout switch does nothing | Not niri or Hyprland; only one layout configured (`layout_count` 1); on Hyprland it sends `switchxkblayout main <i>` to the keyboard marked `main`, which likely fails on Hyprland 0.56+, whose socket parses Lua (the other Hyprland writes use `hl.dsp.*`) | Configure several layouts in the compositor; on Hyprland 0.56+, switch through a compositor keybind until the engine sends an `hl.*` call |
| Caps/Num Lock always `false` | No readable `/dev/input` keyboard with LEDs and no sysfs LED | Give the user read access to the input device |
| `brightness` reads `nil` | No `/sys/class/backlight` device; external monitors are not covered | None; `brightness` is backlight-only |
| Brightness writes ignored | Session not active (another VT), so logind refuses `SetBrightness` | Switch back to the session |
| Wi-Fi or `network` stays `nil` | NetworkManager not running | Start it, then restart the shell; a reload does not retry |
| Bluetooth inert | `bluetoothd` started after the shell | Restart the shell |
| Pairing prompt never shows | The adapter is not visible and this shell did not start the pairing, or the device asks for a PIN or passkey entry (rejected) | Make the adapter visible or pair from the shell |
| Volume or app list missing | PipeWire unreachable when `audio` started; no reconnect | Restart the shell after PipeWire is up |
| Workspaces `nil` | Neither `$NIRI_SOCKET` nor `$HYPRLAND_INSTANCE_SIGNATURE` in the Supervisor's environment | Launch `mantle` from the compositor session |
| Updates never check | `configure` not called (dormant), or no `pacman` on `PATH` (`package_manager` `nil`) | Call `configure` with an interval |
| Shell stops respawning for 30 s | The Renderer died three times within 60 s | Fix the error in `mantle log`; the next respawn follows the cooldown |
| Edits not reloading | File is not `.lua` or `.frag`, the save was byte-identical, or the directory is unreadable (`changes inside it will not reload`) | Check the log; the error of a failed reload shows in `mantle.rescue` |

See also: [capabilities](lua-api/capabilities.md), [scripting](lua-api/scripting.md),
[`CONTEXT.md`](../CONTEXT.md), [decisions](decisions.md).
