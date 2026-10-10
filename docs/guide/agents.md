# Working with coding agents

A coding agent can edit a Mantle config, check it, watch it reload and drive the result, all from a
shell, without a screenshot or a human at the keyboard. Every step below is a command from the
[CLI](cli.md); none is agent-specific.

## The loop

| Step | Command | Touches the session? |
| :--- | :--- | :--- |
| 1. Discover | `mantle call`, `mantle set`, `mantle list` | Reads only |
| 2. Extend | Edit `shell.lua` | No |
| 3. Validate | `mantle check` | No: no Wayland, no GPU |
| 4. Apply | Save; `mantle log -f` | Yes: live reload |
| 5. Drive | `mantle call`, `toggle`, `set`, `input` | Yes |

### 1. Discover

With a shell running:

```text
$ mantle list
PID     UPTIME  DIR                    CONFIG
4242    3m      4242-1760000000000     /home/me/.config/mantle
$ mantle call
volume.up
$ mantle set
launcher_open	false
modal	""
```

`mantle call` with no name prints each `action` the config declares, one per line, sorted.
`mantle set` (or `toggle`) with no name prints each `state` as `name<TAB>JSON`; strings are quoted,
so the line can be passed back as `mantle set <name> <value>`. `mantle list` shows running shells,
oldest first. After a failed reload, both lists describe what the shell still runs
([reload rules](runtime.md#evaluation-reload-and-generations)).

Run `mantle init` once. It writes `.luarc.json` and, unless packaged stubs exist, the type stubs to
`$XDG_DATA_HOME/mantle/lua-meta`, so lua-language-server shows the agent the exact API names,
property names and capability fields instead of guesses.

### 2. Extend

Declare a [`state`](signals.md#named-state) for something the UI shows and an
[`action`](scripting.md#action) for something the shell does. Both are reachable from the command
line, so the agent can test them before wiring a key:

```lua
local count = state("clicks", 0)

action("clicks.bump", function(by)
    local n = count:get() + (tonumber(by) or 1)
    count:set(n)
    log.info("clicks is now", n)
    return n
end)

return panel {
    id = "counter",
    layer = "top",
    anchor = { top = true, left = true },
    width = 120,
    height = 32,
    child = text { content = count:map(function(n) return "clicks: " .. n end) },
}
```

`mantle call clicks.bump 2` prints the return value: a string bare, anything else as JSON, nothing
for `nil` ([call output](cli.md#values-and-arguments)). A raise prints `` `name` failed: <reason> ``
on stderr and exits 1.

The Lua logging functions are `log.error`, `log.warn`, `log.info` and `log.debug`
([log](scripting.md#log)). They print at every level, as `HH:MM:SS LEVEL config: message` with
the arguments tab-joined. `print` also lands in `mantle log`, unstamped.

### 3. Validate

```text
$ mantle check
/home/me/.config/mantle/shell.lua: ok, 1 surface(s)
  panel   counter
```

`mantle check` runs the same evaluation and layout code as a start, with no Wayland, no GPU and
no subprocesses, so it is safe while the real shell runs. It lays every surface out in four passes
(capabilities `nil`, sample data, alternate sample data, empty lists), which catches the
missing-data and empty-list branches an agent rarely reads back. It exits 0 when clean and 1 on an
error, printing `<config dir>: <error>`, with `file:line` and, for a mistyped property, a
suggestion:

```text
/home/me/.config/mantle: before capability data: layout: invalid value for `child`: on `counter@DP-1`: panel (shell.lua:15) > shell.lua:16: `text` has no property `contnet`; did you mean `content`?
```

Details: [What check covers](cli.md#what-check-covers). `-c DIR` checks another directory.

### 4. Apply

Saving any `.lua` or `.frag` file under the config directory reloads the running shell 200 ms
after the last write. Other extensions do not. Read the result:

```text
$ mantle log -f
12:01:07 NOTICE renderer/socket: shell reloaded
```

A reload that raises or whose scene is rejected is logged as an error, and the previous scene
stays on screen ([failure table](runtime.md#evaluation-reload-and-generations)). It also drops the
old evaluation's actions, so `mantle call` after a failure may not find a name that worked a
minute ago. The fix is to correct the file and save again.

### 5. Drive

| Goal | Command |
| :--- | :--- |
| Run an action | `mantle call clicks.bump 3` |
| Flip or set a state | `mantle toggle launcher_open`, `mantle set modal settings` |
| Click | `mantle input counter click 40 16` |
| Drag, scroll | `mantle input bar drag 10 10 90 10`, `mantle input bar wheel 40 16 -3` |
| Keyboard | `mantle input search key ctrl+a`, `mantle input search type "hello"` |

`mantle input <SURFACE> <VERB>` goes through the same hit testing, hover, `on_click`, `on_drag`
and `textfield` code as a real pointer, in the surface's logical pixels, and never moves the real
cursor or takes compositor focus. `<SURFACE>` is the declared `id`, or `id@output` when one id
has an instance per output. Lock surfaces and `secure_submit` fields refuse input. All verbs:
[Injecting input](cli.md#injecting-input).

With several shells running, `--pid <pid>` (from `mantle list`) picks one, and `-c <dir>` picks
the newest on that config. They cannot be combined. The
[selection rules](cli.md#which-config-and-which-shell) cover the default.

## Instructions to paste

For `CLAUDE.md`, `AGENTS.md` or the equivalent:

```text
This repo is a Mantle shell config (Lua). Docs: https://anasgets111.github.io/mantle/
- Run `mantle init` once; read the stubs it points `.luarc.json` at for exact API names before guessing.
- Before saving a change to shell.lua, run `mantle check`; it must exit 0.
- After saving, run `mantle log` and look for "shell reloaded"; any error line means the old scene is still on screen.
- Add `action("name", fn)` for behavior you want to test. Run it with `mantle call name [args]`.
- Use `mantle set`/`toggle` and `mantle input <surface> <verb>` to exercise the UI. Do not run `mantle stop` or start a second shell.
- Use log.info/log.warn, not print, for debug output.
```

Source: [CLI](../../supervisor/src/main.rs), [check](../../renderer/src/check.rs),
[log](../../renderer/src/lua/log.rs).
