# Changelog

User-facing changes to the Lua API and the `mantle` CLI. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Each version is tagged `vX.Y.Z`
and published on [GitHub releases](https://github.com/anasgets111/mantle/releases). While the
version is 0.x, a minor release can break the Lua API.

## Unreleased

- `dofile` and `loadfile` are unavailable in configs because their synchronous file reads can stall the Renderer. Use `require` for Lua modules or `process.run` for other files.
- `accessible_name` makes clickable nodes keyboard focusable and names them for screen readers. Tab and Shift+Tab traverse controls; Enter and Space activate them. The engine outlines a control only when Tab or an assistive-technology action focused it; `focus_ring = false` turns the outline off, and `focused(name)` with a node's `focused` reports focus within a node for custom styles. Mantle exposes the resolved scene through AT-SPI, with secure field values withheld. Breaking: with two or more focusable controls on a surface, Tab moves focus and no longer reaches a textfield's `on_navigate("tab")`.
- Borders follow the corners: a per-edge `border_width` or `border_color` on a rounded box, and any border on a `corner_shape = "Scoop"` box, used to draw as four straight rectangles with square corners. Where two edges meet, the colour change sits on the corner in proportion to their widths, as in CSS.
- `path` `animate`: `commands` tweens point by point between lists with the same ops and `hole` flags, including as `keyframes`, so shapes morph without Lua per frame. Any other change still snaps.
- `path`: `A` draws a circular arc from centre, radius, start and sweep in degrees, and `hole = true` on a subpath's first command cuts it out of the fill. Every other subpath is now solid regardless of winding, so an inner subpath drawn in the opposite direction no longer cuts a hole.
- `shader` and `transition` `params`: a list of up to 4096 numbers fills a `float` or `vec2`-`vec4` uniform array, so a shader can draw a 256-bar visualizer.
- Surfaces paint at their output's compositor scale, fractional included, so text and images are sharp on HiDPI displays. Sizes stay in logical pixels, and a `shader`'s `size` uniform stays logical while `gl_FragCoord` counts buffer pixels.
- `image` `async`: a resized image keeps drawing its previous size, scaled, until the new size decodes, instead of blanking. A changed `source_blur`, or a changed `source` without `retain`, still blanks.
- `idle`: `on_resume` now receives `"input"`, `"activity"`, or `"inhibitor"`, so configs can wake displays only for input while stopping idle stages for other resumes.

## 0.1.0 - 2026-10-01

First release. Mantle is an engine for Wayland desktop shells declared in Lua; it ships no shell of
its own beyond the one-clock starter bar.

- [Surfaces](surfaces/index.md): panels, popups, windows and lock screens.
- [Nodes](nodes/index.md): rects, rows, columns, text, text fields, images, icons, lists, paths,
  shaders and capture.
- [Capabilities](capabilities/index.md): 24 desktop services, among them audio, network,
  Bluetooth, notifications, tray, MPRIS, workspaces, idle, updates and secrets.
- [Signals](guide/signals.md) update widgets when values change; saving the config reloads the
  shell.
- `mantle check` catches Lua and layout errors before a run; `mantle init` sets up LuaLS
  completion and type checking.
- Packages: `.deb` for Ubuntu 26.04+, `.rpm` for Fedora 44+, and an x86_64 tarball
  ([installation](guide/installation.md)).
