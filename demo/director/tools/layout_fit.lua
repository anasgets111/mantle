-- Every layout.* box fits the open area at the three screens the demo is checked on.
-- Usage: lua layout_fit.lua
local here = (arg[0]:match("^(.*)/[^/]*$") or ".")
local layout = dofile(here .. "/../layout.lua")

local SCREENS = { { 1920, 1080 }, { 1920, 1200 }, { 3440, 1440 } }
-- The design widths the stages pass to center/dock/fit.
local DESIGNS = { 380, 620, 640, 660, 760, 900 }
local bad = 0

local function expect(ok, label, size, detail)
    if not ok then
        bad = bad + 1
        print(("FAIL %s at %s: %s"):format(label, size, detail))
    end
end

for _, wh in ipairs(SCREENS) do
    local size = wh[1] .. "x" .. wh[2]
    local screen = { width = wh[1], height = wh[2], name = "DP-1" }
    local m = layout.metrics(screen)
    expect(m.font >= 16, "font", size, m.font)
    local function fits(label, box)
        local right = box.left + (box.width or box.sheet)
        expect(box.left >= 12, label .. ".left", size, box.left)
        expect(right <= m.open, label .. " right edge", size, ("%d > open %d"):format(right, m.open))
    end
    for _, design in ipairs(DESIGNS) do
        fits("center(" .. design .. ")", layout.center(screen, design))
        fits("dock(" .. design .. ")", layout.dock(screen, design))
        expect(layout.fit(screen, design) <= m.open, "fit(" .. design .. ")", size, layout.fit(screen, design))
    end
    local picker = layout.picker(screen)
    fits("picker", picker)
    expect(picker.tile >= 120, "picker.tile", size, picker.tile)
    fits("overview", layout.overview(screen))
    fits("mock", layout.mock(screen))
    -- No width means the caption hugs its text on a stage wide enough for it.
    local caption = layout.caption(m)
    expect(caption.width == nil or caption.width >= 600, "caption.width", size, tostring(caption.width))
end
print(bad == 0 and "layout_fit: ok" or ("layout_fit: " .. bad .. " failure(s)"))
os.exit(bad == 0 and 0 or 1)
