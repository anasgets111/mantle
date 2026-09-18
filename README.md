# Obelisk

A Wayland shell engine. You build the desktop shell in Lua, and Rust runs it. A config declares
the bars, popups, launcher and lock screen as a tree of nodes; the engine owns the platform
connections, input, layout and painting. A config that crashes does not take the session with it,
and saving one reloads it in place without losing its state.

Obelisk ships no shell of its own. [`share/starter`](share/starter) is a minimal example config;
[anasgets111/dotfiles](https://github.com/anasgets111/dotfiles) is the reference one, a full shell
built on the engine:

https://github.com/user-attachments/assets/770bd04b-bc43-43b9-a388-eccda8d9528f

Status: pre-release. Nothing is published, the Lua API changes without notice, and there is no
distro package yet.

## Requirements

| What | Needs |
| :--- | :--- |
| Compositor | Wayland with `wlr-layer-shell-v1` and `ext-session-lock-v1` |
| Blur | `ext-background-effect-v1`, ignored where absent |
| Workspaces, keyboard layout | niri or Hyprland |
| Updates capability | pacman, through libalpm |
| Linked at build | PipeWire, PAM, udev, EGL, xkbcommon, libwayland-client, libwayland-egl |

Lua 5.4 is vendored, so no system Lua is needed. `just check` also needs `lua-language-server`.

## Build and install

There is no packaging recipe: `just swap` is both the install and the release dev loop.

```sh
just build   # obelisk and obelisk-renderer into target/debug
just run     # that pair on share/starter, leaving ~/.config/obelisk alone
just check   # fmt, tests, clippy, doc links, Lua parse and types
just swap    # release, into $CARGO_HOME/bin, replacing and restarting a running shell
```

A package installs `packaging/pam.d/obelisk` itself.

To have the compositor start it instead: `spawn-at-startup "obelisk"` in niri,
`exec-once = obelisk` in Hyprland, `exec obelisk` in sway.

## Commands

| Command | Does |
| :--- | :--- |
| `obelisk` | run the config |
| `obelisk init` | write `shell.lua`, plus a `.luarc.json` pointing the LSP at the stubs |
| `obelisk check` | evaluate the config and exit, taking no surface |
| `obelisk list` | show the running shells: PID, uptime, runtime directory, config |
| `obelisk log -f` | print what a running shell wrote to stdout and stderr |
| `obelisk set NAME VALUE` | write a running config's `state(NAME)` signal |
| `obelisk toggle NAME [VALUE]` | flip it when it holds a boolean, or swap VALUE with its initial |
| `obelisk call NAME [ARGS]` | run the config's `action(NAME, fn)` and print what it returned |

The last three are how a compositor keybind reaches a running shell: bind
`obelisk toggle launcher_open` against `state("launcher_open", false)`, or `obelisk call
launcher.open` against `action("launcher.open", fn)`. VALUE and ARGS are read as JSON, and
anything that is not JSON is taken as a string. `obelisk -h` has the rest.

The config is a directory, not a file: `require` resolves inside it, and any `.lua` file changing
triggers a reload. `-c DIR` beats `$OBELISK_CONFIG_DIR`, which beats `$XDG_CONFIG_HOME/obelisk`.

## A config

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
            content = obelisk.system:map(function(s)
                return os.date("%H:%M", s and s.time)
            end),
            foreground = "#cdd6f4ff",
        },
    },
}
```

Surfaces are `panel`, `window`, `popup`, `lock`. Nodes are `row`, `column`, `text`, `image`,
`icon`, `button`, `textfield`, `rect`, `list`. Anything that changes over time is a
signal, so the `:map` above re-resolves that clock without re-running the config.

## Capabilities

`obelisk.<name>` exposes platform state as a signal and takes actions. A backend starts on first use
and stays for the session.

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

## Why processes

The Supervisor holds the platform connections and the Renderer holds Lua and Wayland surfaces, so a
config that crashes the Renderer leaves the Supervisor and its connections running.

A **generation** is one Renderer process and its Lua state. A new one starts only when the
Supervisor respawns a Renderer that exited, behind a brake that stops a crash loop.

## Docs

| Doc | Holds |
| :--- | :--- |
| [Lua API](docs/lua-api.md) | what a config can declare and call |
| [Services](docs/services.md) | capability payloads, actions and their backends |
| [Decisions](docs/decisions.md) | why it is built this way, including what was rejected |
| [Roadmap](docs/roadmap.md) | what is next, what is waiting on a decision, and what it will never do |
| [CONTEXT.md](CONTEXT.md) | the vocabulary all four use |

## License

MIT.
