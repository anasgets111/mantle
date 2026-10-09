# The demo

The README video is recorded by [`demo/director`](../../demo/director/shell.lua), a Mantle shell
that drives a second, scratch shell on camera. It types each edit into a code pane, saves it, and
the scratch shell reloads.

## Layout

| File | Holds |
| :--- | :--- |
| `shell.lua` | The storyboard: captions, edits and beats in order, and the runner |
| `takes.lua` | The edit list, the modules and assets copied beside the scratch shell, `prune` |
| `stages/take.tpl` | The scratch shell's `shell.lua`, once, with markers naming the edit that adds each block |
| `stages/*.lua` | Modules the scratch shell requires, some with their own marked blocks |
| `edits.lua` | Turns one edit into typing: lines matching the edit's `type` substrings are typed, the rest pasted |
| `feeds.lua` | Mock data the director feeds through `mantle set`, shaped like each capability |
| `layout.lua` | Pane, caption and popup geometry from the screen size, shared by both shells |
| `tools/` | `demo-check` and its pieces |

## Markers

A block between `--@ <edit>` and `--@ end` appears once `<edit>` has played. `--@ else` gives the
text shown before it. Several `--@ <edit>` lines in one block form a cascade: the first played edit
wins, so list the latest first. Markers are flat; nesting raises.

```text
--@ 07-wallpaper
local theme = require("theme")
--@ else
local theme = { accent = "#cba6f7" }
--@ end
```

`take.tpl` is not valid Lua on its own, so `just lua` skips it; `just demo-check` parses and
format-checks every snapshot pruned from it.

## Changing a beat

1. Edit `take.tpl` or a module, add or change the entry in `takes.lua`, and the beat in `shell.lua`.
2. `just demo-check`: every checkpoint through `mantle check` at 1920x1080, 1920x1200 and
   3440x1440, with and without sample mocks, plus layout fit, at most 120 typed characters and 5 s
   per edit, and lines of at most 108 columns. It stops no shell.
3. `just preview-beat <from> <to>` plays that range on screen and saves a screenshot per beat. It
   stops your shells for the run and restarts them.
4. `just demo [out]` records the take.

The take refuses to record unless an empty workspace is focused, and ends the moment the recorded
output shows another workspace or a foreign window. Keep the screen alone while it runs.
