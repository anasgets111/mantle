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

- `translate`, `scale`, `rotate` and `origin` tweens repaint without relayout.
- Hover callbacks fire on pointer entry.
- An equal capability snapshot is not pushed again, except `tray` and `notifications`.
- A bad `layer`, `corner_shape`, `keyboard_interactivity`, popup `anchor` or `gravity`,
  `constraint_adjustment` entry or easing name fails with one wording that lists every choice:
  `` expected one of `Background`, `Bottom`, `Top`, `Overlay`, got … ``.
