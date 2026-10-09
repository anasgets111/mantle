-- Every layout.* box fits the open area at the three screens the demo is checked on, beside the
-- code pane and with it off (`full`).
-- Usage: lua layout_fit.lua
---@diagnostic disable: undefined-global, undefined-field -- plain lua: io and os, which the repo .luarc disables for Mantle
local here = (arg[0]:match("^(.*)/[^/]*$") or ".")
local layout = dofile(here .. "/../layout.lua")

local SCREENS = { { 1920, 1080 }, { 1920, 1200 }, { 3440, 1440 } }
local bad = 0

local function expect(ok, label, size, detail)
    if not ok then
        bad = bad + 1
        print(("FAIL %s at %s: %s"):format(label, size, detail))
    end
end

for k = 1, #SCREENS * 2 do
    local wh, full = SCREENS[(k - 1) % #SCREENS + 1], k > #SCREENS
    local size = wh[1] .. "x" .. wh[2] .. (full and " full" or "")
    local screen = { width = wh[1], height = wh[2], name = "DP-1", full = full }
    local m = layout.metrics(screen)
    expect(m.font >= 16, "font", size, m.font)
    local function fits(label, box)
        local right = box.left + (box.width or box.sheet)
        expect(box.left >= 12, label .. ".left", size, box.left)
        expect(right <= m.open - (full and 12 or 0), label .. " right edge", size, ("%d > open %d"):format(right, m.open))
    end
    for name in pairs(layout.POPUPS) do
        fits(name, layout.popup(name, screen))
    end
    local picker = layout.popup("picker", screen)
    expect(picker.tile >= 120, "picker.tile", size, picker.tile)
    local overview = layout.popup("overview", screen)
    expect(56 + overview.top + overview.shot_height + overview.card + 200 <= m.height, "overview height", size,
        overview.shot_height)
    -- No width means the caption hugs its text on a stage wide enough for it.
    local caption = layout.caption(m)
    expect(caption.width == nil or caption.width >= 600, "caption.width", size, tostring(caption.width))
end
print(bad == 0 and "layout_fit: ok" or ("layout_fit: " .. bad .. " failure(s)"))
os.exit(bad == 0 and 0 or 1)
