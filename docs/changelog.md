# Changelog

User-facing changes to the Lua API and the `mantle` CLI. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Mantle is pre-release and unversioned,
so everything since the rename from Obelisk sits under Unreleased.

## Unreleased

### Added

- Every node kind takes `on_click`, `on_drag`, `on_wheel` and `submit`: a `row`, `text` or `icon` is clickable itself, the innermost node with a handler wins, and it shows the `"pointer"` cursor ([pointer](guide/input.md#pointer)).
- `panel`, `window` and `popup` take `reset_on_close = { ... }`: the listed `state` handles return to their `initial` and `scroll` handles to the top when the surface stops being shown ([surfaces](surfaces/index.md#reset-on-close)).
- `state(...)` signals take `:on_change(fn(current, previous))`, run after any write that changes the value, including `mantle set`/`mantle toggle`, before the next layout pass ([signals](guide/signals.md#on_change-react-to-a-write)).
- `focus(name)` lets an `on_click` return typing to a visible plain `textfield` on its keyboard-focused surface, or a popup under it, without clearing the draft or caret ([input](guide/input.md#text-fields)).
- `mantle.sysinfo` exposes `disks`, `gpu`, and network throughput telemetry (`net_rx_bytes_sec`, `net_tx_bytes_sec`), configurable via `disk_interval`, `gpu_interval`, and `net_interval` in `sysinfo:configure` ([sysinfo](capabilities/sysinfo.md)).
- `mantle.windows:move_to_workspace(id, workspace_id)` moves a window to a workspace on Hyprland and niri ([windows](capabilities/windows.md#actions)).
- `mantle.workspaces` entries and `special` workspaces expose `window_id` for their representative window ([workspaces](capabilities/workspaces.md#workspaceentry)).
- `text` nodes accept `line_height` (font-size multiplier, default `1.2`), `letter_spacing` (pixels, default `0`), `font_weight` (1–1000, default `400`), and `italic` (default `false`).
- Ctrl+C copies a plain textfield selection; Ctrl+V pastes up to 64 KiB of UTF-8 into plain or secure fields ([input](guide/input.md#text-fields)).
- `secure_submit = { capability = "secrets", action = "store", name = "…" }` stores a named secret through the session Secret Service. `mantle.secrets.entries[name]` reports `pending`, `stored`, `unavailable`, or `timed_out` ([secrets](capabilities/secrets.md)).
- `mantle.idle.compositor_hold_stale` marks a previously observed compositor hold as unconfirmed
  while input continues; unconfirmed compositor holds are omitted from `inhibitors` until observed again
  ([idle](capabilities/idle.md#state)).
- `mantle.mpris` exposes bounded TrackList and Playlists state with track and playlist actions.
- `mantle.mpris` exposes capability flags and `control(id, "stop")`, Raise, Quit, OpenUri, volume, loop, shuffle, and rate controls.
- `mantle.mpris.players` exposes album, album artist and genre metadata.
- x86_64 release builds: a `.deb`, an `.rpm` and a tarball, built in Ubuntu 26.04 by the `release`
  workflow on a `v*` tag ([install](guide/installation.md#install)).
- This site: guide, one page per node, surface and capability, cookbook, glossary. Every Lua
  example on it runs as a test, except `fragment` blocks (parsed only) and `no-check` ones, and
  the `lua-meta` stubs link each entry to its page.
- Nodes: [`shader`](nodes/shader.md) (a config fragment shader) and [`capture`](nodes/capture.md)
  (a live output preview, with `live` capped at a frame rate and `region` to crop).
- Paint: gradient backgrounds, `mask`, `shadow_*` with `shadow_mode`, `content_blur`,
  `backdrop_blur`, `image.source_blur`, `clip = "None"` and `z` ([paint](guide/paint.md)).
- [`palette.quantize`](guide/scripting.md#palettequantize) for an image's dominant colours.
- `mantle.screens` entries gain position, `model`, `description`, `fractional_scale` and
  `orientation`; `mantle.screens` and `mantle.rescue` gain `on_change`.
- `monitor = "Active"` lets the compositor pick a panel's output.
- A plain `textfield` has a caret, a selection, grapheme-correct deletion, key repeat and Ctrl
  bindings; Left/Right reach `on_navigate` when the caret cannot move; Escape in a
  `secure_submit` field calls `on_cancel`.
- Numbered verbosity (`-v`, `-vv`, `-vvv`), quiet by default; `--profile` prints its own reports ([CLI](guide/cli.md#flags)).
- `--profile` reports each capability's snapshot pushes sent and deduped beside its size ([CLI](guide/cli.md#flags)).
- A bare `mantle call` lists the running config's actions, and a bare `mantle set` or `mantle toggle`
  its states with their values ([CLI](guide/cli.md#commands)).
- `mantle.system:configure({ interval = 60 })` pushes the clock every minute on the minute
  instead of every second, or never with `0`; default `1` ([system](capabilities/system.md)).
- `mantle stop [--pid | -c]` stops a running shell and waits for it to exit; `mantle.pid` is the
  shell's own pid, so a config can stop itself ([CLI](guide/cli.md#commands)).
- `mantle.updates` runs on Fedora through `dnf` (dnf5 or dnf4) and on Debian and Ubuntu through
  `apt-get`; `package_manager` is `"dnf"` or `"apt"` ([updates](capabilities/updates.md#backend)).
- `mantle.tray:context_menu(id, x, y)` calls the StatusNotifierItem `ContextMenu` method for applications without a DBusMenu ([tray](capabilities/tray.md)).

### Changed

- Node `padding` values outside `[0, 8192]` now fail layout validation. Use a negative `margin` for overflow.
- A write to a signal bound to a `list` item's property, such as `background = theme.bg`, runs that
  property's getter in each item and no longer calls `itemfn` again; `itemfn` runs again only when
  the source, `key`, or something `itemfn` itself read changed ([list](nodes/list.md#when-items-rebuild)).
- A capability action with a wrongly typed or invalid argument raises at the call (`mantle.audio:set_volume: invalid type: …`) instead of logging in the Supervisor ([actions](capabilities/index.md#actions)).
- `brightness:set` and `keyboard:set_backlight` take any number: fractions round to the nearest raw step, negatives clamp to `0` (before, a float or negative was dropped) ([brightness](capabilities/brightness.md)).
- `session_process` raises on an unknown `stop_signal` instead of the Supervisor dropping the declaration ([processes](guide/processes.md)).
- `mantle set` and `mantle toggle` refuse a state a reload removed, exit 1 with "declares no state" ([cli](guide/cli.md)).
- A held Escape clears a textfield once instead of repeating ([input](guide/input.md#text-fields)).
- An eased `animate` tween reversing toward its prior endpoint takes a shorter run, including after repeated reversals ([animation](guide/animation.md#how-a-tween-starts)).
- `mantle.sysinfo` temperature probe recognizes Intel GPU hwmon chips named `i915` or `xe` for `temp_gpu` ([sysinfo](capabilities/sysinfo.md#backend)).
- The Lua CPU budget is tightened to 2.5 ms per evaluation (scaled for 144 Hz and 240 Hz displays). Computeds rerun on invalidation hand their memoized values over to the layout pass, running each computed body once per state push.
- Text shaping uses Parley. Text widths and wrap points can shift slightly; glyphs still draw
  through FemtoVG with Swash hinting.
- `mantle.tray` DBusMenu parsing drops invisible menu items (`visible = false`), spools raw PNG `icon-data` to shared memory as `icon_name` file paths, and refreshes live items on `ItemsPropertiesUpdated` and `NewMenu` ([tray](capabilities/tray.md)).
- `mantle.updates` checks through `pacman`, `pacman-conf`, `curl` and `vercmp` instead of
  linking libalpm, so building no longer needs libalpm and the binary starts off Arch. `packages`
  leaves out `IgnorePkg` entries, and `installed_size` rounds to the two decimals pacman prints
  ([updates](capabilities/updates.md#backend)).
- A `geometry` rect an animation moved schedules one pass when the animation settles, so a
  property bound to it catches up instead of waiting for an unrelated write.
- A `state:set` of the value the state already holds re-resolves nothing, and a `:map` or
  `computed` re-resolves its readers only when its result changes. Scalars and plain-data tables
  compare by value ([signals](guide/signals.md#derived-signals)).
- `mantle.system` pushes land on the wall-clock second after a resume or clock step too, rather
  than mid-second until a restart.
- `mantle.keyboard` pushes once when it starts, so it is no longer `nil` without niri or Hyprland
  until a lock key or the backlight changes.
- `mantle.sysinfo` reads on the wall-clock second, first at the next one after `configure`
  rather than one interval later, so its pushes can share `mantle.system`'s layout pass.
- `mantle.applications` watches its directories and rescans after a change, so installs and
  removals appear without `refresh` ([applications](capabilities/applications.md)).
- A reload kills every `process.run` child and calls its `exit_cb(nil)` before the new evaluation
  runs, so a top-level follower restarts instead of doubling ([processes](guide/processes.md#which-one-do-i-use)).
- Lua's `warn` now logs at warn level like `log.warn`, on by default; before, it printed nothing.
- A `text.content` run's `kind` must be `"text"`, as a notification text span's is; any other value
  fails the pass instead of being ignored.
- `lua-meta` node, surface and global types come from the Rust types the engine parses with: a surface's
  `child` declares the `Bound` it always took, a panel's `width`/`height` the `[0, 8192]` it always
  enforced. A signal-bound popup `parent` fails every pass, not only the first.
- A callback (`on_*`) that is not a function (write `cond and fn or nil`), a `submit` or
  `autofocus` that is not a boolean, and a `nil` or `false` entry in `children`, which dropped
  every child after it, fail the pass instead of being ignored.
- A table property (`padding`, `anchor`, `shadow_offset`, `min_size`, `anchor_rect`, an `animate`
  entry, a text run, `secure_submit`, ...) and the `session_process`, `persistent_table` and
  `palette.quantize` option tables refuse a key they do not take.
- `secure_submit` takes only `lock`/`authenticate`, `polkit`/`authenticate` and `network`/`connect`.
- Two surfaces with one `id`, or two different scalar seeds for one `state` name in one
  evaluation, fail the evaluation.
- A `list` without `source` builds no items instead of raising.
- A `list` keeps its items while nothing its last build read has changed, instead of calling
  `itemfn` for every item on every pass: a write elsewhere on its surface costs about half as much.
  `itemfn`, `key` and the maps inside items must read time and mutable data through signals, or
  they show what they read at the last build ([when items rebuild](nodes/list.md#when-items-rebuild)).
- A node reads each `children` or `child` table once and keeps it while it holds that table, so a
  pass's property reads outside `list` builds cost about 40% less. A node table or `children` array
  changed in place is no longer seen; bind a signal or `:set` a new table ([gotchas](nodes/index.md#gotchas)).
- A node keeps the properties it resolved until a signal they read is written, instead of running
  every getter on its surface on every write: a clock tick re-reads the clock's node, not the bar.
  A map, `computed` or function `child` that reads `os.date()` with no time, `os.time()`, a mutable
  variable or a file keeps its last answer until a signal it read changes. Derive time from
  `mantle.system`: `mantle.system:map(function(s) return s and os.date("%H:%M", s.time) or "" end)`
  ([what a node reads again](guide/signals.md#what-a-node-reads-again)).
- A write only one `list` item read, in its `itemfn` or a property such as `hover`, reaches that item
  alone instead of building every item:
  4.5 ms instead of 16 ms a pass on 500 rows ([when items rebuild](nodes/list.md#when-items-rebuild)).
- A wheel over a container whose `scroll` signal no getter, `map` or `list` build reads moves its
  children without a layout pass: 0.06 ms instead of 2.4 ms on 500 rows.
- **Breaking:** each capability action is a method, and `:invoke` is gone:
  `mantle.audio:set_volume(0.5)` replaces `mantle.audio:invoke("set_volume", 0.5)`. The editor
  stubs type each action's own arguments, so `mantle.audio:set_muted(0.5)` is flagged. An unknown
  name raises, listing the actions the capability has, as does any action on a capability with
  none; a `.` call in place of `:` raises instead of sending a wrong argument
  ([actions](capabilities/index.md#actions)).
- A state field read off a capability raises `did you mean mantle.audio:get().volume?`, and a
  misspelled action names the one it is close to.
- A misspelled node or surface property raises "did you mean `content`?" instead of listing every
  property the kind takes; the list stays for a key close to none.
- `mantle set` and `mantle toggle` wait for the shell and exit 1 with its reason when it refuses
  the write ([CLI](guide/cli.md#values-and-arguments)).
- A tween advances on its own surface's frame callbacks, so a surface animates at its output's
  refresh rate instead of the fastest animating output's ([animation](guide/animation.md)).
- `mantle check` lays the config out on stand-in outputs and fails on a layout error, and says
  when the stubs `mantle init` wrote are out of date.
- A pass that fails names every broken node, one per line, in `mantle check`, the log and
  `mantle.rescue`, instead of stopping at the first. It lists 20, then counts the rest
  ([what check covers](guide/cli.md#what-check-covers)). A `children` entry that is not a node
  reads `children[1]: expected a node table`, counted from 0 like the rest of the path.
- `mantle check` lays out a second time after one sample push per capability, every list one entry
  long, so an error in a list `itemfn` or a data-only branch fails the check; each error names its
  pass ([what check covers](guide/cli.md#what-check-covers)).
- Errors name files relative to the config directory (`widgets/bar.lua:4`, not a path Lua cut to
  `...2b41-2949-.../bar.lua:4`), including `require`d modules. A layout error's path names the
  line that built each node (`row[0] (shell.lua:7) > ...`), a failing `:map` or `computed` the line
  that created it (`signal created at shell.lua:3`), and tracebacks drop the engine's own frames.
  `mantle check` prints the config directory once instead of `shell.lua: shell.lua failed to
  evaluate` ([CLI](guide/cli.md#what-check-covers)).
- The `.luarc.json` from `mantle init` warns on unused locals and on the `type-check`,
  `unbalanced`, `strict` and `global` diagnostic groups in every file.
- `mantle.rescue` is set when a reload, or a live update, fails to apply, and clears only when a
  scene applies. Errors raised in callbacks, failed spawns and failed `mantle call`s are warnings,
  and a missing icon or undecodable image warns once per name.
- The editor stubs flag a misspelled property or table key and a percent that is not a whole
  `"0%"` to `"100%"`, and type `children` as taking a signal, as the engine does.
- The editor stubs type a capability's `:get()` as `T?`, so an unguarded read of a field warns;
  `mantle.screens` and `mantle.rescue` stay non-nil. An `animate` key the node does not take (or
  `z`), and an unknown key in an `animate` entry, `{ steps = n }`, `session_process`,
  `persistent_table` or `palette.quantize` options, are flagged. Each node's `animate` is typed by
  its own alias (`RectAnimations`, `TextAnimations`, ...); a wrapper that passes `animate` through
  types it with that alias. A capability or `scroll(...)` handle passes where a `Signal` or a
  `scroll` property is declared.
- `translate`, `scale`, `rotate` and `origin` tweens repaint without relayout.
- Hover callbacks fire on pointer entry.
- A `nil` or non-signal `computed` dependency raises naming its index,
  `` computed() dependency 2 is nil; ... ``; before, a hole dropped every dependency after it. A
  named key in the list raises too.
- `timer`'s `ms` is a `number`: `timer(1.5, fn)` runs, and `timer(-1, fn)` or a NaN raises
  `timer(-1) is outside 1..=86400000 milliseconds`, not mlua's `error converting Lua integer to u64`.
- `mantle.idle:register_threshold` outside `1..=4294967` seconds raises naming the range; before,
  a negative one got mlua's conversion error and a huge one was clamped. `cancel_threshold(-1)` is a
  no-op like any unknown handle.
- A `nil` in the returned surface list raises `surface 2 is nil: ...`; before, it dropped every
  surface after it. A named key in the list (`return { bar, cfg = x }`) raises instead of being
  ignored.
- An equal capability snapshot is not pushed again, except `tray` and `notifications`.
- A bad `layer`, `corner_shape`, `keyboard_interactivity`, popup `anchor` or `gravity`,
  `constraint_adjustment` entry or easing name fails with one wording that lists every choice:
  `` expected one of `Background`, `Bottom`, `Top`, `Overlay`, got … ``.

### Removed

- **Breaking:** `button` is removed; put `on_click`, `on_drag`, `on_wheel` or `submit` on a `rect`, `row`, `column` or any node. Calling `button { ... }` raises `button was removed: put on_click on a rect, row or column` ([pointer](guide/input.md#pointer)).
