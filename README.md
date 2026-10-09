# Mantle

A Rust engine for building Wayland desktop shells in Lua. Declare bars, popups, launchers and
lock screens; save your Lua config to reload the shell.

[Documentation](https://anasgets111.github.io/mantle/) ·
[Examples](https://anasgets111.github.io/mantle/cookbook/) ·
[Changelog](https://anasgets111.github.io/mantle/changelog.html)

Version 0.x: a minor release can break the Lua API.

Mantle ships no shell of its own. [`share/starter`](share/starter) is a one-clock bar;
[anasgets111/dotfiles](https://github.com/anasgets111/dotfiles) is a full shell built on it.
[mantle-glass](https://github.com/anasgets111/mantle-glass) (Liquid Glass) and
[mantle-material](https://github.com/anasgets111/mantle-material) (Material 3 Expressive) are
[component libraries](https://anasgets111.github.io/mantle/libraries.html) for apps and shells.

The demo opens on a finished shell, then rebuilds it from the starter one save at a time. Mantle
itself [types and records it](demo/director).

https://github.com/user-attachments/assets/6eea81f4-9755-468e-aa2d-f8151c25e2f5

- Built-in [capabilities](https://anasgets111.github.io/mantle/capabilities/) for audio, network,
  Bluetooth, notifications, tray, workspaces and other desktop services.
- [Signals](https://anasgets111.github.io/mantle/guide/signals.html) update widgets when values change.
- `mantle check` catches Lua and layout errors before you run the shell.
- `mantle init` sets up LuaLS completion and type checking for your config.
- [Coding agents](https://anasgets111.github.io/mantle/guide/agents.html) can check, reload and drive a config from the command line.

## Install

Requires a Wayland compositor with `wlr-layer-shell-v1`. Workspaces and keyboard layout use
niri or Hyprland; lock screens need `ext-session-lock-v1`.

| Platform | Install |
| :--- | :--- |
| Arch | [`mantle-git`](https://aur.archlinux.org/packages/mantle-git) from the AUR |
| Ubuntu 26.04+, Fedora 44+ | `.deb` or `.rpm` from [releases](https://github.com/anasgets111/mantle/releases) |
| From source | [Build instructions](https://anasgets111.github.io/mantle/guide/installation.html#build-from-source) |

Developed and run on Arch. Running on Fedora and Ubuntu is untested.
[Installation and requirements](https://anasgets111.github.io/mantle/guide/installation.html)
cover dependencies and setup.

## Quick start

```sh
mantle init           # create the starter config and editor settings
$EDITOR ~/.config/mantle/shell.lua
mantle check          # evaluate and lay out without Wayland; exits 1 on error
mantle -d             # run detached
mantle log -f         # follow its output
```

Save a `.lua` file in the config directory to reload. Use `mantle stop` to stop the shell.
See the [CLI guide](https://anasgets111.github.io/mantle/guide/cli.html) for config paths and
compositor keybinds.

## Development

`just run` builds both binaries and runs the starter. `just check` runs the full engine checks;
see [AGENTS.md](AGENTS.md#checks-by-change) for checks by change. The full suite needs
`lua-language-server`, `luac` and `python3` alongside the build dependencies.
See the [justfile](justfile) for all recipes.

[Roadmap](https://anasgets111.github.io/mantle/roadmap.html) ·
[Design decisions](DECISIONS.md) · [Docs source](docs/)

## License

[MIT](LICENSE).
