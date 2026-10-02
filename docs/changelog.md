# Changelog

User-facing changes to the Lua API and the `mantle` CLI. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Each version is tagged `vX.Y.Z`
and published on [GitHub releases](https://github.com/anasgets111/mantle/releases). While the
version is 0.x, a minor release can break the Lua API.

## Unreleased

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
