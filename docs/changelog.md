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

### Changed

- A reload kills every `process.run` child and calls its `exit_cb(nil)` before the new evaluation
  runs, so a top-level follower restarts instead of doubling ([processes](guide/processes.md#which-one-do-i-use)).
- Lua's `warn` now logs at warn level like `log.warn`, on by default; before, it printed nothing.
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
- `mantle.<name>:invoke` raises on an action name the capability does not have, listing the
  ones it has, and on a capability with no actions, instead of logging and dropping the command.
- `mantle set` and `mantle toggle` wait for the shell and exit 1 with its reason when it refuses
  the write ([CLI](guide/cli.md#values-and-arguments)).
- `mantle check` lays the config out on stand-in outputs and fails on a layout error, and says
  when the stubs `mantle init` wrote are out of date.
- The `.luarc.json` from `mantle init` warns on unused locals and on the `type-check`,
  `unbalanced`, `strict` and `global` diagnostic groups in every file.
- `mantle.rescue` is set when a reload, or a live update, fails to apply, and clears only when a
  scene applies. Errors raised in callbacks, failed spawns and failed `mantle call`s are warnings,
  and a missing icon or undecodable image warns once per name.
- The editor stubs flag a misspelled property or table key and a percent that is not a whole
  `"0%"` to `"100%"`, and type `children` as taking a signal, as the engine does.
- `translate`, `scale`, `rotate` and `origin` tweens repaint without relayout.
- Hover callbacks fire on pointer entry.
- An equal capability snapshot is not pushed again, except `tray` and `notifications`.
- A bad `layer`, `corner_shape`, `keyboard_interactivity`, popup `anchor` or `gravity`,
  `constraint_adjustment` entry or easing name fails with one wording that lists every choice:
  `` expected one of `Background`, `Bottom`, `Top`, `Overlay`, got … ``.
