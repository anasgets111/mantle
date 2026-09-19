# Mantle

A Wayland shell engine. Lua configs declare bars, popups, launchers, and lock screens as node
trees. Rust handles platform connections, input, layout, and rendering. Config crashes do not
kill the Wayland session. File changes reload in place and preserve signal state.

Mantle ships no built-in shell. [`share/starter`](share/starter) provides a minimal config.
[anasgets111/dotfiles](https://github.com/anasgets111/dotfiles) is the reference full shell:

https://github.com/user-attachments/assets/038ee763-d7b6-4df9-9f79-2f131d4f0dcd

Status: pre-release. The Lua API changes without notice.

## Requirements

| Feature | Requirement |
| :--- | :--- |
| Compositor | Wayland with `wlr-layer-shell-v1` and `ext-session-lock-v1` |
| Blur | `ext-background-effect-v1`, ignored when absent |
| Workspaces, keyboard layout | niri or Hyprland |
| Updates capability | pacman, via libalpm |
| Build dependencies | PipeWire, PAM, udev, EGL, xkbcommon, libwayland-client, libwayland-egl |

Lua 5.4 is vendored. `just check` requires `lua-language-server`.

## Build and install

> [!NOTE]
> On Arch, [`mantle-git`](https://aur.archlinux.org/packages/mantle-git) builds from `main` and installs `/etc/pam.d/mantle`.

Build from source with `just`:

```sh
just build   # mantle and mantle-renderer into target/debug
just run     # run against share/starter, leaving ~/.config/mantle untouched
just check   # fmt, tests, clippy, doc links, Lua parse and types
just swap    # release build into $CARGO_HOME/bin, then restart running shell
```

Autostart: `spawn-at-startup "mantle"` in niri, `exec-once = mantle` in Hyprland.

## Commands

| Command | Action |
| :--- | :--- |
| `mantle` | Run the config |
| `mantle init` | Write `shell.lua` and `.luarc.json` pointing the LSP at stubs |
| `mantle check` | Evaluate the config and exit without creating surfaces |
| `mantle list` | List running shells by PID, uptime, runtime directory, and config |
| `mantle log -f` | Stream shell stdout and stderr |
| `mantle set NAME VALUE` | Update a running config's `state(NAME)` signal |
| `mantle toggle NAME [VALUE]` | Toggle a boolean signal, or alternate between VALUE and initial state |
| `mantle call NAME [ARGS]` | Run a registered `action(NAME, fn)` and print the result |

Compositor keybinds reach a running shell via `toggle` and `call`: bind `mantle toggle launcher_open` against `state("launcher_open", false)` or `mantle call launcher.open` against `action("launcher.open", fn)`. Arguments parse as JSON, falling back to strings. `mantle -h` lists all options.

Configs are directories. `require` resolves relative to the config root, and editing any `.lua` file triggers a reload. Precedence: `-c DIR` > `$MANTLE_CONFIG_DIR` > `$XDG_CONFIG_HOME/mantle`.

## Example config

```lua
return {
    panel {
        id = "bar",
        layer = "Top",
        anchor = { top = true, left = true, right = true },
        exclusive = true,
        height = 34,
        background = "#1e1e2e80",
        child = text {
            content = mantle.system:map(function(s)
                return os.date("%H:%M", s and s.time)
            end),
            foreground = "#cdd6f4ff",
        },
    },
}
```

Surfaces: `panel`, `window`, `popup`, `lock`. Nodes: `row`, `column`, `text`, `image`, `icon`, `button`, `textfield`, `rect`, `list`.
Dynamic state uses signals. The `:map` call updates clock text directly without re-evaluating the config tree.

## Capabilities

`mantle.<name>` exposes platform state as signals and accepts actions. Backends start on first use and persist for the session.

| Hardware | Desktop | System |
| :--- | :--- | :--- |
| audio | applications | files |
| battery | idle | polkit |
| bluetooth | lock | power |
| brightness | mpris | processes |
| keyboard | notifications | sysinfo |
| network | privacy | system |
| storage | tray | updates |
| | workspaces | |

## Process architecture

The Supervisor maintains platform connections. The Renderer executes Lua and manages Wayland surfaces. A Renderer crash does not drop platform connections or terminate the Supervisor.

A generation is one Renderer process and its Lua runtime. If the Renderer exits, the Supervisor respawns it with rate-limiting to prevent crash loops.

## Docs

| Doc | Contents |
| :--- | :--- |
| [Lua API](docs/lua-api.md) | Node and surface properties, signal combinators, global functions |
| [Services](docs/services.md) | Capability state payloads, actions, and platform backends |
| [Decisions](docs/decisions.md) | Architecture decisions, alternatives considered, and rejected designs |
| [Roadmap](docs/roadmap.md) | Upcoming milestones, open design questions, and non-goals |
| [CONTEXT.md](CONTEXT.md) | Core domain terminology and concepts |

## License

MIT.
