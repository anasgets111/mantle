-- Heavy shell: a benchmark config that puts a bar in one instance with three busy app windows.
--
-- Knobs, written with `mantle --pid <pid> set <name> <value>`:
--
-- | State | Values | Effect |
-- | :--- | :--- | :--- |
-- | `hs_mode` | `"bar"`, `"idle"`, `"busy"` | Bar only; bar plus open windows; windows driven by a 100 ms timer |
-- | `hs_flip` | `true`/`false` | Flip `hs_theme` every 3 s, re-resolving every surface |
-- | `hs_bar` | `true`/`false` | Show the bar, to A/B its share of a pass |
-- | `hs_rows` | `1`..`2000` | Rows the list window sees |
-- | `hs_gif` | `true`/`false` | Show the gallery's animated GIF |
-- | `hs_fx` | `true`/`false` | Show the effects window (36 cards: blur, glass, shadows, shaders, blends, masks); a 200 ms timer flips its tweens while infinite loops run beside them |
--
-- `driver.lua` logs Lua-side run counters every 10 s (`mantle --pid <pid> log`).
fonts { "Noto Sans" }

local theme = require("theme")
local bar = require("bar")
local calendar = require("calendar")
local listwin = require("listwin")
local gallery = require("gallery")
local fx = require("fx")

local mode = state("hs_mode", "bar")
local cal_hidden = state("hs_cal_hidden", false)

local windows_on = mode:map(function(m) return m ~= "bar" end)
local cal_on = computed({ mode, cal_hidden }, function(m, hidden) return m ~= "bar" and not hidden end)
local function close() mode:set("bar") end

require("driver")(mode, cal_hidden, listwin, gallery, theme, fx)

return {
    bar,
    calendar(cal_on, close),
    listwin.surface(windows_on, close),
    gallery.surface(windows_on, close),
    fx.surface(),
}
