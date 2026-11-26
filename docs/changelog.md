# Changelog

User-facing changes to the Lua API and the `mantle` CLI. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Each version is tagged `vX.Y.Z`
and published on [GitHub releases](https://github.com/anasgets111/mantle/releases). While the
version is 0.x, a minor release can break the Lua API. A `Breaking:` entry needs a config edit.

## Unreleased

- Fixed: a transparent container (a `rect`, `row`, `column` or `list` with no `background`, `border_width`, `effect.backdrop`, explicit `hittable = true` or input property), an unpainted `path`, or a bare `shader` over a sibling such as a click-outside catcher no longer swallows pointer events where none of its children sit; clicks pass through to the sibling beneath it.
- Fixed: a node with `effect` (blur, shader, colour filter, `blend` or content shadow) under a `scale`, `rotate` or `translate` ancestor is no longer cut to the surface's untransformed bounds, so content scaled down or shifted into view is not clipped early.
- Added: `column_span` and `row_span` (integers `[1, 256]`, default `1`) let a child of a `homogeneous` `row` or `column` span multiple slots across columns, rows, or both when wrapped.
- Fixed: a `homogeneous` `column` now applies each child's `align_h` horizontally and lets `height = "fill"` fill its vertical slot instead of treating `align_h` as vertical alignment.
- Fixed: a keyframe run or tween on `effect` no longer restarts on unrelated passes when `effect.shader` shares a `params` or `images` table across frames or passes, or after a tween finishes without an explicit `progress`.
- Fixed: the polkit authentication agent now registers when Mantle starts without `$XDG_SESSION_ID` (such as from a terminal, script or `systemd-run`), via logind's `auto` session lookup; `pkexec` used to fail with no authentication agent.
- Fixed: `mantle.notifications:set_sound(urgency, "")` clears the configured sound for that urgency tier (a sender's own `sound-file` still plays); an empty path used to raise as a relative path.
- Fixed: content-sized popups opened from a panel use that panel's output size as their layout bound.
- Fixed: the type stubs read a `Rect`'s fields as numbers, accept a `geometry` rect as `anchor_rect`, and give `os.date` a string result except for `"*t"` and `"!*t"`, so lua-language-server no longer flags `rect.width` arithmetic or `os.date("%H:%M"):gsub(...)`.
- Fixed: a press on another of the shell's surfaces, such as the bar, a full-screen modal or the desktop, now closes a grabbing popup on niri and Hyprland, which closed it only for a press on another app. Its `on_dismiss` runs, then the press reaches its target, except a press on the popup's own opener (inside its `anchor_rect`), which only closes it.
- Fixed: opening a grabbing popup while another is shown, such as a second panel opened from a click on the bar, no longer ends the shell on niri with "xdg_popup was not created on the topmost popup". The other popup closes first and its `on_dismiss` runs.
- Added: `mantle.network_profiles` lists saved NetworkManager profiles with their IP method, addresses, gateway, DNS servers and search domains, autoconnect and metered settings, and whether each is active. `mantle.network:update_profile(uuid, edit)` changes only the fields `edit` names, such as `{ ipv4 = { dns = { "1.1.1.1" }, ignore_auto_dns = true } }`, and saves the profile (NetworkManager 1.44+); `reapply_profile`, `activate_profile`, `deactivate_profile` and `delete_profile` act on a profile by UUID. A failure lands in `network_profiles.error`.
- Fixed: `vpns` and the saved mark on `available_networks` follow a profile edited after startup, such as one renamed with `nmcli`; they kept the old values until another profile was added or removed.
- Fixed: a key retyped with `network:connect` for a saved Wi-Fi network keeps the profile's DNS-over-TLS servers; saving the key dropped them.
- Changed: `network.vpn_error` for a VPN that NetworkManager refuses to start carries NetworkManager's message, such as why the profile cannot start, instead of "connection failed".
- Fixed: a `text` that fits its width no longer cuts glyph ink past its box sideways, so a script or italic face's last glyph (Caveat's "d") paints whole without right `padding`. Text wider than its box is still cut at the box's width, and a `textfield`'s line at the field's.
- Added: `strikethrough = true` on a `text` content run draws a line through it in the run's colour, following the run across wrapped lines.
- Fixed: an underline, a link or a `textfield` selection on a letter a font joins into a ligature with a plain neighbour (the `i` of `fi`) covers that letter's share of the ligature. The underline and selection drew nothing there and the link could not be clicked.
- Added: `path` takes `taper_start` and `taper_end`, lengths in pixels over which each open stroke end narrows to one physical pixel, for pen and brush lines. They follow `trim_start`/`trim_end` and animate.
- Added: `fonts { files = { "fonts/Caveat.ttf" }, "Caveat" }` registers font files a config ships, so it needs no font installed. A relative path is read from the config directory. Their families take those files over any installed copy, in the chain and in a `text` node's `font`; list every weight and style file you use. `mantle check` measures text in the declared fonts and warns about a font file it cannot read.
- Added: `image { fit = "tile" }` repeats the picture from the box's top-left corner at one image pixel per logical px, for a paper, fabric or noise texture over a box of any size. `capture.fit` refuses it.
- Added: a box `shape` takes up to 41 holes: after its closed contour, more closed contours whose `M` sets `hole = true`. Fill, border, shadows, clip, hit testing, the `compositor_blur` region and `mantle_sdf` leave the union of the holes empty, overlaps and parts past the outline included, and `ring` strokes each hole's rim too. A shader reading the `mantle_shape` uniform directly finds, when a holed shape leaves more than one loop, each loop closed and followed by a point at `(1e9, 1e9)`; skip any edge that touches one. A hole across the edge that only notches the outline leaves one loop and no such point.
- Added: `path` takes `stroke_dash`, a list of dash and gap lengths in px, and `stroke_dash_offset`,
  which tweens. Dashes keep their place on the path under `trim_*`.
- Added: boxes take `border_dash` and `border_dash_offset`, the same pattern run round the border on
  any radius, corner shape, `shape`, per-edge widths and per-edge colours.

## 0.15.0 - 2026-11-22

17 fixes, 8 changes, 3 additions and 3 breaking changes. Read Breaking first; each entry names the edit.

### Breaking

- Breaking: a surface `id`, `output`, `namespace`, `parent`, `title` or `app_id` containing a NUL byte or longer than 1024 bytes is refused as a config error, which `mantle check` reports. A signal-bound `title` or `app_id` that does reads its default instead, logged to `mantle log` and `mantle.rescue`. A NUL used to end the shell, and a window title past about 4 KiB stopped it until restarted. Strip NUL bytes and shorten a longer name.
- Breaking: an SVG with a DTD fails to load if an entity's value holds `&`, `%` or `<`, or if it declares more than 32 entities per 8 MiB of file (256 in a 1 MiB file); plain-text entities such as the namespace and style names Illustrator declares still load. Counting each `&` as the longest entity value, the file must stay under 8 MiB, and the SVGs it embeds under a separate 8 MiB they share. In a file with a DTD, the text `<!ENTITY` anywhere, even in a comment, is checked as a declaration. Such entities could expand a small file without bound, running the shell out of memory, or stall it for seconds. Export the SVG without entities, or with them expanded, to load it.
- Breaking: `process.run`, `process.capture` and `process.detach` raise when `args` has a `nil` before its last entry, or a named key, and name the entry. `{ "-u", urgency, title }` with `urgency` nil used to run the command with only `-u`, silently dropping the rest. Give the value a default (`urgency or "normal"`) or leave it out of the list.

### Lock

- Fixed: a lock asked for while the shell is starting or replacing a crashed Renderer, such as `loginctl lock-session` or `mantle.lock:lock()`, locks once the new Renderer has loaded its config; it was lost, leaving the session unlocked. A Renderer that crashes while taking, holding or animating out the lock may leave the compositor locked, so its replacement asks for the lock again, and a correct password typed as it crashed no longer unlocks the replacement. A takeover the compositor refuses is retried after 1 s, doubling to 30 s. A shell restarted while a lock was being taken, or unable to read its lock record, locks again, since the compositor may still hold the lock. A lock asked for while the compositor was ending the current one is no longer dropped.
- Changed: `mantle.lock:lock()`, `loginctl lock-session` or a suspend during the unlock animation keeps the session locked: `mantle.lock.unlocking` turns back to `false` and the password prompt returns. The session used to unlock anyway, and a suspend then locked it again.
- Fixed: a suspend before the shell has loaded its config waits up to 3 s for the lock screen; it went ahead unlocked.
- Fixed: a Renderer that hangs while a lock or a suspend waits on it, before saying whether its config can lock (20 s), taking the lock (10 s) or releasing it (5 s), is restarted, as is one that can no longer be sent a lock, and its replacement asks for the lock again. Every later lock and unlock used to wait on it forever.

### Surfaces and input

- Added: shell surfaces take drops from other apps. `on_drop(drop)` receives the dropped files as absolute paths (`drop.files`), every URI (`drop.uris`) or plain text (`drop.text`), and `drop_hover = drop_hover(name)` is `true` while a drag is over a node.
- Fixed: Escape in a `textfield` with `on_cancel` (or `escape = "blur"`) leaves the field until its surface next gets the keyboard, its panel's `keyboard_interactivity` turns on, or the field hides and shows again. On an `on_demand` panel its `autofocus` used to take the keys straight back and call `on_change` with the seed text again. `focused(name)` now turns `false` at the Escape; it used to stay `true`.

### Layout, lists and paths

- Changed: a `path` may keep what it read of a literal `commands` table with no signal or metatable in it while the node's declaration stays the same. Editing that table in place is no longer reliably drawn when one of the path's bound properties, such as `shift`, changes; bind `commands` to a signal and `:set` the edited table. A signal inside a literal `commands` still updates it, and declaring the node again with any value changed reads it again.
- Fixed: a `path` with a long literal `commands` list no longer reads the whole list again each time one of its bound properties, such as `shift`, changes. 100 paths of 250 points with `shift` bound to a signal written every 16 ms used all of one core and updated 12 times a second; they now update 62 times a second at 12% of one core.
- Changed: scrolling a narrow list of long wrapped text, such as notification bodies in a slim panel, uses less CPU: 2.4% instead of 3.9% on a 350 px wide list of 500 rows of 2,000 characters. Its peak memory is 3 to 4 MiB higher.
- Fixed: when a long scrolling `list` gets wider or narrower (taller or shorter, for a horizontal one) and its rows change length, its scroll range follows the new lengths: at once for a direct change, and after a `width` animation once it ends. When the rows got shorter it used to keep the old, longer range, so the wheel could scroll past the last row until the list next laid out. The offset does not move unless it is past the new end.
- Fixed: the text layout the shell keeps is capped at about 32 MiB. Scrolling a list of long text, such as long notification bodies, uses less CPU and memory: 2.2% instead of 3.5% and 125 MiB instead of 131 on a 500-row list of 2,000-character rows.
- Fixed: a gradient whose last stop sits below 1, such as `{ { 0, "#FF0000" }, { 0.5, "#0000FF" } }`, holds the last colour out to the end, as CSS does. Past that stop it painted nothing, so a background showed through, a border edge vanished and a mask hid the node.

### Effects

- Fixed: an `effect.shader`, or a `shader` node inside an `effect`, that reads its size (`u_size`) or outline (`mantle_sdf`) redraws after the output scale changes. Where the node and its contents kept their pixel size, such as a full-screen one when the scale goes from 1 to 1.5, it kept showing the picture drawn at the old scale.
- Fixed: nested `effect.shader` nodes with `opacity` below 1 fade their subtree by the product of their opacities when the GPU refuses their offscreen textures. The inner layer used to replace the outer fade, so two at 0.5 drew at 0.5 instead of 0.25.
- Fixed: a surface with `clip = "none"` holding a rotated, scaled or moving node under an `effect` such as `blur` no longer ends the shell on its first paint, and on every restart after it.

### Images and icons

- Changed: an animated GIF, WebP or APNG shown at its own size, or in a box larger than it, holds its frames in less memory while it plays, so a long clip keeps more of its frames within the memory budget. For example, a 100-frame 480x270 GIF where every pixel changes held 51 MB of frames and now holds 13 MB, and the shell drawing it uses 117 MB instead of 154. That clip takes about 20 ms longer to load, on the thread that draws the shell unless the `image` sets `async = true`, and 0.13% more of a CPU core while it plays; a longer clip, keeping more frames, can take longer still to load.
- Changed: an icon name is looked up in its theme's `icon-theme.cache` when the cache is at least as new as the theme directory, as GTK apps do. A name no theme has, such as an unknown notification `app_icon`, costs about 2 ms instead of 6 ms on a theme that inherits from three others, and an installed name about two thirds as long. A file dropped into an existing icon directory leaves the cache current, so it draws only after `gtk-update-icon-cache -f <theme dir>` and a shell restart. In such a theme an `.svgz`, `.ico` or `.bmp` icon is found only in a scalable directory.
- Changed: an animated GIF saved with every frame full size, as many tools write them, holds the part of each frame that changed while it plays, not a whole frame, even when a frame clears its area when it ends. A 50-frame 480x270 clip of a moving shape from ImageMagick held 26 MB and now holds 1.4 MB; a 100-frame 480x270 moving-square clip whose frames clear each time held 51 MB and now holds 0.45 MB. A frame that restores its area when it ends still makes the next frame hold all of that area.

### Capabilities

- Added: apps that hold sleep via `org.freedesktop.PowerManagement` (such as Chromium-based browsers) now keep the machine from suspending and show in `mantle.idle.sleep_inhibitors`.
- Added: `mantle.idle` exposes logind `sleep` block inhibitors as `sleep_inhibited` and `sleep_inhibitors` (excluding `delay` inhibitors). Idle thresholds still fire while one is held.
- Fixed: `balance` on `mantle.audio` sinks and sources reads as set, such as `0.1`, not as `0.10000000149011612`.
- Changed: `mantle.notifications.feed` holds the newest 200 notifications, up from 20, and the server keeps up to 1,000, up from 100, before dropping the oldest.
- Fixed: on Arch, two shells in one login, such as a test shell beside your own, no longer run `mantle.updates` checks at the same time. One could store the other's half-downloaded repo database, even after the shell that started the download exited. A check that starts while another shell's download runs fails with `check_error`; a scheduled one retries on the failed-check backoff, which starts at 5 minutes.
- Fixed: a tray app sending a huge icon in its item properties or menu, or a media player sending large binary data in its track metadata, no longer causes a memory spike in the shell.
- Fixed: a notification action whose key is longer than 256 bytes is left out of `actions`. A sender could pass megabyte keys, which the shell kept for every queued notification and which could stop `mantle.notifications.feed` from updating.
- Fixed: a media player reporting a nonsense playback position, or a PipeWire client sending a malformed audio parameter, no longer ends the shell, taking a locked session's lock screen with it.

### CLI and supervisor

- Fixed: a shell started with `mantle -d` from a terminal app, such as an editor's built-in terminal, keeps running when that app quits. Under a systemd user session it now runs in a scope of its own.
- Changed: an idle shell wakes 2 fewer times a second and runs 2 fewer threads.

## 0.14.0 - 2026-11-18

The "turns out there were a lot of bugs" release, also known as the "Mythos 5.2 happened" release: pointed at the codebase, it kept finding things. 109 fixes, 8 breaking changes, 3 changes, and one lonely new feature (locking before suspend). Read Breaking first; each entry names the edit. Cursed SVGs now fail to load instead of eating your RAM.

### Breaking

- Breaking: `mantle.storage:open`, `mantle.storage:set`, `palette.quantize`, `mantle.notifications:set_sound` and `mantle.android_connect:send_files` raise on a relative path. They used to do nothing, or for `palette.quantize` resolve against a working directory nothing sets. Give an absolute path, or one starting with `~/` (not for `send_files`); build a storage or `palette.quantize` path from `mantle.config_dir`. `set_sound` still plays only files under `/usr/share`, `/usr/local/share`, `/opt` or `$XDG_DATA_HOME`.
- Breaking: names passed to `hover`, `focused`, `focus_visible`, `pointer`, `scroll` and `geometry` may be freed once no node binds them and no Lua value holds them, so names built from changing ids no longer grow memory. A freed name starts fresh: a node removed rather than hidden, or one only an unplugged output showed, can lose its scroll offset and `hover_rect`. List a `scroll` in `reset_on_close` for a reset you can rely on.
- Breaking: a capability action argument, a `process.detach` option or a `mantle call` answer that `json.encode` refuses raises instead of being sent. That covers a table indexed from 1 that is more than half holes, such as `{ [12] = true, [97] = true }`, which was sent as an object keyed `"12"` and `"97"`; a `json.decode` array given a named key; and a NaN or infinity inside a table, which was sent as `null`. Key sparse tables by string (`{ ["12"] = true }`) or list the values (`{ 12, 97 }`).
- Breaking: `opacity` fades an `effect.shader`'s output, so a node at `opacity = 0` no longer leaves alpha its shader adds (an outline or a glow) on screen, and at 0.5 the whole effect is half strength. The program now reads the subtree before the node's `opacity`, so a branch on input alpha sees it unfaded, and overlapping children fade as one group. To keep a shader's output unfaded, move `opacity` to a child that holds the content.
- Breaking: an SVG draws only the filter effects Qt draws: `feGaussianBlur`, `feOffset`, `feFlood`, `feMerge`, `feMergeNode`, `feComposite`, `feColorMatrix` and `feBlend`. An SVG with any other `fe*` element fails to load; some, such as `feTurbulence`, could abort the Renderer. Extreme values in the accepted ones, and CSS `filter` blurs and drop shadows, still can. No installed icon uses another; re-export your own SVG without it.
- Breaking: an SVG shown by an `image`, `icon` or mask that would decode an embedded picture more than once, through `<use>`, a marker, mask, clip path, pattern or filter, fails to load. Re-export it so the picture is drawn once.
- Breaking: an SVG whose `<image>` links a picture by anything but a `data:` URL, such as an absolute path, draws without it, and the log names the SVG. Embed the picture as a `data:` URL.
- Breaking: `capture.live` takes at least 0.001 frames per second, and a smaller positive rate such as `1e-20` is refused at load like `0`. It used to pass the check, then abort the shell on its first capture. Set 0.001 or more.

### Text and fonts

- Fixed: text with characters the loaded fonts lack, such as emoji, symbols or a notification or window title in another script, no longer freezes the shell, about 1 s per 100 such characters. At startup the shell waits up to about 0.1 s for those fonts, so its first frame shows them. After that, such text appears at once with missing-glyph boxes, which fill in as the fonts covering them load, about 15 ms each, so 100 such characters can take 1.5 s.
- Fixed: text already shown switches to a font that loads later and suits it better, as for missing-glyph boxes: bold or italic text drawn by slanting or thickening the regular face takes the family's real bold or italic file once a node names that family, and a joined emoji or a thin space drawn from a face that lacks part of it, such as the joiner, takes a face that has all of it. It used to keep the first face until the text changed.
- Fixed: once a font loads, a long scrolling `list` re-estimates the items after the ones it builds, so its scroll range and `reveal` follow the new font from the list's next layout. The offset does not move. When a font joins a family already loaded, such as the family's real bold file where bold was drawn by thickening the regular face, it re-estimates every such item from the sizes of the items it built; it used to keep their old sizes until each item was shown. When a font for missing-glyph boxes loads, it re-estimates them instead of keeping the box widths they showed until each was shown; the items before the ones it builds do not move either, and built items that showed boxes take their new size.
- Fixed: a `text` with `wrap = "word"`, and a `multiline` `textfield`, break a word wider than the box inside the word, as documented. It used to stay on one line, cut off at the box edge; with `max_lines` and `elide` it showed no ellipsis and `elided` stayed `false`.
- Fixed: a `text` with `wrap = "word"` and left or right `padding` is as tall as the lines it draws, and a content-sized one stays on the line it was measured at. Its last lines used to draw over whatever sat below. A multiline `textfield` with padding grows the same way.
- Fixed: a `text` without `wrap = "word"` ignores `max_lines`, as documented. Its box used to stop at `max_lines` lines while every line still drew, over whatever sat below.
- Fixed: a content-sized `text` with `elide` and left or right `padding` shows its whole content, and `elide = "start"` or `"middle"` no longer drops a character when layout rounds the box a hair narrower. About one width in twenty used to lose its last character to an ellipsis.
- Fixed: a content-sized `text` with `wrap = "word"` and `max_lines` is as wide as the lines it shows, plus the ellipsis an `elide` adds. It used to be as wide as its widest line, dropped ones included.
- Fixed: a `text` whose content holds `\n\r` draws the empty line between them, as its box already counted.

### Text fields and keys

- Fixed: a field whose `on_cancel` turns its panel's `keyboard_interactivity` to `"none"` on Escape no longer has the panel's `autofocus` field re-armed before the compositor takes the keyboard away, which called that field's `on_change` a second time with its initial text.
- Fixed: a multiline `textfield` whose `font_size` or other font property changed resizes when the next edit changes its row count. It could keep its old height until a later edit.
- Fixed: Ctrl+A, Ctrl+Z, Ctrl+Y, Ctrl+X, Ctrl+C and Ctrl+V in a `textfield` follow the letter the layout types. Under Dvorak or Bépo, Ctrl+X, Ctrl+C and Ctrl+V used to act on the QWERTY positions, and a key typing punctuation, a digit or an accented letter could paste or undo. Layouts without Latin letters, such as Russian, still use the QWERTY keys.
- Fixed: a paste into a `textfield` lands unless that field was edited, its caret or selection moved, its `masked` changed, or it lost focus while the clipboard was read. Scrolling a `multiline` field, an input method switching, or an edit in another field used to drop it.
- Fixed: a focused `textfield` calls the `on_change`, `on_submit` and `on_cancel` its node declares now, and follows its current `escape`. After a rebuild or reload replaced them, it used to call the ones it had when focused. Removing a handler stops its calls, and a signal giving a focused plain field a `secure_submit` target stops keys reaching its old handlers.
- Fixed: a key held while the surface with the keyboard is hidden, such as one whose `on_key` hides its own popup, stops repeating. It used to repeat into the next focused field, and its release could reach an `on_key_release` it was never pressed in.
- Fixed: a multiline `textfield` you scrolled keeps the same text at its top when a font for missing-glyph boxes loads, or its width, `font_size` or `line_height` changes. It used to keep its pixel offset, so another row showed at the top.

### Surfaces, popups and focus

- Fixed: a reload that changes several surfaces sends them to the compositor together, with any surface it removes, so they change on the same frame. A surface still waiting for the compositor's frame callback shows its change a frame later. Its surfaces used to reach the screen one or two frames apart, and a removed one vanished up to four frames before the rest changed.
- Fixed: a panel whose `keyboard_interactivity` is bound and turns on from `"none"` focuses its shown `autofocus` field, so `mantle input <panel> type` works before the compositor hands it the keyboard. Its `focused(name)` then stays `true` while the panel waits for the keyboard, unless another shell surface holds it or `keyboard_interactivity` returns to `"none"`. It used to stay unfocused until the panel got the keyboard, usually from a click. A field typing in another shell surface keeps the keys until the panel gets the keyboard, then keeps its draft.
- Fixed: a shown panel whose `keyboard_interactivity` turns from `"none"` to `"on_demand"` is remapped, so a compositor that focuses such a panel only when it appears (Hyprland, niri, sway, river) can hand it the keyboard, and typed keys reach its `autofocus` field instead of the focused app. sway does so only for `"top"` and `"overlay"` panels, Hyprland not under a pointer constraint. An `output = "active"` panel may move to the output the compositor picks. With the pointer on an app window, every copy of an `output = "all"` panel is remapped, and the compositor picks which one gets the keyboard. A panel that reserves an `exclusive_zone` or has a popup open or opening is not remapped, nor one under the pointer: a flip made while the pointer rests on the panel, including a click inside it, gets the keyboard on the next click. Use `"exclusive"` to take it at once.
- Fixed: a `grab = false` popup shown together with its parent panel, or on a panel just remapped because its `keyboard_interactivity` turned on, opens once the compositor has configured the panel. It used to open on a panel with no size yet, so the compositor could misplace or refuse it. A `grab = true` popup still opens at once, since its click only counts that turn.
- Fixed: a popup with no `width` or `height` that is `visible` when the shell starts opens once its parent panel is configured. It used to stay closed, logging "sized (0.0, 0.0), so it is not opened yet". A popup declared before its parent panel no longer warns at startup that its `parent` names no surface that is shown.
- Fixed: a popup opened from a panel on one output stays open when that panel hides on another output.

### Layout, lists, signals and animation

- Fixed: `hover(name)` turns false when its node is removed or loses its `hover` property, keeping its last `hover_rect`, and `mantle input` moving to another surface clears the previous one's hover.
- Fixed: a `geometry()` signal, and signals derived from it, keep their previous rect when a layout pass fails or a reload is rejected. They used to take the rejected pass's rect.
- Fixed: a `geometry()` reader hears a rect that settled while an endless `opacity` or other paint-only loop on the same node still runs.
- Fixed: a non-boolean `hittable`, such as `"yes"`, is a config error naming the property. It used to crash the shell, and `mantle check` passed it.
- Fixed: a `row` or `column` with a set `width` or `height` and a `homogeneous` signal flipped to `true` gives its children equal shares.
- Fixed: the items a `list` keeps follow a `direction` signal that flips. A `"fill"` item used to keep the old axis.
- Fixed: a `list` with `scroll` whose `height` a tween grows past its built rows builds the missing rows on the next pass. They used to show blank until another pass.
- Fixed: a `width` or `height` signal that resolves to nothing on a reused `list` item follows the content again once its tween settles.
- Fixed: `animate` on `spacing` and `line_spacing` eases between negative values. It used to clamp them to 0.
- Fixed: a `font_size`, `stroke_width` or other number in an `animate.exit` that the node never set starts from the property's default, not from 0.
- Fixed: `delay` and `pulse` over a signal that builds a new table on every read settle. A table counts as unchanged when its entries are equal plain values or the same function or userdata; one they cannot compare, such as a subtable of the last one or over 256 entries, is taken when its hold or window ends. The `delay` used to wake the shell once per hold forever, and the `pulse` to read true forever.

### Effects and blur

- Fixed: on Hyprland, a popup with `compositor_blur` no longer shows a hard-edged blurred rectangle over the whole popup while its glass is hidden: faded below half opacity, switched off, or still fading in from 0. This covers popups of panels under a `blur_popups` layer rule and of windows under `decoration:blur:popups`. One blurred logical pixel remains at the popup's top-left corner meanwhile; an `ignore_alpha` layer rule should hide it (untested).
- Fixed: a panel with `compositor_blur` whose glass starts hidden, such as a card fading in from 0, is no longer blurred whole by a blanket compositor blur rule (Hyprland's `layerrule blur`) until the glass shows. Such a panel now opts out of the blanket rule from its first frame, as it already did once its glass had shown.
- Fixed: a `compositor_blur` box whose `opacity` fade-in interrupts an unfinished fade-out with a different `animate` entry (duration, easing or a spring), or that leaves through `animate.exit` early in its fade-in, switches its blur at half the fade's peak. It used to switch it on at once, at an opacity as low as 0.03, or as the exit began.
- Fixed: a shader with `input = "desktop"` reads under opaque paint (text, a border, a card) as the nearest desktop pixel above, below or beside it, instead of repeating each row's edge pixel across the block in horizontal streaks. A row fully covered by paint now shows the desktop from nearby rows instead of nothing.
- Fixed: a `compositor_blur = true` box whose `opacity` tweens in from 0 or out to 0, keyframes included, gets the compositor blur once its opacity passes half the highest value the tween reaches, and loses it there on the way out. It used to blur at full strength from the first frame above 0. A box at rest, or fading between two values above 0, blurs as before; use `effect.backdrop` for blur that fades.
- Fixed: a node with `effect`, `clip = "rounded"` or `mask` larger than the GPU's largest texture paints inside its parent's clip without that effect, clip or mask. It used to vanish or paint unclipped. A node whose `mask` is another node draws nothing at that size.
- Fixed: an `effect` whose `shader` table a getter rebuilds each pass no longer restarts its running tween.
- Fixed: a node with an `effect` layer, such as `blur`, and a backdrop `shader` with `padding` draws the shader's reach when the box itself is clipped out.
- Fixed: tearing down a `lock` surface with `compositor_blur = true` before its first configure, such as when the compositor refuses a lock, no longer disconnects the shell.

### Images, SVG and capture

- Changed: an `image` showing an SVG or SVGZ file decodes on a worker whatever `async` says, so it appears a frame or more after the first instead of freezing the shell while it draws. `icon` is unchanged.
- Fixed: an `image` that decodes on a worker (`async = true`) into a box of another shape than its picture, such as a 16:9 photo with `fit = "cover"` in a square, draws equally sharp on every open. Later opens used to draw a smaller cached thumbnail, blurrier and cropped wider.
- Fixed: an `image` with `source_blur` that decodes on a worker keeps drawing its previous size, scaled, while a resize decodes. It used to vanish for the length of a size animation.
- Fixed: an `image` with `async = true`, or `palette.quantize`, rereads a file replaced within the same second as its thumbnail when its size changed. Thumbnails written before this version, or by programs without `Thumb::Size`, are still trusted on their timestamp alone.
- Fixed: an `icon` or SVG `image` drawn larger than 8192 px a side, such as `icon { size = 30000 }`, renders at 8192 px and scales up, as a raster image already did. It used to rasterize at full size, 3.6 GB for that icon, and could crash the shell.
- Fixed: an SVG shown by an `image`, `icon` or mask fails to load instead of exhausting memory or overflowing the stack. The limits: 8 MiB compressed, inflated or entity-expanded, gzipped once, 256 levels of elements or `<use>` hops, no `<use>` cycle, 256 clip paths, masks and markers, 16 patterns. Embedded `data:` pictures draw blank past 8192 px a side or 67 megapixels in total, as does an SVG nested more than one level or past the shared 8 MiB budget.
- Fixed: an icon theme's `index.theme` or GTK's `settings.ini` that is a FIFO or over 1 MiB reads as absent. A FIFO there used to freeze the shell.
- Fixed: a `capture` of a rotated or flipped output draws upright through `ext-image-copy-capture-v1` when the compositor reports the frame's transform, and through `wlr-screencopy` when the output is turned 90 or 270 degrees (flipped or not), as on niri and Pinnacle. It used to draw sideways or mirrored, and an ext `region` on it showed the whole output with a warning. A wlr capture of an output turned 180 degrees or only flipped still draws as the compositor sends it.
- Fixed: a `capture` node whose GPU buffer the compositor refuses switches to the slower CPU copy and logs a warning. It used to disconnect the shell, which restarted and hit the same refusal on every start.
- Fixed: a compositor reporting a capture size or damage rectangle too large for a buffer no longer aborts the Renderer; that capture is refused.

### Capabilities

- Fixed: a media player or an app holding the screen awake can no longer restart the shell in a loop with a huge title, art URL or reason. `mantle.mpris` lists at most 8 players, picking up a left-out one when another leaves. Track title, artist, album, album artist and genre stop at 1024 bytes, other text at 256; track and playlist entries with longer ids are left out; an `art_url` or `url` over 4096 bytes is ignored. `mantle.idle.inhibitors` text stops at 256 bytes and lists at most 64 logind holders, and a `ScreenSaver` client past 16 holds, or 64 in all, gets an error. A state still too large to send is skipped until it fits.
- Fixed: a media player that stops answering no longer keeps other players out of `mantle.mpris.players` for up to about 100 seconds.
- Fixed: `mantle.mpris` actions sent to one player run in order, so `seek` then `control("next")` no longer lands on the next track. A player that stops answering holds up only its own actions; past 32 waiting, new ones are dropped with a warning.
- Fixed: `mantle.mpris` reports `length = nil` for a live stream in Firefox or Zen, as for any player without a length. It used to report Firefox's "infinite" marker, which drew as about 2.5 billion hours.
- Fixed: `mantle.mpris` reads a track `length` sent as an unsigned integer. Such tracks used to have no `length`, and a seek past their end was not clamped.
- Fixed: `mantle.mpris:seek` on a player without `mpris:trackid` does nothing when the player cannot report its position. It used to seek from a stale position, seconds off.
- Fixed: when another tray host such as waybar exits and Mantle takes over, running tray apps register with Mantle and show in `mantle.tray.items`. They used to stay hidden until restarted.
- Fixed: a tray item that changes its icon, title, tooltip, status or menu while the shell first reads it shows the change.
- Fixed: a tray item from a Qt app that sets its menu after showing its icon gets that menu, and `menu` turns `nil` when the app takes it away.
- Fixed: `mantle.tray` caps the tray at 32 items, 512-byte object paths and 4096 menu nodes across all items, so a flooded session bus cannot push it past the frame limit. An item past 32 waits and enters once another leaves; up to 64 wait, and later ones are dropped. Each app's first refused item logs a warning.
- Fixed: `mantle.network` joins and forgets a Wi-Fi network whose name is not UTF-8, listed with `�` in `ssid`, and no longer mixes up networks whose names differ only in those bytes. Joining used to save an unusable profile, re-ask a saved password or join the other network. When two networks in range share a name, `connect` fails with "several networks share this name" and `forget` deletes nothing.
- Fixed: `mantle.network:connect` and `connect_vpn` wait 90 seconds, up from 45, before failing with `connection timed out`, and a timed-out Wi-Fi join is stopped. It used to keep connecting after showing the error, leaving a retyped key unsaved.
- Fixed: `mantle.network.vpn_secret` clears when NetworkManager restarts. It used to stay up for 2 minutes and block the restarted NetworkManager's own secret requests.
- Fixed: `mantle.bluetooth:pair(mac)` no longer fails after 25 seconds while a device waits for you to confirm a code, type a PIN or press a button. A pairing unfinished after 90 seconds is cancelled.
- Fixed: `mantle.bluetooth:forget` removes a device under a second Bluetooth adapter. A device paired to both adapters still lists twice.
- Fixed: virtual devices from `set_virtual_devices`, except null sinks, appear when PipeWire starts after `mantle`, as at a login where PipeWire is late. They used to stay missing until PipeWire restarted.
- Fixed: `mantle.audio` falls back to the lowest-id sink and source when PipeWire's default-device metadata is cleared, such as with `pw-metadata -d`.
- Fixed: on Hyprland, `mantle.workspaces` and `mantle.windows` reconnect after the event socket drops or is not up at start. They used to freeze until a restart; while disconnected they are empty.
- Fixed: on Hyprland, `mantle.workspaces:focus` and `mantle.windows:move` accept a workspace number with a leading `+`, such as `"+3"`.
- Fixed: on sway and Hyprland, a window title that is not UTF-8 no longer empties or freezes `mantle.workspaces` and `mantle.windows`; the bad bytes show as `�`.
- Fixed: a notification body image given as a `file://` URI under a trusted icon directory shows, as a plain path does.
- Fixed: a notification picture sent as raw pixels with padded rows, as GTK apps send an RGB image whose width is not a multiple of 4, shows in `image_path`.
- Fixed: `mantle.applications` starts a program in the directory its `.desktop` file names in `Path=`, as Wine and Steam launchers expect.
- Fixed: `mantle.applications` reads the `\s`, `\n`, `\t`, `\r` and `\\` escapes in `.desktop` values, so a Wine launcher's Windows path keeps one backslash per `\\`.
- Fixed: `mantle.applications` skips a FIFO or device named `*.desktop`. Scanning one used to block forever.
- Fixed: an update started with `mantle.updates:install()` keeps running if the shell restarts or crashes, and survives output that is not UTF-8. Its full output goes to `$XDG_RUNTIME_DIR/mantle/install.log`; `install_log` shows the last 200 lines, each cut to 4 KiB, bad bytes as `�`. After a restart the shell does not show that update, and `install()` fails until it and any process it left running exit.
- Fixed: `mantle.updates:install()` is no longer refused after an install that left a process running, such as a build daemon an AUR helper started. It used to be refused until that process exited.
- Changed: a failing scheduled `mantle.updates` check retries after 5 minutes, then 10, 20 and so on up to the configured `interval`, instead of every 5 minutes, so an offline machine stops running `curl` 12 times an hour. A machine back online can wait up to one `interval` of awake time for the next scheduled retry; `check()` still runs at once. A successful check returns to the usual schedule.
- Fixed: `mantle.updates:configure({ interval = ... })` counts time asleep, so a daily check on a laptop that sleeps overnight runs daily.
- Fixed: on Arch, a `mantle.updates` check slower than 10 minutes no longer leaves a second check running beside it, which could store a half-downloaded repo database and fail later checks. `checking` stays `true` until it ends; each repo database download gives up after 10 minutes instead.
- Fixed: with `aur = true`, `mantle.updates.packages` leaves out AUR packages that `IgnorePkg` names, as it did for repo packages.
- Fixed: a video holding the screen awake through `org.freedesktop.ScreenSaver` keeps its hold when another app releases a hold logind refused, so the screen no longer idles mid-video.
- Fixed: a `mantle.processes` program paused with `SIGSTOP` is continued when stopped, so it can handle its stop signal, such as a recorder finishing its file. At shutdown the programs stop together.
- Fixed: `mantle.sysinfo.gpu` samples `nvidia-smi` before `nvtop`, and rounds fractional `nvidia-smi` temperatures instead of reporting `temp` as `nil`.
- Fixed: `mantle.sysinfo` `cpu_percent` no longer counts guest time twice, which read low while virtual machines ran.
- Fixed: `mantle.sysinfo.disks` keeps its last list when `lsblk` prints something unreadable.
- Fixed: without the `input` group, `mantle.keyboard`'s layout or backlight no longer reverts to an older value when a `caps_lock`, `num_lock` or `scroll_lock` poll lands at the same moment.
- Fixed: the first `persistent_table` for a file reads it in the background, so a slow or network home directory no longer stalls every other `mantle.*` capability while it reads. The store reads its defaults until the file lands; `set` calls made meanwhile are kept and win over the file.
- Fixed: a `persistent_table` whose `defaults` change reads the new defaults after a reload for keys the file lacks. A key already read kept the old default until a restart.
- Fixed: an in-place reload stops `mantle.folders:watch` watches that `shell.lua` no longer declares and drops them from `mantle.folders.watched`. They used to keep pushing changes until a restart. Kept watches keep their listings instead of resetting `ready`.

### Lock, security and privacy

- Added: a config with a `lock` locks before the machine suspends, from the lid, `systemctl suspend` or anything else. The suspend waits up to 3 s for the lock screen to show, then goes ahead. A suspend while the shell restarts after a crash waits the same way. It used to suspend unlocked, so the desktop showed on resume.
- Fixed: a second shell in the same login, such as one in a nested compositor, no longer changes whether the first shell relocks after a restart. A shell restarted while locked across this upgrade may not relock that once.
- Fixed: a PAM module slower than 30 seconds, such as a slow second factor, sets `mantle.lock.error` to `authentication error: the module did not answer in time; try again`. It used to say authentication is unavailable.
- Fixed: logs and errors no longer quote a value that could be a password or a Bluetooth PIN. A message between `mantle` and its shell process that fails to decode is logged by its position only. A capability action's argument error names the argument's type, not its value: `bad argument #2: expected a number, got string`. The `process` log lines no longer print a command's arguments or a message's contents.
- Fixed: `mantle` no longer risks killing an unrelated process that reused a hung shell process's pid, 5 s after the hang.
- Fixed: a table holding one subtable many times over, such as 40 levels of `{ t, t }`, raises "more than 1048576 values" as a capability action or `process.detach` argument, fails a `mantle call` answer, and lists by name alone in a bare `mantle set`. It used to exhaust memory and abort the shell.

### CLI, check and supervisor

- Fixed: `mantle check` refuses a property of a panel, window, lock or shown popup that is bound to a signal and resolves invalid in any check pass, such as `exclusive_zone = state("zone", 0)`, naming the line. It used to pass, and the shell logged a warning at start and kept the property's default. Hidden popups are not checked this way, since their `anchor_rect` may be set only when they open. The `exclusive_zone` error now says `0` is `false` and `-1` is `"ignore"`.
- Fixed: after an upgrade that changes how `mantle` and its shell process talk, a running `mantle` logs both protocol versions, stops and starts the installed `mantle` in its place with the same arguments, instead of restarting the new shell process in a loop. A locked session stays locked, and the lock screen comes back. If the installed `mantle` cannot start or still disagrees with its shell process, it exits with status 76; start `mantle` again once the upgrade finishes. Upgrades that keep the protocol respawn as before, and `mantle set` and other CLI commands keep connecting across versions. Applies to upgrades from this version on.
- Fixed: a message from `mantle` that its shell process cannot read under the same protocol version is skipped, with one warning per kind of message, when losing it is safe. It used to restart the shell process in a loop. A lock request it cannot read locks the screen, where it used to leave the session open while the shell restarted. One that arrives after a lock was asked for still restarts it, and the lock screen comes back with a fresh password prompt. An idle event or a kind of message the shell process does not know still restarts it.
- Fixed: on a compositor without `wlr-layer-shell-v1`, `wl_compositor` or `wl_shm`, `mantle` logs the missing protocol and stops. It used to respawn the shell in a loop for the whole session.
- Fixed: a restarted Renderer connects even while idle `mantle` commands hold all 64 control connections. It used to be turned away.
- Fixed: `mantle check` warns about a cut shadow inside a hidden panel, popup or window, laid out as if shown. It used to skip hidden surfaces. One that is hidden in every check pass and whose content fails to lay out shown gets a `hidden <role> "<id>" not checked` warning with the error, such as `hidden popup "menu" not checked`, and does not fail the check.
- Fixed: a failing `mantle check` prints only its diagnostic. An action an `on_change` handler ran during the check used to log an error line first, once per sample pass.
- Fixed: `mantle input <surface> type` on a surface whose shown textfields have neither `on_change` nor `on_submit` says so. It used to answer "no textfield is focused ... press one first", though such a field never takes focus.
- Fixed: `mantle log` keeps recording after the terminal the shell started from closes, and the terminal keeps its copy if the log cannot be written.
- Fixed: `mantle call` fails with "over the frame cap" when the answer is too large to send. It used to wait forever.
- Fixed: live reload resumes after the config directory is renamed away and recreated, or deleted while it held a symlinked subdirectory. Edits used to stop reloading until a restart.
- Fixed: `mantle init` writes a valid `.luarc.json` when the stub directory's path holds a quote or backslash.
- Fixed: `mantle` given an argument that is not valid UTF-8 prints a usage error and exits with status 2. It used to panic.
- Fixed: the type stubs accept a popup `anchor_rect` without `x` or `y`, as the engine does.
- Changed: building from source requires Rust 1.100 or later.

## 0.13.3 - 2026-11-10

- Fixed: `mantle.lock` and `mantle.polkit` publish their idle state at startup, so `computed` and `:map` over them run without waiting for a lock or authentication request. After a restart while locked, `mantle.lock` stays `nil` until the lock is taken again.

## 0.13.2 - 2026-11-10

- Fixed: `icon` nodes use the icon theme set with `gsettings set org.gnome.desktop.interface icon-theme`, read through the desktop portal, and redraw when it changes. They used to fall back to `hicolor` without a GTK `settings.ini`, and a theme change needed a restart.
- Fixed: `mantle.updates` reports the current package and install step counts when pacman runs without a terminal.
- Fixed: `mantle.audio:set_default_sink` and `set_default_source` choices stick. A volume change no longer moves the default back to the highest-priority device.

## 0.13.1 - 2026-11-09

- Fixed: `process.run`'s `on_exit` and `capture`'s `on_done` fire as soon as the command exits. A command that closed its output and kept running used to report its exit up to 100 ms late.
- Added: `image` and `icon` nodes load `.ico` and `.bmp` files, so an app whose only icon is a `.ico` (`/usr/share/pixmaps/<name>.ico`) shows it.
- Fixed: an `image` node's SVG draws its embedded `<image>` PNG, JPEG, GIF and WebP pictures. They used to draw blank.
- Changed: `mantle.keyboard`'s `caps_lock`, `num_lock` and `scroll_lock` stay live without the `input` group, up to 250 ms late. They used to freeze at their startup values.

## 0.13.0 - 2026-11-06

- Added: `mantle.audio.cards` lists each ALSA and Bluetooth sound card with every profile it offers (`index`, `name`, `description`, `available`) and the `active` one, so a config can switch one card between analog and HDMI. Unplugged profiles stay listed with `available = false`.
- Breaking: `mantle.audio:set_bluetooth_profile(device, index)` is now `mantle.audio:set_card_profile(device, index)`, and takes any `cards[].device`. Rename the call.
- Breaking: `mantle.audio.bluetooth` is gone; Bluetooth cards are in `mantle.audio.cards`, with a `mac` to join `mantle.bluetooth`. Each profile carries the `codec` it names, if any, and unavailable ones stay listed: a codec picker reads `profiles` where `codec and available`, instead of `codecs`.
- Added: Each `mantle.audio` sink and source carries its own `volume`, `muted` and `balance`, the `card` (a `cards[].device`; `nil` for a virtual device) it comes from, and its reboot-stable `node_name`, so a mixer can show and set every output, not only the default, and group outputs by card.
- Breaking: `mantle.audio`'s `volume`, `muted`, `balance`, `source_volume` and `source_muted` are gone: read the `active` entry of `sinks` or `sources`.
- Breaking: `mantle.audio:set_volume`, `set_muted`, `toggle_mute` and `set_balance` take a `sinks[].id` or `sources[].id` first: `set_volume(sink.id, 50)`. `set_source_volume`, `set_source_muted` and `toggle_source_mute` are gone; pass a source's id to the same actions. `set_sink_channel_volume` and `set_source_channel_volume` are now one `set_channel_volume(id, index, volume)`.
- Added: `mantle.audio.cards[].ports` lists each card's ports (`index`, `name`, `description`, `type`, `direction`, `available`, `profiles`), and `mantle.audio:set_port(card, index)` switches one, such as speakers to headphones. Unplugged ports stay listed with `available = false`.
- Added: `mantle.audio.apps[].target` is the sink an app plays into, or the source it records from, and `mantle.audio:set_app_target(id, device)` moves it; `nil` returns it to the default.
- Added: `mantle.audio:set_virtual_devices(devices)` declares null sinks, combined sinks, loopbacks, echo-cancelled microphones and filter chains such as an equalizer. Each appears in `sinks` or `sources`, PipeWire's module arguments pass through `args`, and a reload keeps them until the next call.
- Added: `mantle.audio.cards[].ports[].active` marks the port a card routes through now, such as headphones rather than speakers.
- Added: `mantle.audio.apps[].pinned_to` is the device an app is pinned to with `set_app_target`, or by another mixer; `nil` when it follows the default or the device it names is gone. `target` stays the device it plays through.
- Added: Each `mantle.audio.cards[].profiles[]` entry carries `outputs` and `inputs`, how many sinks and sources it makes, so a picker can list output or input profiles without reading their names.
- Added: `mantle.audio:set_level_meters(ids)` meters those sinks and sources, and `mantle.audio_levels` carries their peaks (`levels`, each `{ id, peak }` in percent) up to 30 times a second, for VU meters. No call, no meter; an empty list stops them.
- Changed: `mantle check` fails when a config reads a capability field that does not exist, such as a field a release removed, and names the nearest one; a running shell still reads `nil`. It covers reads made while it evaluates and lays out the config; `on_click`, `action` and `timer` handlers do not run.
- Changed: `mantle check` fails when a capability `on_change` handler raises on sample data; before, it only logged a warning.
- Fixed: A content-sized node with `animate.height` or `animate.width` grows with a child's running animation. A pass between two frames, such as a disclosure's first open, used to freeze it until the child settled, so it stuttered.

## 0.12.1 - 2026-11-05

- Added: Packages point to what fingerprint unlock needs: `mantle-bin` lists `fprintd` as optional, the `.deb` suggests `fprintd` and `libpam-fprintd`, and the `.rpm` suggests `fprintd` and `fprintd-pam`.

## 0.12.0 - 2026-11-05

- Added: The lock screen unlocks with a fingerprint when fprintd has a finger enrolled. Packages install the `mantle-fingerprint` PAM stack. `mantle.lock.fingerprint` is `nil` while fingerprint is off, else `{ messages }` with pam_fprintd's lines. A missed scan never counts as an attempt.
- Added: `mantle.polkit.identities` lists the accounts a polkit request accepts, `identity` is the one it authenticates as (your own when offered), and `mantle.polkit:select_identity(uid)` switches to another.
- Added: `mantle.lock` and `mantle.polkit` gain `prompt` (`{ id, text, echo }`, PAM's own question, `nil` while PAM owes nothing) and `messages` (PAM's info and error lines). Two-factor codes, a re-asked password and fingerprint hints now reach the lock screen. Bind the secure field's `masked` to `not prompt.echo` to show a visible answer.
- Changed: The lock screen starts PAM when the session locks, so `prompt` is ready before you type. A password typed before PAM asks is held for its first masked prompt, and `authenticating` now means an answer is with PAM.
- Changed: A polkit prompt starts PAM on the first submit, so its `prompt` is `nil` until then.
- Changed: The release tarball no longer writes to `/etc`, so an upgrade keeps your edits. It puts the PAM stacks, with `-debian` variants for Debian and Ubuntu, and the polkit rule in `/usr/local/share/mantle/`; copy them to `/etc` once.
- Fixed: After a faillock lockout runs out, the first correct password on the lock screen unlocks. It used to fail once, because the lock kept the PAM attempt that had shown the lockout.
- Fixed: A lock screen no longer refuses a correct password once the account's password has expired. The session is already logged in, as GDM decides.
- Fixed: Polkit prompts in other languages show their real text instead of escape codes, and a visible polkit prompt is no longer answered with an empty line.
- Added: `masked = false` on a `secure_submit` textfield draws what is typed instead of the mask. Turning it off wipes what was typed under the mask.

## 0.11.2 - 2026-10-29

- Added: Animated WebP and APNG images (`.apng` files too) now play and loop in `image` nodes, like GIFs, and `playing` pauses them. Masks and a shader's `images` still draw the first frame. `source_blur` is ignored on them, as on GIFs. Still WebP and PNG files draw as before.
- Changed: A long GIF on a large canvas may now loop over fewer frames, and a GIF whose frames never change draws as a still.
- Fixed: On Hyprland 0.57, `mantle.workspaces`, `mantle.windows` and the special workspace list stayed empty whenever a special workspace existed. Hyprland 0.57 sends no `id` for specials.

## 0.11.1 - 2026-10-27

- Fixed: Images in a fast-scrolled list decode the ones now on screen first, and drop decodes for images no longer shown.
- Fixed: The Renderer settles about 3 MB lower after image-heavy use.
- Fixed: A missing `lsblk` now logs a warning instead of leaving `mantle.sysinfo.disks` silently empty.
- Fixed: Unreadable `/dev/input` devices log a warning naming the `input` group, instead of `caps_lock` and `num_lock` quietly stopping updates.
- Fixed: When screen capture can't use dma-buf, the warning names the cause, such as a render node the user cannot open.

## 0.11.0 - 2026-10-23

- Breaking: `mantle.network` drops its flat `scanning`, `strength`, `wifi_ip` and `available_networks`, which repeated the primary Wi-Fi interface. Read them from `wifi_devices[1]` (`nil` without Wi-Fi hardware).
- Added: `mantle.android_connect` lists your phone and other KDE Connect devices with battery, signal and pairing state, and can ring, ping, pair, send files and clipboard, switch plugins and clipboard sync per device, offer commands your phone can run on the PC, and show transfer progress and why browsing failed. It needs `kdeconnect-service` running.

## 0.10.0 - 2026-10-22

- Breaking: `file.write(path, text, on_done?)` is now `file.write(path, text, opts?, on_done?)`. Add `nil,` before an existing callback; a function in the `opts` slot raises.
- Added: `file.write(..., { in_place = true })` rewrites the file in place, so a program watching that one path keeps its watch.
- Added: `process.detach(cmd, args, { env = { NAME = "value", OTHER = false } })` and `mantle.applications:launch(id, { env = ... })` set or unset environment variables for the program they start; a string sets, `false` unsets.
- Added: `:map` and `computed` signals, and so `persistent_table` keys, have `on_change(fn(current, previous))`, run like a state's when a source's write changes the value. `delay` and `pulse` still have none.
- Changed: `file.read` no longer logs its errors; handle `err` in `on_done`.

## 0.9.1 - 2026-10-21

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
