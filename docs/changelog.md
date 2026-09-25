# Changelog

User-facing changes to the Lua API and the `mantle` CLI. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Mantle is pre-release and unversioned,
so everything since the rename from Obelisk sits under Unreleased.

## Unreleased

### Added

- This site: guide, one page per node, surface and capability, cookbook, glossary. Every Lua
  example on it runs as a test, and the `lua-meta` stubs link each entry to its page.
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
- A bare `mantle call` lists the running config's actions, and a bare `mantle set` or `mantle toggle`
  its states with their values ([CLI](guide/cli.md#commands)).

### Changed

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
