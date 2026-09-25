# CLI

The `mantle` binary starts the shell, checks a config without starting it, and lets a
compositor keybind reach a running shell. Reach for `set`/`toggle` when a key should change what
the shell shows, and `call` when it should make the shell do something.

A keybind workflow. The config declares the names:

```lua
-- `mantle toggle launcher_open` flips it; `mantle set launcher_open false` closes it.
local launcher_open = state("launcher_open", false)

-- `mantle toggle modal settings` opens "settings", or closes it when it is already open.
local modal = state("modal", "")

-- `mantle call volume.up` or `mantle call volume.up 0.1`.
action("volume.up", function(step)
    local audio = mantle.audio:get()
    if not audio or not audio.volume then
        error("no default output yet")
    end
    local volume = math.min(1.0, audio.volume + (step or 0.05))
    mantle.audio:set_volume(volume)
    return string.format("%d%%", math.floor(volume * 100 + 0.5))
end)

return panel {
    id = "launcher",
    layer = "Overlay",
    keyboard_interactivity = "OnDemand",
    visible = launcher_open,
    width = 480,
    height = 320,
    background = "#1e1e2eff",
    child = text {
        content = modal:map(function(name) return name == "" and "launcher" or name end),
        foreground = "#cdd6f4ff",
    },
}
```

The compositor binds keys to the commands. Hyprland (`hyprland.conf`):

```text
bind = SUPER, Space, exec, mantle toggle launcher_open
bind = SUPER, Escape, exec, mantle set launcher_open false
bind = SUPER, Comma, exec, mantle toggle modal settings
bind = , XF86AudioRaiseVolume, exec, mantle call volume.up
```

Hyprland with a Lua config (0.56+):

```text
hl.bind("SUPER + Space", hl.dsp.exec_cmd("mantle toggle launcher_open"))
hl.bind("SUPER + Comma", hl.dsp.exec_cmd("mantle toggle modal settings"))
hl.bind("XF86AudioRaiseVolume", hl.dsp.exec_cmd("mantle call volume.up"))
```

niri (`config.kdl`, inside `binds { }`):

```text
Mod+Space repeat=false { spawn "mantle" "toggle" "launcher_open"; }
Mod+Escape { spawn "mantle" "set" "launcher_open" "false"; }
Mod+Comma repeat=false { spawn "mantle" "toggle" "modal" "settings"; }
XF86AudioRaiseVolume allow-when-locked=true { spawn "mantle" "call" "volume.up"; }
```

In a terminal, `mantle call volume.up 0.1` prints the handler's return value, such as `60%`.

## Commands

| Command | Does |
| :--- | :--- |
| `mantle` | Starts the shell in the foreground. Stops on Ctrl-C or `SIGTERM` |
| `mantle -d` | Starts the shell in its own session with no terminal, waits until it is running (up to 5 s), prints its pid and returns. Its output goes to `mantle log` |
| `mantle init [--force]` | Creates the config directory. Writes `.luarc.json` and a starter `shell.lua`, keeping any that exist unless `--force`. Points lua-language-server at the type stubs: an installed package's, else writes current stubs to `$XDG_DATA_HOME/mantle/lua-meta` |
| `mantle check` | Evaluates and lays out the config once without Wayland and exits. See [What check covers](#what-check-covers) |
| `mantle log [-f]` | Prints a shell's stdout and stderr. `-f` keeps printing until that shell exits |
| `mantle list` | Prints running shells, oldest first: `PID`, `UPTIME`, `DIR` (the instance directory) and `CONFIG` |
| `mantle set <name> <value>` | Writes the running config's `state(name, ...)` and waits for the shell to accept it |
| `mantle toggle <name>` | Flips that state. It must hold a boolean |
| `mantle toggle <name> <value>` | Sets the state to `value`. If it already holds `value`, restores the `initial` its `state(name, initial)` declares |
| `mantle call <name> [args...]` | Runs the config's `action(name, fn)` with `args`, waits for it and prints what it returned |
| `mantle call` | Prints each `action` name the running config declares, one per line, sorted. After a failed reload, the actions [it left](runtime.md#evaluation-reload-and-generations) |
| `mantle set`, `mantle toggle` | Prints each `state` the running config declares, sorted, as `name<TAB>value`. The value is JSON with strings quoted, so passed back as one argument, `mantle set <name> <value>` restores it: `"true"` stays a string. A value JSON cannot hold (a function, a number-keyed table that is not a list) prints the name alone. After a failed reload, the states of the scene still on screen |
| `mantle -V`, `--version` | Prints `mantle <version>` |
| `mantle -h`, `--help` | Prints the built-in help |

Flags and the command may come in any order. `-V` and `-h` win over anything after them.

## Flags

| Flag | With | Does |
| :--- | :--- | :--- |
| `-c <dir>`, `--config <dir>`, `--config=<dir>` | Everything except `list` | The config directory. A path to a file (`shell.lua`) means its directory, with a notice. A relative path is made absolute |
| `-d`, `--detach` | Run only | Detached start, as above |
| `-v`, `--verbose` | Run only | Raises the log level. Repeat or group: `-v`, `-vv`, `-vvv` |
| `--profile[=SECS]` | Run only | Logs idle-loop, heap and PSS/GPU memory reports every `SECS` seconds, default 60. Implies `-v` |
| `--force` | `init` only | Overwrites `.luarc.json` and `shell.lua` |
| `-f`, `--follow` | `log` only | Follows the log until its shell exits |
| `--pid <pid>`, `--pid=<pid>` | `set`, `toggle`, `call`, `log` | Addresses the shell with that pid, as `mantle list` shows it. Refused together with `-c` |

A flag given to a command it does not apply to is an error, not ignored. `--detached` is the flag
`-d` passes to the copy it starts; typed by hand, it is ignored and the shell runs in the
foreground.

Log levels for a run:

| Flags | Prints |
| :--- | :--- |
| none | Errors, warnings, and start, reload, respawn and stop notices |
| `-v` | Also info |
| `-vv` | Also debug |
| `-vvv` | Also the noisiest debug lines. More `v`s change nothing |

`MANTLE_LOG` overrides the default level: `MANTLE_LOG=debug`, or per subsystem,
`MANTLE_LOG=warn,wayland=debug`. Levels are `off`, `error`, `warn`, `notice`, `info`, `debug`
(same as `debug1`) and `debug2`. An unknown entry is ignored with a warning. The config's own
`log.*` lines print at every level unless `MANTLE_LOG` names `config=<level>`.

Each log line reads `HH:MM:SS LEVEL subsystem: message`. Renderer lines prefix the subsystem with
`renderer/`, config lines use `config`, and `print` output is written as is. The subsystem is the
name `MANTLE_LOG` filters on (`renderer/wayland: ...` is `wayland`).

## Environment variables

| Variable | Read by | Effect |
| :--- | :--- | :--- |
| `MANTLE_CONFIG_DIR` | Every command | The config directory, below `-c` in [precedence](#which-config-and-which-shell) |
| `XDG_CONFIG_HOME`, `HOME` | Every command | The default config directory, `$XDG_CONFIG_HOME/mantle` or `~/.config/mantle` |
| `XDG_RUNTIME_DIR` | Run, `list`, `log`, `set`, `toggle`, `call` | Required. Instance directories live under `$XDG_RUNTIME_DIR/mantle/` |
| `XDG_DATA_HOME` | `init` | Where stubs go when no package provides them. Default `~/.local/share` |
| `MANTLE_LOG` | Run | Log filter, as above. Overrides the `-v` level |
| `MANTLE_DUMP_LAYOUT=<instance>` | Run, with `-vvv` | Logs every visible node's kind, rect and text on that surface instance (`bar@eDP-1`) after each layout pass |
| `RUST_BACKTRACE=1` | Run | Adds a backtrace to a logged panic |
| `__EGL_VENDOR_LIBRARY_DIRS`, `__EGL_VENDOR_LIBRARY_FILENAMES` | Run | Your own EGL vendor choice. When neither is set and every GPU uses the `nvidia` driver, the Renderer loads only NVIDIA's vendor |

The Supervisor sets `MANTLE_INSTANCE_DIR`, `MANTLE_GENERATION_ID`, `MANTLE_VERBOSE`,
`MANTLE_PROFILE` and `MANTLE_CHECK` on the Renderer it starts. They are internal; setting them by
hand does nothing useful.

## Binaries

| Binary | Role |
| :--- | :--- |
| `mantle` | The Supervisor (the long-lived process that owns backends and restarts the Renderer) and every command on this page |
| `mantle-renderer` | The Renderer (the process holding the Lua VM and drawing the surfaces; see [runtime](runtime.md#the-vm)). `mantle` starts it from its own directory, one per generation. Run by hand, it prints a notice and exits 2 |

Both must come from the same build. After rebuilding one, rebuild both.

## Which config and which shell

The config directory resolves in this order, once at startup. Symlinks are resolved then, so
retargeting one later does not move a running shell.

| Order | Source |
| :--- | :--- |
| 1 | `-c` / `--config` |
| 2 | `$MANTLE_CONFIG_DIR`, naming the directory itself |
| 3 | `$XDG_CONFIG_HOME/mantle`, when `$XDG_CONFIG_HOME` is absolute |
| 4 | `$HOME/.config/mantle` |

Several shells can run at once. Each running `mantle` holds an **instance directory**,
`$XDG_RUNTIME_DIR/mantle/<pid>-<start ms>/`, with its control socket, its log (`shell.log`), the
config path it runs, and a lock that marks it as running. `mantle list` prints its name under `DIR`.
Client commands pick one:

| Command | `--pid` | `-c` | Neither |
| :--- | :--- | :--- | :--- |
| `set`, `toggle`, `call` | That running shell | The newest running shell on that config, else an error | The newest running shell on the default config, else the newest running shell of any config |
| `log` | That shell, running or stopped | The newest running shell on that config, else its last stopped run | The newest running shell, with a note when several are running, else the last stopped run this login |

A stopped run's log stays until logout clears `$XDG_RUNTIME_DIR`. `mantle log` says so when it
prints one.

## Values and arguments

`set`, `toggle` and `call` read each value as JSON when it parses, else as a plain string.

| Typed | Arrives in Lua as |
| :--- | :--- |
| `true`, `false` | boolean |
| `3`, `-5`, `0.1` | number. A value starting with `-` is a value, not a flag |
| `notifications` | string: not JSON, so taken as is |
| `'"true"'`, `'"3"'` | string, because the JSON quotes survive the shell's |
| `'[1,2]'`, `'{"a":1}'` | table |
| `null` | `nil` |

`call` passes any number of arguments to the handler in order. Its output:

| Handler returns | `mantle call` prints | Exit |
| :--- | :--- | :--- |
| `nil` or nothing | Nothing | 0 |
| A string | The string, unquoted | 0 |
| Any other value | JSON | 0 |
| Raises, blows the 5 ms budget, is not declared, or returns over 1 MiB | `` `name` failed: <reason> `` on stderr | 1 |
| No answer within 5 s | A timeout message. The call may still have run | 1 |

The handler contract is in [action](scripting.md#action).

`set` and `toggle` wait for the shell to apply the write, up to 5 s like `call`. The shell refuses
a write to an undeclared name, a bare `toggle` on a non-boolean, or a value that fails the
[scalar checks](runtime.md#limits-and-budgets). A refusal prints `` state `name` refused: <reason> ``
on stderr, exits 1 and is also a warning in `mantle log`. What `toggle <name> <value>` compares and restores follows
[named state](signals.md#named-state): scalars compare by value (`1` equals `1.0`), and a table
never equals, so toggling to a table always sets it.

## What check covers

`mantle check` evaluates `shell.lua` and its `require`s exactly as a start does, with no
Wayland, no GPU, and every capability reading `nil`. Then it lays every surface out once with the
real layout code, on one 1920x1080 output plus one per `monitor` name a panel pins. It prints
`<path>: ok, N surface(s)` and one `<role> <id>` line per surface, preceded by anything the config
`print`ed. A layout error prints as `<path>: layout: <error>` and exits 1. With more than one
broken node, `<error>` is `N nodes failed:` and then one node per line: the first 20, then
`and N more`. A mistake repeated on every output, or by every item of a `list`, is listed once.

| Caught | Not caught |
| :--- | :--- |
| Lua syntax errors, in any required module | Handler errors: `on_click`, `on_change`, `action`, `timer` never fire |
| Runtime errors at the top level of `shell.lua` and its modules | Branches that only show with capability data, since every capability is `nil` |
| A top-level return that is not surfaces, including `require`'s second value | `process.run` output: commands are queued and never run |
| Surface and node properties: unknown names, wrong value types, bad colours, out-of-range sizes | Fonts, images, shaders and the compositor's response |
| Errors in `:map`, `computed`, list `itemfn`s and function `child` builders, as laid out with `nil` capabilities | Sizes that only fail on a smaller or scaled output |
| More than one `lock`, and a missing `shell.lua` | |

When the stubs `mantle init` wrote differ from this `mantle`, `check` also prints one line asking
you to run `mantle init` again.

It starts no programs and writes no state. On failure it prints only the error, not the config's
`print` output.

## Exit codes

| Code | When |
| :--- | :--- |
| 0 | Success. For `set` and `toggle`: the shell applied the write |
| 1 | The command failed. It prints the reason on stderr: no shell running, no shell with that `--pid`, `XDG_RUNTIME_DIR` unset, the socket unreachable, no log this login, `check` found an error, `call` failed or timed out, `set` or `toggle` was refused or timed out, `-d` could not start the shell (not running within 5 s, or it exited), `init` could not write a file |
| 2 | Bad arguments: unknown flag, missing name or value, a non-numeric `--pid`, `--profile=0`, a flag the command does not take, `--pid` with `-c`, `-c` with `list`. It prints `mantle: <reason>` and the help text. Also `mantle-renderer` run by hand |

## How do I…

**…wire a keybind?** Declare a `state` or an `action`, then bind the command in the compositor, as
in the [example at the top](#cli). Use `set`/`toggle` to change what is shown, `call` to make the
shell act. niri's `repeat=false` keeps a held key from toggling repeatedly.

**…start or stop the shell?** Start it from the compositor ([run the shell](installation.md#run-the-shell)),
which gives it `XDG_RUNTIME_DIR` and the Wayland socket. From a terminal, `mantle -d` starts it
and gives the prompt back. Stop it with Ctrl-C in the foreground, or `kill <pid>` with the pid
`mantle list` shows.

**…read the logs?** `mantle log` prints the whole log of the current shell. `mantle log -f` follows
it. Errors raised in callbacks are warnings, so they show by default. For more detail, restart with
`mantle -v` (info) or `mantle -vv` (debug). `mantle log | grep 'renderer/config: '` keeps only the config's own `log.*`
lines.

**…debug a reload that did nothing?** Run `mantle check`, then `mantle log`. The full sequence is
in [runtime](runtime.md#find-out-why-a-reload-did-nothing).

**…target one of two running shells?**

```text
$ mantle list
PID     UPTIME  DIR                    CONFIG
4120    2h13m   4120-1727170000000     /home/me/.config/mantle
9051    41s     9051-1727177900000     /home/me/src/mantle-test
$ mantle --pid 9051 toggle launcher_open
$ mantle -c ~/src/mantle-test call volume.up
$ mantle log --pid 9051 -f
```

**…try a config without touching the running shell?** `mantle check -c ~/src/mantle-test` first,
then `mantle -c ~/src/mantle-test` starts a second shell on it. Its surfaces draw beside the
first shell's, so give them other ids or anchors, and address it with `-c` or `--pid`.

**…see what a keybind can reach?** `mantle call` lists the actions and `mantle set` the states
with their values. Both print nothing when the config declares none, so they pipe into a picker:
`mantle call | fzf | xargs mantle call`.

```text
$ mantle set
launcher_open	false
modal	"settings"
```

**…set a string that looks like a number or boolean?** Quote it as JSON: `mantle set label '"42"'`.

**…use an action's answer in a script?** `volume=$(mantle call volume.up)` captures the printed
value, and a non-zero exit means it failed.

**…see which node has the wrong size?** Run `MANTLE_DUMP_LAYOUT=bar@eDP-1 mantle -vvv` and read
`mantle log`. The instance id is the surface `id`, `@`, and the output name.

**…get completion and type checking in an editor?** [Set up a config](installation.md#set-up-a-config).

## Gotchas

| Trap | Fix |
| :--- | :--- |
| `mantle set label true` stores a boolean, `mantle set count 3` a number | Quote JSON strings: `mantle set label '"true"'` |
| A keybind does nothing and the terminal shows no error | The compositor discards the command's stderr. Run it in a terminal, or `mantle log` and look for `asked to write state` |
| `mantle toggle modal` is refused on a string state | A bare toggle needs a boolean. Pass the value: `mantle toggle modal settings` |
| `mantle call x` says no action exists after a broken save | A failed reload clears actions. Fix the config and save ([runtime](runtime.md#evaluation-reload-and-generations)) |
| Two bars on screen | Two shells are running. `mantle list`, then stop one |
| `mantle -c dir list` is refused | `list` shows every config's shells; drop `-c` |
| `mantle log -f` exits at once | That shell has stopped. The command printed its last run |
| `XDG_RUNTIME_DIR is not set` | The command runs in an environment without it. Start the compositor from a proper login session |

See also: [runtime](runtime.md) · [named state](signals.md#named-state) ·
[action](scripting.md#action) · [capabilities](../capabilities/index.md) · [glossary](../glossary.md) for
Supervisor, Renderer and generation.

Source: [argument parsing](../../supervisor/src/cli.rs), [commands](../../supervisor/src/main.rs),
[shell selection](../../supervisor/src/instance.rs), [set/toggle/call client](../../supervisor/src/control_client.rs),
[state writes](../../renderer/src/lua/signal/globals.rs), [check](../../renderer/src/check.rs),
[init](../../supervisor/src/setup.rs), [log](../../supervisor/src/log.rs), [levels](../../shared/src/log.rs).
