# Documenting

How to write and check a page in this book. The book source is `docs/`, built by mdBook. The code
is the only source of truth: verify every behavioural claim in `renderer/src`, `supervisor/src` or
`shared/src` before writing it. [DECISIONS.md](../../DECISIONS.md) records why things were built;
it is history and may be stale, so cite an ADR only as a "why" pointer after the code confirms it.

## Commands

| Command | Does |
|---|---|
| `just docs` | Serves the book at `http://localhost:3000`, rebuilt on save |
| `just book` | Builds the book as CI publishes it, then checks every link and anchor |
| `just stubs` | Regenerates every `lua-meta/*.lua`, every `docs/capabilities/<name>.md` and the generated property tables |
| `cargo test -p renderer doc_examples` | Runs every Lua block in `docs/` and checks every screenshot (part of `just check`) |
| `just shots` | Re-renders the screenshots that changed, deletes orphans, lists what moved |
| `just rustdoc` | Rustdoc for the crates, warnings as errors. Not this book |

## Lua examples are tests

Every fenced block whose info string is `lua` or starts `lua,` runs in `cargo test`. The test is
`every_lua_block_in_the_docs_evaluates_and_lays_out` in `renderer/src/check.rs`. Each block goes
through the same evaluation and both layout passes as `mantle check`: no Wayland, no subprocesses,
every capability `nil` and then one sample push each, one 704x396 output named `DP-1`. The test fails on a Lua error, a check error or
a layout error, and names the block by `file:line`. Blocks inside `> ` quotes count too.

| Info string | The block |
|---|---|
| `lua` | Runs and lays out. It returns surfaces as `shell.lua` does, or returns one node, which the test mounts in a panel with 24 px of padding |
| `lua,shot` | A `lua` block the book shows a screenshot of ([screenshots](#screenshots)) |
| `lua,fragment` | Only parses. For a snippet that needs context the page has not given, like a `require` of another file |
| `lua,must-fail` | Must fail to evaluate or lay out. For showing a mistake |
| `lua,no-check` | Skipped. Put the reason in an HTML comment on the line above |

Prefer `lua` over `fragment`, and `fragment` over `no-check`. mdBook turns the comma into a second
CSS class, so every tag still highlights as Lua.

A widget example needs no surface around it:

```lua
button {
    padding = 6,
    on_click = function() print("clicked") end,
    children = { text { content = "Click" } },
}
```

Evaluation accepts this width, but layout refuses it:

```lua,must-fail
rect { width = "Wide", height = 10 }
```

A module file that only makes sense beside another one:

```lua,fragment
local clock = require("widgets.clock")
return panel { id = "bar", layer = "Top", child = clock }
```

<!-- no-check: two alternative returns in one block do not parse -->
```lua,no-check
return { require("bar") }  -- fails
local bar = require("bar")
return { bar }             -- works
```

## Screenshots

A `lua,shot` block also renders, headless over EGL, and must match its committed image,
`docs/images/<section>/<page>-<n>.png`, the page's n-th shot. `tools/book_links.py` puts the image
under the block on the site, so a page never links it. Tag the example a reader wants to see; a
block has to return a node or surfaces.

| Part | Rule |
|---|---|
| Image | Every visible surface stacked top to bottom 8 px apart, each popup where its `anchor_rect`, `anchor`, `gravity`, `offset` and `SlideX` put it on its parent, at scale 1 over Catppuccin Mocha crust `#11111b`. Cropped to the painted pixels plus 16 px; a shot that paints nothing fails |
| Still | Drawn with every tween finished |
| `<!-- shot: frames=0..400/20 -->` on the line above | An animated PNG: one frame per time, in ms after the last tween started. `frames=0,50,120` lists them |
| `docs/images/<section>/<page>.fakes.lua` | Runs before each shot on the page. `fakes = { battery = {...} }` is pushed as each capability's first push, `on_change` included. `__pointer = { surface = "bar", x = 40, y = 12 }` rests the pointer there after the first layout, in that surface's logical px (`surface` defaults to the first), so `hover()`, its rect and `on_hover` answer as for a real pointer. A `__after` function runs after the first layout, then the shot lays out again: that is how an OSD shows or a card leaves. A popup a click opens needs its anchor state set to the rect that click would pass |
| `<!-- file: shaders/glow.frag -->` on the line above any fenced block | That block is written to that path in each shot's config directory, `mantle.config_dir`, so a page's shots load the file the page shows |
| Pinned | Fonts, icons and images come from `renderer/fixtures/shots`, `os.time()` is 2026-09-24 12:45 UTC and `os.date` reads UTC, `$USER` is `user` |

A quoted absolute path whose file name is in `renderer/fixtures/shots/images` points at that file.
An icon missing from `fixtures/shots/icons` fails the test with its name; copy it in from Adwaita
and note it in `fixtures/shots/NOTICE`.

| Failure | Fix |
|---|---|
| `<name>.png differs by up to N per channel` | The render moved. Compare `<name>.new.png` beside it. Intended: `just shots`. Not: fix the regression |
| `is missing or a different size` | A new shot, or its size changed: `just shots`, then look at the image |
| `no lua,shot block draws this image` | A shot was removed or renumbered: `just shots` deletes it |
| An APNG passes on one GPU and differs by 100+ on another | A moving edge lands on exactly half a pixel in some frame, and drivers round that differently. Pick frame times that miss it, such as `0..210/30` over `0..200/20` |

A render within 2 per channel of the committed image passes and is not rewritten, so another GPU
driver never shows up in git. EGL is required: without it the test fails.

## Capability pages

`docs/capabilities/<name>.md` is generated by `supervisor/src/stubs.rs` from the Rust payload types.
Never edit it: write prose in `docs/capabilities/intro/<name>.md` and run `just stubs`.

| Part of the intro file | Lands on the page |
|---|---|
| Above `<!-- reference -->` | After the one-line blurb, before the generated `State` and `Actions` tables |
| Below `<!-- reference -->` | After the tables: How do I…, Gotchas, See also |
| No marker | All of it above the tables |

`cargo test` fails while a generated page differs from what `just stubs` would write. `intro/` stays
out of `SUMMARY.md`.

## Property tables

The property table on each `docs/nodes/<kind>.md` and `docs/surfaces/<role>.md`, the common one on
`nodes/index.md` and the box one on `guide/paint.md` are generated by `renderer/src/lua/nodes/stubs.rs`
from the typed fields in `renderer/src/lua/nodes/properties.rs`, the same fields `lua-meta/nodes.lua`
and `surfaces.lua` come from and the engine parses through. `just stubs` rewrites what sits between
two markers inside the page and leaves the rest alone:

```html
<!-- Generated from renderer/src/lua/nodes/properties.rs by `just stubs`: edit the table there. -->
<!-- End of the generated table. -->
```

| Column | From the field |
|---|---|
| Type | Its Rust type's LuaCATS, as the stub declares it; `range` after it |
| Default | `absent` |
| Behaviour | The `Book:` paragraph of its `///` block, which may link relative to the page, else the rest of that block without `(ADR-NNNN)` |

A page links to the table's anchors, not to rows. Prose about a property goes around the table, or
into its row if every page showing it should say it.

A table a property takes, such as `Edges` or `Transition`, is declared with `lua_shape!` on the
struct its parser reads it as (`renderer/src/lua/luacats.rs`): its `///` blocks are what `lua-meta`
says about the shape and each key. The struct fixes the key names and field types; which keys are
optional and how Lua spells one (`as Option<T>`, `as S`) are marked by hand beside them. One struct
has one shape, so `anchor_rect`, whose `x` and `y` may be left out, is declared as `region`'s `Rect`
with every key required. The book describes a shape in prose on the page.

## Page shape

One or two sentences on what the page is for, a small complete example, reference tables, a
`How do I…` table (`Task | Answer`), a `Gotchas` table (`Trap | Fix`), See also, then a `Source:`
line of code links. Each fact lives on one page; link to it elsewhere. Links to code go out of
`docs/` as relative paths (`../../renderer/src/...` from `docs/<dir>/x.md`); the book turns them
into GitHub links and fails the build on a missing target.
