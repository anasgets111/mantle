# Installation

What Mantle needs, how to install it, and how to start it with the session. The
[introduction](../introduction.md#your-first-shell) walks through a first config.

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
| Build | Rust 1.89+, PipeWire, PAM, udev, EGL, GBM, xkbcommon, libwayland-client, libwayland-egl. Lua 5.4 is vendored |
| Editor completion | lua-language-server |

A [capability](../capabilities/index.md) starts on the config's first `mantle.<name>` read and
needs its backend only from then. What each one does without it is in its page's Backend section.

| Capability | Needs |
| :--- | :--- |
| [`notifications`](../capabilities/notifications.md#backend), [`tray`](../capabilities/tray.md#backend) | A session bus, and no other notification daemon or tray host holding the name |
| [`mpris`](../capabilities/mpris.md#backend) | A session bus |
| [`network`](../capabilities/network.md#backend) | NetworkManager |
| [`bluetooth`](../capabilities/bluetooth.md#backend) | `bluetoothd`, running before `mantle` starts |
| [`audio`](../capabilities/audio.md#backend), [`privacy`](../capabilities/privacy.md#backend) | PipeWire, running when the capability starts; it does not reconnect |
| [`battery`](../capabilities/battery.md#backend) | UPower |
| [`power`](../capabilities/power.md#backend) | UPower, power-profiles-daemon |
| [`brightness`](../capabilities/brightness.md#backend) | A `/sys/class/backlight` device, logind |
| [`keyboard`](../capabilities/keyboard.md#backend) | Read access to the `/dev/input` keyboard; niri or Hyprland for layouts |
| [`workspaces`](../capabilities/workspaces.md#backend) | niri or Hyprland |
| [`windows`](../capabilities/windows.md#backend) | niri or Hyprland, else `wlr-foreign-toplevel-management-v1` |
| [`idle`](../capabilities/idle.md#backend) | `ext-idle-notify-v1`, logind |
| [`lock`](../capabilities/lock.md#backend) | `ext-session-lock-v1`, logind, the `mantle` PAM stack ([below](#install)) |
| [`polkit`](../capabilities/polkit.md#backend) | polkitd with its helper socket `/run/polkit/agent-helper.socket`, `$XDG_SESSION_ID`, no other polkit agent running |
| [`sysinfo`](../capabilities/sysinfo.md#backend) | hwmon `k10temp`, `coretemp` or `acpitz` for CPU temperature; `amdgpu`, `nouveau` or `nvidia` for GPU |
| [`updates`](../capabilities/updates.md#backend) | `pacman` and the `curl` it depends on, `dnf` or `apt-get`; `pkexec`, answered by the `polkit` agent; paru or yay for AUR |
| [`applications`](../capabilities/applications.md#backend) | `$TERMINAL` for `Terminal=true` entries, `xdg-open` for `open_url` |

`system`, `files`, `storage` and `processes` need nothing beyond the paths and programs the config
names.

## Install

| Route | Steps |
| :--- | :--- |
| Arch | [`mantle-git`](https://aur.archlinux.org/packages/mantle-git) from the AUR builds `main` and installs the PAM stack |
| Ubuntu, Fedora | A [release](https://github.com/anasgets111/mantle/releases)'s `sudo apt install ./mantle_<version>_amd64.deb` or `sudo dnf install ./mantle-<version>-1.x86_64.rpm`: under `/usr`, with its libraries as dependencies and the PAM stack. Ubuntu 26.04 and Fedora 44 or later, since it needs glibc 2.43 |
| Release tarball | `sudo tar -xzf mantle-<version>-x86_64-linux.tar.gz -C /` installs under `/usr/local`, with the PAM stack and polkit rule under `/etc`; the libraries are yours to install. Needs glibc 2.43 |
| From source | `cargo build --workspace --release`, then copy `target/release/mantle` and `target/release/mantle-renderer` into one directory on `PATH`, such as `~/.local/bin` |
| From a checkout, for development | `just run [config]` builds and runs `config` (default `share/starter`). `just swap` builds an optimised pair into `$CARGO_HOME/bin` and restarts the running shell |

`mantle` starts `mantle-renderer` from its own directory, so both must come from one build
([binaries](cli.md#binaries)).

A source build needs a C compiler and `pkg-config` for the vendored Lua, `clang` for PipeWire's
bindings, and the development files of what the binaries link:

| Distro | Packages |
| :--- | :--- |
| Arch | `base-devel clang pipewire pam systemd-libs wayland libxkbcommon libglvnd mesa` |
| Fedora | `gcc pkgconf-pkg-config clang pipewire-devel pam-devel systemd-devel wayland-devel libxkbcommon-devel mesa-libEGL-devel mesa-libgbm-devel` |
| Debian, Ubuntu | `build-essential pkg-config clang libclang-dev libpipewire-0.3-dev libpam0g-dev libudev-dev libwayland-dev libxkbcommon-dev libegl-dev libgbm-dev` |

Rust 1.89 or later comes from [rustup](https://rustup.rs) where the distro's `cargo` is older.
Mantle is developed and run on Arch. The Fedora and Debian lists build the workspace in a
container; running the shell, and the `updates` capability's dnf and apt backends, are untested
on Fedora and Ubuntu for now.

Two optional system files ship in [`packaging/`](../../packaging):

| File | Install to | Without it |
| :--- | :--- | :--- |
| `pam.d/mantle` | `/etc/pam.d/mantle` | Unlock authenticates against the `login` stack, whose `pam_nologin` or `pam_shells` may refuse the right password. Polkit prompts use polkit's own stack either way |
| `polkit-1/rules.d/50-mantle-pacman.rules` | `/etc/polkit-1/rules.d/` | `updates` installs through pacman ask for the password on every `pkexec`; with it, a `wheel` user approves once per run |

## Set up a config

`mantle init` writes a starter `shell.lua` (a one-clock bar) and a `.luarc.json` into the config
directory, `~/.config/mantle` by default ([which config](cli.md#which-config-and-which-shell)). It
keeps an existing file unless given `--force`.

The `.luarc.json` points lua-language-server at the [`lua-meta/`](../../lua-meta) stubs, which give
completion and type checks for every node, surface and capability. A package installs them under
`$PREFIX/share/mantle/lua-meta`. Otherwise `init` writes the binary's embedded copy to
`$XDG_DATA_HOME/mantle/lua-meta`, rewriting any stub that differs, and `mantle check` says when
they are out of date. The `.luarc.json` also raises LuaLS's `type-check`, `unbalanced`, `strict` and `global` groups
and `unused-local` to warnings in every file, so a wrong type or a dead `require` shows up.

## Run the shell

| Want | Do |
| :--- | :--- |
| Start with the session | niri: `spawn-at-startup "mantle"`. Hyprland: `exec-once = mantle` |
| Start from a terminal | `mantle` (foreground) or `mantle -d` (detached) |
| Use another config | `mantle -c DIR` ([which config](cli.md#which-config-and-which-shell)) |

Launch `mantle` from inside the compositor session: `workspaces`, `windows` and `keyboard` find
niri or Hyprland through the environment the session sets. Every other command is on the
[CLI](cli.md) page.

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `cargo run -p supervisor` runs a stale Renderer and reports the mismatch as a config error | `just run`, which builds both binaries |
| Another notification daemon, tray host or polkit agent is running | Stop it: Mantle takes those names only when they are free, and the tray waits in the queue ([FAQ](faq.md#capabilities)) |

See also: [introduction](../introduction.md), [CLI](cli.md), [FAQ](faq.md).

Source: [init](../../supervisor/src/setup.rs), [binaries](../../supervisor/src/generation.rs),
[capability wiring](../../supervisor/src/capabilities/lifecycle.rs), [PAM worker](../../supervisor/src/pam_worker.rs).
