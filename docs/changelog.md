# Changelog

User-facing changes to the Lua API and the `mantle` CLI. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Each version is tagged `vX.Y.Z`
and published on [GitHub releases](https://github.com/anasgets111/mantle/releases). While the
version is 0.x, a minor release can break the Lua API.

## Unreleased

- `on_click` takes the pointer as a third argument, `{ x, y }` in node-local coordinates like `on_drag`'s. A keyboard activation passes the node's centre.
- An `icon` with `foreground` recolours every shape of a `-symbolic` icon that never uses `currentColor`, as GTK does, so themes that hard-code symbolic colours (Tela) follow it. Icons that use `currentColor` keep their accents, and full-colour icons are unchanged.
- A stacking parent whose size is fixed or capped below its largest child gives every child its content box, so a smaller child no longer centres in the larger one's width.
- A `text` or `icon` capped by `max_width` or `max_height` sizes its content-sized parent at the cap instead of its full content, so an elided title in a content-sized `row` stays centred.
- `battery.capacity` reports battery health: full charge as a percent of the design capacity, `nil` when UPower does not know it.
- `audio` recovers when PipeWire restarts or starts after Mantle. While PipeWire is down, `audio` reads as a machine with no audio hardware and `privacy` clears its microphone and screencast users.
- `network` rebinds its devices when NetworkManager restarts, so `wifi_devices` and wired state no longer show the old daemon's values.
- The Bluetooth pairing agent rejects calls from anyone but bluetoothd.
- `brightness` picks up a backlight that appears after startup, such as on a dock or a hybrid GPU.
- Images scale down with a bilinear filter instead of box sampling: closer to the source, and a 4K wallpaper scales to 2133x1200 in 8.5 ms instead of 75 ms.

## 0.2.0 - 2026-10-04

- `network.vpns` lists saved NetworkManager VPN and WireGuard profiles with `active` and `activating`; `connect_vpn(uuid)` and `disconnect_vpn(uuid)` toggle one and `vpn_error` reports a failed activation. A NetworkManager secret agent raises `vpn_secret` when an activation needs secrets: answer it with `secure_submit = { capability = "network", action = "vpn_secret", name = request.id .. "/" .. field }` fields, or `cancel_vpn_secret()`.
- `workspaces` entries and `windows` entries have `urgent`: niri reports it as set; Hyprland sets it on an `urgent` event and clears it when the window gains focus or closes. It is always `false` on wlr-foreign-toplevel.
- New `radio` capability: `mantle.radio.radios` lists each rfkill kind with `soft_blocked` and `hard_blocked`; `set_blocked(kind, blocked)` and `set_all_blocked(blocked)` write `/dev/rfkill`.
- Bluetooth `pairing_request` has an `id` and supports `"pin_entry"` and `"passkey_entry"`; answer these with `secure_submit = { capability = "bluetooth", action = "pair", name = request.id .. "/" .. request.mac }`. A present adapter takes over when the tracked adapter is removed.
- `network.wifi_devices` lists Wi-Fi interfaces by name. `scan_device`, `connect_device` and `disconnect_wifi_device` target one; flat join fields describe that join, while other flat Wi-Fi fields and old actions use the primary interface.
- Breaking: an omitted `panel` width or height now fills the configured root axis when both opposite edges are anchored. To preserve a deliberately narrow root, wrap its content in a sized `child` and move its background there.
- Plain `textfield` supports Ctrl+Z undo, Ctrl+Shift+Z and Ctrl+Y redo, with `on_change` for restored drafts.
- Plain `textfield` composes through text-input-v3 where available; preedit stays local until commit. Secure fields do not use an input method.
- `animate.move` eases a matched node from its previous painted position to its new layout position without moving siblings or running Lua on each frame. Use stable `id` or list `key` for items that can change order.
- Input regions, text links and pointer cursors follow ancestor transforms; canvas `Frame.transform` composes nested transforms.
- `audio.sinks[]` and `audio.sources[]` report per-channel `index`, `position` and percent `volume`; `set_sink_channel_volume` and `set_source_channel_volume` set one channel without changing the others.
- `battery.peripherals` lists UPower devices outside the system supply, with charge percentage or coarse level when reported.
- Breaking: every engine-defined string enum is lowercase snake_case, and old spellings are refused: `align_h = "center"`, `width = "fill"`, `layer = "top"`, `keyboard_interactivity = "on_demand"`, `easing = "in_out_quad"`, `loops = "infinite"`, `constraint_adjustment = { "flip_y", "slide_x" }`, `exclusive_zone = "ignore"`, gradients `"linear"`/`"radial"`/`"conic"`. Capabilities read the same way: `battery.state` (`"fully_charged"`, `"pending_charge"`, ...), mpris `play_state` (`"playing"`), `loop_status` and `set_loop_status` (`"none"`, `"track"`, `"playlist"`). Panel `output` keywords are `"all"` and `"active"`; `"All"` now names a connector and logs that none is connected. POSIX signal names and SVG path ops keep their case.
- Breaking: `tray.items[].status` is `"active"`, `"passive"` or `"needs_attention"`; an item that sends another value, or none, reads `"active"`.
- Breaking: `panel`'s `monitor` is renamed `output` and `exclusive` is renamed `exclusive_zone`, with the same values.
- Breaking: `margin` on a `window`, `popup` or `lock` root, and `align_h`/`align_v` on any surface root, are refused as unknown properties instead of silently ignored. Set them on the child. `animate` on a `lock` refuses `width`, `height` and `visible`, which a lock already refuses.
- Stubs: `panel`, `window`, `popup` and `lock` return a `Surface` class and node constructors a `Node` class, so a surface used as a child is a type error.
- Breaking: node property `blur` is now `behind_blur`. `focus(name)` is now `focus_target(name)`, and the textfield property `focus` is now `focus_target`.
- `json.encode(value)` returns compact JSON with sorted keys; it raises on functions, userdata, cycles, NaN, infinity, nesting past 127, over 2^20 values, arrays more than half holes and a decoded array given a named key.
- `interval(ms, callback)` is a repeating timer with `timer`'s range, `cancel` and per-evaluation lifetime.
- `process.run` and `process.detach` accept `nil` for `args`.
- Breaking: every volume is a percent. `audio.volume` (0–150), `audio.source_volume`, `apps[].volume` and mpris `players[].volume` read `100` for 100%, and `set_volume`, `set_source_volume`, `set_app_volume` and `mantle.mpris:set_volume` take the same scale. Multiply old fractions by 100.
- Breaking: `mantle.mpris:set_loop_status` takes `"none"|"track"|"playlist"` and raises on anything else; `players[].loop_status` is `nil` instead of `""` when unreported, and `play_state` reads `"stopped"` instead of `""` before its first reading. `mantle.tray:scroll` raises on an orientation other than `"vertical"` or `"horizontal"`. `mantle.power:set_profile` warns and sends nothing for a name not in `profiles`.
- Breaking: unknown values are `nil` instead of `-1`: `sysinfo.temp_gpu`, bluetooth connected-device `battery`, and mpris `position`, `length` and track-list `length`. `network.ethernet_speed` is `nil` instead of `0` when unknown or no wired device is active.
- Breaking: `keyboard.backlight_pct` is renamed `backlight_percent`, `nil` without a readable backlight. `workspaces.active_client` fields `class`, `is_floating` and `is_fullscreen` are renamed `app_id`, `floating` and `fullscreen`, matching `windows` entries.
- Breaking: `mantle.updates.count` and the bluetooth `DiscoveredDevice.paired` field are removed. Use `#updates.packages`; a discovered device is never paired.
- List properties such as popup `constraint_adjustment` and text `content` runs reject a list with a `nil` hole or named keys. Entries after a hole used to be dropped silently.
- `dofile` and `loadfile` are unavailable in configs because their synchronous file reads can stall the Renderer. Use `require` for Lua modules or `process.run` for other files.
- `accessible_name` makes clickable nodes keyboard focusable and names them for screen readers. Tab and Shift+Tab traverse controls; Enter and Space activate them. The engine outlines a control only when Tab or an assistive-technology action focused it; `focus_ring = false` turns the outline off, and `focused(name)` with a node's `focused` reports focus within a node for custom styles. Mantle exposes the resolved scene through AT-SPI, with secure field values withheld. Breaking: with two or more focusable controls on a surface, Tab moves focus and no longer reaches a textfield's `on_navigate("tab")`.
- Borders follow the corners: a per-edge `border_width` or `border_color` on a rounded box, and any border on a `corner_shape = "scoop"` box, used to draw as four straight rectangles with square corners. Where two edges meet, the colour change sits on the corner in proportion to their widths, as in CSS.
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
