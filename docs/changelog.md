# Changelog

User-facing changes to the Lua API and the `mantle` CLI. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Each version is tagged `vX.Y.Z`
and published on [GitHub releases](https://github.com/anasgets111/mantle/releases). While the
version is 0.x, a minor release can break the Lua API.

## Unreleased

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
