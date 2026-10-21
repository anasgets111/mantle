# Changelog

User-facing changes to the Lua API and the `mantle` CLI. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Each version is tagged `vX.Y.Z`
and published on [GitHub releases](https://github.com/anasgets111/mantle/releases). While the
version is 0.x, a minor release can break the Lua API. A `Breaking:` entry needs a config edit.

## Unreleased

- Fixed: a `mantle.mpris` Firefox or zen player's `position` could fall up to a second behind after a play, pause or seek, and step back mid-track. Those browsers report `position` in whole seconds; Mantle now keeps the precise offset they send with each seek.
- Fixed: a `computed` reading another `computed` that had just changed ran its function a second time on the next unrelated layout pass.

## 0.9.0 - 2026-10-20

- Added: `mantle.mpris` players carry `pid`, the process that owns the player, so a config can match a player to its `mantle.audio.apps[]` stream by `pid` instead of by name. It is `nil` when the bus does not report one.
- Added: `mantle.audio.apps[].media_name`, PipeWire's `media.name` for the stream, such as a browser tab's `"YouTube Music"`, so a config can tell one app's streams apart. It updates as the tab navigates.
- Breaking: `state(name, initial)` raises when one evaluation seeds a name with a table and a scalar, such as `{}` then `-1`; rename one. Both calls used to return one signal, so the table's reader failed only once its node was built, often in a closed panel, and `mantle check` passed.

## 0.8.1 - 2026-10-19

- Changed: the LuaLS stubs type `mantle.tray` `menu_type` and `toggle_type`, `mantle.network` `vpns[].kind` and `mantle.bluetooth` device `category` as their literal values, so the editor flags a misspelled comparison. A tray menu entry with an unknown `menu_type` now reads as `"standard"`, and one with an unknown `toggle_type` has `nil` for both `toggle_type` and `toggle_state` (the state was `-1`).
- Added: `on_click`, `on_double_click` and `on_press` report a mouse's back and forward thumb buttons as `"back"` and `"forward"`. They used to be ignored. A handler that treats every button but `"left"` alike now also runs for them.

## 0.8.0 - 2026-10-17

- Breaking: `effect.shader`'s `u_input` and `u_input_blurred` may be larger than their content, so `texture()` on them reads past its edge. Read them with `mantle_input` and `mantle_input_blurred`.
- Changed: a `list` with both `key` and `scroll` builds only the rows near its viewport, so a 1000-row list costs about as much as 50. A row scrolled more than a viewport away plays no exit and loses its tweens, keyboard focus and text draft. Keep row state that must survive in a named `state`.
- Changed: the `--profile` idle report's `ms` columns start with `handlers=`: due timers, signal wakes, off-thread results and scroll steps. They used to count toward `resolve=`, which could show 130 ms with no resolve pass.
- Changed: `ToplevelHandle:resize` types its edge as a union of the eight edge names, so LuaLS flags a misspelled edge.
- Fixed: a `shape` with `corner` commands could cost about 1,000 times a rounded box to draw on every repaint. Mantle skips zero-length curves.
- Fixed: an idle shell spent about 1 ms per clock tick in the Lua garbage collector, run after every getter that allocates.
- Fixed: an idle shell showing an image from your home directory (a `~/Pictures` wallpaper) redrew the whole surface twice a second. Mantle watches image files with inotify, so an edited, replaced or deleted file repaints at once, shader `images` included. A symlink is still checked twice a second.
- Fixed: a `"fill"` scroll container under a `window`, `lock` or spanning `panel` with no size of its own grew to its content instead of the surface, so it never scrolled (since 0.4.0).
- Fixed: the .deb's lock screen and polkit agent refused every password. Its PAM stack named `system-auth`, which Debian and Ubuntu lack. It includes `common-auth` and `common-account` instead.

## 0.7.5 - 2026-10-16

- Changed: a getter, `on_change` handler, action or timer no longer fails at 2.5 ms of CPU, which broke configs on slower machines. It fails at 50 ms, as a runaway: `exceeded the 50ms CPU limit for one evaluation`. Past 2.5 ms the log warns once per signal and names the slowest one.
- Added: [`mantle-bin`](https://aur.archlinux.org/packages/mantle-bin) on the AUR, published from each release's binaries.
- Fixed: on NVIDIA with egl-wayland2's explicit-sync fix (NVIDIA/egl-wayland2#56), the renderer leaked about 3 MiB an hour. egl-wayland2 1.0.2 holds the same buffer releases where Mantle cannot reach them, so on 1.0.2 the leak remains until #56 ships.

## 0.7.4 - 2026-10-16

- Fixed: while the session was locked, the renderer could hold one CPU core at 100% until unlock. It showed after a restarted shell took the lock over, and dates from 0.1.1.
- Fixed: with `--profile` on the NVIDIA proprietary driver, the supervisor leaked one file descriptor per sample (since 0.7.1). After about 2.5 h at `--profile=10` it could not spawn processes, so the lock screen could not unlock.

## 0.7.3 - 2026-10-16

- Changed: a looping `keyframes` animation or a `delay` costs no frames while it holds one value. A blink that rests 3.6 s no longer costs a tick per refresh.
- Changed: `mantle check` prints the source line under each error that names a `file.lua:N`, indented after `| `.
- Changed: a touchpad scroll over a `scroll` container coasts to a stop after the fingers lift. It used to stop dead. Wheel notches are unchanged.
- Added: a `keyframes` entry takes `"current"` as its first frame, to start the run on the value on screen. See [Keyframes](guide/animation.md#keyframes).
- Added: `run` on a `keyframes` entry. A different value starts a new run with the same frames, even mid-run, so a counter replays an animation without removing the entry and adding it back.
- Added: `settled = settled("name")` on any node gives a read-only signal. It is `false` while an `animate` tween on that node runs and `true` once the last one ends. Swap content when a collapse finishes instead of after a guessed `timer`.
- Added: `effect.shader.input = "desktop"` gives a shader on a `panel` box the desktop behind it, for lens and refraction effects. It captures through wlr-screencopy. Return `mantle_input` to leave a pixel, as with `"backdrop"`. See [Desktop input](guide/paint.md#desktop-input) for how it composites and what it costs.
- Added: `shape = { melt = k, parts = { .. } }` joins up to 8 rounded boxes with fillets of about `k` px. The fillets stay tangent through `animate.shape`. Paint, borders, clip, shadows, hit testing and regions follow the joined shape. See [Melt](guide/paint.md#melt).
- Added: `mantle check` warns when its surface's edge cuts off an outer shadow by 4 px or more, naming the side and the room to add. A side where a `panel` meets the output edge is exempt. The exit status is unchanged.
- Added: a warning when another node replaces an id-less child with `animate.exit` in place, which skips its exit.
- Fixed: a `compositor_blur` region that changed while the rest of the surface did not applied only at the next repaint.
- Fixed: a rotated or skewed box takes input across its rotated shape, not its upright bounding box. Clicks in the empty corners pass through.
- Fixed: `compositor_blur` follows `rotate` and skew instead of blurring the upright bounding box.
- Fixed: text in a `rotate`d, skewed or non-uniformly scaled node drew about 1 px heavier per stroke.
- Fixed: `mantle.privacy` names apps using the camera through PipeWire (browsers, Snapshot, OBS) instead of listing `pipewire`.

## 0.7.2 - 2026-10-13

- Changed: an icon name containing `/` no longer resolves through the icon theme.
- Changed: `mantle.secrets` talks to the Secret Service directly. A locked keyring shows its unlock prompt, and the store waits for it. A missing default keyring is created. `timed_out` means a service call took over 25 s, and it is final.
- Fixed: icon lookup finds `.svgz` theme icons and icons in `~/.icons`.
- Fixed: a getter, handler or callback could fail with "exceeded the 2.5ms CPU budget" for garbage collection it didn't cause.

## 0.7.1 - 2026-10-12

- Changed: a `move` or `translate` paints at whole buffer pixels, so a slide of under one pixel steps instead of gliding. Fractional positions drew glyphs differently on each driver.
- Changed: `on_hover` fires on the first pointer motion after layout moves its node under or off a still pointer. The pointer used to have to leave the node and come back.
- Changed: Tab on a lone plain `textfield` that does not hold focus focuses it. It used to reach `on_key`.
- Added: `size` in bytes on each `mantle.folders` entry. A symlink reports its target's size.
- Added: `text` `elide` accepts `"start"` (`…` then the end) and `"middle"` (start and end around a `…`, the end taking the odd grapheme). Both cut at grapheme boundaries, and `elided(name)` is true for either. Under `wrap = "word"` both act as `"end"`.
- Added: `on_key_release(key)` on any node hears a key going up whose press was offered to `on_key`, routed the same way. `key` is `{ name, text, modifiers }`. `mantle input key` sends the release after the press. See [key handlers](guide/input.md#key-handlers).
- Added: `on_double_click(rect, button, pointer, modifiers)` fires after `on_click` on a second click on the same node with the same button, within 400 ms and 4 px. The innermost node with `on_double_click` takes it, so single clicks still reach an ancestor's `on_click`. See [pointer](guide/input.md#pointer).
- Added: `on_drag` takes a fifth argument, `velocity = { x, y }`, in surface px per second over the last 100 ms of motion. On `"end"` it is the release velocity, or the last motion's when the pointer left the surface. Use it for flick scrolling. See [pointer](guide/input.md#pointer).
- Added: `image_status(name)`, bound as an image's `status`, reports the load of its `source`: `"empty"`, `"loading"`, `"ready"` with the file's own `width` and `height`, or `"failed"`. See [image_status](guide/signals.md#image_status-read-an-images-load).
- Added: `image.playing`, default `true`. `false` holds an animated GIF on its current frame (the first, if it never played) and stops redrawing it. `true` resumes from that frame. Each image node pauses on its own.
- Fixed: `elide = "end"` on one line keeps a right-to-left paragraph reading right to left when the kept head loses its first strong character (`"123 שלום"` cut to `"123…"`).
- Fixed: a panel with `exclusive_zone = "ignore"` or a fixed zone could map at a transient size first.
- Fixed: Tab on the only control, while it holds focus, shows the focus outline and reaches `on_key`. It used to be swallowed.

## 0.7.0 - 2026-10-11

### Renamed

Rename each in your config. The old name raises as unknown.

| Old | New | Why |
| :--- | :--- | :--- |
| `mantle.files`, field `folders` | `mantle.folders`, field `watched` | It lists watched folders, and `folders.folders` read badly |
| `mantle.system` | `mantle.clock` | It holds the wall and monotonic clocks, nothing else |
| `mantle.battery` `capacity` | `health` | It is full charge over design capacity; `capacity` read as the charge level |
| `mantle.storage` `files` | `contents` | The values are each file's contents, keyed by path |
| `list` `source` | `items` | It names the data; `source` is a file path on `image` and `shader` |
| `list` `itemfn` | `each` | A plain word |
| window `on_close` | `on_close_request` | The window stays open until the config hides it |
| `behind_blur` | `compositor_blur` | The compositor does the blurring |
| box `outline` (type `Outline`) | `shape` (type `Shape`) | `outline` read as CSS `outline`, which `ring` covers |
| shader uniforms `mantle_outline`, `mantle_outline_len`, `mantle_outline_point` | `mantle_shape`, `mantle_shape_len`, `mantle_shape_point` | They carry the `shape` property's points |
| node `submit` | `submits_secure` | It sends the armed secure field; `submit` read like `on_submit` |

### Changed, needs an edit

- `computed(dependencies, fn, before?)` and a capability's `:map(fn, before?)` never call `fn` with a capability's pre-push `nil`. Until every capability dependency has pushed, the derived signal holds `before` (default `nil`). Move what `fn` returned for that `nil` (`{}`, `0`, `""`) into `before` and drop the guard. Without `before` the signal reads `nil`, which breaks `#t` or arithmetic downstream. `:get()` and `on_change` are unchanged.
- `persistent_table { path, name }` becomes `persistent_table { path }`, where `path` is the file path, absolute or starting with `~/`. `name` raises as an unknown key. A relative path, one ending in `/`, or one with an empty, `.` or `..` component raises. The engine creates the parent directory.
- `mantle.clock`'s `monotonic` is `CLOCK_MONOTONIC` seconds since boot, as a fractional number. It used to be whole seconds since the capability started. `mantle.mpris`'s `position_updated_at` uses the same clock in microseconds, so divide it by `1e6` to compare.
- The internal `--detached` flag is gone and raises as an unknown argument.

### Changed, no edit needed

- A leading `~/` means `$HOME` in every path a config passes: `persistent_table { path }`, `mantle.folders:watch`/`unwatch`, `mantle.storage:open`/`set`, `mantle.notifications:set_sound`, `palette.quantize`, and the `image` `source`, `icon` `name`, `shader` `source` and `images`, `transition.shader` and `images`, `effect.shader` and `mask.source` properties.
- `persistent_table` keys read their declared `defaults` entry until `mantle.storage` pushes that file, instead of `nil`. `set(key, nil)` and a key the file lacks also read the declared default. Saves keep a symlink's link and the file's mode.
- `mantle.folders:watch` on a missing folder, or on one deleted later, waits for it to appear and lists it then. `error` holds until it does. It used to stay unwatched until a second `watch`.

### Added

- `file.read(path, on_done)` and `file.write(path, text, on_done?)` read and write whole files off the Lua thread. A write that would change nothing is skipped and reports `changed = false`. Writes land in call order, through a symlink to its target, keeping the file's mode. See [`file`](guide/scripting.md#file) and [Colour your apps](guide/app-colours.md).
- `process.capture(cmd, args, on_done)` runs a program to its end and calls `on_done(stdout, code)` with all of stdout as one string, newlines kept. It keeps the first 1 MiB and drops stderr. A signal, failed spawn or reload gives `nil, nil`.
- `palette.from_hct{ hue, chroma, tone }` and `palette.harmonize(color, source)` return `"#RRGGBB"`.
- `mantle.clock:now()` reads the monotonic clock at the call, before the first push too.
- `computed` accepts plain strings, numbers, booleans and tables among its dependencies and passes them to `fn` unchanged. A function or userdata still raises.

## 0.6.2 - 2026-10-11

- Changed: a HUP, USR1, USR2, ALRM or other terminating signal stops the shell the way SIGTERM does. The error names the signal and the sender's pid and command line. SIGINT and SIGTERM log their sender too.
- Changed: the renderer's `connection ended` line says whether the supervisor process is still running.
- Fixed: `mantle set` and `toggle` read a joined `--pid=N` or `--profile=N` after the state name as an option. It was stored as the value.
- Fixed: a notification `image-path` or `app_icon` given as a `file://` URL with escapes (`%20`) or a `localhost` host resolves. It was refused.
- Fixed: a tray item that leaves the bus while the tray starts is dropped. It could stay listed.

## 0.6.1 - 2026-10-11

- Fixed: a failed EGL surface, swap or dmabuf image call crashed the renderer. It logs `EGL error 0x…` and carries on.
- Fixed: the privacy capability could crash the supervisor on an empty camera or microphone device-event batch.
- Fixed: hot reload of a config file that could not be re-watched after a reload stopped silently. It logs a warning.

## 0.6.0 - 2026-10-10

- Breaking: config error text changed throughout. A shell that matches on `mantle.rescue.error_log` text must update.
- Breaking: `package.searchers` holds only the Lua file searcher. The C-module searchers could never load in safe mode.
- Changed: a bad value, duplicate id or unknown property on a top-level `panel`, `window`, `popup` or `lock` names the line that built it, as errors on child nodes do: ``shell.lua:2: panel: invalid value for `height`: ...``.
- Changed: an unknown property on any node reports the node, not a top-level return error or a bad `children` value: ``shell.lua:2: text: no property `contnet`; did you mean `content`?``.
- Changed: a layout error from `mantle check` or a reload leads with the line that built the failing node: ``shell.lua:9: text: invalid value for `content`: ...``. Then come the path to its parent and the surface in parentheses. A `children` entry reads `text[2]`, counting from 1 like Lua, and a lone `child` no longer shows a made-up `[0]`.
- Changed: `mantle check` puts the pass on its own last line and no longer repeats `layout:` after the pass name.
- Changed: argument and value errors read like Lua's. An error from a global function, capability action or `mantle check` leads with the `file:line` of the config call that caused it: `mantle.audio:set_volume: expects 1 argument, got 2`. A bad element inside a table argument is named. A method on an engine object names itself, as `method reveal`. A caught `pcall` keeps the caller's `file:line`, and a rejected property value of the wrong type names its type, as `string "tall"` or `nil`.
- Changed: every out-of-range value reads like `timer: ms must be within [1, 86400000], got integer 0`, unit in the subject. A float reads `number 2.5` and a whole float `number 1.0`. This covers `timer`, `interval`, `delay`, `pulse`, `loops`, gradient stops and `idle:register_threshold`.
- Changed: a config handler that raises logs where it was defined (`widgets/bar.lua:12`) and folds identical repeats into `raised again (N times)`.
- Changed: `error({...})` names the value type. A missing `require` lists config-relative paths without the C-modules line.
- Changed: a misspelt table key or choice value (`anchor = { lefft = true }`) ends with "did you mean `left`?".

## 0.5.5 - 2026-10-10

- Added: `mantle.workspaces`, `mantle.windows` and keyboard layouts on Sway, over `$SWAYSOCK`. A workspace's `id` is its name, and an Xwayland window reports its X11 class as `app_id`. Sway ignores `set_maximized` and `toggle_special`.
- Added: MangoWC support for `mantle.workspaces`, `mantle.windows` and the `mantle.keyboard` layout name. Each tag is a workspace with id `"<output>:<tag>"`.
- Fixed: `mantle.idle` reports a compositor hold taken while the seat is already idle, such as a video resumed from a phone. `inhibited` stayed false until input came and went.

## 0.5.4 - 2026-10-09

- Added: `mantle.mpris` players carry `art_url`, the cover URL as the player sent it, so a config can fetch remote artwork such as Spotify's.
- Fixed: `padding` on a `text` or `textfield` insets its glyphs, caret, links and IME cursor. They drew from the box's corner, so padded labels sat off-centre.

## 0.5.3 - 2026-10-09

- Changed: an image file rewritten in place may take up to 0.5 s to show, because Mantle checks each path for edits at most twice a second.
- Fixed: the lock screen ignores a second password submitted during the unlock animation. It used to re-run PAM and flash an error.

## 0.5.2 - 2026-10-09

- Fixed: `mantle call`, `set` and `input` fail at once when no shell is connected. They used to wait 5 s.
- Fixed: a `process.run` child printing faster than the shell reads no longer gets the shell restarted. The child waits on its pipe.
- Fixed: `mantle.bluetooth` device names are BlueZ's `Alias`. A rename shows, and a nameless device shows its address-style alias instead of an empty name.
- Fixed: a Bluetooth device's `Class` change refreshes its category.
- Fixed: `mantle.privacy` sees a camera plugged in after start or replugged.
- Fixed: `mantle.appearance` keeps its last value when the portal call fails.
- Fixed: `workspaces` and `windows` recover after niri restarts. They stayed frozen at the last value.
- Fixed: a polkit prompt that asks a visible question (OTP, username) fails instead of receiving the password.

## 0.5.1 - 2026-10-07

- Fixed: an `effect` under `animate` keeps its `backdrop.mask` while it tweens. It was dropped from the first frame on.

## 0.5.0 - 2026-10-06

- Breaking: workspace ids are strings, and `WorkspaceEntry.idx` is replaced by `number`. Treat `id`, `active_workspace`, `focused_workspace`, `windows[].workspace_id` and the `focus` and `move_to_workspace` arguments as opaque strings to pass back, never to compute with. On Hyprland a numbered workspace's id is its number (`"3"`), so `focus(tostring(n))` still creates one. `number` is what a keybind targets (niri's position on the output, Hyprland's number), and `nil` for a Hyprland named workspace.
- Changed: the engine's focus outline follows the node's `radius`, `corner_smoothing` and `outline`.
- Changed: a rounded `border` has the box's radius on its outer edge instead of half its width more.
- Changed: a card press released over a field inside it no longer clicks the card.
- Added: `ring = { width, color, offset }` on box nodes draws a stroke `offset` px outside the box (inside when negative) with a clear gap, for Material-style focus rings. It follows `radius`, `corner_smoothing` and `outline`, takes no layout space and tweens under `animate`.
- Added: double-click in a plain `textfield` selects a word and triple-click a line. Dragging or Shift+clicking afterwards extends by words or lines.
- Added: a right or middle press on a plain `textfield` calls the field's own `on_press` and, on release, its own `on_click`, never an ancestor's. Use it for right-click menus. The press focuses the field and keeps a selection it lands in. Left presses and `secure_submit` fields are unchanged.
- Added: Ctrl+X cuts a field's selection.
- Added: `focus_target(name)` gains `:cut()`, `:copy()`, `:paste()` and `:select_all()`. `has_selection(name)` is a read-only signal for a field with text selected.
- Added: `textfield` takes `multiline = true`. The draft wraps and takes newlines, and the field grows from `min_lines` to `max_lines` rows, then scrolls. `submit_key` (`"ctrl+return"` or `"return"`) picks the chord that submits. `multiline` with `secure_submit` is refused. See [Multiline](nodes/textfield.md#multiline).
- Added: `focus_target` and `autofocus` work on any focusable control, not only a plain `textfield`. `:set_text` stays textfield-only.
- Added: `focus_target(name):request()` works from `on_key` and from a field's `on_change` and `on_submit`. A request from a key callback shows the focus outline, and one from `on_click` does not.
- Added: `toplevel(id)` takes `:set_maximized(on)`, `:set_minimized()` and `:set_fullscreen(on)`, which need no press. The result arrives through `state()`.
- Added: `textfield` takes `selection = { background, foreground }`, the highlight colour and the selected glyphs' colour. The highlight is drawn as given, so tint it with alpha. Unset, the highlight stays `foreground` at 30% alpha and the glyphs keep the text colour.
- Added: [`mantle.appearance`](capabilities/appearance.md), a read-only capability for the desktop portal's appearance settings, pushed live: `color_scheme`, `accent`, `contrast` and `reduced_motion`. Without a portal every field holds its default.
- Added: `effect.backdrop.mask` takes a node `mask`'s `gradient` or `source` (and `invert`) and scales the glass's coverage, for a scroll edge or a feathered panel. Where the mask is clear the ground shows through, and between it crossfades from the ground to the blur. A `node` mask is refused.
- Added: `on_click`, `on_press`, `on_drag` and `on_wheel` take a trailing `modifiers` argument, the `{ ctrl, shift, alt, super }` table `on_key` reports, for Ctrl-click, Shift-click and Shift-drag. It reads all false while none of the shell's surfaces has keyboard focus.
- Fixed: `on_drag`'s `rect` is the node's current box on every call, not its box at the press. `rect.x + pointer.x` stays the surface position when the node moves under its own drag (a splitter).
- Fixed: Hyprland named workspaces (`workspace = "name:chat"`) are listed in `mantle.workspaces` and `mantle.windows` after the numbered ones instead of dropped, and `focus` reaches them (issue #1).

## 0.4.0 - 2026-10-05

- Breaking: `content_blur` and `backdrop_blur` are replaced by one `effect` table, `{ blur, backdrop = { blur } }`, like CSS `filter` and `backdrop-filter`. `content_blur = 4, backdrop_blur = 8` becomes `effect = { blur = 4, backdrop = { blur = 8 } }`, and `animate = { content_blur = 200 }` becomes `animate = { effect = 200 }`. Each blur is a sigma in `[0, 8192]` and bindable. Only box kinds take `backdrop`. Unknown keys are refused with the accepted ones listed. `animate.effect` tweens both blurs, and a key only one side sets tweens from or to `0`.
- Breaking: `textfield.on_navigate` is removed. The field's own `on_key` hears the same keys, and so do its ancestors'. `on_navigate = function(k) if k == "down" then ... end end` becomes `on_key = function(key) if key.name == "Down" then ... end end`. Key names become keysym names: `"up"`, `"down"`, `"left"`, `"right"`, `"page_up"`, `"page_down"`, `"tab"` and `"backtab"` become `"Up"`, `"Down"`, `"Left"`, `"Right"`, `"Page_Up"`, `"Page_Down"`, `"Tab"` and `"ISO_Left_Tab"`. Left and Right still arrive only when the caret cannot move and Shift is up. A key repeats only if an `on_key` handler hears it.
- Breaking: `textfield.caret_color` is replaced by `caret = { color, width, height, radius }`. `caret_color = c` becomes `caret = { color = c }`. See [textfield](nodes/textfield.md#properties) for each key's default.
- Changed: `shadows` takes up to 16 layers, up from 8.
- Added: box kinds take `outline = { commands = { .. } }` in place of `radius`, any shape as one closed contour of `path` commands: a popover with its arrow, a speech bubble, a tab joined to its panel. A coordinate is px, `"NN%"` of the box or `{ from = "right", px = -12 }`, so the shape follows the box's size and may reach past it. Fill, border (one width and colour), shadows, inset shadows, `clip = "rounded"`, `mask`, `effect.backdrop`, `mantle_sdf`, hit testing and `behind_blur` follow it. `animate.outline` tweens point by point between lists of the same commands, springs included, and snaps otherwise. 0.7.0 renames it `shape`.
- Added: `path` commands take `{ op = "corner", points = { x, y }, radius = r, corner_smoothing = s }`, which rounds the turn at one point as a continuous corner.
- Added: `effect.shader` takes `progress` (`[-8192, 8192]`, default `0`), which becomes `u_progress` as on a `shader` node. `animate = { effect = .. }` tweens it, keyframes and `loops = "infinite"` included, so a content or backdrop shader animates without Lua running per frame. The rest of the `shader` table snaps to the target.
- Added: `focus_visible(name)`, a read-only signal like `focused(name)`, bound as a node's `focus_visible`. It is `true` while the node or one inside it holds focus that Tab, Shift+Tab or an assistive-technology action moved there (CSS `:focus-visible`). A press, `autofocus` or `focus_target(name):request()` makes it `false`. `focus_ring = false` does not affect it, so a custom ring bound to it stays hidden after a mouse click.
- Added: `window.geometry_inset` adds a band around a client-decorated window's frame for its shadow and rounded corners. The compositor sizes, tiles and snaps by the frame alone. Bind it to `toplevel(id):state()` to drop the band when maximized, fullscreen or tiled. See [Client-side decoration](surfaces/window.md#client-side-decoration).
- Added: `shader`, `effect.shader` and `transition.shader` take `images = { name = "/abs/path.png" }`, up to 8 `.png`, `.jpg`, `.jpeg` or `.webp` files for the `.frag` to sample: a normal map, a noise texture or a LUT. Each entry declares `uniform sampler2D name` and `uniform vec2 name_size`. Changing a file reloads it. See [Images](nodes/shader.md#images) for naming rules and sampling.
- Added: `toplevel(id):state()`, a read-only signal of a `window`'s configure state: `activated`, `maximized`, `fullscreen`, `resizing`, `tiled`, `bounds`, `capabilities` and `decoration`. It returns to its defaults when the window closes. See [Window state](surfaces/window.md#window-state).
- Added: `window.decorations` picks who draws the frame, `"server"` (the default, as before) or `"client"`. A reload that changes it asks the compositor again.
- Added: `blend` sets a CSS `mix-blend-mode` (`"multiply"`, `"screen"`, `"plus_lighter"` and more; default `"normal"`) on a node, a `background` layer (`{ fill = .., blend = .. }`) or a `shadows` layer. It composites onto what the surface painted under it, never the desktop. A node's blend applies after every `effect` filter and takes its shadows with it. `blend` snaps under `animate`. See [Blend modes](guide/paint.md#blend-modes).
- Added: `effect.shader` takes `input = "backdrop"` on box kinds. The program reads what the surface painted under the box through `mantle_input`, and the `effect.backdrop` copy through `mantle_input_blurred`. Its output replaces that area before the node paints, mixed with the original by the node's `opacity`. Return `mantle_input(uv)` to leave a pixel unchanged. See [Backdrop input](guide/paint.md#backdrop-input).
- Added: a `shadows` layer with `inset = true` works like CSS `box-shadow: inset`. It shades the inside of the padding box with the same `color`, `blur`, `offset` and `spread`, above the background and under the children and border. It needs `shadow_mode = "box"` and a box kind. See [Inset shadows](guide/paint.md#inset-shadows).
- Added: `textfield.escape` picks what Escape does: `"clear"` (the default, as before), `"blur"` or `"pass"`. `"blur"` keeps the draft, leaves the field and calls `on_cancel(false)`. `"pass"` keeps the draft and focus, and Escape goes up through `on_key`, then `on_escape`. A `secure_submit` field ignores it.
- Added: `on_key(key)` on any node or surface hears key presses and repeats, for custom controls. `key` is `{ name, text, modifiers = { ctrl, shift, alt, super }, repeat }`, with `name` an xkb keysym name (`"Down"`, `"Return"`, `"a"`). A key goes to the focused node, each ancestor, then the surface, and a handler returning `true` stops it. A focused `textfield` keeps the keys it edits with and passes the rest up. A named node with `on_key` takes keyboard focus. See [key handlers](guide/input.md#key-handlers).
- Added: `pointer(name)`, a read-only signal of the pointer's `{ x, y }` in logical pixels from a node's top-left corner while the pointer is over the node or its children, `nil` otherwise. Bind it as a node's `pointer`. It costs nothing while unread.
- Added: `mantle [-c DIR] input <surface> move | press | release | leave | click | drag | wheel | key | type` injects pointer and keyboard input into one surface of a running config, for testing. Lock surfaces and `secure_submit` fields refuse it. See [Injecting input](guide/cli.md#injecting-input).
- Added: `background` takes a list of up to 16 layers, first on top, like CSS multiple backgrounds. Each is a colour, a gradient or `{ fill = .. }`. `animate.background` tweens colour layers pairwise and fades in one a side lacks. Gradient layers snap.
- Added: `textfield` takes `font`, `font_weight`, `italic`, `letter_spacing`, `line_height` and `font_variations`, with `text`'s ranges and defaults. The caret, the selection and IME placement follow the metrics. An unset `height` is `font_size * line_height`.
- Added: `wrap = true` on a `row` or `column` flows children onto new lines, like CSS `flex-wrap`. `line_spacing` sets the gap between lines. `wrap` with `scroll` is refused. See [Wrapping](nodes/row-column.md#wrapping).
- Added: `homogeneous = true` on a `row` or `column` gives every visible child one equal slot along the main axis, like GTK's `homogeneous`. See [Equal slots](nodes/row-column.md#equal-slots).
- Added: `toplevel(id)` returns a handle for a `window`'s own frame: `:move()`, `:resize(edge)` and `:show_menu()`. They work only from an `on_press` or an `on_drag` `"start"`. Anywhere else they warn and send nothing. See [Custom title bar](surfaces/window.md#custom-title-bar).
- Added: `on_press(rect, button, pointer)` fires on any node at press time for the left, right and middle buttons, before `on_click`. A press on a `textfield` does not call it.
- Added: `border_color` takes a gradient table, `{ gradient = "linear" | "radial" | "conic", angle, stops }` as `background` does, painted along the whole outline. A gradient per edge is refused, and a gradient border snaps under `animate`.
- Added: `effect.shader = { source, input, params, padding }` runs a fragment shader over a node's painted subtree, which a `shader` node cannot read. The `.frag` reads the subtree through `mantle_input(uv)` and the distance to the node's outline through `mantle_sdf(p)`. `padding` (`[0, 512]` px) lets it read and draw past the box. Saving the file recompiles it, and one that fails to build leaves the node as painted. See [Shader effects](guide/paint.md#shader-effects).
- Added: `corner_smoothing`, `0` to `1`, gives box kinds and `image` continuous corners like Figma's corner smoothing. `radius = 24, corner_smoothing = 0.6` is close to iOS, and `0` is the circular arc as before. `animate` tweens it. See [Continuous corners](guide/paint.md#continuous-corners).
- Added: `effect` takes the CSS colour filters `saturate`, `brightness` and `contrast`, each in `[0, 8]` with `1` off, on the node and in `backdrop` (`effect = { backdrop = { blur = 45, saturate = 2 } }`). They apply after the blur, are bindable, and `animate.effect` tweens them.
- Fixed: `text` and `textfield` glyphs that run past a tight `line_height` (descenders below the line box) are no longer cut flat at it. `line_height` sets layout only, and an ancestor's `clip` still cuts the ink.

## 0.3.0 - 2026-10-05

- Breaking: `clip` defaults to `"none"`, like CSS `overflow: visible`. Children, their shadows and their transforms paint and take the pointer past a box that does not set it. A scrolling `row`, `column` or `list`, a surface and a box with a `mask` still cut to the box. Add `clip = "box"` where a box must cut, such as a button holding a ripple or a fixed-width `rect` holding a long `text`.
- Breaking: `shadow_color`, `shadow_blur`, `shadow_offset` and `shadow_spread` are replaced by `shadows`, a list of `{ color, blur, offset, spread }` layers like CSS `box-shadow: a, b`. Wrap the old four in one layer: `shadow_blur = 8, shadow_offset = { y = 2 }` becomes `shadows = { { blur = 8, offset = { y = 2 } } }`, and `animate = { shadow_blur = 200 }` becomes `animate = { shadows = 200 }`. The first layer is on top, up to 8. Each field is bindable, and `animate.shadows` tweens layer by layer.
- Changed: `palette.quantize` uses Material's Celebi quantizer instead of median cut, so the same image gives different swatches (colours and `share`s). `opts.method = "wu"` picks Wu alone.
- Changed: a parent's `clip` stays put when a child moves with `translate`, `scale` or `rotate`. A child that transforms past a clipping parent's box is cut at the box.
- Changed: an invalid property value (out of range, wrong type, bad colour, malformed table) no longer freezes its surface at startup or in a live update. The node uses that property's default and the rest applies. The error is logged and held in `mantle.rescue` until the value is fixed. A reload and `mantle check` still refuse it.
- Changed: a node that fails a live update or the startup apply (a raising getter, a malformed `children`) freezes only its own surface, which keeps its last applied tree. The other surfaces keep updating, an error banner drawn from `mantle.rescue` included. The failure is logged as an error, once until its message changes. A reload that fails anywhere is still refused whole.
- Changed: `on_click` and `on_drag` measure `pointer` in the node's own untransformed box, so a translated, scaled or rotated node reports coordinates within its size.
- Changed: each `textfield` keeps its own draft. Moving focus to another field, with Tab or a click, no longer discards the text of the one you left. A draft goes when its node leaves the tree.
- Changed: `textfield.caret_color` colours the caret, not the selection highlight. It defaults to `foreground`, as before.
- Changed: a `textfield` without `height` is one line of `font_size` tall instead of 0 px, so its placeholder shows. `width` is still required.
- Changed: a stacking parent whose size is fixed or capped below its largest child gives every child its content box, so a smaller child no longer centres in the larger one's width.
- Changed: a `text` or `icon` capped by `max_width` or `max_height` sizes its content-sized parent at the cap instead of its full content, so an elided title in a content-sized `row` stays centred.
- Changed: `battery.capacity` reports battery health: full charge as a percent of the design capacity, `nil` when UPower does not know it.
- Changed: the wheel stops at the end of the content the last layout measured, so a `map` of a `scroll` signal runs once per wheel event at an end instead of twice.
- Changed: an `icon` with `foreground` recolours every shape of a `-symbolic` icon that never uses `currentColor`, as GTK does, so themes that hard-code symbolic colours (Tela) follow it. Icons that use `currentColor` keep their accents, and full-colour icons are unchanged.
- Changed: images scale down with a bilinear filter instead of box sampling. The result is closer to the source, and a 4K wallpaper scales to 2133x1200 in 8.5 ms instead of 75 ms.
- Added: `path.trim_axis = "x"` makes `trim_start` and `trim_end` fractions of the node's width. The stroke keeps what lies inside that band, cut at its edges with `stroke_cap` on every cut end.
- Added: `path.shift` moves the geometry inside the node before trimming. Animate it to scroll a shape through a fixed window.
- Added: a `keyframes` entry, or one frame, takes `spring = { stiffness, damping }` in place of `easing`. The segment springs from rest in real time and lands on its frame when its `duration` ends.
- Added: `textfield.initial_text` (string or signal) seeds a plain field's draft once, when the field enters the tree, with the caret at the end. Later changes are ignored, and a changed `id` or `key` is a new field. `autofocus` resets the draft to `initial_text` (`""` when unset) and calls `on_change` with it. It is refused with `secure_submit`. See [textfield](nodes/textfield.md#properties).
- Added: `scroll(name):scroll_to(offset)` and `:scroll_by(delta)` move a scroll offset from Lua, clamped to the content and eased under `animate.scroll`. `scroll_by` adds to where a run is headed, so repeated arrow clicks add up. A request waits while its container is hidden, and the next pass drops one when no container holds the signal.
- Added: `animate.scroll` on a scrolling `row`, `column` or `list` eases each mouse-wheel notch toward a target that stops at the ends. The `scroll` signal holds the offset on screen each frame, so its readers reflow continuously. `:reveal` eases the same way. Touchpads, high-resolution wheels and `reset_on_close` still move it at once.
- Added: a signal may sit inside a property table, such as `margin = { top = sig }`, a gradient stop's colour, an `animate` entry or a text run's flag, and is read again once it is written. `children`, a `list`'s `source`, structural fields such as a panel's `anchor`, tables with a metatable and tables a signal returns still refuse one.
- Added: `textfield.disabled` keeps a field drawn but out of keyboard focus (Tab, press, `autofocus`, `focus_target`).
- Added: `textfield.max_length` caps the draft in grapheme clusters for typing, paste, IME and `set_text`, `secure_submit` fields included.
- Added: `image.radius` rounds the picture's corners with no clipping wrapper, as a number or a per-corner table like `rect.radius`. It rounds what shows, so `fit = "contain"` rounds the fitted picture.
- Added: `path` takes `stroke_cap` (`"butt"`, `"round"`, `"square"`), `stroke_join` (`"miter"`, `"round"`, `"bevel"`), and `trim_start`/`trim_end`, which stroke only that fraction of the path's length and animate like any number.
- Added: `focus_target(name):set_text(text)` prefills or clears the draft of every plain textfield with that `focus_target`, hidden and disabled ones too. It calls no `on_change` and never touches a `secure_submit` field.
- Added: `textfield.placeholder_color` colours the placeholder. It defaults to `foreground`, as before.
- Added: `text` takes `font_variations`, a table of OpenType axis tags to values such as `{ FILL = 1, GRAD = 0, opsz = 24 }`, for variable fonts like Material Symbols. An explicit `wght` overrides `font_weight`.
- Added: `on_escape()` on `panel`, `window` and `popup` fires on Escape when the surface has the keyboard and no focused field took it, so a dialog or menu without a field can close. A field with text or `on_cancel` keeps Escape.
- Added: `palette.hct(color)` returns a colour's `{ hue, chroma, tone }` in HCT, so a config can apply its own seed rule. Each `palette.score` seed is one input swatch's colour.
- Added: `palette.scheme(seed, opts?)` returns the 49 Material 3 colour roles for a seed, with `dark`, `variant` and `contrast` options.
- Added: `palette.score(swatches)` ranks `palette.quantize` swatches into seed colours as Material 3 does.
- Added: `animate.width` and `animate.height` ease a content-sized axis. When a label changes, the box grows from the size on screen to the new content size, and siblings follow. Only eased entries animate it. Springs and keyframes snap.
- Added: `radius` takes per-corner values, `{ top_left, top_right, bottom_right, bottom_left }`, which `animate` tweens. Corners too big for a side shrink together, as in CSS. A shadow rounds by the mean corner.
- Added: `hittable = false` lets the pointer through a node and its descendants to what is underneath. A descendant's `hittable = true` takes it back.
- Added: `on_click` takes the pointer as a third argument, `{ x, y }` in node-local coordinates like `on_drag`'s. A keyboard activation passes the node's centre.
- Fixed: a `map` of a `scroll` signal kept a stale offset when layout clamped it (a wheel past either end) or a `:reveal` moved it, so offset-sized children jumped at the ends. It lays out again in the same frame.
- Fixed: a `text` whose `content` runs table or `font_variations` table was edited in place and set again kept its old size and glyphs. It re-measures.
- Fixed: `audio` recovers when PipeWire restarts or starts after Mantle. While PipeWire is down, `audio` reads as a machine with no audio hardware and `privacy` clears its microphone and screencast users.
- Fixed: `network` rebinds its devices when NetworkManager restarts, so `wifi_devices` and wired state drop the old daemon's values.
- Fixed: the Bluetooth pairing agent rejects calls from anyone but bluetoothd.
- Fixed: `brightness` picks up a backlight that appears after startup, such as on a dock or a hybrid GPU.

## 0.2.0 - 2026-10-04

- Breaking: an omitted `panel` width or height fills the configured root axis when both opposite edges are anchored. To keep a deliberately narrow root, wrap its content in a sized `child` and move its background there.
- Breaking: every engine-defined string enum is lowercase snake_case, and old spellings are refused: `align_h = "center"`, `width = "fill"`, `layer = "top"`, `keyboard_interactivity = "on_demand"`, `easing = "in_out_quad"`, `loops = "infinite"`, `constraint_adjustment = { "flip_y", "slide_x" }`, `exclusive_zone = "ignore"`, gradients `"linear"`/`"radial"`/`"conic"`. Capability values follow the same rule: `battery.state` (`"fully_charged"`, `"pending_charge"`, ...), mpris `play_state` (`"playing"`), `loop_status` and `set_loop_status` (`"none"`, `"track"`, `"playlist"`). POSIX signal names and SVG path ops keep their case.
- Breaking: panel `output` keywords are `"all"` and `"active"`. `"All"` names a connector and logs that none is connected.
- Breaking: `tray.items[].status` is `"active"`, `"passive"` or `"needs_attention"`. An item that sends another value, or none, reads `"active"`.
- Breaking: `panel`'s `monitor` is renamed `output` and `exclusive` is renamed `exclusive_zone`, with the same values.
- Breaking: `margin` on a `window`, `popup` or `lock` root, and `align_h`/`align_v` on any surface root, are refused as unknown properties instead of ignored. Set them on the child. `animate` on a `lock` refuses `width`, `height` and `visible`, which a lock already refuses.
- Breaking: node property `blur` becomes `behind_blur`. `focus(name)` becomes `focus_target(name)`, and the textfield property `focus` becomes `focus_target`.
- Breaking: every volume is a percent. `audio.volume` (0 to 150), `audio.source_volume`, `apps[].volume` and mpris `players[].volume` read `100` for 100%, and `set_volume`, `set_source_volume`, `set_app_volume` and `mantle.mpris:set_volume` take the same scale. Multiply old fractions by 100.
- Breaking: `mantle.mpris:set_loop_status` takes `"none"|"track"|"playlist"` and raises on anything else. `players[].loop_status` is `nil` instead of `""` when unreported, and `play_state` reads `"stopped"` instead of `""` before its first reading.
- Breaking: `mantle.tray:scroll` raises on an orientation other than `"vertical"` or `"horizontal"`.
- Breaking: unknown values are `nil` instead of `-1`: `sysinfo.temp_gpu`, bluetooth connected-device `battery`, and mpris `position`, `length` and track-list `length`. `network.ethernet_speed` is `nil` instead of `0` when unknown or no wired device is active.
- Breaking: `keyboard.backlight_pct` is renamed `backlight_percent`, `nil` without a readable backlight. `workspaces.active_client` fields `class`, `is_floating` and `is_fullscreen` are renamed `app_id`, `floating` and `fullscreen`, matching `windows` entries.
- Breaking: `mantle.updates.count` and the bluetooth `DiscoveredDevice.paired` field are removed. Use `#updates.packages`. A discovered device is never paired.
- Breaking: list properties such as popup `constraint_adjustment` and text `content` runs reject a list with a `nil` hole or named keys. Entries after a hole used to be dropped.
- Breaking: `dofile` and `loadfile` are unavailable in configs, because their synchronous file reads can stall the renderer. Use `require` for Lua modules or `process.run` for other files.
- Breaking: with two or more focusable controls on a surface, Tab moves focus and no longer reaches a textfield's `on_navigate("tab")`.
- Breaking: a `path` subpath is solid regardless of winding, so an inner subpath drawn in the opposite direction no longer cuts a hole. Mark it with `hole = true` on its first command.
- Changed: `mantle.power:set_profile` warns and sends nothing for a name not in `profiles`.
- Changed: surfaces paint at their output's compositor scale, fractional included, so text and images are sharp on HiDPI displays. Sizes stay in logical pixels. A `shader`'s `size` uniform stays logical while `gl_FragCoord` counts buffer pixels.
- Changed: a resized `image` with `async` keeps drawing its previous size, scaled, until the new size decodes, instead of blanking. A changed `source_blur`, or a changed `source` without `retain`, still blanks.
- Changed: stubs give `panel`, `window`, `popup` and `lock` a `Surface` return type and node constructors a `Node` one, so a surface used as a child is a type error.
- Added: `network.vpns` lists saved NetworkManager VPN and WireGuard profiles with `active` and `activating`. `connect_vpn(uuid)` and `disconnect_vpn(uuid)` toggle one, and `vpn_error` reports a failed activation.
- Added: a NetworkManager secret agent raises `vpn_secret` when an activation needs secrets. Answer it with `secure_submit = { capability = "network", action = "vpn_secret", name = request.id .. "/" .. field }` fields, or `cancel_vpn_secret()`.
- Added: `workspaces` and `windows` entries have `urgent`. niri reports it as set. Hyprland sets it on an `urgent` event and clears it when the window gains focus or closes. It is always `false` on wlr-foreign-toplevel.
- Added: a `radio` capability. `mantle.radio.radios` lists each rfkill kind with `soft_blocked` and `hard_blocked`, and `set_blocked(kind, blocked)` and `set_all_blocked(blocked)` write `/dev/rfkill`.
- Added: Bluetooth `pairing_request` has an `id` and supports `"pin_entry"` and `"passkey_entry"`. Answer these with `secure_submit = { capability = "bluetooth", action = "pair", name = request.id .. "/" .. request.mac }`.
- Added: `network.wifi_devices` lists Wi-Fi interfaces by name. `scan_device`, `connect_device` and `disconnect_wifi_device` target one. Flat join fields describe that join, while other flat Wi-Fi fields and the old actions use the primary interface.
- Added: plain `textfield` supports Ctrl+Z undo and Ctrl+Shift+Z and Ctrl+Y redo, with `on_change` for restored drafts.
- Added: plain `textfield` composes through text-input-v3 where available. Preedit stays local until commit. Secure fields do not use an input method.
- Added: `animate.move` eases a matched node from its previous painted position to its new layout position, without moving siblings or running Lua each frame. Give items that can change order a stable `id` or list `key`.
- Added: `audio.sinks[]` and `audio.sources[]` report per-channel `index`, `position` and percent `volume`. `set_sink_channel_volume` and `set_source_channel_volume` set one channel without changing the others.
- Added: `battery.peripherals` lists UPower devices outside the system supply, with charge percentage or coarse level when reported.
- Added: `json.encode(value)` returns compact JSON with sorted keys. It raises on functions, userdata, cycles, NaN, infinity, nesting past 127, over 2^20 values, arrays more than half holes and a decoded array given a named key.
- Added: `interval(ms, callback)`, a repeating timer with `timer`'s range, `cancel` and per-evaluation lifetime.
- Added: `process.run` and `process.detach` accept `nil` for `args`.
- Added: `accessible_name` makes clickable nodes keyboard focusable and names them for screen readers. Tab and Shift+Tab traverse controls, and Enter and Space activate them. Mantle exposes the resolved scene through AT-SPI, with secure field values withheld.
- Added: the engine outlines a control only when Tab or an assistive-technology action focused it. `focus_ring = false` turns the outline off. `focused(name)`, bound as a node's `focused`, reports focus within a node for custom styles.
- Added: `path` `commands` tween point by point under `animate` between lists with the same ops and `hole` flags, `keyframes` included, so shapes morph without Lua per frame. Any other change still snaps.
- Added: `path` `A` draws a circular arc from centre, radius, start and sweep in degrees. `hole = true` on a subpath's first command cuts it out of the fill.
- Added: a list of up to 4096 numbers in `shader` and `transition` `params` fills a `float` or `vec2`-`vec4` uniform array, enough for a 256-bar visualizer.
- Added: `idle` `on_resume` receives `"input"`, `"activity"` or `"inhibitor"`, so a config can wake displays only for input while stopping idle stages for other resumes.
- Fixed: input regions, text links and pointer cursors follow ancestor transforms. Canvas `Frame.transform` composes nested transforms.
- Fixed: a per-edge `border_width` or `border_color` on a rounded box, and any border on a `corner_shape = "scoop"` box, drew as four straight rectangles with square corners. Borders follow the corners, and where two edges meet the colour change sits on the corner in proportion to their widths, as in CSS.
- Fixed: Bluetooth switches to a present adapter when the tracked adapter is removed.

## 0.1.0 - 2026-10-01

First release. Mantle is an engine for Wayland desktop shells declared in Lua. It ships no shell of
its own beyond the one-clock starter bar.

- Added: [surfaces](surfaces/index.md): panels, popups, windows and lock screens.
- Added: [nodes](nodes/index.md): rects, rows, columns, text, text fields, images, icons, lists,
  paths, shaders and capture.
- Added: [capabilities](capabilities/index.md): 24 desktop services, among them audio, network,
  Bluetooth, notifications, tray, MPRIS, workspaces, idle, updates and secrets.
- Added: [signals](guide/signals.md) update widgets when values change. Saving the config reloads
  the shell.
- Added: `mantle check` catches Lua and layout errors before a run. `mantle init` sets up LuaLS
  completion and type checking.
- Added: packages for Ubuntu 26.04+ (`.deb`) and Fedora 44+ (`.rpm`), and an x86_64 tarball
  ([installation](guide/installation.md)).
