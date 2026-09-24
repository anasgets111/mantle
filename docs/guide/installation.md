# Installation

What Mantle needs, how to build and install it, and how to start a first shell. Read this once
before [the introduction](../introduction.md#your-first-shell)'s walkthrough.

```sh
mantle init           # ~/.config/mantle/shell.lua and a .luarc.json for lua-language-server
mantle check          # evaluate it with no Wayland; exits 1 on an error
mantle -d             # run the shell detached
mantle log -f         # follow its output
```

## Requirements

| Feature | Needs |
| :--- | :--- |
| Surfaces | A Wayland compositor with `wlr-layer-shell-v1`; `ext-session-lock-v1` for `lock` |
| `window`, `popup` surfaces | `xdg-shell`; skipped when absent |
| `capture` node | `ext-image-copy-capture-v1`, else `wlr-screencopy-v1` |
| `blur = true` | `ext-background-effect-v1`; ignored when absent |
| Fonts | fontconfig (`fc-match`) |
| Build | Rust 1.89+, libalpm, PipeWire, PAM, udev, EGL, GBM, xkbcommon, libwayland-client, libwayland-egl. Lua 5.4 is vendored |
| Editor completion | lua-language-server |

Each [capability](../capabilities/index.md) starts only when the config first reads
`mantle.<name>`, and needs its backend only then. The log lines quoted are the ones to search for
in `mantle log`.

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
| `lock` | `ext_session_lock_v1`; logind session; PAM stack `mantle` ([below](#install)) | No lock protocol: the Renderer reports `Refused`. No `mantle` stack: authenticates against `login` |
| `polkit` | polkitd on the system bus, `$XDG_SESSION_ID`, the socket-activated helper at `/run/polkit/agent-helper.socket`, and no other agent registered | No session id or another agent: logged, agent off for the run; prompts go to the other agent or nowhere |
| `workspaces`, `windows` | niri (`$NIRI_SOCKET`) or Hyprland (`$HYPRLAND_INSTANCE_SIGNATURE`); else `zwlr_foreign_toplevel_management_v1` for `windows` | `workspaces` `nil`; `windows` `nil` without the wlr protocol |
| `sysinfo` | `/proc`; hwmon `k10temp`, `coretemp` or `acpitz` (CPU); `amdgpu`, `nouveau` or `nvidia` (GPU) | Missing chip: that temperature is absent |
| `system` | Nothing | Always ticks |
| `battery` | UPower | No UPower or no battery: `present = false`, `state` `"Unknown"` without UPower |
| `power` | UPower and/or power-profiles-daemon | Missing half: its fields absent; `set_profile` without the daemon is ignored |
| `brightness` | A `/sys/class/backlight` device; logind; an active session for writes | No device: `nil` for good. Inactive session: `SetBrightness` fails, logged |
| `keyboard` | Read access to a `/dev/input` keyboard with LEDs; `*::kbd_backlight` for backlight; niri or Hyprland for layout | No evdev: lock state read once from sysfs, else `false`. No backlight LED: `-1`. Other compositor: layout empty, count `0` |
| `applications` | `.desktop` files; `$TERMINAL` for `Terminal=true`; `xdg-open` for `open_url` | Terminal entries refuse to launch without `$TERMINAL` |
| `updates` | `pacman` on `PATH`; `pkexec`, answered by the `polkit` agent; paru or yay for AUR | No pacman: `package_manager` `nil`, every action refused |
| `files`, `storage` | Absolute paths from the config | Relative paths refused |
| `processes` | The programs the config names on `PATH` | A failed `session_process` start is logged and set as its `start_error` |

## Install

| Route | Steps |
| :--- | :--- |
| Arch | [`mantle-git`](https://aur.archlinux.org/packages/mantle-git) from the AUR builds `main` and installs the PAM stack |
| From source | `cargo build --workspace --release`, then copy `target/release/mantle` and `target/release/mantle-renderer` into one directory on `PATH`, such as `~/.local/bin` |
| From a checkout, for development | `just run [config]` builds and runs `config` (default `share/starter`). `just swap` builds an optimised pair into `$CARGO_HOME/bin` and restarts the running shell |

`mantle` starts `mantle-renderer` from its own directory, so the two binaries always travel
together and come from the same build ([binaries](cli.md#binaries)).

Two optional system files ship in [`packaging/`](../../packaging):

| File | Install to | Without it |
| :--- | :--- | :--- |
| `pam.d/mantle` | `/etc/pam.d/mantle` | Unlock and polkit prompts authenticate against the `login` stack, whose `pam_nologin` or `pam_shells` may refuse the right password |
| `polkit-1/rules.d/50-mantle-pacman.rules` | `/etc/polkit-1/rules.d/` | `updates` installs ask for the password on every `pkexec`; with it, a `wheel` user approves once per run |

## Set up a config

`mantle init` writes a starter `shell.lua` (a one-clock bar) and a `.luarc.json` into the config
directory, `~/.config/mantle` by default ([which config](cli.md#which-config-and-which-shell)). It
keeps an existing file unless given `--force`.

The `.luarc.json` points lua-language-server at the [`lua-meta/`](../../lua-meta) stubs, which give
completion and type checks for every node, surface and capability. A package installs them under
`$PREFIX/share/mantle/lua-meta`; otherwise `init` writes the copy embedded in the binary to
`$XDG_DATA_HOME/mantle/lua-meta` and rewrites it whenever the binary's copy differs; `mantle check`
says when it does. It also raises LuaLS's `type-check`, `unbalanced`, `strict` and `global` groups
and `unused-local` to warnings in every file, so a wrong type or a dead `require` shows up.

## Run the shell

| Want | Do |
| :--- | :--- |
| Check a config without starting it | `mantle check` ([what it covers](cli.md#what-check-covers)) |
| Start from a terminal | `mantle` (foreground) or `mantle -d` (detached) |
| Start with the session | niri: `spawn-at-startup "mantle"`. Hyprland: `exec-once = mantle` |
| Read its output | `mantle log`, or `mantle log -f` to follow |
| Use another config | `mantle -c DIR` |

Launch `mantle` from inside the compositor session: `workspaces`, `windows` and `keyboard` find
niri or Hyprland through the environment the session sets. Edit `shell.lua` while it runs; every
save reloads in place ([runtime](runtime.md#evaluation-reload-and-generations)).

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `cargo run -p supervisor` runs a stale Renderer and reports the mismatch as a config error | `just run`, which builds both binaries |
| Another notification daemon, tray host or polkit agent is running | Stop it: Mantle takes over those names only when they are free ([FAQ](faq.md#capabilities)) |

See also: [introduction](../introduction.md), [CLI](cli.md), [FAQ](faq.md).

Source: [init](../../supervisor/src/setup.rs), [binaries](../../supervisor/src/generation.rs),
[capability wiring](../../supervisor/src/capabilities/lifecycle.rs), [PAM worker](../../supervisor/src/pam_worker.rs).
