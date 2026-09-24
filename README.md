# Mantle

A Wayland desktop-shell engine: a Lua config declares bars, popups, launchers and lock screens as
node trees; Rust owns platform connections, input, layout and rendering. A config error never
takes the session down, and saving a `.lua` file reloads in place, keeping signal state.

Mantle ships no shell of its own. [`share/starter`](share/starter) is a one-clock bar;
[anasgets111/dotfiles](https://github.com/anasgets111/dotfiles) is a full shell built on it:


https://github.com/user-attachments/assets/b4a56c2f-a946-44f9-9bfd-2c6046d7a72f

https://github.com/user-attachments/assets/038ee763-d7b6-4df9-9f79-2f131d4f0dcd

Status: pre-release. The Lua API changes without notice; the [changelog](docs/changelog.md) lists what moved.

Docs: **<https://anasgets111.github.io/mantle/>**, built from [`docs/`](docs).

## Requirements

| Feature | Needs |
| :--- | :--- |
| Surfaces | A Wayland compositor with `wlr-layer-shell-v1`; `ext-session-lock-v1` for `lock` |
| Workspaces, keyboard layout | niri or Hyprland |
| `windows` capability | niri or Hyprland IPC, else `wlr-foreign-toplevel-management-v1` |
| `window`, `popup` surfaces | `xdg-shell`; skipped when absent |
| `capture` node | `ext-image-copy-capture-v1`, else `wlr-screencopy-v1` |
| `blur = true` | `ext-background-effect-v1`; ignored when absent |
| `idle` capability | `ext-idle-notify-v1` |
| Capabilities over D-Bus | NetworkManager, BlueZ, UPower, power-profiles-daemon, logind, polkit ([per capability](docs/guide/installation.md#requirements)) |
| Fonts | fontconfig (`fc-match`) |
| `updates` capability | pacman; `pkexec` to install |
| Build | Rust 1.89+, libalpm, PipeWire, PAM, udev, EGL, GBM, xkbcommon, libwayland-client, libwayland-egl. Lua 5.4 is vendored |
| `just check` | `lua-language-server`, `luac`, `python3` |

## Install and build

On Arch, [`mantle-git`](https://aur.archlinux.org/packages/mantle-git) builds `main` and installs
`/etc/pam.d/mantle`. [`packaging/`](packaging) holds that PAM stack (without it, unlock and polkit
prompts fall back to `login`) and a polkit rule for `updates` installs.

| Recipe | Does |
| :--- | :--- |
| `just build` | `mantle` and `mantle-renderer` into `target/debug` |
| `just run [config]` | Builds, then runs `config` (default `share/starter`), leaving `~/.config/mantle` alone |
| `just check` | The gate (on the staged tree when there are also unstaged edits): rustfmt, tests, clippy, rustdoc, Lua parse and format, LuaLS types |
| `just docs` / `just book` | Serve the docs site locally / build it and check every link |
| `just fmt` | Formats Rust and Lua |
| `just swap` | Optimised build into `$CARGO_HOME/bin`, then restarts the running shell detached |

Autostart: `spawn-at-startup "mantle"` in niri, `exec-once = mantle` in Hyprland.

## Quick start

```sh
mantle init           # shell.lua and a .luarc.json pointing LuaLS at the stubs
$EDITOR ~/.config/mantle/shell.lua
mantle check          # evaluate with no Wayland or subprocesses; exits 1 on error
mantle -d             # run detached
mantle log -f         # follow its output
```

The config is a directory: `-c DIR`, else `$MANTLE_CONFIG_DIR`, else `$XDG_CONFIG_HOME/mantle`,
else `~/.config/mantle`. `require` resolves inside it, and saving any `.lua` in it reloads.

## CLI

| Command | Does |
| :--- | :--- |
| `mantle [-d] [-v…] [--profile[=SECS]]` | Run the shell; `-d` detaches |
| `mantle init [--force]` | Write `shell.lua` and `.luarc.json` |
| `mantle check` | Evaluate the config, validate each surface's own properties (not the node tree), and exit |
| `mantle log [-f]` | Print or follow the shell's output |
| `mantle list` | Running shells: PID, uptime, runtime dir, config |
| `mantle set NAME VALUE` | Write `state(NAME)` |
| `mantle toggle NAME [VALUE]` | Flip a boolean, or alternate between VALUE and the initial value |
| `mantle call NAME [ARGS…]` | Run `action(NAME)` and print its return |

Keybinds drive a running shell with `toggle` and `call`. `-c` and `--pid` pick the shell; `-V`
and `-h` print version and help. Full contract: [CLI](docs/guide/cli.md).

## Docs

| Doc | For |
| :--- | :--- |
| [Site](https://anasgets111.github.io/mantle/) | Config authors: guide, nodes, surfaces, capabilities, cookbook. Source in [`docs/`](docs), entry [`introduction.md`](docs/introduction.md) |
| [Changelog](docs/changelog.md) | User-facing Lua API and CLI changes |
| [Decisions](DECISIONS.md) | ADRs: why each design, and what was rejected |
| [Roadmap](docs/roadmap.md) | Open gaps, open questions and non-goals |
| [Glossary](docs/glossary.md), [CONTEXT.md](CONTEXT.md) | Vocabulary: user-facing terms, then engine-internal ones |
| [`lua-meta/`](lua-meta) | LuaLS stubs `mantle init` points the editor at |

## License

[MIT](LICENSE).
